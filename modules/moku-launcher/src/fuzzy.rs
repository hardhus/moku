/// Returns `Some(score)` if every character of `query` (case-insensitive)
/// appears in `target` as a subsequence, in order (not necessarily
/// contiguous). Returns `None` if `query` is not a subsequence of `target`.
/// Higher score = better match. An empty `query` always matches with score 0.
///
/// Single left-to-right greedy scan — trivially fast for the short strings
/// and small item counts (module titles) this is used against, so a hand
/// -rolled matcher is preferred over pulling in a fuzzy-matching crate.
pub fn fuzzy_match(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }

    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = target.to_lowercase().chars().collect();

    let mut score = 0i32;
    let mut qi = 0usize;
    let mut prev_matched_ti: Option<usize> = None;

    for (ti, &tc) in t.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if tc == q[qi] {
            score += 1;
            let is_word_start = ti == 0 || !t[ti - 1].is_alphanumeric();
            if is_word_start {
                score += 8;
            }
            if prev_matched_ti == Some(ti.wrapping_sub(1)) {
                score += 5;
            }
            prev_matched_ti = Some(ti);
            qi += 1;
        }
    }

    if qi == q.len() {
        // Mild preference for tighter, shorter targets so a contiguous
        // match doesn't lose to a same-length scattered one elsewhere.
        score -= (t.len() as i32 - q.len() as i32) / 4;
        Some(score)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_query_matches_everything_with_zero_score() {
        assert_eq!(fuzzy_match("", "anything"), Some(0));
        assert_eq!(fuzzy_match("", ""), Some(0));
    }

    #[test]
    fn test_non_subsequence_does_not_match() {
        assert_eq!(fuzzy_match("xyz", "RSS Feed Reader"), None);
    }

    #[test]
    fn test_subsequence_matches_case_insensitively() {
        assert!(fuzzy_match("rss", "RSS Feed Reader").is_some());
        assert!(fuzzy_match("RSS", "rss feed reader").is_some());
    }

    #[test]
    fn test_word_start_scores_higher_than_mid_word() {
        let word_start = fuzzy_match("da", "Dashboard").unwrap();
        let mid_word = fuzzy_match("da", "xxdaxx").unwrap();
        assert!(
            word_start > mid_word,
            "word-start match should outscore a mid-word match"
        );
    }

    #[test]
    fn test_contiguous_scores_higher_than_scattered() {
        // Both matches are mid-word (no word-start bonus on either side)
        // so only the contiguous-run bonus differs between them.
        let contiguous = fuzzy_match("da", "xxdaxx").unwrap();
        let scattered = fuzzy_match("da", "xxdxxaxx").unwrap();
        assert!(
            contiguous > scattered,
            "contiguous match should outscore a scattered match"
        );
    }

    #[test]
    fn test_rss_query_ranks_rss_title_above_unrelated_titles() {
        let titles = [
            "Dashboard",
            "Todo List",
            "Bookmark",
            "RSS Feed Reader",
            "Notes",
            "Secrets",
        ];
        let mut scored: Vec<(&str, i32)> = titles
            .iter()
            .filter_map(|t| fuzzy_match("rss", t).map(|s| (*t, s)))
            .collect();
        scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        assert_eq!(scored[0].0, "RSS Feed Reader");
    }
}
