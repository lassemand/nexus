//! Entry point for the conductor webhook receiver.

use std::sync::Arc;

use clap::{Parser, Subcommand};
use conductor::{
    github::watched_users_from_env,
    http::{serve, AppState, DEFAULT_BIND},
    LogSink,
};

#[derive(Parser)]
#[command(
    name = "conductor",
    about = "Receives Linear and GitHub webhooks and dispatches them to Claude Code sessions",
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
        #[arg(long, env = "CONDUCTOR_BIND", default_value = DEFAULT_BIND)]
        bind: String,
    },
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Plain formatter on stdout — what Kubernetes log collection expects.
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve { bind } => {
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

            serve(&bind, state).await
        }
    }
}
