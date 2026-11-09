/// Language of a release, as MFN tags it in `properties.lang`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    /// English.
    En,
    /// Swedish.
    Sv,
    /// Danish.
    Da,
    /// Finnish.
    Fi,
    /// Norwegian (Bokmål or Nynorsk).
    No,
    /// Any other or unknown language.
    Other,
}

/// How a language writes figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumberStyle {
    /// `1,234.5`: dot decimal, comma or space thousands.
    DotDecimal,
    /// `1 234,5`: comma decimal, space thousands.
    CommaDecimal,
    /// `1.234,5`: comma decimal, dot or space thousands.
    DotThousands,
}

impl Lang {
    /// The code passed over the WASM ABI: 1 en, 2 sv, 3 da, 4 fi, 5 no, 0 other.
    pub fn code(self) -> u32 {
        match self {
            Lang::En => 1,
            Lang::Sv => 2,
            Lang::Da => 3,
            Lang::Fi => 4,
            Lang::No => 5,
            Lang::Other => 0,
        }
    }

    /// The inverse of [`Lang::code`]; an unknown code is [`Lang::Other`].
    pub fn from_code(code: u32) -> Lang {
        match code {
            1 => Lang::En,
            2 => Lang::Sv,
            3 => Lang::Da,
            4 => Lang::Fi,
            5 => Lang::No,
            _ => Lang::Other,
        }
    }

    /// Reads an MFN `properties.lang` tag (`en`, `sv`, `da`, `fi`, `no`, `nb`,
    /// `nn`), ignoring case and any region subtag (`en-GB`).
    pub fn from_tag(tag: &str) -> Lang {
        let primary = tag.trim().split(['-', '_']).next().unwrap_or("");
        let is = |code: &str| primary.eq_ignore_ascii_case(code);
        if is("en") {
            Lang::En
        } else if is("sv") {
            Lang::Sv
        } else if is("da") {
            Lang::Da
        } else if is("fi") {
            Lang::Fi
        } else if is("no") || is("nb") || is("nn") {
            Lang::No
        } else {
            Lang::Other
        }
    }

    /// How releases in this language write figures.
    pub fn number_style(self) -> NumberStyle {
        match self {
            Lang::En | Lang::Other => NumberStyle::DotDecimal,
            Lang::Sv | Lang::Fi | Lang::No => NumberStyle::CommaDecimal,
            Lang::Da => NumberStyle::DotThousands,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for lang in [
            Lang::En,
            Lang::Sv,
            Lang::Da,
            Lang::Fi,
            Lang::No,
            Lang::Other,
        ] {
            assert_eq!(Lang::from_code(lang.code()), lang);
        }
        assert_eq!(Lang::from_code(99), Lang::Other);
    }

    #[test]
    fn tags() {
        assert_eq!(Lang::from_tag("en"), Lang::En);
        assert_eq!(Lang::from_tag("SV"), Lang::Sv);
        assert_eq!(Lang::from_tag("da"), Lang::Da);
        assert_eq!(Lang::from_tag("fi"), Lang::Fi);
        assert_eq!(Lang::from_tag("nb"), Lang::No);
        assert_eq!(Lang::from_tag("nn"), Lang::No);
        assert_eq!(Lang::from_tag("no"), Lang::No);
        assert_eq!(Lang::from_tag("en-GB"), Lang::En);
        assert_eq!(Lang::from_tag("de"), Lang::Other);
        assert_eq!(Lang::from_tag(""), Lang::Other);
    }

    #[test]
    fn styles() {
        assert_eq!(Lang::En.number_style(), NumberStyle::DotDecimal);
        assert_eq!(Lang::Other.number_style(), NumberStyle::DotDecimal);
        assert_eq!(Lang::Sv.number_style(), NumberStyle::CommaDecimal);
        assert_eq!(Lang::Fi.number_style(), NumberStyle::CommaDecimal);
        assert_eq!(Lang::No.number_style(), NumberStyle::CommaDecimal);
        assert_eq!(Lang::Da.number_style(), NumberStyle::DotThousands);
    }
}
