use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn unset_pushgateway_url_makes_no_request() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    std::env::remove_var("PUSHGATEWAY_URL");

    telemetry::batch("quiet").unwrap().finish(Ok(())).await;

    assert!(server.received_requests().await.unwrap().is_empty());
}
