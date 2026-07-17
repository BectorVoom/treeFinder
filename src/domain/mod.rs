//! Pure domain model: data types and errors. No IO in this module.

mod error;

pub use error::{ErrorCode, HdsError, HdsResult};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Stable document descriptor. The UUID survives renames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub document_id: String,
    pub logical_path: String,
    pub title: String,
    pub current_revision: String,
    pub content_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub index_status: IndexStatus,
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexStatus {
    Ready,
    Stale,
    Building,
    Failed,
}

impl IndexStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexStatus::Ready => "ready",
            IndexStatus::Stale => "stale",
            IndexStatus::Building => "building",
            IndexStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> IndexStatus {
        match s {
            "ready" => IndexStatus::Ready,
            "building" => IndexStatus::Building,
            "failed" => IndexStatus::Failed,
            _ => IndexStatus::Stale,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Document,
    Section,
    Subsection,
    SyntheticGroup,
    ParagraphRange,
}

/// Byte and line span of a node within its document revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAttributes {
    pub heading_path: Vec<String>,
    pub word_count: usize,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeNode {
    pub node_id: String,
    pub parent_id: Option<String>,
    pub kind: NodeKind,
    pub level: usize,
    pub title: String,
    pub summary: Option<String>,
    pub source: SourceSpan,
    pub children: Vec<String>,
    pub attributes: NodeAttributes,
}

/// A persisted, derived tree index for one document revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeIndex {
    pub document_id: String,
    pub index_version: String,
    pub builder: String,
    pub builder_version: String,
    pub config_hash: String,
    pub revision_id: String,
    pub created_at: DateTime<Utc>,
    pub root_id: String,
    /// All nodes keyed by node_id. BTreeMap keeps serialization deterministic.
    pub nodes: BTreeMap<String, TreeNode>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

impl TreeIndex {
    pub fn node(&self, id: &str) -> Option<&TreeNode> {
        self.nodes.get(id)
    }

    pub fn root(&self) -> &TreeNode {
        &self.nodes[&self.root_id]
    }

    /// Heading ancestry from root (exclusive) to the node (inclusive).
    pub fn node_path(&self, id: &str) -> Vec<String> {
        let mut path = Vec::new();
        let mut cur = self.nodes.get(id);
        while let Some(n) = cur {
            path.push(n.title.clone());
            cur = n.parent_id.as_ref().and_then(|p| self.nodes.get(p));
        }
        path.reverse();
        path
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    McpClient,
    Cli,
    System,
}

impl ActorType {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorType::McpClient => "mcp_client",
            ActorType::Cli => "cli",
            ActorType::System => "system",
        }
    }

    pub fn parse(s: &str) -> ActorType {
        match s {
            "mcp_client" => ActorType::McpClient,
            "cli" => ActorType::Cli,
            _ => ActorType::System,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    #[serde(rename = "type")]
    pub actor_type: ActorType,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Create,
    Replace,
    Patch,
    Rename,
    Delete,
    Restore,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Create => "create",
            Operation::Replace => "replace",
            Operation::Patch => "patch",
            Operation::Rename => "rename",
            Operation::Delete => "delete",
            Operation::Restore => "restore",
        }
    }

    pub fn parse(s: &str) -> Operation {
        match s {
            "create" => Operation::Create,
            "replace" => Operation::Replace,
            "patch" => Operation::Patch,
            "rename" => Operation::Rename,
            "delete" => Operation::Delete,
            _ => Operation::Restore,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchFormat {
    UnifiedDiff,
    JsonPatch,
    FullSnapshot,
}

impl PatchFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            PatchFormat::UnifiedDiff => "unified_diff",
            PatchFormat::JsonPatch => "json_patch",
            PatchFormat::FullSnapshot => "full_snapshot",
        }
    }

    pub fn parse(s: &str) -> Option<PatchFormat> {
        match s {
            "unified_diff" => Some(PatchFormat::UnifiedDiff),
            "json_patch" => Some(PatchFormat::JsonPatch),
            "full_snapshot" => Some(PatchFormat::FullSnapshot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub revision_id: String,
    pub parent_revision_id: Option<String>,
    pub document_id: String,
    pub actor: Actor,
    pub operation: Operation,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub patch_format: PatchFormat,
}

/// One append-only audit record. Never contains document content or secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub event_id: String,
    pub created_at: DateTime<Utc>,
    pub actor: Actor,
    pub interface: String,
    pub operation: String,
    pub arguments: serde_json::Value,
    pub status: String,
    pub latency_ms: u64,
    pub document_id: Option<String>,
    pub revision_id: Option<String>,
    pub error_code: Option<String>,
}

/// Per-signal score decomposition so every ranking is explainable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub title: f64,
    pub body: f64,
    pub summary: f64,
    pub path: f64,
    pub prior: f64,
    pub plugin: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub document_id: String,
    pub logical_path: String,
    pub node_id: String,
    pub node_path: Vec<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub excerpt: String,
    pub score: ScoreBreakdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceStep {
    pub step: usize,
    pub node_id: String,
    pub depth: usize,
    pub action: String,
    pub score: ScoreBreakdown,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub search_run_id: String,
    pub query: String,
    pub strategy: String,
    pub strategy_version: String,
    pub config_hash: String,
    /// (document_id, revision_id, index_version) triples consulted.
    pub sources: Vec<SearchSource>,
    pub results: Vec<SearchHit>,
    pub trace: Vec<TraceStep>,
    pub nodes_visited: usize,
    pub elapsed_ms: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSource {
    pub document_id: String,
    pub revision_id: String,
    pub index_version: String,
    pub stale: bool,
}
