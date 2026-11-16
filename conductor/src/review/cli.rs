//! `conductor reviews`: the operator's view of automatic reviews.

use std::io::Write;

use clap::Subcommand;
use sqlx::PgPool;

use super::store;

/// `conductor reviews <...>`
#[derive(Debug, Subcommand)]
pub enum ReviewsCommand {
    /// Show each reviewed issue with its send-backs, last verdict and time.
    List {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Set an issue's send-backs to 0, so automatic reviews resume.
    Reset {
        /// Linear identifier, e.g. NEX-270.
        identifier: String,
    },
}

/// Runs a subcommand, returning the exit code.
pub async fn run(
    command: ReviewsCommand,
    pool: &PgPool,
    out: &mut dyn Write,
) -> Result<i32, Box<dyn std::error::Error>> {
    match command {
        ReviewsCommand::List { json } => {
            let states = store::states(pool).await?;
            let runs = store::runs(pool).await?;
            if json {
                let rows: Vec<_> = states
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "state": s,
                            "running": runs.iter().any(|r| r.issue_id == s.issue_id),
                        })
                    })
                    .collect();
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "issues": rows,
                        "runs": runs,
                    }))?
                )?;
                return Ok(0);
            }
            writeln!(
                out,
                "{:<12} {:>10} {:>7} {:<18} {:<20} {:<9}",
                "ISSUE", "SEND-BACKS", "REVIEWS", "LAST VERDICT", "LAST REVIEW", "RUNNING"
            )?;
            for s in &states {
                let running = runs
                    .iter()
                    .find(|r| r.issue_id == s.issue_id)
                    .map(|r| {
                        if r.cancelled_at.is_some() {
                            "cancelled"
                        } else {
                            r.trigger.as_str()
                        }
                    })
                    .unwrap_or("-");
                writeln!(
                    out,
                    "{:<12} {:>10} {:>7} {:<18} {:<20} {:<9}",
                    s.identifier,
                    s.send_backs,
                    s.reviews_total,
                    s.last_verdict.as_deref().unwrap_or("-"),
                    s.last_review_at
                        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| "-".into()),
                    running,
                )?;
            }
            for r in runs
                .iter()
                .filter(|r| !states.iter().any(|s| s.issue_id == r.issue_id))
            {
                writeln!(
                    out,
                    "{:<12} {:>10} {:>7} {:<18} {:<20} {:<9}",
                    r.identifier,
                    0,
                    0,
                    "-",
                    "-",
                    r.trigger.as_str()
                )?;
            }
            Ok(0)
        }
        ReviewsCommand::Reset { identifier } => {
            let n = store::reset(pool, &identifier).await?;
            if n == 0 {
                writeln!(out, "no review state for {identifier}")?;
                return Ok(2);
            }
            writeln!(out, "{identifier}: send-backs reset to 0")?;
            Ok(0)
        }
    }
}
