use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use herdr_mcp::cli::ToolMode;
use herdr_mcp::cli::{
    Args, Command, HttpServeArgs, InstallArgs, NgrokCommand, PrintConfigArgs, TunnelCommand,
};
use herdr_mcp::http::{self, DenyAll, TokenCheck};
use herdr_mcp::install::{self, Action, InstallEnv, Io, Options};
use herdr_mcp::oauth::JwtCheck;
use herdr_mcp::print_config;
use herdr_mcp::profile::ClientKind;
use herdr_mcp::serve::{self, BridgeOptions, HttpSpec, ServeError};
use herdr_mcp::server::HerdrMcp;
use herdr_mcp::tunnel::{
    self, RunOptions, StartOptions, SystemRunner, TunnelConfig, TunnelEnv, ngrok,
};
use rmcp::ServiceExt;
use rmcp::transport::stdio;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match &args.command {
        Some(Command::Install(install_args)) => return run_install(install_args),
        Some(Command::PrintConfig(print_args)) => return run_print_config(print_args),
        Some(Command::Tunnel(TunnelCommand::Ngrok(ngrok_command))) => {
            return run_ngrok(ngrok_command).await;
        }
        Some(Command::Tunnel(tunnel_command)) => return run_tunnel(tunnel_command),
        Some(Command::HttpServe(http_args)) => return run_http_serve(http_args).await,
        None => {}
    }
    let server = match build_server(
        args.session.as_deref(),
        args.client,
        args.max_wait_ms,
        args.tools,
        args.tunnel,
        args.expires_at,
    ) {
        Ok(server) => server,
        Err(code) => return code,
    };
    match server.serve(stdio()).await {
        Ok(running) => {
            let _ = running.waiting().await;
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("herdr-mcp: cannot start MCP server: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run_http_serve(args: &HttpServeArgs) -> ExitCode {
    match serve_http(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("herdr-mcp: {err}");
            ExitCode::from(err.code)
        }
    }
}

async fn serve_http(args: &HttpServeArgs) -> Result<(), ServeError> {
    let (check, metadata): (Arc<dyn TokenCheck>, _) =
        match (&args.oauth_issuer, &args.oauth_audience) {
            (Some(issuer), Some(audience)) => (
                Arc::new(
                    JwtCheck::new(issuer, audience, args.owners.clone())
                        .map_err(ServeError::usage)?,
                ),
                Some((audience.clone(), issuer.clone())),
            ),
            _ => (Arc::new(DenyAll), None),
        };
    let server = serve::prepare_http(HttpSpec {
        bind: args.bind,
        public_url: args.public_url.clone(),
        check,
        metadata,
        bridge: BridgeOptions {
            session: args.session.clone(),
            client: args.client,
            max_wait_ms: args.max_wait_ms,
            tools: args.tools,
            tunnel: tunnel_config(args.tunnel, args.expires_at)?,
        },
    })
    .await?;
    tokio::spawn(http::cancel_on_signal(server.shutdown.clone()));
    if let Ok(addr) = server.listener.local_addr() {
        eprintln!("herdr-mcp: serving MCP on http://{addr}{}", http::MCP_PATH);
    }
    http::serve(server.listener, server.router, server.shutdown)
        .await
        .map_err(|err| ServeError {
            message: format!("http server failed: {err}"),
            code: 1,
        })
}

fn tunnel_config(
    tunnel: bool,
    expires_at: Option<u64>,
) -> Result<Option<TunnelConfig>, ServeError> {
    tunnel
        .then(|| TunnelConfig::from_process(expires_at))
        .transpose()
        .map_err(ServeError::usage)
}

fn build_server(
    session: Option<&str>,
    client: Option<ClientKind>,
    max_wait_ms: Option<u64>,
    tools: ToolMode,
    tunnel: bool,
    expires_at: Option<u64>,
) -> Result<HerdrMcp, ExitCode> {
    let build = || {
        serve::build_bridge(BridgeOptions {
            session: session.map(str::to_owned),
            client,
            max_wait_ms,
            tools,
            tunnel: tunnel_config(tunnel, expires_at)?,
        })
    };
    build().map_err(|err| {
        eprintln!("herdr-mcp: {err}");
        ExitCode::from(err.code)
    })
}

fn run_print_config(args: &PrintConfigArgs) -> ExitCode {
    match InstallEnv::from_process() {
        Ok(env) => {
            print!("{}", print_config::render(&env.shim_path(), args.tools));
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("herdr-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_install(args: &InstallArgs) -> ExitCode {
    let env = match InstallEnv::from_process() {
        Ok(env) => env,
        Err(err) => {
            eprintln!("herdr-mcp: {err}");
            return ExitCode::FAILURE;
        }
    };
    let action = if args.relink_shim {
        Action::RelinkShim
    } else if args.status {
        Action::Status
    } else if args.remove {
        Action::Remove
    } else {
        Action::Install
    };
    let options = Options {
        control: args.control,
        action,
        yes: args.yes,
    };
    let stdin = std::io::stdin();
    let stdin_is_tty = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = install::run(
        &options,
        &env,
        &mut Io {
            stdin_is_tty,
            input: &mut input,
            out: &mut out,
            err: &mut err,
        },
    );
    let _ = out.flush();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn run_tunnel(command: &TunnelCommand) -> ExitCode {
    let env = match TunnelEnv::from_process() {
        Ok(env) => env,
        Err(err) => {
            eprintln!("herdr-mcp: {err}");
            return ExitCode::FAILURE;
        }
    };
    let stdin = std::io::stdin();
    let stdin_is_tty = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut io = Io {
        stdin_is_tty,
        input: &mut input,
        out: &mut out,
        err: &mut err,
    };
    let code = match command {
        TunnelCommand::Setup => tunnel::setup(&env, &mut io),
        TunnelCommand::Start(args) => {
            let duration = match args.duration.as_deref().map(tunnel::parse_duration) {
                None => None,
                Some(Ok(duration)) => Some(duration),
                Some(Err(err)) => {
                    eprintln!("herdr-mcp: {err}");
                    return ExitCode::from(2);
                }
            };
            let options = StartOptions {
                full: args.full,
                duration,
                session: args.session.clone(),
            };
            let now = herdr_mcp::audit::unix_now();
            tunnel::start(&env, &options, &SystemRunner, now, &mut io)
        }
        TunnelCommand::Stop => tunnel::stop(&env, &SystemRunner, &mut io),
        TunnelCommand::Status => tunnel::status(&env, &SystemRunner, &mut io),
        TunnelCommand::Doctor => tunnel::doctor(
            &env,
            &SystemRunner,
            &tunnel::DoctorOptions::default(),
            &mut io,
        ),
        TunnelCommand::Log(args) => tunnel::log(&env, args.lines, &mut io),
        TunnelCommand::Ngrok(_) => unreachable!("main routes ngrok commands to run_ngrok"),
        TunnelCommand::Run(args) => {
            let options = RunOptions {
                session: args.session.clone(),
                full: args.full,
                full_confirmed: args.full_confirmed,
                expires_at: args.expires_at,
            };
            match tunnel::prepare_run(&env, &options, &mut io) {
                Ok(mut child) => {
                    // exec replaces this process, so closing the pane kills tunnel-client directly.
                    let err = std::os::unix::process::CommandExt::exec(&mut child);
                    eprintln!("herdr-mcp: cannot start tunnel-client: {err}");
                    1
                }
                Err(err) => {
                    eprintln!("herdr-mcp: {err}");
                    err.code()
                }
            }
        }
    };
    let _ = out.flush();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

async fn run_ngrok(command: &NgrokCommand) -> ExitCode {
    let env = match TunnelEnv::from_process() {
        Ok(env) => env,
        Err(err) => {
            eprintln!("herdr-mcp: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let NgrokCommand::Run(args) = command {
        let options = ngrok::RunOptions {
            session: args.session.clone(),
            full: args.full,
            full_confirmed: args.full_confirmed,
            expires_at: args.expires_at,
        };
        let code = ngrok::run(&env, &options).await;
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    let stdin = std::io::stdin();
    let stdin_is_tty = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut io = Io {
        stdin_is_tty,
        input: &mut input,
        out: &mut out,
        err: &mut err,
    };
    let code = match command {
        NgrokCommand::Setup => ngrok::setup(&env, &mut io, &ngrok::DoctorOptions::default()).await,
        NgrokCommand::Start(args) => {
            let duration = match args.duration.as_deref().map(tunnel::parse_duration) {
                None => None,
                Some(Ok(duration)) => Some(duration),
                Some(Err(err)) => {
                    eprintln!("herdr-mcp: {err}");
                    return ExitCode::from(2);
                }
            };
            let options = StartOptions {
                full: args.full,
                duration,
                session: args.session.clone(),
            };
            let now = herdr_mcp::audit::unix_now();
            ngrok::start(&env, &options, &SystemRunner, now, &mut io)
        }
        NgrokCommand::Stop => ngrok::stop(&env, &SystemRunner, &mut io),
        NgrokCommand::Status => ngrok::status(&env, &SystemRunner, &mut io),
        NgrokCommand::Doctor => {
            ngrok::doctor(
                &env,
                &SystemRunner,
                &ngrok::DoctorOptions::default(),
                &mut io,
            )
            .await
        }
        NgrokCommand::Run(_) => unreachable!("handled above"),
    };
    let _ = out.flush();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
