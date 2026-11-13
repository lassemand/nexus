use std::time::{Duration, Instant};

use telemetry::trace::{self, LogFormat};

#[test]
fn init_without_endpoint_exports_nothing_and_a_second_init_errors() {
    std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
    std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
    std::env::set_var("RUST_LOG", "off");

    let guard = trace::init("trace-init-test").expect("first init succeeds");
    assert!(!guard.is_exporting());

    tracing::info_span!("trace_init.step").in_scope(|| {});

    let second = trace::init_with("trace-init-test", LogFormat::Text);
    assert!(second.is_err());
    assert!(trace::init("trace-init-test").is_err());

    let started = Instant::now();
    drop(guard);
    assert!(started.elapsed() < Duration::from_secs(1));
}
