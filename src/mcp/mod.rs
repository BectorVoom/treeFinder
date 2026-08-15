//! MCP server built on the official `rmcp` SDK (stdio transport).
//!
//! Read-only data is exposed as `hds://` resources; mutations and search are
//! tools. `HdsMcpServer` implements `rmcp::ServerHandler` directly (no tool
//! macros) so all tools route through the same application services
//! the CLI uses. Application failures surface as tool results carrying the
//! stable wire payload (spec §11.5); JSON-RPC errors are reserved for
//! unroutable requests. Resource-updated and list-changed notifications are
//! emitted only after services return, i.e. after the file/revision/metadata
//! transaction is durable, over whichever subscription mechanism the peer's
//! protocol version uses: `resources/subscribe` before 2026-07-28, and
//! `subscriptions/listen` streams from 2026-07-28 on.

mod resources;
mod tools;

use crate::domain::{Actor, ActorType, HdsError};
use crate::services::{Workspace, WorkspaceRegistry};
use anyhow::Context as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ErrorData as McpError, Implementation,
    ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, RequestId,
    ResourceUpdatedNotificationParam, ServerCapabilities, ServerInfo, SubscribeRequestParams,
    SubscriptionFilter, UnsubscribeRequestParams,
};
use rmcp::service::{
    RequestContext, RoleServer, SubscriptionContext, SubscriptionSendError, SubscriptionSink,
};
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
    // Each workspace holds a rusqlite connection (Send, not Sync), so all
    // service work runs under one mutex. Handlers never await while holding
    // the guard.
    registry: Arc<Mutex<WorkspaceRegistry>>,
    // `resources/subscribe` URIs, used by peers on protocol versions before
    // 2026-07-28.
    subscriptions: Arc<Mutex<HashSet<String>>>,
    // Open `subscriptions/listen` streams, used by peers on 2026-07-28 and
    // later. Each sink applies its own accepted filter.
    listeners: Arc<Mutex<Vec<SubscriptionSink>>>,
}

impl HdsMcpServer {
    /// Serve a single workspace as the default.
    pub fn new(ws: Workspace) -> Self {
        Self::from_registry(WorkspaceRegistry::with_default(ws))
    }

    /// Serve a registry; requests may address any workspace it can open.
    pub fn from_registry(registry: WorkspaceRegistry) -> Self {
        HdsMcpServer {
            registry: Arc::new(Mutex::new(registry)),
            subscriptions: Arc::new(Mutex::new(HashSet::new())),
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, WorkspaceRegistry> {
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn actor_for(&self, context: &RequestContext<RoleServer>) -> Actor {
        // From protocol version 2026-07-28 on, client identity travels in each
        // request's metadata rather than only the handshake; `client_info`
        // reads whichever of the two applies to this session.
        let id = context
            .client_info()
            .map(|info| info.name.clone())
            .unwrap_or_else(|| "unknown-client".to_string());
        Actor {
            actor_type: ActorType::McpClient,
            id,
        }
    }

    /// Send the notifications made due by `changes`, honoring subscriptions.
    ///
    /// The two subscription mechanisms are mutually exclusive per peer: before
    /// protocol version 2026-07-28 a client subscribes with
    /// `resources/subscribe` and gets plain peer notifications, and from
    /// 2026-07-28 on it opens a `subscriptions/listen` stream instead (the SDK
    /// rejects `resources/subscribe` outright on those sessions). Both are
    /// driven from the same set of URIs the change could have touched.
    async fn notify_changes(&self, context: &RequestContext<RoleServer>, changes: &ChangeSet) {
        let touched: Vec<String> = match &changes.document_id {
            Some(id) => {
                let mut uris: Vec<String> = ["content", "tree", "history"]
                    .iter()
                    .map(|s| format!("hds://document/{id}/{s}"))
                    .collect();
                uris.push(format!("hds://document/{id}"));
                uris
            }
            None => Vec::new(),
        };

        if context
            .protocol_version()
            .is_none_or(|v| v < ProtocolVersion::V_2026_07_28)
        {
            let subscribed: Vec<String> = {
                let subs = self
                    .subscriptions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                touched
                    .iter()
                    .filter(|u| subs.contains(*u))
                    .cloned()
                    .collect()
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
            return;
        }

        self.notify_listeners(&touched, changes.list_changed).await;
    }

    /// Fan a change out to every open `subscriptions/listen` stream. Each sink
    /// drops the URIs and categories its client did not ask for, so this sends
    /// the full candidate set and lets the SDK filter. Streams that have since
    /// closed are dropped from the registry.
    async fn notify_listeners(&self, touched: &[String], list_changed: bool) {
        let sinks: Vec<SubscriptionSink> = self
            .listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut closed: Vec<RequestId> = Vec::new();
        for sink in sinks {
            let mut open = true;
            for uri in touched {
                if matches!(
                    sink.notify_resource_updated(uri.clone()).await,
                    Err(SubscriptionSendError::SubscriptionClosed)
                ) {
                    open = false;
                    break;
                }
            }
            if open
                && list_changed
                && matches!(
                    sink.notify_resource_list_changed().await,
                    Err(SubscriptionSendError::SubscriptionClosed)
                )
            {
                open = false;
            }
            if !open {
                closed.push(sink.id().clone());
            }
        }
        if !closed.is_empty() {
            self.listeners
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|s| !closed.contains(s.id()));
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
    ) -> Result<CallToolResponse, McpError> {
        let actor = self.actor_for(&context);
        let outcome = {
            let mut registry = self.registry();
            tools::dispatch(&mut registry, &actor, &request)
        };
        match outcome {
            Ok((value, changes)) => {
                self.notify_changes(&context, &changes).await;
                Ok(CallToolResult::structured(value).into())
            }
            Err(ToolFailure::Application(e)) => {
                Ok(CallToolResult::structured_error(e.to_wire()).into())
            }
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
        let registry = self.registry();
        Ok(rmcp::model::ListToolsResult::with_all_items(
            tools::list_tools(&registry),
        ))
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let registry = self.registry();
        let cursor = request.and_then(|r| r.cursor).map(|c| c.to_string());
        resources::list(&registry, cursor.as_deref()).map_err(to_protocol_error)
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
    ) -> Result<ReadResourceResponse, McpError> {
        let actor = self.actor_for(&context);
        let registry = self.registry();
        resources::read(&registry, &actor, &request.uri)
            .map(|contents| ReadResourceResult::new(vec![contents]).into())
            .map_err(|e| match e.code {
                crate::domain::ErrorCode::NotFound => {
                    McpError::resource_not_found(e.message.clone(), Some(e.to_wire()))
                }
                _ => to_protocol_error(e),
            })
    }

    /// Accept whatever the client asked for. The SDK narrows this to the
    /// intersection of the request and the capabilities `get_info` advertises,
    /// so a category this server cannot emit is never acknowledged.
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.clone())
    }

    /// Hold one `subscriptions/listen` stream open, registering its sink so
    /// `notify_changes` can reach it, until the client cancels the request.
    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let sink = context.sink().clone();
        let id = sink.id().clone();
        self.listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(sink);
        context.cancelled().await;
        self.listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|s| s.id() != &id);
        Ok(())
    }

    /// Legacy subscription path (protocol versions before 2026-07-28); newer
    /// sessions use `listen` and never reach this.
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
pub fn serve_stdio(registry: WorkspaceRegistry) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("failed to start async runtime")?;
    runtime.block_on(async {
        let service = HdsMcpServer::from_registry(registry)
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
