use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use herdr_mcp::caller::{CallerEnv, ProcessAncestry};
use herdr_mcp::cli::{Args, Command, InstallArgs};
use herdr_mcp::herdr::{SessionEnv, resolve_socket_path};
use herdr_mcp::install::{self, Action, InstallEnv, Io, Options};
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use rmcp::ServiceExt;
use rmcp::transport::stdio;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(Command::Install(install_args)) = &args.command {
        return run_install(install_args);
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
    let server = HerdrMcp::new(BridgeConfig {
        socket_path,
        profile: Profile::new(args.client, args.max_wait_ms.map(Duration::from_millis)),
        mode: args.tools,
        ancestry: Arc::new(ProcessAncestry),
        caller_env: CallerEnv::from_process(),
        logger,
        install_env: InstallEnv::from_process().ok(),
    });
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

fn run_install(args: &InstallArgs) -> ExitCode {
    let env = match InstallEnv::from_process() {
        Ok(env) => env,
        Err(err) => {
            eprintln!("herdr-mcp: {err}");
            return ExitCode::FAILURE;
        }
    };
    let action = if args.status {
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
