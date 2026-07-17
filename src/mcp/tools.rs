//! MCP tool definitions and dispatch. Tool handlers parse arguments, call
//! the shared services, and serialize results. Application errors are
//! returned as `isError` tool results carrying the stable wire payload
//! (spec §11.5), so agents can branch on `code`.

use crate::domain::{Actor, ErrorCode, HdsError, HdsResult, PatchFormat};
use crate::mcp::{ChangeSet, ToolFailure};
use crate::services::{
    DocSelector, DocumentService, IndexService, SearchRequest, SearchService, Workspace,
    documents::{IfExists, ReadRange},
    indexing::render_subtree,
};
use rmcp::model::{CallToolRequestParams, Tool};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

struct ToolDef {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

fn tool_defs() -> Vec<ToolDef> {
    let selector = |extra: Value| -> Value {
        let mut base = json!({
            "type": "object",
            "properties": {
                "document_id": { "type": "string", "description": "Document UUID" },
                "path": { "type": "string", "description": "Logical path such as notes/design.md" },
            },
            "additionalProperties": false,
        });
        if let (Some(props), Some(extra_obj)) =
            (base["properties"].as_object_mut(), extra.as_object())
        {
            for (k, v) in extra_obj {
                props.insert(k.clone(), v.clone());
            }
        }
        base
    };
    vec![
        ToolDef {
            name: "document_create",
            description: "Create a Markdown document at a workspace-relative path. Records a revision, an audit event, and builds the tree index.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "metadata": { "type": "object" },
                    "message": { "type": "string" },
                    "if_exists": { "type": "string", "enum": ["error", "overwrite"] },
                },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "document_get",
            description: "Read document content and metadata, optionally at a past revision or restricted to a line/byte range.",
            schema: selector(json!({
                "revision_id": { "type": "string" },
                "range": {
                    "type": "object",
                    "properties": {
                        "lines": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 },
                        "bytes": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 },
                    },
                    "additionalProperties": false,
                },
            })),
        },
        ToolDef {
            name: "document_patch",
            description: "Apply a unified diff against an expected base revision (the primary agent update mechanism). Fails with REVISION_CONFLICT if the document moved.",
            schema: selector(json!({
                "base_revision": { "type": "string" },
                "patch": { "type": "string", "description": "Unified diff" },
                "format": { "type": "string", "enum": ["unified_diff"] },
                "message": { "type": "string" },
            })),
        },
        ToolDef {
            name: "document_replace",
            description: "Replace full document content against an expected base revision. Prefer document_patch for agent edits.",
            schema: selector(json!({
                "base_revision": { "type": "string" },
                "content": { "type": "string" },
                "message": { "type": "string" },
            })),
        },
        ToolDef {
            name: "document_list",
            description: "List registered documents with pagination.",
            schema: json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string" },
                    "cursor": { "type": "string" },
                    "limit": { "type": "integer" },
                    "include_deleted": { "type": "boolean" },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "document_history",
            description: "List revisions of a document, newest first, with pagination.",
            schema: selector(json!({
                "cursor": { "type": "string" },
                "limit": { "type": "integer" },
            })),
        },
        ToolDef {
            name: "document_diff",
            description: "Unified diff between two revisions of a document.",
            schema: selector(json!({
                "from_revision": { "type": "string" },
                "to_revision": { "type": "string" },
            })),
        },
        ToolDef {
            name: "document_restore",
            description: "Restore an earlier revision by creating a new revision with that content. History is never rewritten.",
            schema: selector(json!({
                "target_revision": { "type": "string" },
                "base_revision": { "type": "string" },
                "message": { "type": "string" },
            })),
        },
        ToolDef {
            name: "tree_get",
            description: "Read the hierarchical tree index of a document (optionally depth-limited).",
            schema: selector(json!({
                "depth": { "type": "integer" },
                "include_summaries": { "type": "boolean" },
                "include_content": { "type": "boolean" },
            })),
        },
        ToolDef {
            name: "node_get",
            description: "Read one tree node: title path, source span, and content.",
            schema: selector(json!({
                "node_id": { "type": "string" },
                "context": { "type": "boolean", "description": "Include parent context marker" },
            })),
        },
        ToolDef {
            name: "search_hierarchy",
            description: "Hierarchical search over one document or the whole corpus. Returns ranked nodes with score breakdowns and a traversal trace.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "document_ids": { "type": "array", "items": { "type": "string" } },
                    "path_prefix": { "type": "string" },
                    "strategy": { "type": "string" },
                    "options": { "type": "object" },
                    "top_k": { "type": "integer" },
                    "include_trace": { "type": "boolean" },
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "index_rebuild",
            description: "Rebuild the tree index for a document, optionally with a specific builder.",
            schema: selector(json!({
                "builder": { "type": "string" },
                "force": { "type": "boolean" },
            })),
        },
    ]
}

fn allowed(ws: &Workspace, name: &str) -> bool {
    let allowlist = &ws.config.security.mcp_tool_allowlist;
    allowlist.is_empty() || allowlist.iter().any(|t| t == name)
}

pub(crate) fn list_tools(ws: &Workspace) -> Vec<Tool> {
    tool_defs()
        .into_iter()
        .filter(|t| allowed(ws, t.name))
        .map(|t| {
            let schema = t.schema.as_object().cloned().unwrap_or_default();
            Tool::new(t.name, t.description, Arc::new(schema))
        })
        .collect()
}

/// Route one tools/call to the shared services. Returns the structured
/// result plus the notifications it made due.
pub(crate) fn dispatch(
    ws: &Workspace,
    actor: &Actor,
    request: &CallToolRequestParams,
) -> Result<(Value, ChangeSet), ToolFailure> {
    let name = request.name.as_ref();
    let args = request
        .arguments
        .clone()
        .map(Value::Object)
        .unwrap_or_else(|| json!({}));

    if !allowed(ws, name) {
        return Err(HdsError::new(
            ErrorCode::PermissionDenied,
            format!("tool '{name}' is not allowed by workspace configuration"),
        )
        .into());
    }

    let docs = DocumentService::new(ws, actor.clone(), "mcp");
    let result: HdsResult<(Value, ChangeSet)> = match name {
        "document_create" => (|| {
            let path = required_str(&args, "path")?;
            let content = required_str(&args, "content")?;
            let metadata: BTreeMap<String, Value> = args
                .get("metadata")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            let if_exists = match args.get("if_exists").and_then(Value::as_str) {
                None | Some("error") => IfExists::Error,
                Some("overwrite") => IfExists::Overwrite,
                Some(other) => {
                    return Err(HdsError::new(
                        ErrorCode::InvalidArgument,
                        format!("invalid if_exists '{other}'"),
                    ));
                }
            };
            let outcome = docs.create(
                path,
                content,
                metadata,
                opt_string(&args, "message"),
                if_exists,
            )?;
            let changes = ChangeSet {
                document_id: Some(outcome.document.document_id.clone()),
                list_changed: true,
            };
            Ok((serde_json::to_value(&outcome)?, changes))
        })(),
        "document_get" => (|| {
            let range = parse_range(&args)?;
            let out = docs.read(
                &selector(&args)?,
                args.get("revision_id").and_then(Value::as_str),
                range,
            )?;
            Ok((serde_json::to_value(&out)?, ChangeSet::default()))
        })(),
        "document_patch" => (|| {
            if let Some(fmt) = args.get("format").and_then(Value::as_str)
                && PatchFormat::parse(fmt) != Some(PatchFormat::UnifiedDiff)
            {
                return Err(HdsError::new(
                    ErrorCode::InvalidArgument,
                    "only format 'unified_diff' is supported",
                ));
            }
            let outcome = docs.patch(
                &selector(&args)?,
                required_str(&args, "base_revision")?,
                required_str(&args, "patch")?,
                PatchFormat::UnifiedDiff,
                opt_string(&args, "message"),
            )?;
            let changes = ChangeSet {
                document_id: Some(outcome.document.document_id.clone()),
                list_changed: false,
            };
            Ok((serde_json::to_value(&outcome)?, changes))
        })(),
        "document_replace" => (|| {
            let outcome = docs.replace(
                &selector(&args)?,
                required_str(&args, "base_revision")?,
                required_str(&args, "content")?,
                opt_string(&args, "message"),
            )?;
            let changes = ChangeSet {
                document_id: Some(outcome.document.document_id.clone()),
                list_changed: false,
            };
            Ok((serde_json::to_value(&outcome)?, changes))
        })(),
        "document_list" => (|| {
            let (list, next) = docs.list(
                args.get("prefix").and_then(Value::as_str),
                args.get("cursor").and_then(Value::as_str),
                args.get("limit")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize),
                args.get("include_deleted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )?;
            Ok((
                json!({ "documents": list, "next_cursor": next }),
                ChangeSet::default(),
            ))
        })(),
        "document_history" => (|| {
            let (revisions, next) = docs.history(
                &selector(&args)?,
                args.get("cursor").and_then(Value::as_str),
                args.get("limit")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize),
            )?;
            Ok((
                json!({ "revisions": revisions, "next_cursor": next }),
                ChangeSet::default(),
            ))
        })(),
        "document_diff" => (|| {
            let (diff, summary) = docs.diff(
                &selector(&args)?,
                required_str(&args, "from_revision")?,
                required_str(&args, "to_revision")?,
            )?;
            Ok((
                json!({ "diff": diff, "summary": summary }),
                ChangeSet::default(),
            ))
        })(),
        "document_restore" => (|| {
            let outcome = docs.restore(
                &selector(&args)?,
                required_str(&args, "target_revision")?,
                required_str(&args, "base_revision")?,
                opt_string(&args, "message"),
            )?;
            let changes = ChangeSet {
                document_id: Some(outcome.document.document_id.clone()),
                list_changed: false,
            };
            Ok((serde_json::to_value(&outcome)?, changes))
        })(),
        "tree_get" => (|| {
            let doc = docs.resolve(&selector(&args)?)?;
            let index_service = IndexService::new(ws);
            let include_summaries = args
                .get("include_summaries")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let depth = args
                .get("depth")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let (index, tree, stale) = index_service.tree(&doc, depth, include_summaries)?;
            let mut result = json!({
                "document_id": doc.document_id,
                "logical_path": doc.logical_path,
                "revision_id": index.revision_id,
                "index_version": index.index_version,
                "builder": index.builder,
                "stale": stale,
                "tree": tree,
                "diagnostics": index.diagnostics,
            });
            if args
                .get("include_content")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let content = ws
                    .files
                    .read_document(&doc.logical_path, ws.config.security.follow_symlinks)?;
                result["content"] = json!(content);
            }
            Ok((result, ChangeSet::default()))
        })(),
        "node_get" => (|| {
            let doc = docs.resolve(&selector(&args)?)?;
            let node_id = required_str(&args, "node_id")?;
            let context = args
                .get("context")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (out, stale) = IndexService::new(ws).node(&doc, node_id, context)?;
            let mut value = serde_json::to_value(&out)?;
            value["stale"] = json!(stale);
            Ok((value, ChangeSet::default()))
        })(),
        "search_hierarchy" => (|| {
            let search = SearchService::new(ws, actor.clone(), "mcp");
            let request = SearchRequest {
                query: required_str(&args, "query")?.to_string(),
                document_ids: args
                    .get("document_ids")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default(),
                path_prefix: opt_string(&args, "path_prefix"),
                strategy: opt_string(&args, "strategy"),
                options: args.get("options").cloned(),
                top_k: args
                    .get("top_k")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize),
                include_trace: args
                    .get("include_trace")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            let result = search.search(&request)?;
            Ok((serde_json::to_value(&result)?, ChangeSet::default()))
        })(),
        "index_rebuild" => (|| {
            let doc = docs.resolve(&selector(&args)?)?;
            let outcome = IndexService::new(ws).rebuild_with(
                &doc,
                None,
                args.get("builder").and_then(Value::as_str),
                args.get("force").and_then(Value::as_bool).unwrap_or(false),
            )?;
            let changes = ChangeSet {
                document_id: Some(doc.document_id.clone()),
                list_changed: false,
            };
            Ok((serde_json::to_value(&outcome)?, changes))
        })(),
        other => return Err(ToolFailure::UnknownTool(other.to_string())),
    };
    result.map_err(ToolFailure::from)
}

fn selector(args: &Value) -> HdsResult<DocSelector> {
    if let Some(id) = args.get("document_id").and_then(Value::as_str) {
        return Ok(DocSelector::Id(id.to_string()));
    }
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        return Ok(DocSelector::Path(path.to_string()));
    }
    Err(HdsError::new(
        ErrorCode::InvalidArgument,
        "either 'document_id' or 'path' is required",
    ))
}

fn required_str<'a>(args: &'a Value, key: &str) -> HdsResult<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| HdsError::new(ErrorCode::InvalidArgument, format!("missing '{key}'")))
}

fn opt_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

fn parse_range(args: &Value) -> HdsResult<ReadRange> {
    let Some(range) = args.get("range") else {
        return Ok(ReadRange::Full);
    };
    if range.is_null() {
        return Ok(ReadRange::Full);
    }
    if let Some(lines) = range.get("lines").and_then(Value::as_array)
        && let [Some(s), Some(e)] = [lines.first(), lines.get(1)].map(|v| v.and_then(Value::as_u64))
    {
        return Ok(ReadRange::Lines(s as usize, e as usize));
    }
    if let Some(bytes) = range.get("bytes").and_then(Value::as_array)
        && let [Some(s), Some(e)] = [bytes.first(), bytes.get(1)].map(|v| v.and_then(Value::as_u64))
    {
        return Ok(ReadRange::Bytes(s as usize, e as usize));
    }
    Err(HdsError::new(
        ErrorCode::InvalidArgument,
        "range must contain 'lines' or 'bytes' as [start, end]",
    ))
}

/// `tree_get`'s nested rendering is shared with resources.
pub(crate) fn render_tree_for(ws: &Workspace, doc: &crate::domain::Document) -> HdsResult<Value> {
    let (index, _stale) = IndexService::new(ws).current_index(doc)?;
    Ok(json!({
        "document_id": doc.document_id,
        "revision_id": index.revision_id,
        "index_version": index.index_version,
        "tree": render_subtree(&index, &index.root_id, None, true),
    }))
}
