//! Application services shared by the CLI and MCP adapters.
//!
//! `Workspace` wires configuration, storage, and plugin registries together;
//! the service types expose the use cases. Adapters stay thin: they parse
//! input, call a service, and format output.

pub mod documents;
pub mod indexing;
pub mod searching;

pub use documents::{DocSelector, DocumentService};
pub use indexing::IndexService;
pub use searching::{SearchRequest, SearchService};

use crate::config::Config;
use crate::domain::{Actor, ActorType, AuditEvent, HdsResult, IndexStatus, Operation};
use crate::index::BuilderRegistry;
use crate::infra::file_store::content_hash;
use crate::infra::{AuditLog, FileStore, MetadataDb, WorkspaceLayout};
use crate::search::lexical::LexicalScorer;
use crate::search::{NodeScorer, StrategyRegistry};
use chrono::Utc;
use std::path::Path;

pub struct Workspace {
    pub layout: WorkspaceLayout,
    pub config: Config,
    pub db: MetadataDb,
    pub files: FileStore,
    pub audit: AuditLog,
    pub builders: BuilderRegistry,
    pub strategies: StrategyRegistry,
    pub scorer: Box<dyn NodeScorer>,
    /// Messages produced while reconciling interrupted writes at startup.
    pub recovery_report: Vec<String>,
}

impl Workspace {
    /// Create a new workspace at `root` (idempotent for directories, fails if
    /// a config already exists).
    pub fn init(root: &Path) -> HdsResult<Workspace> {
        let layout = WorkspaceLayout::new(root);
        layout.create_directories()?;
        let config = Config::default();
        if !layout.config_path().exists() {
            config.save(&layout.config_path())?;
        }
        Self::open(root)
    }

    /// Open an existing workspace and reconcile any interrupted writes.
    pub fn open(root: &Path) -> HdsResult<Workspace> {
        let layout = WorkspaceLayout::new(root);
        if !layout.config_path().is_file() {
            return Err(crate::domain::HdsError::not_found(format!(
                "workspace at {} (run `hds init`)",
                root.display()
            )));
        }
        layout.create_directories()?;
        let config = Config::load(&layout.config_path())?;
        let db = MetadataDb::open(&layout.db_path())?;
        let files = FileStore::new(layout.clone());
        let audit = AuditLog::new(layout.audit_log_path());
        let mut ws = Workspace {
            layout,
            config,
            db,
            files,
            audit,
            builders: BuilderRegistry::with_defaults(),
            strategies: StrategyRegistry::with_defaults(),
            scorer: Box::new(LexicalScorer),
            recovery_report: Vec::new(),
        };
        ws.recover_pending_writes()?;
        Ok(ws)
    }

    /// Reconcile `pending` revisions left by an interrupted write using
    /// content hashes. Ambiguous states are reported, never discarded.
    fn recover_pending_writes(&mut self) -> HdsResult<()> {
        let pending = self.db.pending_revisions()?;
        for p in pending {
            let rev = &p.revision;
            let current = self
                .files
                .read_document(&p.logical_path, self.config.security.follow_symlinks)
                .ok();
            let current_hash = current.as_deref().map(content_hash);
            let msg;
            if current_hash.as_deref() == rev.after_hash.as_deref() {
                // The file write completed: finalize.
                self.db.set_revision_status(&rev.revision_id, "committed")?;
                if let Some(mut doc) = self.db.document_by_id(&rev.document_id)? {
                    doc.current_revision = rev.revision_id.clone();
                    doc.content_hash = rev.after_hash.clone().unwrap_or_default();
                    doc.updated_at = Utc::now();
                    doc.index_status = IndexStatus::Stale;
                    self.db.update_document(&doc)?;
                }
                msg = format!("recovered: finalized interrupted write {}", rev.revision_id);
            } else if current_hash.as_deref() == rev.before_hash.as_deref() {
                // The file write never happened: roll the record back.
                self.db.set_revision_status(&rev.revision_id, "aborted")?;
                if rev.operation == Operation::Create
                    && let Some(mut doc) = self.db.document_by_id(&rev.document_id)?
                    && doc.current_revision == rev.revision_id
                {
                    doc.deleted = true;
                    self.db.update_document(&doc)?;
                }
                msg = format!("recovered: rolled back unapplied write {}", rev.revision_id);
            } else {
                msg = format!(
                    "ambiguous pending write {} on {} (file matches neither before nor after hash); \
                     left pending for manual review via `hds doctor`",
                    rev.revision_id, p.logical_path
                );
            }
            self.record_audit(
                &Actor {
                    actor_type: ActorType::System,
                    id: "recovery".to_string(),
                },
                "system",
                "recover_pending_write",
                serde_json::json!({ "revision_id": rev.revision_id }),
                "ok",
                0,
                Some(rev.document_id.clone()),
                Some(rev.revision_id.clone()),
                None,
            );
            self.recovery_report.push(msg);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_audit(
        &self,
        actor: &Actor,
        interface: &str,
        operation: &str,
        arguments: serde_json::Value,
        status: &str,
        latency_ms: u64,
        document_id: Option<String>,
        revision_id: Option<String>,
        error_code: Option<String>,
    ) {
        let event = AuditEvent {
            event_id: ulid::Ulid::generate().to_string(),
            created_at: Utc::now(),
            actor: actor.clone(),
            interface: interface.to_string(),
            operation: operation.to_string(),
            arguments: AuditLog::sanitize_arguments(&arguments),
            status: status.to_string(),
            latency_ms,
            document_id,
            revision_id,
            error_code,
        };
        // Audit failures must not mask the primary result; they are printed
        // to stderr instead.
        if let Err(e) = self.audit.append(&event) {
            eprintln!("hds: audit append failed: {e}");
        }
    }

    pub fn ensure_writable(&self) -> HdsResult<()> {
        if self.config.security.read_only {
            return Err(crate::domain::HdsError::new(
                crate::domain::ErrorCode::PermissionDenied,
                "workspace is in read-only mode",
            ));
        }
        Ok(())
    }

    /// Health checks for `hds doctor`. Returns (level, message) pairs where
    /// level is "ok", "warn", or "error".
    pub fn doctor(&self) -> Vec<(String, String)> {
        let mut findings: Vec<(String, String)> = Vec::new();
        let ok = |m: &str| ("ok".to_string(), m.to_string());

        match self.db.integrity_check() {
            Ok(s) if s == "ok" => findings.push(ok("sqlite integrity check passed")),
            Ok(s) => findings.push(("error".into(), format!("sqlite integrity: {s}"))),
            Err(e) => findings.push(("error".into(), format!("sqlite integrity failed: {e}"))),
        }

        match self.db.all_documents(false) {
            Ok(docs) => {
                let mut missing_files = 0;
                let mut missing_snapshots = 0;
                let mut stale = 0;
                for doc in &docs {
                    match self
                        .files
                        .document_exists(&doc.logical_path, self.config.security.follow_symlinks)
                    {
                        Ok(true) => {}
                        _ => missing_files += 1,
                    }
                    if self
                        .files
                        .read_snapshot(&doc.document_id, &doc.current_revision)
                        .is_err()
                    {
                        missing_snapshots += 1;
                    }
                    if doc.index_status != IndexStatus::Ready {
                        stale += 1;
                    }
                }
                findings.push(ok(&format!("{} registered documents", docs.len())));
                if missing_files > 0 {
                    findings.push((
                        "error".into(),
                        format!("{missing_files} documents missing on disk"),
                    ));
                }
                if missing_snapshots > 0 {
                    findings.push((
                        "error".into(),
                        format!("{missing_snapshots} current-revision snapshots missing"),
                    ));
                }
                if stale > 0 {
                    findings.push((
                        "warn".into(),
                        format!("{stale} documents with non-ready indexes"),
                    ));
                }
                // Orphans: files on disk without a registration.
                let mut orphans = 0;
                let docs_dir = self.layout.documents_dir();
                let registered: std::collections::HashSet<&str> =
                    docs.iter().map(|d| d.logical_path.as_str()).collect();
                visit_markdown_files(&docs_dir, &mut |rel| {
                    if !registered.contains(rel) {
                        orphans += 1;
                    }
                });
                if orphans > 0 {
                    findings.push(("warn".into(), format!("{orphans} unregistered markdown files under documents/ (import with `hds document add`)")));
                }
            }
            Err(e) => findings.push(("error".into(), format!("cannot list documents: {e}"))),
        }

        match self.db.pending_revisions() {
            Ok(p) if p.is_empty() => findings.push(ok("no pending write operations")),
            Ok(p) => findings.push((
                "error".into(),
                format!("{} ambiguous pending write(s) need manual review", p.len()),
            )),
            Err(e) => findings.push((
                "error".into(),
                format!("cannot inspect pending writes: {e}"),
            )),
        }

        match self.builders.get(&self.config.tree.builder) {
            Ok(b) => findings.push(ok(&format!(
                "tree builder '{}' v{} loaded",
                b.name(),
                b.version()
            ))),
            Err(e) => findings.push(("error".into(), e.message)),
        }
        match self.strategies.get(&self.config.search.default_strategy) {
            Ok(s) => findings.push(ok(&format!(
                "default search strategy '{}' v{} loaded",
                s.name(),
                s.version()
            ))),
            Err(e) => findings.push(("error".into(), e.message)),
        }
        for msg in &self.recovery_report {
            findings.push(("warn".into(), msg.clone()));
        }
        findings
    }
}

fn visit_markdown_files(dir: &Path, f: &mut impl FnMut(&str)) {
    fn walk(base: &Path, dir: &Path, f: &mut impl FnMut(&str)) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, f);
            } else if path
                .extension()
                .is_some_and(|e| e == "md" || e == "markdown")
                && let Ok(rel) = path.strip_prefix(base)
            {
                f(&rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    walk(dir, dir, f);
}
