//! Saxo streaming info prices (NEX-270): the WebSocket connection, frame
//! decoding, control messages, delta merging and the subscription calls.
//!
//! Restored from the client NEX-193 removed (`7c8534f^:alpha/src/saxo/stream.rs`)
//! and adapted: it subscribes to info prices instead of prices, keeps full
//! quotes instead of building bars, and takes the WebSocket URL and the
//! authorize URL as two settings (the removed client used one value for both,
//! so neither scheme worked).
//!
//! # Read only
//!
//! [`InfoPriceStreamClient`] builds exactly three kinds of URL: the info-price
//! subscription collection, one subscription (or all of one context) under it,
//! and the streaming authorize URL. No order, position or account endpoint may
//! be added here.
//!
//! # Token constraint
//!
//! The caller passes the access token it read from saxo-stream's
//! [`super::SharedToken`]; nothing here authenticates. A rotated token is
//! pushed to a live connection with [`InfoPriceStreamClient::reauthorize`].
//!
//! # Binary frame format
//!
//! ```text
//! Offset  Size    Content
//! 0       8 bytes Message ID (u64 little-endian)
//! 8       2 bytes Reserved
//! 10      1 byte  Reference ID length (N)
//! 11      N bytes Reference ID (ASCII)
//! 11+N    1 byte  Payload format: 0=JSON, 1=Protobuf
//! 12+N    4 bytes Payload size (u32 little-endian)
//! 16+N    var     Payload
//! ```
//!
//! # Deltas
//!
//! The subscription answer carries a full snapshot; every later message
//! carries only the fields that changed. [`StreamQuote::apply`] merges one
//! [`InfoPriceDelta`] into the stored quote: an absent field keeps its value,
//! an explicit `null` clears it. Prices are kept unpadded, exactly as Saxo
//! sent the digits, and padded to `DisplayAndFormat.Decimals` only by
//! [`StreamQuote::to_quote`], so a `Decimals` change applies to every price.

use super::api_metrics::{self, Endpoint};
use super::quote::{pad_to, raw_decimal, SaxoQuote, FIELD_GROUPS};
use rand::Rng;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::time::Instant;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Path of the info-price subscription collection, the only REST path this
/// module calls besides the authorize URL.
pub const SUBSCRIPTIONS_PATH: &str = "/trade/v1/infoprices/subscriptions";

/// Live streaming connect URL (`SAXO_STREAMING_WS_URL`).
pub const LIVE_WS_URL: &str = "wss://live-streaming.saxobank.com/oapi/streaming/ws/connect";
/// Live streaming re-authorization URL (`SAXO_STREAMING_AUTHORIZE_URL`).
pub const LIVE_AUTHORIZE_URL: &str =
    "https://live-streaming.saxobank.com/oapi/streaming/ws/authorize";
/// SIM streaming connect URL.
pub const SIM_WS_URL: &str = "wss://sim-streaming.saxobank.com/sim/oapi/streaming/ws/connect";
/// SIM streaming re-authorization URL.
pub const SIM_AUTHORIZE_URL: &str =
    "https://sim-streaming.saxobank.com/sim/oapi/streaming/ws/authorize";

/// Reference id of Saxo's heartbeat control message.
pub const HEARTBEAT_REF: &str = "_heartbeat";
/// Reference id of Saxo's disconnect control message.
pub const DISCONNECT_REF: &str = "_disconnect";
/// Reference id of Saxo's reset-subscriptions control message.
pub const RESET_SUBSCRIPTIONS_REF: &str = "_resetsubscriptions";

/// Longest context or reference id Saxo accepts.
pub const MAX_ID_LEN: usize = 50;

/// A live streaming connection.
pub type SaxoSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Why a streaming call failed.
#[derive(Debug, Error)]
pub enum StreamError {
    /// The WebSocket could not be opened or failed.
    #[error("WebSocket error: {0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
    /// No HTTP response (connect, TLS, timeout, body read).
    #[error("streaming request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// Saxo answered a non-2xx status; `body` is the start of its body.
    #[error("Saxo returned {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
        retry_after: Option<std::time::Duration>,
    },
    /// A 2xx whose body is not what Saxo documents.
    #[error("undecodable subscription response: {0}")]
    Decode(String),
    /// A configured URL is not a URL.
    #[error("invalid streaming URL: {0}")]
    Url(#[from] url::ParseError),
    /// The access token cannot go into a header.
    #[error("access token is not a valid header value")]
    Token,
}

impl From<tokio_tungstenite::tungstenite::Error> for StreamError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        StreamError::WebSocket(Box::new(e))
    }
}

impl StreamError {
    /// Whether the same call can succeed later (no response, 408, 429, 5xx,
    /// or a WebSocket failure).
    pub fn is_transient(&self) -> bool {
        match self {
            StreamError::WebSocket(_) => true,
            StreamError::Http(e) => super::is_transient_reqwest(e),
            StreamError::Status { status, .. } => super::is_transient_status(*status),
            StreamError::Decode(_) | StreamError::Url(_) | StreamError::Token => false,
        }
    }

    /// Whether Saxo rejected the access token (401 or 403), which says
    /// nothing about the request itself.
    pub fn is_auth(&self) -> bool {
        matches!(self, StreamError::Status { status, .. }
            if *status == reqwest::StatusCode::UNAUTHORIZED
                || *status == reqwest::StatusCode::FORBIDDEN)
    }

    /// Saxo's `Retry-After`, when the failed call carried one.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            StreamError::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The error a call reports when there is no usable access token to send.
    pub fn no_token() -> Self {
        StreamError::Status {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "no valid access token".to_string(),
            retry_after: None,
        }
    }
}

/// One message inside a binary frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMessage<'a> {
    /// Saxo's message id.
    pub message_id: u64,
    /// The subscription's reference id, or a control id starting with `_`.
    pub reference_id: &'a str,
    /// 0 for JSON, 1 for protobuf.
    pub format: u8,
    /// The payload bytes.
    pub payload: &'a [u8],
}

/// A frame that does not follow the documented layout.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("malformed streaming frame at byte {offset}: {reason}")]
pub struct FrameError {
    /// Where the bad message starts.
    pub offset: usize,
    /// What is wrong.
    pub reason: &'static str,
}

/// Splits a binary frame into its messages. The messages decoded before a
/// malformed one are lost with it: a frame is accepted whole or not at all.
pub fn decode_frame(data: &[u8]) -> Result<Vec<StreamMessage<'_>>, FrameError> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let bad = |reason| FrameError { offset, reason };
        let rest = &data[offset..];
        if rest.len() < 11 {
            return Err(bad("truncated header"));
        }
        let message_id = u64::from_le_bytes(rest[0..8].try_into().map_err(|_| bad("id"))?);
        let ref_len = usize::from(rest[10]);
        let header_end = 11 + ref_len;
        if rest.len() < header_end + 5 {
            return Err(bad("truncated header"));
        }
        let reference_id = std::str::from_utf8(&rest[11..header_end])
            .map_err(|_| bad("reference id is not ASCII"))?;
        let format = rest[header_end];
        let size_bytes: [u8; 4] = rest[header_end + 1..header_end + 5]
            .try_into()
            .map_err(|_| bad("size"))?;
        let size = usize::try_from(u32::from_le_bytes(size_bytes)).map_err(|_| bad("size"))?;
        let payload_start = header_end + 5;
        let payload_end = payload_start
            .checked_add(size)
            .ok_or_else(|| bad("size overflows"))?;
        if rest.len() < payload_end {
            return Err(bad("truncated payload"));
        }
        out.push(StreamMessage {
            message_id,
            reference_id,
            format,
            payload: &rest[payload_start..payload_end],
        });
        offset += payload_end;
    }
    Ok(out)
}

/// Encodes one JSON message in Saxo's frame layout. Used by fake streaming
/// servers in tests; saxo-stream itself only decodes.
pub fn encode_message(message_id: u64, reference_id: &str, payload: &[u8]) -> Vec<u8> {
    let ref_bytes = reference_id.as_bytes();
    let ref_len = u8::try_from(ref_bytes.len()).expect("reference ids are at most 50 bytes");
    let size = u32::try_from(payload.len()).expect("test payloads are smaller than 4 GiB");
    let mut out = Vec::with_capacity(16 + ref_bytes.len() + payload.len());
    out.extend_from_slice(&message_id.to_le_bytes());
    out.extend_from_slice(&[0, 0, ref_len]);
    out.extend_from_slice(ref_bytes);
    out.push(0);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// One entry of a `_heartbeat` message.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Heartbeat {
    /// The subscription the heartbeat is about.
    #[serde(default)]
    pub originating_reference_id: String,
    /// `NoNewData`, `SubscriptionTemporarilyDisabled` or
    /// `SubscriptionPermanentlyDisabled`.
    #[serde(default)]
    pub reason: String,
}

/// A control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// The connection is alive; per-subscription reasons.
    Heartbeat(Vec<Heartbeat>),
    /// Saxo ends the connection; reconnect.
    Disconnect,
    /// Re-create these subscriptions under new reference ids; empty means all.
    ResetSubscriptions(Vec<String>),
    /// A control id this client does not know.
    Other(String),
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawControl {
    #[serde(default)]
    heartbeats: Vec<Heartbeat>,
    #[serde(default)]
    target_reference_ids: Vec<String>,
}

/// Parses a control message (a reference id starting with `_`). Saxo sends the
/// payload as an array of objects; a single object is accepted too. `None`
/// when `reference_id` is not a control id.
pub fn parse_control(reference_id: &str, payload: &[u8]) -> Option<Control> {
    if !reference_id.starts_with('_') {
        return None;
    }
    let entries: Vec<RawControl> = serde_json::from_slice::<Vec<RawControl>>(payload)
        .or_else(|_| serde_json::from_slice::<RawControl>(payload).map(|c| vec![c]))
        .unwrap_or_default();
    Some(match reference_id {
        HEARTBEAT_REF => {
            Control::Heartbeat(entries.into_iter().flat_map(|e| e.heartbeats).collect())
        }
        DISCONNECT_REF => Control::Disconnect,
        RESET_SUBSCRIPTIONS_REF => Control::ResetSubscriptions(
            entries
                .into_iter()
                .flat_map(|e| e.target_reference_ids)
                .collect(),
        ),
        other => Control::Other(other.to_string()),
    })
}

/// `None`: the field was absent (unchanged). `Some(None)`: an explicit
/// `null` (cleared). `Some(Some(v))`: a new value.
type Patch<T> = Option<Option<T>>;

fn patch<'de, D, T>(d: D) -> Result<Patch<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QuoteDelta {
    #[serde(default, deserialize_with = "patch")]
    bid: Patch<Box<RawValue>>,
    #[serde(default, deserialize_with = "patch")]
    ask: Patch<Box<RawValue>>,
    #[serde(default, deserialize_with = "patch")]
    bid_size: Patch<Box<RawValue>>,
    #[serde(default, deserialize_with = "patch")]
    ask_size: Patch<Box<RawValue>>,
    #[serde(default, deserialize_with = "patch")]
    price_type_bid: Patch<String>,
    #[serde(default, deserialize_with = "patch")]
    price_type_ask: Patch<String>,
    #[serde(default, deserialize_with = "patch")]
    market_state: Patch<String>,
    #[serde(default, deserialize_with = "patch")]
    delayed_by_minutes: Patch<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DisplayDelta {
    #[serde(default, deserialize_with = "patch")]
    currency: Patch<String>,
    #[serde(default, deserialize_with = "patch")]
    decimals: Patch<u32>,
}

/// One info-price update for one instrument: a snapshot entry or a delta.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InfoPriceDelta {
    /// The instrument, when Saxo names it (always in list subscriptions).
    #[serde(default)]
    pub uic: Option<u64>,
    #[serde(default)]
    quote: Option<QuoteDelta>,
    #[serde(default)]
    display_and_format: Option<DisplayDelta>,
    #[serde(default, deserialize_with = "patch")]
    last_updated: Patch<String>,
}

/// Parses a streaming payload or snapshot `Data`: an array of updates, or one
/// update. Numbers stay raw text until they become [`Decimal`]s.
pub fn parse_deltas(payload: &[u8]) -> Result<Vec<InfoPriceDelta>, serde_json::Error> {
    serde_json::from_slice::<Vec<InfoPriceDelta>>(payload)
        .or_else(|_| serde_json::from_slice::<InfoPriceDelta>(payload).map(|d| vec![d]))
}

/// The merged, full info price of one instrument. Prices are unpadded; see
/// [`StreamQuote::to_quote`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamQuote {
    bid: Option<Decimal>,
    ask: Option<Decimal>,
    bid_size: Option<Decimal>,
    ask_size: Option<Decimal>,
    bid_type: Option<String>,
    ask_type: Option<String>,
    market_state: Option<String>,
    delayed_minutes: Option<i64>,
    currency: Option<String>,
    decimals: Option<u32>,
    last_updated: Option<String>,
}

fn set<T>(slot: &mut Option<T>, p: Patch<T>) {
    if let Some(v) = p {
        *slot = v;
    }
}

fn set_decimal(slot: &mut Option<Decimal>, p: Patch<Box<RawValue>>) {
    if let Some(v) = p {
        *slot = raw_decimal(&v);
    }
}

impl StreamQuote {
    /// A quote built from a snapshot entry alone: everything the entry does
    /// not carry is `None`.
    pub fn from_snapshot(entry: InfoPriceDelta) -> Self {
        let mut q = StreamQuote::default();
        q.apply(entry);
        q
    }

    /// Saxo's `LastUpdated` of the latest update, as sent.
    pub fn last_updated(&self) -> Option<&str> {
        self.last_updated.as_deref()
    }

    /// Merges one update: fields it carries replace the stored ones (an
    /// explicit `null` clears), every other field keeps its value.
    pub fn apply(&mut self, delta: InfoPriceDelta) {
        if let Some(q) = delta.quote {
            set_decimal(&mut self.bid, q.bid);
            set_decimal(&mut self.ask, q.ask);
            set_decimal(&mut self.bid_size, q.bid_size);
            set_decimal(&mut self.ask_size, q.ask_size);
            set(&mut self.bid_type, q.price_type_bid);
            set(&mut self.ask_type, q.price_type_ask);
            set(&mut self.market_state, q.market_state);
            set(&mut self.delayed_minutes, q.delayed_by_minutes);
        }
        if let Some(d) = delta.display_and_format {
            set(&mut self.currency, d.currency);
            set(&mut self.decimals, d.decimals);
        }
        set(&mut self.last_updated, delta.last_updated);
    }

    /// The quote as `GET /quote` reports one: prices padded to the current
    /// `Decimals`, sizes as sent.
    pub fn to_quote(&self) -> SaxoQuote {
        let price = |d: Option<Decimal>| d.map(|v| pad_to(v, self.decimals));
        SaxoQuote {
            bid: price(self.bid),
            ask: price(self.ask),
            bid_size: self.bid_size,
            ask_size: self.ask_size,
            bid_type: self.bid_type.clone(),
            ask_type: self.ask_type.clone(),
            market_state: self.market_state.clone(),
            delayed_minutes: self.delayed_minutes,
            currency: self.currency.clone(),
            decimals: self.decimals,
            last_updated: self.last_updated.clone(),
        }
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| std::time::Duration::from_secs_f64(s.min(3600.0)))
}

/// A new context id: `nexus-q-` and 16 random hex digits.
pub fn new_context_id() -> String {
    let n: u64 = rand::rng().random();
    format!("nexus-q-{n:016x}")
}

/// The answer to a subscription request.
#[derive(Debug)]
pub struct Subscribed {
    /// The snapshot entries, one per instrument Saxo knows.
    pub snapshot: Vec<InfoPriceDelta>,
    /// Saxo's `InactivityTimeout` in seconds, when it sent one.
    pub inactivity_timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawSubscribed {
    #[serde(default)]
    snapshot: Option<RawSnapshot>,
    #[serde(default)]
    inactivity_timeout: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawSnapshot {
    #[serde(default)]
    data: Option<Box<RawValue>>,
}

const ERROR_BODY_BYTES: usize = 300;

fn truncate(text: &str) -> String {
    let mut end = text.len().min(ERROR_BODY_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The streaming REST calls and the connect, with their URLs fixed at
/// construction.
#[derive(Clone)]
pub struct InfoPriceStreamClient {
    http: reqwest::Client,
    api_base: String,
    ws_url: String,
    authorize_url: String,
}

impl InfoPriceStreamClient {
    /// `api_base` is the REST gateway (e.g. `https://gateway.saxobank.com/openapi`),
    /// `ws_url` the streaming connect URL ([`LIVE_WS_URL`], [`SIM_WS_URL`]) and
    /// `authorize_url` the streaming authorize URL ([`LIVE_AUTHORIZE_URL`],
    /// [`SIM_AUTHORIZE_URL`]).
    pub fn new(
        http: reqwest::Client,
        api_base: impl Into<String>,
        ws_url: impl Into<String>,
        authorize_url: impl Into<String>,
    ) -> Self {
        Self {
            http,
            api_base: api_base.into().trim_end_matches('/').to_string(),
            ws_url: ws_url.into(),
            authorize_url: authorize_url.into(),
        }
    }

    /// The subscription collection URL.
    pub fn subscriptions_url(&self) -> Result<url::Url, StreamError> {
        Ok(url::Url::parse(&format!(
            "{}{SUBSCRIPTIONS_PATH}",
            self.api_base
        ))?)
    }

    /// The URL of one subscription, or of every subscription of `context_id`
    /// when `reference_id` is `None`.
    pub fn subscription_url(
        &self,
        context_id: &str,
        reference_id: Option<&str>,
    ) -> Result<url::Url, StreamError> {
        let mut url = self.subscriptions_url()?;
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|_| url::ParseError::RelativeUrlWithCannotBeABaseBase)?;
            segs.push(context_id);
            if let Some(r) = reference_id {
                segs.push(r);
            }
        }
        Ok(url)
    }

    /// The re-authorization URL of `context_id`.
    pub fn authorize_url(&self, context_id: &str) -> Result<url::Url, StreamError> {
        let mut url = url::Url::parse(&self.authorize_url)?;
        url.query_pairs_mut().append_pair("contextid", context_id);
        Ok(url)
    }

    /// The connect URL of `context_id`.
    pub fn connect_url(&self, context_id: &str) -> Result<url::Url, StreamError> {
        let mut url = url::Url::parse(&self.ws_url)?;
        url.query_pairs_mut().append_pair("contextId", context_id);
        Ok(url)
    }

    /// Opens the streaming connection of `context_id` with `access_token`.
    pub async fn connect(
        &self,
        context_id: &str,
        access_token: &str,
    ) -> Result<SaxoSocket, StreamError> {
        let mut request = self
            .connect_url(context_id)?
            .as_str()
            .into_client_request()?;
        let auth = HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|_| StreamError::Token)?;
        request.headers_mut().insert("Authorization", auth);
        let (socket, _) = tokio_tungstenite::connect_async(request).await?;
        Ok(socket)
    }

    /// Subscribes the stocks `uics` under `reference_id` and returns the
    /// snapshot. With `replace`, Saxo deletes that subscription once the new
    /// one exists.
    pub async fn subscribe(
        &self,
        access_token: &str,
        context_id: &str,
        reference_id: &str,
        uics: &[u64],
        replace: Option<&str>,
    ) -> Result<Subscribed, StreamError> {
        let uic_list = uics
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let field_groups: Vec<&str> = FIELD_GROUPS.split(',').collect();
        let mut body = serde_json::json!({
            "Arguments": {
                "AssetType": "Stock",
                "Uics": uic_list,
                "FieldGroups": field_groups,
            },
            "ContextId": context_id,
            "ReferenceId": reference_id,
            "RefreshRate": 0,
        });
        if let Some(old) = replace {
            body["ReplaceReferenceId"] = serde_json::Value::String(old.to_string());
        }
        let url = self.subscriptions_url()?;
        let started = Instant::now();
        let sent = self
            .http
            .post(url)
            .bearer_auth(access_token)
            .json(&body)
            .send()
            .await;
        api_metrics::record(
            Endpoint::InfoPriceSubscriptions,
            started,
            sent.as_ref().ok().map(|r| r.status()),
        );
        let resp = sent?;
        let status = resp.status();
        let retry_after = parse_retry_after(resp.headers());
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(StreamError::Status {
                status,
                body: truncate(&text),
                retry_after,
            });
        }
        let raw: RawSubscribed = serde_json::from_str(&text)
            .map_err(|e| StreamError::Decode(format!("{e}: {}", truncate(&text))))?;
        let snapshot = match raw.snapshot.and_then(|s| s.data) {
            Some(data) => parse_deltas(data.get().as_bytes())
                .map_err(|e| StreamError::Decode(format!("{e}: {}", truncate(&text))))?,
            None => Vec::new(),
        };
        Ok(Subscribed {
            snapshot,
            inactivity_timeout_secs: raw.inactivity_timeout,
        })
    }

    /// Deletes one subscription, or every subscription of `context_id` when
    /// `reference_id` is `None`.
    pub async fn unsubscribe(
        &self,
        access_token: &str,
        context_id: &str,
        reference_id: Option<&str>,
    ) -> Result<(), StreamError> {
        let url = self.subscription_url(context_id, reference_id)?;
        let started = Instant::now();
        let sent = self.http.delete(url).bearer_auth(access_token).send().await;
        api_metrics::record(
            Endpoint::InfoPriceSubscriptions,
            started,
            sent.as_ref().ok().map(|r| r.status()),
        );
        Self::expect_success(sent?).await
    }

    /// Tells the live connection of `context_id` to use `access_token` from
    /// now on, without reconnecting.
    pub async fn reauthorize(
        &self,
        access_token: &str,
        context_id: &str,
    ) -> Result<(), StreamError> {
        let url = self.authorize_url(context_id)?;
        let started = Instant::now();
        let sent = self.http.put(url).bearer_auth(access_token).send().await;
        api_metrics::record(
            Endpoint::StreamingAuthorize,
            started,
            sent.as_ref().ok().map(|r| r.status()),
        );
        Self::expect_success(sent?).await
    }

    async fn expect_success(resp: reqwest::Response) -> Result<(), StreamError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let retry_after = parse_retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        Err(StreamError::Status {
            status,
            body: truncate(&body),
            retry_after,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(q: &StreamQuote) -> SaxoQuote {
        q.to_quote()
    }

    fn one(json: &str) -> InfoPriceDelta {
        let mut v = parse_deltas(json.as_bytes()).unwrap();
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    const SNAPSHOT: &str = r#"{"Uic":4711,"AssetType":"Stock",
        "LastUpdated":"2026-11-10T09:00:01.123Z",
        "DisplayAndFormat":{"Currency":"SEK","Decimals":2},
        "Quote":{"Bid":67.3,"Ask":67.4,"BidSize":1200,"AskSize":800,
                 "PriceTypeBid":"Tradable","PriceTypeAsk":"Tradable",
                 "MarketState":"Open","DelayedByMinutes":0}}"#;

    #[test]
    fn a_delta_changes_only_the_fields_it_carries() {
        let mut q = StreamQuote::from_snapshot(one(SNAPSHOT));
        q.apply(one(
            r#"{"Uic":4711,"Quote":{"Ask":67.5},"LastUpdated":"2026-11-10T09:00:02Z"}"#,
        ));
        let s = quote(&q);
        assert_eq!(s.ask.unwrap().to_string(), "67.50");
        assert_eq!(s.bid.unwrap().to_string(), "67.30");
        assert_eq!(s.bid_size.unwrap().to_string(), "1200");
        assert_eq!(s.ask_type.as_deref(), Some("Tradable"));
        assert_eq!(s.last_updated.as_deref(), Some("2026-11-10T09:00:02Z"));

        q.apply(one(r#"{"Uic":4711,"Quote":{"PriceTypeAsk":"Indicative"}}"#));
        let s = quote(&q);
        assert_eq!(s.ask_type.as_deref(), Some("Indicative"));
        assert_eq!(s.bid_type.as_deref(), Some("Tradable"));
        assert_eq!(s.ask.unwrap().to_string(), "67.50");
        assert_eq!(s.bid.unwrap().to_string(), "67.30");
        assert_eq!(s.currency.as_deref(), Some("SEK"));
        assert_eq!(s.market_state.as_deref(), Some("Open"));
    }

    #[test]
    fn an_explicit_null_clears_and_an_absent_field_keeps() {
        let mut q = StreamQuote::from_snapshot(one(SNAPSHOT));
        q.apply(one(r#"{"Quote":{"Bid":null}}"#));
        let s = quote(&q);
        assert_eq!(s.bid, None);
        assert_eq!(s.ask.unwrap().to_string(), "67.40");
        q.apply(one(r#"{"Quote":null}"#));
        assert_eq!(quote(&q).ask.unwrap().to_string(), "67.40");
    }

    #[test]
    fn decimals_pad_without_f64() {
        let q = StreamQuote::from_snapshot(one(
            r#"{"DisplayAndFormat":{"Decimals":2},"Quote":{"Ask":101.5,"Bid":0.30000000000000004}}"#,
        ));
        let s = quote(&q);
        assert_eq!(s.ask.unwrap().to_string(), "101.50");
        assert_eq!(s.bid.unwrap().to_string(), "0.30000000000000004");
    }

    #[test]
    fn a_later_decimals_change_pads_the_stored_prices() {
        let mut q = StreamQuote::from_snapshot(one(r#"{"Quote":{"Ask":101.5}}"#));
        assert_eq!(quote(&q).ask.unwrap().to_string(), "101.5");
        q.apply(one(r#"{"DisplayAndFormat":{"Decimals":3}}"#));
        assert_eq!(quote(&q).ask.unwrap().to_string(), "101.500");
    }

    #[test]
    fn a_list_payload_is_split_per_instrument() {
        let v =
            parse_deltas(br#"[{"Uic":1,"Quote":{"Ask":1}},{"Uic":2,"Quote":{"Bid":2}}]"#).unwrap();
        assert_eq!(
            v.iter().map(|d| d.uic).collect::<Vec<_>>(),
            vec![Some(1), Some(2)]
        );
    }

    #[test]
    fn frames_round_trip_and_hold_several_messages() {
        let mut frame = encode_message(7, "q1", br#"[{"Uic":1}]"#);
        frame.extend(encode_message(
            8,
            HEARTBEAT_REF,
            br#"[{"ReferenceId":"_heartbeat"}]"#,
        ));
        let msgs = decode_frame(&frame).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].message_id, 7);
        assert_eq!(msgs[0].reference_id, "q1");
        assert_eq!(msgs[0].format, 0);
        assert_eq!(msgs[0].payload, br#"[{"Uic":1}]"#);
        assert_eq!(msgs[1].reference_id, HEARTBEAT_REF);
    }

    #[test]
    fn a_truncated_frame_is_an_error() {
        let frame = encode_message(7, "q1", br#"[{"Uic":1}]"#);
        assert!(decode_frame(&frame[..frame.len() - 1]).is_err());
        assert!(decode_frame(&frame[..5]).is_err());
        assert_eq!(decode_frame(&[]).unwrap(), vec![]);
    }

    #[test]
    fn control_messages_are_parsed() {
        let hb = br#"[{"ReferenceId":"_heartbeat","Heartbeats":[{"OriginatingReferenceId":"q1","Reason":"NoNewData"}]}]"#;
        assert_eq!(
            parse_control(HEARTBEAT_REF, hb),
            Some(Control::Heartbeat(vec![Heartbeat {
                originating_reference_id: "q1".into(),
                reason: "NoNewData".into()
            }]))
        );
        assert_eq!(
            parse_control(DISCONNECT_REF, br#"[{"ReferenceId":"_disconnect"}]"#),
            Some(Control::Disconnect)
        );
        assert_eq!(
            parse_control(
                RESET_SUBSCRIPTIONS_REF,
                br#"[{"ReferenceId":"_resetsubscriptions","TargetReferenceIds":["q1"]}]"#
            ),
            Some(Control::ResetSubscriptions(vec!["q1".into()]))
        );
        assert_eq!(
            parse_control(
                RESET_SUBSCRIPTIONS_REF,
                br#"{"ReferenceId":"_resetsubscriptions","TargetReferenceIds":[]}"#
            ),
            Some(Control::ResetSubscriptions(vec![]))
        );
        assert_eq!(parse_control("q1", b"[]"), None);
    }

    #[test]
    fn context_ids_fit_saxo_rules() {
        let id = new_context_id();
        assert!(id.len() <= MAX_ID_LEN);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert_ne!(id, new_context_id());
    }

    #[test]
    fn the_client_only_builds_subscription_and_authorize_urls() {
        for base in [
            "https://gateway.saxobank.com/openapi",
            "https://gateway.saxobank.com/openapi/",
        ] {
            let c = InfoPriceStreamClient::new(
                reqwest::Client::new(),
                base,
                LIVE_WS_URL,
                LIVE_AUTHORIZE_URL,
            );
            assert_eq!(
                c.subscriptions_url().unwrap().as_str(),
                "https://gateway.saxobank.com/openapi/trade/v1/infoprices/subscriptions"
            );
            assert_eq!(
                c.subscription_url("ctx", Some("q1")).unwrap().as_str(),
                "https://gateway.saxobank.com/openapi/trade/v1/infoprices/subscriptions/ctx/q1"
            );
            assert_eq!(
                c.subscription_url("ctx", None).unwrap().as_str(),
                "https://gateway.saxobank.com/openapi/trade/v1/infoprices/subscriptions/ctx"
            );
            assert_eq!(
                c.authorize_url("ctx").unwrap().as_str(),
                "https://live-streaming.saxobank.com/oapi/streaming/ws/authorize?contextid=ctx"
            );
            assert_eq!(
                c.connect_url("ctx").unwrap().as_str(),
                "wss://live-streaming.saxobank.com/oapi/streaming/ws/connect?contextId=ctx"
            );
        }
    }

    #[test]
    fn the_module_names_no_trading_or_account_endpoint() {
        let source = include_str!("stream.rs");
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("split yields at least one part");
        let paths: Vec<&str> = code
            .match_indices("\"/")
            .map(|(i, _)| &code[i + 1..])
            .map(|rest| rest.split('"').next().unwrap_or_default())
            .collect();
        assert_eq!(paths, vec![SUBSCRIPTIONS_PATH]);
        for word in [
            ["/ord", "ers"].concat(),
            ["/posi", "tions"].concat(),
            ["/acc", "ounts"].concat(),
            ["/p", "ort/"].concat(),
            ["/trade/v1/pr", "ices"].concat(),
        ] {
            assert!(!code.contains(&word), "{word}");
        }
    }
}
