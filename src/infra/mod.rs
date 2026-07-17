//! Infrastructure: filesystem layout, sandboxed path resolution, atomic file
//! store, SQLite metadata repository, and the append-only audit log.

pub mod audit;
pub mod db;
pub mod file_store;
pub mod paths;

pub use audit::AuditLog;
pub use db::MetadataDb;
pub use file_store::FileStore;
pub use paths::WorkspaceLayout;
