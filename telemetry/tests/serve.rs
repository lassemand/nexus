use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn get(port: u16, request: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn serves_counters_and_build_info_and_rejects_second_recorder() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    telemetry::serve("test-svc", port).await.unwrap();
    metrics::counter!("test_events_total", "outcome" => "ok").increment(3);

    let body = get(port, "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(body.starts_with("HTTP/1.1 200"));
    assert!(body.contains("test_events_total{outcome=\"ok\"} 3"));
    assert!(body.contains("nexus_build_info{service=\"test-svc\",version=\""));

    assert!(get(port, "GET /other HTTP/1.1\r\n\r\n")
        .await
        .starts_with("HTTP/1.1 404"));
    assert!(get(port, "POST /metrics HTTP/1.1\r\n\r\n")
        .await
        .starts_with("HTTP/1.1 405"));

    assert!(telemetry::serve("again", 0).await.is_err());
    assert!(telemetry::batch("job").is_err());
}
