use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use herdr_mcp::caller::{CallerEnv, ProcessAncestry};
use herdr_mcp::cli::{Args, Command, InstallArgs, PrintConfigArgs, TunnelCommand};
use herdr_mcp::herdr::{SessionEnv, resolve_socket_path};
use herdr_mcp::install::{self, Action, InstallEnv, Io, Options};
use herdr_mcp::log::Logger;
use herdr_mcp::print_config;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use herdr_mcp::tunnel::{self, RunOptions, StartOptions, SystemRunner, TunnelConfig, TunnelEnv};
use rmcp::ServiceExt;
use rmcp::transport::stdio;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match &args.command {
        Some(Command::Install(install_args)) => return run_install(install_args),
        Some(Command::PrintConfig(print_args)) => return run_print_config(print_args),
        Some(Command::Tunnel(tunnel_command)) => return run_tunnel(tunnel_command),
        None => {}
    }
    let socket_path =
        match resolve_socket_path(args.session.as_deref(), &SessionEnv::from_process()) {
            Ok(path) => path,
            Err(err) => {
                eprintln!("herdr-mcp: {err}");
                return ExitCode::from(2);
            }
        };
    let logger = socket_path
        .parent()
        .map(|dir| dir.join("herdr-mcp"))
        .and_then(|dir| Logger::to_dir(dir).ok())
        .unwrap_or_else(Logger::stderr_only);
    let tunnel = if args.tunnel {
        match TunnelConfig::from_process(args.expires_at) {
            Ok(config) => Some(config),
            Err(err) => {
                eprintln!("herdr-mcp: {err}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    let mut server = HerdrMcp::new(BridgeConfig {
        socket_path,
        profile: Profile::new(args.client, args.max_wait_ms.map(Duration::from_millis)),
        mode: args.tools,
        ancestry: Arc::new(ProcessAncestry),
        caller_env: CallerEnv::from_process(),
        logger,
        install_env: InstallEnv::from_process().ok(),
    });
    if let Some(tunnel) = tunnel {
        server = server.with_tunnel(tunnel);
    }
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
        TunnelCommand::Log(args) => tunnel::log(&env, args.lines, &mut io),
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
