//! Unified Plugin Manager
//!
//! This module provides a unified interface for the plugin system that works
//! regardless of whether the `plugins` feature is enabled. When plugins are
//! disabled, all methods are no-ops, avoiding the need for cfg attributes
//! scattered throughout the codebase.

use crate::config_io::DirectoryContext;
use crate::input::command_registry::CommandRegistry;
use fresh_core::config::PluginConfig;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

#[cfg(feature = "plugins")]
use super::bridge::EditorServiceBridge;
#[cfg(feature = "plugins")]
use fresh_plugin_runtime::PluginThreadHandle;

/// Unified plugin manager that abstracts over the plugin system.
///
/// When the `plugins` feature is enabled, this wraps `PluginThreadHandle`.
/// When disabled, all methods are no-ops.
pub struct PluginManager {
    #[cfg(feature = "plugins")]
    inner: Option<PluginThreadHandle>,
    /// Per-window filesystem registry, kept so the editor can rebuild it from
    /// its windows whenever it refreshes the plugin state snapshot.
    #[cfg(feature = "plugins")]
    window_registry: Option<Arc<super::bridge::WindowFsRegistry>>,
    #[cfg(not(feature = "plugins"))]
    _phantom: std::marker::PhantomData<()>,
    /// Test-only side channel: commands pushed via
    /// [`Self::test_inject_command`] are returned by the next
    /// `process_command_envelopes()` call as if they had come from the plugin
    /// thread. Always present (zero overhead — empty `Vec`) so
    /// integration tests in `tests/` can use it without an extra
    /// feature flag.
    pending_injected_commands: Vec<super::api::PluginCommandEnvelope>,
}

impl PluginManager {
    /// Create a new plugin manager.
    ///
    /// When `plugins` feature is enabled and `enable` is true, spawns the plugin thread.
    /// Otherwise, creates a no-op manager.
    pub fn new(
        enable: bool,
        command_registry: Arc<RwLock<CommandRegistry>>,
        dir_context: DirectoryContext,
        theme_cache: Arc<RwLock<HashMap<String, serde_json::Value>>>,
        authority_filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync>,
        local_filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync>,
    ) -> Self {
        #[cfg(feature = "plugins")]
        {
            if enable {
                // Per-window authority registry: seeded with the boot backend
                // and rebuilt from the editor's windows on each snapshot
                // refresh. Backs bare-string paths (active window) and
                // `WindowPath` values (a specific window).
                let window_registry =
                    Arc::new(super::bridge::WindowFsRegistry::new(authority_filesystem));
                // Local-host filesystem: fixed, never retargeted, so `LocalPath`
                // values always resolve on the editor host.
                let local_plugin_fs: Arc<dyn fresh_core::services::PluginFilesystem> =
                    Arc::new(super::bridge::RoutedFilesystem::fixed(local_filesystem));
                let services = Arc::new(EditorServiceBridge {
                    command_registry: command_registry.clone(),
                    dir_context,
                    theme_cache,
                    local_plugin_fs,
                    window_registry: Arc::clone(&window_registry),
                });
                match PluginThreadHandle::spawn(services) {
                    Ok(handle) => {
                        return Self {
                            inner: Some(handle),
                            window_registry: Some(window_registry),
                            pending_injected_commands: Vec::new(),
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to spawn TypeScript plugin thread: {}", e);
                        #[cfg(debug_assertions)]
                        panic!("TypeScript plugin thread creation failed: {}", e);
                    }
                }
            } else {
                tracing::info!("Plugins disabled via --no-plugins flag");
            }
            Self {
                inner: None,
                window_registry: None,
                pending_injected_commands: Vec::new(),
            }
        }

        #[cfg(not(feature = "plugins"))]
        {
            let _ = command_registry; // Suppress unused warning
            let _ = dir_context; // Suppress unused warning
            let _ = theme_cache; // Suppress unused warning
            let _ = authority_filesystem; // Suppress unused warning
            let _ = local_filesystem; // Suppress unused warning
            if enable {
                tracing::warn!("Plugins requested but compiled without plugin support");
            }
            Self {
                _phantom: std::marker::PhantomData,
                pending_injected_commands: Vec::new(),
            }
        }
    }

    /// The per-window filesystem registry, if plugins are active.
    ///
    /// The editor calls [`WindowFsRegistry::rebuild`](super::bridge::WindowFsRegistry::rebuild)
    /// on it whenever it refreshes the plugin state snapshot, so plugin file I/O
    /// resolves against the correct window's authority (or the active one).
    #[cfg(feature = "plugins")]
    pub fn window_fs_registry(&self) -> Option<Arc<super::bridge::WindowFsRegistry>> {
        self.window_registry.clone()
    }

    /// Inject a [`PluginCommandEnvelope`](super::api::PluginCommandEnvelope)
    /// into the manager's pending queue as if it had arrived from the plugin
    /// thread. Returned by the next `process_command_envelopes()` call.
    ///
    /// Intended for tests that need to deterministically reproduce
    /// renderer/plugin races (e.g. the mid-render plugin-command drain
    /// in `Editor::render`) without spinning up the real plugin
    /// runtime. Production code should not call this.
    pub fn test_inject_command(&mut self, envelope: super::api::PluginCommandEnvelope) {
        self.pending_injected_commands.push(envelope);
    }

    /// Check if the plugin system is active (has a running plugin thread,
    /// or — in tests — has commands queued via [`Self::test_inject_command`]).
    pub fn is_active(&self) -> bool {
        if !self.pending_injected_commands.is_empty() {
            return true;
        }
        #[cfg(feature = "plugins")]
        {
            self.inner.is_some()
        }
        #[cfg(not(feature = "plugins"))]
        {
            false
        }
    }
    /// Whether this exact loader-minted plugin instance still owns a live context.
    pub fn is_plugin_instance_active(
        &self,
        plugin_instance_id: fresh_core::api::PluginInstanceId,
    ) -> bool {
        #[cfg(feature = "plugins")]
        {
            return self
                .inner
                .as_ref()
                .is_some_and(|inner| inner.is_plugin_instance_active(plugin_instance_id));
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = plugin_instance_id;
            false
        }
    }

    /// Check if the plugin thread is still alive
    pub fn is_alive(&self) -> bool {
        #[cfg(feature = "plugins")]
        {
            self.inner.as_ref().map(|h| h.is_alive()).unwrap_or(false)
        }
        #[cfg(not(feature = "plugins"))]
        {
            false
        }
    }

    /// Check thread health and panic if the plugin thread died due to a panic.
    /// This propagates plugin thread panics to the calling thread.
    /// Call this periodically (e.g., in wait loops) to fail fast on plugin errors.
    pub fn check_thread_health(&mut self) {
        #[cfg(feature = "plugins")]
        {
            if let Some(ref mut handle) = self.inner {
                handle.check_thread_health();
            }
        }
    }

    /// Load plugins from a directory.
    pub fn load_plugins_from_dir(&self, dir: &Path) -> Vec<String> {
        #[cfg(feature = "plugins")]
        {
            if let Some(ref manager) = self.inner {
                return manager.load_plugins_from_dir(dir);
            }
            Vec::new()
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = dir;
            Vec::new()
        }
    }

    /// Load plugins from a directory with config support.
    /// Returns (errors, discovered_plugins) where discovered_plugins is a map of
    /// plugin name -> PluginConfig with paths populated.
    #[cfg(feature = "plugins")]
    pub fn load_plugins_from_dir_with_config(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
    ) -> (Vec<String>, HashMap<String, PluginConfig>) {
        if let Some(ref manager) = self.inner {
            return manager.load_plugins_from_dir_with_config(dir, plugin_configs);
        }
        (Vec::new(), HashMap::new())
    }

    #[cfg(feature = "plugins")]
    pub fn load_plugins_from_dir_with_config_and_kind(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
        kind: fresh_plugin_runtime::runtime::PluginLoadKind,
    ) -> (Vec<String>, HashMap<String, PluginConfig>) {
        if let Some(manager) = &self.inner {
            return manager.load_plugins_from_dir_with_config_and_kind(dir, plugin_configs, kind);
        }
        (Vec::new(), HashMap::new())
    }

    /// Load plugins from a directory with config support (no-op when plugins disabled).
    #[cfg(not(feature = "plugins"))]
    pub fn load_plugins_from_dir_with_config(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
    ) -> (Vec<String>, HashMap<String, PluginConfig>) {
        let _ = (dir, plugin_configs);
        (Vec::new(), HashMap::new())
    }

    /// Unload a plugin by name.
    pub fn unload_plugin(&self, name: &str) -> anyhow::Result<()> {
        #[cfg(feature = "plugins")]
        {
            self.inner
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Plugin system not active"))?
                .unload_plugin(name)
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = name;
            Ok(())
        }
    }

    /// Queue an unload without blocking the editor thread.
    pub fn unload_plugin_request(&self, name: &str) {
        #[cfg(feature = "plugins")]
        if let Some(manager) = &self.inner {
            let _ = manager.unload_plugin_request(name);
        }
        #[cfg(not(feature = "plugins"))]
        let _ = name;
    }

    /// Load a single plugin by path.
    pub fn load_plugin(&self, path: &Path) -> anyhow::Result<()> {
        #[cfg(feature = "plugins")]
        {
            self.inner
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Plugin system not active"))?
                .load_plugin(path)
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = path;
            Ok(())
        }
    }

    /// Load a plugin from source code directly (no file I/O).
    ///
    /// If a plugin with the same name is already loaded, it will be unloaded first
    /// (hot-reload semantics). This is used for "Load Plugin from Buffer".
    pub fn load_plugin_from_source(
        &self,
        source: &str,
        name: &str,
        is_typescript: bool,
    ) -> anyhow::Result<()> {
        #[cfg(feature = "plugins")]
        {
            self.inner
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Plugin system not active"))?
                .load_plugin_from_source(source, name, is_typescript)
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = (source, name, is_typescript);
            Ok(())
        }
    }

    #[cfg(feature = "plugins")]
    pub fn load_plugin_from_source_with_kind(
        &self,
        source: &str,
        name: &str,
        is_typescript: bool,
        kind: fresh_plugin_runtime::runtime::PluginLoadKind,
    ) -> anyhow::Result<()> {
        self.inner
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Plugin system not active"))?
            .load_plugin_from_source_with_kind(source, name, is_typescript, kind)
    }

    /// Run a hook (fire-and-forget). Returns whether a subscribed runtime
    /// consumer accepted the request.
    pub fn run_hook(&self, hook_name: &str, args: super::hooks::HookArgs) -> bool {
        #[cfg(feature = "plugins")]
        {
            self.inner
                .as_ref()
                .is_some_and(|manager| manager.run_hook(hook_name, args))
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = (hook_name, args);
            false
        }
    }
    pub fn run_hook_with_invocation(
        &self,
        hook_name: &str,
        args: super::hooks::HookArgs,
        invocation: Option<fresh_core::api::PluginInvocation>,
    ) -> bool {
        #[cfg(feature = "plugins")]
        {
            self.inner.as_ref().is_some_and(|manager| {
                manager.run_hook_with_invocation(hook_name, args, invocation)
            })
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = (hook_name, args, invocation);
            false
        }
    }

    /// Run a hook in one plugin's context only (fire-and-forget).
    /// Handlers registered by other plugins are skipped.
    pub fn run_hook_for_plugin(
        &self,
        plugin: &str,
        hook_name: &str,
        args: super::hooks::HookArgs,
    ) -> bool {
        #[cfg(feature = "plugins")]
        {
            self.inner
                .as_ref()
                .is_some_and(|manager| manager.run_hook_for_plugin(plugin, hook_name, args))
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = (plugin, hook_name, args);
            false
        }
    }
    pub fn run_hook_for_plugin_with_invocation(
        &self,
        plugin: &str,
        hook_name: &str,
        args: super::hooks::HookArgs,
        invocation: Option<fresh_core::api::PluginInvocation>,
    ) -> bool {
        #[cfg(feature = "plugins")]
        {
            self.inner.as_ref().is_some_and(|manager| {
                manager.run_hook_for_plugin_with_invocation(plugin, hook_name, args, invocation)
            })
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = (plugin, hook_name, args, invocation);
            false
        }
    }

    /// Deliver a response to a pending async plugin operation.
    pub fn deliver_response(&self, response: super::api::PluginResponse) {
        #[cfg(feature = "plugins")]
        {
            if let Some(ref manager) = self.inner {
                manager.deliver_response(response);
            }
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = response;
        }
    }

    /// Process pending plugin commands with their loader-owned context.
    pub fn process_command_envelopes(&mut self) -> Vec<super::api::PluginCommandEnvelope> {
        let mut commands = std::mem::take(&mut self.pending_injected_commands);
        #[cfg(feature = "plugins")]
        {
            if let Some(manager) = &mut self.inner {
                commands.extend(manager.process_command_envelopes());
            }
        }
        commands
    }

    /// Get the state snapshot handle for updating editor state.
    #[cfg(feature = "plugins")]
    pub fn state_snapshot_handle(&self) -> Option<Arc<RwLock<super::api::EditorStateSnapshot>>> {
        self.inner.as_ref().map(|m| m.state_snapshot_handle())
    }

    /// Streaming-search handle registry shared with the plugin runtime.
    /// Producers spawned by `BeginSearch` look up the handle here and write
    /// directly into its `SearchHandleState`; consumers (the plugin) drain
    /// the same state via `_searchHandleTake`.
    #[cfg(feature = "plugins")]
    pub fn search_handles_handle(&self) -> Option<fresh_core::api::SearchHandleRegistry> {
        self.inner.as_ref().map(|m| m.search_handles_handle())
    }

    /// Streaming-search registry accessor (no-op build).
    #[cfg(not(feature = "plugins"))]
    pub fn search_handles_handle(&self) -> Option<fresh_core::api::SearchHandleRegistry> {
        None
    }

    /// Execute a plugin action asynchronously. `args_json`, when set, is a JSON
    /// object handed to the handler as its single argument (the agent command
    /// channel's `RunCommand.args`); `None` calls it with no arguments, which
    /// is what a keybinding or palette invocation does. `request_id`, when set,
    /// asks the runtime to report the handler's return value back under that id
    /// once it settles — how a `cmd run` gets an answer to print.
    #[cfg(feature = "plugins")]
    pub fn execute_action_async(
        &self,
        action_name: &str,
        args_json: Option<String>,
        request_id: Option<u64>,
    ) -> Option<anyhow::Result<fresh_plugin_runtime::thread::oneshot::Receiver<anyhow::Result<()>>>>
    {
        self.inner
            .as_ref()
            .map(|m| m.execute_action_async(action_name, args_json, request_id))
    }
    #[cfg(feature = "plugins")]
    pub fn execute_action_async_with_invocation(
        &self,
        action_name: &str,
        args_json: Option<String>,
        request_id: Option<u64>,
        invocation: Option<fresh_core::api::PluginInvocation>,
    ) -> Option<anyhow::Result<fresh_plugin_runtime::thread::oneshot::Receiver<anyhow::Result<()>>>>
    {
        self.inner.as_ref().map(|manager| {
            manager.execute_action_async_with_invocation(
                action_name,
                args_json,
                request_id,
                invocation,
            )
        })
    }

    /// List all loaded plugins.
    #[cfg(feature = "plugins")]
    pub fn list_plugins(
        &self,
    ) -> Vec<fresh_plugin_runtime::backend::quickjs_backend::TsPluginInfo> {
        self.inner
            .as_ref()
            .map(|m| m.list_plugins())
            .unwrap_or_default()
    }

    /// Collect the isolated-declarations `.d.ts` emit of every loaded
    /// plugin that produced one. Returns `(plugin_name, d_ts_source)`
    /// pairs — callers use this to assemble `plugins.d.ts`.
    ///
    /// Available in all builds: without the `plugins` feature it
    /// returns an empty vec, letting `editor_init` call this
    /// unconditionally.
    pub fn plugin_declarations(&self) -> Vec<(String, String)> {
        #[cfg(feature = "plugins")]
        {
            self.list_plugins()
                .into_iter()
                .filter_map(|info| info.declarations.map(|d| (info.name, d)))
                .collect()
        }
        #[cfg(not(feature = "plugins"))]
        {
            Vec::new()
        }
    }

    /// Reload a plugin by name.
    #[cfg(feature = "plugins")]
    pub fn reload_plugin(&self, name: &str) -> anyhow::Result<()> {
        self.inner
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Plugin system not active"))?
            .reload_plugin(name)
    }

    /// Submit a "load plugins from dir with config" request without
    /// blocking. Returns `None` when the plugin runtime is inactive (no
    /// thread), or when the request couldn't be submitted. Used by the
    /// startup async-load path.
    #[cfg(feature = "plugins")]
    pub fn load_plugins_from_dir_with_config_request(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
    ) -> Option<
        fresh_plugin_runtime::thread::oneshot::Receiver<
            fresh_plugin_runtime::thread::PluginsDirLoadResult,
        >,
    > {
        self.inner.as_ref().and_then(|m| {
            m.load_plugins_from_dir_with_config_request(dir, plugin_configs)
                .ok()
        })
    }

    #[cfg(feature = "plugins")]
    pub fn load_plugins_from_dir_with_config_request_and_kind(
        &self,
        dir: &Path,
        plugin_configs: &HashMap<String, PluginConfig>,
        kind: fresh_plugin_runtime::runtime::PluginLoadKind,
    ) -> Option<
        fresh_plugin_runtime::thread::oneshot::Receiver<
            fresh_plugin_runtime::thread::PluginsDirLoadResult,
        >,
    > {
        self.inner.as_ref().and_then(|manager| {
            manager
                .load_plugins_from_dir_with_config_request_and_kind(dir, plugin_configs, kind)
                .ok()
        })
    }

    /// Submit a "load plugin from source" request without blocking.
    /// Returns `None` when the plugin runtime is inactive.
    #[cfg(feature = "plugins")]
    pub fn load_plugin_from_source_request(
        &self,
        source: &str,
        name: &str,
        is_typescript: bool,
    ) -> Option<fresh_plugin_runtime::thread::oneshot::Receiver<anyhow::Result<()>>> {
        self.inner.as_ref().and_then(|m| {
            m.load_plugin_from_source_request(source, name, is_typescript)
                .ok()
        })
    }

    #[cfg(feature = "plugins")]
    pub fn load_plugin_from_source_request_with_kind(
        &self,
        source: &str,
        name: &str,
        is_typescript: bool,
        kind: fresh_plugin_runtime::runtime::PluginLoadKind,
    ) -> Option<fresh_plugin_runtime::thread::oneshot::Receiver<anyhow::Result<()>>> {
        self.inner.as_ref().and_then(|manager| {
            manager
                .load_plugin_from_source_request_with_kind(source, name, is_typescript, kind)
                .ok()
        })
    }

    /// Submit a "list plugins" request without blocking. Submitted after
    /// a batch of dir-load requests, this guarantees the response covers
    /// every plugin loaded by that batch (FIFO request channel).
    #[cfg(feature = "plugins")]
    pub fn list_plugins_request(
        &self,
    ) -> Option<
        fresh_plugin_runtime::thread::oneshot::Receiver<
            Vec<fresh_plugin_runtime::backend::quickjs_backend::TsPluginInfo>,
        >,
    > {
        self.inner
            .as_ref()
            .and_then(|m| m.list_plugins_request().ok())
    }

    /// Check if any handlers are registered for a hook.
    ///
    /// Blocking call (round-trips through the plugin thread). Suitable for
    /// rare events (mouse clicks, command dispatch). For per-render gating
    /// use `has_subscribers` instead — it reads a shared registry directly.
    pub fn has_hook_handlers(&self, hook_name: &str) -> bool {
        #[cfg(feature = "plugins")]
        {
            self.inner
                .as_ref()
                .map(|m| m.has_hook_handlers(hook_name))
                .unwrap_or(false)
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = hook_name;
            false
        }
    }

    /// Non-blocking variant of `has_hook_handlers`. Reads the shared
    /// `event_handlers` registry directly — safe to call on the hot
    /// render path. Returns `false` when plugins are disabled.
    pub fn has_subscribers(&self, hook_name: &str) -> bool {
        #[cfg(feature = "plugins")]
        {
            self.inner
                .as_ref()
                .map(|m| m.has_subscribers(hook_name))
                .unwrap_or(false)
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = hook_name;
            false
        }
    }
    /// Targeted non-blocking subscriber check.
    pub fn has_subscriber(&self, plugin: &str, hook_name: &str) -> bool {
        #[cfg(feature = "plugins")]
        {
            self.inner
                .as_ref()
                .is_some_and(|manager| manager.has_subscriber(plugin, hook_name))
        }
        #[cfg(not(feature = "plugins"))]
        {
            let _ = (plugin, hook_name);
            false
        }
    }

    /// Resolve an async callback in the plugin runtime
    #[cfg(feature = "plugins")]
    pub fn resolve_callback(&self, callback_id: super::api::JsCallbackId, result_json: String) {
        if let Some(inner) = &self.inner {
            inner.resolve_callback(callback_id, result_json);
        }
    }

    /// Resolve an async callback in the plugin runtime (no-op when plugins disabled)
    #[cfg(not(feature = "plugins"))]
    pub fn resolve_callback(
        &self,
        callback_id: fresh_core::api::JsCallbackId,
        result_json: String,
    ) {
        let _ = (callback_id, result_json);
    }
    /// Resolve only when `callback_id` belongs to this exact loaded instance.
    pub fn resolve_callback_for(
        &self,
        plugin_instance_id: fresh_core::api::PluginInstanceId,
        callback_id: fresh_core::api::JsCallbackId,
        result_json: String,
    ) {
        #[cfg(feature = "plugins")]
        if let Some(inner) = &self.inner {
            inner.resolve_callback_for(plugin_instance_id, callback_id, result_json);
        }
        #[cfg(not(feature = "plugins"))]
        let _ = (plugin_instance_id, callback_id, result_json);
    }

    /// Reject an async callback in the plugin runtime
    #[cfg(feature = "plugins")]
    pub fn reject_callback(&self, callback_id: super::api::JsCallbackId, error: String) {
        if let Some(inner) = &self.inner {
            inner.reject_callback(callback_id, error);
        }
    }

    /// Reject an async callback in the plugin runtime (no-op when plugins disabled)
    #[cfg(not(feature = "plugins"))]
    pub fn reject_callback(&self, callback_id: fresh_core::api::JsCallbackId, error: String) {
        let _ = (callback_id, error);
    }
    /// Reject only when `callback_id` belongs to this exact loaded instance.
    pub fn reject_callback_for(
        &self,
        plugin_instance_id: fresh_core::api::PluginInstanceId,
        callback_id: fresh_core::api::JsCallbackId,
        error: String,
    ) {
        #[cfg(feature = "plugins")]
        if let Some(inner) = &self.inner {
            inner.reject_callback_for(plugin_instance_id, callback_id, error);
        }
        #[cfg(not(feature = "plugins"))]
        let _ = (plugin_instance_id, callback_id, error);
    }
}
