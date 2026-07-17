//! `beam_tree_v1`: deterministic top-down beam search over document trees.
//!
//! Per document: score the root's children, keep the best `beam_width`,
//! expand those above `expand_threshold`, re-score children (their scores
//! already include ancestry via the path signal), and stop at `max_depth`,
//! the node-visit budget, or when no frontier node clears the threshold.

use crate::config::BeamTreeOptions;
use crate::domain::{ErrorCode, HdsError, HdsResult, TraceStep};
use crate::search::{
    Candidate, IndexedDocument, NodeScorer, QueryTerms, SearchStrategy, StrategyContext,
    StrategyOutcome, node_content,
};

pub struct BeamTreeV1;

impl SearchStrategy for BeamTreeV1 {
    fn name(&self) -> &'static str {
        "beam_tree_v1"
    }

    fn version(&self) -> &'static str {
        "1.0.0"
    }

    fn describe(&self) -> String {
        "Deterministic top-down beam search over the tree index with weighted \
         lexical signals (beam_width, max_depth, max_nodes_visited, expand_threshold)."
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
        let opts: BeamTreeOptions = serde_json::from_value(ctx.options.clone()).map_err(|e| {
            HdsError::new(
                ErrorCode::InvalidArgument,
                format!("invalid beam_tree_v1 options: {e}"),
            )
        })?;
        let budget = opts.max_nodes_visited.min(ctx.max_nodes_visited);

        let mut outcome = StrategyOutcome::default();
        let mut step = 0usize;

        for (doc_index, doc) in docs.iter().enumerate() {
            let index = doc.index;
            // Depth 0 frontier: children of the document root.
            let mut frontier: Vec<String> = index.root().children.clone();
            if frontier.is_empty() {
                // Degenerate tree: only a root. Score it so the document is
                // still retrievable.
                frontier = vec![index.root_id.clone()];
            }
            let mut depth = 0usize;

            while !frontier.is_empty() && depth < opts.max_depth {
                if outcome.nodes_visited >= budget {
                    outcome.warnings.push(format!(
                        "node visit budget ({budget}) exhausted; results may be truncated"
                    ));
                    break;
                }
                // Score the frontier.
                let mut scored: Vec<(String, crate::domain::ScoreBreakdown)> = Vec::new();
                for node_id in &frontier {
                    if outcome.nodes_visited >= budget {
                        outcome.warnings.push(format!(
                            "node visit budget ({budget}) exhausted; results may be truncated"
                        ));
                        break;
                    }
                    let Some(node) = index.node(node_id) else {
                        continue;
                    };
                    let breakdown =
                        scorer.score(query, node, node_content(doc, node), &ctx.weights);
                    outcome.nodes_visited += 1;
                    trace.push(TraceStep {
                        step,
                        node_id: node_id.clone(),
                        depth,
                        action: "score".to_string(),
                        score: breakdown.clone(),
                        note: None,
                    });
                    step += 1;
                    scored.push((node_id.clone(), breakdown));
                }
                // Deterministic ordering: score desc, node id asc.
                scored.sort_by(|a, b| {
                    b.1.total
                        .partial_cmp(&a.1.total)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.0.cmp(&b.0))
                });

                // Everything scored becomes a candidate result.
                for (node_id, breakdown) in &scored {
                    outcome.candidates.push(Candidate {
                        doc_index,
                        node_id: node_id.clone(),
                        score: breakdown.clone(),
                    });
                }

                // Keep the beam and expand nodes above the threshold.
                let beam = &scored[..scored.len().min(opts.beam_width)];
                let mut next: Vec<String> = Vec::new();
                for (node_id, breakdown) in beam {
                    if breakdown.total >= opts.expand_threshold {
                        if let Some(node) = index.node(node_id) {
                            trace.push(TraceStep {
                                step,
                                node_id: node_id.clone(),
                                depth,
                                action: "expand".to_string(),
                                score: breakdown.clone(),
                                note: Some(format!("{} children", node.children.len())),
                            });
                            step += 1;
                            next.extend(node.children.iter().cloned());
                        }
                    } else {
                        trace.push(TraceStep {
                            step,
                            node_id: node_id.clone(),
                            depth,
                            action: "prune".to_string(),
                            score: breakdown.clone(),
                            note: Some(format!("below expand_threshold {}", opts.expand_threshold)),
                        });
                        step += 1;
                    }
                }
                frontier = next;
                depth += 1;
            }
        }

        Ok(outcome)
    }
}
