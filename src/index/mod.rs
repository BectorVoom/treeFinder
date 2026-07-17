//! Tree construction plugins.
//!
//! `TreeBuilder` implementations turn one document revision into a
//! `TreeIndex`. Builders are looked up by name in `BuilderRegistry`, so
//! `tree.builder: markdown_heading_v1` in config selects the algorithm
//! without code changes.

pub mod markdown_heading_v1;

use crate::config::TreeConfig;
use crate::domain::{ErrorCode, HdsError, HdsResult, TreeIndex};
use std::collections::BTreeMap;

/// Everything a builder needs to know about the document being indexed.
pub struct BuildInput<'a> {
    pub document_id: &'a str,
    pub revision_id: &'a str,
    pub title_fallback: &'a str,
    pub content: &'a str,
    pub index_version: &'a str,
    pub config_hash: &'a str,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RebuildStats {
    pub reused_nodes: usize,
    pub rebuilt_nodes: usize,
}

pub trait TreeBuilder: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> &'static str;

    fn build(&self, input: &BuildInput<'_>, config: &TreeConfig) -> HdsResult<TreeIndex>;

    /// Rebuild against a previous index. The default implementation builds a
    /// fresh tree and reports how many nodes were reusable (same node ID and
    /// content hash), which callers surface as diagnostics.
    fn rebuild(
        &self,
        input: &BuildInput<'_>,
        previous: &TreeIndex,
        config: &TreeConfig,
    ) -> HdsResult<(TreeIndex, RebuildStats)> {
        let mut fresh = self.build(input, config)?;
        let mut stats = RebuildStats::default();
        for (id, node) in &fresh.nodes {
            match previous.nodes.get(id) {
                Some(old) if old.attributes.content_hash == node.attributes.content_hash => {
                    stats.reused_nodes += 1
                }
                _ => stats.rebuilt_nodes += 1,
            }
        }
        fresh.diagnostics.push(format!(
            "incremental: reused {} unchanged nodes, rebuilt {}",
            stats.reused_nodes, stats.rebuilt_nodes
        ));
        Ok((fresh, stats))
    }
}

#[derive(Default)]
pub struct BuilderRegistry {
    builders: BTreeMap<String, Box<dyn TreeBuilder>>,
}

impl BuilderRegistry {
    pub fn with_defaults() -> Self {
        let mut reg = BuilderRegistry::default();
        reg.register(Box::new(markdown_heading_v1::MarkdownHeadingV1));
        reg
    }

    pub fn register(&mut self, builder: Box<dyn TreeBuilder>) {
        self.builders.insert(builder.name().to_string(), builder);
    }

    pub fn get(&self, name: &str) -> HdsResult<&dyn TreeBuilder> {
        self.builders.get(name).map(|b| b.as_ref()).ok_or_else(|| {
            HdsError::new(
                ErrorCode::IndexFailed,
                format!("tree builder '{name}' is not registered"),
            )
        })
    }

    pub fn names(&self) -> Vec<String> {
        self.builders.keys().cloned().collect()
    }
}
