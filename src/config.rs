//! Workspace configuration (`.hds/config.yaml`).
//!
//! Selection of tree builder and search strategy is configuration-driven so
//! algorithms can be swapped without touching adapters or repositories.

use crate::domain::{ErrorCode, HdsError, HdsResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub tree: TreeConfig,
    pub search: SearchConfig,
    pub limits: Limits,
    pub security: SecurityConfig,
}

impl Default for Config {
    fn default() -> Self {
        let mut strategies = BTreeMap::new();
        strategies.insert(
            "beam_tree_v1".to_string(),
            serde_yaml::to_value(BeamTreeOptions::default()).expect("static default"),
        );
        strategies.insert(
            "exhaustive_tree_v1".to_string(),
            serde_yaml::Value::Mapping(
                [(
                    serde_yaml::Value::from("max_nodes_visited"),
                    serde_yaml::Value::from(5000_u64),
                )]
                .into_iter()
                .collect(),
            ),
        );
        Config {
            tree: TreeConfig::default(),
            search: SearchConfig {
                default_strategy: "beam_tree_v1".to_string(),
                allow_request_override: true,
                experimental_strategies_enabled: false,
                strategies,
            },
            limits: Limits::default(),
            security: SecurityConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TreeConfig {
    pub builder: String,
    /// Paragraph count above which unheaded ranges are split into synthetic groups.
    pub synthetic_group_paragraphs: usize,
    /// Word budget per synthetic group.
    pub synthetic_group_max_words: usize,
    /// Maximum words used for extractive summaries.
    pub summary_max_words: usize,
    /// Files at or below this byte size are indexed synchronously.
    pub sync_index_max_bytes: u64,
}

impl Default for TreeConfig {
    fn default() -> Self {
        TreeConfig {
            builder: "markdown_heading_v1".to_string(),
            synthetic_group_paragraphs: 6,
            synthetic_group_max_words: 400,
            summary_max_words: 48,
            sync_index_max_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub default_strategy: String,
    pub allow_request_override: bool,
    pub experimental_strategies_enabled: bool,
    /// Per-strategy options as free-form YAML; each strategy validates its own.
    pub strategies: BTreeMap<String, serde_yaml::Value>,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Config::default().search
    }
}

/// Options for the baseline beam strategy (spec §9.3 example).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BeamTreeOptions {
    pub beam_width: usize,
    pub max_depth: usize,
    pub max_nodes_visited: usize,
    pub expand_threshold: f64,
    pub weights: ScoreWeights,
}

impl Default for BeamTreeOptions {
    fn default() -> Self {
        BeamTreeOptions {
            beam_width: 8,
            max_depth: 8,
            max_nodes_visited: 200,
            expand_threshold: 0.12,
            weights: ScoreWeights::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoreWeights {
    pub title: f64,
    pub body: f64,
    pub summary: f64,
    pub path: f64,
    pub prior: f64,
    pub plugin: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        ScoreWeights {
            title: 0.30,
            body: 0.25,
            summary: 0.20,
            path: 0.20,
            prior: 0.05,
            plugin: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_patch_bytes: u64,
    pub max_query_chars: usize,
    pub max_top_k: usize,
    pub max_nodes_visited: usize,
    pub max_list_limit: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 10 * 1024 * 1024,
            max_patch_bytes: 2 * 1024 * 1024,
            max_query_chars: 2000,
            max_top_k: 50,
            max_nodes_visited: 10_000,
            max_list_limit: 500,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SecurityConfig {
    pub read_only: bool,
    pub follow_symlinks: bool,
    /// When non-empty, only these MCP tools are exposed.
    pub mcp_tool_allowlist: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> HdsResult<Config> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            HdsError::internal(format!("cannot read config {}: {e}", path.display()))
        })?;
        serde_yaml::from_str(&text)
            .map_err(|e| HdsError::new(ErrorCode::InvalidArgument, format!("invalid config: {e}")))
    }

    pub fn save(&self, path: &Path) -> HdsResult<()> {
        let text = serde_yaml::to_string(self)
            .map_err(|e| HdsError::internal(format!("cannot serialize config: {e}")))?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Hash of the effective options of one strategy or builder, recorded in
    /// every index and search run for reproducibility.
    pub fn options_hash(options: &serde_json::Value) -> String {
        let canonical = serde_json::to_string(options).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex}")
    }

    pub fn strategy_options(&self, name: &str) -> serde_json::Value {
        self.search
            .strategies
            .get(name)
            .and_then(|v| serde_yaml::from_value::<serde_json::Value>(v.clone()).ok())
            .unwrap_or(serde_json::Value::Null)
    }
}
