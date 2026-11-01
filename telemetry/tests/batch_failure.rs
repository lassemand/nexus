use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn failed_run_posts_without_last_success() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    std::env::set_var("PUSHGATEWAY_URL", server.uri());

    let batch = telemetry::batch("failing").unwrap();
    assert!(telemetry::batch("again").is_err());
    metrics::counter!("failing_rows_total").increment(2);
    let err = anyhow::anyhow!("boom");
    batch.finish(Err(&err)).await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "POST");
    assert_eq!(requests[0].url.path(), "/metrics/job/failing");
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(body.contains("failing_rows_total 2"));
    assert!(body.contains("nexus_job_last_run_success{job=\"failing\"} 0"));
    assert!(body.contains("nexus_job_last_run_timestamp_seconds{job=\"failing\"}"));
    assert!(body.contains("nexus_job_duration_seconds{job=\"failing\"}"));
    assert!(!body.contains("last_success"));
}
