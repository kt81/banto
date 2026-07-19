//! Fuzzy search over indexed session strings (titles / paths), backed by nucleo.
//!
//! This is a thin, synchronous ranking wrapper around nucleo's single-threaded
//! matcher. The TUI calls [`rank`] on every keystroke with the current query
//! and the list of candidate strings; it deliberately does not use nucleo's
//! concurrent worker engine (the session lists are small and matching a query
//! this way is cheap and easy to reason about).

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

/// One ranked result referring back into the input slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Index into the `haystacks` slice passed to [`rank`].
    pub index: usize,
    /// nucleo match score; a higher value is a better match.
    pub score: u32,
}

/// Rank `haystacks` against `query`.
///
/// - An empty (or whitespace-only) query returns every index in the original
///   order with score `0`.
/// - Otherwise only matching items are returned, sorted by score descending;
///   ties are broken by index ascending (stable).
///
/// Matching is fuzzy (subsequence based) with smart-case: an all-lowercase
/// query matches case-insensitively, while any uppercase character in the query
/// makes it case-sensitive. Whitespace-separated words are matched as
/// independent fuzzy atoms (all must match). Unicode / Japanese input is matched
/// without panicking.
pub fn rank(query: &str, haystacks: &[String]) -> Vec<Match> {
    // Fast path: no query means "show everything" in the caller's own order.
    if query.trim().is_empty() {
        return haystacks
            .iter()
            .enumerate()
            .map(|(index, _)| Match { index, score: 0 })
            .collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );

    // Both the matcher and this UTF-32 conversion buffer are reused across every
    // candidate so we keep their allocations instead of reallocating per item.
    let mut buf: Vec<char> = Vec::new();
    let mut matches: Vec<Match> = haystacks
        .iter()
        .enumerate()
        .filter_map(|(index, haystack)| {
            let candidate = Utf32Str::new(haystack, &mut buf);
            pattern
                .score(candidate, &mut matcher)
                .map(|score| Match { index, score })
        })
        .collect();

    // Score descending, then index ascending. `sort_by` is stable, so the tie
    // break is deterministic and preserves the original relative order.
    matches.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_string()).collect()
    }

    fn indices(matches: &[Match]) -> Vec<usize> {
        matches.iter().map(|m| m.index).collect()
    }

    #[test]
    fn empty_query_returns_everything_in_order() {
        let hay = strings(&["foo", "bar", "baz"]);
        let got = rank("", &hay);
        assert_eq!(indices(&got), vec![0, 1, 2]);
        assert!(got.iter().all(|m| m.score == 0));
    }

    #[test]
    fn whitespace_only_query_is_treated_as_empty() {
        let hay = strings(&["foo", "bar"]);
        let got = rank("   \t ", &hay);
        assert_eq!(indices(&got), vec![0, 1]);
        assert!(got.iter().all(|m| m.score == 0));
    }

    #[test]
    fn basic_fuzzy_subsequence() {
        let hay = strings(&["foobar", "xyz", "fizzbuzz"]);
        let got = rank("fb", &hay);
        let idx = indices(&got);
        // "fb" is a subsequence of "foobar" and "fizzbuzz" but not "xyz".
        assert!(idx.contains(&0));
        assert!(idx.contains(&2));
        assert!(!idx.contains(&1));
    }

    #[test]
    fn no_match_yields_empty() {
        let hay = strings(&["alpha", "beta"]);
        let got = rank("zzzz", &hay);
        assert!(got.is_empty());
    }

    #[test]
    fn better_match_ranks_higher() {
        // A contiguous match should outrank the same characters spread out.
        let hay = strings(&["a_b_c", "abc"]);
        let got = rank("abc", &hay);
        assert_eq!(got.len(), 2);
        assert_eq!(got.first().map(|m| m.index), Some(1));
        assert!(got[0].score > got[1].score);
    }

    #[test]
    fn ties_broken_by_index_ascending() {
        // Identical haystacks score equally; output must keep index order.
        let hay = strings(&["match", "match", "match"]);
        let got = rank("mat", &hay);
        assert_eq!(indices(&got), vec![0, 1, 2]);
    }

    #[test]
    fn smart_case_lowercase_query_is_insensitive() {
        let hay = strings(&["ABCDEF"]);
        let got = rank("abc", &hay);
        assert_eq!(indices(&got), vec![0]);
    }

    #[test]
    fn smart_case_uppercase_query_is_sensitive() {
        let hay = strings(&["abcdef", "ABCDEF"]);
        let got = rank("ABC", &hay);
        // The uppercase query must not match the lowercase haystack.
        assert_eq!(indices(&got), vec![1]);
    }

    #[test]
    fn japanese_query_matches_contiguous_substring() {
        let hay = strings(&["東京タワー", "大阪城", "東京駅"]);
        let got = rank("東京", &hay);
        let idx = indices(&got);
        assert!(idx.contains(&0));
        assert!(idx.contains(&2));
        assert!(!idx.contains(&1));
    }

    #[test]
    fn emoji_and_multibyte_do_not_panic() {
        let hay = strings(&["party 🎉 time", "no emoji here", "日本語テキスト"]);
        // Both querying with an emoji and querying an emoji-bearing list must be
        // panic-free on multibyte / grapheme input.
        let _ = rank("time", &hay);
        let got = rank("🎉", &hay);
        assert!(indices(&got).contains(&0));
    }
}
