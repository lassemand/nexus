# ADR-0001: Saxo Bank OpenAPI as Nordic/First North Real-Time Data Source

**Status:** Draft — pending SIM account validation  
**Date:** 2026-07-11  
**Linear:** NEX-77  

---

## Context

The nexus pipeline needs real-time or low-latency trade/bar data for instruments listed on
Nasdaq First North Growth Market Stockholm (MIC: FNSE), starting with GomSpace (GOMX,
ISIN DK0060738599). This market is not covered by Polygon (US-only) or the existing
`PolygonBarProvider`. After evaluating all realistic free and low-cost alternatives, Saxo
Bank's OpenAPI is the strongest candidate.

### Alternatives considered and ruled out

| Option | Verdict | Reason |
|---|---|---|
| Nasdaq Nordic direct | ❌ | €200–600+/month for a single non-display licence |
| Nasdaq MiFID delayed feed (tradereports.nasdaq.com) | ⚠️ | 15-min delay; First North inclusion unconfirmed; schema unpublished |
| Nordnet External API | ❌ (blocked) | Not onboarding new API customers as of 2026-07-11 |
| Yahoo Finance (`yahoo_finance_api`) | ⚠️ | EOD only reliable; unofficial API, no SLA |
| Avanza unofficial API | ❌ | No official API; scraping fragile, TOTP 2FA required |
| Saxo Bank OpenAPI | ✅ Selected | Official OAuth2 API; €7–10/month equity data; free SIM env |

---

## Acceptance Criteria Status

### 1. OAuth2 app registration + token lifecycle

**Confirmed from docs:**

- **SIM environment:** Free. Register at `https://www.developer.saxo/openapi/appmanagement`.
  A 24-hour test token is available directly from the developer portal for immediate testing
  without implementing OAuth.
- **Live environment:** Requires a Saxo brokerage account + OAuth2 app approval.
- **Auth flow:** Authorization Code (for user-facing apps) or PKCE.

**Token TTL — partially confirmed:**

- Access token has a finite lifetime (exact value not published in docs; typically 20 minutes
  based on community reports — **must be confirmed via SIM**).
- Refresh token rotation: every refresh issues a new refresh token and invalidates the
  previous one. Storing the latest refresh token is mandatory.

**WebSocket token extend — confirmed mechanism exists:**

The streaming docs confirm a dedicated endpoint to refresh the token on an existing WebSocket
connection without reconnecting:

```
PUT https://sim-streaming.saxobank.com/sim/oapi/streaming/ws/authorize?contextid=<ContextId>
Authorization: Bearer <new_access_token>
```

This avoids dropping and re-subscribing the WebSocket when the access token expires. The
exact call sequence is:
1. Refresh the access token via the standard OAuth token endpoint.
2. Immediately `PUT` the new token to the WebSocket authorize endpoint.
3. The existing connection and all subscriptions remain live.

**⚠️ Requires SIM verification:** Exact access token TTL and refresh token expiry window.

---

### 2. Market data enablement + actual cost

**Confirmed:**

Market data for non-Forex instruments is **disabled by default** on live accounts. To enable:
1. Log in to SaxoTrader GO → My Profile → Other → Open API Access → Enable → Accept terms.
2. Then subscribe to individual exchanges via the in-platform Subscription Tool.

**Pricing (confirmed from public price list):**

| Exchange | Level 1 (private) | Level 2 (private) | Level 1 (professional) | Level 2 (professional) |
|---|---|---|---|---|
| Nasdaq OMX Copenhagen/Stockholm/Helsinki | **€7/month** | **€10/month** | €46/month | €86/month |

**Refund scheme:** Non-professional clients receive a full fee refund if they make ≥4 trades/month
on that exchange (stocks, ETFs, or CFDs). Active trading effectively makes data free.

**⚠️ Unconfirmed:** Whether "Nasdaq OMX Copenhagen/Stockholm/Helsinki" includes First North
Growth Market instruments or only main-market (OMXS30) names. This must be confirmed by
subscribing to the entitlement in the SIM environment and querying GOMX.

---

### 3. First North as separate entitlement vs. bundled

**Status: Unconfirmed — requires SIM testing.**

The published price list shows "Nasdaq OMX Copenhagen Stockholm Helsinki" as a single line
item. First North is classified as an MTF (Multilateral Trading Facility) rather than a
regulated market, which sometimes results in separate entitlement treatment.

**Test plan:** With a SIM account + exchange subscription enabled, query:
```
GET /ref/v1/instruments?Keywords=GOMX&AssetTypes=Stock
```
If GOMX returns with `PriceSourceName` indicating the First North tape (not "Saxo synthetic"),
it is covered. If the field returns `NoAccess`, a separate entitlement is needed.

---

### 4. `Stock` vs. `CfdOnStock` disambiguation

**Confirmed from docs:** Saxo serves both `Stock` (real exchange-listed share, priced from
the actual exchange tape) and `CfdOnStock` (Saxo's internally-priced derivative). These are
distinct tradable instruments with different `Uic` values.

**Disambiguation method — confirmed:**

`GET /ref/v1/instruments/details/?Uics=<uic>&AssetTypes=Stock` returns:
- `PriceSourceName`: identifies the originating exchange tape vs. Saxo internal pricing.
- `RelatedInstruments`: links the `Stock` to its corresponding `CfdOnStock` and vice versa.
- `ExchangeId`: for a genuine exchange-listed `Stock`, this will be the exchange MIC
  (e.g. `FSN` for First North Sweden, or similar Saxo internal code).

Ingesting the `CfdOnStock` instead of `Stock` would silently track Saxo's internal derivative
pricing rather than real Nasdaq First North market activity — this is a critical distinction.

**⚠️ Requires SIM verification:** Stable `Uic` for GOMX `Stock` (not `CfdOnStock`) on First
North. A sample `/ref/v1/instruments/details/` response for GOMX must be captured as a test
fixture.

---

### 5. APA/OTC-reported trade coverage

**Status: Unconfirmed — requires SIM or vendor confirmation.**

Under MiFID II, some trades in FNSE-listed instruments are crossed off the lit order book and
reported via an Approved Publication Arrangement (APA). Whether Saxo's `Stock` market data
stream for GOMX includes these APA prints or only the primary lit-book trades is not
documented in public Saxo docs.

**Test plan:** Monitor a live session's trade stream alongside the Nasdaq Nordic MiFID feed
(tradereports.nasdaq.com) and compare total reported volume. A consistent shortfall would
indicate APA trades are absent. Alternatively, contact `openapisupport@saxobank.com`.

---

### 6. WebSocket message framing

**Confirmed from docs:**

Binary WebSocket frames. Each frame may contain multiple messages; a single message may
span multiple frames (continuation frames, FIN bit indicates completion).

**Per-message layout:**

| Offset | Size | Content |
|---|---|---|
| 0 | 8 bytes | Message ID (64-bit little-endian unsigned int) |
| 8 | 2 bytes | Reserved |
| 10 | 1 byte | Reference ID size (N) |
| 11 | N bytes | Reference ID (ASCII) |
| 11+N | 1 byte | Payload format: 0=JSON/UTF-8, 1=Protobuf binary |
| 12+N | 4 bytes | Payload size (32-bit unsigned int) |
| 16+N | variable | Payload |

**Protobuf schema:** Not published. JSON is the default and recommended format for
implementation; Protobuf is an optimization path requiring schema negotiation with Saxo support.

**Heartbeat:** `_heartbeat` reference ID every N seconds when no data. Reasons:
`NoNewData`, `SubscriptionTemporarilyDisabled`, `SubscriptionPermanentlyDisabled`.

**Reconnect with replay:** Pass `?messageid=<last_received_id>` on reconnect to resume from
a known offset.

**Delta ticks:** Subscriptions emit full snapshot on first message; subsequent messages contain
only changed fields plus `m` (market) and `i` (instrument identifier).

---

### 7. Historical gap-fill via `GET /chart/v1/charts`

**Status: Partially confirmed.**

The endpoint exists in the Saxo reference documentation:
```
GET /chart/v1/charts?Uic=<uic>&AssetType=Stock&Horizon=<minutes>&Count=<n>
```

Availability requires the same exchange entitlement as streaming. Whether it is available in
the SIM environment or only on live accounts with a market data subscription is **unconfirmed**.

This endpoint is important as a gap-fill mechanism after stream disconnects.

---

### 8. Stable `Uic` for GOMX

**Status: Unconfirmed — requires SIM account.**

The `Uic` is Saxo's internal numeric instrument identifier. It must be confirmed as stable
(not rotating) for GOMX `Stock` on First North. The lookup is:

```
GET https://gateway.saxobank.com/sim/openapi/ref/v1/instruments
  ?Keywords=GOMX
  &AssetTypes=Stock
Authorization: Bearer <token>
```

A sample response should be captured and committed to `tests/fixtures/saxo_gomx_instrument.json`.

---

## Go/No-Go Recommendation

**Tentative GO** — pending SIM validation of items 2, 3, 4, 8.

The Saxo Bank OpenAPI is the strongest available option for real-time First North data:
- Official, stable, well-documented API with OAuth2
- €7/month for Level 1 (effectively free with the trading refund scheme)
- Free SIM environment enables development and testing before any cost commitment
- WebSocket streaming with per-instrument subscriptions fits the nexus pipeline model
- Token-on-wire refresh (`PUT /ws/authorize`) avoids reconnection drops

**Blockers before implementation can start:**

1. SIM account created and 24h token obtained from developer portal
2. GOMX `Uic` (Stock, not CfdOnStock) confirmed and fixture captured
3. First North coverage under "Nasdaq OMX Stockholm" entitlement confirmed
4. Access token TTL confirmed (to size the refresh loop correctly)

---

## Next Steps

1. Create a Saxo SIM developer account at `https://www.developer.saxo/openapi/appmanagement`
2. Get a 24h token from the developer portal
3. Run `GET /ref/v1/instruments?Keywords=GOMX&AssetTypes=Stock` — capture response as fixture
4. Check `PriceSourceName` and `ExchangeId` on the returned instrument to confirm Stock vs. CFD
5. Enable market data on a live account (€7/month) and confirm First North coverage
6. Update this ADR with confirmed values and flip status to **Accepted**

---

## References

- Saxo Developer Portal: https://www.developer.saxo/openapi/learn
- Saxo Streaming Docs: https://www.developer.saxo/openapi/learn/streaming
- Market Data Subscriptions: https://www.home.saxo/products/market-data-subscriptions
- OpenAPI Support: https://openapi.help.saxo.com
- SIM App Registration: https://www.developer.saxo/openapi/appmanagement
