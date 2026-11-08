use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `Ok(false)` (a partial failure for the exit code) still counts as a success run.
#[tokio::test]
async fn any_ok_value_is_a_successful_run() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    std::env::set_var("PUSHGATEWAY_URL", server.uri());

    let result = telemetry::run_batch("partial", async { Ok(false) }).await;
    assert!(!result.unwrap());

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(body.contains("nexus_job_last_run_success{job=\"partial\"} 1"));
    assert!(body.contains("nexus_job_last_success_timestamp_seconds{job=\"partial\"}"));
}
