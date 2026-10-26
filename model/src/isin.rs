//! ISIN (ISO 6166) validation.

/// Whether `isin` is a well-formed ISIN with a correct check digit.
///
/// Format: two uppercase letters (country), nine alphanumerics, one digit.
/// The check digit is the Luhn digit over the string with every letter
/// replaced by its two-digit value (`A` = 10 … `Z` = 35).
pub fn is_valid(isin: &str) -> bool {
    let b = isin.as_bytes();
    if b.len() != 12
        || !b[..2].iter().all(u8::is_ascii_uppercase)
        || !b[2..11]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        || !b[11].is_ascii_digit()
    {
        return false;
    }
    let mut digits: Vec<u32> = Vec::with_capacity(23);
    for &c in b {
        if c.is_ascii_digit() {
            digits.push(u32::from(c - b'0'));
        } else {
            let v = u32::from(c - b'A') + 10;
            digits.push(v / 10);
            digits.push(v % 10);
        }
    }
    // Luhn: from the right, double every second digit (the check digit is not doubled).
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let x = d * 2;
                x / 10 + x % 10
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

#[cfg(test)]
mod tests {
    use super::is_valid;

    #[test]
    fn known_isins_are_valid() {
        assert!(is_valid("DK0062498333")); // Novo Nordisk B
        assert!(is_valid("SE0008348304")); // GomSpace
        assert!(is_valid("US0378331005")); // Apple
    }

    #[test]
    fn one_character_typo_is_invalid() {
        assert!(!is_valid("DK0062498332"));
        assert!(!is_valid("DK0062498433"));
        assert!(!is_valid("SE0008348305"));
    }

    #[test]
    fn malformed_is_invalid() {
        for s in [
            "",
            "DK006249833",
            "dk0062498333",
            "DK00624983333",
            "1K0062498333",
            "DK006249833A",
        ] {
            assert!(!is_valid(s), "{s}");
        }
    }
}
