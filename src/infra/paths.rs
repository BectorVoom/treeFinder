//! Workspace layout and logical-path sandboxing.
//!
//! Every externally supplied document path is a *logical path* such as
//! `notes/design.md`. It is validated lexically (no absolute paths, no `..`,
//! no `.hds`, Markdown extension only) and then re-checked physically so that
//! symlinks cannot escape the workspace.

use crate::domain::{ErrorCode, HdsError, HdsResult};
use std::path::{Component, Path, PathBuf};

pub const HDS_DIR: &str = ".hds";
pub const DOCUMENTS_DIR: &str = "documents";

#[derive(Debug, Clone)]
pub struct WorkspaceLayout {
    root: PathBuf,
}

impl WorkspaceLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        WorkspaceLayout { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn documents_dir(&self) -> PathBuf {
        self.root.join(DOCUMENTS_DIR)
    }

    pub fn hds_dir(&self) -> PathBuf {
        self.root.join(HDS_DIR)
    }

    pub fn config_path(&self) -> PathBuf {
        self.hds_dir().join("config.yaml")
    }

    pub fn db_path(&self) -> PathBuf {
        self.hds_dir().join("metadata.sqlite3")
    }

    pub fn revisions_dir(&self, document_id: &str) -> PathBuf {
        self.hds_dir().join("revisions").join(document_id)
    }

    pub fn revision_path(&self, document_id: &str, revision_id: &str) -> PathBuf {
        self.revisions_dir(document_id)
            .join(format!("{revision_id}.md"))
    }

    pub fn indexes_dir(&self, document_id: &str) -> PathBuf {
        self.hds_dir().join("indexes").join(document_id)
    }

    pub fn index_path(&self, document_id: &str, index_version: &str) -> PathBuf {
        self.indexes_dir(document_id)
            .join(format!("{index_version}.json"))
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.hds_dir().join("logs")
    }

    pub fn audit_log_path(&self) -> PathBuf {
        self.logs_dir().join("audit.jsonl")
    }

    pub fn is_initialized(&self) -> bool {
        self.config_path().is_file() && self.db_path().is_file()
    }

    pub fn create_directories(&self) -> HdsResult<()> {
        std::fs::create_dir_all(self.documents_dir())?;
        std::fs::create_dir_all(self.hds_dir().join("revisions"))?;
        std::fs::create_dir_all(self.hds_dir().join("indexes"))?;
        std::fs::create_dir_all(self.logs_dir())?;
        Ok(())
    }

    /// Validate and normalize a logical document path (lexical checks).
    pub fn normalize_logical_path(&self, logical: &str) -> HdsResult<String> {
        let logical = logical.trim();
        if logical.is_empty() {
            return Err(HdsError::invalid_path("empty document path"));
        }
        if logical.contains('\0') || logical.chars().any(|c| c.is_control()) {
            return Err(HdsError::invalid_path("path contains control characters"));
        }
        let unified = logical.replace('\\', "/");
        let p = Path::new(&unified);
        if p.is_absolute() || unified.starts_with('/') || unified.starts_with('~') {
            return Err(HdsError::invalid_path("absolute paths are not allowed"));
        }
        let mut parts: Vec<String> = Vec::new();
        for comp in p.components() {
            match comp {
                Component::Normal(part) => {
                    let part = part
                        .to_str()
                        .ok_or_else(|| HdsError::invalid_path("path is not valid UTF-8"))?;
                    if part == HDS_DIR {
                        return Err(HdsError::invalid_path(
                            "internal .hds paths are not addressable",
                        ));
                    }
                    parts.push(part.to_string());
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(HdsError::invalid_path(
                        "parent traversal ('..') is not allowed",
                    ));
                }
                _ => return Err(HdsError::invalid_path("unsupported path component")),
            }
        }
        if parts.is_empty() {
            return Err(HdsError::invalid_path("empty document path"));
        }
        let joined = parts.join("/");
        let lower = joined.to_ascii_lowercase();
        if !(lower.ends_with(".md") || lower.ends_with(".markdown")) {
            return Err(HdsError::invalid_path(
                "only Markdown documents (.md, .markdown) are supported",
            ));
        }
        Ok(joined)
    }

    /// Resolve a normalized logical path to an absolute filesystem path and
    /// verify that no existing component is a symlink escaping the workspace.
    pub fn physical_document_path(
        &self,
        normalized_logical: &str,
        follow_symlinks: bool,
    ) -> HdsResult<PathBuf> {
        let full = self.documents_dir().join(normalized_logical);
        if !follow_symlinks {
            // Walk each existing ancestor and the file itself: reject symlinks.
            let mut cur = self.documents_dir();
            for part in Path::new(normalized_logical).components() {
                if let Component::Normal(p) = part {
                    cur = cur.join(p);
                    match std::fs::symlink_metadata(&cur) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            return Err(HdsError::invalid_path(format!(
                                "symlink in document path: {}",
                                normalized_logical
                            )));
                        }
                        _ => {}
                    }
                }
            }
        }
        // Containment double-check on the canonical parent, if it exists.
        if let Some(parent) = full.parent()
            && parent.exists()
        {
            let canon_parent = parent
                .canonicalize()
                .map_err(|e| HdsError::internal(format!("canonicalize failed: {e}")))?;
            let canon_docs = self
                .documents_dir()
                .canonicalize()
                .map_err(|e| HdsError::internal(format!("canonicalize failed: {e}")))?;
            if !canon_parent.starts_with(&canon_docs) {
                return Err(HdsError::new(
                    ErrorCode::PermissionDenied,
                    "resolved path escapes the workspace",
                ));
            }
        }
        Ok(full)
    }
}
