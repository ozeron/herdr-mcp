use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use herdr_mcp::caller::{CallerEnv, ProcessAncestry};
use herdr_mcp::cli::Args;
use herdr_mcp::herdr::{SessionEnv, resolve_socket_path};
use herdr_mcp::log::Logger;
use herdr_mcp::profile::Profile;
use herdr_mcp::server::{BridgeConfig, HerdrMcp};
use rmcp::ServiceExt;
use rmcp::transport::stdio;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
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
