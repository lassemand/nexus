/// A tradeable asset identified by its ticker symbol.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Asset {
    pub ticker: String,
}

impl Asset {
    pub fn new(ticker: impl Into<String>) -> Self {
        Self {
            ticker: ticker.into(),
        }
    }
}
