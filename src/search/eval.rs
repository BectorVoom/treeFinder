//! Retrieval evaluation runner: replays a labeled query set against one or
//! more strategies and reports ranking metrics. The runner pins whatever the
//! search layer reports (revisions, index versions, config hashes) into the
//! report for reproducibility.

use crate::domain::{ErrorCode, HdsError, HdsResult, SearchResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalCase {
    pub query: String,
    #[serde(default)]
    pub document_ids: Vec<String>,
    pub relevant_node_ids: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

pub fn load_dataset(text: &str) -> HdsResult<Vec<EvalCase>> {
    let mut cases = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let case: EvalCase = serde_json::from_str(line).map_err(|e| {
            HdsError::new(
                ErrorCode::InvalidArgument,
                format!("dataset line {}: {e}", lineno + 1),
            )
        })?;
        cases.push(case);
    }
    Ok(cases)
}

#[derive(Debug, Clone, Serialize)]
pub struct StrategyReport {
    pub strategy: String,
    pub strategy_version: String,
    pub config_hash: String,
    pub k: usize,
    pub cases: usize,
    pub failures: usize,
    pub recall_at_k: f64,
    pub precision_at_k: f64,
    pub mean_reciprocal_rank: f64,
    pub ndcg_at_k: f64,
    pub mean_nodes_visited: f64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    /// Pinned (document_id, revision_id, index_version) triples seen.
    pub pinned_sources: Vec<serde_json::Value>,
}

/// Evaluate one strategy. `run` executes a search and returns the full
/// result; it is injected so the runner stays decoupled from services.
pub fn evaluate_strategy(
    strategy: &str,
    cases: &[EvalCase],
    k: usize,
    mut run: impl FnMut(&EvalCase) -> HdsResult<SearchResult>,
) -> StrategyReport {
    let mut recalls = Vec::new();
    let mut precisions = Vec::new();
    let mut rrs = Vec::new();
    let mut ndcgs = Vec::new();
    let mut visits = Vec::new();
    let mut latencies = Vec::new();
    let mut failures = 0usize;
    let mut version = String::new();
    let mut config_hash = String::new();
    let mut pinned: Vec<serde_json::Value> = Vec::new();

    for case in cases {
        match run(case) {
            Ok(result) => {
                version = result.strategy_version.clone();
                config_hash = result.config_hash.clone();
                for s in &result.sources {
                    let v = serde_json::json!({
                        "document_id": s.document_id,
                        "revision_id": s.revision_id,
                        "index_version": s.index_version,
                    });
                    if !pinned.contains(&v) {
                        pinned.push(v);
                    }
                }
                let ranked: Vec<&str> = result
                    .results
                    .iter()
                    .take(k)
                    .map(|h| h.node_id.as_str())
                    .collect();
                let relevant: std::collections::HashSet<&str> =
                    case.relevant_node_ids.iter().map(|s| s.as_str()).collect();
                let hits = ranked.iter().filter(|id| relevant.contains(**id)).count();
                recalls.push(if relevant.is_empty() {
                    0.0
                } else {
                    hits as f64 / relevant.len() as f64
                });
                precisions.push(if ranked.is_empty() {
                    0.0
                } else {
                    hits as f64 / ranked.len() as f64
                });
                let rr = ranked
                    .iter()
                    .position(|id| relevant.contains(*id))
                    .map(|p| 1.0 / (p as f64 + 1.0))
                    .unwrap_or(0.0);
                rrs.push(rr);
                ndcgs.push(ndcg(&ranked, &relevant, k));
                visits.push(result.nodes_visited as f64);
                latencies.push(result.elapsed_ms);
            }
            Err(_) => failures += 1,
        }
    }

    latencies.sort_unstable();
    StrategyReport {
        strategy: strategy.to_string(),
        strategy_version: version,
        config_hash,
        k,
        cases: cases.len(),
        failures,
        recall_at_k: mean(&recalls),
        precision_at_k: mean(&precisions),
        mean_reciprocal_rank: mean(&rrs),
        ndcg_at_k: mean(&ndcgs),
        mean_nodes_visited: mean(&visits),
        latency_p50_ms: percentile(&latencies, 50),
        latency_p95_ms: percentile(&latencies, 95),
        pinned_sources: pinned,
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() * p).div_ceil(100).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

fn ndcg(ranked: &[&str], relevant: &std::collections::HashSet<&str>, k: usize) -> f64 {
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, id)| {
            if relevant.contains(*id) {
                1.0 / ((i as f64 + 2.0).log2())
            } else {
                0.0
            }
        })
        .sum();
    let ideal: f64 = (0..relevant.len().min(k))
        .map(|i| 1.0 / ((i as f64 + 2.0).log2()))
        .sum();
    if ideal == 0.0 { 0.0 } else { dcg / ideal }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_parses_and_skips_comments() {
        let text = r#"
# comment
{"query":"rollback procedure","document_ids":["d1"],"relevant_node_ids":["n1"],"tags":["ops"]}

{"query":"other","relevant_node_ids":["n2"]}
"#;
        let cases = load_dataset(text).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].tags, vec!["ops"]);
        let err = load_dataset("{broken").unwrap_err();
        assert_eq!(err.code, crate::domain::ErrorCode::InvalidArgument);
    }

    #[test]
    fn metrics_perfect_and_miss() {
        use crate::domain::*;
        let mk_result = |node_ids: Vec<&str>| SearchResult {
            search_run_id: "sr_x".into(),
            query: "q".into(),
            strategy: "s".into(),
            strategy_version: "1".into(),
            config_hash: "h".into(),
            sources: vec![],
            results: node_ids
                .into_iter()
                .map(|id| SearchHit {
                    document_id: "d".into(),
                    logical_path: "p.md".into(),
                    node_id: id.into(),
                    node_path: vec![],
                    start_line: 1,
                    end_line: 1,
                    excerpt: String::new(),
                    score: ScoreBreakdown::default(),
                })
                .collect(),
            trace: vec![],
            nodes_visited: 5,
            elapsed_ms: 1,
            warnings: vec![],
        };
        let cases = vec![EvalCase {
            query: "q".into(),
            document_ids: vec![],
            relevant_node_ids: vec!["n1".into()],
            tags: vec![],
        }];
        let hit = evaluate_strategy("s", &cases, 10, |_| Ok(mk_result(vec!["n1", "n2"])));
        assert_eq!(hit.recall_at_k, 1.0);
        assert_eq!(hit.mean_reciprocal_rank, 1.0);
        assert_eq!(hit.ndcg_at_k, 1.0);
        assert_eq!(hit.failures, 0);

        let second = evaluate_strategy("s", &cases, 10, |_| Ok(mk_result(vec!["nX", "n1"])));
        assert_eq!(second.mean_reciprocal_rank, 0.5);
        assert!(second.ndcg_at_k < 1.0 && second.ndcg_at_k > 0.0);

        let fail = evaluate_strategy("s", &cases, 10, |_| {
            Err(crate::domain::HdsError::internal("boom"))
        });
        assert_eq!(fail.failures, 1);
    }
}
