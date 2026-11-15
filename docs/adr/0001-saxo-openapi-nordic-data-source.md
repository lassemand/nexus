# ADR-0001: Saxo Bank OpenAPI as Nordic/First North Real-Time Data Source

> **Update 2026-10-27 (NEX-193):** the live WebSocket price stream described below was removed from
> `saxo-stream`. Nothing consumed its 1-minute bars; Nordic daily prices come from Saxo's chart API
> (`signal.daily_bars`, NEX-162). The OAuth, Uic and chart parts of this ADR still apply.
>
> **Update 2026-11-15 (NEX-270):** `saxo-stream` streams Saxo **info prices** again, for the
> instruments its `/quotes/stream` clients watch. See [Streaming info prices](#streaming-info-prices-nex-270)
> below.

**Status:** Accepted  
**Date:** 2026-07-12  
**Linear:** NEX-77  

---

## Context

The nexus pipeline needs real-time or low-latency trade/bar data for instruments listed on
Nasdaq First North Growth Market Stockholm (MIC: FNSE), starting with GomSpace (GOMX,
ISIN DK0060738599). This market is not covered by Polygon (US-only) or the existing
`PolygonBarProvider`. After evaluating all realistic free and low-cost alternatives, Saxo
Bank's OpenAPI is confirmed as the data source.

### Alternatives considered and ruled out

| Option | Verdict | Reason |
|---|---|---|
| Nasdaq Nordic direct | ❌ | €200–600+/month for a single non-display licence |
| Nasdaq MiFID delayed feed (tradereports.nasdaq.com) | ⚠️ | 15-min delay; First North inclusion unconfirmed; schema unpublished |
| Nordnet External API | ❌ (blocked) | Not onboarding new API customers as of 2026-07-11 |
| Yahoo Finance (`yahoo_finance_api`) | ⚠️ | EOD only reliable; unofficial API, no SLA |
| Avanza unofficial API | ❌ | No official API; scraping fragile, TOTP 2FA required |
| Saxo Bank OpenAPI | ✅ **Selected** | Official OAuth2 API; €7–10/month equity data; free SIM env |

---

## Confirmed Findings (from SIM API testing, 2026-07-12)

### 1. OAuth2 app registration + token lifecycle

- **SIM environment:** Free. Register at `https://www.developer.saxo/openapi/appmanagement`.
  A 24-hour test token is available directly from the developer portal.
- **Live environment:** Requires a Saxo brokerage account + OAuth2 app approval.
- **Access token TTL:** ~20 hours confirmed from JWT `exp` claim on the 24h SIM token
  (`isa=False` → non-professional client).
- **Refresh token rotation:** every refresh issues a new refresh token and invalidates the
  previous one. Latest refresh token must always be persisted.
- **WebSocket token extend — confirmed mechanism:**

```
PUT https://sim-streaming.saxobank.com/sim/oapi/streaming/ws/authorize?contextid=<ContextId>
Authorization: Bearer <new_access_token>
```

This refreshes the token on an existing WebSocket without dropping subscriptions.

---

### 2. Market data enablement + actual cost

**To enable market data on a live account:**
1. Log in to SaxoTrader GO → My Profile → Other → Open API Access → Enable → Accept terms.
2. Subscribe to exchange entitlements via the in-platform Subscription Tool.

**Confirmed from SIM testing:** When `MarketDataViaOpenApiTermsAccepted=false`, price
subscriptions return `PriceTypeAsk: "NoAccess"` and `PriceTypeBid: "NoAccess"`. The
subscription itself succeeds (HTTP 201) but prices are gated.

**Pricing (confirmed from public price list):**

| Entitlement | Level 1 (private) | Level 2 (private) | Level 1 (professional) |
|---|---|---|---|
| Nasdaq OMX Copenhagen/Stockholm/Helsinki | **€7/month** | **€10/month** | €46/month |

**Refund:** Full fee refund if ≥4 trades/month on that exchange (non-professional clients).

---

### 3. First North as separate entitlement — **CONFIRMED SEPARATE**

From the exchange listing query, First North Sweden is a **distinct exchange ID**:

| ExchangeId | Name | PriceSourceName |
|---|---|---|
| `SSE` | NASDAQ OMX Stockholm | Nasdaq |
| `SSE_FN-SE` | NASDAQ OMX Stockholm (First North) | Nasdaq |
| `CSE_FN-DK` | NASDAQ OMX Copenhagen (First North) | Nasdaq |
| `HSE_FN` | NASDAQ OMX Helsinki (First North) | Nasdaq |

**Implication:** The €7/month "Nasdaq OMX Stockholm" entitlement may only cover the main
market (`SSE`). First North (`SSE_FN-SE`) may require a separate or additional entitlement.
This must be confirmed by subscribing and checking whether prices become accessible for
`SSE_FN-SE` instruments. Contact `openapisupport@saxobank.com` to confirm before paying.

---

### 4. `Stock` vs `CfdOnStock` disambiguation — **CONFIRMED CLEAN**

From SIM API testing:
- `GET /ref/v1/instruments?Keywords=GOMX&AssetTypes=Stock` → returns 1 result: `Uic=4769462`
- `GET /ref/v1/instruments?Keywords=GOMX&AssetTypes=CfdOnStock` → returns **empty** — no CFD listed for GOMX

The instrument details confirm:
```json
"Exchange": {
  "ExchangeId": "SSE_FN-SE",
  "Name": "NASDAQ OMX Stockholm (First North)",
  "OperatingMic": "XSTO"
},
"PriceSource": "SSE_FN-SE"
```

**GOMX on Saxo is only available as a real exchange-listed `Stock` — no CFD version exists.**
There is no ambiguity; ingesting `Uic=4769462` with `AssetType=Stock` is the genuine First
North share priced from the Nasdaq tape.

---

### 5. APA/OTC-reported trade coverage

**Status: Partially confirmed via exchange metadata.**

The exchange entry for `SSE_FN-SE` shows:
- `PriceSourceName: "Nasdaq"` — prices come from the Nasdaq tape directly, not Saxo internal
- `IsoMic: "SSME"` — First North's MiFID Systematic Internaliser MIC
- `OperatingMic: "XSTO"` — operated by Nasdaq Stockholm

APA trade coverage is not documented in public Saxo docs. The `PriceSourceType: "Firm"` in
the subscription snapshot indicates exchange-sourced quotes. Full APA coverage would require
monitoring a live session and comparing total volume against the Nasdaq Nordic MiFID feed.
**Contact `openapisupport@saxobank.com` to confirm before implementation.**

---

### 6. WebSocket message framing — **CONFIRMED**

Binary frames, each containing one or more messages. Message layout:

| Offset | Size | Content |
|---|---|---|
| 0 | 8 bytes | Message ID (64-bit little-endian unsigned int) |
| 8 | 2 bytes | Reserved |
| 10 | 1 byte | Reference ID size (N) |
| 11 | N bytes | Reference ID (ASCII) |
| 11+N | 1 byte | Payload format: `0`=JSON, `1`=Protobuf |
| 12+N | 4 bytes | Payload size (32-bit unsigned int) |
| 16+N | variable | Payload |

Protobuf schema not published — use JSON (`format=0`). Delta ticks after first full snapshot.

Heartbeat ReferenceIds: `_heartbeat`, `_disconnect`, `_resetsubscriptions`.

Reconnect: `GET .../ws/connect?contextId=<id>&messageid=<last_id>`

---

### 7. Historical `GET /chart/v1/charts` endpoint

**Status: Returns 404 in SIM environment.** This endpoint is not available without a live
account with an active market data subscription. It is present in the live API reference but
gated behind the exchange entitlement. Gap-fill strategy after stream disconnects must rely
on the `messageid` reconnect mechanism (Saxo replays from the last received message).

---

### 8. Stable `Uic` for GOMX — **CONFIRMED**

```
Uic:        4769462
AssetType:  Stock
ExchangeId: SSE_FN-SE
Symbol:     GOMX:xome
Currency:   SEK
PriceSource: Nasdaq (SSE_FN-SE tape)
```

Fixture committed to: `tests/fixtures/saxo/gomx_instrument.json`

---

## Go/No-Go Recommendation

**GO** — with one procurement step before implementation begins.

**Action required:** Contact `openapisupport@saxobank.com` to confirm whether First North
(`SSE_FN-SE`) is included in the €7/month "Nasdaq OMX Stockholm" entitlement or requires a
separate subscription. This is the only remaining financial unknown before committing to
implementation.

**Everything else is confirmed:**
- GOMX is a clean `Stock` (no CFD ambiguity), `Uic=4769462`
- First North is a distinct, well-formed exchange in the Saxo reference data
- Prices are Nasdaq-sourced, not Saxo internal
- WebSocket framing and token lifecycle are well-documented and tested
- Free SIM environment works for development; production needs a live Saxo account

---

## Implementation Checklist for Next Task

1. Accept market data terms in SaxoTrader GO (live account)
2. Subscribe to `SSE_FN-SE` exchange entitlement (confirm cost with support first)
3. Register OAuth2 app at `https://www.developer.saxo/openapi/appmanagement`
4. Implement `SaxoBarProvider` in `alpha/` using `Uic=4769462`, `AssetType=Stock`
5. Implement token refresh loop with `PUT /ws/authorize` to keep connection alive
6. Subscribe to `trade` and `price` message types on `SSE_FN-SE`
7. Handle `_resetsubscriptions` control message by recreating subscriptions
8. Wire `SaxoBarProvider` into `chronicle/market` for FNSE-listed instruments

---

## References

- Saxo Developer Portal: https://www.developer.saxo/openapi/learn
- SIM Environment URLs: https://www.developer.saxo/openapi/learn/environments
- Streaming Docs: https://www.developer.saxo/openapi/learn/streaming
- Market Data Subscriptions: https://www.home.saxo/products/market-data-subscriptions
- OpenAPI Support: https://openapi.help.saxo.com
- GOMX fixture: `tests/fixtures/saxo/gomx_instrument.json`

---

## Streaming info prices (NEX-270)

`saxo-stream` subscribes to `POST /trade/v1/infoprices/subscriptions` (not `/trade/v1/prices`) with
`AssetType=Stock`, the comma-separated `Uics` of the watched instruments, the same `FieldGroups` as
`GET /quote` (`Quote`, `DisplayAndFormat`, `InstrumentPriceDetails`) and `RefreshRate=0`, and pushes
the merged quotes to its clients over `GET /quotes/stream` (`chronicle_lib::saxo_quote_stream`).

### What the implementation relies on

From Saxo's developer documentation (learn pages "Plain WebSocket streaming", "Environments",
"Rate limiting", the info-price reference and Saxo support article 4417467162641), checked
2026-11-15:

| Topic | Documented | How saxo-stream handles it |
|---|---|---|
| Connect | `GET <streaming>/connect?contextId=<id>`, token in the `Authorization` header; context id ≤ 50 chars of `a-zA-Z0-9-` | `SAXO_STREAMING_WS_URL` is the full connect URL; a fresh context id `nexus-q-<16 hex>` per connection |
| Re-authorize | `PUT <streaming>/authorize?contextid=<id>` with the new token, 202 | `SAXO_STREAMING_AUTHORIZE_URL`; sent whenever saxo-stream's refresh task rotates the access token |
| URLs | Live `live-streaming.saxobank.com/oapi/streaming/ws`, SIM `sim-streaming.saxobank.com/sim/oapi/streaming/ws` (changed by Saxo in 2025; `/connect` and `/authorize` paths unchanged) | defaults are live: `wss://live-streaming.saxobank.com/oapi/streaming/ws/connect`, `https://live-streaming.saxobank.com/oapi/streaming/ws/authorize`. SIM: `wss://sim-streaming.saxobank.com/sim/oapi/streaming/ws/connect`, `https://sim-streaming.saxobank.com/sim/oapi/streaming/ws/authorize` |
| Deltas | only the properties whose values changed; apply in order onto the snapshot | merged per Uic (`alpha::saxo::StreamQuote::apply`); absent field keeps its value, explicit `null` clears |
| Snapshot | in the subscription answer (`Snapshot.Data`, one entry per Uic) | replaces the stored quote; deltas that arrive before it are buffered and applied after |
| Control messages | `_heartbeat` (`Heartbeats[].Reason`: `NoNewData`, `SubscriptionTemporarilyDisabled`, `SubscriptionPermanentlyDisabled`), `_disconnect`, `_resetsubscriptions` (`TargetReferenceIds`, empty = all) | heartbeats prove liveness, a permanently disabled subscription is re-created; `_disconnect` reconnects; reset re-creates the targets under new reference ids with `ReplaceReferenceId` |
| Liveness | "keep track of when you last received a message or heartbeat" | nothing for `SAXO_STREAM_HEARTBEAT_TIMEOUT` (15 s) → reconnect |
| Limits | price subscriptions: 200 instruments per session; 4 streaming connections per session (different context ids); 120 requests/minute per session and service group | one connection; Uics batched up to `SAXO_STREAM_MAX_UICS_PER_SUBSCRIPTION` (100) per subscription; watch-list changes debounced 200 ms |

### Not yet verified on SIM

The NEX-270 session could not log in to SIM: a SIM token needs a person to sign in to the developer
portal, and saxo-stream's live token may not be used by anything else, because refresh tokens
rotate on use. These are still **unverified**:

- that `/trade/v1/infoprices/subscriptions` with a `Uics` list answers 201 with a `Snapshot.Data`
  array and then streams JSON arrays of per-Uic deltas carrying `Uic`;
- the largest number of Uics Saxo accepts in one info-price subscription;
- the subscription rate limits (create/delete per minute) and whether info prices count towards
  the 200-instrument limit;
- whether `RefreshRate=0` is accepted or raised to a minimum for info prices.

If Saxo refuses a batch with a 4xx, saxo-stream retries each Uic in its own subscription, so a lower
per-subscription limit degrades rather than breaks. To verify on SIM: run `saxo_stream` locally with
`SAXO_API_BASE=https://gateway.saxobank.com/sim/openapi`, the two SIM streaming URLs above and a
24-hour SIM token registered with `POST /tokens`; connect a WebSocket client to
`ws://localhost:8080/quotes/stream`, send `{"watch":[...]}` with a few registered SIM instruments, and
check the logs (`subscribing Saxo info prices`, `Saxo subscription active`) and the quotes. Raise the
batch size until Saxo refuses it to find the limit. Record the results here.

