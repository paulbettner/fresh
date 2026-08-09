//! Plugin Thread: Dedicated thread for TypeScript plugin execution
//!
//! This module implements a dedicated thread architecture for plugin execution,
//! using QuickJS as the JavaScript runtime with oxc for TypeScript transpilation.
//!
//! Architecture:
//! - Main thread (UI) sends requests to plugin thread via channel
//! - Plugin thread owns QuickJS runtime and persistent tokio runtime
//! - Results are sent back via the existing PluginCommand channel
//! - Async operations complete naturally without runtime destruction

use crate::backend::{QuickJsBackend, TsPluginInfo};
use crate::runtime::{
    ActivePluginInstances, AsyncResourceOwner, AsyncResourceOwners, PendingResponses,
    PluginLoadKind, TrackedAsyncResource,
};
use anyhow::{anyhow, Result};
use fresh_core::api::{
    EditorStateSnapshot, JsCallbackId, PluginCommandEnvelope, PluginInstanceId, PluginInvocation,
    SearchHandleRegistry,
};
use fresh_core::hooks::HookArgs;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// Re-export PluginConfig from fresh-core
pub use fresh_core::config::PluginConfig;

/// Consume and discard a `Result` from a fire-and-forget channel send.
///
/// Use when the receiver may have been dropped (e.g. during shutdown) and
/// failure is expected and non-actionable.
fn fire_and_forget<T, E: std::fmt::Debug>(result: std::result::Result<T, E>) {
    if let Err(e) = result {
        tracing::trace!(error = ?e, "fire-and-forget send failed");
    }
}

/// Result type for `LoadPluginsFromDirWithConfig`: `(load errors,
/// discovered plugins keyed by name)`. Aliased so the non-blocking
/// `_request` helpers can return a clippy-tractable receiver type.
pub type PluginsDirLoadResult = (Vec<String>, HashMap<String, PluginConfig>);

/// Request messages sent to the plugin thread
#[derive(Debug)]
pub enum PluginRequest {
    /// Load a plugin from a file
    LoadPlugin {
        path: PathBuf,
        kind: PluginLoadKind,
        response: oneshot::Sender<Result<()>>,
    },

    /// Resolve an async callback with a result (for async operations like SpawnProcess, Delay)
    ResolveCallback {
        expected_instance: Option<PluginInstanceId>,
        callback_id: fresh_core::api::JsCallbackId,
        result_json: String,
    },

    /// Reject an async callback with an error
    RejectCallback {
        expected_instance: Option<PluginInstanceId>,
        callback_id: fresh_core::api::JsCallbackId,
        error: String,
    },

    /// Load all plugins from a directory
    LoadPluginsFromDir {
        dir: PathBuf,
        kind: PluginLoadKind,
        response: oneshot::Sender<Vec<String>>,
    },

    /// Load all plugins from a directory with config support
    /// Returns (errors, discovered_plugins) where discovered_plugins contains
    /// all found plugins with their paths and enabled status
    LoadPluginsFromDirWithConfig {
        dir: PathBuf,
        kind: PluginLoadKind,
        plugin_configs: HashMap<String, PluginConfig>,
        response: oneshot::Sender<(Vec<String>, HashMap<String, PluginConfig>)>,
    },

    /// Load a plugin from source code (no file I/O)
    LoadPluginFromSource {
        source: String,
        name: String,
        is_typescript: bool,
        kind: PluginLoadKind,
        response: oneshot::Sender<Result<()>>,
    },

    /// Unload a plugin by name
    UnloadPlugin {
        name: String,
        response: oneshot::Sender<Result<()>>,
    },

    /// Reload a plugin by name
    ReloadPlugin {
        name: String,
        response: oneshot::Sender<Result<()>>,
    },

    /// Execute a plugin action. `args_json`, when set, is a JSON object handed
    /// to the handler as its single argument — how the agent command channel
    /// passes `RunCommand.args` through to a plugin command. `None` calls the
    /// handler with no arguments (the keybinding / palette path).
    ExecuteAction {
        action_name: String,
        args_json: Option<String>,
        /// When set, the runtime reports the handler's return value (or its
        /// failure) back to the editor under this id once the handler settles —
        /// how a `RunCommand` over the agent command channel gets an answer.
        request_id: Option<u64>,
        invocation: Option<PluginInvocation>,
        response: oneshot::Sender<Result<()>>,
    },

    /// Run a hook (fire-and-forget, no response needed). When `target`
    /// is set, only that plugin's handlers run — used for events that
    /// belong to one plugin, like a panel's `widget_event`.
    RunHook {
        hook_name: String,
        args: HookArgs,
        target: Option<String>,
        invocation: Option<PluginInvocation>,
    },

    /// Check if any handlers are registered for a hook
    HasHookHandlers {
        hook_name: String,
        response: oneshot::Sender<bool>,
    },

    /// List all loaded plugins
    ListPlugins {
        response: oneshot::Sender<Vec<TsPluginInfo>>,
    },

    /// Track an async resource whose creation was confirmed by the editor.
    TrackAsyncResource {
        owner: AsyncResourceOwner,
        resource: TrackedAsyncResource,
    },

    /// Shutdown the plugin thread
    Shutdown,
}

/// Simple oneshot channel implementation
pub mod oneshot {
    use std::fmt;
    use std::sync::mpsc;

    pub struct Sender<T>(mpsc::SyncSender<T>);
    pub struct Receiver<T>(mpsc::Receiver<T>);

    use anyhow::Result;

    impl<T> fmt::Debug for Sender<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_tuple("Sender").finish()
        }
    }

    impl<T> fmt::Debug for Receiver<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_tuple("Receiver").finish()
        }
    }

    impl<T> Sender<T> {
        pub fn send(self, value: T) -> Result<(), T> {
            self.0.send(value).map_err(|e| e.0)
        }
    }

    impl<T> Receiver<T> {
        pub fn recv(self) -> Result<T, mpsc::RecvError> {
            self.0.recv()
        }

        pub fn recv_timeout(
            self,
            timeout: std::time::Duration,
        ) -> Result<T, mpsc::RecvTimeoutError> {
            self.0.recv_timeout(timeout)
        }

        pub fn try_recv(&self) -> Result<T, mpsc::TryRecvError> {
            self.0.try_recv()
        }
    }

    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (Sender(tx), Receiver(rx))
    }
}

/// Handle to the plugin thread for sending requests
pub struct PluginThreadHandle {
    /// Channel to send requests to the plugin thread
    /// Wrapped in Option so we can drop it to signal shutdown
    request_sender: Option<tokio::sync::mpsc::UnboundedSender<PluginRequest>>,

    /// Thread join handle
    thread_handle: Option<JoinHandle<()>>,

    /// State snapshot handle for editor to update
    state_snapshot: Arc<RwLock<EditorStateSnapshot>>,

    /// Pending response senders for async operations (shared with runtime)
    pending_responses: PendingResponses,

    /// Receiver for plugin commands and their loader-owned context.
    command_receiver: std::sync::mpsc::Receiver<PluginCommandEnvelope>,

    /// Shared map of request_id → plugin_name for async resource creations.
    /// JsEditorApi inserts entries at creation time; deliver_response reads them
    /// when the editor confirms resource creation to track the actual IDs.
    async_resource_owners: AsyncResourceOwners,
    /// Loader-owned plugin instances whose contexts are currently live.
    active_plugin_instances: ActivePluginInstances,

    /// Streaming-search handle registry. JsEditorApi's `_beginSearch`
    /// inserts an `Arc<SearchHandleState>`; the editor's `BeginSearch`
    /// handler looks it up by handle id so its parallel searcher tasks
    /// write directly into the same shared state the JS side drains via
    /// `_searchHandleTake`.
    search_handles: SearchHandleRegistry,

    /// Shared registry of `event_handlers` so the editor thread can
    /// cheaply ask "does any plugin subscribe to hook X?" before doing
    /// expensive per-render work (e.g. building hook args). See
    /// `EventHandlerRegistry` in `quickjs_backend.rs`.
    event_handlers: crate::backend::quickjs_backend::EventHandlerRegistry,
}

impl PluginThreadHandle {
    /// Create a new plugin thread and return its handle
    pub fn spawn(services: Arc<dyn fresh_core::services::PluginServiceBridge>) -> Result<Self> {
        tracing::debug!("PluginThreadHandle::spawn: starting plugin thread creation");

        // Create channel for plugin commands
        let (command_sender, command_receiver) = std::sync::mpsc::channel();

        // Create editor state snapshot for query API
        let state_snapshot = Arc::new(RwLock::new(EditorStateSnapshot::new()));

        // Create pending responses map (shared between handle and runtime)
        let pending_responses: PendingResponses =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let thread_pending_responses = Arc::clone(&pending_responses);

        // Create async resource owners map (shared between handle and runtime)
        let async_resource_owners: AsyncResourceOwners =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let thread_async_resource_owners = Arc::clone(&async_resource_owners);
        let active_plugin_instances: ActivePluginInstances =
            Arc::new(std::sync::RwLock::new(std::collections::HashSet::new()));
        let thread_active_plugin_instances = Arc::clone(&active_plugin_instances);

        // Streaming-search handle registry shared with the editor thread.
        let search_handles: SearchHandleRegistry =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let thread_search_handles = Arc::clone(&search_handles);

        // Plugin event-handler registry, shared with the editor thread so
        // the renderer can skip expensive hook arg building when no plugin
        // subscribes.
        let event_handlers: crate::backend::quickjs_backend::EventHandlerRegistry =
            Arc::new(RwLock::new(std::collections::HashMap::new()));
        let thread_event_handlers = Arc::clone(&event_handlers);

        // Create channel for requests (unbounded allows sync send, async recv)
        let (request_sender, request_receiver) = tokio::sync::mpsc::unbounded_channel();

        // Clone state snapshot for the thread
        let thread_state_snapshot = Arc::clone(&state_snapshot);

        // Spawn the plugin thread
        tracing::debug!("PluginThreadHandle::spawn: spawning OS thread for plugin runtime");
        let thread_handle = thread::spawn(move || {
            tracing::debug!("Plugin thread: OS thread started, creating tokio runtime");
            // Create tokio runtime for the plugin thread
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    tracing::debug!("Plugin thread: tokio runtime created successfully");
                    rt
                }
                Err(e) => {
                    tracing::error!("Failed to create plugin thread runtime: {}", e);
                    return;
                }
            };

            // Create QuickJS runtime with state
            tracing::debug!("Plugin thread: creating QuickJS runtime");
            let runtime = match QuickJsBackend::with_state_responses_and_resources(
                Arc::clone(&thread_state_snapshot),
                command_sender,
                thread_pending_responses,
                services.clone(),
                thread_async_resource_owners,
                thread_active_plugin_instances,
                thread_search_handles,
                thread_event_handlers,
            ) {
                Ok(rt) => {
                    tracing::debug!("Plugin thread: QuickJS runtime created successfully");
                    rt
                }
                Err(e) => {
                    tracing::error!("Failed to create QuickJS runtime: {}", e);
                    return;
                }
            };

            // Create internal manager state
            let mut plugins: HashMap<String, TsPluginInfo> = HashMap::new();

            // Run the event loop with a LocalSet to allow concurrent task execution
            tracing::debug!("Plugin thread: starting event loop with LocalSet");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async {
                // Wrap runtime in RefCell for interior mutability during concurrent operations
                let runtime = Rc::new(RefCell::new(runtime));
                tracing::debug!("Plugin thread: entering plugin_thread_loop");
                plugin_thread_loop(runtime, &mut plugins, request_receiver).await;
            });

            tracing::info!("Plugin thread shutting down");
        });

        tracing::debug!("PluginThreadHandle::spawn: OS thread spawned, returning handle");
        tracing::info!("Plugin thread spawned");

        Ok(Self {
            request_sender: Some(request_sender),
            thread_handle: Some(thread_handle),
            state_snapshot,
            pending_responses,
            command_receiver,
            async_resource_owners,
            active_plugin_instances,
            search_handles,
            event_handlers,
        })
    }

    pub fn is_plugin_instance_active(&self, plugin_instance_id: PluginInstanceId) -> bool {
        self.active_plugin_instances
            .read()
            .map(|active| active.contains(&plugin_instance_id))
            .unwrap_or(false)
    }

    /// Accessor for the streaming-search handle registry.
    pub fn search_handles_handle(&self) -> SearchHandleRegistry {
        Arc::clone(&self.search_handles)
    }

    /// Non-blocking check: does any loaded plugin subscribe to `hook_name`?
    /// Used by the renderer to skip building expensive hook args (e.g.
    /// the full tokenized viewport) when
    /// nothing would consume them. Reads from the shared
    /// `event_handlers` registry directly — no channel round-trip.
    pub fn has_subscribers(&self, hook_name: &str) -> bool {
        self.event_handlers
            .read()
            .map(|h| h.get(hook_name).is_some_and(|v| !v.is_empty()))
            .unwrap_or(false)
    }
    /// Non-blocking targeted variant used before building a private invocation
    /// snapshot for a hook that only one plugin may consume.
    pub fn has_subscriber(&self, plugin: &str, hook_name: &str) -> bool {
        self.event_handlers
            .read()
            .map(|handlers| {
                handlers.get(hook_name).is_some_and(|handlers| {
                    handlers.iter().any(|handler| handler.plugin_name == plugin)
                })
            })
            .unwrap_or(false)
    }

    /// Check if the plugin thread is still alive
    pub fn is_alive(&self) -> bool {
        self.thread_handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// Check thread health and panic if the plugin thread died due to a panic.
    /// This propagates plugin thread panics to the calling thread.
    /// Call this periodically to detect plugin thread failures.
    pub fn check_thread_health(&mut self) {
        if let Some(handle) = &self.thread_handle {
            if handle.is_finished() {
                tracing::error!(
                    "check_thread_health: plugin thread is finished, checking for panic"
                );
                // Thread finished - take ownership and check result
                if let Some(handle) = self.thread_handle.take() {
                    match handle.join() {
                        Ok(()) => {
                            tracing::warn!("Plugin thread exited normally (unexpected)");
                        }
                        Err(panic_payload) => {
                            // Re-panic with the original panic message to propagate it
                            std::panic::resume_unwind(panic_payload);
                        }
                    }
                }
            }
        }
    }

    /// Deliver a response to a pending async operation in the plugin
    ///
    /// This is called by the editor after processing a command that requires a response.
    pub fn deliver_response(&self, response: fresh_core::api::PluginResponse) {
        // First try to find a pending Rust request (oneshot channel)
        if respond_to_pending(&self.pending_responses, response.clone()) {
            return;
        }

        // If not found, it must be a JS callback
        use fresh_core::api::PluginResponse;

        match response {
            PluginResponse::VirtualBufferCreated {
                request_id,
                buffer_id,
                split_id,
            } => {
                // Track the created buffer for cleanup on plugin unload
                self.track_async_resource(
                    request_id,
                    TrackedAsyncResource::VirtualBuffer(buffer_id),
                );
                // Return an object with bufferId and splitId (camelCase for JS)
                let result = serde_json::json!({
                    "bufferId": buffer_id.0,
                    "splitId": split_id.map(|s| s.0)
                });
                self.resolve_callback(JsCallbackId(request_id), result.to_string());
            }
            PluginResponse::LspRequest { request_id, result } => match result {
                Ok(value) => {
                    self.resolve_callback(JsCallbackId(request_id), value.to_string());
                }
                Err(e) => {
                    self.reject_callback(JsCallbackId(request_id), e);
                }
            },
            PluginResponse::HighlightsComputed { request_id, spans } => {
                self.resolve_json_callback(request_id, &spans, "[]");
            }
            PluginResponse::BufferText { request_id, text } => match text {
                Ok(content) => {
                    // JSON stringify the content string
                    let result =
                        serde_json::to_string(&content).unwrap_or_else(|_| "\"\"".to_string());
                    self.resolve_callback(JsCallbackId(request_id), result);
                }
                Err(e) => {
                    self.reject_callback(JsCallbackId(request_id), e);
                }
            },
            PluginResponse::SplitWindowCreated { request_id, result } => match result {
                Ok(split) => {
                    let json = serde_json::to_string(&split)
                        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                    self.resolve_callback(JsCallbackId(request_id), json);
                }
                // A failed split rejects rather than resolving to null: the
                // caller asked for a pane and there isn't one, which should
                // stop a script rather than have it carry on against an id
                // that was never created.
                Err(e) => self.reject_callback(JsCallbackId(request_id), e),
            },
            PluginResponse::SnapshotSynced { request_id } => {
                self.resolve_callback(JsCallbackId(request_id), "null".to_string());
            }
            PluginResponse::CompositeBufferCreated {
                request_id,
                buffer_id,
            } => {
                // Track the created buffer for cleanup on plugin unload
                self.track_async_resource(
                    request_id,
                    TrackedAsyncResource::CompositeBuffer(buffer_id),
                );
                // Return just the buffer_id number, not an object
                self.resolve_callback(JsCallbackId(request_id), buffer_id.0.to_string());
            }
            PluginResponse::LineStartPosition {
                request_id,
                position,
            } => {
                self.resolve_json_callback(request_id, position, "null");
            }
            PluginResponse::LineEndPosition {
                request_id,
                position,
            } => {
                self.resolve_json_callback(request_id, position, "null");
            }
            PluginResponse::BufferLineCount { request_id, count } => {
                self.resolve_json_callback(request_id, count, "null");
            }
            PluginResponse::TerminalCreated {
                request_id,
                buffer_id,
                terminal_id,
                split_id,
            } => {
                self.track_async_resource(request_id, TrackedAsyncResource::Terminal(terminal_id));
                let result = serde_json::json!({
                    "bufferId": buffer_id.0,
                    "terminalId": terminal_id,
                    "splitId": split_id.map(|s| s.0)
                });
                self.resolve_callback(JsCallbackId(request_id), result.to_string());
            }
            PluginResponse::WindowWithTerminalCreated { request_id, result } => {
                self.resolve_json_callback(request_id, result, "null");
            }
            PluginResponse::SplitByLabel {
                request_id,
                split_id,
            } => {
                self.resolve_json_callback(request_id, split_id.map(|s| s.0), "null");
            }
            PluginResponse::WatchPathRegistered { request_id, result } => match result {
                Ok(handle) => {
                    self.track_async_resource(
                        request_id,
                        TrackedAsyncResource::WatchHandle(handle),
                    );
                    self.resolve_callback(JsCallbackId(request_id), handle.to_string());
                }
                Err(e) => {
                    self.forget_async_resource(request_id);
                    self.reject_callback(JsCallbackId(request_id), e);
                }
            },
        }
    }

    /// Serialize `value` to JSON and resolve a JS callback with the result.
    /// Uses `fallback` as the JSON string if serialization fails.
    fn resolve_json_callback(&self, request_id: u64, value: impl serde::Serialize, fallback: &str) {
        let result = serde_json::to_string(&value).unwrap_or_else(|_| fallback.to_string());
        self.resolve_callback(JsCallbackId(request_id), result);
    }

    /// Move the exact owner of `request_id` back to the plugin thread so the
    /// current instance records it or a stale instance compensates immediately.
    fn track_async_resource(&self, request_id: u64, resource: TrackedAsyncResource) {
        let owner = self
            .async_resource_owners
            .lock()
            .ok()
            .and_then(|mut owners| owners.remove(&request_id));
        if let Some(owner) = owner {
            if let Some(sender) = self.request_sender.as_ref() {
                fire_and_forget(sender.send(PluginRequest::TrackAsyncResource { owner, resource }));
            }
        }
    }

    fn forget_async_resource(&self, request_id: u64) {
        if let Ok(mut owners) = self.async_resource_owners.lock() {
            owners.remove(&request_id);
        }
    }

    pub fn load_plugin(&self, path: &Path) -> Result<()> {
        self.load_plugin_with_kind(path, PluginLoadKind::External)
    }

    pub fn load_plugin_with_kind(&self, path: &Path, kind: PluginLoadKind) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::LoadPlugin {
                path: path.to_path_buf(),
                kind,
                response: tx,
            })
            .map_err(|_| anyhow!("Plugin thread not responding"))?;

        rx.recv().map_err(|_| anyhow!("Plugin thread closed"))?
    }

    /// Load all plugins from a directory (blocking)
    pub fn load_plugins_from_dir(&self, dir: &Path) -> Vec<String> {
        self.load_plugins_from_dir_with_kind(dir, PluginLoadKind::External)
    }

    pub fn load_plugins_from_dir_with_kind(&self, dir: &Path, kind: PluginLoadKind) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        let Some(sender) = self.request_sender.as_ref() else {
            return vec!["Plugin thread shut down".to_string()];
        };
        if sender
            .send(PluginRequest::LoadPluginsFromDir {
                dir: dir.to_path_buf(),
                kind,
                response: tx,
            })
            .is_err()
        {
            return vec!["Plugin thread not responding".to_string()];
        }

        rx.recv()
            .unwrap_or_else(|_| vec!["Plugin thread closed".to_string()])
    }

    /// Load all plugins from a directory with config support (blocking)
    /// Returns (errors, discovered_plugins) where discovered_plugins is a map of
    /// plugin name -> PluginConfig with paths populated.
    pub fn load_plugins_from_dir_with_config(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
    ) -> (Vec<String>, HashMap<String, PluginConfig>) {
        self.load_plugins_from_dir_with_config_and_kind(
            dir,
            plugin_configs,
            PluginLoadKind::External,
        )
    }

    pub fn load_plugins_from_dir_with_config_and_kind(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
        kind: PluginLoadKind,
    ) -> (Vec<String>, HashMap<String, PluginConfig>) {
        let (tx, rx) = oneshot::channel();
        let Some(sender) = self.request_sender.as_ref() else {
            return (vec!["Plugin thread shut down".to_string()], HashMap::new());
        };
        if sender
            .send(PluginRequest::LoadPluginsFromDirWithConfig {
                dir: dir.to_path_buf(),
                plugin_configs: plugin_configs.clone(),
                kind,
                response: tx,
            })
            .is_err()
        {
            return (
                vec!["Plugin thread not responding".to_string()],
                HashMap::new(),
            );
        }

        rx.recv()
            .unwrap_or_else(|_| (vec!["Plugin thread closed".to_string()], HashMap::new()))
    }

    /// Load a plugin from source code directly (blocking).
    ///
    /// If a plugin with the same name is already loaded, it will be unloaded first
    /// (hot-reload semantics).
    pub fn load_plugin_from_source(
        &self,
        source: &str,
        name: &str,
        is_typescript: bool,
    ) -> Result<()> {
        self.load_plugin_from_source_with_kind(
            source,
            name,
            is_typescript,
            PluginLoadKind::External,
        )
    }

    pub fn load_plugin_from_source_with_kind(
        &self,
        source: &str,
        name: &str,
        is_typescript: bool,
        kind: PluginLoadKind,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::LoadPluginFromSource {
                source: source.to_string(),
                name: name.to_string(),
                is_typescript,
                kind,
                response: tx,
            })
            .map_err(|_| anyhow!("Plugin thread not responding"))?;

        rx.recv().map_err(|_| anyhow!("Plugin thread closed"))?
    }

    /// Unload a plugin (blocking)
    pub fn unload_plugin(&self, name: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::UnloadPlugin {
                name: name.to_string(),
                response: tx,
            })
            .map_err(|_| anyhow!("Plugin thread not responding"))?;

        rx.recv().map_err(|_| anyhow!("Plugin thread closed"))?
    }

    /// Queue a plugin unload without blocking the caller.
    pub fn unload_plugin_request(&self, name: &str) -> Result<()> {
        let (tx, _rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::UnloadPlugin {
                name: name.to_string(),
                response: tx,
            })
            .map_err(|_| anyhow!("Plugin thread not responding"))
    }

    /// Reload a plugin (blocking)
    pub fn reload_plugin(&self, name: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::ReloadPlugin {
                name: name.to_string(),
                response: tx,
            })
            .map_err(|_| anyhow!("Plugin thread not responding"))?;

        rx.recv().map_err(|_| anyhow!("Plugin thread closed"))?
    }

    /// Execute a plugin action (non-blocking)
    ///
    /// Returns a receiver that will receive the result when the action completes.
    /// The caller should poll this while processing commands to avoid deadlock.
    pub fn execute_action_async(
        &self,
        action_name: &str,
        args_json: Option<String>,
        request_id: Option<u64>,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        self.execute_action_async_with_invocation(action_name, args_json, request_id, None)
    }

    pub fn execute_action_async_with_invocation(
        &self,
        action_name: &str,
        args_json: Option<String>,
        request_id: Option<u64>,
        invocation: Option<PluginInvocation>,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        tracing::trace!("execute_action_async: starting action '{}'", action_name);
        let (tx, rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::ExecuteAction {
                action_name: action_name.to_string(),
                args_json,
                request_id,
                invocation,
                response: tx,
            })
            .map_err(|_| anyhow!("Plugin thread not responding"))?;

        tracing::trace!("execute_action_async: request sent for '{}'", action_name);
        Ok(rx)
    }

    /// Run a hook (non-blocking, fire-and-forget). Returns whether the request
    /// was enqueued; hooks without subscribers never enter the runtime queue.
    pub fn run_hook(&self, hook_name: &str, args: HookArgs) -> bool {
        self.run_hook_with_invocation(hook_name, args, None)
    }

    pub fn run_hook_with_invocation(
        &self,
        hook_name: &str,
        args: HookArgs,
        invocation: Option<PluginInvocation>,
    ) -> bool {
        if !self.has_subscribers(hook_name) {
            return false;
        }
        self.request_sender.as_ref().is_some_and(|sender| {
            sender
                .send(PluginRequest::RunHook {
                    hook_name: hook_name.to_string(),
                    args,
                    target: None,
                    invocation,
                })
                .is_ok()
        })
    }

    /// Run a hook in a single plugin's context only.
    pub fn run_hook_for_plugin(&self, plugin: &str, hook_name: &str, args: HookArgs) -> bool {
        self.run_hook_for_plugin_with_invocation(plugin, hook_name, args, None)
    }

    pub fn run_hook_for_plugin_with_invocation(
        &self,
        plugin: &str,
        hook_name: &str,
        args: HookArgs,
        invocation: Option<PluginInvocation>,
    ) -> bool {
        if !self.has_subscriber(plugin, hook_name) {
            return false;
        }
        self.request_sender.as_ref().is_some_and(|sender| {
            sender
                .send(PluginRequest::RunHook {
                    hook_name: hook_name.to_string(),
                    args,
                    target: Some(plugin.to_string()),
                    invocation,
                })
                .is_ok()
        })
    }

    /// Check if any handlers are registered for a hook (blocking)
    pub fn has_hook_handlers(&self, hook_name: &str) -> bool {
        let (tx, rx) = oneshot::channel();
        let Some(sender) = self.request_sender.as_ref() else {
            return false;
        };
        if sender
            .send(PluginRequest::HasHookHandlers {
                hook_name: hook_name.to_string(),
                response: tx,
            })
            .is_err()
        {
            return false;
        }

        rx.recv().unwrap_or(false)
    }

    /// List all loaded plugins (blocking)
    pub fn list_plugins(&self) -> Vec<TsPluginInfo> {
        let (tx, rx) = oneshot::channel();
        let Some(sender) = self.request_sender.as_ref() else {
            return vec![];
        };
        if sender
            .send(PluginRequest::ListPlugins { response: tx })
            .is_err()
        {
            return vec![];
        }

        rx.recv().unwrap_or_default()
    }

    /// Submit a "load plugins from dir with config" request without blocking.
    /// Returns the response receiver for the caller to await elsewhere
    /// (typically a forwarder thread that bridges to `AsyncBridge`).
    pub fn load_plugins_from_dir_with_config_request(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
    ) -> Result<oneshot::Receiver<PluginsDirLoadResult>> {
        self.load_plugins_from_dir_with_config_request_and_kind(
            dir,
            plugin_configs,
            PluginLoadKind::External,
        )
    }

    pub fn load_plugins_from_dir_with_config_request_and_kind(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
        kind: PluginLoadKind,
    ) -> Result<oneshot::Receiver<PluginsDirLoadResult>> {
        let (tx, rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::LoadPluginsFromDirWithConfig {
                dir: dir.to_path_buf(),
                plugin_configs: plugin_configs.clone(),
                kind,
                response: tx,
            })
            .map_err(|_| anyhow!("Plugin thread not responding"))?;
        Ok(rx)
    }

    /// Submit a "load plugin from source" request without blocking.
    /// Returns the response receiver.
    pub fn load_plugin_from_source_request(
        &self,
        source: &str,
        name: &str,
        is_typescript: bool,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        self.load_plugin_from_source_request_with_kind(
            source,
            name,
            is_typescript,
            PluginLoadKind::External,
        )
    }

    pub fn load_plugin_from_source_request_with_kind(
        &self,
        source: &str,
        name: &str,
        is_typescript: bool,
        kind: PluginLoadKind,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        let (tx, rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::LoadPluginFromSource {
                source: source.to_string(),
                name: name.to_string(),
                is_typescript,
                kind,
                response: tx,
            })
            .map_err(|_| anyhow!("Plugin thread not responding"))?;
        Ok(rx)
    }

    /// Submit a "list plugins" request without blocking. The plugin thread
    /// processes requests FIFO, so submitting this immediately after a
    /// batch of `LoadPluginsFromDirWithConfig` guarantees the response
    /// reflects all of those loads having completed.
    pub fn list_plugins_request(&self) -> Result<oneshot::Receiver<Vec<TsPluginInfo>>> {
        let (tx, rx) = oneshot::channel();
        self.request_sender
            .as_ref()
            .ok_or_else(|| anyhow!("Plugin thread shut down"))?
            .send(PluginRequest::ListPlugins { response: tx })
            .map_err(|_| anyhow!("Plugin thread not responding"))?;
        Ok(rx)
    }

    /// Process pending plugin commands and their loader-owned security context.
    pub fn process_command_envelopes(&mut self) -> Vec<PluginCommandEnvelope> {
        self.command_receiver.try_iter().collect()
    }

    /// Get the state snapshot handle for editor to update
    pub fn state_snapshot_handle(&self) -> Arc<RwLock<EditorStateSnapshot>> {
        Arc::clone(&self.state_snapshot)
    }

    /// Shutdown the plugin thread
    pub fn shutdown(&mut self) {
        tracing::debug!("PluginThreadHandle::shutdown: starting shutdown");

        // Drop all pending response senders - this wakes up any plugin code waiting for responses
        // by causing their oneshot receivers to return an error
        if let Ok(mut pending) = self.pending_responses.lock() {
            if !pending.is_empty() {
                tracing::warn!(
                    "PluginThreadHandle::shutdown: dropping {} pending responses: {:?}",
                    pending.len(),
                    pending.keys().collect::<Vec<_>>()
                );
                pending.clear(); // Drop all senders, waking up waiting receivers
            }
        }

        // First send a Shutdown request to allow clean processing of pending work
        if let Some(sender) = self.request_sender.as_ref() {
            tracing::debug!("PluginThreadHandle::shutdown: sending Shutdown request");
            fire_and_forget(sender.send(PluginRequest::Shutdown));
        }

        // Then drop the sender to close the channel - this reliably wakes the receiver
        // even when it's parked in a tokio LocalSet (the Shutdown message above may not wake it)
        tracing::debug!("PluginThreadHandle::shutdown: dropping request_sender to close channel");
        self.request_sender.take();

        if let Some(handle) = self.thread_handle.take() {
            tracing::debug!("PluginThreadHandle::shutdown: joining plugin thread");
            if handle.join().is_err() {
                tracing::trace!("plugin thread panicked during join");
            }
            tracing::debug!("PluginThreadHandle::shutdown: plugin thread joined");
        }

        tracing::debug!("PluginThreadHandle::shutdown: shutdown complete");
    }

    /// Resolve an async callback in the recorded owning plugin instance.
    pub fn resolve_callback(
        &self,
        callback_id: fresh_core::api::JsCallbackId,
        result_json: String,
    ) {
        self.send_callback_resolution(None, callback_id, result_json);
    }

    pub fn resolve_callback_for(
        &self,
        plugin_instance_id: PluginInstanceId,
        callback_id: fresh_core::api::JsCallbackId,
        result_json: String,
    ) {
        self.send_callback_resolution(Some(plugin_instance_id), callback_id, result_json);
    }

    fn send_callback_resolution(
        &self,
        expected_instance: Option<PluginInstanceId>,
        callback_id: fresh_core::api::JsCallbackId,
        result_json: String,
    ) {
        if let Some(sender) = self.request_sender.as_ref() {
            fire_and_forget(sender.send(PluginRequest::ResolveCallback {
                expected_instance,
                callback_id,
                result_json,
            }));
        }
    }

    /// Reject an async callback in the recorded owning plugin instance.
    pub fn reject_callback(&self, callback_id: fresh_core::api::JsCallbackId, error: String) {
        self.send_callback_rejection(None, callback_id, error);
    }

    pub fn reject_callback_for(
        &self,
        plugin_instance_id: PluginInstanceId,
        callback_id: fresh_core::api::JsCallbackId,
        error: String,
    ) {
        self.send_callback_rejection(Some(plugin_instance_id), callback_id, error);
    }

    fn send_callback_rejection(
        &self,
        expected_instance: Option<PluginInstanceId>,
        callback_id: fresh_core::api::JsCallbackId,
        error: String,
    ) {
        if let Some(sender) = self.request_sender.as_ref() {
            fire_and_forget(sender.send(PluginRequest::RejectCallback {
                expected_instance,
                callback_id,
                error,
            }));
        }
    }
}

impl Drop for PluginThreadHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn respond_to_pending(
    pending_responses: &PendingResponses,
    response: fresh_core::api::PluginResponse,
) -> bool {
    let request_id = response.request_id();
    let sender = {
        let mut pending = pending_responses.lock().unwrap();
        pending.remove(&request_id)
    };

    if let Some(tx) = sender {
        fire_and_forget(tx.send(response));
        true
    } else {
        false
    }
}

#[cfg(test)]
mod plugin_thread_tests {
    use super::*;
    use fresh_core::api::PluginResponse;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    #[test]
    fn respond_to_pending_sends_lsp_response() {
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().insert(123, tx);

        respond_to_pending(
            &pending,
            PluginResponse::LspRequest {
                request_id: 123,
                result: Ok(json!({ "key": "value" })),
            },
        );

        let response = rx.try_recv().expect("expected response");
        match response {
            PluginResponse::LspRequest { result, .. } => {
                assert_eq!(result.unwrap(), json!({ "key": "value" }));
            }
            _ => panic!("unexpected variant"),
        }

        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn respond_to_pending_handles_virtual_buffer_created() {
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = oneshot::channel();
        pending.lock().unwrap().insert(456, tx);

        respond_to_pending(
            &pending,
            PluginResponse::VirtualBufferCreated {
                request_id: 456,
                buffer_id: fresh_core::BufferId(7),
                split_id: Some(fresh_core::SplitId(1)),
            },
        );

        let response = rx.try_recv().expect("expected response");
        match response {
            PluginResponse::VirtualBufferCreated { buffer_id, .. } => {
                assert_eq!(buffer_id.0, 7);
            }
            _ => panic!("unexpected variant"),
        }

        assert!(pending.lock().unwrap().is_empty());
    }
}

const MAX_READY_REQUESTS_BEFORE_EVENT_LOOP_POLL: usize = 32;

/// Main loop for the plugin thread
///
/// Uses `tokio::select!` to interleave request handling with periodic event loop
/// polling. This allows long-running promises (like process spawns) to make progress
/// even when no requests are coming in, preventing the UI from getting stuck.
async fn plugin_thread_loop(
    runtime: Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
    mut request_receiver: tokio::sync::mpsc::UnboundedReceiver<PluginRequest>,
) {
    tracing::info!("Plugin thread event loop started");

    // Poll promptly when idle, and also after a bounded run of ready requests.
    let poll_interval = Duration::from_millis(1);
    let mut has_pending_work = false;
    let mut requests_since_poll = 0;

    loop {
        // Check for fatal JS errors (e.g., unhandled promise rejections in test mode)
        // These are set via set_fatal_js_error() because panicking inside FFI callbacks
        // is caught by rquickjs and doesn't terminate the thread.
        if crate::backend::has_fatal_js_error() {
            if let Some(error_msg) = crate::backend::take_fatal_js_error() {
                tracing::error!(
                    "Fatal JS error detected, terminating plugin thread: {}",
                    error_msg
                );
                panic!("Fatal plugin error: {}", error_msg);
            }
        }
        if has_pending_work && requests_since_poll >= MAX_READY_REQUESTS_BEFORE_EVENT_LOOP_POLL {
            has_pending_work = runtime.borrow_mut().poll_event_loop_once();
            requests_since_poll = 0;
        }

        tokio::select! {
            biased; // Prefer requests only within the bounded batch above.

            request = request_receiver.recv() => {
                match request {
                    Some(PluginRequest::ExecuteAction {
                        action_name,
                        args_json,
                        request_id,
                        invocation,
                        response,
                    }) => {
                        // Start the action without blocking - this allows us to process
                        // ResolveCallback requests that the action may be waiting for.
                        let result = runtime.borrow_mut().start_action(
                            &action_name,
                            args_json.as_deref(),
                            request_id,
                            invocation,
                        );
                        fire_and_forget(response.send(result));
                        has_pending_work = true; // Action may have started async work
                    }
                    Some(request) => {
                        let should_shutdown =
                            handle_request(request, Rc::clone(&runtime), plugins).await;

                        if should_shutdown {
                            break;
                        }
                        has_pending_work = true; // Request may have started async work
                    }
                    None => {
                        // Channel closed
                        tracing::info!("Plugin thread request channel closed");
                        break;
                    }
                }
                requests_since_poll += 1;
            }

            // Poll the JS event loop periodically to make progress on pending promises
            _ = tokio::time::sleep(poll_interval), if has_pending_work => {
                has_pending_work = runtime.borrow_mut().poll_event_loop_once();
                requests_since_poll = 0;
            }
        }
    }
}

/// Run a hook with Rc<RefCell<QuickJsBackend>>
///
/// # Safety (clippy::await_holding_refcell_ref)
/// The RefCell borrow held across await is safe because:
/// - This runs on a single-threaded tokio runtime (no parallel task execution)
/// - No spawn_local calls exist that could create concurrent access to `runtime`
/// - The runtime Rc<RefCell<>> is never shared with other concurrent tasks
#[allow(clippy::await_holding_refcell_ref)]
async fn run_hook_internal_rc(
    runtime: Rc<RefCell<QuickJsBackend>>,
    hook_name: &str,
    args: &HookArgs,
    target: Option<&str>,
    invocation: Option<PluginInvocation>,
) -> Result<()> {
    // Convert HookArgs to serde_json::Value using hook_args_to_json which produces flat JSON
    // (not enum-tagged JSON from serde's default Serialize)
    let json_start = std::time::Instant::now();
    let json_data = fresh_core::hooks::hook_args_to_json(args)?;
    tracing::trace!(
        hook = hook_name,
        json_us = json_start.elapsed().as_micros(),
        "hook args serialized"
    );

    // Emit to TypeScript handlers
    let emit_start = std::time::Instant::now();
    runtime
        .borrow_mut()
        .emit_to(hook_name, &json_data, target, invocation)
        .await?;
    tracing::trace!(
        hook = hook_name,
        emit_ms = emit_start.elapsed().as_millis(),
        "emit completed"
    );

    Ok(())
}

/// Handle a single request in the plugin thread
#[allow(clippy::await_holding_refcell_ref)]
async fn handle_request(
    request: PluginRequest,
    runtime: Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
) -> bool {
    match request {
        PluginRequest::LoadPlugin {
            path,
            kind,
            response,
        } => {
            let result = load_plugin_internal(Rc::clone(&runtime), plugins, &path, kind).await;
            fire_and_forget(response.send(result));
        }

        PluginRequest::LoadPluginsFromDir {
            dir,
            kind,
            response,
        } => {
            let errors =
                load_plugins_from_dir_internal(Rc::clone(&runtime), plugins, &dir, kind).await;
            fire_and_forget(response.send(errors));
        }

        PluginRequest::LoadPluginsFromDirWithConfig {
            dir,
            kind,
            plugin_configs,
            response,
        } => {
            let (errors, discovered) = load_plugins_from_dir_with_config_internal(
                Rc::clone(&runtime),
                plugins,
                &dir,
                &plugin_configs,
                kind,
            )
            .await;
            fire_and_forget(response.send((errors, discovered)));
        }

        PluginRequest::LoadPluginFromSource {
            source,
            name,
            is_typescript,
            kind,
            response,
        } => {
            let result = load_plugin_from_source_internal(
                Rc::clone(&runtime),
                plugins,
                &source,
                &name,
                is_typescript,
                kind,
            );
            fire_and_forget(response.send(result));
        }

        PluginRequest::UnloadPlugin { name, response } => {
            let result = unload_plugin_internal(Rc::clone(&runtime), plugins, &name);
            fire_and_forget(response.send(result));
        }

        PluginRequest::ReloadPlugin { name, response } => {
            let result = reload_plugin_internal(Rc::clone(&runtime), plugins, &name).await;
            fire_and_forget(response.send(result));
        }

        PluginRequest::ExecuteAction {
            action_name,
            response,
            ..
        } => {
            // This is handled in plugin_thread_loop with select! for concurrent processing
            // If we get here, it's an unexpected state
            tracing::error!(
                "ExecuteAction should be handled in main loop, not here: {}",
                action_name
            );
            fire_and_forget(response.send(Err(anyhow::anyhow!(
                "Internal error: ExecuteAction in wrong handler"
            ))));
        }

        PluginRequest::RunHook {
            hook_name,
            args,
            target,
            invocation,
        } => {
            // Fire-and-forget hook execution
            let hook_start = std::time::Instant::now();
            // Use info level for prompt hooks to aid debugging
            if hook_name == "prompt_confirmed" || hook_name == "prompt_cancelled" {
                tracing::info!(hook = %hook_name, ?args, "RunHook request received (prompt hook)");
            } else {
                tracing::trace!(hook = %hook_name, "RunHook request received");
            }
            if let Err(e) = run_hook_internal_rc(
                Rc::clone(&runtime),
                &hook_name,
                &args,
                target.as_deref(),
                invocation.clone(),
            )
            .await
            {
                let error_msg = format!("Plugin error in '{}': {}", hook_name, e);
                tracing::error!("{}", error_msg);
                // Surface the error to the UI
                runtime.borrow_mut().send_status(error_msg);
            }
            // Send sentinel so the main thread can wait deterministically
            // for all commands from this hook to be available.
            runtime
                .borrow()
                .send_hook_completed(hook_name.clone(), invocation);
            if hook_name == "prompt_confirmed" || hook_name == "prompt_cancelled" {
                tracing::info!(
                    hook = %hook_name,
                    elapsed_ms = hook_start.elapsed().as_millis(),
                    "RunHook completed (prompt hook)"
                );
            } else {
                tracing::trace!(
                    hook = %hook_name,
                    elapsed_ms = hook_start.elapsed().as_millis(),
                    "RunHook completed"
                );
            }
        }

        PluginRequest::HasHookHandlers {
            hook_name,
            response,
        } => {
            let has_handlers = runtime.borrow().has_handlers(&hook_name);
            fire_and_forget(response.send(has_handlers));
        }

        PluginRequest::ListPlugins { response } => {
            let plugin_list: Vec<TsPluginInfo> = plugins.values().cloned().collect();
            fire_and_forget(response.send(plugin_list));
        }

        PluginRequest::ResolveCallback {
            expected_instance,
            callback_id,
            result_json,
        } => {
            tracing::trace!(%callback_id, "resolving plugin callback");
            let mut runtime = runtime.borrow_mut();
            if let Some(plugin_instance_id) = expected_instance {
                runtime.resolve_callback_for(plugin_instance_id, callback_id, &result_json);
            } else {
                runtime.resolve_callback(callback_id, &result_json);
            }
        }

        PluginRequest::RejectCallback {
            expected_instance,
            callback_id,
            error,
        } => {
            let mut runtime = runtime.borrow_mut();
            if let Some(plugin_instance_id) = expected_instance {
                runtime.reject_callback_for(plugin_instance_id, callback_id, &error);
            } else {
                runtime.reject_callback(callback_id, &error);
            }
        }

        PluginRequest::TrackAsyncResource { owner, resource } => {
            runtime
                .borrow()
                .record_or_cleanup_async_resource(owner, resource);
        }

        PluginRequest::Shutdown => {
            tracing::info!("Plugin thread received shutdown request");
            return true;
        }
    }

    false
}

/// Result of the parallel preparation phase for a single plugin.
/// Contains everything needed to execute the plugin — no further I/O or transpilation required.
struct PreparedPlugin {
    name: String,
    path: PathBuf,
    js_code: String,
    i18n: Option<HashMap<String, HashMap<String, String>>>,
    dependencies: Vec<String>,
    trusted_builtin: Option<fresh_core::api::TrustedBuiltinPlugin>,
    /// `.d.ts` emit for the plugin source, produced by oxc's
    /// isolated-declarations transformer. Present on every successful
    /// TS/JS prepare; callers can use it to assemble a consolidated
    /// plugins.d.ts so init.ts/other plugins can reach each plugin's
    /// public types without manual `as`-casts. `None` only when
    /// isolated-declarations emit failed outright — the plugin still
    /// loads at runtime.
    declarations: Option<String>,
}

/// Prepare a plugin for execution: read source, transpile, extract dependencies.
///
/// This function does I/O and CPU-bound work only — no QuickJS interaction.
/// It is safe to call from any thread (all inputs/outputs are Send).
fn prepare_plugin(path: &Path, kind: &PluginLoadKind) -> Result<PreparedPlugin> {
    let plugin_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("Invalid plugin filename"))?
        .to_string();
    let trusted_spec = kind.verified_trusted_builtin(&plugin_name, path)?;

    let source = if let Some(spec) = &trusted_spec {
        let bytes = spec
            .sources()
            .get(spec.entrypoint())
            .expect("trusted entrypoint is present in its source map");
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|error| anyhow!("Trusted plugin {} is not UTF-8: {error}", path.display()))?
    } else {
        std::fs::read_to_string(path)
            .map_err(|error| anyhow!("Failed to read plugin {}: {error}", path.display()))?
    };

    let filename = trusted_spec
        .as_ref()
        .map(|spec| spec.entrypoint())
        .unwrap_or(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("plugin.ts");

    // Extract dependencies before transpilation
    let dependencies = fresh_parser_js::extract_plugin_dependencies(&source);

    // Emit `.d.ts` via oxc's isolated-declarations before the
    // transpile step consumes `source`. We want the raw TS (every
    // `export type`, `export interface`, and `declare global` block
    // the plugin author wrote) so downstream plugins and init.ts
    // reach the plugin's public types without casts. Failures are
    // non-fatal — the plugin still runs.
    let declarations = if filename.ends_with(".ts") {
        match fresh_parser_js::emit_isolated_declarations(&source, filename) {
            Ok(dts) => Some(dts),
            Err(e) => {
                tracing::warn!(
                    "Plugin {} isolated-declarations emit failed: {}",
                    path.display(),
                    e
                );
                None
            }
        }
    } else {
        None
    };

    // Trusted built-ins bundle only the immutable bytes compiled into the host.
    let js_code = if fresh_parser_js::has_es_imports(&source) {
        let bundled = match &trusted_spec {
            Some(spec) => {
                fresh_parser_js::bundle_module_from_sources(spec.entrypoint(), spec.sources())
            }
            None => fresh_parser_js::bundle_module(path),
        };
        match bundled {
            Ok(bundled) => bundled,
            Err(error) => {
                tracing::warn!(
                    "Plugin {} uses ES imports but bundling failed: {}. Skipping.",
                    path.display(),
                    error
                );
                return Err(anyhow!("Bundling failed for {}: {}", plugin_name, error));
            }
        }
    } else if fresh_parser_js::has_es_module_syntax(&source) {
        let stripped = fresh_parser_js::strip_imports_and_exports(&source);
        if filename.ends_with(".ts") {
            fresh_parser_js::transpile_typescript(&stripped, filename)?
        } else {
            stripped
        }
    } else if filename.ends_with(".ts") {
        fresh_parser_js::transpile_typescript(&source, filename)?
    } else {
        source
    };

    let i18n = if let Some(spec) = &trusted_spec {
        spec.sources()
            .get(&spec.entrypoint().with_extension("i18n.json"))
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|content| serde_json::from_str(content).ok())
    } else {
        let i18n_path = path.with_extension("i18n.json");
        if i18n_path.exists() {
            std::fs::read_to_string(&i18n_path)
                .ok()
                .and_then(|content| serde_json::from_str(&content).ok())
        } else {
            None
        }
    };
    Ok(PreparedPlugin {
        name: plugin_name,
        path: path.to_path_buf(),
        js_code,
        i18n,
        dependencies,
        trusted_builtin: trusted_spec.as_ref().map(|spec| spec.identity()),
        declarations,
    })
}

/// Execute a pre-prepared plugin in QuickJS. This is the serial phase —
/// must run on the plugin thread.
fn execute_prepared_plugin(
    runtime: &Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
    prepared: &PreparedPlugin,
    kind: PluginLoadKind,
) -> Result<()> {
    if kind.trusted_builtin(&prepared.name) != prepared.trusted_builtin {
        return Err(anyhow!(
            "trusted built-in attestation changed after preparation"
        ));
    }
    runtime
        .borrow()
        .validate_plugin_load(&prepared.name, &kind)?;
    if plugins.contains_key(&prepared.name) {
        unload_plugin_internal(Rc::clone(runtime), plugins, &prepared.name)?;
    }
    runtime.borrow().prepare_plugin_load(&prepared.name, kind)?;

    let result = (|| {
        if let Some(i18n) = &prepared.i18n {
            runtime
                .borrow_mut()
                .services
                .register_plugin_strings(&prepared.name, i18n.clone());
        }

        let path_str = prepared
            .path
            .to_str()
            .ok_or_else(|| anyhow!("Invalid path encoding"))?;
        runtime
            .borrow_mut()
            .execute_js(&prepared.js_code, path_str)?;

        plugins.insert(
            prepared.name.clone(),
            TsPluginInfo {
                name: prepared.name.clone(),
                path: prepared.path.clone(),
                enabled: true,
                declarations: prepared.declarations.clone(),
            },
        );
        Ok(())
    })();

    if result.is_err() {
        runtime.borrow().abandon_plugin_load(&prepared.name);
    }
    result
}

#[allow(clippy::await_holding_refcell_ref)]
async fn load_plugin_internal(
    runtime: Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
    path: &Path,
    kind: PluginLoadKind,
) -> Result<()> {
    let prepared = prepare_plugin(path, &kind)?;
    execute_prepared_plugin(&runtime, plugins, &prepared, kind)
}

/// Load all plugins from a directory
async fn load_plugins_from_dir_internal(
    runtime: Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
    dir: &Path,
    kind: PluginLoadKind,
) -> Vec<String> {
    tracing::debug!(
        "load_plugins_from_dir_internal: scanning directory {:?}",
        dir
    );
    let mut errors = Vec::new();

    if !dir.exists() {
        tracing::warn!("Plugin directory does not exist: {:?}", dir);
        return errors;
    }

    // Scan directory for .ts and .js files
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|s| s.to_str());
                if ext == Some("ts") || ext == Some("js") {
                    tracing::debug!(
                        "load_plugins_from_dir_internal: attempting to load {:?}",
                        path
                    );
                    if let Err(e) =
                        load_plugin_internal(Rc::clone(&runtime), plugins, &path, kind.clone())
                            .await
                    {
                        let err = format!("Failed to load {:?}: {}", path, e);
                        tracing::error!("{}", err);
                        errors.push(err);
                    }
                }
            }

            tracing::debug!(
                "load_plugins_from_dir_internal: finished loading from {:?}, {} errors",
                dir,
                errors.len()
            );
        }
        Err(e) => {
            let err = format!("Failed to read plugin directory: {}", e);
            tracing::error!("{}", err);
            errors.push(err);
        }
    }

    errors
}

/// Load all plugins from a directory with config support
/// Returns (errors, discovered_plugins) where discovered_plugins contains all
/// found plugin files with their configs (respecting enabled state from provided configs)
async fn load_plugins_from_dir_with_config_internal(
    runtime: Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
    dir: &Path,
    plugin_configs: &HashMap<String, PluginConfig>,
    kind: PluginLoadKind,
) -> (Vec<String>, HashMap<String, PluginConfig>) {
    tracing::debug!(
        "load_plugins_from_dir_with_config_internal: scanning directory {:?}",
        dir
    );
    let mut errors = Vec::new();
    let mut discovered_plugins: HashMap<String, PluginConfig> = HashMap::new();

    if !dir.exists() {
        tracing::warn!("Plugin directory does not exist: {:?}", dir);
        return (errors, discovered_plugins);
    }

    // First pass: scan directory and collect all plugin files
    let mut plugin_files: Vec<(String, std::path::PathBuf)> = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|s| s.to_str());
                if ext == Some("ts") || ext == Some("js") {
                    // Skip .i18n.json files (they're not plugins)
                    if path.to_string_lossy().contains(".i18n.") {
                        continue;
                    }
                    // Get plugin name from filename (without extension)
                    let plugin_name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    plugin_files.push((plugin_name, path));
                }
            }
        }
        Err(e) => {
            let err = format!("Failed to read plugin directory: {}", e);
            tracing::error!("{}", err);
            errors.push(err);
            return (errors, discovered_plugins);
        }
    }

    // Second pass: build discovered_plugins map, collect enabled plugins with paths
    let mut enabled_plugins: Vec<(String, std::path::PathBuf)> = Vec::new();
    for (plugin_name, path) in plugin_files {
        // Check if we have an existing config for this plugin
        let config = if let Some(existing_config) = plugin_configs.get(&plugin_name) {
            // Use existing config but ensure path is set
            PluginConfig {
                enabled: existing_config.enabled,
                path: Some(path.clone()),
                settings: existing_config.settings.clone(),
            }
        } else {
            // Create new config with default enabled = true
            PluginConfig::new_with_path(path.clone())
        };

        // Add to discovered plugins
        discovered_plugins.insert(plugin_name.clone(), config.clone());

        if config.enabled {
            enabled_plugins.push((plugin_name, path));
        } else {
            tracing::info!(
                "load_plugins_from_dir_with_config_internal: skipping disabled plugin '{}'",
                plugin_name
            );
        }
    }

    // Phase 1: Parallel preparation — read files, transpile TS→JS, extract deps
    // All I/O and CPU-bound work happens here, concurrently across threads.
    let prep_start = std::time::Instant::now();
    let paths: Vec<std::path::PathBuf> = enabled_plugins.iter().map(|(_, p)| p.clone()).collect();
    let prepared_results: Vec<(String, Result<PreparedPlugin>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .iter()
            .map(|path| {
                let path = path.clone();
                let kind = kind.clone();
                scope.spawn(move || {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let result = prepare_plugin(&path, &kind);
                    (name, result)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let prep_elapsed = prep_start.elapsed();

    // Collect successful preparations and errors
    let mut prepared_map: std::collections::HashMap<String, PreparedPlugin> =
        std::collections::HashMap::new();
    for (name, result) in prepared_results {
        match result {
            Ok(prepared) => {
                prepared_map.insert(name, prepared);
            }
            Err(e) => {
                let err = format!("Failed to prepare plugin '{}': {}", name, e);
                tracing::error!("{}", err);
                errors.push(err);
            }
        }
    }

    tracing::info!(
        "Parallel plugin preparation completed in {:?} ({} plugins)",
        prep_elapsed,
        prepared_map.len()
    );

    // Build dependency map from prepared plugins
    let mut dependency_map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (name, prepared) in &prepared_map {
        if !prepared.dependencies.is_empty() {
            tracing::debug!(
                "Plugin '{}' declares dependencies: {:?}",
                name,
                prepared.dependencies
            );
            dependency_map.insert(name.clone(), prepared.dependencies.clone());
        }
    }

    // Topologically sort by dependencies
    let plugin_names: Vec<String> = prepared_map.keys().cloned().collect();
    let load_order = match fresh_parser_js::topological_sort_plugins(&plugin_names, &dependency_map)
    {
        Ok(order) => order,
        Err(e) => {
            let err = format!("Plugin dependency resolution failed: {}", e);
            tracing::error!("{}", err);
            errors.push(err);
            // Fall back to alphabetical order
            let mut names = plugin_names;
            names.sort();
            names
        }
    };

    // Phase 2: Serial execution — run prepared JS in QuickJS (must be single-threaded)
    let exec_start = std::time::Instant::now();
    for plugin_name in load_order {
        if let Some(prepared) = prepared_map.get(&plugin_name) {
            tracing::debug!(
                "load_plugins_from_dir_with_config_internal: executing plugin '{}'",
                plugin_name
            );
            if let Err(e) = execute_prepared_plugin(&runtime, plugins, prepared, kind.clone()) {
                let err = format!("Failed to execute plugin '{}': {}", plugin_name, e);
                tracing::error!("{}", err);
                errors.push(err);
            }
        }
    }
    let exec_elapsed = exec_start.elapsed();

    tracing::info!(
        "Serial plugin execution completed in {:?} ({} plugins)",
        exec_elapsed,
        plugins.len()
    );

    tracing::debug!(
        "load_plugins_from_dir_with_config_internal: finished. Discovered {} plugins, {} errors (prep: {:?}, exec: {:?})",
        discovered_plugins.len(),
        errors.len(),
        prep_elapsed,
        exec_elapsed
    );

    (errors, discovered_plugins)
}

/// Load a plugin from source code directly (no file I/O).
///
/// If a plugin with the same name is already loaded, it will be unloaded first
/// (hot-reload semantics).
fn load_plugin_from_source_internal(
    runtime: Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
    source: &str,
    name: &str,
    is_typescript: bool,
    kind: PluginLoadKind,
) -> Result<()> {
    if matches!(&kind, PluginLoadKind::Bundled { .. }) {
        return Err(anyhow!(
            "bundled plugin provenance requires a verified filesystem entrypoint"
        ));
    }
    runtime.borrow().validate_plugin_load(name, &kind)?;
    if plugins.contains_key(name) {
        unload_plugin_internal(Rc::clone(&runtime), plugins, name)?;
    }
    runtime.borrow().prepare_plugin_load(name, kind)?;

    let result = runtime
        .borrow_mut()
        .execute_source(source, name, is_typescript);
    if let Err(error) = result {
        runtime.borrow().abandon_plugin_load(name);
        return Err(error);
    }

    plugins.insert(
        name.to_string(),
        TsPluginInfo {
            name: name.to_string(),
            path: PathBuf::from(format!("<buffer:{}>", name)),
            enabled: true,
            declarations: None,
        },
    );
    Ok(())
}

/// Unload a plugin
fn unload_plugin_internal(
    runtime: Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
    name: &str,
) -> Result<()> {
    if plugins.remove(name).is_some() {
        tracing::info!("Unloading TypeScript plugin: {}", name);

        // Unregister i18n strings
        runtime
            .borrow_mut()
            .services
            .unregister_plugin_strings(name);

        // Remove all commands registered by this plugin
        runtime
            .borrow()
            .services
            .unregister_commands_by_plugin(name);

        // Clean up plugin runtime state (context, event handlers, actions, callbacks)
        runtime.borrow().cleanup_plugin(name);

        Ok(())
    } else {
        Err(anyhow!("Plugin '{}' not found", name))
    }
}

/// Reload a plugin
async fn reload_plugin_internal(
    runtime: Rc<RefCell<QuickJsBackend>>,
    plugins: &mut HashMap<String, TsPluginInfo>,
    name: &str,
) -> Result<()> {
    let path = plugins
        .get(name)
        .ok_or_else(|| anyhow!("Plugin '{}' not found", name))?
        .path
        .clone();
    let kind = runtime
        .borrow()
        .plugin_load_kind(name)
        .unwrap_or(PluginLoadKind::External);

    unload_plugin_internal(Rc::clone(&runtime), plugins, name)?;
    load_plugin_internal(runtime, plugins, &path, kind).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresh_core::api::{PluginCommand, TrustedBuiltinPlugin};
    use fresh_core::hooks::hook_args_to_json;

    #[test]
    fn test_oneshot_channel() {
        let (tx, rx) = oneshot::channel::<i32>();
        assert!(tx.send(42).is_ok());
        assert_eq!(rx.recv().unwrap(), 42);
    }

    #[test]
    fn test_hook_args_to_json_editor_initialized() {
        let args = HookArgs::EditorInitialized {};
        let json = hook_args_to_json(&args).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn test_hook_args_to_json_prompt_changed() {
        let args = HookArgs::PromptChanged {
            prompt_type: "search".to_string(),
            input: "test".to_string(),
        };
        let json = hook_args_to_json(&args).unwrap();
        assert_eq!(json["prompt_type"], "search");
        assert_eq!(json["input"], "test");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ready_requests_cannot_starve_cross_plugin_promise_settlement() {
        let (command_sender, command_receiver) = std::sync::mpsc::channel();
        let state_snapshot = Arc::new(RwLock::new(EditorStateSnapshot::new()));
        let mut backend = QuickJsBackend::with_state(
            state_snapshot,
            command_sender,
            Arc::new(fresh_core::services::NoopServiceBridge),
        )
        .unwrap();

        let spec = crate::runtime::TrustedBuiltinSpec::from_embedded_files(
            TrustedBuiltinPlugin::Orchestrator,
            PathBuf::from("/unused-trusted-plugin-root"),
            PathBuf::from("orchestrator.ts"),
            [(PathBuf::from("orchestrator.ts"), b"".as_slice())],
        )
        .unwrap();
        backend
            .prepare_plugin_load(
                "orchestrator",
                PluginLoadKind::bundled(crate::runtime::TrustedBuiltinManifest::new([(
                    "orchestrator".to_string(),
                    spec,
                )])),
            )
            .unwrap();
        backend
            .execute_source(
                r#"
                editor.exportPluginApi("orchestrator", {
                    runAgent: async () => "settled",
                });
                "#,
                "orchestrator",
                false,
            )
            .unwrap();
        backend
            .prepare_plugin_load("api-caller", PluginLoadKind::External)
            .unwrap();
        backend
            .execute_source(
                r#"
                editor.getPluginApi("orchestrator").runAgent({}).then(() => {
                    editor.setStatus("bridge promise settled");
                });
                "#,
                "api-caller",
                false,
            )
            .unwrap();
        assert!(command_receiver.try_recv().is_err());

        let runtime = Rc::new(RefCell::new(backend));
        let (request_sender, request_receiver) = tokio::sync::mpsc::unbounded_channel();
        for _ in 0..MAX_READY_REQUESTS_BEFORE_EVENT_LOOP_POLL {
            let (response, _response_receiver) = oneshot::channel();
            request_sender
                .send(PluginRequest::HasHookHandlers {
                    hook_name: "never-registered".to_string(),
                    response,
                })
                .unwrap();
        }
        request_sender.send(PluginRequest::Shutdown).unwrap();

        let mut plugins = HashMap::new();
        plugin_thread_loop(runtime, &mut plugins, request_receiver).await;

        assert!(command_receiver.try_iter().any(|envelope| {
            matches!(
                envelope.command,
                PluginCommand::SetStatus { ref message }
                    if message == "bridge promise settled"
            )
        }));
    }
}
