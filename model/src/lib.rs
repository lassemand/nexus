pub mod asset;
pub mod bar;
pub mod calendar;
pub mod exchange;
pub mod insider;
pub mod isin;
pub mod nordic;
pub mod price;
pub mod sector;
pub mod stockholm;

pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/nexus.rs"));
}

#[cfg(test)]
mod daily_bar_tests {
    use super::generated::DailyBar;
    use prost::Message;

    #[test]
    fn decimal_strings_round_trip_exactly() {
        let bar = DailyBar {
            ticker: "GOMX".into(),
            exchange_mic: "FNSE".into(),
            date: "2026-08-26".into(),
            open: "12.345678".into(),
            high: "13.000001".into(),
            low: "12.000000".into(),
            close: "12.345678".into(),
            volume: 1_234_567,
            currency: "SEK".into(),
            source: "saxo".into(),
            fetched_at_unix_secs: 1_790_000_000,
        };
        let decoded = DailyBar::decode(bar.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, bar);
        assert_eq!(decoded.close, "12.345678");
        assert_eq!(decoded.high, "13.000001");
    }
}

#[cfg(test)]
mod unified_insider_transaction_tests {
    use super::generated::{
        unified_insider_transaction, FiDetail, MarReleaseDetail, SecDetail, SourceRegistry,
        UnifiedInsiderTransaction, UnifiedTransactionType,
    };
    use prost::Message;

    fn sec_record() -> UnifiedInsiderTransaction {
        UnifiedInsiderTransaction {
            ticker: "AAPL".to_string(),
            exchange_mic: "XNAS".to_string(),
            source_registry: SourceRegistry::Sec as i32,
            person_name: "Tim Cook".to_string(),
            person_role: "CEO".to_string(),
            transaction_date: "2026-01-15".to_string(),
            published_date: "2026-01-16".to_string(),
            transaction_type: UnifiedTransactionType::Buy as i32,
            volume: 10_000.0,
            price_per_unit: 220.50,
            currency: "USD".to_string(),
            is_amendment: false,
            amended_transaction_id: String::new(),
            source_detail: Some(unified_insider_transaction::SourceDetail::Sec(SecDetail {
                issuer_cik: "0000320193".to_string(),
                filer_cik: "0001234567".to_string(),
                raw_transaction_code: "P".to_string(),
                filing_url: "https://www.sec.gov/Archives/edgar/data/320193/000001.txt".to_string(),
            })),
        }
    }

    fn fi_record() -> UnifiedInsiderTransaction {
        UnifiedInsiderTransaction {
            ticker: "GOMX".to_string(),
            exchange_mic: "FNSE".to_string(),
            source_registry: SourceRegistry::Fi as i32,
            person_name: "Lars Krogh Alminde".to_string(),
            person_role: "Annan ledande befattningshavare".to_string(),
            transaction_date: "2026-03-12".to_string(),
            published_date: "2026-03-15".to_string(),
            transaction_type: UnifiedTransactionType::Sell as i32,
            volume: 43_000.0,
            price_per_unit: 17.457,
            currency: "SEK".to_string(),
            is_amendment: false,
            amended_transaction_id: String::new(),
            source_detail: Some(unified_insider_transaction::SourceDetail::Fi(FiDetail {
                lei: "213800ES6SHKK5QTN734".to_string(),
                isin: "SE0008348304".to_string(),
                instrument_type: "Aktie".to_string(),
                is_close_associate: true,
                correction_description: String::new(),
            })),
        }
    }

    #[test]
    fn sec_record_round_trips() {
        let original = sec_record();
        let decoded =
            UnifiedInsiderTransaction::decode(original.encode_to_vec().as_slice()).unwrap();

        assert_eq!(decoded.ticker, "AAPL");
        assert_eq!(decoded.exchange_mic, "XNAS");
        assert_eq!(decoded.source_registry, SourceRegistry::Sec as i32);
        assert_eq!(decoded.person_name, "Tim Cook");
        assert_eq!(decoded.transaction_date, "2026-01-15");
        assert_eq!(decoded.transaction_type, UnifiedTransactionType::Buy as i32);
        assert_eq!(decoded.currency, "USD");
        assert!(!decoded.is_amendment);

        let sec = match decoded.source_detail.unwrap() {
            unified_insider_transaction::SourceDetail::Sec(s) => s,
            _ => panic!("expected SecDetail"),
        };
        assert_eq!(sec.issuer_cik, "0000320193");
        assert_eq!(sec.raw_transaction_code, "P");
        assert!(!sec.filing_url.is_empty());
    }

    #[test]
    fn fi_record_round_trips() {
        let original = fi_record();
        let decoded =
            UnifiedInsiderTransaction::decode(original.encode_to_vec().as_slice()).unwrap();

        assert_eq!(decoded.ticker, "GOMX");
        assert_eq!(decoded.exchange_mic, "FNSE");
        assert_eq!(decoded.source_registry, SourceRegistry::Fi as i32);
        assert_eq!(decoded.currency, "SEK");

        let fi = match decoded.source_detail.unwrap() {
            unified_insider_transaction::SourceDetail::Fi(f) => f,
            _ => panic!("expected FiDetail"),
        };
        assert_eq!(fi.lei, "213800ES6SHKK5QTN734");
        assert_eq!(fi.isin, "SE0008348304");
        assert_eq!(fi.instrument_type, "Aktie");
        assert!(fi.is_close_associate);
    }

    #[test]
    fn sec_amendment_round_trips() {
        let mut amendment = sec_record();
        amendment.is_amendment = true;
        amendment.amended_transaction_id = "SEC:AAPL:TIM COOK:2026-01-15".to_string();
        amendment.transaction_type = UnifiedTransactionType::Sell as i32;

        let decoded =
            UnifiedInsiderTransaction::decode(amendment.encode_to_vec().as_slice()).unwrap();

        assert!(decoded.is_amendment);
        assert_eq!(
            decoded.amended_transaction_id,
            "SEC:AAPL:TIM COOK:2026-01-15"
        );
        assert_eq!(
            decoded.transaction_type,
            UnifiedTransactionType::Sell as i32
        );
    }

    #[test]
    fn fi_amendment_round_trips() {
        let mut amendment = fi_record();
        amendment.is_amendment = true;
        amendment.amended_transaction_id = "FI:GOMX:LARS KROGH ALMINDE:2026-03-12".to_string();
        if let Some(unified_insider_transaction::SourceDetail::Fi(ref mut fi)) =
            amendment.source_detail
        {
            fi.correction_description = "Volume corrected from 86000 to 43000".to_string();
        }

        let decoded =
            UnifiedInsiderTransaction::decode(amendment.encode_to_vec().as_slice()).unwrap();

        assert!(decoded.is_amendment);
        let fi = match decoded.source_detail.unwrap() {
            unified_insider_transaction::SourceDetail::Fi(f) => f,
            _ => panic!("expected FiDetail"),
        };
        assert_eq!(
            fi.correction_description,
            "Volume corrected from 86000 to 43000"
        );
    }

    #[test]
    fn sec_and_fi_records_are_not_interchangeable() {
        // Encoding an SEC record and decoding as FI (or vice versa) must not
        // silently succeed — the oneof discriminator ensures this.
        let sec = sec_record().encode_to_vec();
        let decoded = UnifiedInsiderTransaction::decode(sec.as_slice()).unwrap();
        // source_registry field distinguishes SEC from FI
        assert_eq!(decoded.source_registry, SourceRegistry::Sec as i32);
        assert_ne!(decoded.source_registry, SourceRegistry::Fi as i32);
    }

    fn mar_record() -> UnifiedInsiderTransaction {
        UnifiedInsiderTransaction {
            ticker: "METSO".into(),
            exchange_mic: "XHEL".into(),
            source_registry: SourceRegistry::FinMar as i32,
            person_name: "Sami Takaluoma".into(),
            person_role: "Chief Executive Officer".into(),
            transaction_date: "2026-03-19".into(),
            published_date: "2026-03-20".into(),
            transaction_type: UnifiedTransactionType::Grant as i32,
            volume: 23201.0,
            price_per_unit: 0.0,
            currency: "EUR".into(),
            source_detail: Some(unified_insider_transaction::SourceDetail::Mar(
                MarReleaseDetail {
                    lei: "549300R0VN9C371W0E07".into(),
                    reference_number: "549300R0VN9C371W0E07_20260320093338_264".into(),
                    instrument_type: "Share".into(),
                    venue: "NASDAQ HELSINKI".into(),
                    nature: "Receipt of a Share-Based Incentive".into(),
                    mfn_news_id: "9954de80-88dd-5084-a03b-4b42e17c1644".into(),
                    lang: "en".into(),
                    volume_decimal: "23201".into(),
                    unit_price_decimal: "0.00".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    #[test]
    fn mar_record_round_trips() {
        let original = mar_record();
        let decoded =
            UnifiedInsiderTransaction::decode(original.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, original);
        assert_eq!(decoded.source_registry, SourceRegistry::FinMar as i32);
        assert_eq!(SourceRegistry::FinMar as i32, 3);
    }

    #[test]
    fn existing_registry_values_are_unchanged() {
        assert_eq!(SourceRegistry::Unspecified as i32, 0);
        assert_eq!(SourceRegistry::Sec as i32, 1);
        assert_eq!(SourceRegistry::Fi as i32, 2);
    }

    /// An old consumer (schema without field 16 / registry 3) sees the common
    /// fields intact; the unknown oneof member and enum value are tolerated.
    #[test]
    fn old_consumer_decodes_new_message() {
        // The previous schema, as it was before NEX-206.
        #[derive(Clone, PartialEq, prost::Message)]
        struct OldMsg {
            #[prost(string, tag = "1")]
            ticker: String,
            #[prost(enumeration = "OldRegistry", tag = "3")]
            source_registry: i32,
            #[prost(string, tag = "4")]
            person_name: String,
            #[prost(double, tag = "9")]
            volume: f64,
            #[prost(oneof = "OldDetail", tags = "14, 15")]
            source_detail: Option<OldDetail>,
        }
        #[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
        #[repr(i32)]
        enum OldRegistry {
            Unspecified = 0,
            Sec = 1,
            Fi = 2,
        }
        #[derive(Clone, PartialEq, prost::Oneof)]
        enum OldDetail {
            #[prost(message, tag = "14")]
            Sec(SecDetail),
            #[prost(message, tag = "15")]
            Fi(FiDetail),
        }

        let old = OldMsg::decode(mar_record().encode_to_vec().as_slice()).unwrap();
        assert_eq!(old.ticker, "METSO");
        assert_eq!(old.person_name, "Sami Takaluoma");
        assert_eq!(old.volume, 23201.0);
        assert_eq!(
            old.source_registry, 3,
            "unknown enum value is preserved as an int"
        );
        assert!(
            old.source_detail.is_none(),
            "unknown oneof member is skipped"
        );
    }

    #[test]
    fn new_consumer_decodes_old_sec_and_fi_messages() {
        for (record, registry) in [
            (sec_record(), SourceRegistry::Sec),
            (fi_record(), SourceRegistry::Fi),
        ] {
            let decoded =
                UnifiedInsiderTransaction::decode(record.encode_to_vec().as_slice()).unwrap();
            assert_eq!(decoded, record);
            assert_eq!(decoded.source_registry, registry as i32);
            assert!(!matches!(
                decoded.source_detail,
                Some(unified_insider_transaction::SourceDetail::Mar(_))
            ));
        }
    }
}

#[cfg(test)]
mod insider_transaction_tests {
    use super::generated::{InsiderTransaction, TransactionType};
    use prost::Message;

    fn sample_transaction() -> InsiderTransaction {
        InsiderTransaction {
            ticker: "GOMX".to_string(),
            exchange_mic: "FNSE".to_string(),
            pdmr_name: "Lars Krogh Alminde".to_string(),
            pdmr_role: "CEO".to_string(),
            is_close_associate: false,
            transaction_date_unix_secs: 1_700_000_000,
            publication_date_unix_secs: 1_700_086_400,
            transaction_type: TransactionType::Buy as i32,
            volume: 10_000.0,
            price_per_unit: 17.45,
            currency: "SEK".to_string(),
            isin: "SE0008348304".to_string(),
            instrument_type: "Aktie".to_string(),
            is_correction: false,
            correction_description: String::new(),
            source_registry: "FI".to_string(),
        }
    }

    #[test]
    fn normal_transaction_round_trips() {
        let original = sample_transaction();
        let encoded = original.encode_to_vec();
        let decoded = InsiderTransaction::decode(encoded.as_slice()).unwrap();

        assert_eq!(decoded.ticker, "GOMX");
        assert_eq!(decoded.exchange_mic, "FNSE");
        assert_eq!(decoded.pdmr_name, "Lars Krogh Alminde");
        assert_eq!(decoded.pdmr_role, "CEO");
        assert!(!decoded.is_close_associate);
        assert_eq!(decoded.transaction_type, TransactionType::Buy as i32);
        assert_eq!(decoded.volume, 10_000.0);
        assert_eq!(decoded.price_per_unit, 17.45);
        assert_eq!(decoded.currency, "SEK");
        assert_eq!(decoded.isin, "SE0008348304");
        assert_eq!(decoded.instrument_type, "Aktie");
        assert!(!decoded.is_correction);
        assert_eq!(decoded.source_registry, "FI");
    }

    #[test]
    fn correction_record_round_trips() {
        let mut correction = sample_transaction();
        correction.is_correction = true;
        correction.correction_description = "Volume corrected from 5000 to 10000".to_string();
        correction.transaction_type = TransactionType::Sell as i32;
        correction.is_close_associate = true;

        let encoded = correction.encode_to_vec();
        let decoded = InsiderTransaction::decode(encoded.as_slice()).unwrap();

        assert!(decoded.is_correction);
        assert_eq!(
            decoded.correction_description,
            "Volume corrected from 5000 to 10000"
        );
        assert_eq!(decoded.transaction_type, TransactionType::Sell as i32);
        assert!(decoded.is_close_associate);
    }

    #[test]
    fn other_transaction_type_encodes_correctly() {
        let mut txn = sample_transaction();
        txn.transaction_type = TransactionType::Other as i32;
        txn.instrument_type = "Warrant".to_string();

        let decoded = InsiderTransaction::decode(txn.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded.transaction_type, TransactionType::Other as i32);
        assert_eq!(decoded.instrument_type, "Warrant");
    }
}
