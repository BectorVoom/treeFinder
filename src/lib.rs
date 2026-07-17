//! Hierarchical Document Store (HDS).
//!
//! A local-first Markdown document store with:
//! - file-canonical storage and derived, rebuildable tree indexes,
//! - revision history and an append-only audit log,
//! - pluggable tree builders and hierarchical search strategies,
//! - an MCP stdio server and a CLI sharing one application-service layer.
//!
//! Layering (dependencies point downward only):
//!
//! ```text
//! adapters:   cli, mcp
//! services:   services::{Workspace, DocumentService, IndexService, SearchService}
//! plugins:    index::{TreeBuilder registry}, search::{SearchStrategy registry}
//! infra:      infra::{paths, file_store, db, audit}
//! domain:     domain (pure data + errors, no IO)
//! ```

pub mod cli;
pub mod config;
pub mod domain;
pub mod index;
pub mod infra;
pub mod mcp;
pub mod search;
pub mod services;
