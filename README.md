# hds — Hierarchical Document Store

A local-first Markdown document store with hierarchical tree indexes,
explainable tree search, full revision history, an audit log, an MCP stdio
server, and an equivalent CLI. Implemented in Rust from
`hierarchical_document_store_mcp_spec.md` (clean-room; no third-party code,
prompts, or schemas).

## Quick start

```sh
cargo build --release                     # build ./target/release/hds
hds init my-workspace && cd my-workspace  # create workspace (documents/ + .hds/)
echo '# Notes' | hds document create notes/plan.md
hds document list                         # UUID, path, revision, index status
hds tree show notes/plan.md               # heading tree with node IDs and line spans
hds search "rollback procedure" --trace   # ranked nodes + score breakdown + traversal trace
hds mcp serve --transport stdio           # MCP server (13 tools, hds:// resources)
hds doctor                                # integrity checks
```

Patch workflow (optimistic concurrency — the agent default):

```sh
REV=$(hds document list | awk '{print $4}')
hds document patch notes/plan.md --base "$REV" --patch change.patch --message "edit"
# stale base -> exit code 4, REVISION_CONFLICT, file untouched
```

More: `hds document {add,create,list,show,patch,replace,history,diff,restore}`,
`hds index rebuild`, `hds strategy {list,describe}`, `hds evaluate DATASET
--strategies beam_tree_v1,exhaustive_tree_v1`. Every command supports `--json`
where output is structured. Exit codes: 0 ok, 2 usage, 3 not found,
4 conflict, 5 validation, 1 unexpected.

## Architecture

```
adapters   src/cli.rs (clap)        src/mcp/ (official rmcp MCP SDK, stdio)
services   src/services/            Workspace, DocumentService, IndexService, SearchService
plugins    src/index/               TreeBuilder registry  (markdown_heading_v1)
           src/search/              SearchStrategy + NodeScorer registries
                                    (beam_tree_v1, exhaustive_tree_v1, lexical_overlap_v1)
infra      src/infra/               sandboxed paths, atomic file store,
                                    SQLite (WAL) metadata, JSONL audit log
domain     src/domain/              pure data types + stable error codes
```

- **Files are canonical.** Markdown lives in `documents/`; everything under
  `.hds/` (SQLite metadata, revision snapshots, index JSON, audit log,
  config) is derived or append-only and indexes are rebuildable from files +
  metadata alone.
- **Every write follows a recoverable protocol** (snapshot → pending revision
  → atomic temp-file rename → hash verify → finalize). On startup, pending
  writes are reconciled by hash; ambiguous states are surfaced by
  `hds doctor`, never discarded.
- **Algorithms are configuration.** `.hds/config.yaml` selects
  `tree.builder` and `search.default_strategy`; strategies/builders register
  in registries, and per-request override is allowed unless disabled. Every
  search result carries strategy name/version, config hash, revision and
  index version, per-signal score breakdown, and a persisted traversal trace
  (`hds://search-run/{id}/trace`).
- **Sandboxing:** logical paths only (no absolute paths, no `..`, no `.hds`,
  Markdown extensions only), symlinks rejected by default, size/query/visit
  limits, read-only mode, and an MCP tool allowlist. Audit entries carry
  sanitized arguments — never document content or secrets.

## MCP surface

Built on the official Rust MCP SDK ([`rmcp`](https://docs.rs/rmcp)):
`HdsMcpServer` implements `rmcp::ServerHandler` directly, `hds mcp serve`
spins up a tokio runtime and serves the stdio transport (`anyhow` wraps the
bootstrap errors; domain errors stay `thiserror`-based `HdsError`s).

Tools: `document_create/get/patch/replace/list/history/diff/restore`,
`tree_get`, `node_get`, `search_hierarchy`, `index_rebuild`,
`workspace_list`. Application errors return `isError` results with stable
payloads (`{code, message, details, retryable}`, e.g. `REVISION_CONFLICT`).

Change notifications follow whichever subscription mechanism the client's
protocol version defines: `resources/subscribe` before 2026-07-28, and
`subscriptions/listen` streams from 2026-07-28 on. Both are served.

**Multi-workspace serving.** One long-lived server can address several
workspaces: every workspace-scoped tool accepts an optional `workspace`
argument — a path to (or inside) a workspace root — which the server resolves
by walking up to the nearest `.hds` and opens on demand. Calls without it go
to the default workspace (the one `hds mcp serve` was started in, or
`--workspace`). Started outside any workspace, the server still runs and
requires `workspace` on each call. `workspace_list` reports the open roots;
resource listing and `hds://` reads span all open workspaces. Each
workspace keeps its own config (allowlist, read-only mode), enforced per
call.

Resources: `hds://documents`, `hds://document/{id}[/content|/tree|/node/{node}|/history|/revision/{rev}]`,
`hds://search-run/{id}/trace`, plus resource templates, list pagination,
subscriptions, and `resources/updated` / `list_changed` notifications emitted
only after durable commits.

## Tests

`cargo test` — 37 tests: path/symlink sandboxing, tree construction (skipped
heading levels, code-block immunity, synthetic groups, stable node IDs),
scorer signals, deterministic beam search with visit budgets, patch/conflict,
restore-preserves-history, audit content-leak checks, pagination, crash
recovery at both protocol stages, rebuild-from-files, and the full MCP flow
(driven by a real `rmcp` client over an in-process duplex transport,
including subscription/notification delivery and multi-workspace serving).
`cargo fmt` and `cargo clippy --all-targets` are clean.

## Known limitations (MVP)

- Patch format: unified diff only (`json_patch` reserved in the schema).
- Large files (> `tree.sync_index_max_bytes`) are marked stale and rebuilt
  lazily on next read/search instead of via a background job queue.
- Rename is defined in the domain model but not yet exposed; delete is a
  service-level soft delete without a CLI/MCP verb (matching the spec's tool
  list).
- Snapshots are full copies (no delta storage); retention policy is not yet
  configurable.
- Transport is stdio only; network transport (auth, TLS, origin checks) is
  future work per spec §11.1.
