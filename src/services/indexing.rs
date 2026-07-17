//! Index use cases: build/rebuild trees via the configured builder, load the
//! current index (building lazily when missing), and serve tree/node reads.

use crate::config::Config;
use crate::domain::{Document, ErrorCode, HdsError, HdsResult, IndexStatus, TreeIndex, TreeNode};
use crate::index::{BuildInput, RebuildStats};
use crate::infra::db::IndexRecord;
use crate::services::Workspace;
use chrono::Utc;

pub struct IndexService<'a> {
    ws: &'a Workspace,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RebuildOutcome {
    pub index_version: String,
    pub builder: String,
    pub builder_version: String,
    pub diagnostics: Vec<String>,
    pub reused_nodes: usize,
    pub rebuilt_nodes: usize,
    pub node_count: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeReadOutcome {
    pub node: TreeNode,
    pub node_path: Vec<String>,
    pub content: String,
    pub index_version: String,
    pub revision_id: String,
}

impl<'a> IndexService<'a> {
    pub fn new(ws: &'a Workspace) -> Self {
        IndexService { ws }
    }

    fn builder_config_hash(&self) -> String {
        let cfg = serde_json::to_value(&self.ws.config.tree).unwrap_or_default();
        Config::options_hash(&cfg)
    }

    /// Build (or rebuild) the index for a document. `content` may be passed
    /// to avoid a re-read when the caller just wrote the file.
    pub fn rebuild(&self, doc: &Document, content: Option<&str>) -> HdsResult<RebuildOutcome> {
        self.rebuild_with(doc, content, None, false)
    }

    pub fn rebuild_with(
        &self,
        doc: &Document,
        content: Option<&str>,
        builder_override: Option<&str>,
        force: bool,
    ) -> HdsResult<RebuildOutcome> {
        let builder_name = builder_override.unwrap_or(&self.ws.config.tree.builder);
        let builder = self.ws.builders.get(builder_name)?;
        let owned;
        let content = match content {
            Some(c) => c,
            None => {
                owned = self
                    .ws
                    .files
                    .read_document(&doc.logical_path, self.ws.config.security.follow_symlinks)?;
                &owned
            }
        };
        let config_hash = self.builder_config_hash();

        // Skip when an index for this exact revision/builder/config exists.
        let previous = self.ws.db.current_index(&doc.document_id)?;
        if !force
            && let Some(prev) = &previous
            && prev.revision_id == doc.current_revision
            && prev.builder == builder.name()
            && prev.builder_version == builder.version()
            && prev.config_hash == config_hash
            && let Ok(existing) = self
                .ws
                .files
                .read_index(&doc.document_id, &prev.index_version)
        {
            self.ws
                .db
                .set_index_status(&doc.document_id, IndexStatus::Ready)?;
            return Ok(RebuildOutcome {
                index_version: prev.index_version.clone(),
                builder: prev.builder.clone(),
                builder_version: prev.builder_version.clone(),
                diagnostics: vec!["index already current; skipped".to_string()],
                reused_nodes: existing.nodes.len(),
                rebuilt_nodes: 0,
                node_count: existing.nodes.len(),
            });
        }

        self.ws
            .db
            .set_index_status(&doc.document_id, IndexStatus::Building)?;
        let index_version = ulid::Ulid::generate().to_string();
        let title_fallback = doc.title.clone();
        let input = BuildInput {
            document_id: &doc.document_id,
            revision_id: &doc.current_revision,
            title_fallback: &title_fallback,
            content,
            index_version: &index_version,
            config_hash: &config_hash,
        };

        let build_result: HdsResult<(TreeIndex, RebuildStats)> = {
            let prev_index = previous.as_ref().and_then(|p| {
                self.ws
                    .files
                    .read_index(&doc.document_id, &p.index_version)
                    .ok()
            });
            match prev_index {
                Some(prev) => builder.rebuild(&input, &prev, &self.ws.config.tree),
                None => builder
                    .build(&input, &self.ws.config.tree)
                    .map(|t| (t, RebuildStats::default())),
            }
        };

        let (index, stats) = match build_result {
            Ok(v) => v,
            Err(e) => {
                self.ws
                    .db
                    .set_index_status(&doc.document_id, IndexStatus::Failed)?;
                return Err(HdsError::new(
                    ErrorCode::IndexFailed,
                    format!("tree build failed: {}", e.message),
                ));
            }
        };

        self.ws.files.write_index(&index)?;
        self.ws.db.record_index(&IndexRecord {
            document_id: doc.document_id.clone(),
            index_version: index_version.clone(),
            builder: builder.name().to_string(),
            builder_version: builder.version().to_string(),
            config_hash: config_hash.clone(),
            revision_id: doc.current_revision.clone(),
            created_at: Utc::now(),
            current: true,
        })?;
        self.ws
            .db
            .set_index_status(&doc.document_id, IndexStatus::Ready)?;

        Ok(RebuildOutcome {
            index_version,
            builder: builder.name().to_string(),
            builder_version: builder.version().to_string(),
            diagnostics: index.diagnostics.clone(),
            reused_nodes: stats.reused_nodes,
            rebuilt_nodes: stats.rebuilt_nodes,
            node_count: index.nodes.len(),
        })
    }

    /// Load the current index, building it lazily when missing or stale.
    /// Returns the index and whether it lags the current revision.
    pub fn current_index(&self, doc: &Document) -> HdsResult<(TreeIndex, bool)> {
        let record = self.ws.db.current_index(&doc.document_id)?;
        if let Some(rec) = record
            && let Ok(index) = self
                .ws
                .files
                .read_index(&doc.document_id, &rec.index_version)
        {
            let stale = rec.revision_id != doc.current_revision;
            if !stale {
                return Ok((index, false));
            }
            // Stale: try to rebuild; fall back to the stale index.
            match self.rebuild(doc, None) {
                Ok(outcome) => {
                    let fresh = self
                        .ws
                        .files
                        .read_index(&doc.document_id, &outcome.index_version)?;
                    return Ok((fresh, false));
                }
                Err(_) => return Ok((index, true)),
            }
        }
        // No usable index yet: build one.
        let outcome = self.rebuild(doc, None)?;
        let index = self
            .ws
            .files
            .read_index(&doc.document_id, &outcome.index_version)?;
        Ok((index, false))
    }

    /// Tree for a document, optionally depth-limited.
    pub fn tree(
        &self,
        doc: &Document,
        depth: Option<usize>,
        include_summaries: bool,
    ) -> HdsResult<(TreeIndex, serde_json::Value, bool)> {
        let (index, stale) = self.current_index(doc)?;
        let rendered = render_subtree(&index, &index.root_id, depth, include_summaries);
        Ok((index, rendered, stale))
    }

    pub fn node(
        &self,
        doc: &Document,
        node_id: &str,
        context: bool,
    ) -> HdsResult<(NodeReadOutcome, bool)> {
        let (index, stale) = self.current_index(doc)?;
        let node = index
            .node(node_id)
            .ok_or_else(|| HdsError::not_found(format!("node {node_id}")))?
            .clone();
        let content = self
            .ws
            .files
            .read_document(&doc.logical_path, self.ws.config.security.follow_symlinks)?;
        let start = node.source.start_byte.min(content.len());
        let end = node.source.end_byte.min(content.len()).max(start);
        let mut text = content[slice_bounds(&content, start, end)].to_string();
        if context
            && let Some(parent_id) = &node.parent_id
            && let Some(parent) = index.node(parent_id)
        {
            text = format!(
                "<!-- parent: {} -->\n{}",
                parent.title.replace("-->", "--"),
                text
            );
        }
        Ok((
            NodeReadOutcome {
                node_path: index.node_path(node_id),
                node,
                content: text,
                index_version: index.index_version.clone(),
                revision_id: index.revision_id.clone(),
            },
            stale,
        ))
    }
}

fn slice_bounds(content: &str, mut start: usize, mut end: usize) -> std::ops::Range<usize> {
    while start < content.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    while end > start && !content.is_char_boundary(end) {
        end -= 1;
    }
    start..end
}

/// Render a nested JSON view of the tree (node objects with `children`
/// inlined), depth-limited when requested.
pub fn render_subtree(
    index: &TreeIndex,
    node_id: &str,
    depth: Option<usize>,
    include_summaries: bool,
) -> serde_json::Value {
    let Some(node) = index.node(node_id) else {
        return serde_json::Value::Null;
    };
    let mut obj = serde_json::json!({
        "node_id": node.node_id,
        "kind": node.kind,
        "level": node.level,
        "title": node.title,
        "start_line": node.source.start_line,
        "end_line": node.source.end_line,
        "word_count": node.attributes.word_count,
    });
    if include_summaries {
        obj["summary"] = serde_json::to_value(&node.summary).unwrap_or_default();
    }
    let recurse = match depth {
        Some(0) => false,
        Some(_) | None => true,
    };
    if recurse {
        let next_depth = depth.map(|d| d - 1);
        let children: Vec<serde_json::Value> = node
            .children
            .iter()
            .map(|c| render_subtree(index, c, next_depth, include_summaries))
            .collect();
        obj["children"] = serde_json::Value::Array(children);
    } else {
        obj["children_truncated"] = serde_json::json!(node.children.len());
    }
    obj
}
