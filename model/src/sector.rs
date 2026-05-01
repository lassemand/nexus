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

    /// Map a US SIC code to the closest GICS sector.
    pub fn from_sic(sic: u32) -> Sector {
        match sic {
            // Agriculture → ConsumerStaples
            100..=999 => Sector::ConsumerStaples,
            // Oil & gas extraction / services → Energy
            1311 | 1381..=1389 => Sector::Energy,
            // Other mining → Materials
            1000..=1499 => Sector::Materials,
            // Construction → Industrials
            1500..=1799 => Sector::Industrials,
            // Food & tobacco → ConsumerStaples
            2000..=2199 => Sector::ConsumerStaples,
            // Textiles, apparel, furniture → ConsumerDiscretionary
            2200..=2599 => Sector::ConsumerDiscretionary,
            // Paper → Materials
            2600..=2699 => Sector::Materials,
            // Industrial chemicals → Materials
            2700..=2829 => Sector::Materials,
            // Pharmaceuticals / drugs → Healthcare
            2830..=2836 => Sector::Healthcare,
            // Other chemicals → Materials
            2837..=2899 => Sector::Materials,
            // Petroleum refining → Energy
            2900..=2999 => Sector::Energy,
            // Rubber, stone, glass, primary/fabricated metals → Materials
            3000..=3399 => Sector::Materials,
            // Industrial machinery → Industrials
            3400..=3569 => Sector::Industrials,
            // Computers & electronic components → Technology
            3570..=3699 => Sector::Technology,
            // Motor vehicles → ConsumerDiscretionary
            3710..=3716 => Sector::ConsumerDiscretionary,
            // Aircraft & other transportation equipment → Industrials
            3717..=3799 => Sector::Industrials,
            // Medical instruments & supplies → Healthcare
            3841..=3851 => Sector::Healthcare,
            // Other instruments (test, optical, watches) → Technology
            3800..=3840 | 3852..=3899 => Sector::Technology,
            // Misc manufacturing → ConsumerDiscretionary
            3900..=3999 => Sector::ConsumerDiscretionary,
            // Rail, transit, trucking, water, air transport → Industrials
            4000..=4599 => Sector::Industrials,
            // Pipelines → Energy
            4600..=4699 => Sector::Energy,
            // Transportation services → Industrials
            4700..=4799 => Sector::Industrials,
            // Telecommunications → CommunicationServices
            4800..=4899 => Sector::CommunicationServices,
            // Electric, gas, water utilities → Utilities
            4900..=4991 => Sector::Utilities,
            // Wholesale durable goods → Industrials
            5000..=5099 => Sector::Industrials,
            // Wholesale non-durable (food, drugs) → ConsumerStaples
            5100..=5199 => Sector::ConsumerStaples,
            // Building materials, general merchandise → ConsumerDiscretionary
            5200..=5399 => Sector::ConsumerDiscretionary,
            // Food stores → ConsumerStaples
            5400..=5499 => Sector::ConsumerStaples,
            // Auto dealers, apparel, furniture, restaurants → ConsumerDiscretionary
            5500..=5899 => Sector::ConsumerDiscretionary,
            // Drug stores → ConsumerStaples
            5900..=5912 => Sector::ConsumerStaples,
            // Other retail → ConsumerDiscretionary
            5913..=5999 => Sector::ConsumerDiscretionary,
            // Banks, credit, insurance, brokerage → Financials
            6000..=6499 => Sector::Financials,
            // Real estate → RealEstate
            6500..=6552 => Sector::RealEstate,
            // Holding companies, investment trusts → Financials
            6553..=6797 | 6799 => Sector::Financials,
            // REITs → RealEstate
            6798 => Sector::RealEstate,
            // Hotels, lodging → ConsumerDiscretionary
            7000..=7099 => Sector::ConsumerDiscretionary,
            // Personal services → ConsumerDiscretionary
            7200..=7299 => Sector::ConsumerDiscretionary,
            // Business services → Industrials
            7300..=7369 => Sector::Industrials,
            // Computer programming / data processing → Technology
            7370..=7379 => Sector::Technology,
            // Misc business services → Industrials
            7380..=7499 => Sector::Industrials,
            // Auto repair → ConsumerDiscretionary
            7500..=7599 => Sector::ConsumerDiscretionary,
            // Misc repair → Industrials
            7600..=7799 => Sector::Industrials,
            // Motion pictures & entertainment → CommunicationServices
            7800..=7899 => Sector::CommunicationServices,
            // Amusement & recreation → ConsumerDiscretionary
            7900..=7999 => Sector::ConsumerDiscretionary,
            // Health services → Healthcare
            8000..=8099 => Sector::Healthcare,
            // Legal services → Industrials
            8100..=8199 => Sector::Industrials,
            // Educational services → ConsumerDiscretionary
            8200..=8299 => Sector::ConsumerDiscretionary,
            // Social services → Healthcare
            8300..=8399 => Sector::Healthcare,
            // Museums & cultural → ConsumerDiscretionary
            8400..=8499 => Sector::ConsumerDiscretionary,
            // Membership orgs, engineering, management consulting → Industrials
            8600..=8799 => Sector::Industrials,
            // Catch-all → Industrials
            _ => Sector::Industrials,
        }
    }
}
