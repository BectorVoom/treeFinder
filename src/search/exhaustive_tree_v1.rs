//! `exhaustive_tree_v1`: score every node of every tree (up to the visit
//! budget). Useful as a recall ceiling when evaluating pruning strategies.

use crate::domain::{HdsResult, TraceStep};
use crate::search::{
    Candidate, IndexedDocument, NodeScorer, QueryTerms, SearchStrategy, StrategyContext,
    StrategyOutcome, node_content,
};
use serde::Deserialize;

pub struct ExhaustiveTreeV1;

#[derive(Deserialize)]
#[serde(default)]
struct Options {
    max_nodes_visited: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_nodes_visited: 5000,
        }
    }
}

impl SearchStrategy for ExhaustiveTreeV1 {
    fn name(&self) -> &'static str {
        "exhaustive_tree_v1"
    }

    fn version(&self) -> &'static str {
        "1.0.0"
    }

    fn describe(&self) -> String {
        "Scores every tree node (up to max_nodes_visited); recall ceiling for \
         evaluating pruning strategies."
            .to_string()
    }

    fn search(
        &self,
        query: &QueryTerms,
        docs: &[IndexedDocument<'_>],
        ctx: &StrategyContext,
        scorer: &dyn NodeScorer,
        trace: &mut Vec<TraceStep>,
    ) -> HdsResult<StrategyOutcome> {
        let opts: Options = serde_json::from_value(ctx.options.clone()).unwrap_or_default();
        let budget = opts.max_nodes_visited.min(ctx.max_nodes_visited);
        let mut outcome = StrategyOutcome::default();
        let mut step = 0usize;

        'docs: for (doc_index, doc) in docs.iter().enumerate() {
            // BTreeMap iteration gives deterministic node order.
            for (node_id, node) in &doc.index.nodes {
                if outcome.nodes_visited >= budget {
                    outcome.warnings.push(format!(
                        "node visit budget ({budget}) exhausted; results may be truncated"
                    ));
                    break 'docs;
                }
                let breakdown = scorer.score(query, node, node_content(doc, node), &ctx.weights);
                outcome.nodes_visited += 1;
                trace.push(TraceStep {
                    step,
                    node_id: node_id.clone(),
                    depth: node.level,
                    action: "score".to_string(),
                    score: breakdown.clone(),
                    note: None,
                });
                step += 1;
                outcome.candidates.push(Candidate {
                    doc_index,
                    node_id: node_id.clone(),
                    score: breakdown,
                });
            }
        }
        Ok(outcome)
    }
}
