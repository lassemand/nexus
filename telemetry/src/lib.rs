//! Shared metrics setup for nexus services.
//!
//! Long-running services call [`serve`] once at startup; their `metrics::counter!`
//! calls are then exposed on `GET /metrics`. CronJobs call [`batch`] at startup and
//! [`BatchMetrics::finish`] at the end, which pushes the run's metrics to a
//! Prometheus Pushgateway. See `docs/guides/metrics.md`.
//!
//! The scrape endpoint is a minimal handler on tokio rather than the exporter's
//! `http-listener` feature, which would add hyper's server stack and `ipnet`.
//! The exporter's own `push-gateway` feature is not used: it pushes periodically
//! with HTTP PUT, which would erase `last_success` metrics after a failed run.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PUSH_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: usize = 8 * 1024;
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn install() -> Result<PrometheusHandle> {
    PrometheusBuilder::new()
        .install_recorder()
        .context("a global metrics recorder is already installed")
}

fn set_build_info(service: &'static str) {
    metrics::gauge!("nexus_build_info", "service" => service, "version" => VERSION).set(1.0);
}

/// Installs the global metrics recorder and serves `GET /metrics` on `0.0.0.0:<port>`.
///
/// Sets `nexus_build_info{service, version} = 1`. Returns once the port is bound; the
/// server runs on a spawned task for the lifetime of the runtime. Errors if a global
/// recorder is already installed or the port cannot be bound.
pub async fn serve(service: &'static str, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("failed to bind metrics port {port}"))?;
    serve_listener(service, listener)
}

fn serve_listener(service: &'static str, listener: TcpListener) -> Result<()> {
    let handle = install()?;
    set_build_info(service);
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(%addr, service, "metrics endpoint listening");
    }
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        let _ =
                            tokio::time::timeout(REQUEST_TIMEOUT, respond(stream, &handle)).await;
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "metrics accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok(())
}

async fn respond(mut stream: TcpStream, handle: &PrometheusHandle) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < MAX_REQUEST_BYTES {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = path.split('?').next().unwrap_or("");
    let (status, content_type, body) = match (method, path) {
        ("GET", "/metrics") => (
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            handle.render(),
        ),
        (_, "/metrics") => ("405 Method Not Allowed", "text/plain", String::new()),
        _ => ("404 Not Found", "text/plain", String::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// Installs the global metrics recorder (no listener) for a CronJob and starts timing the run.
///
/// Errors if a global recorder is already installed.
pub fn batch(job: &'static str) -> Result<BatchMetrics> {
    Ok(BatchMetrics::new(job, install()?))
}

/// Metrics of one CronJob run; see [`batch`].
pub struct BatchMetrics {
    job: &'static str,
    handle: PrometheusHandle,
    started: Instant,
}

impl BatchMetrics {
    fn new(job: &'static str, handle: PrometheusHandle) -> Self {
        Self {
            job,
            handle,
            started: Instant::now(),
        }
    }

    /// Records the run's outcome and pushes everything recorded to
    /// `$PUSHGATEWAY_URL/metrics/job/<job>` with HTTP POST.
    ///
    /// Sets `nexus_job_last_run_timestamp_seconds`, `nexus_job_last_run_success`,
    /// `nexus_job_duration_seconds` and, on success only,
    /// `nexus_job_last_success_timestamp_seconds`. A missing `PUSHGATEWAY_URL` skips
    /// the push with a warning; a failed push is logged and never affects the exit code.
    pub async fn finish(self, outcome: Result<(), &anyhow::Error>) {
        let job = self.job;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        metrics::gauge!("nexus_job_last_run_timestamp_seconds", "job" => job).set(now);
        metrics::gauge!("nexus_job_last_run_success", "job" => job).set(if outcome.is_ok() {
            1.0
        } else {
            0.0
        });
        metrics::gauge!("nexus_job_duration_seconds", "job" => job)
            .set(self.started.elapsed().as_secs_f64());
        if outcome.is_ok() {
            metrics::gauge!("nexus_job_last_success_timestamp_seconds", "job" => job).set(now);
        }

        let Some(base) = std::env::var("PUSHGATEWAY_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
        else {
            tracing::warn!(
                job,
                "PUSHGATEWAY_URL is not set; job metrics are not pushed"
            );
            return;
        };
        if let Err(error) = push(&base, job, self.handle.render()).await {
            tracing::error!(job, error = %format!("{error:#}"), "failed to push job metrics");
        }
    }
}

/// POSTs `body` to the Pushgateway group of `job`, retrying once.
async fn push(base: &str, job: &str, body: String) -> Result<()> {
    let url = format!("{}/metrics/job/{job}", base.trim_end_matches('/'));
    let client = reqwest::Client::builder().timeout(PUSH_TIMEOUT).build()?;
    let mut last = None;
    for _ in 0..2 {
        let result = client
            .post(&url)
            .header("Content-Type", "text/plain; version=0.0.4")
            .body(body.clone())
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match result {
            Ok(_) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(last
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("push failed")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn push_posts_to_job_group() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        push(&server.uri(), "market", "x 1\n".into()).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method.as_str(), "POST");
        assert_eq!(requests[0].url.path(), "/metrics/job/market");
    }

    #[tokio::test]
    async fn push_retries_once_then_errors() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(push(&server.uri(), "market", String::new()).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
