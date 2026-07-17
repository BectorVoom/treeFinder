//! `hds` command-line interface. A thin adapter: parses arguments, calls the
//! same application services the MCP server uses, formats output, and maps
//! error codes to the exit codes required by the spec (0 ok, 2 usage,
//! 3 not found, 4 conflict, 5 validation, 1 unexpected).

use crate::domain::{Actor, ActorType, ErrorCode, HdsError, PatchFormat};
use crate::search::eval;
use crate::services::{
    DocSelector, DocumentService, IndexService, SearchRequest, SearchService, Workspace,
    documents::{IfExists, ReadRange},
};
use clap::{Args, Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "hds",
    version,
    about = "Hierarchical document store with tree-indexed search (files are canonical)"
)]
pub struct Cli {
    /// Workspace root (defaults to the nearest ancestor containing .hds)
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new workspace
    Init { path: Option<PathBuf> },
    /// Document operations
    #[command(subcommand)]
    Document(DocumentCmd),
    /// Tree index views
    #[command(subcommand)]
    Tree(TreeCmd),
    /// Index maintenance
    #[command(subcommand)]
    Index(IndexCmd),
    /// Search the corpus
    Search(SearchArgs),
    /// Search strategy registry
    #[command(subcommand)]
    Strategy(StrategyCmd),
    /// Run the retrieval evaluation suite
    Evaluate(EvaluateArgs),
    /// MCP server
    #[command(subcommand)]
    Mcp(McpCmd),
    /// Workspace health checks
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DocumentCmd {
    /// Import an existing Markdown file into the workspace
    Add {
        file: PathBuf,
        #[arg(long = "as")]
        as_path: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Create a document from a file or stdin
    Create {
        path: String,
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        json: bool,
    },
    /// List registered documents
    List {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show document content
    Show {
        id_or_path: String,
        #[arg(long)]
        revision: Option<String>,
        /// Line range START:END (1-based, inclusive)
        #[arg(long)]
        lines: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Apply a unified diff
    Patch {
        id_or_path: String,
        #[arg(long)]
        base: String,
        #[arg(long)]
        patch: PathBuf,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Replace full content with an expected base revision
    Replace {
        id_or_path: String,
        #[arg(long)]
        base: String,
        #[arg(long)]
        from: PathBuf,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List revisions
    History {
        id_or_path: String,
        #[arg(long)]
        json: bool,
    },
    /// Unified diff between two revisions
    Diff {
        id_or_path: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// Restore an earlier revision as a new revision
    Restore {
        id_or_path: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        base: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum TreeCmd {
    /// Show the tree index of a document
    Show {
        id_or_path: String,
        #[arg(long)]
        depth: Option<usize>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum IndexCmd {
    /// Rebuild the tree index
    Rebuild {
        id_or_path: String,
        #[arg(long)]
        builder: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
struct SearchArgs {
    query: String,
    /// Restrict to one document (repeatable)
    #[arg(long = "document")]
    documents: Vec<String>,
    #[arg(long)]
    strategy: Option<String>,
    #[arg(long = "top-k")]
    top_k: Option<usize>,
    /// Include the traversal trace in the output
    #[arg(long)]
    trace: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum StrategyCmd {
    List,
    Describe { name: String },
}

#[derive(Args)]
struct EvaluateArgs {
    dataset: PathBuf,
    /// Comma-separated strategy names
    #[arg(long)]
    strategies: String,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long, default_value_t = 10)]
    k: usize,
}

#[derive(Subcommand)]
enum McpCmd {
    Serve {
        #[arg(long, default_value = "stdio")]
        transport: String,
    },
}

pub fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    match dispatch(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error [{}]: {}", e.code.as_str(), safe_text(&e.message));
            if !e.details.is_null() {
                eprintln!("details: {}", e.details);
            }
            e.code.exit_code()
        }
    }
}

fn cli_actor() -> Actor {
    Actor {
        actor_type: ActorType::Cli,
        id: std::env::var("USER").unwrap_or_else(|_| "cli-user".to_string()),
    }
}

fn find_workspace(explicit: Option<&Path>) -> Result<PathBuf, HdsError> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let mut dir = std::env::current_dir().map_err(HdsError::internal)?;
    loop {
        if dir.join(".hds").join("config.yaml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(HdsError::not_found(
                "workspace (no .hds directory here or above; run `hds init`)",
            ));
        }
    }
}

fn open_workspace(explicit: Option<&Path>) -> Result<Workspace, HdsError> {
    let root = find_workspace(explicit)?;
    let ws = Workspace::open(&root)?;
    for msg in &ws.recovery_report {
        eprintln!("recovery: {}", safe_text(msg));
    }
    Ok(ws)
}

fn dispatch(cli: Cli) -> Result<(), HdsError> {
    match cli.command {
        Command::Init { path } => {
            let root = path.unwrap_or(std::env::current_dir().map_err(HdsError::internal)?);
            Workspace::init(&root)?;
            println!("initialized workspace at {}", root.display());
            Ok(())
        }
        Command::Document(cmd) => {
            let ws = open_workspace(cli.workspace.as_deref())?;
            let docs = DocumentService::new(&ws, cli_actor(), "cli");
            document_cmd(&ws, &docs, cmd)
        }
        Command::Tree(TreeCmd::Show {
            id_or_path,
            depth,
            json,
        }) => {
            let ws = open_workspace(cli.workspace.as_deref())?;
            let docs = DocumentService::new(&ws, cli_actor(), "cli");
            let doc = docs.resolve(&DocSelector::parse(&id_or_path))?;
            let index_service = IndexService::new(&ws);
            let (index, rendered, stale) = index_service.tree(&doc, depth, true)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "document_id": doc.document_id,
                        "index_version": index.index_version,
                        "revision_id": index.revision_id,
                        "stale": stale,
                        "tree": rendered,
                    })
                );
            } else {
                if stale {
                    eprintln!("warning: index is stale");
                }
                print_tree(&rendered, 0);
            }
            Ok(())
        }
        Command::Index(IndexCmd::Rebuild {
            id_or_path,
            builder,
            force,
            json,
        }) => {
            let ws = open_workspace(cli.workspace.as_deref())?;
            let docs = DocumentService::new(&ws, cli_actor(), "cli");
            let doc = docs.resolve(&DocSelector::parse(&id_or_path))?;
            let outcome =
                IndexService::new(&ws).rebuild_with(&doc, None, builder.as_deref(), force)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                println!(
                    "index {} built with {} v{} ({} nodes, {} reused, {} rebuilt)",
                    outcome.index_version,
                    outcome.builder,
                    outcome.builder_version,
                    outcome.node_count,
                    outcome.reused_nodes,
                    outcome.rebuilt_nodes
                );
                for d in &outcome.diagnostics {
                    println!("  note: {}", safe_text(d));
                }
            }
            Ok(())
        }
        Command::Search(args) => {
            let ws = open_workspace(cli.workspace.as_deref())?;
            let search = SearchService::new(&ws, cli_actor(), "cli");
            let result = search.search(&SearchRequest {
                query: args.query.clone(),
                document_ids: args.documents.clone(),
                path_prefix: None,
                strategy: args.strategy.clone(),
                options: None,
                top_k: args.top_k,
                include_trace: args.trace,
            })?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "{} results via {} v{} ({} nodes visited, {} ms, run {})",
                    result.results.len(),
                    result.strategy,
                    result.strategy_version,
                    result.nodes_visited,
                    result.elapsed_ms,
                    result.search_run_id
                );
                for w in &result.warnings {
                    eprintln!("warning: {}", safe_text(w));
                }
                for (i, hit) in result.results.iter().enumerate() {
                    println!(
                        "{}. [{:.3}] {} :: {} (lines {}-{}, node {})",
                        i + 1,
                        hit.score.total,
                        safe_text(&hit.logical_path),
                        safe_text(&hit.node_path.join(" > ")),
                        hit.start_line,
                        hit.end_line,
                        hit.node_id
                    );
                    println!("   {}", safe_text(&hit.excerpt.replace('\n', " ")));
                }
                if args.trace {
                    println!("--- trace ---");
                    for step in &result.trace {
                        println!(
                            "{:>4} d{} {:<7} {} total={:.3} (t={:.2} b={:.2} s={:.2} p={:.2} prior={:.2}){}",
                            step.step,
                            step.depth,
                            step.action,
                            step.node_id,
                            step.score.total,
                            step.score.title,
                            step.score.body,
                            step.score.summary,
                            step.score.path,
                            step.score.prior,
                            step.note
                                .as_deref()
                                .map(|n| format!(" — {}", safe_text(n)))
                                .unwrap_or_default()
                        );
                    }
                }
            }
            Ok(())
        }
        Command::Strategy(cmd) => {
            let ws = open_workspace(cli.workspace.as_deref())?;
            match cmd {
                StrategyCmd::List => {
                    for name in ws.strategies.names() {
                        let s = ws.strategies.get(&name)?;
                        let default = if name == ws.config.search.default_strategy {
                            " (default)"
                        } else {
                            ""
                        };
                        println!("{} v{}{}", s.name(), s.version(), default);
                    }
                    Ok(())
                }
                StrategyCmd::Describe { name } => {
                    let s = ws.strategies.get(&name)?;
                    println!("{} v{}", s.name(), s.version());
                    println!("{}", s.describe());
                    println!(
                        "configured options: {}",
                        serde_json::to_string_pretty(&ws.config.strategy_options(&name))?
                    );
                    Ok(())
                }
            }
        }
        Command::Evaluate(args) => {
            let ws = open_workspace(cli.workspace.as_deref())?;
            let text = std::fs::read_to_string(&args.dataset).map_err(|e| {
                HdsError::not_found(format!("dataset {}: {e}", args.dataset.display()))
            })?;
            let cases = eval::load_dataset(&text)?;
            let search = SearchService::new(&ws, cli_actor(), "cli");
            let mut reports = Vec::new();
            for strategy in args
                .strategies
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                // Fail fast on unknown strategies rather than reporting 100% failures.
                ws.strategies.get(strategy)?;
                let report = eval::evaluate_strategy(strategy, &cases, args.k, |case| {
                    search.search(&SearchRequest {
                        query: case.query.clone(),
                        document_ids: case.document_ids.clone(),
                        strategy: Some(strategy.to_string()),
                        top_k: Some(args.k),
                        ..Default::default()
                    })
                });
                println!(
                    "{}: recall@{}={:.3} precision@{}={:.3} mrr={:.3} ndcg@{}={:.3} visits={:.1} p50={}ms p95={}ms failures={}",
                    report.strategy,
                    args.k,
                    report.recall_at_k,
                    args.k,
                    report.precision_at_k,
                    report.mean_reciprocal_rank,
                    args.k,
                    report.ndcg_at_k,
                    report.mean_nodes_visited,
                    report.latency_p50_ms,
                    report.latency_p95_ms,
                    report.failures
                );
                reports.push(report);
            }
            if let Some(out) = args.output {
                std::fs::write(&out, serde_json::to_string_pretty(&reports)?)?;
                println!("report written to {}", out.display());
            }
            Ok(())
        }
        Command::Mcp(McpCmd::Serve { transport }) => {
            if transport != "stdio" {
                return Err(HdsError::new(
                    ErrorCode::InvalidArgument,
                    "only --transport stdio is supported in this release",
                ));
            }
            let ws = open_workspace(cli.workspace.as_deref())?;
            // The rmcp server reports failures as anyhow chains; flatten the
            // chain into one internal-error message for exit-code mapping.
            crate::mcp::serve_stdio(ws).map_err(|e| HdsError::internal(format!("{e:#}")))
        }
        Command::Doctor { json } => {
            let ws = open_workspace(cli.workspace.as_deref())?;
            let findings = ws.doctor();
            let has_error = findings.iter().any(|(level, _)| level == "error");
            if json {
                let items: Vec<serde_json::Value> = findings
                    .iter()
                    .map(|(level, msg)| serde_json::json!({ "level": level, "message": msg }))
                    .collect();
                println!("{}", serde_json::json!({ "findings": items }));
            } else {
                for (level, msg) in &findings {
                    println!("[{level}] {}", safe_text(msg));
                }
            }
            if has_error {
                Err(HdsError::internal("doctor found errors"))
            } else {
                Ok(())
            }
        }
    }
}

fn document_cmd(
    ws: &Workspace,
    docs: &DocumentService<'_>,
    cmd: DocumentCmd,
) -> Result<(), HdsError> {
    match cmd {
        DocumentCmd::Add {
            file,
            as_path,
            json,
        } => {
            let content = std::fs::read_to_string(&file)
                .map_err(|e| HdsError::not_found(format!("file {}: {e}", file.display())))?;
            let logical = match as_path {
                Some(p) => p,
                None => file
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .ok_or_else(|| HdsError::invalid_path("file has no name"))?,
            };
            let outcome =
                docs.create(&logical, &content, BTreeMap::new(), None, IfExists::Error)?;
            print_mutation(&outcome, json)
        }
        DocumentCmd::Create {
            path,
            from,
            message,
            overwrite,
            json,
        } => {
            let content = match from {
                Some(f) => std::fs::read_to_string(&f)
                    .map_err(|e| HdsError::not_found(format!("file {}: {e}", f.display())))?,
                None => {
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .map_err(HdsError::internal)?;
                    buf
                }
            };
            let if_exists = if overwrite {
                IfExists::Overwrite
            } else {
                IfExists::Error
            };
            let outcome = docs.create(&path, &content, BTreeMap::new(), message, if_exists)?;
            print_mutation(&outcome, json)
        }
        DocumentCmd::List { prefix, json } => {
            let (list, _next) = docs.list(prefix.as_deref(), None, Some(500), false)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for d in &list {
                    println!(
                        "{}  {}  rev {}  index {}",
                        d.document_id,
                        safe_text(&d.logical_path),
                        d.current_revision,
                        d.index_status.as_str()
                    );
                }
            }
            Ok(())
        }
        DocumentCmd::Show {
            id_or_path,
            revision,
            lines,
            json,
        } => {
            let range = match lines.as_deref() {
                None => ReadRange::Full,
                Some(spec) => {
                    let (a, b) = spec.split_once(':').ok_or_else(|| {
                        HdsError::new(ErrorCode::InvalidArgument, "--lines expects START:END")
                    })?;
                    ReadRange::Lines(
                        a.parse().map_err(|_| {
                            HdsError::new(ErrorCode::InvalidArgument, "invalid start line")
                        })?,
                        b.parse().map_err(|_| {
                            HdsError::new(ErrorCode::InvalidArgument, "invalid end line")
                        })?,
                    )
                }
            };
            let out = docs.read(&DocSelector::parse(&id_or_path), revision.as_deref(), range)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "document": out.document,
                        "revision_id": out.revision_id,
                        "content_hash": out.content_hash,
                        "content": out.content,
                    })
                );
            } else {
                // Document text is untrusted: escape terminal control chars.
                println!("{}", safe_text(&out.content));
            }
            Ok(())
        }
        DocumentCmd::Patch {
            id_or_path,
            base,
            patch,
            message,
            json,
        } => {
            let patch_text = std::fs::read_to_string(&patch)
                .map_err(|e| HdsError::not_found(format!("patch {}: {e}", patch.display())))?;
            let outcome = docs.patch(
                &DocSelector::parse(&id_or_path),
                &base,
                &patch_text,
                PatchFormat::UnifiedDiff,
                message,
            )?;
            print_mutation(&outcome, json)
        }
        DocumentCmd::Replace {
            id_or_path,
            base,
            from,
            message,
            json,
        } => {
            let content = std::fs::read_to_string(&from)
                .map_err(|e| HdsError::not_found(format!("file {}: {e}", from.display())))?;
            let outcome =
                docs.replace(&DocSelector::parse(&id_or_path), &base, &content, message)?;
            print_mutation(&outcome, json)
        }
        DocumentCmd::History { id_or_path, json } => {
            let (revisions, _next) =
                docs.history(&DocSelector::parse(&id_or_path), None, Some(100))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&revisions)?);
            } else {
                for r in &revisions {
                    println!(
                        "{}  {}  {}  {}  {}",
                        r.revision_id,
                        r.created_at.to_rfc3339(),
                        r.operation.as_str(),
                        r.actor.actor_type.as_str(),
                        safe_text(r.message.as_deref().unwrap_or("-"))
                    );
                }
            }
            Ok(())
        }
        DocumentCmd::Diff {
            id_or_path,
            from,
            to,
        } => {
            let (diff, summary) = docs.diff(&DocSelector::parse(&id_or_path), &from, &to)?;
            println!("{}", safe_text(&diff));
            eprintln!("+{} -{}", summary.lines_added, summary.lines_removed);
            Ok(())
        }
        DocumentCmd::Restore {
            id_or_path,
            target,
            base,
            message,
            json,
        } => {
            let outcome =
                docs.restore(&DocSelector::parse(&id_or_path), &target, &base, message)?;
            print_mutation(&outcome, json)
        }
    }?;
    let _ = ws; // Workspace kept alive for the duration of the command.
    Ok(())
}

fn print_mutation(
    outcome: &crate::services::documents::MutationOutcome,
    json: bool,
) -> Result<(), HdsError> {
    if json {
        println!("{}", serde_json::to_string_pretty(outcome)?);
    } else {
        println!(
            "{} {} -> revision {} (index {})",
            outcome.revision.operation.as_str(),
            safe_text(&outcome.document.logical_path),
            outcome.revision.revision_id,
            outcome.document.index_status.as_str()
        );
        if let Some(s) = &outcome.diff_summary {
            println!("+{} -{}", s.lines_added, s.lines_removed);
        }
    }
    Ok(())
}

fn print_tree(node: &serde_json::Value, indent: usize) {
    let title = node["title"].as_str().unwrap_or("?");
    let kind = node["kind"].as_str().unwrap_or("?");
    let id = node["node_id"].as_str().unwrap_or("?");
    let lines = format!(
        "{}-{}",
        node["start_line"].as_u64().unwrap_or(0),
        node["end_line"].as_u64().unwrap_or(0)
    );
    println!(
        "{}{} [{}] ({}, lines {})",
        "  ".repeat(indent),
        safe_text(title),
        kind,
        id,
        lines
    );
    if let Some(children) = node["children"].as_array() {
        for c in children {
            print_tree(c, indent + 1);
        }
    }
}

/// Escape terminal control characters in untrusted display text.
pub fn safe_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c == '\t' {
                c
            } else if c.is_control() {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}
