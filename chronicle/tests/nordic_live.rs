//! nordic-live: the connection rules against a local WebSocket server, and the
//! poll's "already ingested" check against a real (signal-migrated) Postgres.
//! The database test is skipped unless `TEST_DATABASE_URL` is set.

mod common;

use chronicle_lib::nordic::live::{
    handle_message, Companies, ConnEvent, Disposition, Listener, Received, Registered, SeenSet,
    SignalDb,
};
use chrono::Utc;
use common::TestDb;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

const GOMSPACE: &str = include_str!("fixtures/mfn_ws_gomspace_report.json");

/// What the test server does on its n-th connection.
#[derive(Clone)]
enum Script {
    /// Send `text`, then close with `code`.
    SendThenClose { text: &'static str, code: u16 },
    /// Send `text`, then keep the connection open until the client leaves.
    SendThenHold { text: &'static str },
    /// Accept and say nothing.
    Silent,
    /// Ping every 50 ms for `for_ms`, then send `text` and hold.
    Pings { for_ms: u64, text: &'static str },
}

async fn serve(scripts: Vec<Script>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut n = 0;
        while let Ok((stream, _)) = listener.accept().await {
            let script = scripts.get(n).cloned().unwrap_or(Script::Silent);
            n += 1;
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                match script {
                    Script::SendThenClose { text, code } => {
                        ws.send(Message::Text(text.into())).await.unwrap();
                        let frame = CloseFrame {
                            code: CloseCode::from(code),
                            reason: "test".into(),
                        };
                        ws.send(Message::Close(Some(frame))).await.unwrap();
                        while ws.next().await.is_some() {}
                    }
                    Script::SendThenHold { text } => {
                        ws.send(Message::Text(text.into())).await.unwrap();
                        while ws.next().await.is_some() {}
                    }
                    Script::Silent => while ws.next().await.is_some() {},
                    Script::Pings { for_ms, text } => {
                        let end = tokio::time::Instant::now() + Duration::from_millis(for_ms);
                        while tokio::time::Instant::now() < end {
                            ws.send(Message::Ping(vec![1])).await.unwrap();
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        ws.send(Message::Text(text.into())).await.unwrap();
                        while ws.next().await.is_some() {}
                    }
                }
            });
        }
    });
    format!("ws://{addr}")
}

struct Run {
    messages: mpsc::Receiver<Received>,
    events: mpsc::UnboundedReceiver<ConnEvent>,
    handle: tokio::task::JoinHandle<()>,
}

fn start(url: String, dead_after: Duration) -> Run {
    let (tx, messages) = mpsc::channel(16);
    let (etx, events) = mpsc::unbounded_channel();
    let mut listener = Listener::new(url, "nordic-live-test");
    listener.dead_after = dead_after;
    listener.events = Some(etx);
    let handle = tokio::spawn(async move { listener.run(tx).await });
    Run {
        messages,
        events,
        handle,
    }
}

async fn next_event(r: &mut Run) -> ConnEvent {
    tokio::time::timeout(Duration::from_secs(5), r.events.recv())
        .await
        .expect("timed out waiting for a connection event")
        .unwrap()
}

async fn next_text(r: &mut Run) -> String {
    tokio::time::timeout(Duration::from_secs(5), r.messages.recv())
        .await
        .expect("timed out waiting for a message")
        .unwrap()
        .text
}

#[tokio::test]
async fn code_3000_reconnects_at_once_and_other_closes_back_off() {
    let url = serve(vec![
        Script::SendThenClose {
            text: "A",
            code: 3000,
        },
        Script::SendThenClose {
            text: "B",
            code: 1011,
        },
        Script::SendThenHold { text: "C" },
    ])
    .await;
    let mut run = start(url, Duration::from_secs(60));

    assert_eq!(next_text(&mut run).await, "A");
    assert_eq!(next_text(&mut run).await, "B");
    assert_eq!(next_text(&mut run).await, "C");

    let mut events = vec![];
    for _ in 0..5 {
        events.push(next_event(&mut run).await);
    }
    assert_eq!(events[0], ConnEvent::Connected);
    assert_eq!(events[1], ConnEvent::FastReconnect); // 3000: no delay at all
    assert_eq!(events[2], ConnEvent::Connected);
    match &events[3] {
        ConnEvent::Backoff { reason, delay } => {
            assert_eq!(*delay, Duration::from_millis(10));
            assert!(reason.contains("closed by server"), "{reason}");
        }
        other => panic!("expected a backoff after 1011, got {other:?}"),
    }
    assert_eq!(events[4], ConnEvent::Connected);
    run.handle.abort();
}

#[tokio::test]
async fn a_silent_connection_is_dropped_and_reconnected() {
    let url = serve(vec![Script::Silent, Script::SendThenHold { text: "alive" }]).await;
    let mut run = start(url, Duration::from_millis(200));
    assert_eq!(next_event(&mut run).await, ConnEvent::Connected);
    assert_eq!(next_event(&mut run).await, ConnEvent::Dead);
    assert!(matches!(
        next_event(&mut run).await,
        ConnEvent::Backoff { .. }
    ));
    assert_eq!(next_event(&mut run).await, ConnEvent::Connected);
    assert_eq!(next_text(&mut run).await, "alive");
    run.handle.abort();
}

#[tokio::test]
async fn pings_keep_a_quiet_connection_alive() {
    // 700 ms of pings only, with a 300 ms silence limit: never dead.
    let url = serve(vec![Script::Pings {
        for_ms: 700,
        text: "late",
    }])
    .await;
    let mut run = start(url, Duration::from_millis(300));
    assert_eq!(next_event(&mut run).await, ConnEvent::Connected);
    assert_eq!(next_text(&mut run).await, "late");
    while let Ok(e) = run.events.try_recv() {
        assert_ne!(e, ConnEvent::Dead);
    }
    run.handle.abort();
}

#[tokio::test]
async fn connect_failures_back_off_and_dropping_the_receiver_stops_the_listener() {
    // Nothing listens on this port.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let mut run = start(format!("ws://127.0.0.1:{port}"), Duration::from_secs(60));
    match next_event(&mut run).await {
        ConnEvent::Backoff { reason, delay } => {
            assert!(reason.contains("connect failed"), "{reason}");
            assert_eq!(delay, Duration::from_millis(10));
        }
        other => panic!("{other:?}"),
    }
    match next_event(&mut run).await {
        ConnEvent::Backoff { delay, .. } => assert_eq!(delay, Duration::from_millis(20)),
        other => panic!("{other:?}"),
    }
    drop(run.messages);
    // The listener notices at its next attempt and ends by itself.
    tokio::time::timeout(Duration::from_secs(5), run.handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn the_listener_hands_over_messages_that_process_end_to_end() {
    let url = serve(vec![Script::SendThenHold { text: GOMSPACE }]).await;
    let mut run = start(url, Duration::from_secs(60));
    let msg = tokio::time::timeout(Duration::from_secs(5), run.messages.recv())
        .await
        .unwrap()
        .unwrap();
    let companies = Companies::new(vec![Registered {
        ticker: "GOMX".into(),
        exchange_mic: "FNSE".into(),
        mfn_slug: "gomspace-group".into(),
    }]);
    let (counters, out) = handle_message(
        &msg.text,
        msg.received_at,
        &companies,
        &mut SeenSet::new(10),
        None,
    )
    .await;
    assert_eq!((counters.seen, counters.relevant), (1, 1));
    assert!(matches!(&out[0], Disposition::New(i) if i.event() == "report_released"));
    run.handle.abort();
}

#[tokio::test]
async fn an_item_the_poll_already_stored_is_not_logged_as_new() {
    let Some(db) = TestDb::create_empty().await else {
        return;
    };
    // The poll's table is signal's special_events; companies has mfn_slug there too.
    sqlx::migrate!("../signal/migrations")
        .run(&db.pool)
        .await
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(GOMSPACE).unwrap();
    let news_id = doc["items"][0]["news_id"].as_str().unwrap().to_string();
    let companies = Companies::new(vec![Registered {
        ticker: "GOMX".into(),
        exchange_mic: "FNSE".into(),
        mfn_slug: "gomspace-group".into(),
    }]);
    let check = SignalDb(db.pool.clone());

    // Not stored yet: new.
    let (_, out) = handle_message(
        GOMSPACE,
        Utc::now(),
        &companies,
        &mut SeenSet::new(10),
        Some(&check),
    )
    .await;
    assert!(matches!(out.as_slice(), [Disposition::New(i)] if i.event() == "report_released"));

    // The poll stores it (as signal does: source = 'mfn', source_id = the MFN news id).
    sqlx::query(
        "INSERT INTO special_events (ticker, exchange_mic, event_type, occurred_at, status, source, source_id) \
         VALUES ('GOMX','FNSE','quarterly_report','2026-08-26T06:00:00Z','occurred','mfn',$1)",
    )
    .bind(&news_id)
    .execute(&db.pool)
    .await
    .unwrap();
    let (_, out) = handle_message(
        GOMSPACE,
        Utc::now(),
        &companies,
        &mut SeenSet::new(10),
        Some(&check),
    )
    .await;
    assert!(
        matches!(out.as_slice(), [Disposition::AlreadyIngested(id)] if id.to_string() == news_id),
        "{out:?}"
    );

    // A failing lookup (database gone) still logs the item.
    db.pool.close().await;
    let (_, out) = handle_message(
        GOMSPACE,
        Utc::now(),
        &companies,
        &mut SeenSet::new(10),
        Some(&check),
    )
    .await;
    assert!(matches!(out.as_slice(), [Disposition::New(_)]), "{out:?}");
    db.drop_db().await;
}
