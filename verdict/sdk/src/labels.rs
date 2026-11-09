use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};

/// Where a label was found: `hay[start..end]` is the matched phrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// Byte offset of the phrase's first byte.
    pub start: usize,
    /// Byte offset just past the phrase; read figures from here.
    pub end: usize,
}

/// Every metric's label phrases, searched for in one pass.
///
/// One Aho-Corasick DFA over all phrases, ASCII case-insensitive, leftmost
/// longest. Build it once, in a `static LazyLock`.
#[derive(Debug, Clone)]
pub struct Labels {
    automaton: Option<AhoCorasick>,
    metric_of: Vec<usize>,
    open: Vec<bool>,
    metrics: usize,
    searchable: usize,
}

impl Labels {
    /// Builds the automaton. `metrics[i]` lists the phrases of metric `i`.
    ///
    /// A phrase must start at a word boundary and end at one, unless it ends
    /// with `*`: then it is an open stem (`nettoomsättning*` also matches
    /// `nettoomsättningen`). Matching is case-insensitive for ASCII letters
    /// only, so list both cases of `Ø`/`ø` and the like.
    ///
    /// When phrases overlap at the same byte the longest wins, across metrics.
    /// This never fails: if no automaton can be built, every search finds
    /// nothing, and a kernel then abstains because its metrics are missing.
    pub fn new(metrics: &[&[&str]]) -> Labels {
        let mut patterns = Vec::new();
        let mut metric_of = Vec::new();
        let mut open = Vec::new();
        let mut has_phrase = vec![false; metrics.len()];
        for (metric, phrases) in metrics.iter().enumerate() {
            for phrase in phrases.iter() {
                let (pattern, is_open) = match phrase.strip_suffix('*') {
                    Some(stem) => (stem, true),
                    None => (*phrase, false),
                };
                if pattern.is_empty() {
                    continue;
                }
                patterns.push(pattern);
                metric_of.push(metric);
                open.push(is_open);
                if let Some(flag) = has_phrase.get_mut(metric) {
                    *flag = true;
                }
            }
        }
        let build = |kind: Option<AhoCorasickKind>| {
            AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .match_kind(MatchKind::LeftmostLongest)
                .kind(kind)
                .build(&patterns)
        };
        let automaton = if patterns.is_empty() {
            None
        } else {
            build(Some(AhoCorasickKind::DFA))
                .or_else(|_| build(None))
                .ok()
        };
        Labels {
            automaton,
            metric_of,
            open,
            metrics: metrics.len(),
            searchable: has_phrase.iter().filter(|flag| **flag).count(),
        }
    }

    /// The number of metrics.
    pub fn len(&self) -> usize {
        self.metrics
    }

    /// True when there are no metrics.
    pub fn is_empty(&self) -> bool {
        self.metrics == 0
    }

    /// The first valid hit of each metric, indexed like the `metrics` passed to
    /// [`Labels::new`]. Stops as soon as every metric has one.
    pub fn first_hits(&self, hay: &str) -> Vec<Option<Hit>> {
        self.scan(hay, true)
    }

    /// Like [`Labels::first_hits`] but always scans the whole text. For
    /// benchmarks only.
    pub fn first_hits_full_scan(&self, hay: &str) -> Vec<Option<Hit>> {
        self.scan(hay, false)
    }

    fn scan(&self, hay: &str, stop_early: bool) -> Vec<Option<Hit>> {
        let mut hits = vec![None; self.metrics];
        let Some(automaton) = &self.automaton else {
            return hits;
        };
        let mut missing = self.searchable;
        for found in automaton.find_iter(hay) {
            let pattern = found.pattern().as_usize();
            let Some(&metric) = self.metric_of.get(pattern) else {
                continue;
            };
            let Some(slot) = hits.get_mut(metric) else {
                continue;
            };
            if slot.is_some() || !left_boundary(hay, found.start()) {
                continue;
            }
            let is_open = self.open.get(pattern).copied().unwrap_or(false);
            if !is_open && !right_boundary(hay, found.end()) {
                continue;
            }
            *slot = Some(Hit {
                start: found.start(),
                end: found.end(),
            });
            missing = missing.saturating_sub(1);
            if stop_early && missing == 0 {
                break;
            }
        }
        hits
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphabetic() || c.is_ascii_digit()
}

/// True when a word may start at byte `at`: it is the start of the text, or
/// the character before it is neither alphabetic nor an ASCII digit. Bullets,
/// dashes, no-break spaces and superscripts are boundaries; `å` is not. False
/// when `at` is not a char boundary.
pub fn left_boundary(hay: &str, at: usize) -> bool {
    match hay.get(..at) {
        Some(before) => before.chars().next_back().is_none_or(|c| !is_word_char(c)),
        None => false,
    }
}

/// True when a word may end at byte `at`: it is the end of the text, or the
/// character from it is neither alphabetic nor an ASCII digit (`share¹` ends at
/// `share`). False when `at` is not a char boundary.
pub fn right_boundary(hay: &str, at: usize) -> bool {
    match hay.get(at..) {
        Some(after) => after.chars().next().is_none_or(|c| !is_word_char(c)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(labels: &Labels, hay: &str) -> Vec<Option<(usize, usize)>> {
        labels
            .first_hits(hay)
            .into_iter()
            .map(|hit| hit.map(|h| (h.start, h.end)))
            .collect()
    }

    #[test]
    fn left_boundaries() {
        assert!(left_boundary("•Net", "•".len()));
        assert!(left_boundary("–Net", "–".len()));
        assert!(left_boundary("\u{a0}Net", 2));
        assert!(left_boundary("Net", 0));
        assert!(left_boundary(" Net", 1));
        assert!(!left_boundary("åNet", "å".len()));
        assert!(!left_boundary("xNet", 1));
        assert!(!left_boundary("3Net", 1));
        assert!(!left_boundary("•Net", 1));
    }

    #[test]
    fn right_boundaries() {
        assert!(!right_boundary("Net salesen", 9));
        assert!(right_boundary("Net sales", 9));
        assert!(right_boundary("Net sales.", 9));
        assert!(right_boundary("share¹", 5));
        assert!(right_boundary("share\u{a0}", 5));
        assert!(!right_boundary("shareö", 5));
    }

    #[test]
    fn closed_phrase_needs_both_boundaries() {
        let labels = Labels::new(&[&["net sales"]]);
        assert_eq!(
            hits(&labels, "Net salesen 5. Net sales 6"),
            vec![Some((15, 24))]
        );
        assert_eq!(hits(&labels, "•Net sales"), vec![Some((3, 12))]);
        assert_eq!(hits(&labels, "åNet sales"), vec![None]);
    }

    #[test]
    fn open_stem() {
        let labels = Labels::new(&[&["nettoomsättning*"]]);
        let hay = "Nettoomsättningen ökade";
        assert_eq!(hits(&labels, hay), vec![Some((0, "Nettoomsättning".len()))]);
        assert_eq!(hits(&labels, "xnettoomsättning"), vec![None]);
    }

    #[test]
    fn longest_wins_at_same_byte() {
        let labels = Labels::new(&[&["revenue"], &["revenues"]]);
        assert_eq!(hits(&labels, "Revenues rose"), vec![None, Some((0, 8))]);
        let labels = Labels::new(&[&["revenue", "revenues"]]);
        assert_eq!(hits(&labels, "Revenues rose"), vec![Some((0, 8))]);
    }

    #[test]
    fn ebitda_is_not_ebit() {
        let labels = Labels::new(&[&["ebit"]]);
        assert_eq!(hits(&labels, "EBITDA was 5"), vec![None]);
        assert_eq!(hits(&labels, "EBITDA was 5, EBIT 4"), vec![Some((14, 18))]);
    }

    #[test]
    fn ebit_margin_goes_to_margin() {
        let labels = Labels::new(&[&["ebit margin"], &["ebit"]]);
        assert_eq!(hits(&labels, "EBIT margin 5%"), vec![Some((0, 11)), None]);
        assert_eq!(
            hits(&labels, "EBIT margin 5%. EBIT 4"),
            vec![Some((0, 11)), Some((16, 20))]
        );
    }

    #[test]
    fn first_hit_only_and_full_scan_agree() {
        let labels = Labels::new(&[&["a"], &["b"], &[]]);
        let hay = "a b a b";
        assert_eq!(labels.first_hits(hay), labels.first_hits_full_scan(hay));
        assert_eq!(hits(&labels, hay), vec![Some((0, 1)), Some((2, 3)), None]);
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn empty_labels_find_nothing() {
        let labels = Labels::new(&[]);
        assert!(labels.is_empty());
        assert!(labels.first_hits("anything").is_empty());
        let labels = Labels::new(&[&["*"]]);
        assert_eq!(hits(&labels, "anything"), vec![None]);
    }
}
