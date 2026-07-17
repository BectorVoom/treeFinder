//! Baseline tree builder: headings become sections, body blocks attach to the
//! closest preceding heading, and long unheaded ranges are split into
//! deterministic synthetic groups. Node IDs are derived from the document
//! UUID, normalized heading ancestry, node kind, and a local occurrence
//! index, so they stay stable when unrelated parts of the document change.

use crate::config::TreeConfig;
use crate::domain::{HdsResult, NodeAttributes, NodeKind, SourceSpan, TreeIndex, TreeNode};
use crate::index::{BuildInput, TreeBuilder};
use crate::infra::file_store::content_hash;
use chrono::Utc;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

pub struct MarkdownHeadingV1;

#[derive(Debug, Clone)]
struct Heading {
    level: usize,
    text: String,
    span: Range<usize>,
}

#[derive(Debug, Clone)]
struct Block {
    span: Range<usize>,
}

impl TreeBuilder for MarkdownHeadingV1 {
    fn name(&self) -> &'static str {
        "markdown_heading_v1"
    }

    fn version(&self) -> &'static str {
        "1.0.0"
    }

    fn build(&self, input: &BuildInput<'_>, config: &TreeConfig) -> HdsResult<TreeIndex> {
        let content = input.content;
        let mut diagnostics = Vec::new();

        let front_matter_end = front_matter_end(content);
        let body = &content[front_matter_end..];
        let (mut headings, blocks) = parse_structure(body);
        for h in &mut headings {
            h.span = h.span.start + front_matter_end..h.span.end + front_matter_end;
        }
        let blocks: Vec<Block> = blocks
            .into_iter()
            .map(|b| Block {
                span: b.span.start + front_matter_end..b.span.end + front_matter_end,
            })
            .collect();

        let lines = LineIndex::new(content);
        let doc_title = headings
            .iter()
            .find(|h| h.level == 1)
            .map(|h| h.text.clone())
            .unwrap_or_else(|| input.title_fallback.to_string());

        let mut builder = TreeAssembler {
            document_id: input.document_id,
            content,
            lines: &lines,
            config,
            nodes: BTreeMap::new(),
            id_occurrences: HashMap::new(),
            diagnostics: &mut diagnostics,
        };

        let root_id = builder.assemble(&doc_title, &headings, &blocks);

        Ok(TreeIndex {
            document_id: input.document_id.to_string(),
            index_version: input.index_version.to_string(),
            builder: self.name().to_string(),
            builder_version: self.version().to_string(),
            config_hash: input.config_hash.to_string(),
            revision_id: input.revision_id.to_string(),
            created_at: Utc::now(),
            root_id,
            nodes: builder.nodes,
            diagnostics,
        })
    }
}

/// Byte length of a leading YAML front-matter block, or 0.
fn front_matter_end(content: &str) -> usize {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return 0;
    }
    let mut offset = content.find('\n').map(|i| i + 1).unwrap_or(0);
    for line in content[offset..].split_inclusive('\n') {
        let trimmed = line.trim_end();
        offset += line.len();
        if trimmed == "---" || trimmed == "..." {
            return offset;
        }
    }
    0 // unterminated front matter: treat as normal content
}

/// Collect headings (with text and full spans) and top-level non-heading
/// blocks using pulldown-cmark's offset iterator, so headings inside code
/// blocks are not misparsed.
fn parse_structure(body: &str) -> (Vec<Heading>, Vec<Block>) {
    let mut headings = Vec::new();
    let mut blocks = Vec::new();
    let mut container_depth = 0usize;
    let mut heading_text: Option<String> = None;
    let mut heading_span = 0..0;
    let mut heading_level = 1usize;

    let parser = Parser::new_ext(body, Options::ENABLE_TABLES | Options::ENABLE_FOOTNOTES)
        .into_offset_iter();
    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) if container_depth == 0 => {
                heading_text = Some(String::new());
                heading_span = range;
                heading_level = level as usize;
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(text) = heading_text.take() {
                    headings.push(Heading {
                        level: heading_level,
                        text: normalize_ws(&text),
                        span: heading_span.clone(),
                    });
                }
            }
            Event::Text(t) => {
                if let Some(buf) = heading_text.as_mut() {
                    buf.push_str(&t);
                }
            }
            // Keep inline-code delimiters so titles match the source markdown.
            Event::Code(t) => {
                if let Some(buf) = heading_text.as_mut() {
                    buf.push('`');
                    buf.push_str(&t);
                    buf.push('`');
                }
            }
            Event::Start(tag) if container_depth == 0 && heading_text.is_none() => {
                if is_block_tag(&tag) {
                    blocks.push(Block { span: range });
                    container_depth += 1;
                }
            }
            Event::Start(tag) => {
                if is_block_tag(&tag) {
                    container_depth += 1;
                }
            }
            Event::End(tag_end) if is_block_tag_end(&tag_end) => {
                container_depth = container_depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    (headings, blocks)
}

fn is_block_tag(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::CodeBlock(_)
            | Tag::List(_)
            | Tag::BlockQuote(_)
            | Tag::Table(_)
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
    )
}

fn is_block_tag_end(tag: &TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Paragraph
            | TagEnd::CodeBlock
            | TagEnd::List(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::Table
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
    )
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(content: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in content.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineIndex { line_starts }
    }

    /// 1-based line containing byte offset.
    fn line_of(&self, byte: usize) -> usize {
        match self.line_starts.binary_search(&byte) {
            Ok(i) => i + 1,
            Err(i) => i,
        }
    }

    fn span(&self, range: &Range<usize>) -> (usize, usize) {
        let start = self.line_of(range.start);
        let end = self.line_of(range.end.saturating_sub(1).max(range.start));
        (start, end.max(start))
    }
}

struct TreeAssembler<'a> {
    document_id: &'a str,
    content: &'a str,
    lines: &'a LineIndex,
    config: &'a TreeConfig,
    nodes: BTreeMap<String, TreeNode>,
    id_occurrences: HashMap<String, usize>,
    diagnostics: &'a mut Vec<String>,
}

impl<'a> TreeAssembler<'a> {
    fn assemble(&mut self, doc_title: &str, headings: &[Heading], blocks: &[Block]) -> String {
        // Section end = start of the next heading at the same or higher level.
        let mut section_ends = vec![self.content.len(); headings.len()];
        for i in 0..headings.len() {
            for j in (i + 1)..headings.len() {
                if headings[j].level <= headings[i].level {
                    section_ends[i] = headings[j].span.start;
                    break;
                }
            }
        }

        let root_id = self.make_node_id(&[], NodeKind::Document, doc_title);
        // Heading index -> node id; usize::MAX marks the root.
        const ROOT: usize = usize::MAX;
        let mut node_ids: Vec<String> = Vec::with_capacity(headings.len());
        // Stack of (heading index, level); root is level 0.
        let mut stack: Vec<(usize, usize)> = vec![(ROOT, 0)];
        let mut ancestry: Vec<String> = Vec::new();

        let mut children_of: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut parent_of: Vec<usize> = vec![ROOT; headings.len()];
        let mut heading_paths: Vec<Vec<String>> = Vec::with_capacity(headings.len());

        for (i, h) in headings.iter().enumerate() {
            while let Some(&(_, lvl)) = stack.last() {
                if lvl >= h.level && stack.len() > 1 {
                    stack.pop();
                    ancestry.pop();
                } else {
                    break;
                }
            }
            let &(parent_idx, parent_level) = stack.last().expect("root always present");
            if h.level > parent_level + 1 {
                self.diagnostics.push(format!(
                    "heading level skip: '{}' (H{}) attached to level-{} ancestor",
                    h.text, h.level, parent_level
                ));
            }
            parent_of[i] = parent_idx;
            children_of.entry(parent_idx).or_default().push(i);
            let path: Vec<String> = ancestry.clone();
            let id = self.make_node_id(&path, heading_kind(h.level), &h.text);
            node_ids.push(id);
            heading_paths.push(path);
            stack.push((i, h.level));
            ancestry.push(h.text.clone());
        }

        // Assign each top-level block to the innermost section containing it.
        let mut own_blocks: HashMap<usize, Vec<&Block>> = HashMap::new();
        for block in blocks {
            let mut owner = ROOT;
            for (i, h) in headings.iter().enumerate() {
                if h.span.start < block.span.start && block.span.end <= section_ends[i] {
                    owner = i; // headings are in order, so the last match is innermost
                }
            }
            own_blocks.entry(owner).or_default().push(block);
        }

        // Create nodes bottom-up is unnecessary; create heading nodes, then
        // synthetic groups, then wire children sorted by position.
        for (i, h) in headings.iter().enumerate() {
            let span = h.span.start..section_ends[i];
            let (start_line, end_line) = self.lines.span(&span);
            let text = &self.content[span.clone()];
            let mut hp = heading_paths[i].clone();
            hp.push(h.text.clone());
            let node = TreeNode {
                node_id: node_ids[i].clone(),
                parent_id: Some(if parent_of[i] == ROOT {
                    root_id.clone()
                } else {
                    node_ids[parent_of[i]].clone()
                }),
                kind: heading_kind(h.level),
                level: h.level,
                title: h.text.clone(),
                summary: self.extract_summary(i, h, section_ends[i], &own_blocks),
                source: SourceSpan {
                    start_line,
                    end_line,
                    start_byte: span.start,
                    end_byte: span.end,
                },
                children: Vec::new(),
                attributes: NodeAttributes {
                    heading_path: hp,
                    word_count: text.split_whitespace().count(),
                    content_hash: content_hash(text),
                },
            };
            self.nodes.insert(node.node_id.clone(), node);
        }

        // Synthetic groups for long unheaded ranges.
        let mut synthetic_children: HashMap<usize, Vec<(usize, String)>> = HashMap::new();
        // Deterministic order: document position first, root (usize::MAX) last.
        let mut owners: Vec<usize> = own_blocks.keys().copied().collect();
        owners.sort();
        for owner in owners {
            let blocks = own_blocks[&owner].clone();
            if blocks.len() <= self.config.synthetic_group_paragraphs {
                continue;
            }
            let (parent_node_id, parent_level, parent_path) = if owner == ROOT {
                (root_id.clone(), 0usize, Vec::new())
            } else {
                let mut p = heading_paths[owner].clone();
                p.push(headings[owner].text.clone());
                (node_ids[owner].clone(), headings[owner].level, p)
            };
            for group in self.chunk_blocks(&blocks) {
                let span = group.first().unwrap().span.start..group.last().unwrap().span.end;
                let text = &self.content[span.clone()];
                let title = synthetic_title(text);
                let id = self.make_node_id(&parent_path, NodeKind::SyntheticGroup, &title);
                let (start_line, end_line) = self.lines.span(&span);
                let node = TreeNode {
                    node_id: id.clone(),
                    parent_id: Some(parent_node_id.clone()),
                    kind: NodeKind::SyntheticGroup,
                    level: parent_level + 1,
                    title,
                    summary: Some(extractive_summary(text, self.config.summary_max_words)),
                    source: SourceSpan {
                        start_line,
                        end_line,
                        start_byte: span.start,
                        end_byte: span.end,
                    },
                    children: Vec::new(),
                    attributes: NodeAttributes {
                        heading_path: parent_path.clone(),
                        word_count: text.split_whitespace().count(),
                        content_hash: content_hash(text),
                    },
                };
                synthetic_children
                    .entry(owner)
                    .or_default()
                    .push((span.start, id.clone()));
                self.nodes.insert(id, node);
            }
        }

        // Compute children (heading children + synthetic groups) by position.
        let kids_for = |owner: usize| -> Vec<String> {
            let mut kids: Vec<(usize, String)> = Vec::new();
            if let Some(hs) = children_of.get(&owner) {
                for &ci in hs {
                    kids.push((headings[ci].span.start, node_ids[ci].clone()));
                }
            }
            if let Some(sg) = synthetic_children.get(&owner) {
                kids.extend(sg.iter().cloned());
            }
            kids.sort();
            kids.into_iter().map(|(_, id)| id).collect()
        };
        let mut wiring: Vec<(String, Vec<String>)> = Vec::new();
        for (i, id) in node_ids.iter().enumerate() {
            wiring.push((id.clone(), kids_for(i)));
        }
        wiring.push((root_id.clone(), kids_for(ROOT)));

        // Root node covers the whole document.
        let whole = 0..self.content.len();
        let (start_line, end_line) = self.lines.span(&whole);
        let root_summary = extractive_summary(
            own_blocks
                .get(&ROOT)
                .and_then(|b| b.first())
                .map(|b| &self.content[b.span.clone()])
                .unwrap_or(""),
            self.config.summary_max_words,
        );
        let root = TreeNode {
            node_id: root_id.clone(),
            parent_id: None,
            kind: NodeKind::Document,
            level: 0,
            title: doc_title.to_string(),
            summary: (!root_summary.is_empty()).then_some(root_summary),
            source: SourceSpan {
                start_line,
                end_line,
                start_byte: 0,
                end_byte: self.content.len(),
            },
            children: Vec::new(),
            attributes: NodeAttributes {
                heading_path: vec![],
                word_count: self.content.split_whitespace().count(),
                content_hash: content_hash(self.content),
            },
        };
        self.nodes.insert(root_id.clone(), root);
        for (owner_id, kids) in wiring {
            if let Some(node) = self.nodes.get_mut(&owner_id) {
                node.children = kids;
            }
        }

        root_id
    }

    fn chunk_blocks<'b>(&self, blocks: &[&'b Block]) -> Vec<Vec<&'b Block>> {
        let mut groups = Vec::new();
        let mut current: Vec<&Block> = Vec::new();
        let mut words = 0usize;
        for b in blocks {
            let block_words = self.content[b.span.clone()].split_whitespace().count();
            let over = !current.is_empty()
                && (current.len() >= self.config.synthetic_group_paragraphs
                    || words + block_words > self.config.synthetic_group_max_words);
            if over {
                groups.push(std::mem::take(&mut current));
                words = 0;
            }
            current.push(b);
            words += block_words;
        }
        if !current.is_empty() {
            groups.push(current);
        }
        groups
    }

    fn extract_summary(
        &self,
        idx: usize,
        _h: &Heading,
        _section_end: usize,
        own_blocks: &HashMap<usize, Vec<&Block>>,
    ) -> Option<String> {
        let blocks = own_blocks.get(&idx)?;
        let first = blocks.first()?;
        let text = &self.content[first.span.clone()];
        let s = extractive_summary(text, self.config.summary_max_words);
        (!s.is_empty()).then_some(s)
    }

    fn make_node_id(&mut self, ancestry: &[String], kind: NodeKind, title: &str) -> String {
        let key = format!(
            "{}\u{1f}{}\u{1f}{:?}\u{1f}{}",
            self.document_id,
            ancestry.join("\u{1f}"),
            kind,
            title
        );
        let occurrence = {
            let counter = self.id_occurrences.entry(key.clone()).or_insert(0);
            let v = *counter;
            *counter += 1;
            v
        };
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        hasher.update([0x1f]);
        hasher.update(occurrence.to_string().as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("n_{hex}")
    }
}

fn heading_kind(level: usize) -> NodeKind {
    if level <= 1 {
        NodeKind::Section
    } else {
        NodeKind::Subsection
    }
}

fn synthetic_title(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().take(6).collect();
    if words.is_empty() {
        "Untitled block".to_string()
    } else {
        format!("{}…", words.join(" "))
    }
}

/// First words of the text, cut at a word boundary.
fn extractive_summary(text: &str, max_words: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().take(max_words + 1).collect();
    if words.is_empty() {
        return String::new();
    }
    if words.len() > max_words {
        format!("{}…", words[..max_words].join(" "))
    } else {
        words.join(" ")
    }
}
