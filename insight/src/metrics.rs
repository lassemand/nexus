//! Prometheus instrumentation for the MCP tools.
//!
//! Every tool body runs through [`observe`], the single place that records
//! `insight_tool_calls_total`, `insight_tool_duration_seconds` and
//! `insight_tool_result_rows`. Tool arguments and results are never recorded.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use metrics::{counter, gauge, histogram};
use rmcp::model::{CallToolResult, ErrorCode};
use rmcp::Error as McpError;

/// Histogram bounds (seconds) for tool and database durations.
pub const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Histogram bounds for the number of rows a tool returns.
pub const ROW_BUCKETS: &[f64] = &[
    0.0, 1.0, 5.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0, 50000.0,
];

/// Successful output of a tool body: the MCP result and how many rows it holds.
pub struct ToolOutput {
    /// The result returned to the MCP client.
    pub result: CallToolResult,
    /// Number of rows serialised into `result`.
    pub rows: usize,
}

/// Runs a tool body and records its call count, duration and result size.
///
/// `outcome` is `ok`, `invalid_params` for an `INVALID_PARAMS` error, or `error`.
pub async fn observe<F>(tool: &'static str, body: F) -> Result<CallToolResult, McpError>
where
    F: Future<Output = Result<ToolOutput, McpError>>,
{
    let started = Instant::now();
    let outcome = body.await;
    histogram!("insight_tool_duration_seconds", "tool" => tool)
        .record(started.elapsed().as_secs_f64());
    match outcome {
        Ok(output) => {
            counter!("insight_tool_calls_total", "tool" => tool, "outcome" => "ok").increment(1);
            histogram!("insight_tool_result_rows", "tool" => tool).record(output.rows as f64);
            Ok(output.result)
        }
        Err(error) => {
            let label = if error.code == ErrorCode::INVALID_PARAMS {
                "invalid_params"
            } else {
                "error"
            };
            counter!("insight_tool_calls_total", "tool" => tool, "outcome" => label).increment(1);
            Err(error)
        }
    }
}

/// Times a database future into `insight_db_query_duration_seconds{tool}`.
pub async fn timed_db<T, F>(tool: &'static str, query: F) -> T
where
    F: Future<Output = T>,
{
    let started = Instant::now();
    let value = query.await;
    histogram!("insight_db_query_duration_seconds", "tool" => tool)
        .record(started.elapsed().as_secs_f64());
    value
}

/// Maps a database error to an internal MCP error without exposing it as a label.
pub fn db_error(error: sqlx::Error) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

/// Keeps `insight_sse_sessions` at the number of open SSE sessions.
///
/// Created when a session is accepted; the gauge drops when the last clone of the
/// owning server is dropped, which rmcp does when the session ends.
#[derive(Debug)]
pub struct SessionGuard;

impl SessionGuard {
    /// Counts a newly accepted SSE session.
    pub fn open() -> Arc<Self> {
        counter!("insight_sse_connections_total").increment(1);
        gauge!("insight_sse_sessions").increment(1.0);
        Arc::new(Self)
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        gauge!("insight_sse_sessions").decrement(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    fn render(f: impl FnOnce()) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, f);
        handle.render()
    }

    fn run<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime builds")
            .block_on(f)
    }

    #[test]
    fn ok_call_is_counted_with_rows_and_duration() {
        let text = render(|| {
            let out = run(observe("fetch_trade_results", async {
                Ok(ToolOutput {
                    result: CallToolResult::success(vec![Content::text("[]")]),
                    rows: 3,
                })
            }));
            assert!(out.is_ok());
        });
        assert!(
            text.contains(
                "insight_tool_calls_total{tool=\"fetch_trade_results\",outcome=\"ok\"} 1"
            ),
            "{text}"
        );
        assert!(text.contains("insight_tool_duration_seconds"), "{text}");
        assert!(text.contains("insight_tool_result_rows"), "{text}");
    }

    #[test]
    fn injected_db_error_is_an_error_outcome() {
        let text = render(|| {
            let out = run(observe("summarise_trade_results", async {
                Err(db_error(sqlx::Error::PoolTimedOut))
            }));
            assert!(out.is_err());
        });
        assert!(
            text.contains(
                "insight_tool_calls_total{tool=\"summarise_trade_results\",outcome=\"error\"} 1"
            ),
            "{text}"
        );
        assert!(!text.contains("outcome=\"ok\""), "{text}");
    }

    #[test]
    fn invalid_params_has_its_own_outcome() {
        let text = render(|| {
            let _ = run(observe("fetch_trade_results", async {
                Err(McpError::invalid_params("bad", None))
            }));
        });
        assert!(text.contains("outcome=\"invalid_params\"} 1"), "{text}");
    }

    #[test]
    fn session_gauge_follows_the_guard() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let guard = SessionGuard::open();
            assert!(handle.render().contains("insight_sse_sessions 1"));
            drop(guard);
        });
        assert!(handle.render().contains("insight_sse_sessions 0"));
    }
}
