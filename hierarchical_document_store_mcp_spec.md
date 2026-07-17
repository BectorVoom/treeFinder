# Product and Technical Specification: Hierarchical Document Store MCP Server

**Document status:** Draft for vibe-coding implementation  
**Language:** English  
**Target release:** MVP  
**License intent:** Original clean-room implementation. Do not copy source code, prompts, schemas, or documentation text from PageIndex or any other third-party project.

## 1. Purpose

Build a local-first document system that:

1. Stores Markdown documents as ordinary files.
2. Builds a hierarchical tree index from each document.
3. Searches the tree using a replaceable hierarchical retrieval algorithm.
4. Returns the structure and content of previously registered documents.
5. Allows agents to create, read, update by patch, and save Markdown files through MCP.
6. Automatically records every edit.
7. Publishes all appropriate capabilities to connected MCP clients.
8. Provides an independent CLI with equivalent core behavior.

The design may take inspiration from the general idea of tree-based, reasoning-oriented retrieval, but the implementation must use original interfaces, code, prompts, tests, and terminology. Public PageIndex materials describe a document-to-tree workflow followed by reasoning over that tree, rather than conventional vector-only retrieval. This specification adopts only that high-level architectural pattern. [Source: PageIndex developer documentation, accessed 2026-07-17](https://docs.pageindex.ai/) and [PageIndex GitHub repository](https://github.com/VectifyAI/PageIndex).

MCP resources are suitable for exposing file and index data through URIs, while tools are appropriate for model-invoked mutations and searches. MCP resource capabilities can also advertise subscriptions and list-change notifications. [Source: MCP Resources specification, 2025-06-18](https://modelcontextprotocol.io/specification/2025-06-18/server/resources).

## 2. Scope

### 2.1 In scope

- Markdown ingestion, creation, reading, update, deletion, and export
- UTF-8 text handling
- Heading-based and synthetically inferred tree construction
- Hierarchical search with traceable scoring and traversal
- Pluggable retrieval strategies
- MCP server over stdio for MVP
- Resources, resource templates, tools, and change notifications
- Standalone CLI
- File-level revision history and operation audit log
- Atomic writes, optimistic concurrency control, and path sandboxing
- Unit, integration, golden, and retrieval-evaluation tests

### 2.2 Out of scope for MVP

- PDF, DOCX, image, or OCR ingestion
- Collaborative real-time editing
- Binary asset management
- Hosted multi-tenant control plane
- Full Git compatibility
- Mandatory vector database
- Autonomous answer generation beyond retrieval

## 3. Product principles

- **Files are canonical:** The Markdown file is the source of truth. Indexes are derived and rebuildable.
- **No hidden mutation:** Every content-changing operation creates a revision and an audit event.
- **Explainable retrieval:** Every search result includes the path, selected nodes, scores, reasons, and algorithm version.
- **Replaceable algorithms:** Tree construction and search strategies are selected by configuration and dependency injection.
- **MCP and CLI parity:** Both interfaces call the same application service layer.
- **Safe by default:** All paths remain inside a configured workspace; writes are atomic; conflicting updates fail clearly.
- **Clean-room implementation:** Use published concepts only. Do not port third-party implementation details.

## 4. Personas and primary workflows

### 4.1 Agent through MCP

- Discover registered documents.
- Read a document tree.
- Search one document or the entire corpus.
- Read selected node content.
- Create a Markdown file.
- Apply a patch using an expected revision.
- Save a full replacement only when explicitly requested.
- Inspect edit history and retrieve an earlier revision.

### 4.2 Human through CLI

- Initialize a workspace.
- Import or create Markdown.
- Build or rebuild indexes.
- Run searches with a chosen algorithm and debug trace.
- View trees and history.
- Diff, patch, restore, and validate documents.

### 4.3 Evaluator

- Swap search algorithms using one configuration value.
- Run the same query set against multiple strategy versions.
- Compare relevance, latency, node visits, token usage, and determinism.

## 5. Recommended architecture

```text
MCP Client                 CLI User
    |                         |
MCP Adapter              CLI Adapter
    |                         |
    +------ Application Services ------+
            |       |       |          |
        Document  Index   Search    History
        Service   Service Service   Service
            |       |       |          |
            +---- Repository Ports ----+
                    |          |
              File Store   SQLite Metadata
                    |          |
              Markdown     Trees, revisions,
              workspace    audit, search runs
```

### 5.1 Suggested stack

- Python 3.12+
- Official or maintained MCP Python SDK
- Typer for CLI
- Pydantic v2 for schemas and configuration
- SQLite with WAL mode for metadata
- `pathlib` and atomic temporary-file replacement for file operations
- Markdown parser that exposes headings and source spans
- `pytest`, snapshot tests, and property-based tests

The architecture must not depend on a specific LLM provider. LLM-assisted strategies are optional plugins behind a provider interface.

## 6. Workspace layout

```text
workspace/
  documents/
    <document-path>.md
  .hds/
    metadata.sqlite3
    revisions/
      <document-id>/
        <revision-id>.md
    indexes/
      <document-id>/
        <index-version>.json
    logs/
      audit.jsonl
    config.yaml
```

Requirements:

- Agents may address documents by logical path, such as `notes/design.md`.
- Resolve and normalize every path before access.
- Reject absolute paths, `..` traversal, symlink escapes, and non-Markdown extensions by default.
- Internal `.hds` content must not be writable through generic document tools.
- A document must have a stable UUID independent of renames.

## 7. Core domain model

### 7.1 Document

```yaml
document_id: uuid
logical_path: notes/design.md
title: Design Notes
current_revision: rev_...
content_hash: sha256:...
created_at: RFC3339
updated_at: RFC3339
index_status: ready|stale|building|failed
metadata: {}
```

### 7.2 Tree node

```yaml
node_id: stable-string
parent_id: string|null
kind: document|section|subsection|synthetic_group|paragraph_range
level: integer
title: string
summary: string|null
source:
  start_line: integer
  end_line: integer
  start_byte: integer
  end_byte: integer
children: [node_id]
attributes:
  heading_path: [string]
  word_count: integer
  content_hash: sha256:...
```

Node IDs should remain stable when unrelated parts of a document change. Recommended input: document UUID, normalized heading ancestry, local occurrence index, and node content fingerprint.

### 7.3 Revision

```yaml
revision_id: ulid
parent_revision_id: ulid|null
document_id: uuid
actor:
  type: mcp_client|cli|system
  id: string
operation: create|replace|patch|rename|delete|restore
before_hash: sha256:...|null
after_hash: sha256:...|null
message: string|null
created_at: RFC3339
patch_format: unified_diff|json_patch|full_snapshot
```

### 7.4 Audit event

Audit events are append-only and include event ID, timestamp, actor, interface, operation, sanitized arguments, status, latency, affected document and revision IDs, and error code. Never log secrets or full document content in the audit stream.

## 8. Tree construction

### 8.1 Baseline builder: `markdown_heading_v1`

1. Parse front matter, headings, paragraphs, lists, code blocks, and source positions.
2. Create a document root.
3. Convert headings into nodes according to heading level.
4. If heading levels skip, attach the node to the nearest valid ancestor and emit a diagnostic.
5. Associate body blocks with the closest preceding heading.
6. For long unheaded ranges, create deterministic synthetic groups using paragraph boundaries and configurable size limits.
7. Generate extractive summaries by default. Optional provider plugins may generate abstractive summaries.
8. Persist the tree with builder name, version, configuration hash, document revision, and creation timestamp.

### 8.2 Incremental rebuilding

- Compare content hashes and parsed source spans against the prior index.
- Reuse unchanged subtrees.
- Rebuild changed nodes and ancestors.
- Mark the index stale immediately after a write.
- Rebuild synchronously for small files and through an internal job queue for large files.
- Search must declare which revision and index version it used.

### 8.3 Builder plugin interface

```python
class TreeBuilder(Protocol):
    name: str
    version: str
    def build(self, document, config) -> TreeIndex: ...
    def rebuild(self, document, previous_index, change_set, config) -> TreeIndex: ...
```

Builders are registered through an internal registry or Python entry points. Configuration must allow `tree.builder: markdown_heading_v1` without code changes.

## 9. Hierarchical search algorithm

### 9.1 Clean-room baseline: `beam_tree_v1`

The baseline is an original, deterministic, top-down beam search over the document tree.

1. Normalize the query and derive lexical terms.
2. Score the root's children using weighted signals.
3. Keep the best `beam_width` candidates.
4. Expand candidates whose score exceeds `expand_threshold`.
5. Re-score children with ancestry context.
6. Stop at `max_depth`, `max_nodes_visited`, or when the frontier cannot improve the current top results.
7. Fetch content for the final nodes and optionally include parent or sibling context.
8. Deduplicate ancestor and descendant overlaps.
9. Return ranked nodes plus a complete traversal trace.

Default score:

```text
score(node, query) =
    w_title   * lexical_title_match
  + w_body    * lexical_body_match
  + w_summary * lexical_summary_match
  + w_path    * ancestor_path_match
  + w_prior   * structural_prior
  + w_plugin  * optional_external_score
```

The MVP lexical implementation may use BM25 or a simpler normalized term score. The optional external score can be an embedding, reranker, or LLM reasoner, but the baseline must work without network access or an LLM.

### 9.2 Search strategy interface

```python
class SearchStrategy(Protocol):
    name: str
    version: str
    def search(self, query, indexes, options, scorer, trace_sink) -> SearchResult: ...
```

The scorer must also be replaceable:

```python
class NodeScorer(Protocol):
    name: str
    version: str
    def score(self, query, node, ancestry, options) -> ScoreBreakdown: ...
```

### 9.3 Easy algorithm replacement

Required mechanisms:

- Strategy registry keyed by name.
- YAML and environment-variable selection.
- Per-request override where policy allows.
- No strategy-specific fields in core domain objects.
- Versioned options validated by Pydantic discriminated unions.
- Golden query suite executable against every registered strategy.
- Trace schema shared across strategies.
- Feature flag to disable experimental strategies.

Example configuration:

```yaml
search:
  default_strategy: beam_tree_v1
  strategies:
    beam_tree_v1:
      beam_width: 8
      max_depth: 8
      max_nodes_visited: 200
      expand_threshold: 0.12
      weights:
        title: 0.30
        body: 0.25
        summary: 0.20
        path: 0.20
        prior: 0.05
    exhaustive_tree_v1:
      max_nodes_visited: 5000
```

### 9.4 Search response

Each response must include:

- query
- strategy and version
- configuration hash
- document revision and index version
- ranked results
- node path, line range, excerpt, and score breakdown
- traversal trace or trace ID
- nodes visited
- elapsed milliseconds
- truncation and stale-index warnings

## 10. Document operations

### 10.1 Create

- Accept logical path, UTF-8 Markdown, optional metadata, and commit message.
- Fail if the file exists unless `if_exists=error|overwrite` is explicitly supplied.
- Validate path and size.
- Atomically write the file.
- Create revision, audit event, and index.

### 10.2 Read

Support full content, line range, byte range, node ID, and revision ID. Return hashes and revision metadata.

### 10.3 Patch

Primary update mechanism:

- Inputs: document ID or path, base revision, patch, patch format, message.
- Require `base_revision` for agent writes.
- Support unified diff in MVP.
- Apply in memory, validate, atomically save, create snapshot and audit event, then rebuild index.
- If current revision differs from base revision, return `REVISION_CONFLICT` and do not write.
- Return the new revision and a concise diff summary.

### 10.4 Replace

Allow full replacement with an expected revision. This is useful for small files but should not be the agent default.

### 10.5 History and restore

- List revisions with pagination.
- Read any retained revision.
- Diff two revisions.
- Restore by creating a new revision whose content equals the selected earlier revision.
- Never rewrite history during restore.

## 11. MCP surface

### 11.1 Server capabilities

Advertise tools and resources. Enable resource subscriptions and `listChanged` when supported. Connected clients can use resource listing and reading to consume documents and trees, while mutations are exposed as tools. MCP's resource model uses unique URIs and defines list and read operations; subscriptions and list-change notifications are optional advertised capabilities. [Source: MCP Resources specification, 2025-06-18](https://modelcontextprotocol.io/specification/2025-06-18/server/resources).

MVP transport: stdio. A later release may add Streamable HTTP after authentication, origin validation, and deployment hardening.

### 11.2 Resource URIs

```text
hds://documents
hds://document/{document_id}
hds://document/{document_id}/content
hds://document/{document_id}/tree
hds://document/{document_id}/node/{node_id}
hds://document/{document_id}/history
hds://document/{document_id}/revision/{revision_id}
hds://search-run/{search_run_id}/trace
```

`resources/list` should expose concrete documents with pagination. Resource templates should expose dynamic document, node, revision, and trace URIs. Resource reads are side-effect free.

### 11.3 MCP tools

#### `document_create`

Inputs: `path`, `content`, `metadata?`, `message?`, `if_exists?`  
Output: document descriptor, revision, index status.

#### `document_get`

Inputs: `document_id|path`, `revision_id?`, `range?`  
Output: content and metadata.

#### `document_patch`

Inputs: `document_id|path`, `base_revision`, `patch`, `format`, `message?`  
Output: new revision, hashes, diff summary, index status.

#### `document_replace`

Inputs: `document_id|path`, `base_revision`, `content`, `message?`  
Output: new revision and index status.

#### `document_list`

Inputs: `prefix?`, `cursor?`, `limit?`, `include_deleted?`  
Output: paginated descriptors.

#### `document_history`

Inputs: `document_id|path`, `cursor?`, `limit?`  
Output: paginated revisions.

#### `document_diff`

Inputs: `document_id|path`, `from_revision`, `to_revision`  
Output: unified diff and summary.

#### `document_restore`

Inputs: `document_id|path`, `target_revision`, `base_revision`, `message?`  
Output: newly created revision.

#### `tree_get`

Inputs: `document_id|path`, `revision_id?`, `depth?`, `include_summaries?`, `include_content?`  
Output: tree or paginated subtree.

#### `node_get`

Inputs: `document_id|path`, `node_id`, `revision_id?`, `context?`  
Output: node, path, source span, and content.

#### `search_hierarchy`

Inputs: `query`, `document_ids?`, `path_prefix?`, `strategy?`, `options?`, `top_k?`, `include_trace?`  
Output: ranked results and reproducibility metadata.

#### `index_rebuild`

Inputs: `document_id|path`, `builder?`, `force?`  
Output: new index version and diagnostics.

### 11.4 Notifications

After successful changes:

- Send resource-updated notification for the document content, tree, and history when subscribed.
- Send resource-list-changed notification after create, rename, or delete.
- Do not notify before the file, revision, and metadata transaction is durable.

### 11.5 MCP errors

Use stable application error data:

```json
{
  "code": "REVISION_CONFLICT",
  "message": "The document changed after the supplied base revision.",
  "details": {
    "expected": "rev_01...",
    "actual": "rev_02..."
  },
  "retryable": false
}
```

Required codes include `NOT_FOUND`, `INVALID_PATH`, `INVALID_MARKDOWN`, `REVISION_CONFLICT`, `PATCH_FAILED`, `INDEX_STALE`, `INDEX_FAILED`, `STRATEGY_NOT_FOUND`, `LIMIT_EXCEEDED`, and `PERMISSION_DENIED`.

## 12. CLI specification

Executable name: `hds`

```text
hds init [PATH]
hds document add FILE [--as LOGICAL_PATH]
hds document create PATH [--from FILE]
hds document list [--prefix PREFIX] [--json]
hds document show ID_OR_PATH [--revision REV] [--lines START:END]
hds document patch ID_OR_PATH --base REV --patch FILE [--message TEXT]
hds document replace ID_OR_PATH --base REV --from FILE [--message TEXT]
hds document history ID_OR_PATH [--json]
hds document diff ID_OR_PATH --from REV --to REV
hds document restore ID_OR_PATH --target REV --base REV
hds tree show ID_OR_PATH [--depth N] [--json]
hds index rebuild ID_OR_PATH [--builder NAME] [--force]
hds search QUERY [--document ID] [--strategy NAME] [--top-k N] [--trace] [--json]
hds strategy list
hds strategy describe NAME
hds evaluate DATASET --strategies NAME,NAME [--output FILE]
hds mcp serve --transport stdio
hds doctor
```

CLI requirements:

- Human-readable output by default and stable JSON with `--json`.
- Exit code 0 on success, 2 for usage errors, 3 for not found, 4 for conflict, 5 for validation, and 1 for unexpected failure.
- Commands must use the same application services as MCP handlers.
- Commands must never bypass revision or audit recording.

## 13. Persistence and transaction behavior

A content update spans a file store and SQLite, so implement a recoverable write protocol:

1. Validate request and expected revision.
2. Write candidate content to a temporary file in the same filesystem.
3. Store the previous snapshot if not already retained.
4. Start SQLite transaction and create pending revision.
5. Atomically replace the Markdown file.
6. Compute and verify final hash.
7. Finalize revision and audit event, then commit.
8. Trigger index rebuild.

On startup, inspect pending operations and reconcile using hashes. Never silently discard an ambiguous operation.

## 14. Security requirements

- Workspace sandbox and canonical-path checks on every operation.
- Symlinks disabled by default.
- Maximum file size, patch size, query length, result count, and nodes visited.
- No shell execution in document operations.
- Treat document text as untrusted data, not instructions.
- Escape terminal control characters in CLI display.
- Redact secrets from logs and MCP errors.
- Optional read-only mode.
- Optional allowlist for MCP tools.
- For network transport, require authentication, authorization, TLS termination, request limits, and origin checks.
- Audit actor identity supplied by the host must be preserved but not blindly trusted as authorization proof.

## 15. Observability

- Structured JSON logs with correlation ID.
- Metrics: request count, latency, errors, conflicts, index duration, index failures, query latency, node visits, cache hit rate, and stale-index searches.
- Search trace retained according to policy.
- `hds doctor` checks workspace permissions, database integrity, orphan files, missing snapshots, stale indexes, strategy loading, and configuration validity.

## 16. Testing strategy

### 16.1 Unit tests

- Path containment and symlink attacks
- Markdown parsing and skipped heading levels
- Stable node IDs
- Every scorer signal
- Beam limits and deterministic tie-breaking
- Patch application and conflict detection
- Revision and audit creation

### 16.2 Integration tests

- MCP create, read, patch, tree, search, history, and notification flow
- CLI and MCP produce equivalent domain results
- Crash recovery at each write-protocol stage
- Rebuild after edit and restore
- Pagination and limits

### 16.3 Retrieval evaluation

Dataset format:

```json
{"query":"rollback procedure","document_ids":["..."],"relevant_node_ids":["..."],"tags":["operations"]}
```

Report per strategy:

- Recall@k
- Precision@k
- Mean reciprocal rank
- nDCG@k
- node visits
- latency p50 and p95
- optional LLM token and cost totals
- failure and timeout rates

The evaluation runner must pin corpus revisions, index versions, strategy versions, configuration hashes, and random seeds.

## 17. Acceptance criteria

The MVP is accepted when all conditions below are true:

1. An MCP client can create a Markdown document and immediately discover and read it as a resource.
2. An MCP client can retrieve the complete tree and a selected node's content.
3. Search returns relevant nodes, score breakdowns, and a traversal trace.
4. Changing `search.default_strategy` switches the algorithm without modifying adapters or repositories.
5. An agent can apply a unified diff using a base revision.
6. A stale base revision produces a conflict and no file change.
7. Every create, patch, replace, rename, delete, and restore produces a revision and audit event.
8. A restore creates a new revision rather than altering history.
9. The CLI supports equivalent create, read, patch, tree, search, history, diff, restore, and rebuild operations.
10. All writes are atomic under forced-process-termination tests.
11. Path traversal and symlink escape tests pass.
12. A workspace can rebuild all indexes entirely from Markdown files and metadata.

## 18. Suggested implementation milestones

### Milestone 1: Skeleton

- Packaging, configuration, domain models, repository interfaces, SQLite migrations, CLI shell, MCP server shell.

### Milestone 2: Document and history

- Secure file store, create/read/list, atomic replace, unified-diff patch, revisions, audit, conflict handling.

### Milestone 3: Tree index

- Markdown parser, baseline builder, persistent tree, incremental rebuild, tree and node APIs.

### Milestone 4: Search

- Strategy registry, baseline beam search, scorer breakdown, trace, corpus search, evaluation runner.

### Milestone 5: MCP completeness

- Resources, templates, all tools, subscriptions, notifications, pagination, error mapping.

### Milestone 6: Hardening

- Recovery, limits, security tests, observability, benchmarks, documentation, release packaging.

## 19. Vibe-coding guardrails

Give the coding agent one milestone at a time and require it to:

1. Write or update tests before declaring completion.
2. Keep adapters thin and business logic in application services.
3. Avoid global strategy conditionals. Use registries and interfaces.
4. Preserve backward-compatible JSON schemas unless a migration is included.
5. Run formatting, type checking, unit tests, and integration tests after each change.
6. Show changed files, design decisions, test results, and known limitations.
7. Never copy third-party source or prompts. Implement from this original specification.
8. Never weaken path, revision, audit, or atomic-write protections to make a test pass.

## 20. Initial coding-agent prompt

```text
Implement Milestone 1 of the Hierarchical Document Store specification.
Use Python 3.12, a src layout, Pydantic v2, Typer, SQLite, pytest, and an MCP Python SDK.
Create domain models and ports first, then thin CLI and MCP adapters.
Do not implement retrieval logic yet, but define versioned TreeBuilder, SearchStrategy,
and NodeScorer protocols plus registries. All paths must be workspace-relative and validated.
Add unit tests, type checking, formatting, and a README with one-line commands.
Do not copy code, prompts, or schemas from PageIndex or other third-party projects.
At completion, report changed files, architecture decisions, commands run, test results,
and remaining work. Do not claim success if tests fail.
```

## 21. Open design decisions

- Retention policy for full snapshots versus delta storage
- Whether deleted files remain addressable as historical resources
- Maximum synchronous indexing threshold
- Default tokenizer and language handling for lexical search
- Whether front matter metadata contributes to scoring
- Authentication model for future network transport
- Optional Git mirroring versus internal revision store only

## 22. Reference notes

- The MCP resources specification describes URI-addressed resources, listing, reading, subscriptions, and list-change notifications. These semantics inform the read-only resource surface in this design: <https://modelcontextprotocol.io/specification/2025-06-18/server/resources>
- Public PageIndex documentation describes a high-level workflow that creates a document tree and then performs context-aware reasoning over it. This specification uses that broad idea only and defines an independent algorithm and implementation: <https://docs.pageindex.ai/>
- The public PageIndex repository describes the project as an open-source, tree-oriented retrieval system. No code, prompt, or schema from that repository is required or incorporated here: <https://github.com/VectifyAI/PageIndex>
