use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An error returned with `?` from the body still pushes the run, as a failure.
#[tokio::test]
async fn an_error_from_the_body_is_pushed_and_returned() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    std::env::set_var("PUSHGATEWAY_URL", server.uri());

    async fn body() -> anyhow::Result<bool> {
        metrics::counter!("early_rows_total").increment(3);
        let _: u32 = "not a number".parse()?;
        Ok(true)
    }
    let result = telemetry::run_batch("early", body()).await;
    assert!(result.is_err());

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/metrics/job/early");
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(body.contains("early_rows_total 3"));
    assert!(body.contains("nexus_job_last_run_success{job=\"early\"} 0"));
    assert!(!body.contains("last_success_timestamp"));
}
