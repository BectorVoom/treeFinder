//! MCP resources: read-only `hds://` URIs for documents, trees, nodes,
//! history, revisions, and search traces. Reads are side-effect free.

use crate::domain::{Actor, ErrorCode, HdsError, HdsResult};
use crate::services::{DocSelector, DocumentService, IndexService, SearchService, Workspace};
use rmcp::model::{ListResourcesResult, Resource, ResourceContents, ResourceTemplate};

pub(crate) fn templates() -> Vec<ResourceTemplate> {
    let t = |uri: &str, name: &str, desc: &str, mime: &str| {
        ResourceTemplate::new(uri, name)
            .with_description(desc)
            .with_mime_type(mime)
    };
    vec![
        t(
            "hds://document/{document_id}",
            "document-descriptor",
            "Document metadata and revision state",
            "application/json",
        ),
        t(
            "hds://document/{document_id}/content",
            "document-content",
            "Current Markdown content",
            "text/markdown",
        ),
        t(
            "hds://document/{document_id}/tree",
            "document-tree",
            "Hierarchical tree index",
            "application/json",
        ),
        t(
            "hds://document/{document_id}/node/{node_id}",
            "document-node",
            "One tree node with content",
            "application/json",
        ),
        t(
            "hds://document/{document_id}/history",
            "document-history",
            "Revision history (newest first)",
            "application/json",
        ),
        t(
            "hds://document/{document_id}/revision/{revision_id}",
            "document-revision",
            "Snapshot of one revision",
            "text/markdown",
        ),
        t(
            "hds://search-run/{search_run_id}/trace",
            "search-trace",
            "Traversal trace of a search run",
            "application/json",
        ),
    ]
}

/// Concrete resources: the corpus listing plus one content resource per
/// document, paginated by opaque cursor (the last logical path).
pub(crate) fn list(ws: &Workspace, cursor: Option<&str>) -> HdsResult<ListResourcesResult> {
    let page_size = 100usize;
    let (docs, next) = ws.db.list_documents(None, cursor, page_size, false)?;

    let mut resources = Vec::new();
    if cursor.is_none() {
        resources.push(
            Resource::new("hds://documents", "documents")
                .with_description("All registered documents with descriptors")
                .with_mime_type("application/json"),
        );
    }
    for d in &docs {
        resources.push(
            Resource::new(
                format!("hds://document/{}/content", d.document_id),
                d.logical_path.clone(),
            )
            .with_title(d.title.clone())
            .with_description(format!("Markdown document at {}", d.logical_path))
            .with_mime_type("text/markdown"),
        );
    }
    let mut result = ListResourcesResult::with_all_items(resources);
    result.next_cursor = next;
    Ok(result)
}

pub(crate) fn read(ws: &Workspace, actor: &Actor, uri: &str) -> HdsResult<ResourceContents> {
    let docs = DocumentService::new(ws, actor.clone(), "mcp");
    let text = |mime: &str, body: String| ResourceContents::TextResourceContents {
        uri: uri.to_string(),
        mime_type: Some(mime.to_string()),
        text: body,
        meta: None,
    };

    let rest = uri
        .strip_prefix("hds://")
        .ok_or_else(|| HdsError::new(ErrorCode::InvalidPath, format!("unsupported URI '{uri}'")))?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["documents"] => {
            let list = ws.db.all_documents(false)?;
            Ok(text(
                "application/json",
                serde_json::to_string_pretty(&list)?,
            ))
        }
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
