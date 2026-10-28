//! Fixture tests for the Finnish MAR release parser (NEX-206). Fixtures are
//! real releases fetched from MFN; see `fixtures/mfn/finnish/mar/README.md`.
use chronicle_lib::nordic::mar_release::{parse, MarParseError};
use model::generated::UnifiedTransactionType as T;

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/mfn/finnish/mar/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn metso_ceo_box_layout_english() {
    let tx = parse(&fixture("metso_ceo_en.txt"), "en").unwrap();
    assert_eq!(tx.len(), 1);
    let t = &tx[0];
    assert_eq!(t.person_name, "Sami Takaluoma");
    assert_eq!(t.person_role, "Chief Executive Officer");
    assert!(!t.is_close_associate && !t.is_amendment);
    assert_eq!(
        t.reference_number,
        "549300R0VN9C371W0E07_20260320093338_264"
    );
    assert_eq!(t.id(), "FIN_MAR:549300R0VN9C371W0E07_20260320093338_264:0");
    assert_eq!(t.issuer_name, "Metso Corporation");
    assert_eq!(t.lei, "549300R0VN9C371W0E07");
    assert_eq!(t.transaction_date.to_string(), "2026-03-19");
    assert_eq!(t.venue.as_deref(), Some("NASDAQ HELSINKI"));
    assert_eq!(t.instrument_type, "Share");
    assert_eq!(t.nature, "Receipt of a Share-Based Incentive");
    assert_eq!(t.transaction_type, T::Grant);
    assert_eq!(t.volume.to_string(), "23201");
    assert!(t.unit_price.unwrap().is_zero());
    assert_eq!(t.currency, "EUR");
}

#[test]
fn metso_ceo_box_layout_finnish() {
    let tx = parse(&fixture("metso_ceo_fi.txt"), "fi").unwrap();
    assert_eq!(tx.len(), 1);
    let t = &tx[0];
    assert_eq!(t.person_name, "Sami Takaluoma");
    assert_eq!(t.person_role, "Toimitusjohtaja");
    assert_eq!(t.issuer_name, "Metso Oyj");
    assert_eq!(t.nature, "Osakepalkkion vastaanottaminen");
    assert_eq!(t.transaction_type, T::Grant);
    assert_eq!(t.instrument_type, "Osake");
    assert_eq!(t.volume.to_string(), "23201");
    assert!(t.unit_price.unwrap().is_zero());
    assert_eq!(t.currency, "EUR");
    assert_eq!(t.lang, "fi");
}

#[test]
fn metso_correction_finnish_close_associate_and_thousands_separator() {
    let tx = parse(&fixture("metso_korjaus_fi.txt"), "fi").unwrap();
    assert_eq!(tx.len(), 1);
    let t = &tx[0];
    // The trade was made by a foundation; it is attributed to the board member.
    assert_eq!(t.person_name, "Klaus Cawén");
    assert_eq!(t.person_role, "Hallituksen jäsen/varajäsen");
    assert!(t.is_close_associate);
    assert_eq!(t.transaction_type, T::Sell);
    assert_eq!(t.volume.to_string(), "13000");
    assert_eq!(t.unit_price.unwrap().to_string(), "10.11110");
    assert_eq!(t.currency, "EUR");
}

#[test]
fn close_associate_plain_english_with_two_blocks() {
    let tx = parse(&fixture("wartsila_close_associate_en.txt"), "en").unwrap();
    assert_eq!(tx.len(), 2);
    for (i, t) in tx.iter().enumerate() {
        assert_eq!(t.index, i);
        assert_eq!(t.person_name, "Anu Sirkiä");
        assert_eq!(t.person_role, "Other senior manager");
        assert!(t.is_close_associate);
        assert_eq!(t.transaction_type, T::Buy);
        assert_eq!(t.reference_number, "167108/9/12");
    }
    assert_eq!(tx[0].venue.as_deref(), Some("HREU"));
    assert_eq!(tx[1].venue.as_deref(), Some("CEUX"));
    assert_eq!(tx[0].unit_price.unwrap().to_string(), "30.44");
    assert_eq!(tx[1].id(), "FIN_MAR:167108/9/12:1");
}

#[test]
fn close_associate_finnish() {
    let tx = parse(&fixture("wartsila_close_associate_fi.txt"), "fi").unwrap();
    assert_eq!(tx.len(), 2);
    assert_eq!(tx[0].person_name, "Anu Sirkiä");
    assert_eq!(tx[0].person_role, "Muu ylin johto");
    assert!(tx[0].is_close_associate);
    assert_eq!(tx[0].transaction_type, T::Buy);
}

#[test]
fn multi_block_finnish_option_exercise_then_sale() {
    let tx = parse(&fixture("wartsila_multi_fi.txt"), "fi").unwrap();
    assert!(tx.len() >= 2, "{} transactions", tx.len());
    let types: Vec<_> = tx.iter().map(|t| t.transaction_type).collect();
    assert!(types.contains(&T::Sell) || types.contains(&T::Buy));
    let ids: std::collections::HashSet<_> = tx.iter().map(|t| t.id()).collect();
    assert_eq!(ids.len(), tx.len(), "ids must be unique within a release");
    assert!(tx.iter().all(|t| !t.volume.is_zero()));
}

#[test]
fn amendment_with_many_price_lines_and_no_original_reference() {
    let tx = parse(&fixture("kesko_amendment_en.txt"), "en").unwrap();
    // 8 blocks, 21 price lines; the 8 aggregate summaries add none.
    assert_eq!(tx.len(), 21);
    let t = &tx[0];
    assert!(t.is_amendment);
    assert!(t.is_close_associate);
    assert_eq!(t.person_name, "Esa Kiiskinen");
    assert_eq!(t.person_role, "Member of the Board");
    assert!(t
        .amendment_reason
        .starts_with("This is to amend the Managers' Transactions"));
    assert!(t.amendment_reason.contains("jointly controlled"));
    // The text names a date, not a reference number: the amended id stays empty.
    assert_eq!(t.original_reference_number, None);
    assert_eq!(t.amended_transaction_id(), "");
    assert_eq!(tx[0].volume.to_string(), "2");
    assert_eq!(tx[4].unit_price.unwrap().to_string(), "15.52");
    assert_eq!(tx[5].venue.as_deref(), Some("BEUP"));
    assert_eq!(tx[20].volume.to_string(), "68");
    assert_eq!(tx[20].unit_price.unwrap().to_string(), "15.1");
    assert_eq!(tx[20].id(), "FIN_MAR:41890/11/16:20");
}

#[test]
fn plain_layout_hiab() {
    let tx = parse(&fixture("hiab_plain_en.txt"), "en").unwrap();
    assert_eq!(tx.len(), 1);
    let t = &tx[0];
    assert_eq!(t.person_name, "Raija-Leena Hankonen-Nybom");
    assert_eq!(t.person_role, "Member of the Board/Deputy member");
    assert_eq!(t.venue.as_deref(), Some("XHEL"));
    assert_eq!(t.isin, "FI4000571013");
    assert_eq!(t.transaction_type, T::Grant);
    assert_eq!(t.volume.to_string(), "810");
    assert!(t.unit_price.unwrap().is_zero());
}

#[test]
fn plain_layout_orion_with_signature_box_in_footer() {
    let tx = parse(&fixture("orion_plain_en.txt"), "en").unwrap();
    assert_eq!(tx.len(), 1);
    assert_eq!(tx[0].person_name, "Sophie Papa");
    assert_eq!(tx[0].volume.to_string(), "358");
    assert_eq!(
        tx[0].venue.as_deref(),
        Some("OFF-EXCHANGE LIIKETOIMET (XOFF)")
    );
    assert_eq!(tx[0].currency, "EUR");
}

#[test]
fn garbage_is_an_error_not_a_partial_transaction() {
    assert_eq!(
        parse("Q3 interim report", "en").unwrap_err(),
        MarParseError::MissingName
    );
    let truncated: String = fixture("metso_ceo_en.txt")
        .lines()
        .take(12)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(parse(&truncated, "en").is_err());
}
