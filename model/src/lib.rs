pub mod asset;
pub mod bar;
pub mod calendar;
pub mod price;
pub mod sector;

pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/nexus.rs"));
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
