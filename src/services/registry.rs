//! Multi-workspace registry for long-lived adapters (the MCP server).
//!
//! The CLI opens one workspace per invocation; the MCP server is long-lived
//! and may be asked to operate on several workspaces. The registry opens
//! workspaces on demand, keyed by canonical root, and keeps them open for
//! the lifetime of the server.

use super::Workspace;
use crate::domain::{ErrorCode, HdsError, HdsResult};
use crate::infra::paths::TREEFINDER_DIR;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Walk up from `start` to the nearest ancestor containing `.treefinder/config.yaml`.
pub fn find_workspace_root(start: &Path) -> HdsResult<PathBuf> {
    let mut dir = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(HdsError::internal)?
            .join(start)
    };
    loop {
        if dir.join(TREEFINDER_DIR).join("config.yaml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(HdsError::not_found(format!(
                "workspace at or above {} (run `hds init` there first)",
                start.display()
            )));
        }
    }
}

pub struct WorkspaceRegistry {
    /// Canonical root of the workspace used when a request names none.
    default_root: Option<PathBuf>,
    /// Open workspaces by canonical root, in deterministic order.
    open: BTreeMap<PathBuf, Workspace>,
}

impl WorkspaceRegistry {
    /// A registry with no default: every request must name a workspace.
    pub fn new() -> Self {
        WorkspaceRegistry {
            default_root: None,
            open: BTreeMap::new(),
        }
    }

    /// A registry whose default is `ws` (requests without a `workspace`
    /// argument go there).
    pub fn with_default(ws: Workspace) -> Self {
        let root = ws
            .layout
            .root()
            .canonicalize()
            .unwrap_or_else(|_| ws.layout.root().to_path_buf());
        WorkspaceRegistry {
            default_root: Some(root.clone()),
            open: BTreeMap::from([(root, ws)]),
        }
    }

    pub fn default_root(&self) -> Option<&Path> {
        self.default_root.as_deref()
    }

    pub fn default_workspace(&self) -> Option<&Workspace> {
        self.default_root.as_ref().and_then(|r| self.open.get(r))
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.open.keys().cloned().collect()
    }

    pub fn get(&self, root: &Path) -> Option<&Workspace> {
        self.open.get(root)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, &Workspace)> {
        self.open.iter()
    }

    /// Open (or return the already-open) workspace containing `path`.
    pub fn open_containing(&mut self, path: &Path) -> HdsResult<&Workspace> {
        let root = find_workspace_root(path)?;
        let root = root
            .canonicalize()
            .map_err(|e| HdsError::internal(format!("canonicalize failed: {e}")))?;
        if !self.open.contains_key(&root) {
            let ws = Workspace::open(&root)?;
            // stderr is the logging channel for a stdio MCP server; recovery
            // messages contain only normalized paths (no control characters).
            for msg in &ws.recovery_report {
                eprintln!("recovery [{}]: {msg}", root.display());
            }
            self.open.insert(root.clone(), ws);
        }
        Ok(self.open.get(&root).expect("workspace just opened"))
    }

    /// Resolve a request's optional `workspace` argument to an open workspace.
    pub fn resolve(&mut self, workspace: Option<&str>) -> HdsResult<&Workspace> {
        match workspace {
            Some(p) => self.open_containing(Path::new(p)),
            None => {
                let root = self.default_root.clone().ok_or_else(|| {
                    HdsError::new(
                        ErrorCode::InvalidArgument,
                        "this server has no default workspace; pass 'workspace' \
                         with a path to (or inside) a workspace root",
                    )
                })?;
                Ok(self
                    .open
                    .get(&root)
                    .expect("default workspace is always open"))
            }
        }
    }
}

impl Default for WorkspaceRegistry {
    fn default() -> Self {
        Self::new()
    }
}
