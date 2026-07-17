//! Hierarchical search plugins.
//!
//! `SearchStrategy` walks one or more document trees and returns scored
//! nodes; `NodeScorer` produces the per-signal breakdown. Both are looked up
//! by name in registries so `search.default_strategy` (or a per-request
//! override) selects the algorithm without touching adapters or services.

pub mod beam_tree_v1;
pub mod eval;
pub mod exhaustive_tree_v1;
pub mod lexical;

use crate::config::ScoreWeights;
use crate::domain::{
    Document, ErrorCode, HdsError, HdsResult, ScoreBreakdown, TraceStep, TreeIndex, TreeNode,
};
use std::collections::BTreeMap;

/// A document plus its loaded index and current content, ready to search.
pub struct IndexedDocument<'a> {
    pub document: &'a Document,
    pub index: &'a TreeIndex,
    pub content: &'a str,
    pub stale: bool,
}

/// Normalized query: lowercase alphanumeric terms.
#[derive(Debug, Clone)]
pub struct QueryTerms {
    pub terms: Vec<String>,
}

impl QueryTerms {
    pub fn parse(query: &str) -> Self {
        let mut terms: Vec<String> = tokenize(query);
        terms.dedup();
        QueryTerms { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// One scored node produced by a strategy (pre-ranking).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub doc_index: usize,
    pub node_id: String,
    pub score: ScoreBreakdown,
}

#[derive(Debug, Default)]
pub struct StrategyOutcome {
    pub candidates: Vec<Candidate>,
    pub nodes_visited: usize,
    pub warnings: Vec<String>,
}

/// Execution context handed to a strategy: validated options plus hard caps.
pub struct StrategyContext {
    /// Strategy-specific options (already merged config + request overrides).
    pub options: serde_json::Value,
    /// Hard cap from workspace limits; strategies must respect it.
    pub max_nodes_visited: usize,
    pub weights: ScoreWeights,
}

pub trait NodeScorer: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str;
    fn score(
        &self,
        query: &QueryTerms,
        node: &TreeNode,
        node_content: &str,
        weights: &ScoreWeights,
    ) -> ScoreBreakdown;
}

pub trait SearchStrategy: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str;
    fn describe(&self) -> String;
    fn experimental(&self) -> bool {
        false
    }
    fn search(
        &self,
        query: &QueryTerms,
        docs: &[IndexedDocument<'_>],
        ctx: &StrategyContext,
        scorer: &dyn NodeScorer,
        trace: &mut Vec<TraceStep>,
    ) -> HdsResult<StrategyOutcome>;
}

#[derive(Default)]
pub struct StrategyRegistry {
    strategies: BTreeMap<String, Box<dyn SearchStrategy>>,
}

impl StrategyRegistry {
    pub fn with_defaults() -> Self {
        let mut reg = StrategyRegistry::default();
        reg.register(Box::new(beam_tree_v1::BeamTreeV1));
        reg.register(Box::new(exhaustive_tree_v1::ExhaustiveTreeV1));
        reg
    }

    pub fn register(&mut self, strategy: Box<dyn SearchStrategy>) {
        self.strategies
            .insert(strategy.name().to_string(), strategy);
    }

    pub fn get(&self, name: &str) -> HdsResult<&dyn SearchStrategy> {
        self.strategies
            .get(name)
            .map(|s| s.as_ref())
            .ok_or_else(|| {
                HdsError::new(
                    ErrorCode::StrategyNotFound,
                    format!("search strategy '{name}' is not registered"),
                )
                .with_details(serde_json::json!({ "requested": name }))
            })
    }

    pub fn names(&self) -> Vec<String> {
        self.strategies.keys().cloned().collect()
    }
}

/// Slice of document content covered by a node, used for body scoring and
/// excerpts. Robust to spans computed against a different revision.
pub fn node_content<'a>(doc: &IndexedDocument<'a>, node: &TreeNode) -> &'a str {
    let start = node.source.start_byte.min(doc.content.len());
    let end = node.source.end_byte.min(doc.content.len());
    let mut s = start;
    let mut e = end.max(s);
    // Clamp to char boundaries.
    while s < doc.content.len() && !doc.content.is_char_boundary(s) {
        s += 1;
    }
    while e > s && !doc.content.is_char_boundary(e) {
        e -= 1;
    }
    &doc.content[s..e]
}

pub fn excerpt(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

/// Drop candidates whose ancestor or descendant (same document) already ranks
/// higher. Input must be sorted best-first; order is preserved.
pub fn dedupe_overlaps(candidates: Vec<Candidate>, docs: &[IndexedDocument<'_>]) -> Vec<Candidate> {
    let mut kept: Vec<Candidate> = Vec::new();
    'outer: for cand in candidates {
        let index = docs[cand.doc_index].index;
        for prev in kept.iter().filter(|p| p.doc_index == cand.doc_index) {
            if is_ancestor(index, &prev.node_id, &cand.node_id)
                || is_ancestor(index, &cand.node_id, &prev.node_id)
            {
                continue 'outer;
            }
        }
        kept.push(cand);
    }
    kept
}

fn is_ancestor(index: &TreeIndex, ancestor: &str, node: &str) -> bool {
    let mut cur = index.node(node).and_then(|n| n.parent_id.as_deref());
    while let Some(id) = cur {
        if id == ancestor {
            return true;
        }
        cur = index.node(id).and_then(|n| n.parent_id.as_deref());
    }
    false
}

/// Deterministic best-first ordering: score desc, then doc index, then node id.
pub fn sort_candidates(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        b.score
            .total
            .partial_cmp(&a.score.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.doc_index.cmp(&b.doc_index))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
}
