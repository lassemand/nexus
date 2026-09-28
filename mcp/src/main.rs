//! Entry point for the standalone webhook server.

use std::sync::Arc;

use clap::{Parser, Subcommand};
use mcp::{
    LogSink,
    github::watched_users_from_env,
    http::{AppState, DEFAULT_ADDR, serve},
};

#[derive(Parser)]
#[command(
    name = "mcp",
    about = "Webhook server for Linear and GitHub events",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server until stopped.
    Serve {
        /// Address to bind.
        #[arg(long, default_value = DEFAULT_ADDR, env = "MCP_ADDR")]
        addr: String,
    },
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Logs go to stderr so stdout stays free for future subcommands that emit
    // machine-readable output.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve { addr } => {
            let github_webhook_secret = std::env::var("GITHUB_WEBHOOK_SECRET").ok();
            if github_webhook_secret.is_none() {
                tracing::warn!(
                    "GITHUB_WEBHOOK_SECRET not set — skipping signature verification; \
                     unsigned payloads will be accepted"
                );
            }

            let state = AppState {
                sink: Arc::new(LogSink),
                github_webhook_secret,
                watched_github_users: watched_users_from_env(),
            };

            serve(&addr, state).await
        }
    }
}
