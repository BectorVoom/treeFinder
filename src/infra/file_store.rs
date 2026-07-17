//! Atomic file operations for canonical Markdown files, revision snapshots,
//! and persisted index JSON.

use crate::domain::{ErrorCode, HdsError, HdsResult, TreeIndex};
use crate::infra::paths::WorkspaceLayout;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

#[derive(Debug, Clone)]
pub struct FileStore {
    layout: WorkspaceLayout,
}

impl FileStore {
    pub fn new(layout: WorkspaceLayout) -> Self {
        FileStore { layout }
    }

    /// Write `content` to `target` atomically: temp file in the same
    /// directory, fsync, then rename over the target.
    pub fn atomic_write(&self, target: &Path, content: &str) -> HdsResult<()> {
        let parent = target
            .parent()
            .ok_or_else(|| HdsError::internal("target has no parent directory"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".hds-tmp-{}-{}",
            std::process::id(),
            ulid::Ulid::generate()
        ));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, target)?;
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    pub fn read_document(
        &self,
        normalized_logical: &str,
        follow_symlinks: bool,
    ) -> HdsResult<String> {
        let path = self
            .layout
            .physical_document_path(normalized_logical, follow_symlinks)?;
        std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                HdsError::not_found(format!("file {normalized_logical}"))
            }
            _ => HdsError::internal(format!("cannot read {normalized_logical}: {e}")),
        })
    }

    pub fn write_document(
        &self,
        normalized_logical: &str,
        content: &str,
        follow_symlinks: bool,
    ) -> HdsResult<()> {
        let path = self
            .layout
            .physical_document_path(normalized_logical, follow_symlinks)?;
        self.atomic_write(&path, content)
    }

    pub fn document_exists(
        &self,
        normalized_logical: &str,
        follow_symlinks: bool,
    ) -> HdsResult<bool> {
        let path = self
            .layout
            .physical_document_path(normalized_logical, follow_symlinks)?;
        Ok(path.is_file())
    }

    pub fn delete_document(
        &self,
        normalized_logical: &str,
        follow_symlinks: bool,
    ) -> HdsResult<()> {
        let path = self
            .layout
            .physical_document_path(normalized_logical, follow_symlinks)?;
        std::fs::remove_file(&path)
            .map_err(|e| HdsError::internal(format!("cannot delete {normalized_logical}: {e}")))
    }

    /// Persist an immutable revision snapshot (idempotent).
    pub fn write_snapshot(
        &self,
        document_id: &str,
        revision_id: &str,
        content: &str,
    ) -> HdsResult<()> {
        let path = self.layout.revision_path(document_id, revision_id);
        if path.is_file() {
            return Ok(());
        }
        self.atomic_write(&path, content)
    }

    pub fn read_snapshot(&self, document_id: &str, revision_id: &str) -> HdsResult<String> {
        let path = self.layout.revision_path(document_id, revision_id);
        std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                HdsError::not_found(format!("revision snapshot {revision_id}"))
            }
            _ => HdsError::internal(format!("cannot read snapshot {revision_id}: {e}")),
        })
    }

    pub fn write_index(&self, index: &TreeIndex) -> HdsResult<()> {
        let path = self
            .layout
            .index_path(&index.document_id, &index.index_version);
        let json = serde_json::to_string_pretty(index)?;
        self.atomic_write(&path, &json)
    }

    pub fn read_index(&self, document_id: &str, index_version: &str) -> HdsResult<TreeIndex> {
        let path = self.layout.index_path(document_id, index_version);
        let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                HdsError::not_found(format!("index {index_version} for document {document_id}"))
            }
            _ => HdsError::internal(format!("cannot read index: {e}")),
        })?;
        serde_json::from_str(&text).map_err(|e| {
            HdsError::new(
                ErrorCode::IndexFailed,
                format!("stored index is corrupt: {e}"),
            )
        })
    }
}
