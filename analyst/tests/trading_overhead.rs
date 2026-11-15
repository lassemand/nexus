//! What tracing adds to the entry request of a simulated trade. The time from the
//! hand-off to the first `GET /quote` is measured with no tracing installed and
//! then with tracing on and the collector unreachable, in the same process, and
//! the difference of the two p99s must stay small. Its own test binary because
//! the tracing subscriber is installed globally and cannot be removed.

mod common;

use std::time::{Duration, Instant};

use analyst::DecisionKind;
use common::trading::{hand_off, quote, rig, signal, sto};
use telemetry::trace::OpenTelemetrySpanExt;

const SAMPLES: usize = 500;
const MAX_OVERHEAD: Duration = Duration::from_micros(100);

async fn measure() -> Vec<Duration> {
    let released = sto(11, 10, 11, 0, 0);
    let rig = rig(released, Duration::ZERO, 8, |_| {
        Ok(quote("101.50", "100.00"))
    });
    let mut elapsed = Vec::with_capacity(SAMPLES);
    for id in 0..SAMPLES {
        let seen = rig.quotes.real_times.lock().expect("lock").len();
        let decision_span = tracing::info_span!("analyst.hand_off");
        let mut signal = signal(
            i64::try_from(id).expect("small") + 1,
            "AAA",
            "XSTO",
            DecisionKind::Buy,
            released,
        );
        signal.context = decision_span.context();
        let handed = Instant::now();
        hand_off(&rig, signal);
        loop {
            tokio::task::yield_now().await;
            let times = rig.quotes.real_times.lock().expect("lock");
            if times.len() > seen {
                elapsed.push(times[seen].duration_since(handed));
                break;
            }
        }
    }
    elapsed.sort();
    elapsed
}

fn p99(sorted: &[Duration]) -> Duration {
    sorted[sorted.len() * 99 / 100]
}

#[tokio::test(start_paused = true)]
async fn tracing_adds_under_a_hundred_microseconds_to_the_entry_request() {
    let _ = measure().await;
    let untraced = measure().await;

    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1");
    let _guard = telemetry::trace::init("analyst-overhead-test").expect("tracing starts");
    let _ = measure().await;
    let traced = measure().await;

    eprintln!(
        "hand-off to first quote request, {SAMPLES} samples: untraced p50 {:?} p99 {:?}; traced (collector down) p50 {:?} p99 {:?}",
        untraced[untraced.len() / 2],
        p99(&untraced),
        traced[traced.len() / 2],
        p99(&traced),
    );
    let overhead = p99(&traced).saturating_sub(p99(&untraced));
    assert!(
        overhead < MAX_OVERHEAD,
        "tracing added {overhead:?} at p99 (untraced {:?}, traced {:?})",
        p99(&untraced),
        p99(&traced)
    );
    assert!(p99(&traced) < Duration::from_millis(5));
}
