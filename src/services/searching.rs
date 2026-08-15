//! Search use case: resolve the corpus, load indexes, run the selected
//! strategy, rank + deduplicate results, and persist a reproducible run.

use crate::config::Config;
use crate::domain::{
    Actor, Document, ErrorCode, HdsError, HdsResult, SearchHit, SearchResult, SearchSource,
    TreeIndex,
};
use crate::search::{
    IndexedDocument, QueryTerms, StrategyContext, dedupe_overlaps, excerpt, node_content,
    sort_candidates,
};
use crate::services::{DocSelector, DocumentService, IndexService, Workspace};
use chrono::Utc;
use std::time::Instant;

pub struct SearchService<'a> {
    ws: &'a Workspace,
    actor: Actor,
    interface: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub query: String,
    pub document_ids: Vec<String>,
    pub path_prefix: Option<String>,
    pub strategy: Option<String>,
    pub options: Option<serde_json::Value>,
    pub top_k: Option<usize>,
    pub include_trace: bool,
}

impl<'a> SearchService<'a> {
    pub fn new(ws: &'a Workspace, actor: Actor, interface: &'static str) -> Self {
        SearchService {
            ws,
            actor,
            interface,
        }
    }

    pub fn search(&self, req: &SearchRequest) -> HdsResult<SearchResult> {
        let started = Instant::now();
        let result = self.search_inner(req, started);
        let latency = started.elapsed().as_millis() as u64;
        let (status, error_code) = match &result {
            Ok(_) => ("ok", None),
            Err(e) => ("error", Some(e.code.as_str().to_string())),
        };
        self.ws.record_audit(
            &self.actor,
            self.interface,
            "search_hierarchy",
            serde_json::json!({
                "query_chars": req.query.chars().count(),
                "strategy": req.strategy,
                "document_ids": req.document_ids,
            }),
            status,
            latency,
            None,
            None,
            error_code,
        );
        result
    }

    fn search_inner(&self, req: &SearchRequest, started: Instant) -> HdsResult<SearchResult> {
        if req.query.trim().is_empty() {
            return Err(HdsError::new(ErrorCode::InvalidArgument, "query is empty"));
        }
        if req.query.chars().count() > self.ws.config.limits.max_query_chars {
            return Err(HdsError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "query exceeds {} characters",
                    self.ws.config.limits.max_query_chars
                ),
            ));
        }
        let top_k = req
            .top_k
            .unwrap_or(10)
            .min(self.ws.config.limits.max_top_k)
            .max(1);

        // Strategy selection: request override (if allowed) > config default.
        let strategy_name = match &req.strategy {
            Some(name) => {
                if !self.ws.config.search.allow_request_override {
                    return Err(HdsError::new(
                        ErrorCode::PermissionDenied,
                        "per-request strategy override is disabled by configuration",
                    ));
                }
                name.clone()
            }
            None => self.ws.config.search.default_strategy.clone(),
        };
        let strategy = self.ws.strategies.get(&strategy_name)?;
        if strategy.experimental() && !self.ws.config.search.experimental_strategies_enabled {
            return Err(HdsError::new(
                ErrorCode::PermissionDenied,
                format!("strategy '{strategy_name}' is experimental and disabled"),
            ));
        }

        // Effective options: config block shallow-merged with request options.
        let mut options = self.ws.config.strategy_options(&strategy_name);
        if let Some(overrides) = &req.options {
            options = shallow_merge(options, overrides.clone());
        }
        if options.is_null() {
            options = serde_json::json!({});
        }
        let config_hash = Config::options_hash(&options);

        // Resolve corpus.
        let docs = self.resolve_corpus(req)?;
        if docs.is_empty() {
            return Err(HdsError::not_found("no matching documents"));
        }

        // Load contents and indexes.
        let index_service = IndexService::new(self.ws);
        let mut contents: Vec<String> = Vec::new();
        let mut indexes: Vec<(TreeIndex, bool)> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        for doc in &docs {
            contents.push(
                self.ws
                    .files
                    .read_document(&doc.logical_path, self.ws.config.security.follow_symlinks)?,
            );
            let (index, stale) = index_service.current_index(doc)?;
            if stale {
                warnings.push(format!(
                    "index for '{}' is stale (revision {} indexed, {} current)",
                    doc.logical_path, index.revision_id, doc.current_revision
                ));
            }
            indexes.push((index, stale));
        }
        let indexed: Vec<IndexedDocument<'_>> = docs
            .iter()
            .zip(contents.iter())
            .zip(indexes.iter())
            .map(|((document, content), (index, stale))| IndexedDocument {
                document,
                index,
                content,
                stale: *stale,
            })
            .collect();

        let weights = beam_weights(&options);
        let ctx = StrategyContext {
            options,
            max_nodes_visited: self.ws.config.limits.max_nodes_visited,
            weights,
        };
        let query = QueryTerms::parse(&req.query);
        let mut trace = Vec::new();
        let mut outcome =
            strategy.search(&query, &indexed, &ctx, self.ws.scorer.as_ref(), &mut trace)?;
        warnings.append(&mut outcome.warnings);

        // Rank, deduplicate overlapping ancestors/descendants, take top_k.
        let mut candidates = outcome.candidates;
        // A node can be scored at several depths; keep its best score.
        candidates.sort_by(|a, b| {
            (a.doc_index, &a.node_id)
                .cmp(&(b.doc_index, &b.node_id))
                .then(
                    b.score
                        .total
                        .partial_cmp(&a.score.total)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });
        candidates.dedup_by(|a, b| a.doc_index == b.doc_index && a.node_id == b.node_id);
        sort_candidates(&mut candidates);
        let deduped = dedupe_overlaps(candidates, &indexed);
        let hits: Vec<SearchHit> = deduped
            .into_iter()
            .take(top_k)
            .filter_map(|c| {
                let doc = &indexed[c.doc_index];
                let node = doc.index.node(&c.node_id)?;
                Some(SearchHit {
                    document_id: doc.document.document_id.clone(),
                    logical_path: doc.document.logical_path.clone(),
                    node_id: c.node_id.clone(),
                    node_path: doc.index.node_path(&c.node_id),
                    start_line: node.source.start_line,
                    end_line: node.source.end_line,
                    excerpt: excerpt(node_content(doc, node), 240),
                    score: c.score,
                })
            })
            .collect();

        let sources: Vec<SearchSource> = indexed
            .iter()
            .map(|d| SearchSource {
                document_id: d.document.document_id.clone(),
                revision_id: d.index.revision_id.clone(),
                index_version: d.index.index_version.clone(),
                stale: d.stale,
            })
            .collect();

        let run_id = format!("sr_{}", ulid::Ulid::generate().to_string().to_lowercase());
        let mut result = SearchResult {
            search_run_id: run_id.clone(),
            query: req.query.clone(),
            strategy: strategy.name().to_string(),
            strategy_version: strategy.version().to_string(),
            config_hash: config_hash.clone(),
            sources,
            results: hits,
            trace,
            nodes_visited: outcome.nodes_visited,
            elapsed_ms: started.elapsed().as_millis() as u64,
            warnings,
        };

        // Persist the full run (always with trace) so
        // treefinder://search-run/{id}/trace stays available afterwards.
        self.ws.db.record_search_run(
            &run_id,
            Utc::now(),
            &result.query,
            &result.strategy,
            &result.strategy_version,
            &config_hash,
            &serde_json::to_string(&result)?,
        )?;
        if !req.include_trace {
            result.trace = Vec::new();
        }
        Ok(result)
    }

    fn resolve_corpus(&self, req: &SearchRequest) -> HdsResult<Vec<Document>> {
        let doc_service = DocumentService::new(self.ws, self.actor.clone(), self.interface);
        if !req.document_ids.is_empty() {
            let mut docs = Vec::new();
            for id in &req.document_ids {
                docs.push(doc_service.resolve(&DocSelector::parse(id))?);
            }
            return Ok(docs);
        }
        let mut docs = self.ws.db.all_documents(false)?;
        if let Some(prefix) = &req.path_prefix {
            docs.retain(|d| d.logical_path.starts_with(prefix.trim_start_matches('/')));
        }
        Ok(docs)
    }

    pub fn trace_for_run(&self, run_id: &str) -> HdsResult<serde_json::Value> {
        let json = self
            .ws
            .db
            .search_run(run_id)?
            .ok_or_else(|| HdsError::not_found(format!("search run {run_id}")))?;
        Ok(serde_json::from_str(&json)?)
    }
}

fn shallow_merge(base: serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(mut b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                b.insert(k, v);
            }
            serde_json::Value::Object(b)
        }
        (b, serde_json::Value::Null) => b,
        (_, o) => o,
    }
}

fn beam_weights(options: &serde_json::Value) -> crate::config::ScoreWeights {
    options
        .get("weights")
        .and_then(|w| serde_json::from_value(w.clone()).ok())
        .unwrap_or_default()
}
