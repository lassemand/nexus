//! Entry point for the conductor webhook receiver and dispatcher.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use conductor::{
    dispatcher::{DispatchArgs, Dispatcher, PerPullRequestResolver},
    github::watched_users_from_env,
    http::{serve_with_shutdown, AppState, DEFAULT_BIND},
    registry::{Registry, SystemClock},
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
    /// Run the HTTP server and dispatcher until stopped.
    Serve {
        /// Address to bind.
        #[arg(long, env = "CONDUCTOR_BIND", default_value = DEFAULT_BIND)]
        bind: String,

        #[command(flatten)]
        dispatch: DispatchArgs,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Plain formatter on stdout — what Kubernetes log collection expects.
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve { bind, dispatch } => {
            let github_webhook_secret = std::env::var("GITHUB_WEBHOOK_SECRET").ok();
            if github_webhook_secret.is_none() {
                tracing::warn!(
                    "GITHUB_WEBHOOK_SECRET not set — skipping signature verification; \
                     unsigned payloads will be accepted"
                );
            }

            let registry = Registry::from_env(Arc::new(SystemClock)).await?;
            registry.migrate().await?;

            let config = dispatch.into_config();
            tracing::info!(
                max_sessions = config.max_sessions,
                repo_root = %config.repo_root.display(),
                worktree_root = %config.worktree_root.display(),
                agent = %config.agent,
                skip_permissions = config.skip_permissions,
                "dispatcher configured"
            );

            let shutting_down = Arc::new(AtomicBool::new(false));
            let (dispatcher, sink, rx) = Dispatcher::new(
                registry,
                config,
                Arc::new(PerPullRequestResolver),
                Arc::clone(&shutting_down),
            );

            tokio::spawn(Arc::clone(&dispatcher).run_ingest(rx));
            tokio::spawn(Arc::clone(&dispatcher).run_scheduler());

            let state = AppState {
                sink: Arc::new(sink),
                github_webhook_secret,
                watched_github_users: watched_users_from_env(),
                shutting_down,
            };

            let stopping = Arc::clone(&dispatcher);
            serve_with_shutdown(&bind, state, async move {
                wait_for_termination().await;
                // Refuses further webhooks and drains running work before the
                // container is torn down.
                stopping.shutdown().await;
            })
            .await?;

            Ok(())
        }
    }
}

/// Resolves on SIGTERM (how Kubernetes asks a pod to stop) or Ctrl-C.
async fn wait_for_termination() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                return std::future::pending().await;
            }
        };
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received"),
            _ = tokio::signal::ctrl_c() => tracing::info!("interrupt received"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
