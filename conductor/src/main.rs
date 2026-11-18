//! Entry point for the conductor webhook receiver and dispatcher.

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use conductor::{
    dispatcher::{DispatchArgs, Dispatcher},
    github::watched_users_from_env,
    github_api::{GithubRest, GithubToken},
    hooks,
    http::{serve_with_shutdown, AppState, DEFAULT_BIND},
    linear_api::{IssueSetSource, LinearGraphql, LinearIdentity, LinearReviewApi, LinearWriter},
    registry::{Registry, SystemClock},
    resolve::BranchResolver,
    review::{
        cli::ReviewsCommand, github_repo_from_remote, ReviewConfig, ReviewDeps, ReviewGithub,
    },
    sessions::{self, SessionsArgs},
    tmux::{SessionRuntime, TmuxRuntime},
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
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Run the HTTP server and dispatcher until stopped.
    Serve {
        /// Address to bind.
        #[arg(long, env = "CONDUCTOR_BIND", default_value = DEFAULT_BIND)]
        bind: String,

        #[command(flatten)]
        dispatch: DispatchArgs,
    },

    /// Inspect and control the sessions `serve` is running.
    ///
    /// Operates on the same registry as a running server, through Postgres, so
    /// it is safe to use while `serve` is working.
    Sessions(SessionsArgs),

    /// Inspect automatic reviews and reset an issue's send-backs.
    Reviews {
        #[command(subcommand)]
        command: ReviewsCommand,
    },

    /// Forward a Claude Code hook payload (read from stdin) to `conductor serve`.
    ///
    /// Registered as a Claude hook command. Always exits 0 and prints nothing to
    /// stdout, even when conductor is unreachable: a non-zero exit or any stdout
    /// output can change Claude's behaviour.
    Hook,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if matches!(cli.command, Command::Hook) {
        // stderr only: stdout is read by Claude.
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
        run_hook().await;
        return Ok(());
    }

    // Plain formatter on stdout — what Kubernetes log collection expects.
    tracing_subscriber::fmt::init();

    match cli.command {
        Command::Hook => unreachable!("handled above"),
        Command::Sessions(args) => {
            let registry = Registry::from_env(Arc::new(SystemClock)).await?;
            // No `migrate` here: the CLI inspects a database the server owns,
            // and a read-only operator command should not alter its schema.
            let env = args.env.resolve();

            // The unlocked handle: a held `StdoutLock` is not `Send`, and the
            // attach path awaits across writes. Each write locks internally.
            let mut out = std::io::stdout();
            let runtime = TmuxRuntime::from_env();
            let code = sessions::run(
                args.command,
                &env,
                &registry,
                &runtime,
                &sessions::StdTerminal,
                &mut out,
            )
            .await?;
            out.flush()?;
            std::process::exit(code);
        }

        Command::Reviews { command } => {
            let registry = Registry::from_env(Arc::new(SystemClock)).await?;
            let mut out = std::io::stdout();
            let code = conductor::review::cli::run(command, registry.pool(), &mut out).await?;
            out.flush()?;
            std::process::exit(code);
        }

        Command::Serve { bind, dispatch } => {
            // Own port: 8788 receives external webhooks, so /metrics must not live there.
            // Metrics must not take conductor down: a port that cannot bind is only logged.
            if let Err(e) = telemetry::serve("conductor", 9464).await {
                tracing::error!(error = %e, "metrics endpoint unavailable, continuing without it");
            }

            let github_webhook_secret = std::env::var("GITHUB_WEBHOOK_SECRET").ok();
            if github_webhook_secret.is_none() {
                tracing::warn!(
                    "GITHUB_WEBHOOK_SECRET not set — skipping signature verification; \
                     unsigned payloads will be accepted"
                );
            }

            let registry = Registry::from_env(Arc::new(SystemClock)).await?;
            registry.migrate().await?;

            // Read before `into_config` consumes the args; these configure the
            // resolver rather than the dispatcher.
            let github_api_url = dispatch.github_api_url.clone();
            let github_token = dispatch.github_token.clone();
            let issue_source: Option<Arc<dyn IssueSetSource>> =
                match std::env::var("LINEAR_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty())
                {
                    Some(key) => Some(Arc::new(LinearGraphql::new(
                        key,
                        std::env::var("CONDUCTOR_LINEAR_API_URL").ok(),
                    )?)),
                    None => None,
                };
            let mut config = dispatch.into_config();
            let github: Option<Arc<GithubRest>> = GithubToken::from_env(github_token)
                .map(|token| GithubRest::new(&github_api_url, token))
                .transpose()?
                .map(Arc::new);
            if github.is_none() {
                tracing::warn!(
                    "GITHUB_TOKEN not set and no GitHub App credential helper — pull request \
                     branches cannot be looked up, so issue comments will route to a \
                     per-pull-request group"
                );
            }
            if config.review.repo.is_none() {
                config.review.repo = origin_repo(&config.repo_root).await;
            }
            match &config.review_launch {
                Some(launch) if launch.is_ready() => {
                    if config.review.bot_login.is_none() {
                        config.review.bot_login = launch.bot_login();
                    }
                    tracing::info!(
                        bot_login = ?config.review.bot_login,
                        dir = %launch.identity_dir.display(),
                        "review sessions run as the reviewer app"
                    );
                }
                _ => tracing::warn!(
                    "the reviewer's GitHub App is not configured; review sessions run as the author app \
                     and cannot approve its pull requests"
                ),
            }
            let reviews = review_deps(&config.review, github.clone())?;
            match &config.model {
                Some(model) => tracing::info!("claude model: {model}"),
                None => {
                    tracing::warn!(
                        "CONDUCTOR_CLAUDE_MODEL not set; sessions use Claude Code's default model"
                    );
                    tracing::info!("claude model: default");
                }
            }
            tracing::info!(
                max_sessions = config.max_sessions,
                repo_root = %config.repo_root.display(),
                worktree_root = %config.worktree_root.display(),
                agent = %config.agent,
                skip_permissions = config.skip_permissions,
                "dispatcher configured"
            );

            // Cloned before the move below: the resolver reads branch ownership
            // from the same registry the dispatcher writes to.
            let resolver_registry = registry.clone();
            let shutting_down = Arc::new(AtomicBool::new(false));
            let runtime: Arc<dyn SessionRuntime> = Arc::new(TmuxRuntime::from_env());
            let review_bot_login = config.review.bot_login.clone();
            let (dispatcher, sink, rx) = Dispatcher::new_with_reviews(
                registry,
                config,
                Arc::new(BranchResolver::new(
                    resolver_registry,
                    github.map(|g| g as Arc<dyn ReviewGithub>),
                )),
                Arc::clone(&shutting_down),
                issue_source,
                Arc::clone(&runtime),
                reviews,
            );

            // Hooks drive the tmux-model states; the listener is loopback only,
            // so it is never reachable through the public webhook address.
            dispatcher.reconcile_tmux().await;
            let hook_addr = format!("127.0.0.1:{}", hooks::hook_port());
            let hook_state = dispatcher.hook_state();
            tokio::spawn(async move {
                if let Err(e) = hooks::serve(&hook_addr, hook_state).await {
                    tracing::error!(error = %e, addr = %hook_addr, "hook listener stopped");
                }
            });

            tokio::spawn(Arc::clone(&dispatcher).run_ingest(rx));
            tokio::spawn(Arc::clone(&dispatcher).run_scheduler());

            let state = AppState {
                sink: Arc::new(sink),
                github_webhook_secret,
                watched_github_users: watched_users_from_env(),
                shutting_down,
                review_bot_login,
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

/// Builds the review orchestration's clients, or `None` when reviews are off
/// or cannot work (no `LINEAR_API_KEY` to read issues, no GitHub credentials).
///
/// Linear writes go through the reviewer's OAuth app when
/// `REVIEW_LINEAR_CLIENT_ID` and `REVIEW_LINEAR_CLIENT_SECRET` are set, and
/// through `LINEAR_API_KEY` otherwise, with one warning.
fn review_deps(
    config: &ReviewConfig,
    github: Option<Arc<GithubRest>>,
) -> Result<Option<ReviewDeps>, Box<dyn std::error::Error>> {
    if !config.enabled {
        tracing::info!("CONDUCTOR_REVIEW_ENABLED is false; automatic and manual reviews are off");
        return Ok(None);
    }
    let Some(api_key) = std::env::var("LINEAR_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    else {
        tracing::warn!("LINEAR_API_KEY not set; reviews are off");
        return Ok(None);
    };
    let Some(github) = github else {
        tracing::warn!("no GitHub credentials for conductor itself; reviews are off");
        return Ok(None);
    };
    let api_url = std::env::var("CONDUCTOR_LINEAR_API_URL").ok();
    let identity = LinearIdentity::for_reviews(
        std::env::var("REVIEW_LINEAR_CLIENT_ID").ok(),
        std::env::var("REVIEW_LINEAR_CLIENT_SECRET").ok(),
        &api_key,
        std::env::var("CONDUCTOR_LINEAR_OAUTH_URL").ok(),
    )?;
    tracing::info!(
        identity = identity.kind(),
        state = %config.state,
        max_iterations = config.max_iterations,
        max_sessions = config.max_sessions,
        timeout_secs = config.timeout.as_secs(),
        model = ?config.model,
        "reviews configured"
    );
    let linear = LinearReviewApi::new(
        LinearGraphql::new(api_key, api_url.clone())?,
        LinearWriter::new(identity, api_url)?,
    );
    Ok(Some(ReviewDeps {
        config: config.clone(),
        linear: Arc::new(linear),
        github,
    }))
}

/// `owner/name` of the repository's `origin` remote, if it is on GitHub.
async fn origin_repo(repo_root: &std::path::Path) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(repo_root)
        .args(["remote", "get-url", "origin"])
        .output()
        .await
        .ok()?;
    let repo = out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        .and_then(|url| github_repo_from_remote(&url));
    if repo.is_none() {
        tracing::warn!("cannot tell the GitHub repository from `origin`; reviews cover every repository an issue lists");
    }
    repo
}

/// Reads the hook JSON from stdin and POSTs it to the local listener.
///
/// Never fails: every error goes to stderr and the process still exits 0.
async fn run_hook() {
    use tokio::io::AsyncReadExt;

    let mut body = Vec::new();
    if let Err(e) = tokio::io::stdin().read_to_end(&mut body).await {
        tracing::warn!(error = %e, "cannot read hook payload from stdin");
        return;
    }
    let url = format!("http://127.0.0.1:{}/hook", hooks::hook_port());
    if let Err(e) = hooks::forward(&url, body).await {
        tracing::warn!(error = %e, %url, "cannot deliver hook to conductor");
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
