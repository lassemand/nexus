//! Pins every Nordic fact the workspace answers from `alpha` and `model`, so
//! the move into `model::nordic` (NEX-211) is provably a no-op.
//!
//! Snapshots live in `tests/snapshots/`. Regenerate deliberately with
//! `UPDATE_SNAPSHOTS=1 cargo test -p alpha --test nordic_snapshot`.

use alpha::calendar::{country_for_exchange, CalendarProvider};
use model::asset::mic;
use model::exchange;
use std::fmt::Write as _;
use std::path::PathBuf;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn check_snapshot(name: &str, actual: &str) {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&file, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&file).unwrap();
    assert_eq!(expected, actual, "snapshot {name} changed");
}

/// Every MIC the issue names (the Nordic list, both US venues, one unknown).
fn mics() -> Vec<&'static str> {
    vec![
        "FNSE",
        "XSTO",
        "XCSE",
        "FNDK",
        "XHEL",
        "FNFI",
        mic::XNAS,
        mic::XNYS,
        "XOSL",
    ]
}

#[test]
fn mic_facts() {
    let mut out = String::new();
    writeln!(out, "NORDIC = {:?}", mic::NORDIC).unwrap();
    for m in mics() {
        writeln!(
            out,
            "{m}: country={:?} currency={:?} timezone={:?} calendar_country={:?} saxo={:?}",
            mic::country(m),
            mic::currency(m),
            exchange::timezone(m),
            country_for_exchange(m),
            alpha::saxo::exchange_ids::default_for(m),
        )
        .unwrap();
    }
    for code in [
        "NMS", "NGM", "NCM", "NYQ", "ASE", "PCX", "BTS", "STO", "CPH", "HEL", "OSL", "GER", "FRA",
        "LSE", "TOR", "VAN",
    ] {
        writeln!(
            out,
            "yahoo {code} -> {:?}",
            mic::mic_for_yahoo_exchange(code)
        )
        .unwrap();
    }
    writeln!(
        out,
        "yahoo UNKNOWN -> {:?}",
        mic::mic_for_yahoo_exchange("UNKNOWN")
    )
    .unwrap();
    check_snapshot("mic_facts.txt", &out);
}

/// The SE, DK and FI holiday lists 2015-2027 (closures and half days), built
/// from saved Nager.Date responses served by a mock server.
#[tokio::test]
async fn holiday_lists_2015_to_2027() {
    let server = MockServer::start().await;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nager");
    for c in ["SE", "DK", "FI"] {
        for y in 2015..=2027 {
            let body = std::fs::read_to_string(dir.join(format!("{c}_{y}.json"))).unwrap();
            Mock::given(method("GET"))
                .and(path(format!("/api/v3/publicholidays/{y}/{c}")))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
                .mount(&server)
                .await;
        }
    }
    let provider = CalendarProvider::with_base_url(server.uri());
    let mut out = String::new();
    for c in ["SE", "DK", "FI"] {
        for y in 2015..=2027 {
            for e in provider.holidays_for_country(c, y).await.unwrap() {
                writeln!(out, "{c} {} {} {}", e.date, e.status, e.note).unwrap();
            }
        }
    }
    check_snapshot("holidays.txt", &out);
}
