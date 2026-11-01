use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn successful_run_posts_last_success() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    std::env::set_var("PUSHGATEWAY_URL", format!("{}/", server.uri()));

    telemetry::batch("ok-job").unwrap().finish(Ok(())).await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "POST");
    assert_eq!(requests[0].url.path(), "/metrics/job/ok-job");
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(body.contains("nexus_job_last_run_success{job=\"ok-job\"} 1"));
    assert!(body.contains("nexus_job_last_success_timestamp_seconds{job=\"ok-job\"}"));
}
