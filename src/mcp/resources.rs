//! MCP resources: read-only `treefinder://` URIs for documents, trees, nodes,
//! history, revisions, and search traces. Reads are side-effect free.

use crate::domain::{Actor, ErrorCode, HdsError, HdsResult};
use crate::services::{
    DocSelector, DocumentService, IndexService, SearchService, Workspace, WorkspaceRegistry,
};
use rmcp::model::{ListResourcesResult, Resource, ResourceContents, ResourceTemplate};
use serde_json::json;

pub(crate) fn templates() -> Vec<ResourceTemplate> {
    let t = |uri: &str, name: &str, desc: &str, mime: &str| {
        ResourceTemplate::new(uri, name)
            .with_description(desc)
            .with_mime_type(mime)
    };
    vec![
        t(
            "treefinder://document/{document_id}",
            "document-descriptor",
            "Document metadata and revision state",
            "application/json",
        ),
        t(
            "treefinder://document/{document_id}/content",
            "document-content",
            "Current Markdown content",
            "text/markdown",
        ),
        t(
            "treefinder://document/{document_id}/tree",
            "document-tree",
            "Hierarchical tree index",
            "application/json",
        ),
        t(
            "treefinder://document/{document_id}/node/{node_id}",
            "document-node",
            "One tree node with content",
            "application/json",
        ),
        t(
            "treefinder://document/{document_id}/history",
            "document-history",
            "Revision history (newest first)",
            "application/json",
        ),
        t(
            "treefinder://document/{document_id}/revision/{revision_id}",
            "document-revision",
            "Snapshot of one revision",
            "text/markdown",
        ),
        t(
            "treefinder://search-run/{search_run_id}/trace",
            "search-trace",
            "Traversal trace of a search run",
            "application/json",
        ),
    ]
}

/// Concrete resources: the corpus listing plus one content resource per
/// document across every open workspace. Pagination walks the workspaces in
/// deterministic (sorted-root) order; the opaque cursor records the workspace
/// being paged and the last logical path within it.
pub(crate) fn list(
    registry: &WorkspaceRegistry,
    cursor: Option<&str>,
) -> HdsResult<ListResourcesResult> {
    let page_size = 100usize;
    let roots = registry.roots();
    let invalid = || HdsError::new(ErrorCode::InvalidArgument, "invalid resource cursor");
    let (idx, inner) = match cursor {
        None => (0usize, None),
        Some(c) => {
            let v: serde_json::Value = serde_json::from_str(c).map_err(|_| invalid())?;
            let w = v["workspace"].as_str().ok_or_else(invalid)?;
            let idx = roots
                .iter()
                .position(|r| r.to_string_lossy() == w)
                .ok_or_else(invalid)?;
            (idx, v["cursor"].as_str().map(String::from))
        }
    };

    let mut resources = Vec::new();
    if cursor.is_none() {
        resources.push(
            Resource::new("treefinder://documents", "documents")
                .with_description("All registered documents with descriptors")
                .with_mime_type("application/json"),
        );
    }
    let mut next_cursor = None;
    if let Some(root) = roots.get(idx) {
        let ws = registry.get(root).expect("listed roots are open");
        let multi = roots.len() > 1;
        let (docs, next) = ws
            .db
            .list_documents(None, inner.as_deref(), page_size, false)?;
        for d in &docs {
            let description = if multi {
                format!(
                    "Markdown document at {} in workspace {}",
                    d.logical_path,
                    root.display()
                )
            } else {
                format!("Markdown document at {}", d.logical_path)
            };
            resources.push(
                Resource::new(
                    format!("treefinder://document/{}/content", d.document_id),
                    d.logical_path.clone(),
                )
                .with_title(d.title.clone())
                .with_description(description)
                .with_mime_type("text/markdown"),
            );
        }
        next_cursor = match next {
            Some(n) => {
                Some(json!({ "workspace": root.to_string_lossy(), "cursor": n }).to_string())
            }
            None => roots
                .get(idx + 1)
                .map(|r| json!({ "workspace": r.to_string_lossy(), "cursor": null }).to_string()),
        };
    }
    let mut result = ListResourcesResult::with_all_items(resources);
    result.next_cursor = next_cursor;
    Ok(result)
}

/// Resolve a `treefinder://` URI against the open workspaces. Document and
/// search-run identifiers are UUIDs, so the first workspace that recognizes
/// one wins; the aggregate `treefinder://documents` listing spans all workspaces.
pub(crate) fn read(
    registry: &WorkspaceRegistry,
    actor: &Actor,
    uri: &str,
) -> HdsResult<ResourceContents> {
    let rest = uri
        .strip_prefix("treefinder://")
        .ok_or_else(|| HdsError::new(ErrorCode::InvalidPath, format!("unsupported URI '{uri}'")))?;
    if rest == "documents" {
        let mut all = Vec::new();
        for (_root, ws) in registry.iter() {
            all.extend(ws.db.all_documents(false)?);
        }
        return Ok(ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: Some("application/json".to_string()),
            text: serde_json::to_string_pretty(&all)?,
            meta: None,
        });
    }
    let mut last_err = HdsError::not_found(format!("resource {uri}"));
    for (_root, ws) in registry.iter() {
        match read_in(ws, actor, uri) {
            Ok(contents) => return Ok(contents),
            Err(e) if e.code == ErrorCode::NotFound => last_err = e,
            Err(e) => return Err(e),
        }
    }
    Err(last_err)
}

fn read_in(ws: &Workspace, actor: &Actor, uri: &str) -> HdsResult<ResourceContents> {
    let docs = DocumentService::new(ws, actor.clone(), "mcp");
    let text = |mime: &str, body: String| ResourceContents::TextResourceContents {
        uri: uri.to_string(),
        mime_type: Some(mime.to_string()),
        text: body,
        meta: None,
    };

    let rest = uri
        .strip_prefix("treefinder://")
        .ok_or_else(|| HdsError::new(ErrorCode::InvalidPath, format!("unsupported URI '{uri}'")))?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["document", id] => {
            let doc = docs.resolve(&DocSelector::Id(id.to_string()))?;
            Ok(text(
                "application/json",
                serde_json::to_string_pretty(&doc)?,
            ))
        }
        ["document", id, "content"] => {
            let out = docs.read(
                &DocSelector::Id(id.to_string()),
                None,
                crate::services::documents::ReadRange::Full,
            )?;
            Ok(text("text/markdown", out.content))
        }
        ["document", id, "tree"] => {
            let doc = docs.resolve(&DocSelector::Id(id.to_string()))?;
            let tree = crate::mcp::tools::render_tree_for(ws, &doc)?;
            Ok(text(
                "application/json",
                serde_json::to_string_pretty(&tree)?,
            ))
        }
        ["document", id, "node", node_id] => {
            let doc = docs.resolve(&DocSelector::Id(id.to_string()))?;
            let (out, _stale) = IndexService::new(ws).node(&doc, node_id, false)?;
            Ok(text(
                "application/json",
                serde_json::to_string_pretty(&out)?,
            ))
        }
        ["document", id, "history"] => {
            let (revisions, _next) =
                docs.history(&DocSelector::Id(id.to_string()), None, Some(100))?;
            Ok(text(
                "application/json",
                serde_json::to_string_pretty(&revisions)?,
            ))
        }
        ["document", id, "revision", revision_id] => {
            let out = docs.read(
                &DocSelector::Id(id.to_string()),
                Some(revision_id),
                crate::services::documents::ReadRange::Full,
            )?;
            Ok(text("text/markdown", out.content))
        }
        ["search-run", run_id, "trace"] => {
            let search = SearchService::new(ws, actor.clone(), "mcp");
            let run = search.trace_for_run(run_id)?;
            Ok(text(
                "application/json",
                serde_json::to_string_pretty(&run)?,
            ))
        }
        _ => Err(HdsError::not_found(format!("resource {uri}"))),
    }
}
