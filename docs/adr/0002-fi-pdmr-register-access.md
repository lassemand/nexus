# ADR-0002: Finansinspektionen (FI) PDMR Register — Access Protocol

**Status:** Accepted  
**Date:** 2026-07-12  
**Linear:** NEX-78  

---

## Context

FI (Sweden's financial regulator) publishes MAR Article 19 PDMR (Persons Discharging
Managerial Responsibilities) insider-transaction disclosures as public open data. This ADR
documents the confirmed access protocol, field schema, data depth, and protocol recommendation.

---

## Protocol: CSV HTTP export — **no SOAP needed**

The community implementations (`w3stling/insynsregistret` Java, `djonsson/insynsregistret`
Python) both wrap an **HTTP GET endpoint** that returns a **semicolon-delimited CSV file
encoded in UTF-16 LE**. There is no SOAP/WSDL involved — the Java library uses
`java.net.http.HttpClient`, not a SOAP stack.

### Endpoint

```
GET https://marknadssok.fi.se/Publiceringsklient/{language}/Search/Search
  ?SearchFunctionType=Insyn
  &Utgivare={issuer_name_url_encoded}
  &PersonILedandeStällningNamn={pdmr_name_url_encoded}
  &Transaktionsdatum.From={YYYY-MM-DD}
  &Transaktionsdatum.To={YYYY-MM-DD}
  &Publiceringsdatum.From={YYYY-MM-DD}
  &Publiceringsdatum.To={YYYY-MM-DD}
  &button=export
```

**Language values:** `sv-SE` (Swedish) or `en-GB` (English)

**Authentication:** None — fully public, no credentials required.

**Response encoding:** UTF-16 LE (no BOM in practice — decode as `utf-16-le`).

**Response format:** Semicolon-delimited CSV with a header row.

### Issuer autocomplete (for resolving exact issuer names)

```
GET https://marknadssok.fi.se/Publiceringsklient/sv-SE/AutoComplete/HämtaAutoCompleteListaFull
  ?sokfunktion=Insyn
  &falt=Utgivare
  &sokterm={partial_name}
→ JSON array of matching issuer name strings
```

Example: `?sokterm=GomSpace` → `["GomSpace AB", "GomSpace Group AB"]`

---

## Field Schema (confirmed from live data)

All field names are in Swedish when using `sv-SE`. The English equivalents are in parentheses.

| Field (Swedish) | Type | Notes |
|---|---|---|
| `Publiceringsdatum` | datetime | Publication date/time (`YYYY-MM-DD HH:MM:SS`) |
| `Emittent` | string | Issuer name (uppercase) |
| `LEI-kod` | string | Legal Entity Identifier (20-char ISO 17442) |
| `Anmälningsskyldig` | string | Reporting person (the PDMR or their close associate) |
| `Person i ledande ställning` | string | Person in managerial position |
| `Befattning` | string | Role/position (CEO, board member, etc.) |
| `Närstående` | string | Close associate flag (`Ja` = yes, empty = no) |
| `Korrigering` | string | Amendment/correction flag |
| `Beskrivning av korrigering` | string | Description of correction |
| `Är förstagångsrapportering` | string | First-time reporting (`Ja`/empty) |
| `Är kopplad till aktieprogram` | string | Share programme related (`Ja`/empty) |
| `Karaktär` | string | Transaction character (see types below) |
| `Instrumenttyp` | string | Instrument type (see types below) |
| `Instrumentnamn` | string | Instrument name |
| `ISIN` | string | ISIN code |
| `Transaktionsdatum` | datetime | Transaction date (`YYYY-MM-DD HH:MM:SS`) |
| `Volym` | decimal | Volume (Swedish locale: comma as decimal separator) |
| `Volymsenhet` | string | Volume unit (`Antal` = count, `Nominellt belopp` = nominal) |
| `Pris` | decimal | Price per unit (Swedish locale: comma decimal) |
| `Valuta` | string | Currency (ISO 4217, e.g. `SEK`) |
| `Handelsplats` | string | Trading venue (e.g. `FIRST NORTH SWEDEN`) |
| `Status` | string | `Aktuell` (current) or cancelled/superseded |

### Transaction types (`Karaktär`) observed for GomSpace

- `Förvärv` — Acquisition (buy)
- `Avyttring` — Disposal (sell)
- `Tilldelning` — Allotment (e.g. options granted)
- `Teckning` — Subscription (rights issue)
- `Interntransaktion – Förvärv` — Internal transaction (acquisition)
- `Lösen minskning` — Exercise/redemption (decrease)
- `Utbyte minskning` — Exchange (decrease)

### Instrument types observed for GomSpace

- `Aktie` — Share (common stock)
- `Warrant` — Warrant
- `Teckningsrätt` — Subscription right
- `Teckningsrätt/Uniträtt` — Subscription/unit right
- `BTA (betald tecknad aktie)` — Paid subscribed share (interim security)

---

## Data confirmed for GomSpace Group AB

- **Total transactions:** 148 (as of 2026-07-12)
- **Date range:** 2018-11-22 to 2026-05-08
- **Corrections in dataset:** 21 rows with `Korrigering` set
- **Cancelled/superseded:** 14 rows where `Status != "Aktuell"`
- **LEI:** `213800ES6SHKK5QTN734`
- **ISIN:** `SE0008348304`
- **Trading venue:** `FIRST NORTH SWEDEN`

Fixture saved to: `tests/fixtures/fi/gomspace_pdmr_transactions.json`

---

## Data depth: MAR boundary

**Confirmed:** The FI register in its current form covers MAR Article 19 disclosures from
**3 July 2016 onward**. Querying with `Transaktionsdatum.From=2010-01-01` and
`Transaktionsdatum.To=2016-07-02` for GomSpace returns **zero rows** — the pre-MAR register
used a different regime and is not available through this endpoint.

GomSpace's earliest transaction in the register is **2018-11-22**, which is after MAR
implementation. There is no gap — GomSpace was not yet listed before MAR came into force
(it listed on First North in 2016).

For any issuer that was listed before 3 July 2016, the pipeline must treat that date as the
hard lower bound for data availability.

---

## Protocol Recommendation: CSV export

**Use the CSV HTTP export endpoint.** Do not implement a SOAP client.

Rationale:
- The SOAP hypothesis was incorrect — the community libraries use plain HTTP GET.
- The CSV endpoint is simple, fully public, no auth, no registration.
- A single `curl`-equivalent HTTP GET returns a complete, machine-readable dataset.
- The response is large (148 rows for GomSpace covers 8 years) but manageable.
- The encoding quirk (UTF-16 LE, semicolon delimiter, comma decimal separator) is the only
  implementation complexity.

---

## Implementation Notes for Ingestion Binary

1. **Issuer resolution:** Use the autocomplete endpoint to resolve a ticker/company name to
   the exact `Utgivare` string used by FI (e.g. `GOMX` → `GomSpace Group AB`).

2. **Encoding:** Decode response as `utf-16-le`. Do not attempt UTF-8.

3. **Decimal parsing:** Volumes and prices use Swedish locale (`,` as decimal separator).
   Replace `,` with `.` before parsing as `f64`.

4. **Filtering active records:** Filter on `Status == "Aktuell"` to exclude cancelled/amended
   rows. Corrections (`Korrigering` set) replace earlier rows — keep the latest.

5. **Date lower bound:** Use `2016-07-03` as the hard minimum `Transaktionsdatum.From`.

6. **Update frequency:** The register is updated continuously as reports are received. FI
   states no SLA — poll daily or on-demand. There is no push/streaming mechanism.

7. **Language:** Use `sv-SE` for consistency with column names above. English (`en-GB`)
   is also available but the field names differ slightly.

---

## References

- FI register search UI: https://marknadssok.fi.se/publiceringsklient
- FI register info page: https://www.fi.se/sv/vara-register/insynsregistret/
- Community Java library: https://github.com/w3stling/insynsregistret
- GomSpace fixture: `tests/fixtures/fi/gomspace_pdmr_transactions.json`
