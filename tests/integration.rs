//! Integration tests: workspace lifecycle, sandboxing, tree construction,
//! search, patch/conflict flow, history/restore, crash recovery, and the MCP
//! message loop (spec §16.2 and acceptance criteria §17).

use tree_finder::domain::{Actor, ActorType, ErrorCode, IndexStatus, PatchFormat};

use serde_json::{Value, json};
use std::collections::BTreeMap;
use tree_finder::services::documents::{IfExists, ReadRange};
use tree_finder::services::{
    DocSelector, DocumentService, IndexService, SearchRequest, SearchService, Workspace,
    WorkspaceRegistry,
};

fn actor() -> Actor {
    Actor {
        actor_type: ActorType::Cli,
        id: "test".to_string(),
    }
}

fn new_workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ws = Workspace::init(dir.path()).expect("init");
    (dir, ws)
}

const OPS_DOC: &str = "# Ops Guide\n\nIntro paragraph about operations.\n\n## Deployment\n\nDeploy with the blue-green strategy.\n\n### Rollback procedure\n\nTo roll back, redeploy the previous artifact and run smoke tests.\n\n## Monitoring\n\nDashboards live in Grafana.\n";

fn create_ops_doc(ws: &Workspace) -> tree_finder::services::documents::MutationOutcome {
    DocumentService::new(ws, actor(), "cli")
        .create(
            "notes/ops.md",
            OPS_DOC,
            BTreeMap::new(),
            Some("initial".into()),
            IfExists::Error,
        )
        .expect("create")
}

// ----- path sandboxing (§14, §16.1) -----

#[test]
fn rejects_unsafe_paths() {
    let (_dir, ws) = new_workspace();
    let docs = DocumentService::new(&ws, actor(), "cli");
    for path in [
        "/etc/passwd.md",
        "../escape.md",
        "notes/../../escape.md",
        ".hds/config.yaml",
        ".hds/x.md",
        "notes/plain.txt",
        "",
        "notes/\u{7}bell.md",
    ] {
        let err = docs
            .create(path, "# x\n", BTreeMap::new(), None, IfExists::Error)
            .expect_err(&format!("path {path:?} must be rejected"));
        assert_eq!(err.code, ErrorCode::InvalidPath, "path {path:?}");
    }
}

#[cfg(unix)]
#[test]
fn rejects_symlink_escape() {
    let (dir, ws) = new_workspace();
    let outside = dir.path().join("outside.md");
    std::fs::write(&outside, "# secret\n").unwrap();
    let link = dir.path().join("documents").join("link.md");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let docs = DocumentService::new(&ws, actor(), "cli");
    // Reading or writing through the symlink must fail (symlinks disabled).
    let err = docs
        .create(
            "link.md",
            "# x\n",
            BTreeMap::new(),
            None,
            IfExists::Overwrite,
        )
        .expect_err("symlink write must fail");
    assert_eq!(err.code, ErrorCode::InvalidPath);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "# secret\n");
}

// ----- tree construction (§8, §16.1) -----

#[test]
fn builds_heading_tree_with_spans() {
    let (_dir, ws) = new_workspace();
    let outcome = create_ops_doc(&ws);
    let (index, stale) = IndexService::new(&ws)
        .current_index(&outcome.document)
        .expect("index");
    assert!(!stale);
    let root = index.root();
    let titles: Vec<&str> = index.nodes.values().map(|n| n.title.as_str()).collect();
    assert!(titles.contains(&"Rollback procedure"));
    assert!(titles.contains(&"Monitoring"));
    assert_eq!(root.title, "Ops Guide");
    // Sections carry line spans that cover their content.
    let rollback = index
        .nodes
        .values()
        .find(|n| n.title == "Rollback procedure")
        .unwrap();
    assert!(
        rollback.source.start_line >= 9 && rollback.source.end_line >= rollback.source.start_line
    );
    assert_eq!(
        rollback.attributes.heading_path.last().unwrap(),
        "Rollback procedure"
    );
}

#[test]
fn heading_titles_keep_inline_code_backticks() {
    let (_dir, ws) = new_workspace();
    let content = "# The `Atomic<T>` API\n\nBody.\n\n## Using `sync_cube()` and planes\n\nMore.\n";
    let outcome = DocumentService::new(&ws, actor(), "cli")
        .create("code.md", content, BTreeMap::new(), None, IfExists::Error)
        .unwrap();
    let (index, _) = IndexService::new(&ws)
        .current_index(&outcome.document)
        .unwrap();
    let titles: Vec<&str> = index.nodes.values().map(|n| n.title.as_str()).collect();
    assert!(
        titles.contains(&"The `Atomic<T>` API"),
        "titles: {titles:?}"
    );
    assert!(
        titles.contains(&"Using `sync_cube()` and planes"),
        "titles: {titles:?}"
    );
    assert_eq!(index.root().title, "The `Atomic<T>` API");
}

#[test]
fn skipped_heading_levels_attach_to_ancestor_with_diagnostic() {
    let (_dir, ws) = new_workspace();
    let content = "# Top\n\n#### Deep Dive\n\nBody text here.\n";
    let outcome = DocumentService::new(&ws, actor(), "cli")
        .create("skip.md", content, BTreeMap::new(), None, IfExists::Error)
        .unwrap();
    let (index, _) = IndexService::new(&ws)
        .current_index(&outcome.document)
        .unwrap();
    assert!(
        index.diagnostics.iter().any(|d| d.contains("level skip")),
        "diagnostics: {:?}",
        index.diagnostics
    );
    let deep = index
        .nodes
        .values()
        .find(|n| n.title == "Deep Dive")
        .unwrap();
    let parent = index.node(deep.parent_id.as_ref().unwrap()).unwrap();
    assert_eq!(parent.title, "Top");
}

#[test]
fn headings_inside_code_blocks_are_not_sections() {
    let (_dir, ws) = new_workspace();
    let content = "# Real\n\n```\n# not a heading\n## also not\n```\n\ntext\n";
    let outcome = DocumentService::new(&ws, actor(), "cli")
        .create("code.md", content, BTreeMap::new(), None, IfExists::Error)
        .unwrap();
    let (index, _) = IndexService::new(&ws)
        .current_index(&outcome.document)
        .unwrap();
    assert!(
        !index
            .nodes
            .values()
            .any(|n| n.title.contains("not a heading"))
    );
}

#[test]
fn node_ids_stable_across_unrelated_edits() {
    let (_dir, ws) = new_workspace();
    let outcome = create_ops_doc(&ws);
    let svc = IndexService::new(&ws);
    let (before, _) = svc.current_index(&outcome.document).unwrap();
    let rollback_before = before
        .nodes
        .values()
        .find(|n| n.title == "Rollback procedure")
        .unwrap()
        .node_id
        .clone();

    // Edit an unrelated section (Monitoring).
    let docs = DocumentService::new(&ws, actor(), "cli");
    let updated = OPS_DOC.replace("Grafana", "Grafana and Prometheus");
    let out2 = docs
        .replace(
            &DocSelector::Path("notes/ops.md".into()),
            &outcome.revision.revision_id,
            &updated,
            None,
        )
        .unwrap();
    let (after, _) = svc.current_index(&out2.document).unwrap();
    let rollback_after = after
        .nodes
        .values()
        .find(|n| n.title == "Rollback procedure")
        .unwrap()
        .node_id
        .clone();
    assert_eq!(rollback_before, rollback_after);
}

#[test]
fn synthetic_groups_created_for_long_unheaded_ranges() {
    let (_dir, ws) = new_workspace();
    let paragraphs: Vec<String> = (0..12)
        .map(|i| format!("Paragraph number {i} body."))
        .collect();
    let content = format!("# Long\n\n{}\n", paragraphs.join("\n\n"));
    let outcome = DocumentService::new(&ws, actor(), "cli")
        .create("long.md", &content, BTreeMap::new(), None, IfExists::Error)
        .unwrap();
    let (index, _) = IndexService::new(&ws)
        .current_index(&outcome.document)
        .unwrap();
    let groups: Vec<_> = index
        .nodes
        .values()
        .filter(|n| n.kind == tree_finder::domain::NodeKind::SyntheticGroup)
        .collect();
    assert!(
        groups.len() >= 2,
        "expected synthetic groups, got {}",
        groups.len()
    );
}

// ----- search (§9, §16.1) -----

#[test]
fn search_returns_scored_nodes_with_trace() {
    let (_dir, ws) = new_workspace();
    create_ops_doc(&ws);
    let search = SearchService::new(&ws, actor(), "cli");
    let result = search
        .search(&SearchRequest {
            query: "rollback procedure".into(),
            include_trace: true,
            ..Default::default()
        })
        .expect("search");
    assert!(!result.results.is_empty());
    let top = &result.results[0];
    assert!(top.node_path.iter().any(|p| p == "Rollback procedure"));
    assert!(top.score.total > 0.0);
    assert!(
        top.score.title > 0.0,
        "title signal expected: {:?}",
        top.score
    );
    assert!(!result.trace.is_empty());
    assert!(result.nodes_visited > 0);
    assert_eq!(result.strategy, "beam_tree_v1");
    assert!(!result.sources.is_empty());
    assert!(!result.config_hash.is_empty());
}

#[test]
fn search_is_deterministic() {
    let (_dir, ws) = new_workspace();
    create_ops_doc(&ws);
    let search = SearchService::new(&ws, actor(), "cli");
    let req = SearchRequest {
        query: "deployment strategy".into(),
        ..Default::default()
    };
    let a = search.search(&req).unwrap();
    let b = search.search(&req).unwrap();
    let ids = |r: &tree_finder::domain::SearchResult| -> Vec<String> {
        r.results.iter().map(|h| h.node_id.clone()).collect()
    };
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(a.nodes_visited, b.nodes_visited);
}

#[test]
fn switching_default_strategy_changes_algorithm() {
    let (dir, ws) = new_workspace();
    create_ops_doc(&ws);
    drop(ws);
    // Flip the config only — no code changes (acceptance criterion 4).
    let config_path = dir.path().join(".hds/config.yaml");
    let cfg = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        cfg.replace(
            "default_strategy: beam_tree_v1",
            "default_strategy: exhaustive_tree_v1",
        ),
    )
    .unwrap();
    let ws = Workspace::open(dir.path()).unwrap();
    let result = SearchService::new(&ws, actor(), "cli")
        .search(&SearchRequest {
            query: "rollback".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(result.strategy, "exhaustive_tree_v1");
}

#[test]
fn unknown_strategy_is_reported() {
    let (_dir, ws) = new_workspace();
    create_ops_doc(&ws);
    let err = SearchService::new(&ws, actor(), "cli")
        .search(&SearchRequest {
            query: "x".into(),
            strategy: Some("nope_v9".into()),
            ..Default::default()
        })
        .expect_err("must fail");
    assert_eq!(err.code, ErrorCode::StrategyNotFound);
}

#[test]
fn beam_respects_visit_budget() {
    let (_dir, ws) = new_workspace();
    create_ops_doc(&ws);
    let result = SearchService::new(&ws, actor(), "cli")
        .search(&SearchRequest {
            query: "operations".into(),
            options: Some(json!({ "max_nodes_visited": 1 })),
            ..Default::default()
        })
        .unwrap();
    assert!(result.nodes_visited <= 1);
    assert!(result.warnings.iter().any(|w| w.contains("budget")));
}

// ----- patch / conflict / history / restore (§10, §16.1) -----

#[test]
fn patch_applies_and_stale_base_conflicts_without_write() {
    let (_dir, ws) = new_workspace();
    let outcome = create_ops_doc(&ws);
    let docs = DocumentService::new(&ws, actor(), "cli");
    let base = outcome.revision.revision_id.clone();

    let updated = OPS_DOC.replace("Grafana", "Grafana and Loki");
    let patch = diffy::create_patch(OPS_DOC, &updated).to_string();
    let out2 = docs
        .patch(
            &DocSelector::Path("notes/ops.md".into()),
            &base,
            &patch,
            PatchFormat::UnifiedDiff,
            Some("loki".into()),
        )
        .expect("patch applies");
    assert_eq!(
        out2.revision.operation,
        tree_finder::domain::Operation::Patch
    );
    assert!(out2.diff_summary.is_some());

    // Same base again: conflict, and the file must not change.
    let before_file = ws.files.read_document("notes/ops.md", false).unwrap();
    let err = docs
        .patch(
            &DocSelector::Path("notes/ops.md".into()),
            &base,
            &patch,
            PatchFormat::UnifiedDiff,
            None,
        )
        .expect_err("stale base must conflict");
    assert_eq!(err.code, ErrorCode::RevisionConflict);
    assert_eq!(
        ws.files.read_document("notes/ops.md", false).unwrap(),
        before_file
    );

    // Unapplicable patch reports PATCH_FAILED.
    let bogus = diffy::create_patch("completely different\n", "other text\n").to_string();
    let err = docs
        .patch(
            &DocSelector::Path("notes/ops.md".into()),
            &out2.revision.revision_id,
            &bogus,
            PatchFormat::UnifiedDiff,
            None,
        )
        .expect_err("bad patch must fail");
    assert_eq!(err.code, ErrorCode::PatchFailed);
}

#[test]
fn restore_creates_new_revision_and_keeps_history() {
    let (_dir, ws) = new_workspace();
    let outcome = create_ops_doc(&ws);
    let docs = DocumentService::new(&ws, actor(), "cli");
    let rev1 = outcome.revision.revision_id.clone();
    let updated = OPS_DOC.replace("blue-green", "canary");
    let out2 = docs
        .replace(
            &DocSelector::Path("notes/ops.md".into()),
            &rev1,
            &updated,
            None,
        )
        .unwrap();
    let out3 = docs
        .restore(
            &DocSelector::Path("notes/ops.md".into()),
            &rev1,
            &out2.revision.revision_id,
            None,
        )
        .unwrap();
    assert_eq!(
        out3.revision.operation,
        tree_finder::domain::Operation::Restore
    );
    // Content equals revision 1, history now has three entries.
    assert_eq!(
        ws.files.read_document("notes/ops.md", false).unwrap(),
        OPS_DOC
    );
    let (revs, _) = docs
        .history(&DocSelector::Path("notes/ops.md".into()), None, Some(10))
        .unwrap();
    assert_eq!(revs.len(), 3);
    // Old revisions still readable (never rewritten).
    let old = docs
        .read(
            &DocSelector::Path("notes/ops.md".into()),
            Some(&out2.revision.revision_id),
            ReadRange::Full,
        )
        .unwrap();
    assert!(old.content.contains("canary"));
}

#[test]
fn every_mutation_records_revision_and_audit() {
    let (dir, ws) = new_workspace();
    let outcome = create_ops_doc(&ws);
    let docs = DocumentService::new(&ws, actor(), "cli");
    let rev1 = outcome.revision.revision_id.clone();
    let updated = OPS_DOC.replace("Grafana", "Kibana");
    let out2 = docs
        .replace(
            &DocSelector::Path("notes/ops.md".into()),
            &rev1,
            &updated,
            None,
        )
        .unwrap();
    docs.restore(
        &DocSelector::Path("notes/ops.md".into()),
        &rev1,
        &out2.revision.revision_id,
        None,
    )
    .unwrap();

    let audit = std::fs::read_to_string(dir.path().join(".hds/logs/audit.jsonl")).unwrap();
    let events: Vec<Value> = audit
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let ops: Vec<&str> = events
        .iter()
        .filter_map(|e| e["operation"].as_str())
        .collect();
    assert!(ops.contains(&"document_create"));
    assert!(ops.contains(&"document_replace"));
    assert!(ops.contains(&"document_restore"));
    // Audit never contains document content.
    assert!(!audit.contains("Grafana"), "audit must not leak content");
    assert!(!audit.contains("Kibana"), "audit must not leak content");
    for e in &events {
        assert!(e["event_id"].as_str().is_some());
        assert!(e["latency_ms"].as_u64().is_some());
    }
}

#[test]
fn read_supports_line_and_byte_ranges_and_revisions() {
    let (_dir, ws) = new_workspace();
    create_ops_doc(&ws);
    let docs = DocumentService::new(&ws, actor(), "cli");
    let sel = DocSelector::Path("notes/ops.md".into());
    let lines = docs.read(&sel, None, ReadRange::Lines(5, 7)).unwrap();
    assert!(lines.content.contains("## Deployment"));
    let bytes = docs.read(&sel, None, ReadRange::Bytes(0, 11)).unwrap();
    assert_eq!(bytes.content, "# Ops Guide");
}

#[test]
fn create_conflicts_when_exists_and_overwrite_works() {
    let (_dir, ws) = new_workspace();
    create_ops_doc(&ws);
    let docs = DocumentService::new(&ws, actor(), "cli");
    let err = docs
        .create(
            "notes/ops.md",
            "# New\n",
            BTreeMap::new(),
            None,
            IfExists::Error,
        )
        .expect_err("must conflict");
    assert_eq!(err.code, ErrorCode::AlreadyExists);
    let out = docs
        .create(
            "notes/ops.md",
            "# New\n",
            BTreeMap::new(),
            None,
            IfExists::Overwrite,
        )
        .unwrap();
    assert_eq!(
        out.revision.operation,
        tree_finder::domain::Operation::Replace
    );
}

#[test]
fn list_pagination_walks_all_documents() {
    let (_dir, ws) = new_workspace();
    let docs = DocumentService::new(&ws, actor(), "cli");
    for i in 0..7 {
        docs.create(
            &format!("p/doc{i}.md"),
            "# D\n\ntext\n",
            BTreeMap::new(),
            None,
            IfExists::Error,
        )
        .unwrap();
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (page, next) = docs.list(None, cursor.as_deref(), Some(3), false).unwrap();
        assert!(page.len() <= 3);
        seen.extend(page.into_iter().map(|d| d.logical_path));
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(seen.len(), 7);
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted);
}

// ----- crash recovery (§13, §16.2) -----

#[test]
fn recovery_finalizes_completed_write_and_rolls_back_unapplied() {
    let (dir, ws) = new_workspace();
    let outcome = create_ops_doc(&ws);
    let doc = outcome.document.clone();

    // Simulate a crash after the file replace but before finalize:
    // insert a pending revision whose after-hash matches a new file state.
    let new_content = OPS_DOC.replace("Grafana", "Datadog");
    let new_hash = tree_finder::infra::file_store::content_hash(&new_content);
    let pending = tree_finder::domain::Revision {
        revision_id: ulid::Ulid::generate().to_string(),
        parent_revision_id: Some(doc.current_revision.clone()),
        document_id: doc.document_id.clone(),
        actor: actor(),
        operation: tree_finder::domain::Operation::Replace,
        before_hash: Some(doc.content_hash.clone()),
        after_hash: Some(new_hash.clone()),
        message: None,
        created_at: chrono::Utc::now(),
        patch_format: tree_finder::domain::PatchFormat::FullSnapshot,
    };
    ws.files
        .write_snapshot(&doc.document_id, &pending.revision_id, &new_content)
        .unwrap();
    ws.db.insert_revision(&pending, "pending").unwrap();
    ws.files
        .write_document("notes/ops.md", &new_content, false)
        .unwrap();
    drop(ws);

    let ws = Workspace::open(dir.path()).unwrap();
    assert!(ws.recovery_report.iter().any(|m| m.contains("finalized")));
    let doc_after = ws.db.document_by_id(&doc.document_id).unwrap().unwrap();
    assert_eq!(doc_after.current_revision, pending.revision_id);
    assert_eq!(doc_after.content_hash, new_hash);

    // Simulate a crash before the file replace: pending revision, file
    // still at the old content. Recovery must roll the record back.
    let doc = doc_after;
    let unapplied = tree_finder::domain::Revision {
        revision_id: ulid::Ulid::generate().to_string(),
        parent_revision_id: Some(doc.current_revision.clone()),
        document_id: doc.document_id.clone(),
        actor: actor(),
        operation: tree_finder::domain::Operation::Replace,
        before_hash: Some(doc.content_hash.clone()),
        after_hash: Some("sha256:deadbeef".to_string()),
        message: None,
        created_at: chrono::Utc::now(),
        patch_format: tree_finder::domain::PatchFormat::FullSnapshot,
    };
    ws.db.insert_revision(&unapplied, "pending").unwrap();
    drop(ws);
    let ws = Workspace::open(dir.path()).unwrap();
    assert!(ws.recovery_report.iter().any(|m| m.contains("rolled back")));
    let doc_final = ws.db.document_by_id(&doc.document_id).unwrap().unwrap();
    assert_eq!(doc_final.current_revision, doc.current_revision);
    assert!(ws.db.revision(&unapplied.revision_id).unwrap().is_none());
}

#[test]
fn indexes_rebuildable_from_files_and_metadata() {
    let (dir, ws) = new_workspace();
    let outcome = create_ops_doc(&ws);
    drop(ws);
    // Wipe all derived indexes.
    std::fs::remove_dir_all(dir.path().join(".hds/indexes")).unwrap();
    let ws = Workspace::open(dir.path()).unwrap();
    let (index, stale) = IndexService::new(&ws)
        .current_index(&outcome.document)
        .unwrap();
    assert!(!stale);
    assert!(index.nodes.len() > 3);
    // Search works again end to end.
    let result = SearchService::new(&ws, actor(), "cli")
        .search(&SearchRequest {
            query: "rollback".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(!result.results.is_empty());
}

// ----- MCP flow via the rmcp SDK (§11, §16.2) -----

use rmcp::ClientHandler;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, Implementation,
    JsonObject, ProtocolVersion, ReadResourceRequestParams, ResourceContents,
    ResourceUpdatedNotificationParam, ServerNotification, SubscribeRequestParams,
    SubscriptionFilter,
};
use rmcp::service::{NotificationContext, RoleClient, RunningService};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Test client that records the notifications the server pushes.
#[derive(Clone, Default)]
struct RecordingClient {
    updated: Arc<StdMutex<Vec<String>>>,
    list_changed: Arc<AtomicBool>,
}

impl ClientHandler for RecordingClient {
    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.updated.lock().unwrap().push(params.uri);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.list_changed.store(true, Ordering::SeqCst);
    }
}

/// Serve `ws` over an in-process duplex pipe and connect a recording client.
async fn start_mcp(
    ws: Workspace,
) -> (RunningService<RoleClient, RecordingClient>, RecordingClient) {
    start_mcp_registry(WorkspaceRegistry::with_default(ws)).await
}

/// Serve a workspace registry over an in-process duplex pipe.
async fn start_mcp_registry(
    registry: WorkspaceRegistry,
) -> (RunningService<RoleClient, RecordingClient>, RecordingClient) {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(server) = tree_finder::mcp::HdsMcpServer::from_registry(registry)
            .serve(server_io)
            .await
        {
            let _ = server.waiting().await;
        }
    });
    let handler = RecordingClient::default();
    let client = handler
        .clone()
        .serve(client_io)
        .await
        .expect("client connects");
    (client, handler)
}

fn args(value: Value) -> JsonObject {
    value.as_object().cloned().expect("object args")
}

async fn call<H: ClientHandler>(
    client: &RunningService<RoleClient, H>,
    name: &str,
    arguments: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(args(arguments)))
        .await
        .expect("tools/call succeeds at the protocol level")
}

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("structured content")
}

async fn wait_for(mut check: impl FnMut() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("condition not met within 1s");
}

#[tokio::test]
async fn mcp_full_flow_create_read_patch_tree_search_history() {
    let (_dir, ws) = new_workspace();
    let (client, notifications) = start_mcp(ws).await;

    // Server advertises tools + subscribable resources.
    let info = client.peer_info().expect("server info");
    let caps = &info.capabilities;
    assert!(caps.tools.is_some());
    let resources = caps.resources.as_ref().expect("resource capability");
    assert_eq!(resources.subscribe, Some(true));
    assert_eq!(resources.list_changed, Some(true));

    let tools = client.list_tools(None).await.unwrap();
    assert_eq!(tools.tools.len(), 13);

    // Create a document (criterion 1: immediately discoverable + readable).
    let created = call(
        &client,
        "document_create",
        json!({
            "path": "kb/setup.md",
            "content": "# Setup\n\nInstall steps.\n\n## Prerequisites\n\nA C compiler and make.\n",
            "message": "first",
        }),
    )
    .await;
    assert_eq!(created.is_error, Some(false));
    let doc_id = structured(&created)["document"]["document_id"]
        .as_str()
        .unwrap()
        .to_string();
    let rev1 = structured(&created)["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for(|| notifications.list_changed.load(Ordering::SeqCst)).await;

    let listed = client.list_resources(None).await.unwrap();
    let content_uri = format!("hds://document/{doc_id}/content");
    assert!(
        listed.resources.iter().any(|r| r.uri == content_uri),
        "created document must be listed as a resource"
    );
    let read = client
        .read_resource(ReadResourceRequestParams::new(content_uri.clone()))
        .await
        .unwrap();
    let text = match &read.contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        _ => panic!("expected text contents"),
    };
    assert!(text.contains("# Setup"));

    // Complete tree and node content (criterion 2).
    let tree = call(&client, "tree_get", json!({ "document_id": doc_id })).await;
    assert_eq!(tree.is_error, Some(false));
    let tree_root = &structured(&tree)["tree"];
    let node_id = tree_root["children"][0]["children"][0]["node_id"]
        .as_str()
        .or_else(|| tree_root["children"][0]["node_id"].as_str())
        .unwrap()
        .to_string();
    let node = call(
        &client,
        "node_get",
        json!({
            "document_id": doc_id, "node_id": node_id,
        }),
    )
    .await;
    assert_eq!(node.is_error, Some(false));
    assert!(structured(&node)["content"].as_str().is_some());

    // Subscribe, patch with base revision (criterion 5), and expect an
    // updated notification for the subscribed URI. This client negotiates
    // 2025-11-25, where `resources/subscribe` is still the subscription
    // mechanism; see `mcp_listen_stream_delivers_updates_on_draft_protocol`
    // for the 2026-07-28 replacement.
    #[allow(deprecated)]
    client
        .subscribe(SubscribeRequestParams::new(content_uri.clone()))
        .await
        .unwrap();
    let patch = diffy::create_patch(
        "# Setup\n\nInstall steps.\n\n## Prerequisites\n\nA C compiler and make.\n",
        "# Setup\n\nInstall steps.\n\n## Prerequisites\n\nA C compiler, make, and cargo.\n",
    )
    .to_string();
    let patched = call(
        &client,
        "document_patch",
        json!({
            "document_id": doc_id, "base_revision": rev1, "patch": patch, "format": "unified_diff",
        }),
    )
    .await;
    assert_eq!(patched.is_error, Some(false), "patch failed: {patched:?}");
    wait_for(|| notifications.updated.lock().unwrap().contains(&content_uri)).await;

    // Stale base now conflicts with stable error data (criterion 6).
    let conflict = call(&client, "document_patch", json!({
        "document_id": doc_id, "base_revision": rev1, "patch": "@@ -1 +1 @@", "format": "unified_diff",
    }))
    .await;
    assert_eq!(conflict.is_error, Some(true));
    let wire = structured(&conflict);
    assert_eq!(wire["code"], "REVISION_CONFLICT");
    assert_eq!(wire["retryable"], false);
    assert!(wire["details"]["actual"].as_str().is_some());

    // Search with score breakdown and trace (criterion 3).
    let found = call(
        &client,
        "search_hierarchy",
        json!({
            "query": "prerequisites compiler", "include_trace": true,
        }),
    )
    .await;
    assert_eq!(found.is_error, Some(false));
    let sc = structured(&found).clone();
    assert!(!sc["results"].as_array().unwrap().is_empty());
    assert!(sc["results"][0]["score"]["total"].as_f64().unwrap() > 0.0);
    assert!(!sc["trace"].as_array().unwrap().is_empty());

    // History through tool and resource.
    let history = call(
        &client,
        "document_history",
        json!({ "document_id": doc_id }),
    )
    .await;
    assert_eq!(
        structured(&history)["revisions"].as_array().unwrap().len(),
        2
    );
    let hist = client
        .read_resource(ReadResourceRequestParams::new(format!(
            "hds://document/{doc_id}/history"
        )))
        .await
        .unwrap();
    let hist_text = match &hist.contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        _ => panic!("expected text contents"),
    };
    assert!(hist_text.contains("patch"));

    // Search trace resource exists for the recorded run.
    let run_id = sc["search_run_id"].as_str().unwrap();
    let trace = client
        .read_resource(ReadResourceRequestParams::new(format!(
            "hds://search-run/{run_id}/trace"
        )))
        .await
        .unwrap();
    let trace_text = match &trace.contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        _ => panic!("expected text contents"),
    };
    assert!(trace_text.contains("trace"));

    // Resource templates advertised.
    let templates = client.list_resource_templates(None).await.unwrap();
    assert!(
        templates
            .resource_templates
            .iter()
            .any(|t| t.uri_template.contains("{node_id}"))
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn mcp_and_service_layer_produce_equivalent_results() {
    let (_dir, ws) = new_workspace();
    create_ops_doc(&ws);
    // Direct service call.
    let direct = SearchService::new(&ws, actor(), "cli")
        .search(&SearchRequest {
            query: "rollback procedure".into(),
            ..Default::default()
        })
        .unwrap();
    // Same query through the MCP adapter on the same workspace.
    let (client, _notifications) = start_mcp(ws).await;
    let via_mcp = call(
        &client,
        "search_hierarchy",
        json!({
            "query": "rollback procedure",
        }),
    )
    .await;
    let sc = structured(&via_mcp);
    let mcp_nodes: Vec<&str> = sc["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node_id"].as_str().unwrap())
        .collect();
    let direct_nodes: Vec<&str> = direct.results.iter().map(|h| h.node_id.as_str()).collect();
    assert_eq!(mcp_nodes, direct_nodes);
    client.cancel().await.ok();
}

#[tokio::test]
async fn mcp_tool_allowlist_blocks_tools() {
    let (dir, ws) = new_workspace();
    drop(ws);
    let config_path = dir.path().join(".hds/config.yaml");
    let cfg = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        cfg.replace(
            "mcp_tool_allowlist: []",
            "mcp_tool_allowlist: [document_get, document_list]",
        ),
    )
    .unwrap();
    let ws = Workspace::open(dir.path()).unwrap();
    let (client, _notifications) = start_mcp(ws).await;
    let tools = client.list_tools(None).await.unwrap();
    assert_eq!(tools.tools.len(), 2);
    let blocked = call(
        &client,
        "document_create",
        json!({
            "path": "x.md", "content": "# x\n",
        }),
    )
    .await;
    assert_eq!(blocked.is_error, Some(true));
    assert_eq!(structured(&blocked)["code"], "PERMISSION_DENIED");
    client.cancel().await.ok();
}

#[test]
fn read_only_mode_blocks_mutations() {
    let (dir, ws) = new_workspace();
    create_ops_doc(&ws);
    drop(ws);
    let config_path = dir.path().join(".hds/config.yaml");
    let cfg = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        cfg.replace("read_only: false", "read_only: true"),
    )
    .unwrap();
    let ws = Workspace::open(dir.path()).unwrap();
    let docs = DocumentService::new(&ws, actor(), "cli");
    let err = docs
        .create(
            "blocked.md",
            "# x\n",
            BTreeMap::new(),
            None,
            IfExists::Error,
        )
        .expect_err("read-only must block");
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    // Reads still work.
    assert!(
        docs.read(
            &DocSelector::Path("notes/ops.md".into()),
            None,
            ReadRange::Full
        )
        .is_ok()
    );
}

// ----- limits (§14) -----

#[test]
fn size_limits_are_enforced() {
    let (dir, ws) = new_workspace();
    drop(ws);
    let config_path = dir.path().join(".hds/config.yaml");
    let cfg = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        cfg.replace("max_file_bytes: 10485760", "max_file_bytes: 64"),
    )
    .unwrap();
    let ws = Workspace::open(dir.path()).unwrap();
    let docs = DocumentService::new(&ws, actor(), "cli");
    let big = format!("# Big\n\n{}\n", "word ".repeat(100));
    let err = docs
        .create("big.md", &big, BTreeMap::new(), None, IfExists::Error)
        .expect_err("must exceed limit");
    assert_eq!(err.code, ErrorCode::LimitExceeded);
}

#[test]
fn index_status_transitions_to_ready_after_write() {
    let (_dir, ws) = new_workspace();
    let outcome = create_ops_doc(&ws);
    let doc = ws
        .db
        .document_by_id(&outcome.document.document_id)
        .unwrap()
        .unwrap();
    assert_eq!(doc.index_status, IndexStatus::Ready);
}

// ---------------------------------------------------------------------------
// Multi-workspace MCP serving
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_workspace_argument_targets_other_workspaces() {
    let (_dir_a, ws_a) = new_workspace();
    let dir_b = tempfile::tempdir().expect("tempdir");
    Workspace::init(dir_b.path()).expect("init b");
    let (client, _notifications) = start_mcp(ws_a).await;

    // Calls without a `workspace` argument go to the default workspace.
    let a = call(
        &client,
        "document_create",
        json!({ "path": "a.md", "content": "# A\n" }),
    )
    .await;
    assert_eq!(a.is_error, Some(false));

    // A call naming workspace B opens it on demand and writes there.
    let b = call(
        &client,
        "document_create",
        json!({
            "path": "b.md",
            "content": "# B doc\n\nOnly in workspace B.\n",
            "workspace": dir_b.path().to_string_lossy(),
        }),
    )
    .await;
    assert_eq!(b.is_error, Some(false), "create in B failed: {b:?}");
    assert!(dir_b.path().join("documents/b.md").is_file());

    // Listings are scoped to the addressed workspace; a path *inside* the
    // workspace resolves to its root by walking up to `.hds`.
    let paths = |result: &CallToolResult| -> Vec<String> {
        structured(result)["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["logical_path"].as_str().unwrap().to_string())
            .collect()
    };
    let list_default = call(&client, "document_list", json!({})).await;
    assert_eq!(paths(&list_default), vec!["a.md"]);
    let inside_b = dir_b.path().join("documents");
    let list_b = call(
        &client,
        "document_list",
        json!({ "workspace": inside_b.to_string_lossy() }),
    )
    .await;
    assert_eq!(paths(&list_b), vec!["b.md"]);

    // workspace_list reports both, with exactly one default.
    let wl = call(&client, "workspace_list", json!({})).await;
    assert_eq!(wl.is_error, Some(false));
    let workspaces = structured(&wl)["workspaces"].as_array().unwrap().clone();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(
        workspaces
            .iter()
            .filter(|w| w["default"] == json!(true))
            .count(),
        1
    );
    let b_canon = dir_b.path().canonicalize().unwrap();
    assert!(
        workspaces
            .iter()
            .any(|w| w["root"].as_str().unwrap() == b_canon.to_string_lossy())
    );

    // Reads honor the workspace argument too.
    let got = call(
        &client,
        "document_get",
        json!({ "path": "b.md", "workspace": dir_b.path().to_string_lossy() }),
    )
    .await;
    assert_eq!(got.is_error, Some(false));
    assert!(
        structured(&got)["content"]
            .as_str()
            .unwrap()
            .contains("Only in workspace B")
    );

    // A path outside any workspace is a structured NOT_FOUND, and the server
    // keeps serving afterwards.
    let missing = tempfile::tempdir().expect("tempdir");
    let bad = call(
        &client,
        "document_list",
        json!({ "workspace": missing.path().to_string_lossy() }),
    )
    .await;
    assert_eq!(bad.is_error, Some(true));
    assert_eq!(structured(&bad)["code"], "NOT_FOUND");
    let still_ok = call(&client, "document_list", json!({})).await;
    assert_eq!(still_ok.is_error, Some(false));

    client.cancel().await.ok();
}

#[tokio::test]
async fn mcp_serves_without_default_workspace() {
    let (client, _notifications) = start_mcp_registry(WorkspaceRegistry::new()).await;

    let wl = call(&client, "workspace_list", json!({})).await;
    assert_eq!(wl.is_error, Some(false));
    assert_eq!(structured(&wl)["workspaces"].as_array().unwrap().len(), 0);

    // Without a default, calls must name a workspace.
    let no_ws = call(&client, "document_list", json!({})).await;
    assert_eq!(no_ws.is_error, Some(true));
    assert_eq!(structured(&no_ws)["code"], "INVALID_ARGUMENT");

    let dir = tempfile::tempdir().expect("tempdir");
    Workspace::init(dir.path()).expect("init");
    let created = call(
        &client,
        "document_create",
        json!({
            "path": "n.md",
            "content": "# N\n",
            "workspace": dir.path().to_string_lossy(),
        }),
    )
    .await;
    assert_eq!(created.is_error, Some(false), "create failed: {created:?}");

    client.cancel().await.ok();
}

#[tokio::test]
async fn mcp_resources_span_open_workspaces() {
    let (_dir_a, ws_a) = new_workspace();
    let dir_b = tempfile::tempdir().expect("tempdir");
    Workspace::init(dir_b.path()).expect("init b");
    let (client, _notifications) = start_mcp(ws_a).await;

    let a = call(
        &client,
        "document_create",
        json!({ "path": "a.md", "content": "# A\n" }),
    )
    .await;
    let a_id = structured(&a)["document"]["document_id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = call(
        &client,
        "document_create",
        json!({
            "path": "b.md",
            "content": "# B resource\n",
            "workspace": dir_b.path().to_string_lossy(),
        }),
    )
    .await;
    let b_id = structured(&b)["document"]["document_id"]
        .as_str()
        .unwrap()
        .to_string();

    // resources/list pages workspace by workspace; walking the cursor chain
    // must surface documents from both workspaces.
    let mut uris = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let request = cursor
            .take()
            .map(|c| rmcp::model::PaginatedRequestParams::default().with_cursor(Some(c)));
        let page = client.list_resources(request).await.unwrap();
        uris.extend(page.resources.iter().map(|r| r.uri.clone()));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert!(uris.contains(&"hds://documents".to_string()));
    assert!(uris.contains(&format!("hds://document/{a_id}/content")));
    assert!(uris.contains(&format!("hds://document/{b_id}/content")));

    // Document-id URIs resolve across workspaces.
    let read = client
        .read_resource(ReadResourceRequestParams::new(format!(
            "hds://document/{b_id}/content"
        )))
        .await
        .unwrap();
    let text = match &read.contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        _ => panic!("expected text contents"),
    };
    assert!(text.contains("# B resource"));

    // The aggregate descriptor listing spans both workspaces.
    let all = client
        .read_resource(ReadResourceRequestParams::new("hds://documents"))
        .await
        .unwrap();
    let all_text = match &all.contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        _ => panic!("expected text contents"),
    };
    let docs: Value = serde_json::from_str(&all_text).unwrap();
    let ids: Vec<&str> = docs
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["document_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&a_id.as_str()));
    assert!(ids.contains(&b_id.as_str()));

    client.cancel().await.ok();
}

/// Client negotiating the 2026-07-28 draft, where the server must serve
/// `subscriptions/listen` because `resources/subscribe` is rejected.
#[derive(Clone, Default)]
struct DraftClient;

impl ClientHandler for DraftClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::from_build_env(),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
    }
}

#[tokio::test]
async fn mcp_listen_stream_delivers_updates_on_draft_protocol() {
    let (_dir, ws) = new_workspace();
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(server) = tree_finder::mcp::HdsMcpServer::new(ws)
            .serve(server_io)
            .await
        {
            let _ = server.waiting().await;
        }
    });
    let client = DraftClient.serve(client_io).await.expect("client connects");
    assert_eq!(
        client.peer_info().expect("server info").protocol_version,
        ProtocolVersion::V_2026_07_28,
        "server must serve the draft version the client asked for"
    );

    let created = call(
        &client,
        "document_create",
        json!({ "path": "notes/setup.md", "content": "# Setup\n\nInstall steps.\n" }),
    )
    .await;
    assert_eq!(created.is_error, Some(false));
    let doc_id = structured(&created)["document"]["document_id"]
        .as_str()
        .unwrap()
        .to_string();
    let rev1 = structured(&created)["revision"]["revision_id"]
        .as_str()
        .unwrap()
        .to_string();
    let content_uri = format!("hds://document/{doc_id}/content");

    // Open a stream for one URI plus the list-changed signal. Updates for the
    // document's other URIs must be filtered out by the accepted filter.
    let mut stream = client
        .listen(
            SubscriptionFilter::builder()
                .resource_subscription(content_uri.clone())
                .resources_list_changed()
                .build(),
        )
        .await
        .expect("listen stream is established");
    assert_eq!(
        stream.acknowledged().resource_subscriptions.as_deref(),
        Some([content_uri.clone()].as_slice())
    );

    let patch = diffy::create_patch(
        "# Setup\n\nInstall steps.\n",
        "# Setup\n\nInstall steps, then build.\n",
    )
    .to_string();
    let patched = call(
        &client,
        "document_patch",
        json!({
            "document_id": doc_id, "base_revision": rev1, "patch": patch, "format": "unified_diff",
        }),
    )
    .await;
    assert_eq!(patched.is_error, Some(false), "patch failed: {patched:?}");

    // A patch touches content/tree/history/descriptor, but only the subscribed
    // content URI may reach the stream.
    match next_notification(&mut stream).await {
        ServerNotification::ResourceUpdatedNotification(n) => assert_eq!(n.params.uri, content_uri),
        other => panic!("expected a resource update, got {other:?}"),
    }

    // A second create registers a new document: its own URIs are outside the
    // filter, so the list-changed signal is the only thing that arrives.
    let second = call(
        &client,
        "document_create",
        json!({ "path": "notes/other.md", "content": "# Other\n" }),
    )
    .await;
    assert_eq!(second.is_error, Some(false));
    match next_notification(&mut stream).await {
        ServerNotification::ResourceListChangedNotification(_) => {}
        other => panic!("expected resources/list_changed, got {other:?}"),
    }

    stream.cancel().await.ok();
    client.cancel().await.ok();
}

/// Await the next notification on a subscription, failing rather than hanging.
async fn next_notification(stream: &mut rmcp::service::Subscription) -> ServerNotification {
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("notification arrives within 5s")
        .expect("subscription stays healthy")
        .expect("subscription stays open")
}
