//! MCP server built on the official `rmcp` SDK (stdio transport).
//!
//! Read-only data is exposed as `hds://` resources; mutations and search are
//! tools. `HdsMcpServer` implements `rmcp::ServerHandler` directly (no tool
//! macros) so all twelve tools route through the same application services
//! the CLI uses. Application failures surface as tool results carrying the
//! stable wire payload (spec §11.5); JSON-RPC errors are reserved for
//! unroutable requests. Resource-updated and list-changed notifications are
//! emitted through the connected peer only after services return, i.e. after
//! the file/revision/metadata transaction is durable.

mod resources;
mod tools;

use crate::domain::{Actor, ActorType, HdsError};
use crate::services::Workspace;
use anyhow::Context as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorData as McpError, Implementation,
    ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResult, ResourceUpdatedNotificationParam,
    ServerCapabilities, ServerInfo, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ServerHandler, ServiceExt};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Which notifications a successful tool call makes due.
#[derive(Debug, Default, Clone)]
pub(crate) struct ChangeSet {
    pub document_id: Option<String>,
    pub list_changed: bool,
}

#[derive(Clone)]
pub struct HdsMcpServer {
    // The workspace holds a rusqlite connection (Send, not Sync), so all
    // service work runs under one mutex. Handlers never await while holding
    // the guard.
    ws: Arc<Mutex<Workspace>>,
    subscriptions: Arc<Mutex<HashSet<String>>>,
}

impl HdsMcpServer {
    pub fn new(ws: Workspace) -> Self {
        HdsMcpServer {
            ws: Arc::new(Mutex::new(ws)),
            subscriptions: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn workspace(&self) -> std::sync::MutexGuard<'_, Workspace> {
        self.ws
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn actor_for(&self, context: &RequestContext<RoleServer>) -> Actor {
        let id = context
            .peer
            .peer_info()
            .map(|info| info.client_info.name.clone())
            .unwrap_or_else(|| "unknown-client".to_string());
        Actor {
            actor_type: ActorType::McpClient,
            id,
        }
    }

    /// Send the notifications made due by `changes`, honoring subscriptions.
    async fn notify_changes(&self, context: &RequestContext<RoleServer>, changes: &ChangeSet) {
        let subscribed: Vec<String> = {
            let subs = self
                .subscriptions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match &changes.document_id {
                Some(id) => {
                    let mut uris: Vec<String> = ["content", "tree", "history"]
                        .iter()
                        .map(|s| format!("hds://document/{id}/{s}"))
                        .collect();
                    uris.push(format!("hds://document/{id}"));
                    uris.retain(|u| subs.contains(u));
                    uris
                }
                None => Vec::new(),
            }
        };
        for uri in subscribed {
            let _ = context
                .peer
                .notify_resource_updated(ResourceUpdatedNotificationParam::new(uri))
                .await;
        }
        if changes.list_changed {
            let _ = context.peer.notify_resource_list_changed().await;
        }
    }
}

impl ServerHandler for HdsMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .build(),
        );
        info.server_info = {
            let mut imp = Implementation::from_build_env();
            imp.name = "tree_finder".to_string();
            imp.title = Some("Hierarchical Document Store".to_string());
            imp.version = env!("CARGO_PKG_VERSION").to_string();
            imp
        };
        info.instructions = Some(
            "Markdown documents with tree indexes. Use search_hierarchy to find \
             relevant nodes, tree_get/node_get to navigate, and document_patch \
             (unified diff + base_revision) to edit. Read-only data is also \
             available as hds:// resources."
                .to_string(),
        );
        info
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.actor_for(&context);
        let outcome = {
            let ws = self.workspace();
            tools::dispatch(&ws, &actor, &request)
        };
        match outcome {
            Ok((value, changes)) => {
                self.notify_changes(&context, &changes).await;
                Ok(CallToolResult::structured(value))
            }
            Err(ToolFailure::Application(e)) => Ok(CallToolResult::structured_error(e.to_wire())),
            Err(ToolFailure::UnknownTool(name)) => Err(McpError::invalid_params(
                format!("unknown tool '{name}'"),
                None,
            )),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        let ws = self.workspace();
        Ok(rmcp::model::ListToolsResult::with_all_items(
            tools::list_tools(&ws),
        ))
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let ws = self.workspace();
        let cursor = request.and_then(|r| r.cursor).map(|c| c.to_string());
        resources::list(&ws, cursor.as_deref()).map_err(to_protocol_error)
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(
            resources::templates(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let actor = self.actor_for(&context);
        let ws = self.workspace();
        resources::read(&ws, &actor, &request.uri)
            .map(|contents| ReadResourceResult::new(vec![contents]))
            .map_err(|e| match e.code {
                crate::domain::ErrorCode::NotFound => {
                    McpError::resource_not_found(e.message.clone(), Some(e.to_wire()))
                }
                _ => to_protocol_error(e),
            })
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(request.uri);
        Ok(())
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&request.uri);
        Ok(())
    }
}

/// How a tool call failed: an application error the agent should see, or an
/// unroutable request that becomes a JSON-RPC error.
pub(crate) enum ToolFailure {
    Application(HdsError),
    UnknownTool(String),
}

impl From<HdsError> for ToolFailure {
    fn from(e: HdsError) -> Self {
        ToolFailure::Application(e)
    }
}

fn to_protocol_error(e: HdsError) -> McpError {
    McpError::internal_error(e.message.clone(), Some(e.to_wire()))
}

/// Blocking stdio server loop (spawns a tokio runtime for rmcp).
pub fn serve_stdio(ws: Workspace) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("failed to start async runtime")?;
    runtime.block_on(async {
        let service = HdsMcpServer::new(ws)
            .serve(rmcp::transport::stdio())
            .await
            .context("MCP server failed to initialize")?;
        service
            .waiting()
            .await
            .context("MCP server terminated abnormally")?;
        Ok(())
    })
}
