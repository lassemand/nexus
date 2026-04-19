#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Sector {
    Technology,
    Healthcare,
    Financials,
    ConsumerDiscretionary,
    ConsumerStaples,
    Energy,
    Utilities,
    Industrials,
    Materials,
    RealEstate,
    CommunicationServices,
}

impl Sector {
    /// Canonical kebab-case name, used e.g. for Kafka topic routing.
    pub fn slug(&self) -> &'static str {
        match self {
            Sector::Technology => "technology",
            Sector::Healthcare => "healthcare",
            Sector::Financials => "financials",
            Sector::ConsumerDiscretionary => "consumer-discretionary",
            Sector::ConsumerStaples => "consumer-staples",
            Sector::Energy => "energy",
            Sector::Utilities => "utilities",
            Sector::Industrials => "industrials",
            Sector::Materials => "materials",
            Sector::RealEstate => "real-estate",
            Sector::CommunicationServices => "communication-services",
        }
    }
}
