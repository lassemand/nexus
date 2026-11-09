use rust_decimal::Decimal;

use crate::NumberStyle;

/// A figure read from the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Num {
    /// The exact value.
    pub value: Decimal,
    /// Byte offset of the figure's first byte (its minus sign, if any).
    pub start: usize,
    /// Byte offset just past the figure's last digit.
    pub end: usize,
}

const MAX_DIGITS: usize = 28;
const PRIOR_LOOKAHEAD: usize = 48;
const UNIT_MAX_BYTES: usize = 16;
const SPACES: [&[u8]; 4] = [
    b" ",
    "\u{a0}".as_bytes(),
    "\u{202f}".as_bytes(),
    "\u{2009}".as_bytes(),
];
const DASHES: [&[u8]; 2] = ["\u{2212}".as_bytes(), "\u{2013}".as_bytes()];

/// The first figure starting within `window` bytes from `from`.
///
/// Reads `1,234.5`, `1 234,5` and `1.234,5` as `style` says. Thousands
/// separators may also be a no-break, narrow no-break or thin space; a group
/// must be exactly three digits followed by a non-digit. A `-`, `−` or `–`
/// directly before the digits is a minus sign, unless a digit (or a digit and a
/// space) comes before it (`2025–2026` is a range). Parentheses are not a
/// minus sign: `67.3 (72.7)` is this year (last year).
///
/// Skipped: digits right after an ASCII letter (`Q3`, `H1`), footnote markers
/// `^{1}` and `[1]`, and four-digit integers 1990–2099 with no separator or
/// decimals unless followed by `%` (years). Figures of more than 28 digits are
/// skipped too. Values are built exactly, never through `f64`.
pub fn next_number(hay: &str, from: usize, window: usize, style: NumberStyle) -> Option<Num> {
    let bytes = hay.as_bytes();
    let limit = from.saturating_add(window).min(bytes.len());
    let mut at = from;
    while at < limit {
        if !is_digit(bytes, at) {
            at += 1;
            continue;
        }
        let run_end = digits_end(bytes, at);
        let after_word = at > 0 && bytes.get(at - 1).is_some_and(|b| b.is_ascii_alphanumeric());
        if after_word || is_footnote(bytes, at, run_end) {
            at = run_end;
            continue;
        }
        let parsed = parse_figure(bytes, at, run_end, style);
        if let Some(value) = parsed.value {
            let sign = minus_sign(bytes, at).filter(|start| *start >= from);
            let (value, start) = match sign {
                Some(start) => (-value, start),
                None => (value, at),
            };
            return Some(Num {
                value,
                start,
                end: parsed.end,
            });
        }
        at = parsed.end.max(run_end);
    }
    None
}

/// The first figure within `window` bytes from `from`, and the prior-period
/// figure after it.
///
/// The prior figure is the number inside the first `(` within 48 bytes after
/// the current figure, which must end before the `)`. The `(` may follow a
/// single line break (MFN wraps at about 80 columns) but not a blank line.
pub fn current_and_prior(
    hay: &str,
    from: usize,
    window: usize,
    style: NumberStyle,
) -> Option<(Num, Option<Num>)> {
    let current = next_number(hay, from, window, style)?;
    let bytes = hay.as_bytes();
    let scan_end = current.end.saturating_add(PRIOR_LOOKAHEAD).min(bytes.len());
    let mut line_breaks = 0;
    let mut prior = None;
    for at in current.end..scan_end {
        match bytes.get(at) {
            Some(b'\n') => {
                line_breaks += 1;
                if line_breaks > 1 {
                    break;
                }
            }
            Some(b'(') => {
                prior = in_parentheses(hay, at, style);
                break;
            }
            _ => {}
        }
    }
    Some((current, prior))
}

/// The unit after a figure ending at `end`: `%`, or the next word of up to 16
/// bytes (cut on a char boundary), e.g. `million`, `MSEK`, `mkr`.
///
/// Spaces, no-break spaces and a single line break before the unit are skipped.
pub fn unit_after(hay: &str, end: usize) -> Option<&str> {
    let rest = hay.get(end..)?;
    let mut line_break = false;
    let mut word_start = rest.len();
    for (offset, c) in rest.char_indices() {
        match c {
            ' ' | '\t' | '\r' | '\u{a0}' | '\u{202f}' | '\u{2009}' => {}
            '\n' if !line_break => line_break = true,
            _ => {
                word_start = offset;
                break;
            }
        }
    }
    let rest = rest.get(word_start..)?;
    if rest.starts_with('%') {
        return rest.get(..1);
    }
    let mut word_end = 0;
    for c in rest.chars() {
        if !c.is_alphabetic() || word_end + c.len_utf8() > UNIT_MAX_BYTES {
            break;
        }
        word_end += c.len_utf8();
    }
    match word_end {
        0 => None,
        len => rest.get(..len),
    }
}

fn in_parentheses(hay: &str, open: usize, style: NumberStyle) -> Option<Num> {
    let inside = hay.as_bytes().get(open + 1..)?;
    let close = open + 1 + memchr::memchr(b')', inside)?;
    let prior = next_number(hay, open + 1, close - (open + 1), style)?;
    (prior.end <= close).then_some(prior)
}

struct Figure {
    value: Option<Decimal>,
    end: usize,
}

fn parse_figure(bytes: &[u8], start: usize, run_end: usize, style: NumberStyle) -> Figure {
    let mut digits: Vec<(usize, usize)> = vec![(start, run_end)];
    let mut end = run_end;
    if run_end - start <= 3 {
        while let Some(sep) = thousands_separator(bytes, end, style) {
            let group = end + sep;
            if digits_end(bytes, group) != group + 3 {
                break;
            }
            digits.push((group, group + 3));
            end = group + 3;
        }
    }
    let grouped = digits.len() > 1;
    let mut scale = 0;
    if bytes.get(end) == Some(&decimal_mark(style)) && is_digit(bytes, end + 1) {
        let fraction_end = digits_end(bytes, end + 1);
        scale = fraction_end - (end + 1);
        digits.push((end + 1, fraction_end));
        end = fraction_end;
    }
    let total: usize = digits.iter().map(|(a, b)| b - a).sum();
    if total > MAX_DIGITS {
        return Figure { value: None, end };
    }
    let mut mantissa: i128 = 0;
    for &(a, b) in &digits {
        for digit in bytes.get(a..b).unwrap_or(&[]) {
            mantissa = mantissa * 10 + i128::from(digit - b'0');
        }
    }
    let is_year = !grouped
        && scale == 0
        && run_end - start == 4
        && (1990..=2099).contains(&mantissa)
        && bytes.get(end) != Some(&b'%');
    if is_year {
        return Figure { value: None, end };
    }
    let value = u32::try_from(scale)
        .ok()
        .and_then(|scale| Decimal::try_from_i128_with_scale(mantissa, scale).ok());
    Figure { value, end }
}

fn thousands_separator(bytes: &[u8], at: usize, style: NumberStyle) -> Option<usize> {
    let rest = bytes.get(at..)?;
    if let Some(space) = SPACES.iter().find(|space| rest.starts_with(space)) {
        return Some(space.len());
    }
    let mark = match style {
        NumberStyle::DotDecimal => b',',
        NumberStyle::DotThousands => b'.',
        NumberStyle::CommaDecimal => return None,
    };
    (rest.first() == Some(&mark)).then_some(1)
}

fn decimal_mark(style: NumberStyle) -> u8 {
    match style {
        NumberStyle::DotDecimal => b'.',
        NumberStyle::CommaDecimal | NumberStyle::DotThousands => b',',
    }
}

fn minus_sign(bytes: &[u8], digit: usize) -> Option<usize> {
    let before = bytes.get(..digit)?;
    let sign = if before.ends_with(b"-") {
        digit - 1
    } else {
        let dash = DASHES.iter().find(|dash| before.ends_with(dash))?;
        digit - dash.len()
    };
    let is_range = match sign.checked_sub(1).and_then(|at| bytes.get(at)) {
        Some(b) if b.is_ascii_digit() => true,
        Some(b' ') => sign >= 2 && is_digit(bytes, sign - 2),
        _ => false,
    };
    (!is_range).then_some(sign)
}

fn is_footnote(bytes: &[u8], start: usize, run_end: usize) -> bool {
    if run_end - start > 2 {
        return false;
    }
    let before = bytes.get(..start).unwrap_or(&[]);
    let after = bytes.get(run_end);
    (before.ends_with(b"^{") && after == Some(&b'}'))
        || (before.ends_with(b"[") && after == Some(&b']'))
}

fn is_digit(bytes: &[u8], at: usize) -> bool {
    bytes.get(at).is_some_and(u8::is_ascii_digit)
}

fn digits_end(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while is_digit(bytes, at) {
        at += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const EN: NumberStyle = NumberStyle::DotDecimal;
    const SV: NumberStyle = NumberStyle::CommaDecimal;
    const DA: NumberStyle = NumberStyle::DotThousands;

    fn first(hay: &str, style: NumberStyle) -> Option<String> {
        next_number(hay, 0, hay.len(), style).map(|n| n.value.to_string())
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal literal")
    }

    #[test]
    fn english() {
        assert_eq!(first("67.3", EN).as_deref(), Some("67.3"));
        assert_eq!(first("1,234.5", EN).as_deref(), Some("1234.5"));
        assert_eq!(first("1 234 567", EN).as_deref(), Some("1234567"));
        assert_eq!(first("EBIT -6.1", EN).as_deref(), Some("-6.1"));
        assert_eq!(first("EBIT −6.1", EN).as_deref(), Some("-6.1"));
        assert_eq!(first("EBIT –6.1", EN).as_deref(), Some("-6.1"));
    }

    #[test]
    fn swedish() {
        assert_eq!(first("1 234,5", SV).as_deref(), Some("1234.5"));
        assert_eq!(first("1\u{a0}234,5", SV).as_deref(), Some("1234.5"));
        assert_eq!(first("1\u{202f}234,5", SV).as_deref(), Some("1234.5"));
        assert_eq!(first("1\u{2009}234,5", SV).as_deref(), Some("1234.5"));
        assert_eq!(first("12,345", SV).as_deref(), Some("12.345"));
        assert_eq!(first("12,345", EN).as_deref(), Some("12345"));
    }

    #[test]
    fn danish() {
        assert_eq!(first("1.234,5", DA).as_deref(), Some("1234.5"));
        assert_eq!(first("234,5", DA).as_deref(), Some("234.5"));
    }

    #[test]
    fn thousands_group_is_exactly_three_digits() {
        assert_eq!(first("1, 2 and 3", EN).as_deref(), Some("1"));
        assert_eq!(first("1,2345", EN).as_deref(), Some("1"));
        assert_eq!(first("1 23", EN).as_deref(), Some("1"));
    }

    #[test]
    fn skips_periods_years_and_footnotes() {
        assert_eq!(first("Q3 2026 were 67.3", EN).as_deref(), Some("67.3"));
        assert_eq!(first("2025–2026: 12", EN).as_deref(), Some("12"));
        assert_eq!(first("2025%", EN).as_deref(), Some("2025"));
        assert_eq!(first("H1 sales 5", EN).as_deref(), Some("5"));
        assert_eq!(first("EUR 67.3m", EN).as_deref(), Some("67.3"));
        assert_eq!(
            first("per share^{1}, SEK │3.73", EN).as_deref(),
            Some("3.73")
        );
        assert_eq!(first("per share[2] SEK 3.73", EN).as_deref(), Some("3.73"));
        assert_eq!(
            first("per share¹ amounted to SEK 4.24", EN).as_deref(),
            Some("4.24")
        );
        assert_eq!(first("in 1989 sales", EN).as_deref(), Some("1989"));
        assert_eq!(first("2025.5", EN).as_deref(), Some("2025.5"));
    }

    #[test]
    fn range_dash_is_not_minus() {
        assert_eq!(first("12 –5", EN).as_deref(), Some("12"));
        let hay = "12 –5";
        let second = next_number(hay, 2, hay.len(), EN).expect("a figure");
        assert_eq!(second.value, dec("5"));
        let hay = "a -5";
        let n = next_number(hay, 0, hay.len(), EN).expect("a figure");
        assert_eq!((n.value, n.start, n.end), (dec("-5"), 2, 4));
    }

    #[test]
    fn exact_decimals() {
        let hay = "0.1+0.1+0.1";
        let mut at = 0;
        let mut sum = Decimal::ZERO;
        while let Some(n) = next_number(hay, at, hay.len(), EN) {
            sum += n.value;
            at = n.end;
        }
        assert_eq!(sum, dec("0.3"));
        assert_eq!(sum.to_string(), "0.3");
    }

    #[test]
    fn window_bounds_the_start() {
        let hay = "abc 12";
        assert_eq!(next_number(hay, 0, 4, EN), None);
        assert_eq!(next_number(hay, 0, 5, EN).map(|n| n.value), Some(dec("12")));
        assert_eq!(next_number(hay, 10, 5, EN), None);
    }

    #[test]
    fn too_many_digits_is_skipped() {
        let hay = "12345678901234567890123456789 7";
        assert_eq!(first(hay, EN).as_deref(), Some("7"));
    }

    fn prior(hay: &str, style: NumberStyle) -> (String, Option<String>) {
        let (current, prior) = current_and_prior(hay, 0, 200, style).expect("a figure");
        (
            current.value.to_string(),
            prior.map(|p| p.value.to_string()),
        )
    }

    #[test]
    fn prior_figures() {
        assert_eq!(
            prior("SEK 67.3 million (72.7)", EN),
            ("67.3".into(), Some("72.7".into()))
        );
        assert_eq!(
            prior("EBIT -6.1 (-11.7)", EN),
            ("-6.1".into(), Some("-11.7".into()))
        );
        assert_eq!(
            prior("SEK 67.3\n  million (72.7)", EN),
            ("67.3".into(), Some("72.7".into()))
        );
        assert_eq!(
            prior("SEK 67.3 million.\n\nNext section (12)", EN),
            ("67.3".into(), None)
        );
        assert_eq!(
            prior("67,3 MSEK (72,7)", SV),
            ("67.3".into(), Some("72.7".into()))
        );
        assert_eq!(prior("67.3 (n/a) 5", EN), ("67.3".into(), None));
        assert_eq!(prior("67.3 (5", EN), ("67.3".into(), None));
        assert_eq!(
            prior(
                "67.3 million, which is a lot more than it has ever been before (72.7)",
                EN
            ),
            ("67.3".into(), None)
        );
    }

    #[test]
    fn units() {
        let hay = "SEK 67.3 million (72.7)";
        let n = next_number(hay, 0, hay.len(), EN).expect("a figure");
        assert_eq!(unit_after(hay, n.end), Some("million"));
        assert_eq!(unit_after("5 MSEK", 1), Some("MSEK"));
        assert_eq!(unit_after("5\u{a0}mkr", 1), Some("mkr"));
        assert_eq!(unit_after("5 %", 1), Some("%"));
        assert_eq!(unit_after("5%", 1), Some("%"));
        assert_eq!(unit_after("67.3m", 4), Some("m"));
        assert_eq!(unit_after("5\n million", 1), Some("million"));
        assert_eq!(unit_after("5\n\nmillion", 1), None);
        assert_eq!(unit_after("5 (6)", 1), None);
        assert_eq!(unit_after("5", 1), None);
        assert_eq!(unit_after("5 öööööööööö", 1), Some("öööööööö"));
    }
}
