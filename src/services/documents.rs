//! Document use cases: create, read, patch, replace, list, history, diff,
//! restore, delete. Every mutation follows the recoverable write protocol
//! (spec §13) and records a revision plus an audit event.

use crate::domain::{
    Actor, Document, ErrorCode, HdsError, HdsResult, IndexStatus, Operation, PatchFormat, Revision,
};
use crate::infra::file_store::content_hash;
use crate::services::{IndexService, Workspace};
use chrono::Utc;
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone)]
pub enum DocSelector {
    Id(String),
    Path(String),
}

impl DocSelector {
    /// CLI convenience: UUID-shaped inputs select by ID, otherwise by path.
    pub fn parse(input: &str) -> DocSelector {
        if uuid::Uuid::parse_str(input).is_ok() {
            DocSelector::Id(input.to_string())
        } else {
            DocSelector::Path(input.to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfExists {
    Error,
    Overwrite,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MutationOutcome {
    pub document: Document,
    pub revision: Revision,
    pub diff_summary: Option<DiffSummary>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiffSummary {
    pub lines_added: usize,
    pub lines_removed: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadOutcome {
    pub document: Document,
    pub revision_id: String,
    pub content: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Copy)]
pub enum ReadRange {
    Full,
    Lines(usize, usize),
    Bytes(usize, usize),
}

pub struct DocumentService<'a> {
    ws: &'a Workspace,
    actor: Actor,
    interface: &'static str,
}

impl<'a> DocumentService<'a> {
    pub fn new(ws: &'a Workspace, actor: Actor, interface: &'static str) -> Self {
        DocumentService {
            ws,
            actor,
            interface,
        }
    }

    pub fn resolve(&self, selector: &DocSelector) -> HdsResult<Document> {
        let doc = match selector {
            DocSelector::Id(id) => self.ws.db.document_by_id(id)?,
            DocSelector::Path(path) => {
                let normalized = self.ws.layout.normalize_logical_path(path)?;
                self.ws.db.document_by_path(&normalized)?
            }
        };
        match doc {
            Some(d) if !d.deleted => Ok(d),
            Some(d) => Err(HdsError::not_found(format!(
                "document {} (deleted)",
                d.logical_path
            ))),
            None => Err(HdsError::not_found("document")),
        }
    }

    // ----- create -----

    pub fn create(
        &self,
        path: &str,
        content: &str,
        metadata: BTreeMap<String, serde_json::Value>,
        message: Option<String>,
        if_exists: IfExists,
    ) -> HdsResult<MutationOutcome> {
        let started = Instant::now();
        let result = self.create_inner(path, content, metadata, message, if_exists);
        self.audit_mutation(
            "document_create",
            serde_json::json!({ "path": path, "content": content, "if_exists": format!("{if_exists:?}") }),
            started,
            &result,
        );
        result
    }

    fn create_inner(
        &self,
        path: &str,
        content: &str,
        metadata: BTreeMap<String, serde_json::Value>,
        message: Option<String>,
        if_exists: IfExists,
    ) -> HdsResult<MutationOutcome> {
        self.ws.ensure_writable()?;
        let normalized = self.ws.layout.normalize_logical_path(path)?;
        self.check_size(content)?;

        if let Some(existing) = self.ws.db.document_by_path(&normalized)? {
            return match if_exists {
                IfExists::Error => Err(HdsError::new(
                    ErrorCode::AlreadyExists,
                    format!("document '{normalized}' already exists"),
                )
                .with_details(serde_json::json!({ "document_id": existing.document_id }))),
                IfExists::Overwrite => self.commit_change(
                    existing.clone(),
                    content,
                    Operation::Replace,
                    Some(existing.current_revision.clone()),
                    message,
                    PatchFormat::FullSnapshot,
                ),
            };
        }
        if self
            .ws
            .files
            .document_exists(&normalized, self.ws.config.security.follow_symlinks)?
            && if_exists == IfExists::Error
        {
            return Err(HdsError::new(
                ErrorCode::AlreadyExists,
                format!(
                    "unregistered file already exists at '{normalized}' (use if_exists=overwrite or `hds document add`)"
                ),
            ));
        }

        let now = Utc::now();
        let revision_id = ulid::Ulid::generate().to_string();
        let document_id = uuid::Uuid::new_v4().to_string();
        let doc = Document {
            document_id: document_id.clone(),
            logical_path: normalized.clone(),
            title: derive_title(content, &normalized),
            current_revision: revision_id.clone(),
            content_hash: content_hash(content),
            created_at: now,
            updated_at: now,
            index_status: IndexStatus::Stale,
            metadata,
            deleted: false,
        };
        let revision = Revision {
            revision_id: revision_id.clone(),
            parent_revision_id: None,
            document_id: document_id.clone(),
            actor: self.actor.clone(),
            operation: Operation::Create,
            before_hash: None,
            after_hash: Some(doc.content_hash.clone()),
            message,
            created_at: now,
            patch_format: PatchFormat::FullSnapshot,
        };

        // Recoverable write protocol.
        self.ws
            .files
            .write_snapshot(&document_id, &revision_id, content)?;
        self.ws.db.insert_document(&doc)?;
        self.ws.db.insert_revision(&revision, "pending")?;
        self.ws.files.write_document(
            &normalized,
            content,
            self.ws.config.security.follow_symlinks,
        )?;
        self.verify_written(&normalized, &doc.content_hash)?;
        self.ws.db.set_revision_status(&revision_id, "committed")?;

        let mut doc = doc;
        self.reindex_after_write(&mut doc, content);
        Ok(MutationOutcome {
            document: doc,
            revision,
            diff_summary: None,
        })
    }

    // ----- read -----

    pub fn read(
        &self,
        selector: &DocSelector,
        revision_id: Option<&str>,
        range: ReadRange,
    ) -> HdsResult<ReadOutcome> {
        let doc = self.resolve(selector)?;
        let (content, effective_revision) = match revision_id {
            None => (
                self.ws
                    .files
                    .read_document(&doc.logical_path, self.ws.config.security.follow_symlinks)?,
                doc.current_revision.clone(),
            ),
            Some(rev) => {
                let revision = self
                    .ws
                    .db
                    .revision(rev)?
                    .filter(|r| r.document_id == doc.document_id)
                    .ok_or_else(|| HdsError::not_found(format!("revision {rev}")))?;
                (
                    self.ws
                        .files
                        .read_snapshot(&doc.document_id, &revision.revision_id)?,
                    revision.revision_id,
                )
            }
        };
        let hash = content_hash(&content);
        let sliced = apply_range(&content, range)?;
        Ok(ReadOutcome {
            document: doc,
            revision_id: effective_revision,
            content: sliced,
            content_hash: hash,
        })
    }

    // ----- patch / replace -----

    pub fn patch(
        &self,
        selector: &DocSelector,
        base_revision: &str,
        patch_text: &str,
        format: PatchFormat,
        message: Option<String>,
    ) -> HdsResult<MutationOutcome> {
        let started = Instant::now();
        let result = self.patch_inner(selector, base_revision, patch_text, format, message);
        self.audit_mutation(
            "document_patch",
            serde_json::json!({ "base_revision": base_revision, "patch": patch_text }),
            started,
            &result,
        );
        result
    }

    fn patch_inner(
        &self,
        selector: &DocSelector,
        base_revision: &str,
        patch_text: &str,
        format: PatchFormat,
        message: Option<String>,
    ) -> HdsResult<MutationOutcome> {
        self.ws.ensure_writable()?;
        if format != PatchFormat::UnifiedDiff {
            return Err(HdsError::new(
                ErrorCode::InvalidArgument,
                "only patch format 'unified_diff' is supported in this release",
            ));
        }
        if patch_text.len() as u64 > self.ws.config.limits.max_patch_bytes {
            return Err(HdsError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "patch exceeds limit of {} bytes",
                    self.ws.config.limits.max_patch_bytes
                ),
            ));
        }
        let doc = self.resolve(selector)?;
        self.check_base_revision(&doc, base_revision)?;
        let current = self
            .ws
            .files
            .read_document(&doc.logical_path, self.ws.config.security.follow_symlinks)?;

        // Tolerate hunk-only patches by prepending a generic header.
        let owned_patch;
        let patch_source = if patch_text.trim_start().starts_with("@@") {
            owned_patch = format!("--- a\n+++ b\n{patch_text}");
            owned_patch.as_str()
        } else {
            patch_text
        };
        let parsed = diffy::Patch::from_str(patch_source).map_err(|e| {
            HdsError::new(ErrorCode::PatchFailed, format!("invalid unified diff: {e}"))
        })?;
        let updated = diffy::apply(&current, &parsed).map_err(|e| {
            HdsError::new(ErrorCode::PatchFailed, format!("patch does not apply: {e}"))
        })?;
        self.check_size(&updated)?;
        self.commit_change(
            doc,
            &updated,
            Operation::Patch,
            Some(base_revision.to_string()),
            message,
            PatchFormat::UnifiedDiff,
        )
    }

    pub fn replace(
        &self,
        selector: &DocSelector,
        base_revision: &str,
        content: &str,
        message: Option<String>,
    ) -> HdsResult<MutationOutcome> {
        let started = Instant::now();
        let result = self.replace_inner(selector, base_revision, content, message);
        self.audit_mutation(
            "document_replace",
            serde_json::json!({ "base_revision": base_revision, "content": content }),
            started,
            &result,
        );
        result
    }

    fn replace_inner(
        &self,
        selector: &DocSelector,
        base_revision: &str,
        content: &str,
        message: Option<String>,
    ) -> HdsResult<MutationOutcome> {
        self.ws.ensure_writable()?;
        self.check_size(content)?;
        let doc = self.resolve(selector)?;
        self.check_base_revision(&doc, base_revision)?;
        self.commit_change(
            doc,
            content,
            Operation::Replace,
            Some(base_revision.to_string()),
            message,
            PatchFormat::FullSnapshot,
        )
    }

    // ----- list / history / diff / restore / delete -----

    pub fn list(
        &self,
        prefix: Option<&str>,
        cursor: Option<&str>,
        limit: Option<usize>,
        include_deleted: bool,
    ) -> HdsResult<(Vec<Document>, Option<String>)> {
        let limit = limit
            .unwrap_or(100)
            .min(self.ws.config.limits.max_list_limit)
            .max(1);
        let normalized_prefix = match prefix {
            // Prefixes are directory-ish, not full document paths; only
            // lexical safety checks apply.
            Some(p) if p.contains("..") => {
                return Err(HdsError::invalid_path("prefix must not contain '..'"));
            }
            Some(p) => Some(p.trim_start_matches('/').to_string()),
            None => None,
        };
        self.ws
            .db
            .list_documents(normalized_prefix.as_deref(), cursor, limit, include_deleted)
    }

    pub fn history(
        &self,
        selector: &DocSelector,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> HdsResult<(Vec<Revision>, Option<String>)> {
        let doc = self.resolve(selector)?;
        let limit = limit
            .unwrap_or(50)
            .min(self.ws.config.limits.max_list_limit)
            .max(1);
        self.ws.db.list_revisions(&doc.document_id, cursor, limit)
    }

    pub fn diff(
        &self,
        selector: &DocSelector,
        from_revision: &str,
        to_revision: &str,
    ) -> HdsResult<(String, DiffSummary)> {
        let doc = self.resolve(selector)?;
        let from = self.revision_content(&doc, from_revision)?;
        let to = self.revision_content(&doc, to_revision)?;
        let patch = diffy::create_patch(&from, &to);
        let text = patch.to_string();
        let summary = diff_summary(&text);
        Ok((text, summary))
    }

    pub fn restore(
        &self,
        selector: &DocSelector,
        target_revision: &str,
        base_revision: &str,
        message: Option<String>,
    ) -> HdsResult<MutationOutcome> {
        let started = Instant::now();
        let result = self.restore_inner(selector, target_revision, base_revision, message);
        self.audit_mutation(
            "document_restore",
            serde_json::json!({ "target_revision": target_revision, "base_revision": base_revision }),
            started,
            &result,
        );
        result
    }

    fn restore_inner(
        &self,
        selector: &DocSelector,
        target_revision: &str,
        base_revision: &str,
        message: Option<String>,
    ) -> HdsResult<MutationOutcome> {
        self.ws.ensure_writable()?;
        let doc = self.resolve(selector)?;
        self.check_base_revision(&doc, base_revision)?;
        let target_content = self.revision_content(&doc, target_revision)?;
        let message = message.or_else(|| Some(format!("restore to revision {target_revision}")));
        // History is never rewritten: restoring creates a new revision whose
        // content equals the target snapshot.
        self.commit_change(
            doc,
            &target_content,
            Operation::Restore,
            Some(base_revision.to_string()),
            message,
            PatchFormat::FullSnapshot,
        )
    }

    /// Soft delete: the file is removed, history and snapshots are retained,
    /// and the descriptor is marked deleted.
    pub fn delete(
        &self,
        selector: &DocSelector,
        base_revision: &str,
        message: Option<String>,
    ) -> HdsResult<MutationOutcome> {
        let started = Instant::now();
        let result = self.delete_inner(selector, base_revision, message);
        self.audit_mutation(
            "document_delete",
            serde_json::json!({ "base_revision": base_revision }),
            started,
            &result,
        );
        result
    }

    fn delete_inner(
        &self,
        selector: &DocSelector,
        base_revision: &str,
        message: Option<String>,
    ) -> HdsResult<MutationOutcome> {
        self.ws.ensure_writable()?;
        let mut doc = self.resolve(selector)?;
        self.check_base_revision(&doc, base_revision)?;
        let now = Utc::now();
        let revision = Revision {
            revision_id: ulid::Ulid::generate().to_string(),
            parent_revision_id: Some(doc.current_revision.clone()),
            document_id: doc.document_id.clone(),
            actor: self.actor.clone(),
            operation: Operation::Delete,
            before_hash: Some(doc.content_hash.clone()),
            after_hash: None,
            message,
            created_at: now,
            patch_format: PatchFormat::FullSnapshot,
        };
        self.ws.db.insert_revision(&revision, "committed")?;
        self.ws
            .files
            .delete_document(&doc.logical_path, self.ws.config.security.follow_symlinks)?;
        doc.deleted = true;
        doc.current_revision = revision.revision_id.clone();
        doc.updated_at = now;
        self.ws.db.update_document(&doc)?;
        Ok(MutationOutcome {
            document: doc,
            revision,
            diff_summary: None,
        })
    }

    // ----- shared helpers -----

    fn revision_content(&self, doc: &Document, revision_id: &str) -> HdsResult<String> {
        let revision = self
            .ws
            .db
            .revision(revision_id)?
            .filter(|r| r.document_id == doc.document_id)
            .ok_or_else(|| HdsError::not_found(format!("revision {revision_id}")))?;
        self.ws
            .files
            .read_snapshot(&doc.document_id, &revision.revision_id)
    }

    fn check_base_revision(&self, doc: &Document, base_revision: &str) -> HdsResult<()> {
        if doc.current_revision != base_revision {
            return Err(HdsError::new(
                ErrorCode::RevisionConflict,
                "the document changed after the supplied base revision",
            )
            .with_details(serde_json::json!({
                "expected": base_revision,
                "actual": doc.current_revision,
            })));
        }
        Ok(())
    }

    fn check_size(&self, content: &str) -> HdsResult<()> {
        if content.len() as u64 > self.ws.config.limits.max_file_bytes {
            return Err(HdsError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "content exceeds limit of {} bytes",
                    self.ws.config.limits.max_file_bytes
                ),
            ));
        }
        Ok(())
    }

    fn verify_written(&self, logical: &str, expected_hash: &str) -> HdsResult<()> {
        let readback = self
            .ws
            .files
            .read_document(logical, self.ws.config.security.follow_symlinks)?;
        if content_hash(&readback) != expected_hash {
            return Err(HdsError::internal(
                "post-write verification failed: file hash mismatch",
            ));
        }
        Ok(())
    }

    /// Recoverable write protocol for an existing document (spec §13).
    fn commit_change(
        &self,
        mut doc: Document,
        new_content: &str,
        operation: Operation,
        base_revision: Option<String>,
        message: Option<String>,
        patch_format: PatchFormat,
    ) -> HdsResult<MutationOutcome> {
        let before_content = self
            .ws
            .files
            .read_document(&doc.logical_path, self.ws.config.security.follow_symlinks)?;
        let before_hash = content_hash(&before_content);
        let after_hash = content_hash(new_content);
        let now = Utc::now();
        let revision_id = ulid::Ulid::generate().to_string();
        let revision = Revision {
            revision_id: revision_id.clone(),
            parent_revision_id: base_revision.or(Some(doc.current_revision.clone())),
            document_id: doc.document_id.clone(),
            actor: self.actor.clone(),
            operation,
            before_hash: Some(before_hash.clone()),
            after_hash: Some(after_hash.clone()),
            message,
            created_at: now,
            patch_format,
        };

        // 1-3: snapshots for both sides of the write.
        self.ws
            .files
            .write_snapshot(&doc.document_id, &doc.current_revision, &before_content)?;
        self.ws
            .files
            .write_snapshot(&doc.document_id, &revision_id, new_content)?;
        // 4: pending revision record.
        self.ws.db.insert_revision(&revision, "pending")?;
        // 5: atomic file replacement.
        self.ws.files.write_document(
            &doc.logical_path,
            new_content,
            self.ws.config.security.follow_symlinks,
        )?;
        // 6: verify.
        self.verify_written(&doc.logical_path, &after_hash)?;
        // 7: finalize.
        self.ws.db.set_revision_status(&revision_id, "committed")?;
        doc.current_revision = revision_id;
        doc.content_hash = after_hash;
        doc.updated_at = now;
        doc.title = derive_title(new_content, &doc.logical_path);
        doc.index_status = IndexStatus::Stale;
        self.ws.db.update_document(&doc)?;

        // 8: reindex.
        self.reindex_after_write(&mut doc, new_content);

        let diff = diffy::create_patch(&before_content, new_content).to_string();
        Ok(MutationOutcome {
            document: doc,
            revision,
            diff_summary: Some(diff_summary(&diff)),
        })
    }

    /// Synchronous rebuild for small files; large files stay `stale` and are
    /// rebuilt lazily (on demand) instead of through a background queue.
    fn reindex_after_write(&self, doc: &mut Document, content: &str) {
        if content.len() as u64 > self.ws.config.tree.sync_index_max_bytes {
            return;
        }
        let index_service = IndexService::new(self.ws);
        match index_service.rebuild(doc, Some(content)) {
            Ok(_) => doc.index_status = IndexStatus::Ready,
            Err(e) => {
                doc.index_status = IndexStatus::Failed;
                let _ = self
                    .ws
                    .db
                    .set_index_status(&doc.document_id, IndexStatus::Failed);
                eprintln!("hds: index rebuild failed for {}: {e}", doc.logical_path);
            }
        }
    }

    fn audit_mutation(
        &self,
        operation: &str,
        arguments: serde_json::Value,
        started: Instant,
        result: &HdsResult<MutationOutcome>,
    ) {
        let latency = started.elapsed().as_millis() as u64;
        match result {
            Ok(outcome) => self.ws.record_audit(
                &self.actor,
                self.interface,
                operation,
                arguments,
                "ok",
                latency,
                Some(outcome.document.document_id.clone()),
                Some(outcome.revision.revision_id.clone()),
                None,
            ),
            Err(e) => self.ws.record_audit(
                &self.actor,
                self.interface,
                operation,
                arguments,
                "error",
                latency,
                None,
                None,
                Some(e.code.as_str().to_string()),
            ),
        }
    }
}

pub fn derive_title(content: &str, logical_path: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(t) = trimmed.strip_prefix("# ") {
            return t.trim().to_string();
        }
    }
    std::path::Path::new(logical_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| logical_path.to_string())
}

fn apply_range(content: &str, range: ReadRange) -> HdsResult<String> {
    match range {
        ReadRange::Full => Ok(content.to_string()),
        ReadRange::Lines(start, end) => {
            if start == 0 || end < start {
                return Err(HdsError::new(
                    ErrorCode::InvalidArgument,
                    "line range must be 1-based with start <= end",
                ));
            }
            let lines: Vec<&str> = content.lines().collect();
            if start > lines.len() {
                return Ok(String::new());
            }
            Ok(lines[start - 1..end.min(lines.len())].join("\n"))
        }
        ReadRange::Bytes(start, end) => {
            if end < start {
                return Err(HdsError::new(
                    ErrorCode::InvalidArgument,
                    "byte range must have start <= end",
                ));
            }
            let mut s = start.min(content.len());
            let mut e = end.min(content.len());
            while s < content.len() && !content.is_char_boundary(s) {
                s += 1;
            }
            while e > s && !content.is_char_boundary(e) {
                e -= 1;
            }
            Ok(content[s..e].to_string())
        }
    }
}

fn diff_summary(diff_text: &str) -> DiffSummary {
    let mut added = 0;
    let mut removed = 0;
    for line in diff_text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    DiffSummary {
        lines_added: added,
        lines_removed: removed,
    }
}
