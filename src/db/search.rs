//! Hybrid vector + FTS5 search with Reciprocal Rank Fusion merging.

use std::collections::HashMap;

/// Escape a query string for safe use in FTS5 MATCH expressions.
///
/// Wraps each whitespace-delimited token in double quotes to prevent FTS5
/// operators (AND, OR, NOT, NEAR, *, "phrases") from being interpreted.
/// Internal double quotes are escaped by doubling them.
///
/// Tokens containing no alphanumeric characters (e.g. `->`, `||`) are dropped
/// because FTS5's unicode61 tokenizer produces zero tokens for them, which would
/// create an unmatchable AND clause that breaks the entire MATCH expression.
///
/// The resulting query uses FTS5's implicit AND: all tokens must appear in the
/// document for it to match. Vector search compensates for partial keyword matches.
///
/// Returns `None` if the query contains no searchable tokens.
pub(super) fn escape_fts5_query(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|token| token.chars().any(|c| c.is_alphanumeric()))
        .map(|token| {
            let escaped = token.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect();

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

/// A ranked item from a single search method.
#[derive(Debug)]
pub(super) struct RankedItem {
    pub id: String,
    /// 1-based rank within the source list.
    pub rank: u32,
}

/// Merge ranked lists using Reciprocal Rank Fusion.
///
/// Each item's score is `Σ(1 / (k + rank))` across all lists it appears in,
/// where rank is 1-based. Returns `(id, score)` pairs sorted by descending score.
pub(super) fn rrf_merge(lists: &[&[RankedItem]], k: u32) -> Vec<(String, f64)> {
    let mut scores: HashMap<&str, f64> = HashMap::new();

    for list in lists {
        for item in *list {
            *scores.entry(&item.id).or_default() += 1.0 / (k as f64 + item.rank as f64);
        }
    }

    let mut results: Vec<(String, f64)> = scores
        .into_iter()
        .map(|(id, score)| (id.to_string(), score))
        .collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── escape_fts5_query ───────────────────────────────────────────────

    #[test]
    fn escape_simple_query() {
        assert_eq!(
            escape_fts5_query("hello world"),
            Some(r#""hello" "world""#.to_string())
        );
    }

    #[test]
    fn escape_operators() {
        assert_eq!(
            escape_fts5_query("foo AND bar"),
            Some(r#""foo" "AND" "bar""#.to_string())
        );
    }

    #[test]
    fn escape_quotes_in_tokens() {
        // Input: say "hello"
        // Token "hello" has internal quotes → each " becomes ""
        // Then wrapped in outer quotes: """hello"""
        assert_eq!(
            escape_fts5_query(r#"say "hello""#),
            Some(r#""say" """hello""""#.to_string())
        );
    }

    #[test]
    fn escape_special_chars() {
        assert_eq!(
            escape_fts5_query("foo* NEAR bar"),
            Some(r#""foo*" "NEAR" "bar""#.to_string())
        );
    }

    #[test]
    fn escape_empty_query() {
        assert_eq!(escape_fts5_query(""), None);
        assert_eq!(escape_fts5_query("   "), None);
    }

    #[test]
    fn escape_drops_punctuation_only_tokens() {
        // Punctuation-only tokens produce zero FTS5 tokens inside a quoted phrase,
        // creating an unmatchable AND clause. They must be filtered out.
        assert_eq!(
            escape_fts5_query("error -> fix"),
            Some(r#""error" "fix""#.to_string())
        );
        assert_eq!(
            escape_fts5_query("|| operator"),
            Some(r#""operator""#.to_string())
        );
        // All-punctuation query should return None.
        assert_eq!(escape_fts5_query("-> => ||"), None);
    }

    #[test]
    fn escape_adversarial_inputs() {
        // All-quote token: contains no alphanumeric, so filtered out.
        assert_eq!(escape_fts5_query(r#""""#), None);

        // Unicode CJK characters.
        assert_eq!(
            escape_fts5_query("rust 错误处理"),
            Some(r#""rust" "错误处理""#.to_string())
        );

        // Emoji-only token (no alphanumeric).
        assert_eq!(escape_fts5_query("🦀 rust"), Some(r#""rust""#.to_string()));

        // Tab and newline in input (split_whitespace handles them).
        assert_eq!(
            escape_fts5_query("hello\t\nworld"),
            Some(r#""hello" "world""#.to_string())
        );
    }

    #[test]
    fn escape_single_token() {
        assert_eq!(escape_fts5_query("rust"), Some(r#""rust""#.to_string()));
    }

    // ── rrf_merge ───────────────────────────────────────────────────────

    #[test]
    fn rrf_single_list() {
        let list = vec![
            RankedItem {
                id: "a".into(),
                rank: 1,
            },
            RankedItem {
                id: "b".into(),
                rank: 2,
            },
            RankedItem {
                id: "c".into(),
                rank: 3,
            },
        ];
        let results = rrf_merge(&[&list], 60);

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, "a");
        assert_eq!(results[1].0, "b");
        assert_eq!(results[2].0, "c");
        assert!((results[0].1 - 1.0 / 61.0).abs() < 1e-10);
    }

    #[test]
    fn rrf_two_lists_boosts_overlap() {
        // "b" appears in both lists — should be ranked first.
        let vec_list = vec![
            RankedItem {
                id: "a".into(),
                rank: 1,
            },
            RankedItem {
                id: "b".into(),
                rank: 2,
            },
        ];
        let fts_list = vec![
            RankedItem {
                id: "b".into(),
                rank: 1,
            },
            RankedItem {
                id: "c".into(),
                rank: 2,
            },
        ];
        let results = rrf_merge(&[&vec_list, &fts_list], 60);

        assert_eq!(results[0].0, "b");
        let expected_b = 1.0 / 62.0 + 1.0 / 61.0;
        assert!((results[0].1 - expected_b).abs() < 1e-10);
    }

    #[test]
    fn rrf_empty_lists() {
        let results = rrf_merge(&[], 60);
        assert!(results.is_empty());

        let empty: Vec<RankedItem> = vec![];
        let results = rrf_merge(&[&empty], 60);
        assert!(results.is_empty());
    }

    #[test]
    fn rrf_equal_ranks_equal_scores() {
        let list = vec![
            RankedItem {
                id: "a".into(),
                rank: 1,
            },
            RankedItem {
                id: "b".into(),
                rank: 1,
            },
        ];
        let results = rrf_merge(&[&list], 60);
        assert_eq!(results.len(), 2);
        assert!((results[0].1 - results[1].1).abs() < 1e-10);
    }

    #[test]
    fn rrf_k_affects_score_spread() {
        let list = vec![
            RankedItem {
                id: "a".into(),
                rank: 1,
            },
            RankedItem {
                id: "b".into(),
                rank: 2,
            },
        ];

        let k1 = rrf_merge(&[&list], 1);
        let k60 = rrf_merge(&[&list], 60);

        // k=1: gap = 1/2 - 1/3 ≈ 0.167
        // k=60: gap = 1/61 - 1/62 ≈ 0.00027
        let gap_k1 = k1[0].1 - k1[1].1;
        let gap_k60 = k60[0].1 - k60[1].1;
        assert!(gap_k1 > gap_k60);
    }

    #[test]
    fn rrf_three_lists() {
        let l1 = vec![RankedItem {
            id: "a".into(),
            rank: 1,
        }];
        let l2 = vec![RankedItem {
            id: "a".into(),
            rank: 1,
        }];
        let l3 = vec![RankedItem {
            id: "a".into(),
            rank: 1,
        }];
        let results = rrf_merge(&[&l1, &l2, &l3], 60);

        assert_eq!(results.len(), 1);
        assert!((results[0].1 - 3.0 / 61.0).abs() < 1e-10);
    }
}
