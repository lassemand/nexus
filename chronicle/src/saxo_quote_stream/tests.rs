//! The stream against a fake Saxo: an axum app serving the streaming connect,
//! the authorize call and the info-price subscription calls. No test needs
//! the real Saxo.

use super::*;
use crate::saxo_quotes::QuoteInstrument;
use alpha::saxo::stream::{encode_message, RESET_SUBSCRIPTIONS_REF};
use alpha::saxo::{SaxoToken, SharedToken};
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, post, put};
use axum::Json;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use tokio_tungstenite::tungstenite::Message as ClientMessage;

#[derive(Default)]
struct FakeInner {
    sockets: Vec<(String, mpsc::UnboundedSender<Option<Vec<u8>>>)>,
    connects: Vec<(String, String)>,
    closed: usize,
    active: HashMap<(String, String), Vec<u64>>,
    posts: Vec<(String, String, Vec<u64>, Option<String>)>,
    deletes: Vec<(String, Option<String>)>,
    authorizes: Vec<(String, String)>,
    snapshots: HashMap<u64, String>,
    refuse: HashSet<u64>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<FakeInner>>);

impl Fake {
    fn with<R>(&self, f: impl FnOnce(&mut FakeInner) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }

    fn set_snapshot(&self, uic: u64, json: &str) {
        self.with(|i| i.snapshots.insert(uic, json.to_string()));
    }

    fn latest_ctx(&self) -> Option<String> {
        self.with(|i| i.sockets.last().map(|(c, _)| c.clone()))
    }

    fn active_uics(&self) -> Vec<u64> {
        let ctx = self.latest_ctx();
        let mut v: Vec<u64> = self.with(|i| {
            i.active
                .iter()
                .filter(|((c, _), _)| Some(c) == ctx.as_ref())
                .flat_map(|(_, u)| u.clone())
                .collect()
        });
        v.sort_unstable();
        v
    }

    fn ref_of(&self, uic: u64) -> Option<String> {
        let ctx = self.latest_ctx()?;
        self.with(|i| {
            i.active
                .iter()
                .find(|((c, _), u)| *c == ctx && u.contains(&uic))
                .map(|((_, r), _)| r.clone())
        })
    }

    fn push(&self, reference: &str, payload: &str) {
        let frame = encode_message(1, reference, payload.as_bytes());
        self.with(|i| {
            if let Some((_, tx)) = i.sockets.last() {
                let _ = tx.send(Some(frame));
            }
        });
    }

    fn drop_sockets(&self) {
        self.with(|i| {
            for (_, tx) in i.sockets.drain(..) {
                let _ = tx.send(None);
            }
        });
    }
}

#[derive(Deserialize)]
struct ConnectParams {
    #[serde(rename = "contextId")]
    context_id: String,
}

#[derive(Deserialize)]
struct AuthorizeParams {
    contextid: String,
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .to_string()
}

async fn fake_connect(
    ws: WebSocketUpgrade,
    Query(p): Query<ConnectParams>,
    headers: HeaderMap,
    State(fake): State<Fake>,
) -> Response {
    let token = bearer(&headers);
    ws.on_upgrade(move |mut socket| async move {
        let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
        fake.with(|i| {
            i.connects.push((p.context_id.clone(), token));
            i.sockets.push((p.context_id.clone(), tx));
        });
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Some(frame)) => {
                        if socket.send(Message::Binary(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                },
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
        fake.with(|i| {
            i.closed += 1;
            i.sockets.retain(|(c, _)| *c != p.context_id);
        });
    })
}

async fn fake_subscribe(
    State(fake): State<Fake>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, String) {
    let ctx = body["ContextId"].as_str().unwrap_or_default().to_string();
    let reference = body["ReferenceId"].as_str().unwrap_or_default().to_string();
    let replace = body["ReplaceReferenceId"].as_str().map(str::to_string);
    assert_eq!(body["Arguments"]["AssetType"], "Stock");
    assert_eq!(
        body["Arguments"]["FieldGroups"],
        serde_json::json!(["Quote", "DisplayAndFormat", "InstrumentPriceDetails"])
    );
    let uics: Vec<u64> = body["Arguments"]["Uics"]
        .as_str()
        .unwrap_or_default()
        .split(',')
        .filter_map(|u| u.parse().ok())
        .collect();
    fake.with(|i| {
        i.posts
            .push((ctx.clone(), reference.clone(), uics.clone(), replace.clone()));
        if uics.iter().any(|u| i.refuse.contains(u)) {
            return (StatusCode::BAD_REQUEST, r#"{"ErrorCode":"InvalidRequest"}"#.into());
        }
        if let Some(old) = replace {
            i.active.remove(&(ctx.clone(), old));
        }
        i.active.insert((ctx.clone(), reference.clone()), uics.clone());
        let data: Vec<String> = uics
            .iter()
            .filter_map(|u| i.snapshots.get(u).cloned())
            .collect();
        (
            StatusCode::CREATED,
            format!(
                r#"{{"ContextId":"{ctx}","ReferenceId":"{reference}","InactivityTimeout":30,"Snapshot":{{"Data":[{}]}}}}"#,
                data.join(",")
            ),
        )
    })
}

async fn fake_delete_one(
    State(fake): State<Fake>,
    Path((ctx, reference)): Path<(String, String)>,
) -> StatusCode {
    fake.with(|i| {
        i.deletes.push((ctx.clone(), Some(reference.clone())));
        i.active.remove(&(ctx, reference));
    });
    StatusCode::ACCEPTED
}

async fn fake_delete_all(State(fake): State<Fake>, Path(ctx): Path<String>) -> StatusCode {
    fake.with(|i| {
        i.deletes.push((ctx.clone(), None));
        i.active.retain(|(c, _), _| *c != ctx);
    });
    StatusCode::ACCEPTED
}

async fn fake_authorize(
    State(fake): State<Fake>,
    Query(p): Query<AuthorizeParams>,
    headers: HeaderMap,
) -> StatusCode {
    let token = bearer(&headers);
    fake.with(|i| i.authorizes.push((p.contextid, token)));
    StatusCode::ACCEPTED
}

async fn start_fake() -> (Fake, SocketAddr) {
    let fake = Fake::default();
    let app = Router::new()
        .route("/connect", get(fake_connect))
        .route("/authorize", put(fake_authorize))
        .route("/trade/v1/infoprices/subscriptions", post(fake_subscribe))
        .route(
            "/trade/v1/infoprices/subscriptions/{ctx}/{reference}",
            delete(fake_delete_one),
        )
        .route(
            "/trade/v1/infoprices/subscriptions/{ctx}",
            delete(fake_delete_all),
        )
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (fake, addr)
}

fn snapshot(uic: u64, bid: &str, ask: &str) -> String {
    format!(
        r#"{{"Uic":{uic},"AssetType":"Stock","LastUpdated":"2026-11-10T09:00:01.123Z",
            "DisplayAndFormat":{{"Currency":"SEK","Decimals":2}},
            "Quote":{{"Bid":{bid},"Ask":{ask},"BidSize":1200,"AskSize":800,
                     "PriceTypeBid":"Tradable","PriceTypeAsk":"Tradable",
                     "MarketState":"Open","DelayedByMinutes":0}}}}"#
    )
}

fn fast_settings() -> StreamSettings {
    StreamSettings {
        heartbeat_timeout: Duration::from_secs(5),
        client_queue: 1024,
        max_uics_per_subscription: 100,
        debounce: Duration::from_millis(20),
        idle_disconnect: Duration::from_secs(60),
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        tick: Duration::from_millis(50),
        slow_client_grace: Duration::from_secs(10),
        unresolved_error_after: Duration::from_secs(60),
        resolve_retry: Duration::from_secs(60),
    }
}

struct Harness {
    fake: Fake,
    handle: QuoteStreamHandle,
    token: SharedToken,
    slot: TokenSlot,
    uics: UicMap,
    unknown: mpsc::Receiver<Pair>,
    addr: SocketAddr,
}

fn valid_token(value: &str) -> SharedToken {
    Arc::new(std::sync::Mutex::new(SaxoToken {
        access_token: value.into(),
        expires_at: Utc::now() + chrono::Duration::hours(1),
    }))
}

async fn start(settings: StreamSettings, with_token: bool) -> Harness {
    let (fake, saxo) = start_fake().await;
    let uics: UicMap = Arc::default();
    {
        let mut map = uics.write().unwrap();
        for (t, u) in [("AAA", 1), ("BBB", 2), ("CCC", 3), ("ABC", 4711)] {
            map.insert(saxo_quotes::key(t, "XSTO"), QuoteInstrument { uic: u });
        }
    }
    for u in [1, 2, 3] {
        fake.set_snapshot(u, &snapshot(u, "10.1", "10.2"));
    }
    fake.set_snapshot(4711, &snapshot(4711, "67.3", "67.4"));
    let token = valid_token("token-1");
    let slot: TokenSlot = Arc::new(OnceLock::new());
    if with_token {
        let _ = slot.set(token.clone());
    }
    let (unknown_tx, unknown) = mpsc::channel(16);
    let client = InfoPriceStreamClient::new(
        reqwest::Client::new(),
        format!("http://{saxo}"),
        format!("ws://{saxo}/connect"),
        format!("http://{saxo}/authorize"),
    );
    let handle = spawn(
        StreamDeps {
            client,
            token: slot.clone(),
            uics: uics.clone(),
            unknown: unknown_tx,
        },
        settings,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(handle.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    Harness {
        fake,
        handle,
        token,
        slot,
        uics,
        unknown,
        addr,
    }
}

type ClientSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestClient {
    ws: ClientSocket,
}

impl TestClient {
    async fn connect(h: &Harness) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/quotes/stream", h.addr))
            .await
            .unwrap();
        TestClient { ws }
    }

    async fn watch(&mut self, pairs: &[&str]) {
        let watch: Vec<serde_json::Value> = pairs
            .iter()
            .map(|t| serde_json::json!({"ticker": t, "mic": "XSTO"}))
            .collect();
        let text = serde_json::json!({ "watch": watch }).to_string();
        self.ws.send(ClientMessage::Text(text)).await.unwrap();
    }

    async fn next_json(&mut self) -> serde_json::Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("a message within 5 s")
                .expect("socket open")
                .expect("no socket error");
            if let ClientMessage::Text(t) = msg {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    async fn next_of(&mut self, kind: &str) -> serde_json::Value {
        loop {
            let v = self.next_json().await;
            if v["type"] == kind {
                return v;
            }
        }
    }

    async fn heartbeat_where(
        &mut self,
        f: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        loop {
            let v = self.next_of("heartbeat").await;
            if f(&v) {
                return v;
            }
        }
    }
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_delta_is_merged_and_the_full_quote_is_pushed() {
    let h = start(fast_settings(), true).await;
    let mut c = TestClient::connect(&h).await;
    c.watch(&["abc"]).await;
    let q = c.next_of("quote").await;
    assert_eq!(q["ticker"], "ABC");
    assert_eq!(q["mic"], "XSTO");
    assert_eq!(q["bid"], "67.30");
    assert_eq!(q["ask"], "67.40");
    assert_eq!(q["bid_size"], "1200");
    assert_eq!(q["bid_type"], "Tradable");
    assert_eq!(q["market_state"], "Open");
    assert_eq!(q["delayed_minutes"], 0);
    assert_eq!(q["currency"], "SEK");
    assert_eq!(q["saxo_time"], "2026-11-10T09:00:01.123Z");
    assert!(q["received_at"].as_str().unwrap().ends_with('Z'));
    let seq = q["seq"].as_u64().unwrap();

    let reference = h.fake.ref_of(4711).unwrap();
    h.fake
        .push(&reference, r#"[{"Uic":4711,"Quote":{"Ask":67.5}}]"#);
    let q = c.next_of("quote").await;
    assert_eq!(q["ask"], "67.50");
    assert_eq!(q["bid"], "67.30");
    assert_eq!(q["ask_type"], "Tradable");
    assert_eq!(q["seq"].as_u64().unwrap(), seq + 1);

    h.fake.push(
        &reference,
        r#"[{"Uic":4711,"Quote":{"PriceTypeAsk":"Indicative"}}]"#,
    );
    let q = c.next_of("quote").await;
    assert_eq!(q["ask_type"], "Indicative");
    assert_eq!(q["bid_type"], "Tradable");
    assert_eq!(q["ask"], "67.50");
    assert_eq!(q["bid"], "67.30");
    assert_eq!(q["seq"].as_u64().unwrap(), seq + 2);
}

#[tokio::test]
async fn prices_are_padded_decimal_strings() {
    let h = start(fast_settings(), true).await;
    h.fake.set_snapshot(
        4711,
        r#"{"Uic":4711,"DisplayAndFormat":{"Decimals":2},"Quote":{"Ask":101.5,"Bid":0.30000000000000004}}"#,
    );
    let mut c = TestClient::connect(&h).await;
    c.watch(&["ABC"]).await;
    let q = c.next_of("quote").await;
    assert_eq!(q["ask"], "101.50");
    assert_eq!(q["bid"], "0.30000000000000004");
    assert!(q["bid_size"].is_null());
    assert!(q["currency"].is_null());
}

#[tokio::test]
async fn overlapping_clients_share_one_subscription_per_instrument() {
    let h = start(fast_settings(), true).await;
    let mut a = TestClient::connect(&h).await;
    let mut b = TestClient::connect(&h).await;
    a.watch(&["AAA", "BBB"]).await;
    b.watch(&["BBB", "CCC"]).await;
    eventually("all three subscribed", || {
        h.fake.active_uics() == vec![1, 2, 3]
    })
    .await;
    a.next_of("quote").await;
    b.next_of("quote").await;

    drop(b);
    eventually("only CCC unsubscribed", || {
        h.fake.active_uics() == vec![1, 2]
    })
    .await;
    let posts = h.fake.with(|i| i.posts.clone());
    for (_, _, uics, _) in &posts {
        let mut u = uics.clone();
        u.sort_unstable();
        u.dedup();
        assert_eq!(u.len(), uics.len(), "{posts:?}");
    }
    let reference = h.fake.ref_of(2).unwrap();
    h.fake
        .push(&reference, r#"[{"Uic":2,"Quote":{"Bid":10.0}}]"#);
    loop {
        let q = a.next_of("quote").await;
        if q["ticker"] == "BBB" && q["bid"] == "10.00" {
            break;
        }
        assert_eq!(
            q["bid"], "10.10",
            "only a replacement's snapshot may come first: {q}"
        );
    }
}

#[tokio::test]
async fn a_newly_watched_pair_gets_its_current_quote_first() {
    let h = start(fast_settings(), true).await;
    let mut a = TestClient::connect(&h).await;
    a.watch(&["ABC"]).await;
    let first = a.next_of("quote").await;
    let reference = h.fake.ref_of(4711).unwrap();
    h.fake
        .push(&reference, r#"[{"Uic":4711,"Quote":{"Bid":67.2}}]"#);
    let latest = a.next_of("quote").await;
    assert_eq!(
        latest["seq"].as_u64(),
        Some(first["seq"].as_u64().unwrap() + 1)
    );
    let posts = h.fake.with(|i| i.posts.len());

    let mut b = TestClient::connect(&h).await;
    b.watch(&["ABC"]).await;
    let q = b.next_of("quote").await;
    assert_eq!(q["bid"], "67.20");
    assert_eq!(q["seq"], latest["seq"]);
    h.fake
        .push(&reference, r#"[{"Uic":4711,"Quote":{"Bid":67.1}}]"#);
    let q = b.next_of("quote").await;
    assert_eq!(q["bid"], "67.10");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.fake.with(|i| i.posts.len()), posts);
}

#[tokio::test]
async fn a_saxo_disconnect_is_reported_and_the_new_snapshot_replaces_the_quote() {
    let h = start(fast_settings(), true).await;
    let mut c = TestClient::connect(&h).await;
    c.watch(&["ABC"]).await;
    let q = c.next_of("quote").await;
    let seq = q["seq"].as_u64().unwrap();
    c.heartbeat_where(|v| v["saxo_connected"] == true).await;
    let first_ctx = h.fake.latest_ctx().unwrap();

    h.fake.set_snapshot(4711, &snapshot(4711, "70", "70.1"));
    h.fake.drop_sockets();
    c.heartbeat_where(|v| v["saxo_connected"] == false).await;

    let q = c.next_of("quote").await;
    assert_eq!(q["bid"], "70.00");
    assert_eq!(q["ask"], "70.10");
    assert!(q["seq"].as_u64().unwrap() > seq);
    c.heartbeat_where(|v| v["saxo_connected"] == true).await;
    assert_ne!(h.fake.latest_ctx().unwrap(), first_ctx);
    eventually("old context deleted", || {
        h.fake
            .with(|i| i.deletes.contains(&(first_ctx.clone(), None)))
    })
    .await;
}

#[tokio::test]
async fn reset_subscriptions_re_creates_every_subscription() {
    let h = start(
        StreamSettings {
            max_uics_per_subscription: 1,
            ..fast_settings()
        },
        true,
    )
    .await;
    let mut c = TestClient::connect(&h).await;
    c.watch(&["AAA", "BBB"]).await;
    eventually("both subscribed", || h.fake.active_uics() == vec![1, 2]).await;
    let before: HashSet<String> = [h.fake.ref_of(1).unwrap(), h.fake.ref_of(2).unwrap()].into();
    assert_eq!(before.len(), 2);

    let payload =
        format!(r#"[{{"ReferenceId":"{RESET_SUBSCRIPTIONS_REF}","TargetReferenceIds":[]}}]"#);
    h.fake.push(RESET_SUBSCRIPTIONS_REF, &payload);
    eventually("both re-created", || {
        let now: HashSet<String> = [h.fake.ref_of(1), h.fake.ref_of(2)]
            .into_iter()
            .flatten()
            .collect();
        now.len() == 2 && now.is_disjoint(&before)
    })
    .await;
    assert_eq!(h.fake.active_uics(), vec![1, 2]);
    let replaced: HashSet<String> = h
        .fake
        .with(|i| i.posts.iter().filter_map(|p| p.3.clone()).collect());
    assert_eq!(replaced, before);
}

#[tokio::test]
async fn without_a_token_clients_stay_and_hear_saxo_is_down() {
    let h = start(fast_settings(), false).await;
    let mut c = TestClient::connect(&h).await;
    c.watch(&["ABC"]).await;
    for _ in 0..5 {
        let hb = c.next_of("heartbeat").await;
        assert_eq!(hb["saxo_connected"], false);
        assert!(hb["at"].as_str().unwrap().ends_with('Z'));
    }
    assert!(h.fake.with(|i| i.connects.is_empty()));
    let _ = h.slot.set(h.token.clone());
    c.next_of("quote").await;
    c.heartbeat_where(|v| v["saxo_connected"] == true).await;
}

#[tokio::test]
async fn a_silent_connection_is_replaced() {
    let h = start(
        StreamSettings {
            heartbeat_timeout: Duration::from_millis(300),
            ..fast_settings()
        },
        true,
    )
    .await;
    let mut c = TestClient::connect(&h).await;
    c.watch(&["ABC"]).await;
    c.next_of("quote").await;
    eventually("a reconnect", || h.fake.with(|i| i.connects.len() >= 2)).await;
}

#[tokio::test]
async fn a_rotated_token_is_pushed_to_the_live_connection() {
    let h = start(fast_settings(), true).await;
    let mut c = TestClient::connect(&h).await;
    c.watch(&["ABC"]).await;
    c.next_of("quote").await;
    assert_eq!(h.fake.with(|i| i.connects[0].1.clone()), "token-1");
    h.token.lock().unwrap().access_token = "token-2".into();
    let ctx = h.fake.latest_ctx().unwrap();
    eventually("re-authorized", || {
        h.fake
            .with(|i| i.authorizes.contains(&(ctx.clone(), "token-2".to_string())))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.fake.with(|i| i.authorizes.len()), 1);
    assert_eq!(h.fake.with(|i| i.connects.len()), 1);
}

#[tokio::test]
async fn saxo_is_disconnected_once_nobody_has_watched_for_a_while() {
    let h = start(
        StreamSettings {
            idle_disconnect: Duration::from_millis(300),
            ..fast_settings()
        },
        true,
    )
    .await;
    let mut c = TestClient::connect(&h).await;
    c.watch(&["ABC"]).await;
    c.next_of("quote").await;
    c.watch(&[]).await;
    eventually("the subscription deleted", || {
        h.fake.active_uics().is_empty()
    })
    .await;
    assert_eq!(h.fake.with(|i| i.closed), 0);
    eventually("the socket closed", || h.fake.with(|i| i.closed == 1)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.fake.with(|i| i.connects.len()), 1);
    c.heartbeat_where(|v| v["saxo_connected"] == false).await;
}

#[tokio::test]
async fn an_unknown_pair_is_resolved_in_the_background_then_subscribed() {
    let mut h = start(
        StreamSettings {
            unresolved_error_after: Duration::from_millis(150),
            ..fast_settings()
        },
        true,
    )
    .await;
    let mut c = TestClient::connect(&h).await;
    c.watch(&["NEW"]).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), h.unknown.recv())
            .await
            .unwrap(),
        Some(("NEW".to_string(), "XSTO".to_string()))
    );
    let hb = c
        .heartbeat_where(|v| !v["unresolved"].as_array().unwrap().is_empty())
        .await;
    assert_eq!(
        hb["unresolved"],
        serde_json::json!([{"ticker":"NEW","mic":"XSTO"}])
    );
    let err = c.next_of("error").await;
    assert_eq!(
        err["pair"],
        serde_json::json!({"ticker":"NEW","mic":"XSTO"})
    );

    h.fake.set_snapshot(99, &snapshot(99, "5", "5.5"));
    h.uics
        .write()
        .unwrap()
        .insert(saxo_quotes::key("NEW", "XSTO"), QuoteInstrument { uic: 99 });
    let q = c.next_of("quote").await;
    assert_eq!(q["ticker"], "NEW");
    assert_eq!(q["ask"], "5.50");
}

#[tokio::test]
async fn a_refused_instrument_is_retried_alone_and_reported() {
    let h = start(fast_settings(), true).await;
    h.fake.with(|i| i.refuse.insert(2));
    let mut c = TestClient::connect(&h).await;
    c.watch(&["AAA", "BBB"]).await;
    let err = c.next_of("error").await;
    assert_eq!(
        err["pair"],
        serde_json::json!({"ticker":"BBB","mic":"XSTO"})
    );
    eventually("AAA subscribed alone", || h.fake.active_uics() == vec![1]).await;
}

#[tokio::test]
async fn a_message_that_is_not_a_watch_list_is_an_error_not_a_close() {
    let h = start(fast_settings(), true).await;
    let mut c = TestClient::connect(&h).await;
    c.ws.send(ClientMessage::Text("hello".into()))
        .await
        .unwrap();
    let err = c.next_of("error").await;
    assert!(err["pair"].is_null());
    c.next_of("heartbeat").await;
}

#[tokio::test]
async fn a_delta_before_its_snapshot_is_applied_after_it() {
    let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
    let (unknown, _unknown_rx) = mpsc::channel(4);
    let uics: UicMap = Arc::default();
    uics.write().unwrap().insert(
        saxo_quotes::key("ABC", "XSTO"),
        QuoteInstrument { uic: 4711 },
    );
    let deps = StreamDeps {
        client: InfoPriceStreamClient::new(
            reqwest::Client::new(),
            "http://127.0.0.1:9",
            "ws://127.0.0.1:9",
            "http://127.0.0.1:9",
        ),
        token: Arc::new(OnceLock::new()),
        uics,
        unknown,
    };
    let mut hub = Hub::new(deps, fast_settings(), ev_tx);
    let (tx, mut rx) = mpsc::channel(16);
    let (close, _close_rx) = oneshot::channel();
    hub.on_command(Command::Join { id: 1, tx, close });
    hub.on_command(Command::Watch {
        id: 1,
        pairs: vec![("ABC".into(), "XSTO".into())],
    });
    let (fake, saxo) = start_fake().await;
    drop(fake);
    let client = InfoPriceStreamClient::new(
        reqwest::Client::new(),
        format!("http://{saxo}"),
        format!("ws://{saxo}/connect"),
        format!("http://{saxo}/authorize"),
    );
    let socket = client.connect("ctx", "t").await.unwrap();
    hub.conn = Conn::Live(Box::new(Live {
        ctx: "ctx".into(),
        socket,
        token: "t".into(),
        last_msg: Instant::now(),
        got_message: true,
        subs: [(
            "q1".to_string(),
            Sub {
                uics: vec![4711],
                active: false,
                buffered: Vec::new(),
            },
        )]
        .into(),
        reauth_inflight: false,
        awaiting_snapshot: HashSet::new(),
        resynced: true,
    }));
    let frame = encode_message(1, "q1", br#"[{"Uic":4711,"Quote":{"Ask":67.9}}]"#);
    hub.on_frame(Some(Ok(SaxoMessage::Binary(frame))));
    let snap = saxo::parse_deltas(snapshot(4711, "67.3", "67.4").as_bytes()).unwrap();
    hub.on_subscribed(
        "ctx".into(),
        "q1".into(),
        Ok(Subscribed {
            snapshot: snap,
            inactivity_timeout_secs: None,
        }),
        Instant::now(),
        Utc::now(),
    );
    let mut quotes = Vec::new();
    while let Ok(m) = rx.try_recv() {
        let v: serde_json::Value = serde_json::from_str(m.text.as_str()).unwrap();
        if v["type"] == "quote" {
            quotes.push(v);
        }
    }
    assert_eq!(quotes.len(), 2);
    assert_eq!(quotes[0]["ask"], "67.40");
    assert_eq!(quotes[1]["ask"], "67.90");
    assert_eq!(quotes[1]["bid"], "67.30");
    assert_eq!(quotes[1]["seq"], 2);
}

/// The slow client's queue fills: it receives the latest quote per
/// instrument instead of every update, and the fast client's fan-out (Saxo
/// frame pushed to client frame read, both loopback hops included) stays
/// under 5 ms at p99.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_client_gets_coalesced_quotes_and_does_not_slow_the_fast_one() {
    let h = start(fast_settings(), true).await;
    let mut slow = h.handle.join_with_queue(4).await.unwrap();
    h.handle
        .watch(
            slow.id,
            vec![
                ("AAA".into(), "XSTO".into()),
                ("BBB".into(), "XSTO".into()),
                ("CCC".into(), "XSTO".into()),
            ],
        )
        .await;
    let mut fast = TestClient::connect(&h).await;
    fast.watch(&["AAA", "BBB", "CCC"]).await;
    eventually("all subscribed", || h.fake.active_uics() == vec![1, 2, 3]).await;
    for _ in 0..3 {
        fast.next_of("quote").await;
    }
    let refs: Vec<String> = [1, 2, 3]
        .iter()
        .map(|u| h.fake.ref_of(*u).unwrap())
        .collect();

    let mut times = Vec::new();
    let mut last_seq: HashMap<String, u64> = HashMap::new();
    for i in 0..600u64 {
        let k = (i % 3) as usize;
        let uic = k as u64 + 1;
        let payload = format!(
            r#"[{{"Uic":{uic},"Quote":{{"Bid":{}.{:02}}}}}]"#,
            10 + i / 100,
            i % 100
        );
        let started = Instant::now();
        h.fake.push(&refs[k], &payload);
        let q = fast.next_of("quote").await;
        times.push(started.elapsed());
        last_seq.insert(
            q["ticker"].as_str().unwrap().to_string(),
            q["seq"].as_u64().unwrap(),
        );
    }
    times.sort();
    let p99 = times[times.len() * 99 / 100];
    assert!(p99 < Duration::from_millis(5), "fast client p99 {p99:?}");

    let mut seen: Vec<serde_json::Value> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        while let Ok(m) = slow.rx.try_recv() {
            seen.push(serde_json::from_str(m.text.as_str()).unwrap());
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let quotes: Vec<&serde_json::Value> = seen.iter().filter(|v| v["type"] == "quote").collect();
    assert!(
        quotes.len() <= 4 + 3,
        "slow client got {} quotes",
        quotes.len()
    );
    for (ticker, seq) in &last_seq {
        let latest = quotes
            .iter()
            .filter(|q| q["ticker"] == ticker.as_str())
            .map(|q| q["seq"].as_u64().unwrap())
            .max();
        assert_eq!(latest, Some(*seq), "{ticker}");
    }
    assert!(slow.close.try_recv().is_err());
}

#[tokio::test]
async fn a_client_that_stays_full_is_closed_with_a_reason() {
    let h = start(
        StreamSettings {
            slow_client_grace: Duration::from_millis(200),
            ..fast_settings()
        },
        true,
    )
    .await;
    let mut slow = h.handle.join_with_queue(1).await.unwrap();
    h.handle
        .watch(slow.id, vec![("ABC".into(), "XSTO".into())])
        .await;
    let reason = tokio::time::timeout(Duration::from_secs(3), &mut slow.close)
        .await
        .expect("closed in time")
        .unwrap();
    assert_eq!(reason, REASON_SLOW);
}
