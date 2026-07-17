//! Baseline lexical scorer: normalized term-overlap signals, no network, no
//! LLM. Every signal is reported separately in the `ScoreBreakdown`.

use crate::config::ScoreWeights;
use crate::domain::{ScoreBreakdown, TreeNode};
use crate::search::{NodeScorer, QueryTerms, tokenize};
use std::collections::HashMap;

pub struct LexicalScorer;

impl NodeScorer for LexicalScorer {
    fn name(&self) -> &'static str {
        "lexical_overlap_v1"
    }

    fn version(&self) -> &'static str {
        "1.0.0"
    }

    fn score(
        &self,
        query: &QueryTerms,
        node: &TreeNode,
        node_content: &str,
        weights: &ScoreWeights,
    ) -> ScoreBreakdown {
        let title = overlap_score(query, &tokenize(&node.title));
        let body = frequency_score(query, node_content);
        let summary = node
            .summary
            .as_deref()
            .map(|s| overlap_score(query, &tokenize(s)))
            .unwrap_or(0.0);
        let path_tokens: Vec<String> = node
            .attributes
            .heading_path
            .iter()
            .flat_map(|p| tokenize(p))
            .collect();
        let path = overlap_score(query, &path_tokens);
        let prior = structural_prior(node);

        let mut breakdown = ScoreBreakdown {
            title,
            body,
            summary,
            path,
            prior,
            plugin: 0.0,
            total: 0.0,
        };
        breakdown.total = weights.title * title
            + weights.body * body
            + weights.summary * summary
            + weights.path * path
            + weights.prior * prior
            + weights.plugin * breakdown.plugin;
        breakdown
    }
}

/// Fraction of distinct query terms present in `tokens` (0..=1).
fn overlap_score(query: &QueryTerms, tokens: &[String]) -> f64 {
    if query.terms.is_empty() || tokens.is_empty() {
        return 0.0;
    }
    let set: std::collections::HashSet<&str> = tokens.iter().map(|s| s.as_str()).collect();
    let hits = query
        .terms
        .iter()
        .filter(|t| set.contains(t.as_str()))
        .count();
    hits as f64 / query.terms.len() as f64
}

/// Saturating term-frequency score over the node body (0..=1):
/// mean over query terms of tf/(tf+1), which rewards presence strongly and
/// repetition only mildly, keeping long sections from dominating.
fn frequency_score(query: &QueryTerms, content: &str) -> f64 {
    if query.terms.is_empty() || content.is_empty() {
        return 0.0;
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for token in tokenize(content) {
        *counts.entry(token).or_insert(0) += 1;
    }
    let sum: f64 = query
        .terms
        .iter()
        .map(|t| {
            let tf = *counts.get(t).unwrap_or(&0) as f64;
            tf / (tf + 1.0)
        })
        .sum();
    sum / query.terms.len() as f64
}

/// Mild preference for shallower, heading-backed structure (0..=1).
fn structural_prior(node: &TreeNode) -> f64 {
    let depth_prior = 1.0 / (1.0 + node.level as f64);
    match node.kind {
        crate::domain::NodeKind::Document => 0.0, // roots match everything; don't boost
        crate::domain::NodeKind::SyntheticGroup | crate::domain::NodeKind::ParagraphRange => {
            depth_prior * 0.5
        }
        _ => depth_prior,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NodeAttributes, NodeKind, SourceSpan};

    fn node(title: &str, summary: Option<&str>, path: &[&str], level: usize) -> TreeNode {
        TreeNode {
            node_id: "n_test".into(),
            parent_id: None,
            kind: NodeKind::Section,
            level,
            title: title.into(),
            summary: summary.map(String::from),
            source: SourceSpan {
                start_line: 1,
                end_line: 1,
                start_byte: 0,
                end_byte: 0,
            },
            children: vec![],
            attributes: NodeAttributes {
                heading_path: path.iter().map(|s| s.to_string()).collect(),
                word_count: 0,
                content_hash: String::new(),
            },
        }
    }

    fn score(q: &str, n: &TreeNode, body: &str) -> ScoreBreakdown {
        LexicalScorer.score(&QueryTerms::parse(q), n, body, &ScoreWeights::default())
    }

    #[test]
    fn title_signal_fires_on_title_match() {
        let n = node("Rollback procedure", None, &[], 2);
        let s = score("rollback", &n, "");
        assert!(s.title > 0.9);
        let miss = score("unrelated", &n, "");
        assert_eq!(miss.title, 0.0);
    }

    #[test]
    fn body_signal_saturates_with_frequency() {
        let n = node("X", None, &[], 2);
        let once = score("kafka", &n, "kafka is mentioned");
        let many = score("kafka", &n, &"kafka ".repeat(50));
        assert!(once.body > 0.0);
        assert!(many.body > once.body);
        assert!(many.body <= 1.0);
    }

    #[test]
    fn summary_and_path_signals() {
        let n = node(
            "X",
            Some("about database migrations"),
            &["Operations", "Database"],
            3,
        );
        let s = score("database", &n, "");
        assert!(s.summary > 0.0);
        assert!(s.path > 0.0);
    }

    #[test]
    fn prior_prefers_shallow_sections_and_zeroes_roots() {
        let shallow = node("A", None, &[], 1);
        let deep = node("B", None, &[], 5);
        let s1 = score("q", &shallow, "");
        let s2 = score("q", &deep, "");
        assert!(s1.prior > s2.prior);
        let mut root = node("R", None, &[], 0);
        root.kind = NodeKind::Document;
        assert_eq!(score("q", &root, "").prior, 0.0);
    }

    #[test]
    fn total_is_weighted_sum() {
        let n = node("Rollback", None, &["Ops"], 2);
        let w = ScoreWeights::default();
        let s = score("rollback ops", &n, "rollback details");
        let expected = w.title * s.title
            + w.body * s.body
            + w.summary * s.summary
            + w.path * s.path
            + w.prior * s.prior
            + w.plugin * s.plugin;
        assert!((s.total - expected).abs() < 1e-12);
    }
}
