//! Plugin command dispatch and plugin-specific handlers on `Editor`.
//!
//! Three clusters previously inline in mod.rs:
//!
//! - `update_plugin_state_snapshot` — synchronizes the immutable view of
//!   editor state plugins observe between commands.
//! - `handle_plugin_command` — the giant match dispatching every
//!   PluginCommand variant to a specialized handler. Most arms call
//!   methods in app/plugin_commands.rs; the rest live below.
//! - The handle_* family — buffer/path lookups, action execution, plugin
//!   lifecycle management, and view-control commands callable from
//!   plugin code.

use std::sync::Arc;

use anyhow::Result as AnyhowResult;

use fresh_core::api::{
    BufferSavedDiff, EditorStateSnapshot, JsCallbackId, PluginCommand, PluginCommandContext,
    PluginCommandEnvelope,
};

use crate::model::event::{BufferId, LeafId, SplitId};
use crate::services::async_bridge::AsyncMessage;
use crate::services::plugins::hooks::HookArgs;
use crate::view::split::SplitViewState;

use super::window::Window;
use super::{Editor, FloatingWidgetState};

/// Normalize a session path for the plugin API. Sessions reach `WindowInfo`
/// from two sources — the canonicalized launch session and `create_window_at`'s
/// raw `PathBuf` — so any byte-level path field (lex sort, equality, …) in a
/// plugin needs them encoded the same way. On Windows that means resolving
/// 8.3 short names (`RUNNER~1` → `runneradmin`) and stripping the `\\?\`
/// verbatim prefix `canonicalize` adds. No-op on non-Windows.
///
/// `canonicalize` only works on paths that exist on disk. Worktree session
/// roots created by the orchestrator often point at directories that haven't
/// been materialized yet (`<repo>/wt-<name>`), so a naïve canonicalize would
/// leave them in their original 8.3 short-name form while the launch session
/// — whose root exists — gets the long-name form, and lex compare inverts
/// on case (`R` 0x52 < `r` 0x72). Walk up to the deepest existing ancestor,
/// canonicalize that, then re-attach the missing tail so siblings share the
/// same prefix encoding regardless of which exist.
fn normalize_plugin_path(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let canonical = canonicalize_deepest_existing(&path);
        let s = canonical.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(stripped);
        }
        return canonical;
    }
    #[cfg(not(windows))]
    path
}

#[cfg(windows)]
fn canonicalize_deepest_existing(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    // Walk up to the deepest ancestor that does canonicalize, then re-attach
    // the components we walked past. Falls back to the raw path if no
    // ancestor canonicalizes (drive root missing, etc).
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut ancestor = path;
    loop {
        let Some(parent) = ancestor.parent() else {
            return path.to_path_buf();
        };
        if let Some(name) = ancestor.file_name() {
            tail.push(name);
        }
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        ancestor = parent;
    }
}

/// Returns the byte offset of the start (want_end=false) or end (want_end=true)
/// of `line` (0-indexed) within `content`. Returns `None` when `line` is out of
/// range. The "end" position is the byte index of the terminating `\n`; for the
/// last line with no trailing newline it is `buffer_len`.
fn buffer_line_byte_offset(
    content: &str,
    buffer_len: usize,
    line: usize,
    want_end: bool,
) -> Option<usize> {
    if !want_end && line == 0 {
        return Some(0);
    }
    let mut current_line = 0usize;
    for (byte_idx, c) in content.char_indices() {
        if c == '\n' {
            if want_end && current_line == line {
                return Some(byte_idx);
            }
            current_line += 1;
            if !want_end && current_line == line {
                return Some(byte_idx + 1);
            }
        }
    }
    if want_end && current_line == line {
        Some(buffer_len)
    } else {
        None
    }
}
fn plugin_context_may_dispatch(context: &PluginCommandContext, instance_is_active: bool) -> bool {
    context.provenance == fresh_core::api::PluginLoadProvenance::Internal
        || context.is_compensating_cleanup()
        || instance_is_active
}

impl Editor {
    /// Update the plugin state snapshot with current editor state.
    ///
    /// Per-window snapshot population (active buffer, splits, view
    /// states, cursors, diagnostics, folding ranges, plugin view
    /// states) lives in [`Window::populate_plugin_state_snapshot`].
    /// This function adds the editor-wide fields that no single Window
    /// owns (clipboard, the full `windows` list, the memoized config
    /// JSON cache, `user_config_raw`, and `plugin_global_state`).
    #[cfg(feature = "plugins")]
    /// Shift plugin interval markers for one buffer to track an edit at `pos`
    /// that removed `removed` bytes and inserted `inserted` bytes. Called from
    /// the edit-apply path *before* the post-edit snapshot refresh + hook, so a
    /// plugin querying markers in after_insert/after_delete sees current
    /// coordinates. Insert gives `start` right-gravity / `end` left-gravity at
    /// the boundary; delete clamps into the deletion. Markers whose interior is
    /// touched keep shifting here (the plugin deletes + re-discovers them).
    ///
    /// Also a `#[doc(hidden)]` test hook: integration tests call this to
    /// deterministically reproduce the cross-thread marker/event desync (shift a
    /// plugin marker without editing the buffer) that the async `lines_changed`
    /// pipeline otherwise only produces as a timing race.
    #[doc(hidden)]
    pub fn shift_plugin_markers_for_edit(
        &self,
        buffer_id: BufferId,
        pos: usize,
        removed: usize,
        inserted: usize,
    ) {
        let Some(handle) = self.plugin_manager.read().unwrap().state_snapshot_handle() else {
            return;
        };
        let Ok(mut snapshot) = handle.write() else {
            return;
        };
        let Some(markers) = snapshot.plugin_markers.get_mut(&buffer_id) else {
            return;
        };
        // Reduce a same-position replacement (both lengths non-zero, as the bulk
        // path passes for type-over-selection / paste-over / query-replace / LSP
        // code actions) to its NET delta, mirroring `marker_list` and
        // `coord_map::record_replace`. The old code applied only the deletion
        // clamp and dropped `inserted`, so every plugin marker past such an edit
        // landed off by `inserted` bytes (the wrong direction when ins > del).
        let (eff_removed, eff_inserted) = if removed > 0 && inserted > 0 {
            if inserted >= removed {
                (0, inserted - removed)
            } else {
                (removed - inserted, 0)
            }
        } else {
            (removed, inserted)
        };
        for m in markers.values_mut() {
            if eff_removed == 0 {
                if m.start >= pos {
                    m.start += eff_inserted;
                }
                if m.end > pos {
                    m.end += eff_inserted;
                }
            } else {
                let d0 = pos;
                let d1 = pos + eff_removed;
                let clamp = |x: usize| {
                    if x <= d0 {
                        x
                    } else if x >= d1 {
                        x - eff_removed
                    } else {
                        d0
                    }
                };
                m.start = clamp(m.start);
                m.end = clamp(m.end);
            }
        }
    }

    pub fn update_plugin_state_snapshot(&mut self) {
        self.update_plugin_state_snapshot_to(None, None);
    }

    fn update_plugin_state_snapshot_to(
        &mut self,
        target: Option<Arc<std::sync::RwLock<EditorStateSnapshot>>>,
        scoped_window: Option<fresh_core::WindowId>,
    ) {
        let private_snapshot = target.is_some();
        let inherited_plugin_state = if private_snapshot {
            self.plugin_manager
                .read()
                .unwrap()
                .state_snapshot_handle()
                .and_then(|shared| {
                    shared.read().ok().map(|shared| {
                        (
                            shared.plugin_markers.clone(),
                            shared.keybinding_labels.clone(),
                        )
                    })
                })
        } else {
            None
        };

        // Only the shared snapshot retargets the global filesystem registry.
        // Agent-script snapshots are private views and must not affect other
        // plugins' authority routing.
        #[cfg(feature = "plugins")]
        if target.is_none() {
            if let Some(registry) = self.plugin_manager.read().unwrap().window_fs_registry() {
                let active = self.active_window;
                let entries: Vec<(
                    fresh_core::WindowId,
                    fresh_core::api::AuthorityStamp,
                    std::sync::Arc<dyn crate::model::filesystem::FileSystem + Send + Sync>,
                )> = self
                    .windows
                    .iter()
                    .map(|(id, window)| {
                        (
                            *id,
                            window.authority().stamp(),
                            Arc::clone(&window.authority.filesystem),
                        )
                    })
                    .collect();
                registry.rebuild(active, entries);
            }
        }

        let snapshot_handle =
            target.or_else(|| self.plugin_manager.read().unwrap().state_snapshot_handle());
        let Some(snapshot_handle) = snapshot_handle else {
            return;
        };
        let mut snapshot = snapshot_handle.write().unwrap();
        if let Some((plugin_markers, keybinding_labels)) = inherited_plugin_state {
            snapshot.plugin_markers = plugin_markers;
            snapshot.keybinding_labels = keybinding_labels;
        }

        self.active_window_mut()
            .populate_plugin_state_snapshot(&mut snapshot);
        // Private invocation snapshots expose one window; the shared runtime
        // snapshot retains markers for every still-open window. Buffer IDs are
        // editor-global, so preserving inactive-window markers is unambiguous.
        if private_snapshot {
            let mut plugin_markers = std::mem::take(&mut snapshot.plugin_markers);
            plugin_markers.retain(|buffer_id, _| snapshot.buffers.contains_key(buffer_id));
            snapshot.plugin_markers = plugin_markers;
        } else {
            snapshot.plugin_markers.retain(|buffer_id, _| {
                self.windows
                    .values()
                    .any(|window| window.buffers.contains_key(buffer_id))
            });
        }

        // Editor-wide fields below — these reach state outside any
        // single Window.

        snapshot.clipboard = self.clipboard.get_internal().to_string();
        snapshot.working_dir = self.working_dir().to_path_buf();

        // Total terminal dimensions (full screen, not the active
        // split's viewport). Plugins read this via `getScreenSize()`
        // to size floating overlays against the whole terminal.
        snapshot.terminal_width = self.terminal_width;
        snapshot.terminal_height = self.terminal_height;

        // Authority label tracks `Editor::authority` (the active
        // authority). It can't be sourced from `Window::resources.authority`
        // because `set_boot_authority` replaces `self.authority` by value
        // — the per-window resource clones still point at the previous
        // authority handle. Reading from `Editor` keeps the snapshot in
        // lockstep with the canonical seat.
        snapshot.authority_label = self.authority().display_label.clone();

        // Surface the active project's Workspace Trust level so plugins that
        // run repo-controlled work can gate on it.
        snapshot.workspace_trust_level = self
            .authority()
            .workspace_trust
            .level()
            .as_str()
            .to_string();
        snapshot.env_active = self.authority().env_provider.is_active();

        // Core is the *only* place that detects which environment a workspace
        // has. The env-manager plugin reads this resolved result via
        // `editor.detectedEnv()` rather than probing the filesystem itself.
        // Empty string ⇒ no env detected.
        snapshot.detected_env = crate::services::workspace_trust::detect_env(
            self.working_dir(),
            &self.config.env.detectors,
        )
        .and_then(|d| serde_json::to_string(&d).ok())
        .unwrap_or_default();

        // Publish the session list so plugins (Orchestrator, etc.)
        // see updates from createWindow/closeWindow without
        // a separate notification path. Sorted by id for
        // deterministic order — `next_window_id` is monotonic
        // so this is "creation order".
        // Dormant remote sessions (SSH / kube discovered at boot, not yet
        // connected) usually have no `Window` — they live in `dormant_remote`
        // as authority-less descriptors. They must still appear in the dock,
        // so fold them into the snapshot alongside the live windows. A dive
        // promotes one to a real window (removing it from `dormant_remote`);
        // a *failed* dive leaves the descriptor AND a disconnected shell
        // window at the same id, so descriptors shadowed by a window are
        // skipped to keep the list one-row-per-session.
        let dormant_infos = self
            .dormant_remote
            .iter()
            .filter(|(id, _)| !self.windows.contains_key(id))
            .map(|(_, d)| {
                let slot = d.plugin_state.get("orchestrator");
                let project_path = slot
                    .and_then(|m| m.get("project_path"))
                    .and_then(|v| v.as_str())
                    .filter(|p| !p.is_empty())
                    .map(std::path::PathBuf::from)
                    .or_else(|| d.project_path.clone())
                    .unwrap_or_else(|| d.root.clone());
                fresh_core::api::WindowInfo {
                    id: fresh_core::WindowId(d.id),
                    // A dormant shell carries the persisted id when its
                    // workspace file had one; legacy files leave it empty.
                    stable_id: d.stable_id.clone().unwrap_or_default(),
                    label: d.label.clone(),
                    root: normalize_plugin_path(d.root.clone()),
                    project_path: normalize_plugin_path(project_path),
                    shared_worktree: d.shared_worktree,
                    // Never connected (or its window would exist) — the dock
                    // badges it as its real backend, disconnected.
                    remote: d.authority_spec.remote_backend_info(false),
                    selected_agent_terminal_id: None,
                }
            });
        let mut session_infos: Vec<fresh_core::api::WindowInfo> = self
            .windows
            .values()
            .map(|s| {
                let slot = s.plugin_state.get("orchestrator");
                // Normalise project_path at the API boundary: explicit
                // non-empty value if the orchestrator recorded one,
                // otherwise the session's root. Filtering empty strings
                // is the same guard the plugin used to apply via
                // `?? root` / `|| root` — now centralised so plugins
                // can treat `project_path` as an always-set `string`.
                let project_path = slot
                    .and_then(|m| m.get("project_path"))
                    .and_then(|v| v.as_str())
                    .filter(|p| !p.is_empty())
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| s.root.clone());
                let shared_worktree = slot
                    .and_then(|m| m.get("shared_worktree"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                fresh_core::api::WindowInfo {
                    id: s.id,
                    stable_id: s.stable_id.clone(),
                    label: s.label.clone(),
                    root: normalize_plugin_path(s.root.clone()),
                    project_path: normalize_plugin_path(project_path),
                    shared_worktree,
                    selected_agent_terminal_id: s.tracked_agent_terminal.and_then(|terminal_id| {
                        let live = s
                            .terminal_buffers
                            .values()
                            .any(|binding| binding.terminal_id == terminal_id)
                            && s.terminal_manager
                                .get(terminal_id)
                                .is_some_and(|handle| handle.is_alive());
                        live.then_some(fresh_core::WindowTerminalId::new(s.id, terminal_id))
                    }),

                    remote: s.authority_spec.remote_backend_info(
                        s.authority.filesystem.remote_connection_info().is_some()
                            && s.authority.filesystem.is_remote_connected(),
                    ),
                }
            })
            .collect();
        session_infos.extend(dormant_infos);
        session_infos.sort_by_key(|s| s.id.0);
        snapshot.windows = session_infos;
        if let Some(window_id) = scoped_window {
            snapshot.windows.retain(|window| window.id == window_id);
        }
        snapshot.active_window_id = self.active_window;

        // Reserialize config only when the underlying `Arc<Config>`
        // pointer has actually moved since the last refresh —
        // `Arc::ptr_eq` vs `config_snapshot_anchor` is a sound cache
        // key because the anchor keeps `self.config`'s strong count
        // at ≥ 2, forcing every `Arc::make_mut` on the editor side
        // to CoW into a new allocation. On idle (no config mutation),
        // this branch is skipped entirely and the snapshot update is
        // a refcount bump.
        if !Arc::ptr_eq(&self.config, &self.config_snapshot_anchor) {
            let json = serde_json::to_value(&*self.config).unwrap_or(serde_json::Value::Null);
            self.config_cached_json = Arc::new(json);
            self.config_snapshot_anchor = Arc::clone(&self.config);
        }
        snapshot.config = Arc::clone(&self.config_cached_json);

        // Cached raw user config file contents (not merged with defaults).
        // Lets plugins distinguish user-set from default values.
        snapshot.user_config = Arc::clone(&self.user_config_raw);

        // Merge plugin global states from Rust-side store.
        // `or_insert` preserves JS-side write-through entries.
        for (plugin_name, state_map) in &self.plugin_global_state {
            let entry = snapshot
                .plugin_global_states
                .entry(plugin_name.clone())
                .or_default();
            for (key, value) in state_map {
                entry.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        if !private_snapshot {
            snapshot.host_revision = snapshot.host_revision.wrapping_add(1);
        }
    }

    fn create_plugin_snapshot(
        &mut self,
        window_id: fresh_core::WindowId,
        scoped: bool,
    ) -> Option<Arc<std::sync::RwLock<EditorStateSnapshot>>> {
        if !self.windows.contains_key(&window_id) {
            return None;
        }
        let previous = self.active_window;
        self.switch_active_window_pointer(window_id);
        let snapshot = Arc::new(std::sync::RwLock::new(EditorStateSnapshot::new()));
        self.update_plugin_state_snapshot_to(
            Some(Arc::clone(&snapshot)),
            scoped.then_some(window_id),
        );
        self.switch_active_window_pointer(previous);
        Some(snapshot)
    }

    pub(crate) fn create_scoped_plugin_snapshot(
        &mut self,
        window_id: fresh_core::WindowId,
    ) -> Option<Arc<std::sync::RwLock<EditorStateSnapshot>>> {
        self.create_plugin_snapshot(window_id, true)
    }
    pub(crate) fn plugin_authority_stamp(
        &self,
        window_id: fresh_core::WindowId,
    ) -> Option<fresh_core::api::AuthorityStamp> {
        self.windows
            .get(&window_id)
            .map(|window| window.authority().stamp())
    }
    pub(crate) fn plugin_invocation(
        &mut self,
        window_id: fresh_core::WindowId,
    ) -> Option<fresh_core::api::PluginInvocation> {
        let authority = self.windows.get(&window_id)?.authority().stamp();
        let snapshot = self.create_plugin_snapshot(window_id, false)?;
        Some(fresh_core::api::PluginInvocation {
            window_id,
            authority: Some(authority),
            state_snapshot: Some(snapshot),
        })
    }
    pub(crate) fn run_plugin_hook_for_window(
        &mut self,
        window_id: fresh_core::WindowId,
        hook_name: &str,
        args: fresh_core::hooks::HookArgs,
    ) -> bool {
        if !self
            .plugin_manager
            .read()
            .unwrap()
            .has_subscribers(hook_name)
        {
            return false;
        }
        let Some(invocation) = self.plugin_invocation(window_id) else {
            return false;
        };
        self.plugin_manager
            .read()
            .unwrap()
            .run_hook_with_invocation(hook_name, args, Some(invocation))
    }

    pub(crate) fn run_plugin_hook_for_plugin_in_window(
        &mut self,
        plugin: &str,
        window_id: fresh_core::WindowId,
        hook_name: &str,
        args: fresh_core::hooks::HookArgs,
    ) -> bool {
        if !self
            .plugin_manager
            .read()
            .unwrap()
            .has_subscriber(plugin, hook_name)
        {
            return false;
        }
        let Some(invocation) = self.plugin_invocation(window_id) else {
            return false;
        };
        self.plugin_manager
            .read()
            .unwrap()
            .run_hook_for_plugin_with_invocation(plugin, hook_name, args, Some(invocation))
    }

    pub(crate) fn closed_plugin_invocation(
        window_id: fresh_core::WindowId,
    ) -> fresh_core::api::PluginInvocation {
        fresh_core::api::PluginInvocation {
            window_id,
            authority: None,
            state_snapshot: None,
        }
    }

    fn scoped_plugin_command_allowed(
        &self,
        scope: fresh_core::WindowId,
        command: &PluginCommand,
    ) -> bool {
        match command {
            // Window-addressed operations may only touch the immutable script scope.
            PluginCommand::SetActiveWindow { id }
            | PluginCommand::ActivateWindow { id, .. }
            | PluginCommand::CloseWindow { id }
            | PluginCommand::PrewarmWindow { id }
            | PluginCommand::SignalWindow { id, .. }
            | PluginCommand::StopWindow { id, .. } => *id == scope,
            PluginCommand::SetActiveWindowAnimated { id, .. } => *id == scope,
            PluginCommand::OpenFileInBackground { window_id, .. } => {
                window_id.map_or(true, |id| id == scope)
            }
            PluginCommand::SpawnProcess { window_id, .. }
            | PluginCommand::SpawnBackgroundProcess { window_id, .. }
            | PluginCommand::KillBackgroundProcess { window_id, .. }
            | PluginCommand::SpawnProcessWait { window_id, .. }
            | PluginCommand::SetWindowState { window_id, .. }
            | PluginCommand::SetRemoteIndicatorState { window_id, .. }
            | PluginCommand::ClearRemoteIndicatorState { window_id }
            | PluginCommand::CreateTerminal { window_id, .. } => *window_id == scope,
            PluginCommand::SendTerminalInput { terminal_id, .. }
            | PluginCommand::CloseTerminal { terminal_id } => terminal_id.window == scope,

            // These commands are local to the temporarily-selected scoped window.
            PluginCommand::InsertText { .. }
            | PluginCommand::DeleteRange { .. }
            | PluginCommand::AddOverlay { .. }
            | PluginCommand::RemoveOverlay { .. }
            | PluginCommand::SetStatus { .. }
            | PluginCommand::SetStatusBarValue { .. }
            | PluginCommand::WatchPath { .. }
            | PluginCommand::UnwatchPath { .. }
            | PluginCommand::InsertAtCursor { .. }
            | PluginCommand::Delay { .. }
            | PluginCommand::HttpFetch { .. }
            | PluginCommand::SetLayoutHints { .. }
            | PluginCommand::SetLineNumbers { .. }
            | PluginCommand::SetIndentationGuide { .. }
            | PluginCommand::SetViewMode { .. }
            | PluginCommand::SetLineWrap { .. }
            | PluginCommand::SetViewState { .. }
            | PluginCommand::ClearAllOverlays { .. }
            | PluginCommand::ClearNamespace { .. }
            | PluginCommand::ClearOverlaysInRange { .. }
            | PluginCommand::ClearOverlaysInRangeForNamespace { .. }
            | PluginCommand::AddVirtualText { .. }
            | PluginCommand::AddVirtualTextStyled { .. }
            | PluginCommand::RemoveVirtualText { .. }
            | PluginCommand::RemoveVirtualTextsByPrefix { .. }
            | PluginCommand::ClearVirtualTexts { .. }
            | PluginCommand::AddVirtualLine { .. }
            | PluginCommand::ClearVirtualTextNamespace { .. }
            | PluginCommand::ClearVirtualLinesInRange { .. }
            | PluginCommand::AddConceal { .. }
            | PluginCommand::ClearConcealNamespace { .. }
            | PluginCommand::ClearConcealsInRange { .. }
            | PluginCommand::ClearConcealsInRangeForNamespace { .. }
            | PluginCommand::AddFold { .. }
            | PluginCommand::ClearFolds { .. }
            | PluginCommand::SetFoldingRanges { .. }
            | PluginCommand::AddSoftBreak { .. }
            | PluginCommand::ClearSoftBreakNamespace { .. }
            | PluginCommand::ClearSoftBreaksInRange { .. }
            | PluginCommand::RefreshLines { .. }
            | PluginCommand::RefreshAllLines
            | PluginCommand::HookCompleted { .. }
            | PluginCommand::SetLineIndicator { .. }
            | PluginCommand::SetLineIndicators { .. }
            | PluginCommand::ClearLineIndicators { .. }
            | PluginCommand::SetScrollbarMarkers { .. }
            | PluginCommand::SetScrollbarMarkersInRange { .. }
            | PluginCommand::ClearScrollbarMarkers { .. }
            | PluginCommand::SetFileExplorerDecorations { .. }
            | PluginCommand::ClearFileExplorerDecorations { .. }
            | PluginCommand::SetFileExplorerSlots { .. }
            | PluginCommand::ClearFileExplorerSlots { .. }
            | PluginCommand::OpenFileAtLocation { .. }
            | PluginCommand::OpenFileInSplit { .. }
            | PluginCommand::CancelPrompt
            | PluginCommand::StartPrompt { .. }
            | PluginCommand::StartPromptWithInitial { .. }
            | PluginCommand::StartPromptAsync { .. }
            | PluginCommand::StartFilePickAsync { .. }
            | PluginCommand::AwaitNextKey { .. }
            | PluginCommand::SetKeyCaptureActive { .. }
            | PluginCommand::SetPromptSuggestions { .. }
            | PluginCommand::SetPromptInputSync { .. }
            | PluginCommand::SetPromptTitle { .. }
            | PluginCommand::SetPromptFooter { .. }
            | PluginCommand::SetPromptToolbar { .. }
            | PluginCommand::SetPromptStatus { .. }
            | PluginCommand::ToggleOverlayToolbarWidget { .. }
            | PluginCommand::SetPromptSelectedIndex { .. }
            | PluginCommand::CreateVirtualBuffer { .. }
            | PluginCommand::CreateVirtualBufferWithContent { .. }
            | PluginCommand::CreateVirtualBufferInSplit { .. }
            | PluginCommand::SetVirtualBufferContent { .. }
            | PluginCommand::GetTextPropertiesAtCursor { .. }
            | PluginCommand::CreateBufferGroup { .. }
            | PluginCommand::SetPanelContent { .. }
            | PluginCommand::CloseBufferGroup { .. }
            | PluginCommand::FocusPanel { .. }
            | PluginCommand::ShowBuffer { .. }
            | PluginCommand::StartAnimationArea { .. }
            | PluginCommand::StartAnimationVirtualBuffer { .. }
            | PluginCommand::CancelAnimation { .. }
            | PluginCommand::CreateVirtualBufferInExistingSplit { .. }
            | PluginCommand::CloseBuffer { .. }
            | PluginCommand::CloseOtherBuffersInSplit { .. }
            | PluginCommand::CloseAllBuffersInSplit { .. }
            | PluginCommand::CloseBuffersToRightInSplit { .. }
            | PluginCommand::CloseBuffersToLeftInSplit { .. }
            | PluginCommand::MoveTabLeft
            | PluginCommand::MoveTabRight
            | PluginCommand::CreateCompositeBuffer { .. }
            | PluginCommand::UpdateCompositeAlignment { .. }
            | PluginCommand::CloseCompositeBuffer { .. }
            | PluginCommand::FlushLayout
            | PluginCommand::MoveBufferToSplit { .. }
            | PluginCommand::SetLineTargets { .. }
            | PluginCommand::SplitWindow { .. }
            | PluginCommand::CompositeNextHunk { .. }
            | PluginCommand::CompositePrevHunk { .. }
            | PluginCommand::FocusSplit { .. }
            | PluginCommand::SetSplitBuffer { .. }
            | PluginCommand::SetSplitScroll { .. }
            | PluginCommand::RequestHighlights { .. }
            | PluginCommand::CloseSplit { .. }
            | PluginCommand::SetSplitRatio { .. }
            | PluginCommand::SetSplitLabel { .. }
            | PluginCommand::ClearSplitLabel { .. }
            | PluginCommand::GetSplitByLabel { .. }
            | PluginCommand::DistributeSplitsEvenly { .. }
            | PluginCommand::SetBufferCursor { .. }
            | PluginCommand::SetBufferShowCursors { .. }
            | PluginCommand::SendLspRequest { .. }
            | PluginCommand::SetClipboard { .. }
            | PluginCommand::DeleteSelection
            | PluginCommand::SetReviewDiffHunks { .. }
            | PluginCommand::GetBufferText { .. }
            | PluginCommand::GetLineStartPosition { .. }
            | PluginCommand::GetLineEndPosition { .. }
            | PluginCommand::GetBufferLineCount { .. }
            | PluginCommand::GetCompositeCursorInfo { .. }
            | PluginCommand::OpenFileStreaming { .. }
            | PluginCommand::RefreshBufferFromDisk { .. }
            | PluginCommand::SetBufferGroupPanelBuffer { .. }
            | PluginCommand::ScrollToLineCenter { .. }
            | PluginCommand::ScrollBufferToLine { .. }
            | PluginCommand::ShowActionPopup { .. }
            | PluginCommand::MarkBufferReadOnly { .. }
            | PluginCommand::CreateScrollSyncGroup { .. }
            | PluginCommand::SetScrollSyncAnchors { .. }
            | PluginCommand::RemoveScrollSyncGroup { .. }
            | PluginCommand::SaveBufferToPath { .. }
            | PluginCommand::RegisterDiffBaseline { .. }
            | PluginCommand::DiffAgainstBaseline { .. }
            | PluginCommand::DiffBaselinePair { .. }
            | PluginCommand::GetBaselineLines { .. }
            | PluginCommand::RefreshDiffBaseline { .. }
            | PluginCommand::ReleaseDiffBaseline { .. }
            | PluginCommand::GrepProject { .. }
            | PluginCommand::BeginSearch { .. }
            | PluginCommand::ReplaceInBuffer { .. }
            | PluginCommand::MountWidgetPanel { .. }
            | PluginCommand::UpdateWidgetPanel { .. }
            | PluginCommand::UnmountWidgetPanel { .. }
            | PluginCommand::WidgetCommand { .. }
            | PluginCommand::WidgetMutate { .. } => true,

            // Global registration/configuration, host authority, remote lifecycle,
            // companion control, and future command variants are denied by default.
            _ => false,
        }
    }
    fn source_plugin_command_allowed(
        &self,
        source: fresh_core::WindowId,
        command: &PluginCommand,
        trusted_orchestrator: bool,
    ) -> bool {
        match command {
            PluginCommand::SetActiveWindow { id }
            | PluginCommand::ActivateWindow { id, .. }
            | PluginCommand::CloseWindow { id }
            | PluginCommand::PrewarmWindow { id }
            | PluginCommand::SignalWindow { id, .. }
            | PluginCommand::StopWindow { id, .. } => *id == source || trusted_orchestrator,
            PluginCommand::SetActiveWindowAnimated { id, .. } => {
                *id == source || trusted_orchestrator
            }
            PluginCommand::OpenFileInBackground { window_id, .. } => {
                window_id.map_or(true, |id| id == source)
            }
            PluginCommand::SpawnProcess { window_id, .. }
            | PluginCommand::SpawnBackgroundProcess { window_id, .. }
            | PluginCommand::KillBackgroundProcess { window_id, .. }
            | PluginCommand::SpawnProcessWait { window_id, .. }
            | PluginCommand::SetRemoteIndicatorState { window_id, .. }
            | PluginCommand::ClearRemoteIndicatorState { window_id }
            | PluginCommand::CreateTerminal { window_id, .. }
            | PluginCommand::SetAuthority { window_id, .. }
            | PluginCommand::ClearAuthority { window_id }
            | PluginCommand::AttachRemoteAgent { window_id, .. } => *window_id == source,
            PluginCommand::SetWindowState { window_id, .. } => {
                *window_id == source || trusted_orchestrator
            }
            PluginCommand::SendTerminalInput { terminal_id, .. }
            | PluginCommand::CloseTerminal { terminal_id } => terminal_id.window == source,
            PluginCommand::RestoreWorkspaceWindow { .. } => trusted_orchestrator,
            _ => true,
        }
    }
    fn async_command_callback_id(command: &PluginCommand) -> Option<JsCallbackId> {
        if matches!(command, PluginCommand::CompleteCommand { .. }) {
            return None;
        }
        let serialized = serde_json::to_value(command).ok()?;
        let payload = serialized.as_object()?.values().next()?.as_object()?;
        payload
            .get("callback_id")
            .or_else(|| payload.get("request_id"))
            .and_then(serde_json::Value::as_u64)
            .map(JsCallbackId::from)
    }

    fn reject_dropped_async_command(
        &self,
        command: &PluginCommand,
        context: &PluginCommandContext,
        reason: &str,
    ) {
        if let Some(callback_id) = Self::async_command_callback_id(command) {
            self.reject_callback_for_context(context, callback_id, reason.to_string());
        }
    }

    pub fn dispatch_plugin_command_envelope(&mut self, envelope: PluginCommandEnvelope) {
        let PluginCommandEnvelope { command, context } = envelope;
        if let PluginCommand::CompleteCommand {
            request_id,
            ok,
            output,
            error,
        } = &command
        {
            let request_id = *request_id;
            let current_authority = context.source_window.and_then(|window_id| {
                self.windows
                    .get(&window_id)
                    .map(|window| window.authority().stamp())
            });
            if context.is_agent_script() {
                if let Some(plugin_name) = crate::server::command_access::complete_agent_script(
                    &context,
                    request_id,
                    current_authority,
                    *ok,
                    output.clone(),
                    error.clone(),
                ) {
                    self.plugin_manager
                        .read()
                        .unwrap()
                        .unload_plugin_request(&plugin_name);
                } else {
                    tracing::warn!(
                        request_id,
                        "rejected forged or stale agent-script completion"
                    );
                }
            } else {
                tracing::warn!(
                    request_id,
                    plugin = %context.plugin_name,
                    "rejected command completion outside its exact agent-script instance"
                );
            }
            return;
        }
        let instance_is_active = self
            .plugin_manager
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_plugin_instance_active(context.plugin_instance_id);
        if !plugin_context_may_dispatch(&context, instance_is_active) {
            tracing::warn!(
                plugin = %context.plugin_name,
                ?context.plugin_instance_id,
                command = VariantNameSink::of(&command).as_str(),
                "dropping command from an unloaded or replaced plugin instance"
            );
            self.reject_dropped_async_command(
                &command,
                &context,
                "plugin instance was unloaded or replaced",
            );
            return;
        }

        let trusted_orchestrator =
            context.window_scope.is_none() && context.is_trusted_orchestrator();
        let requested_scope = context.window_scope.or(context.source_window);
        let source_closed = context
            .source_window
            .is_some_and(|source| !self.windows.contains_key(&source));
        // Lifecycle hooks may finish after their source window is gone. Their
        // editor-global state publication and completion bookkeeping remain
        // valid, but no window-scoped command may fall through to another window.
        if source_closed
            && matches!(
                command,
                PluginCommand::SetGlobalState { .. } | PluginCommand::HookCompleted { .. }
            )
        {
            self.dispatch_plugin_command_measured(command, &context);
            return;
        }
        let scope = match requested_scope {
            None => {
                self.dispatch_plugin_command_measured(command, &context);
                return;
            }
            Some(scope) if self.windows.contains_key(&scope) => scope,
            Some(_)
                if trusted_orchestrator
                    && source_closed
                    && self.windows.contains_key(&self.active_window) =>
            {
                self.active_window
            }
            Some(scope) => {
                if let PluginCommand::SyncSnapshot { request_id } = &command {
                    if let Some(snapshot) = &context.state_snapshot {
                        *snapshot.write().unwrap() = EditorStateSnapshot::new();
                    }
                    self.send_plugin_response(fresh_core::api::PluginResponse::SnapshotSynced {
                        request_id: *request_id,
                    });
                } else {
                    tracing::warn!(?scope, "dropping command for a closed source window");
                }
                if !matches!(command, PluginCommand::SyncSnapshot { .. }) {
                    self.reject_dropped_async_command(
                        &command,
                        &context,
                        "plugin command source window is closed",
                    );
                }
                return;
            }
        };
        let using_trusted_fallback =
            trusted_orchestrator && source_closed && context.source_window != Some(scope);

        // The completion sentinel is bookkeeping rather than a window mutation;
        // dispatch it before authority checks while its source window is live.
        if matches!(command, PluginCommand::HookCompleted { .. }) {
            self.dispatch_plugin_command_measured(command, &context);
            return;
        }

        if !using_trusted_fallback {
            if let Some(expected) = context.source_authority {
                let actual = self
                    .windows
                    .get(&scope)
                    .map(|window| window.authority().stamp());
                if actual != Some(expected) {
                    tracing::warn!(
                        ?scope,
                        ?expected,
                        ?actual,
                        "dropping command from a replaced source authority"
                    );
                    self.reject_dropped_async_command(
                        &command,
                        &context,
                        "plugin command source authority was replaced",
                    );
                    return;
                }
            }
        }

        if let PluginCommand::SyncSnapshot { request_id } = &command {
            let previous = self.active_window;
            self.switch_active_window_pointer(scope);
            if let Some(snapshot) = &context.state_snapshot {
                self.update_plugin_state_snapshot_to(
                    Some(Arc::clone(snapshot)),
                    context.window_scope,
                );
            }
            self.send_plugin_response(fresh_core::api::PluginResponse::SnapshotSynced {
                request_id: *request_id,
            });
            self.switch_active_window_pointer(previous);
            return;
        }

        let allowed = if context.is_agent_script() {
            self.scoped_plugin_command_allowed(scope, &command)
        } else {
            self.source_plugin_command_allowed(scope, &command, trusted_orchestrator)
        };
        if !allowed {
            tracing::warn!(
                ?scope,
                command = VariantNameSink::of(&command).as_str(),
                "denied plugin command outside its immutable source window"
            );
            self.reject_dropped_async_command(
                &command,
                &context,
                "plugin command is denied outside its immutable source window",
            );
            return;
        }

        let requested_active_target = match &command {
            PluginCommand::SetActiveWindow { id }
            | PluginCommand::ActivateWindow { id, .. }
            | PluginCommand::SetActiveWindowAnimated { id, .. } => Some(*id),
            _ => None,
        };
        let creates_active_window = matches!(
            &command,
            PluginCommand::CreateWindowWithTerminal { activate: true, .. }
        );
        let closes_source_window =
            matches!(&command, PluginCommand::CloseWindow { id } if *id == scope);
        let previous = self.active_window;
        if requested_active_target.is_none() && !closes_source_window {
            self.switch_active_window_pointer(scope);
        }
        self.dispatch_plugin_command_measured(command, &context);
        let resulting_active = self.active_window;
        if self.windows.contains_key(&scope) {
            self.switch_active_window_pointer(scope);
            if let Some(snapshot) = &context.state_snapshot {
                self.update_plugin_state_snapshot_to(
                    Some(Arc::clone(snapshot)),
                    context.window_scope,
                );
            }
            self.switch_active_window_pointer(resulting_active);
        }
        let active_change_committed = requested_active_target
            .is_some_and(|target| resulting_active == target)
            || (creates_active_window && resulting_active != scope);
        if !active_change_committed && self.windows.contains_key(&previous) {
            self.switch_active_window_pointer(previous);
        }
        if self.windows.contains_key(&self.active_window) {
            self.update_plugin_state_snapshot();
        }
    }

    /// Dispatch one plugin command, timing the handler and reporting any that
    /// blocks the editor thread.
    ///
    /// This is the guard that keeps the "no plugin work on the editor thread
    /// except bounded state mutation" invariant honest: a handler that does
    /// unbounded I/O or computation shows up here by name instead of as an
    /// unattributable stall.
    ///
    /// Naming the offender is all it does. Failing the process on a single
    /// overrun would key correctness to wall-clock time on whatever machine is
    /// running — the kind of test CONTRIBUTING §3 rules out, and the reason the
    /// e2e hostile-plugin test judges a *median* dispatch latency instead. That
    /// test is where a regression actually fails the build; this is how you
    /// find out which handler caused it.
    pub(crate) fn dispatch_plugin_command_measured(
        &mut self,
        command: PluginCommand,
        context: &PluginCommandContext,
    ) {
        let label = VariantNameSink::of(&command);
        let label = label.as_str();
        let started = std::time::Instant::now();
        if let Err(e) = self.handle_plugin_command_with_context(command, context) {
            tracing::error!("Error handling plugin command {}: {}", label, e);
        }
        let elapsed = started.elapsed();
        if elapsed > super::PLUGIN_COMMAND_HANDLER_HARD_LIMIT {
            tracing::warn!(
                target: "plugin_budget",
                handler = label,
                elapsed_ms = elapsed.as_millis() as u64,
                limit_ms = super::PLUGIN_COMMAND_HANDLER_HARD_LIMIT.as_millis() as u64,
                "plugin command handler blocked the editor thread — move this work to plugin_offloop"
            );
        } else if elapsed > super::PLUGIN_COMMAND_HANDLER_LIMIT {
            tracing::debug!(
                target: "plugin_budget",
                handler = label,
                elapsed_ms = elapsed.as_millis() as u64,
                limit_ms = super::PLUGIN_COMMAND_HANDLER_LIMIT.as_millis() as u64,
                "plugin command handler ran over its editor-thread budget"
            );
        }
    }

    /// Handle a host-internal command. Plugin-thread commands must enter through
    /// [`Self::dispatch_plugin_command_envelope`] so their loader-owned context
    /// cannot be separated from the payload.
    pub fn handle_plugin_command(&mut self, command: PluginCommand) -> AnyhowResult<()> {
        let context = PluginCommandContext {
            source_window: Some(self.active_window),
            ..PluginCommandContext::default()
        };
        self.handle_plugin_command_with_context(command, &context)
    }
    fn context_may_use_privileged_terminal_options(&self, context: &PluginCommandContext) -> bool {
        if context.provenance == fresh_core::api::PluginLoadProvenance::Internal {
            return true;
        }
        context.is_trusted_orchestrator()
            && self
                .plugin_manager
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_plugin_instance_active(context.plugin_instance_id)
    }

    fn context_may_manage_bundled_plugins(context: &PluginCommandContext) -> bool {
        context.provenance == fresh_core::api::PluginLoadProvenance::Internal
    }

    fn resolve_bool_for_context(
        &self,
        context: &PluginCommandContext,
        request_id: u64,
        value: bool,
    ) {
        let manager = self.plugin_manager.read().unwrap();
        let callback_id = JsCallbackId::from(request_id);
        let json = if value { "true" } else { "false" }.to_string();
        if context.provenance == fresh_core::api::PluginLoadProvenance::Internal {
            manager.resolve_callback(callback_id, json);
        } else {
            manager.resolve_callback_for(context.plugin_instance_id, callback_id, json);
        }
    }

    fn resolve_json_for_context(
        &self,
        context: &PluginCommandContext,
        callback_id: JsCallbackId,
        json: String,
    ) {
        let manager = self.plugin_manager.read().unwrap();
        if context.provenance == fresh_core::api::PluginLoadProvenance::Internal {
            manager.resolve_callback(callback_id, json);
        } else {
            manager.resolve_callback_for(context.plugin_instance_id, callback_id, json);
        }
    }

    fn context_may_manage_workspace_persistence(&self, context: &PluginCommandContext) -> bool {
        context.window_scope.is_none()
            && context.is_trusted_orchestrator()
            && self
                .plugin_manager
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_plugin_instance_active(context.plugin_instance_id)
    }

    fn workspace_persistence_is_owned(
        &self,
        root: &std::path::Path,
        stable_id: Option<&str>,
    ) -> bool {
        let target = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let same_root = |candidate: &std::path::Path| {
            candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.to_path_buf())
                == target
                || candidate == root
        };
        self.windows.values().any(|window| {
            same_root(&window.root) && stable_id.is_none_or(|expected| window.stable_id == expected)
        }) || self.closing_windows.values().any(|window| {
            same_root(&window.root) && stable_id.is_none_or(|expected| window.stable_id == expected)
        }) || self.dormant_remote.values().any(|window| {
            same_root(&window.root)
                && stable_id.is_none_or(|expected| window.stable_id.as_deref() == Some(expected))
        })
    }

    fn reject_callback_for_context(
        &self,
        context: &PluginCommandContext,
        callback_id: JsCallbackId,
        error: String,
    ) {
        let manager = self.plugin_manager.read().unwrap();
        if context.provenance == fresh_core::api::PluginLoadProvenance::Internal {
            manager.reject_callback(callback_id, error);
        } else {
            manager.reject_callback_for(context.plugin_instance_id, callback_id, error);
        }
    }

    fn handle_plugin_command_with_context(
        &mut self,
        command: PluginCommand,
        context: &PluginCommandContext,
    ) -> AnyhowResult<()> {
        match command {
            // ==================== Text Editing Commands ====================
            PluginCommand::InsertText {
                buffer_id,
                position,
                text,
            } => {
                self.handle_insert_text(buffer_id, position, text);
            }
            PluginCommand::DeleteRange { buffer_id, range } => {
                self.handle_delete_range(buffer_id, range);
            }
            PluginCommand::InsertAtCursor { text } => {
                self.handle_insert_at_cursor(text);
            }
            PluginCommand::DeleteSelection => {
                self.handle_delete_selection();
            }

            // ==================== Overlay Commands ====================
            PluginCommand::AddOverlay {
                buffer_id,
                namespace,
                range,
                options,
            } => {
                self.handle_add_overlay(buffer_id, namespace, range, options);
            }
            PluginCommand::RemoveOverlay { buffer_id, handle } => {
                self.handle_remove_overlay(buffer_id, handle);
            }
            PluginCommand::ClearAllOverlays { buffer_id } => {
                self.handle_clear_all_overlays(buffer_id);
            }
            PluginCommand::ClearNamespace {
                buffer_id,
                namespace,
            } => {
                self.handle_clear_namespace(buffer_id, namespace);
            }
            PluginCommand::ClearOverlaysInRange {
                buffer_id,
                start,
                end,
            } => {
                self.handle_clear_overlays_in_range(buffer_id, start, end);
            }
            PluginCommand::ClearOverlaysInRangeForNamespace {
                buffer_id,
                namespace,
                start,
                end,
            } => {
                self.handle_clear_overlays_in_range_for_namespace(buffer_id, namespace, start, end);
            }

            // ==================== Virtual Text Commands ====================
            PluginCommand::AddVirtualText {
                buffer_id,
                virtual_text_id,
                position,
                text,
                color,
                use_bg,
                before,
            } => {
                self.handle_add_virtual_text(
                    buffer_id,
                    virtual_text_id,
                    position,
                    text,
                    color,
                    use_bg,
                    before,
                );
            }
            PluginCommand::AddVirtualTextStyled {
                buffer_id,
                virtual_text_id,
                position,
                text,
                fg,
                bg,
                bold,
                italic,
                before,
            } => {
                self.handle_add_virtual_text_styled(
                    buffer_id,
                    virtual_text_id,
                    position,
                    text,
                    fg,
                    bg,
                    bold,
                    italic,
                    before,
                );
            }
            PluginCommand::RemoveVirtualText {
                buffer_id,
                virtual_text_id,
            } => {
                self.handle_remove_virtual_text(buffer_id, virtual_text_id);
            }
            PluginCommand::RemoveVirtualTextsByPrefix { buffer_id, prefix } => {
                self.handle_remove_virtual_texts_by_prefix(buffer_id, prefix);
            }
            PluginCommand::ClearVirtualTexts { buffer_id } => {
                self.handle_clear_virtual_texts(buffer_id);
            }
            PluginCommand::AddVirtualLine {
                buffer_id,
                position,
                text,
                fg_color,
                bg_color,
                above,
                namespace,
                priority,
                gutter_glyph,
                gutter_color,
                text_overlays,
                epoch,
            } => {
                self.handle_add_virtual_line(
                    buffer_id,
                    position,
                    text,
                    fg_color,
                    bg_color,
                    above,
                    namespace,
                    priority,
                    gutter_glyph,
                    gutter_color,
                    text_overlays,
                    epoch,
                );
            }
            PluginCommand::ClearVirtualTextNamespace {
                buffer_id,
                namespace,
            } => {
                self.handle_clear_virtual_text_namespace(buffer_id, namespace);
            }
            PluginCommand::ClearVirtualLinesInRange {
                buffer_id,
                namespace,
                start,
                end,
                epoch,
            } => {
                self.handle_clear_virtual_lines_in_range(buffer_id, namespace, start, end, epoch);
            }

            // ==================== Conceal Commands ====================
            PluginCommand::AddConceal {
                buffer_id,
                namespace,
                start,
                end,
                replacement,
                epoch,
                activation,
            } => {
                self.handle_add_conceal(
                    buffer_id,
                    namespace,
                    start,
                    end,
                    replacement,
                    epoch,
                    activation,
                );
            }
            PluginCommand::ClearConcealNamespace {
                buffer_id,
                namespace,
            } => {
                self.handle_clear_conceal_namespace(buffer_id, namespace);
            }
            PluginCommand::ClearConcealsInRange {
                buffer_id,
                start,
                end,
                epoch,
            } => {
                self.handle_clear_conceals_in_range(buffer_id, start, end, epoch);
            }
            PluginCommand::ClearConcealsInRangeForNamespace {
                buffer_id,
                namespace,
                start,
                end,
                epoch,
            } => {
                self.handle_clear_conceals_in_range_for_namespace(
                    buffer_id, namespace, start, end, epoch,
                );
            }

            PluginCommand::AddFold {
                buffer_id,
                start,
                end,
                placeholder,
            } => {
                self.handle_add_fold(buffer_id, start, end, placeholder);
            }
            PluginCommand::ClearFolds { buffer_id } => {
                self.handle_clear_folds(buffer_id);
            }
            PluginCommand::SetFoldingRanges { buffer_id, ranges } => {
                self.handle_set_folding_ranges(buffer_id, ranges);
            }

            // ==================== Soft Break Commands ====================
            PluginCommand::AddSoftBreak {
                buffer_id,
                namespace,
                position,
                indent,
                epoch,
                activation,
            } => {
                self.handle_add_soft_break(
                    buffer_id, namespace, position, indent, epoch, activation,
                );
            }
            PluginCommand::ClearSoftBreakNamespace {
                buffer_id,
                namespace,
            } => {
                self.handle_clear_soft_break_namespace(buffer_id, namespace);
            }
            PluginCommand::ClearSoftBreaksInRange {
                buffer_id,
                start,
                end,
                epoch,
            } => {
                self.handle_clear_soft_breaks_in_range(buffer_id, start, end, epoch);
            }

            // ==================== Menu Commands ====================
            PluginCommand::AddMenuItem {
                menu_label,
                item,
                position,
            } => {
                self.handle_add_menu_item(menu_label, item, position);
            }
            PluginCommand::AddMenu { menu, position } => {
                self.handle_add_menu(menu, position);
            }
            PluginCommand::RemoveMenuItem {
                menu_label,
                item_label,
            } => {
                self.handle_remove_menu_item(menu_label, item_label);
            }
            PluginCommand::RemoveMenu { menu_label } => {
                self.handle_remove_menu(menu_label);
            }

            // ==================== Split Commands ====================
            PluginCommand::FocusSplit { split_id } => {
                self.handle_focus_split(split_id);
            }
            PluginCommand::SetSplitBuffer {
                split_id,
                buffer_id,
            } => {
                self.handle_set_split_buffer(split_id, buffer_id);
            }
            PluginCommand::MoveBufferToSplit {
                buffer_id,
                split_id,
            } => {
                self.handle_move_buffer_to_split(buffer_id, split_id);
            }
            PluginCommand::SetSplitScroll { split_id, top_byte } => {
                self.handle_set_split_scroll(split_id, top_byte);
            }
            PluginCommand::RequestHighlights {
                buffer_id,
                range,
                request_id,
            } => {
                self.handle_request_highlights(buffer_id, range, request_id);
            }
            PluginCommand::CloseSplit { split_id } => {
                self.handle_close_split(split_id);
            }
            PluginCommand::SetSplitRatio { split_id, ratio } => {
                self.handle_set_split_ratio(split_id, ratio);
            }
            PluginCommand::SetSplitLabel { split_id, label } => {
                self.handle_set_split_label(split_id, label);
            }
            PluginCommand::ClearSplitLabel { split_id } => {
                self.handle_clear_split_label(split_id);
            }
            PluginCommand::GetSplitByLabel { label, request_id } => {
                self.handle_get_split_by_label(label, request_id);
            }
            PluginCommand::DistributeSplitsEvenly { split_ids: _ } => {
                self.handle_distribute_splits_evenly();
            }
            PluginCommand::SetBufferCursor {
                buffer_id,
                position,
            } => {
                self.handle_set_buffer_cursor(buffer_id, position);
            }
            PluginCommand::SetBufferShowCursors { buffer_id, show } => {
                self.handle_set_buffer_show_cursors(buffer_id, show);
            }

            // ==================== View/Layout Commands ====================
            PluginCommand::SetLayoutHints {
                buffer_id,
                split_id,
                range: _,
                hints,
            } => {
                self.handle_set_layout_hints(buffer_id, split_id, hints);
            }
            PluginCommand::SetLineNumbers { buffer_id, enabled } => {
                self.handle_set_line_numbers(buffer_id, enabled);
            }
            PluginCommand::SetIndentationGuide { buffer_id, enabled } => {
                self.handle_set_indentation_guide(buffer_id, enabled);
            }
            PluginCommand::SetViewMode { buffer_id, mode } => {
                self.handle_set_view_mode(buffer_id, &mode);
            }
            PluginCommand::SetLineWrap {
                buffer_id,
                split_id,
                enabled,
            } => {
                self.handle_set_line_wrap(buffer_id, split_id, enabled);
            }
            PluginCommand::SetViewState {
                buffer_id,
                key,
                value,
            } => {
                self.handle_set_view_state(buffer_id, key, value);
            }
            PluginCommand::SetGlobalState {
                plugin_name,
                key,
                value,
            } => {
                self.handle_set_global_state(plugin_name, key, value);
            }
            PluginCommand::SetWindowState {
                window_id,
                key,
                value,
            } => {
                self.handle_set_session_state(
                    window_id,
                    context.plugin_name.to_string(),
                    key,
                    value,
                );
            }
            PluginCommand::RefreshLines { buffer_id } => {
                self.handle_refresh_lines(buffer_id);
            }
            PluginCommand::RefreshAllLines => {
                self.handle_refresh_all_lines();
            }
            PluginCommand::HookCompleted { hook_name } => {
                self.complete_terminal_output_hook(&hook_name);
                self.complete_omp_companion_hook(&hook_name);
            }
            PluginCommand::SetLineIndicator {
                buffer_id,
                line,
                namespace,
                symbol,
                color,
                priority,
            } => {
                self.handle_set_line_indicator(buffer_id, line, namespace, symbol, color, priority);
            }
            PluginCommand::SetLineIndicators {
                buffer_id,
                lines,
                namespace,
                symbol,
                color,
                priority,
            } => {
                self.handle_set_line_indicators(
                    buffer_id, lines, namespace, symbol, color, priority,
                );
            }
            PluginCommand::ClearLineIndicators {
                buffer_id,
                namespace,
            } => {
                self.handle_clear_line_indicators(buffer_id, namespace);
            }
            PluginCommand::SetScrollbarMarkers {
                buffer_id,
                namespace,
                markers,
            } => {
                self.handle_set_scrollbar_markers(buffer_id, namespace, markers);
            }
            PluginCommand::SetScrollbarMarkersInRange {
                buffer_id,
                namespace,
                start,
                end,
                markers,
            } => {
                self.handle_set_scrollbar_markers_in_range(
                    buffer_id, namespace, start, end, markers,
                );
            }
            PluginCommand::ClearScrollbarMarkers {
                buffer_id,
                namespace,
            } => {
                self.handle_clear_scrollbar_markers(buffer_id, namespace);
            }
            PluginCommand::SetFileExplorerDecorations {
                namespace,
                decorations,
            } => {
                self.active_window_mut()
                    .handle_set_file_explorer_decorations(namespace, decorations);
            }
            PluginCommand::ClearFileExplorerDecorations { namespace } => {
                self.active_window_mut()
                    .handle_clear_file_explorer_decorations(&namespace);
            }
            PluginCommand::SetFileExplorerSlots { namespace, slots } => {
                self.active_window_mut()
                    .handle_set_file_explorer_slots(namespace, slots);
            }
            PluginCommand::ClearFileExplorerSlots { namespace } => {
                self.active_window_mut()
                    .handle_clear_file_explorer_slots(&namespace);
            }

            // ==================== Status/Prompt Commands ====================
            PluginCommand::SetStatus { message } => {
                self.handle_set_status(message);
            }
            PluginCommand::ApplyTheme { theme_name } => {
                self.apply_theme(&theme_name);
            }
            PluginCommand::OverrideThemeColors { overrides } => {
                self.handle_override_theme_colors(overrides);
            }
            PluginCommand::ReloadConfig => {
                self.reload_config();
            }
            PluginCommand::SetSetting { path, value, .. } => {
                self.handle_set_setting(path, value);
            }
            PluginCommand::AddPluginConfigField {
                plugin_name,
                field_name,
                field_schema,
            } => {
                self.handle_add_plugin_config_field(plugin_name, field_name, field_schema);
            }
            PluginCommand::ReloadThemes { apply_theme } => {
                self.handle_reload_themes(apply_theme);
            }
            PluginCommand::RegisterGrammar {
                language,
                grammar_path,
                extensions,
            } => {
                self.handle_register_grammar(language, grammar_path, extensions);
            }
            PluginCommand::RegisterLanguageConfig { language, config } => {
                self.handle_register_language_config(language, config);
            }
            PluginCommand::RegisterLspServer { language, config } => {
                self.handle_register_lsp_server(language, config);
            }
            PluginCommand::ReloadGrammars { callback_id } => {
                self.handle_reload_grammars(callback_id);
            }
            PluginCommand::CancelPrompt => {
                self.cancel_prompt();
            }
            PluginCommand::StartPrompt {
                label,
                prompt_type,
                floating_overlay,
            } => {
                self.handle_start_prompt(label, prompt_type, floating_overlay);
            }
            PluginCommand::StartPromptWithInitial {
                label,
                prompt_type,
                initial_value,
                floating_overlay,
            } => {
                self.handle_start_prompt_with_initial(
                    label,
                    prompt_type,
                    initial_value,
                    floating_overlay,
                );
            }
            PluginCommand::StartPromptAsync {
                label,
                initial_value,
                callback_id,
            } => {
                self.handle_start_prompt_async(label, initial_value, callback_id);
            }
            PluginCommand::StartFilePickAsync {
                label,
                directory,
                show_hidden,
                callback_id,
            } => {
                self.handle_start_file_pick_async(label, directory, show_hidden, callback_id);
            }
            PluginCommand::AwaitNextKey { callback_id } => {
                self.handle_await_next_key(callback_id);
            }
            PluginCommand::SetKeyCaptureActive { active } => {
                self.handle_set_key_capture_active(active);
            }
            PluginCommand::SetPromptSuggestions {
                suggestions,
                selected_index,
            } => {
                self.handle_set_prompt_suggestions(suggestions, selected_index);
            }
            PluginCommand::SetPromptInputSync { sync } => {
                self.handle_set_prompt_input_sync(sync);
            }
            PluginCommand::SetPromptTitle { title } => {
                self.handle_set_prompt_title(title);
            }
            PluginCommand::SetPromptFooter { footer } => {
                self.handle_set_prompt_footer(footer);
            }
            PluginCommand::SetPromptToolbar { spec } => {
                self.handle_set_prompt_toolbar(spec);
            }
            PluginCommand::ToggleOverlayToolbarWidget { key } => {
                self.toggle_overlay_toolbar_widget(&key);
            }
            PluginCommand::SetPromptStatus { status } => {
                self.handle_set_prompt_status(status);
            }
            PluginCommand::SetPromptSelectedIndex { index } => {
                self.handle_set_prompt_selected_index(index);
            }

            // ==================== Session lifecycle ====================
            // See docs/internal/orchestrator-sessions-design.md.
            PluginCommand::CreateWindow { root, label } => {
                self.handle_create_window(root, label);
            }
            PluginCommand::CreateWindowWithTerminal {
                root,
                label,
                cwd,
                command,
                relaunch,
                title,
                resume,
                env,
                companion,
                allow_script,
                selected_agent,
                activate,
                initial_state,
                request_id,
            } => {
                self.handle_create_window_with_terminal(
                    root,
                    label,
                    cwd,
                    command,
                    relaunch,
                    title,
                    resume,
                    env,
                    allow_script,
                    companion,
                    selected_agent,
                    activate,
                    initial_state,
                    request_id,
                    context,
                );
            }
            PluginCommand::RestoreWorkspaceWindow {
                root,
                label,
                stable_id,
                activate,
                callback_id,
            } => self.handle_restore_workspace_window(
                root,
                label,
                stable_id,
                activate,
                callback_id,
                context,
            ),
            PluginCommand::SendOmpCompanionCommand {
                terminal_id,
                command_type,
                target,
                request_id,
            } => self.handle_send_omp_companion_command(
                terminal_id,
                command_type,
                target,
                request_id,
                context,
            ),
            PluginCommand::SetActiveWindow { id } => {
                // Diving into a dormant remote session starts its backend
                // connect AND commits the switch immediately: the dive lands
                // in the session's empty shell (status: Connecting…) rather
                // than leaving the editor on the previous workspace while the
                // dock already selected this one — a dead host can keep the
                // connect in flight for minutes (issue #2570). A success
                // promotes the shell to the fully-restored window
                // (`promote_dormant_remote`); a failure records the reason on
                // it (Disconnected + Retry).
                if self.dormant_remote.contains_key(&id) {
                    self.ensure_dormant_shell(id);
                    self.set_active_window(id);
                    // Start (or join) the backend connect AFTER the switch:
                    // status messages are per-window, so its "Connecting
                    // to …" lands on the now-active shell rather than on
                    // the workspace we just left.
                    self.bring_dormant_remote_online(id);
                } else {
                    self.set_active_window(id);
                }
            }
            PluginCommand::ActivateWindow { id, request_id } => {
                if self.windows.contains_key(&id) || self.dormant_remote.contains_key(&id) {
                    if self.dormant_remote.contains_key(&id) {
                        self.ensure_dormant_shell(id);
                        self.set_active_window(id);
                        self.bring_dormant_remote_online(id);
                    } else {
                        self.set_active_window(id);
                    }
                }
                self.resolve_bool_for_context(context, request_id, self.active_window == id);
            }
            PluginCommand::SetActiveWindowAnimated { id, from_edge } => {
                // See `SetActiveWindow`: the dive commits into the session's
                // shell while its backend connects.
                if self.dormant_remote.contains_key(&id) {
                    self.ensure_dormant_shell(id);
                    self.set_active_window_animated(id, &from_edge);
                    // See `SetActiveWindow`: connect starts after the
                    // switch so its status lands on the shell.
                    self.bring_dormant_remote_online(id);
                } else {
                    self.set_active_window_animated(id, &from_edge);
                }
            }
            PluginCommand::SetWindowCycleOrder { ids } => {
                self.window_cycle_order = if ids.is_empty() { None } else { Some(ids) };
            }
            PluginCommand::CloseWindow { id } => {
                let _ = self.close_window(id);
            }
            PluginCommand::InspectWorkspacePersistence { root, callback_id } => {
                if !self.context_may_manage_workspace_persistence(context) {
                    self.reject_callback_for_context(
                        context,
                        callback_id,
                        "only the bundled unscoped Orchestrator may inspect workspace persistence"
                            .to_string(),
                    );
                } else {
                    match crate::workspace::inspect_workspace_persistence_in(
                        &self.dir_context,
                        &root,
                    )
                    .and_then(|files| serde_json::to_string(&files).map_err(Into::into))
                    {
                        Ok(json) => self.resolve_json_for_context(context, callback_id, json),
                        Err(error) => self.reject_callback_for_context(
                            context,
                            callback_id,
                            error.to_string(),
                        ),
                    }
                }
            }
            PluginCommand::InspectWorkspaceCreateAttempt {
                attempt_id,
                root_hint,
                workspace_id_hint,
                callback_id,
            } => {
                if !self.context_may_manage_workspace_persistence(context) {
                    self.reject_callback_for_context(
                        context,
                        callback_id,
                        "only the bundled unscoped Orchestrator may inspect workspace create attempts"
                            .to_string(),
                    );
                } else {
                    let inventory = crate::workspace::inspect_workspace_create_attempt_in(
                        &self.dir_context,
                        &attempt_id,
                        root_hint.as_deref(),
                        workspace_id_hint.as_deref(),
                    );
                    match serde_json::to_string(&inventory) {
                        Ok(json) => self.resolve_json_for_context(context, callback_id, json),
                        Err(error) => self.reject_callback_for_context(
                            context,
                            callback_id,
                            error.to_string(),
                        ),
                    }
                }
            }
            PluginCommand::ForgetWorkspacePersistence {
                root,
                stable_id,
                callback_id,
            } => {
                let result = if !self.context_may_manage_workspace_persistence(context) {
                    Err(
                        "only the bundled unscoped Orchestrator may forget workspace persistence"
                            .to_string(),
                    )
                } else if stable_id.as_deref() == Some("") {
                    Err("workspace stable id must not be empty".to_string())
                } else if self.workspace_persistence_is_owned(&root, stable_id.as_deref()) {
                    Err(
                        "workspace persistence is still owned by a live or draining session"
                            .to_string(),
                    )
                } else {
                    crate::workspace::lock_workspace_root(&self.dir_context, &root)
                        .map_err(|error| error.to_string())
                        .and_then(|_root_lock| {
                            if let Some(stable_id) = stable_id.as_deref() {
                                crate::workspace::Workspace::delete_by_id_in(
                                    &self.dir_context,
                                    &root,
                                    stable_id,
                                )
                                .map_err(|error| error.to_string())
                                .and_then(|()| {
                                    crate::workspace::delete_terminal_artifacts_by_id(
                                        &self.dir_context,
                                        &root,
                                        stable_id,
                                    )
                                    .map_err(|error| error.to_string())
                                })
                            } else {
                                crate::workspace::Workspace::delete_in(&self.dir_context, &root)
                                    .map_err(|error| error.to_string())
                                    .and_then(|()| {
                                        crate::workspace::delete_terminal_artifacts_for_root(
                                            &self.dir_context,
                                            &root,
                                        )
                                        .map_err(|error| error.to_string())
                                    })
                            }
                        })
                };
                match result {
                    Ok(()) => {
                        self.resolve_json_for_context(context, callback_id, "null".to_string())
                    }
                    Err(error) => self.reject_callback_for_context(context, callback_id, error),
                }
            }
            PluginCommand::AcquireWorkspaceRootOwnership {
                root,
                owner_id,
                callback_id,
            } => {
                let result = if self.context_may_manage_workspace_persistence(context) {
                    crate::workspace::acquire_workspace_root_ownership(
                        &self.dir_context,
                        &root,
                        &owner_id,
                    )
                    .map_err(|error| error.to_string())
                } else {
                    Err(
                        "only the bundled unscoped Orchestrator may own workspace roots"
                            .to_string(),
                    )
                };
                match result {
                    Ok(()) => {
                        self.resolve_json_for_context(context, callback_id, "null".to_string())
                    }
                    Err(error) => self.reject_callback_for_context(context, callback_id, error),
                }
            }
            PluginCommand::ReleaseWorkspaceRootOwnership {
                owner_id,
                callback_id,
            } => {
                let result = if self.context_may_manage_workspace_persistence(context) {
                    crate::workspace::release_workspace_root_ownership(&owner_id)
                        .map_err(|error| error.to_string())
                } else {
                    Err(
                        "only the bundled unscoped Orchestrator may own workspace roots"
                            .to_string(),
                    )
                };
                match result {
                    Ok(()) => {
                        self.resolve_json_for_context(context, callback_id, "null".to_string())
                    }
                    Err(error) => self.reject_callback_for_context(context, callback_id, error),
                }
            }
            PluginCommand::QuarantineWorkspaceArtifacts {
                root,
                stable_id,
                owner_id,
                callback_id,
            } => {
                let result = if self.context_may_manage_workspace_persistence(context) {
                    crate::workspace::quarantine_workspace_artifacts(
                        &self.dir_context,
                        &root,
                        stable_id.as_deref(),
                        &owner_id,
                    )
                    .map_err(|error| error.to_string())
                } else {
                    Err(
                        "only the bundled unscoped Orchestrator may quarantine workspace artifacts"
                            .to_string(),
                    )
                };
                match result {
                    Ok(()) => {
                        self.resolve_json_for_context(context, callback_id, "null".to_string())
                    }
                    Err(error) => self.reject_callback_for_context(context, callback_id, error),
                }
            }
            PluginCommand::RestoreWorkspaceArtifacts {
                target_root,
                stable_id,
                owner_id,
                callback_id,
            } => {
                let result = if self.context_may_manage_workspace_persistence(context) {
                    crate::workspace::restore_workspace_artifacts(
                        &self.dir_context,
                        &target_root,
                        stable_id.as_deref(),
                        &owner_id,
                    )
                    .map_err(|error| error.to_string())
                } else {
                    Err(
                        "only the bundled unscoped Orchestrator may restore workspace artifacts"
                            .to_string(),
                    )
                };
                match result {
                    Ok(()) => {
                        self.resolve_json_for_context(context, callback_id, "null".to_string())
                    }
                    Err(error) => self.reject_callback_for_context(context, callback_id, error),
                }
            }
            PluginCommand::PurgeWorkspaceArtifactQuarantine {
                owner_id,
                callback_id,
            } => {
                let result = if self.context_may_manage_workspace_persistence(context) {
                    crate::workspace::purge_workspace_artifact_quarantine(
                        &self.dir_context,
                        &owner_id,
                    )
                    .map_err(|error| error.to_string())
                } else {
                    Err(
                        "only the bundled unscoped Orchestrator may purge workspace artifacts"
                            .to_string(),
                    )
                };
                match result {
                    Ok(()) => {
                        self.resolve_json_for_context(context, callback_id, "null".to_string())
                    }
                    Err(error) => self.reject_callback_for_context(context, callback_id, error),
                }
            }
            PluginCommand::PrewarmWindow { id } => {
                self.prewarm_window(id);
            }

            // ==================== File watching ====================
            PluginCommand::WatchPath {
                path,
                recursive,
                request_id,
            } => {
                self.handle_watch_path(path, recursive, request_id, context);
            }
            PluginCommand::UnwatchPath { handle } => {
                self.handle_unwatch_path(handle, context);
            }

            PluginCommand::PreviewWindowInRect { id } => {
                self.handle_preview_window_in_rect(id);
            }

            // ==================== Command/Mode Registration ====================
            PluginCommand::RegisterCommand { command } => {
                self.handle_register_command(command);
            }
            PluginCommand::RegisterStatusBarElement {
                plugin_name,
                token_name,
                title,
            } => {
                self.handle_register_status_bar_element(plugin_name, token_name, title);
            }
            PluginCommand::SetStatusBarValue {
                buffer_id,
                key,
                value,
            } => {
                self.handle_set_status_bar_value(buffer_id, key, value);
            }
            PluginCommand::UnregisterCommand { name } => {
                self.handle_unregister_command(name);
            }
            PluginCommand::DefineMode {
                name,
                bindings,
                read_only,
                allow_text_input,
                inherit_normal_bindings,
                plugin_name,
            } => {
                self.handle_define_mode(
                    name,
                    bindings,
                    read_only,
                    allow_text_input,
                    inherit_normal_bindings,
                    plugin_name,
                );
            }

            // ==================== File/Navigation Commands ====================
            PluginCommand::OpenFileInBackground { path, window_id } => {
                self.handle_open_file_in_background_routed(path, window_id);
            }
            PluginCommand::OpenFileAtLocation { path, line, column } => {
                return self.handle_open_file_at_location(path, line, column);
            }
            PluginCommand::OpenFileInSplit {
                split_id,
                path,
                line,
                column,
            } => {
                return self.handle_open_file_in_split(split_id, path, line, column);
            }
            PluginCommand::ShowBuffer { buffer_id } => {
                self.handle_show_buffer(buffer_id);
            }
            PluginCommand::CloseBuffer { buffer_id, force } => {
                self.handle_close_buffer(buffer_id, force);
            }
            PluginCommand::CloseOtherBuffersInSplit {
                buffer_id,
                split_id,
            } => {
                self.handle_close_other_buffers_in_split(buffer_id, split_id);
            }
            PluginCommand::CloseAllBuffersInSplit { split_id } => {
                self.handle_close_all_buffers_in_split(split_id);
            }
            PluginCommand::CloseBuffersToRightInSplit {
                buffer_id,
                split_id,
            } => {
                self.handle_close_buffers_to_right_in_split(buffer_id, split_id);
            }
            PluginCommand::CloseBuffersToLeftInSplit {
                buffer_id,
                split_id,
            } => {
                self.handle_close_buffers_to_left_in_split(buffer_id, split_id);
            }

            PluginCommand::MoveTabLeft => {
                self.handle_move_tab_left();
            }
            PluginCommand::MoveTabRight => {
                self.handle_move_tab_right();
            }

            // ==================== Animation Commands ====================
            PluginCommand::StartAnimationArea { id, rect, kind } => {
                self.handle_start_animation_area(id, rect, kind);
            }
            PluginCommand::StartAnimationVirtualBuffer {
                id,
                buffer_id,
                kind,
            } => {
                self.handle_start_animation_virtual_buffer(id, buffer_id, kind);
            }
            PluginCommand::CancelAnimation { id } => {
                self.handle_cancel_animation(id);
            }

            // ==================== LSP Commands ====================
            PluginCommand::SendLspRequest {
                language,
                method,
                params,
                request_id,
            } => {
                self.handle_send_lsp_request(language, method, params, request_id);
            }

            // ==================== Clipboard Commands ====================
            PluginCommand::SetClipboard { text } => {
                self.handle_set_clipboard(text);
            }

            // ==================== Async Plugin Commands ====================
            PluginCommand::SpawnProcess {
                window_id,
                command,
                args,
                cwd,
                stdout_to,
                callback_id,
            } => {
                self.handle_spawn_process(window_id, command, args, cwd, stdout_to, callback_id);
            }

            PluginCommand::SpawnHostProcess {
                window_id,
                command,
                args,
                cwd,
                callback_id,
            } => {
                self.handle_spawn_host_process(window_id, command, args, cwd, callback_id);
            }

            PluginCommand::KillHostProcess {
                window_id,
                process_id,
            } => {
                self.handle_kill_host_process(window_id, process_id);
            }

            PluginCommand::SetAuthority { window_id, payload } => {
                self.handle_set_authority(window_id, payload);
            }

            PluginCommand::AttachRemoteAgent {
                window_id,
                payload,
                request_id,
            } => {
                if context
                    .source_window
                    .is_some_and(|source_window| source_window != window_id)
                {
                    self.reject_remote_attach(
                        context.plugin_instance_id,
                        request_id,
                        "remote attach window does not match its command context".to_string(),
                    );
                } else {
                    self.handle_attach_remote_agent(
                        context.plugin_instance_id,
                        context.plugin_name.to_string(),
                        window_id,
                        payload,
                        request_id,
                    );
                }
            }

            PluginCommand::CancelRemoteAttach { request_id } => {
                self.cancel_plugin_remote_attach(context.plugin_instance_id, request_id);
            }

            PluginCommand::CancelRemoteAttaches => {
                self.cancel_plugin_remote_attaches(context.plugin_instance_id);
            }

            PluginCommand::ClearAuthority { window_id } => {
                self.handle_clear_authority(window_id);
            }

            PluginCommand::SetEnv {
                window_id,
                snippet,
                dir,
            } => {
                self.handle_set_env(window_id, snippet, dir);
            }

            PluginCommand::ClearEnv { window_id } => {
                self.handle_clear_env(window_id);
            }

            PluginCommand::SetRemoteIndicatorState { window_id, state } => {
                self.handle_set_remote_indicator_state(window_id, state);
            }

            PluginCommand::ClearRemoteIndicatorState { window_id } => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.remote_indicator_override = None;
                }
            }

            PluginCommand::SpawnProcessWait {
                window_id,
                process_id,
                callback_id,
            } => {
                self.handle_spawn_process_wait(window_id, process_id, callback_id);
            }

            PluginCommand::Delay {
                callback_id,
                duration_ms,
            } => {
                self.handle_delay(callback_id, duration_ms);
            }

            PluginCommand::HttpFetch {
                url,
                target_path,
                callback_id,
            } => {
                self.handle_http_fetch(url, target_path, callback_id);
            }

            PluginCommand::SpawnBackgroundProcess {
                window_id,
                process_id,
                command,
                args,
                cwd,
                callback_id,
            } => {
                self.handle_spawn_background_process(
                    window_id,
                    process_id,
                    command,
                    args,
                    cwd,
                    callback_id,
                );
            }

            PluginCommand::KillBackgroundProcess {
                window_id,
                process_id,
            } => {
                self.handle_kill_background_process(window_id, process_id);
            }

            // ==================== Virtual Buffer Commands (complex, kept inline) ====================
            PluginCommand::CreateVirtualBuffer {
                name,
                mode,
                read_only,
            } => {
                self.handle_create_virtual_buffer(name, mode, read_only);
            }
            PluginCommand::CreateVirtualBufferWithContent {
                name,
                mode,
                read_only,
                entries,
                show_line_numbers,
                show_cursors,
                editing_disabled,
                hidden_from_tabs,
                initial_cursor_line,
                indentation_guide,
                request_id,
            } => {
                self.handle_create_virtual_buffer_with_content(
                    name,
                    mode,
                    read_only,
                    entries,
                    show_line_numbers,
                    show_cursors,
                    editing_disabled,
                    hidden_from_tabs,
                    initial_cursor_line,
                    indentation_guide,
                    request_id,
                );
            }
            PluginCommand::CreateVirtualBufferInSplit {
                name,
                mode,
                read_only,
                entries,
                ratio,
                direction,
                panel_id,
                show_line_numbers,
                show_cursors,
                editing_disabled,
                line_wrap,
                before,
                role,
                scrollable,
                request_id,
            } => {
                self.handle_create_virtual_buffer_in_split(
                    name,
                    mode,
                    read_only,
                    entries,
                    ratio,
                    direction,
                    panel_id,
                    show_line_numbers,
                    show_cursors,
                    editing_disabled,
                    line_wrap,
                    before,
                    role,
                    scrollable,
                    request_id,
                );
            }
            PluginCommand::SetVirtualBufferContent { buffer_id, entries } => {
                self.handle_set_virtual_buffer_content(buffer_id, entries);
            }
            PluginCommand::GetTextPropertiesAtCursor { buffer_id } => {
                self.handle_get_text_properties_at_cursor(buffer_id);
            }
            PluginCommand::CreateVirtualBufferInExistingSplit {
                name,
                mode,
                read_only,
                entries,
                split_id,
                show_line_numbers,
                show_cursors,
                editing_disabled,
                line_wrap,
                initial_cursor_line,
                request_id,
            } => {
                self.handle_create_virtual_buffer_in_existing_split(
                    name,
                    mode,
                    read_only,
                    entries,
                    split_id,
                    show_line_numbers,
                    show_cursors,
                    editing_disabled,
                    line_wrap,
                    initial_cursor_line,
                    request_id,
                );
            }

            // ==================== Context Commands ====================
            PluginCommand::SetContext { name, active } => {
                self.handle_set_context(name, active);
            }

            // ==================== Review Diff Commands ====================
            PluginCommand::SetReviewDiffHunks { hunks } => {
                self.handle_set_review_diff_hunks(hunks);
            }

            // ==================== Vi Mode Commands ====================
            PluginCommand::ExecuteAction { action_name } => {
                self.handle_execute_action(action_name);
            }
            PluginCommand::CompleteCommand { request_id, .. } => {
                tracing::warn!(
                    request_id,
                    plugin = %context.plugin_name,
                    "rejected completion outside the envelope dispatch boundary"
                );
            }
            PluginCommand::ExecuteActions { actions } => {
                self.handle_execute_actions(actions);
            }
            PluginCommand::DefineMacro { register, steps } => {
                self.handle_define_macro(register, steps);
            }
            PluginCommand::PlayMacroByRegister { register } => {
                if let Some(key) = register.chars().next() {
                    self.play_macro(key);
                }
            }
            PluginCommand::GetBufferText {
                buffer_id,
                start,
                end,
                request_id,
            } => {
                self.handle_get_buffer_text(buffer_id, start, end, request_id);
            }
            PluginCommand::GetLineStartPosition {
                buffer_id,
                line,
                request_id,
            } => {
                self.handle_get_line_start_position(buffer_id, line, request_id);
            }
            PluginCommand::GetLineEndPosition {
                buffer_id,
                line,
                request_id,
            } => {
                self.handle_get_line_end_position(buffer_id, line, request_id);
            }
            PluginCommand::GetBufferLineCount {
                buffer_id,
                request_id,
            } => {
                self.handle_get_buffer_line_count(buffer_id, request_id);
            }
            PluginCommand::GetCompositeCursorInfo { request_id } => {
                self.handle_get_composite_cursor_info(request_id);
            }
            PluginCommand::OpenFileStreaming { path, request_id } => {
                self.handle_open_file_streaming(path, request_id);
            }
            PluginCommand::RefreshBufferFromDisk {
                buffer_id,
                request_id,
            } => {
                self.handle_refresh_buffer_from_disk(buffer_id, request_id);
            }
            PluginCommand::SetBufferGroupPanelBuffer {
                group_id,
                panel_name,
                buffer_id,
                request_id,
            } => {
                self.handle_set_buffer_group_panel_buffer(
                    group_id, panel_name, buffer_id, request_id,
                );
            }
            PluginCommand::ScrollToLineCenter {
                split_id,
                buffer_id,
                line,
            } => {
                self.handle_scroll_to_line_center(split_id, buffer_id, line);
            }
            PluginCommand::ScrollBufferToLine { buffer_id, line } => {
                self.handle_scroll_buffer_to_line(buffer_id, line);
            }
            PluginCommand::SetEditorMode { mode } => {
                self.handle_set_editor_mode(mode);
            }

            // ==================== LSP Helper Commands ====================
            PluginCommand::ShowActionPopup {
                popup_id,
                title,
                message,
                actions,
                buffer_id,
            } => {
                self.handle_show_action_popup(popup_id, title, message, actions, buffer_id);
            }

            PluginCommand::SetLspMenuContributions {
                plugin_id,
                language,
                items,
            } => {
                self.handle_set_lsp_menu_contributions(plugin_id, language, items);
            }

            PluginCommand::DisableLspForLanguage { language } => {
                self.handle_disable_lsp_for_language(language);
            }

            PluginCommand::RestartLspForLanguage { language } => {
                self.handle_restart_lsp_for_language(language);
            }

            PluginCommand::RegisterLspUriScheme { scheme } => {
                tracing::debug!("Plugin registered LSP URI scheme: {}", scheme);
                self.lsp_uri_schemes.insert(scheme);
            }

            PluginCommand::MarkBufferReadOnly { path } => {
                self.handle_mark_buffer_read_only(path);
            }

            PluginCommand::SetLspRootUri { language, uri } => {
                self.handle_set_lsp_root_uri(language, uri);
            }

            // ==================== Scroll Sync Commands ====================
            PluginCommand::CreateScrollSyncGroup {
                group_id,
                left_split,
                right_split,
            } => {
                self.handle_create_scroll_sync_group(group_id, left_split, right_split);
            }
            PluginCommand::SetScrollSyncAnchors { group_id, anchors } => {
                self.handle_set_scroll_sync_anchors(group_id, anchors);
            }
            PluginCommand::RemoveScrollSyncGroup { group_id } => {
                self.handle_remove_scroll_sync_group(group_id);
            }

            // ==================== Composite Buffer Commands ====================
            PluginCommand::CreateCompositeBuffer {
                name,
                mode,
                layout,
                sources,
                hunks,
                initial_focus_hunk,
                request_id,
            } => {
                self.handle_create_composite_buffer(
                    crate::app::composite_buffer_actions::CreateCompositeBufferArgs {
                        name,
                        mode,
                        layout_config: layout,
                        source_configs: sources,
                        hunks,
                        initial_focus_hunk,
                        request_id,
                    },
                );
            }
            PluginCommand::UpdateCompositeAlignment { buffer_id, hunks } => {
                self.handle_update_composite_alignment(buffer_id, hunks);
            }
            PluginCommand::CloseCompositeBuffer { buffer_id } => {
                self.active_window_mut().close_composite_buffer(buffer_id);
            }
            PluginCommand::FlushLayout => {
                self.flush_layout();
            }
            PluginCommand::SetLineTargets { buffer_id, targets } => {
                self.set_line_targets(buffer_id, targets);
            }
            PluginCommand::SplitWindow {
                options,
                request_id,
            } => {
                self.handle_split_window(options, request_id);
            }
            PluginCommand::SyncSnapshot { request_id } => {
                // Everything queued ahead of this has been applied by now
                // (the queue is FIFO), so refreshing the snapshot here is
                // what makes the caller's next read observe its own writes.
                self.update_plugin_state_snapshot();
                self.send_plugin_response(fresh_core::api::PluginResponse::SnapshotSynced {
                    request_id,
                });
            }
            PluginCommand::CompositeNextHunk { buffer_id } => {
                self.handle_composite_next_hunk(buffer_id);
            }
            PluginCommand::CompositePrevHunk { buffer_id } => {
                self.handle_composite_prev_hunk(buffer_id);
            }

            // ==================== Buffer Groups ====================
            PluginCommand::CreateBufferGroup {
                name,
                mode,
                layout_json,
                request_id,
            } => {
                self.handle_create_buffer_group(name, mode, layout_json, request_id);
            }
            PluginCommand::SetPanelContent {
                group_id,
                panel_name,
                entries,
            } => {
                self.set_panel_content(group_id, panel_name, entries);
            }
            PluginCommand::CloseBufferGroup { group_id } => {
                self.close_buffer_group(group_id);
            }
            PluginCommand::FocusPanel {
                group_id,
                panel_name,
            } => {
                self.focus_panel(group_id, panel_name);
            }

            // ==================== File Operations ====================
            PluginCommand::SaveBufferToPath { buffer_id, path } => {
                self.handle_save_buffer_to_path(buffer_id, path);
            }

            // ==================== Plugin Management ====================
            #[cfg(feature = "plugins")]
            PluginCommand::LoadPlugin { path, callback_id } => {
                self.handle_load_plugin(path, callback_id);
            }
            #[cfg(feature = "plugins")]
            PluginCommand::UnloadPlugin { name, callback_id } => {
                self.handle_unload_plugin(name, callback_id, context);
            }
            #[cfg(feature = "plugins")]
            PluginCommand::ReloadPlugin { name, callback_id } => {
                self.handle_reload_plugin(name, callback_id, context);
            }
            #[cfg(feature = "plugins")]
            PluginCommand::ListPlugins { callback_id } => {
                self.handle_list_plugins(callback_id);
            }
            PluginCommand::SetPluginTimer {
                timer_id,
                plugin_name,
                handler_name,
                interval_ms,
                repeat,
            } => {
                self.handle_set_plugin_timer(
                    timer_id,
                    plugin_name,
                    handler_name,
                    interval_ms,
                    repeat,
                );
            }

            PluginCommand::ClearPluginTimer { timer_id } => {
                self.handle_clear_plugin_timer(timer_id);
            }

            #[cfg(feature = "plugins")]
            PluginCommand::ReloadInit { callback_id } => {
                self.handle_reload_init(callback_id);
            }
            #[cfg(feature = "plugins")]
            PluginCommand::RunEditorCommand { name, callback_id } => {
                self.handle_run_editor_command(name, callback_id);
            }
            #[cfg(feature = "plugins")]
            PluginCommand::ListEditorCommands { callback_id } => {
                self.handle_list_editor_commands(callback_id);
            }
            // When plugins feature is disabled, these commands are no-ops
            #[cfg(not(feature = "plugins"))]
            PluginCommand::LoadPlugin { .. }
            | PluginCommand::UnloadPlugin { .. }
            | PluginCommand::ReloadPlugin { .. }
            | PluginCommand::ListPlugins { .. }
            | PluginCommand::ReloadInit { .. }
            | PluginCommand::RunEditorCommand { .. }
            | PluginCommand::ListEditorCommands { .. } => {
                tracing::warn!("Plugin management commands require the 'plugins' feature");
            }

            // ==================== Terminal Commands ====================
            PluginCommand::CreateTerminal {
                cwd,
                direction,
                ratio,
                focus,
                persistent,
                window_id,
                command,
                relaunch,
                title,
                resume,
                env,
                companion,
                allow_script,
                selected_agent,
                request_id,
            } => {
                self.handle_create_terminal(
                    cwd,
                    direction,
                    ratio,
                    focus,
                    persistent,
                    window_id,
                    command,
                    relaunch,
                    title,
                    resume,
                    env,
                    companion,
                    allow_script,
                    selected_agent,
                    request_id,
                    context,
                );
            }

            PluginCommand::SendTerminalInput { terminal_id, data } => {
                self.handle_send_terminal_input(terminal_id, data);
            }

            PluginCommand::CloseTerminal { terminal_id } => {
                self.handle_close_terminal(terminal_id);
            }

            PluginCommand::SignalWindow { id, signal } => {
                self.handle_signal_window(id, &signal);
            }
            PluginCommand::StopWindow { id, grace_ms } => {
                self.handle_stop_window(id, grace_ms);
            }

            PluginCommand::RegisterDiffBaseline {
                buffer_id,
                kind,
                git_ref,
                callback_id,
            } => {
                self.handle_register_diff_baseline(buffer_id, kind, git_ref, callback_id);
            }

            PluginCommand::DiffAgainstBaseline {
                buffer_id,
                baseline_id,
                callback_id,
            } => {
                self.handle_diff_against_baseline(buffer_id, baseline_id, callback_id);
            }

            PluginCommand::DiffBaselinePair {
                old_baseline_id,
                new_baseline_id,
                callback_id,
            } => {
                self.handle_diff_baseline_pair(old_baseline_id, new_baseline_id, callback_id);
            }

            PluginCommand::GetBaselineLines {
                baseline_id,
                ranges,
                callback_id,
            } => {
                self.handle_get_baseline_lines(baseline_id, ranges, callback_id);
            }

            PluginCommand::RefreshDiffBaseline {
                baseline_id,
                callback_id,
            } => {
                self.handle_refresh_diff_baseline(baseline_id, callback_id);
            }

            PluginCommand::ReleaseDiffBaseline { baseline_id } => {
                self.handle_release_diff_baseline(baseline_id);
            }

            PluginCommand::GrepProject {
                plugin_name,
                pattern,
                fixed_string,
                case_sensitive,
                max_results,
                whole_words,
                callback_id,
            } => {
                self.handle_grep_project(
                    plugin_name,
                    pattern,
                    fixed_string,
                    case_sensitive,
                    max_results,
                    whole_words,
                    callback_id,
                );
            }

            PluginCommand::BeginSearch {
                pattern,
                fixed_string,
                case_sensitive,
                max_results,
                whole_words,
                file_glob,
                source_buffer_id,
                handle_id,
            } => {
                self.handle_begin_search(crate::app::plugin_commands::BeginSearchArgs {
                    pattern,
                    fixed_string,
                    case_sensitive,
                    max_results,
                    whole_words,
                    file_glob,
                    source_buffer_id,
                    handle_id,
                });
            }

            PluginCommand::ReplaceInBuffer {
                file_path,
                buffer_id,
                matches,
                replacement,
                callback_id,
            } => {
                self.handle_replace_in_buffer(
                    file_path,
                    buffer_id,
                    matches,
                    replacement,
                    callback_id,
                );
            }

            PluginCommand::MountWidgetPanel {
                plugin,
                panel_id,
                buffer_id,
                spec,
            } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_mount_widget_panel(key, buffer_id, spec);
            }

            PluginCommand::UpdateWidgetPanel {
                plugin,
                panel_id,
                spec,
            } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_update_widget_panel(&key, spec);
            }

            PluginCommand::UnmountWidgetPanel { plugin, panel_id } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_unmount_widget_panel(&key);
            }

            PluginCommand::WidgetCommand {
                plugin,
                panel_id,
                action,
            } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_widget_command(&key, action);
            }

            PluginCommand::WidgetMutate {
                plugin,
                panel_id,
                mutation,
            } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_widget_mutate(&key, mutation);
            }

            PluginCommand::MountFloatingWidget {
                plugin,
                panel_id,
                spec,
                width_pct,
                height_pct,
                as_dock,
                focus_marker,
                title,
                closable,
                start_blurred,
            } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_mount_floating_widget(
                    key,
                    spec,
                    width_pct,
                    height_pct,
                    as_dock,
                    focus_marker,
                    title,
                    closable,
                    start_blurred,
                );
            }

            PluginCommand::UpdateFloatingWidget {
                plugin,
                panel_id,
                spec,
            } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_update_floating_widget(&key, spec);
            }

            PluginCommand::UnmountFloatingWidget { plugin, panel_id } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_unmount_floating_widget(&key);
            }

            PluginCommand::FloatingPanelControl {
                plugin,
                panel_id,
                op,
                arg,
            } => {
                let key = crate::widgets::PanelKey::new(plugin, panel_id);
                self.handle_floating_panel_control(&key, &op, arg);
            }
        }
        Ok(())
    }

    // ── Delegated handlers extracted from the dispatch match ─────────────

    fn watch_owner_for_context(
        &self,
        context: &PluginCommandContext,
    ) -> Option<crate::services::file_watcher::WatchOwner> {
        let window_id = context
            .window_scope
            .or(context.source_window)
            .unwrap_or(self.active_window);
        let authority = self.windows.get(&window_id)?.authority().stamp();
        let plugin_instance_id = (context.provenance
            != fresh_core::api::PluginLoadProvenance::Internal)
            .then_some(context.plugin_instance_id);
        Some(crate::services::file_watcher::WatchOwner {
            window_id,
            authority,
            plugin_instance_id,
        })
    }

    fn handle_watch_path(
        &mut self,
        path: std::path::PathBuf,
        recursive: bool,
        request_id: u64,
        context: &PluginCommandContext,
    ) {
        let result = match self.watch_owner_for_context(context) {
            Some(owner)
                if self.windows.get(&owner.window_id).is_some_and(|window| {
                    matches!(
                        &window.authority_spec,
                        crate::services::authority::SessionAuthoritySpec::RemoteAgent(_)
                    )
                }) => Err(
                    "watchPath is unavailable for remote authorities; refusing to reinterpret a remote path on the host"
                        .to_string(),
                ),
            Some(owner) => {
                if let Some(ref bridge) = self.async_bridge {
                    self.file_watcher_manager
                        .watch(bridge, &path, recursive, owner)
                } else {
                    Err(
                        "watchPath: no async bridge — file watching is unavailable in this build"
                            .to_string(),
                    )
                }
            }
            None => Err("watchPath source window is closed".to_string()),
        };
        self.last_watch_response_for_test = Some((request_id, result.clone()));
        self.send_plugin_response(fresh_core::api::PluginResponse::WatchPathRegistered {
            request_id,
            result,
        });
    }

    fn handle_unwatch_path(&mut self, handle: u64, context: &PluginCommandContext) {
        let Some(owner) = self.watch_owner_for_context(context) else {
            return;
        };
        if !self.file_watcher_manager.unwatch_owned(handle, owner) {
            tracing::warn!(
                handle,
                ?owner.window_id,
                "denied unwatchPath outside its immutable plugin/window authority"
            );
        }
    }

    fn handle_set_env(
        &mut self,
        window_id: fresh_core::WindowId,
        snippet: String,
        dir: Option<String>,
    ) {
        use crate::services::workspace_trust::TrustLevel;
        let Some(window) = self.windows.get_mut(&window_id) else {
            tracing::warn!(?window_id, "SetEnv targeted a closed window");
            return;
        };
        if window.authority().workspace_trust.level() == TrustLevel::Trusted {
            window
                .authority()
                .env_provider
                .set(snippet, dir.map(std::path::PathBuf::from));
            self.refresh_window_lsp_for_env(window_id);
        } else {
            window.status_message =
                Some("Workspace not trusted — cannot activate environment".to_string());
        }
    }

    fn handle_clear_env(&mut self, window_id: fresh_core::WindowId) {
        let Some(window) = self.windows.get(&window_id) else {
            tracing::warn!(?window_id, "ClearEnv targeted a closed window");
            return;
        };
        let was_active = window.authority().env_provider.is_active();
        window.authority().env_provider.clear();
        if was_active {
            self.refresh_window_lsp_for_env(window_id);
        }
    }

    /// Re-launch one exact window's already-running language servers so they
    /// pick up its changed environment without rebuilding any sibling window.
    fn refresh_window_lsp_for_env(&mut self, window_id: fresh_core::WindowId) {
        let running: Vec<String> = self
            .windows
            .get(&window_id)
            .map(|w| w.lsp.running_servers())
            .unwrap_or_default();
        if running.is_empty() {
            return;
        }
        let file_path = self.windows.get(&window_id).and_then(|window| {
            window
                .buffer_metadata
                .get(&window.active_buffer())
                .and_then(|meta| meta.file_path().cloned())
        });
        for language in &running {
            if let Some(window) = self.windows.get_mut(&window_id) {
                let _ = window.lsp.manual_restart(language, file_path.as_deref());
            }
            self.reopen_buffers_for_language_in_window(window_id, language);
        }
    }

    fn handle_open_file_in_background_routed(
        &mut self,
        path: std::path::PathBuf,
        window_id: Option<fresh_core::WindowId>,
    ) {
        let route_to_inactive =
            window_id.filter(|&id| id != self.active_window && self.windows.contains_key(&id));
        if let Some(target) = route_to_inactive {
            self.handle_open_file_in_inactive_session(target, path);
        } else {
            self.handle_open_file_in_background(path);
        }
    }

    // ── Handlers extracted from the dispatch match ───────────────────────

    fn handle_set_split_label(&mut self, split_id: SplitId, label: String) {
        self.windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_manager_mut())
            .expect("active window must have a populated split layout")
            .set_label(LeafId(split_id), label);
    }

    fn handle_clear_split_label(&mut self, split_id: SplitId) {
        self.windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_manager_mut())
            .expect("active window must have a populated split layout")
            .clear_label(split_id);
    }

    fn handle_reload_themes(&mut self, apply_theme: Option<String>) {
        self.reload_themes();
        if let Some(theme_name) = apply_theme {
            self.apply_theme(&theme_name);
        }
    }

    fn handle_set_key_capture_active(&mut self, active: bool) {
        self.active_window_mut().key_capture_active = active;
        if !active {
            // Capture window closed; any leftover queued keys were intended
            // for the plugin and should not leak into normal dispatch.
            self.active_window_mut().pending_key_capture_buffer.clear();
        }
    }

    fn handle_set_prompt_input_sync(&mut self, sync: bool) {
        if let Some(prompt) = &mut self.active_window_mut().prompt {
            prompt.sync_input_on_navigate = sync;
        }
    }

    fn handle_set_prompt_title(&mut self, title: Vec<fresh_core::api::StyledText>) {
        if let Some(prompt) = &mut self.active_window_mut().prompt {
            prompt.title = title;
        }
    }

    fn handle_set_prompt_footer(&mut self, footer: Vec<fresh_core::api::StyledText>) {
        if let Some(prompt) = &mut self.active_window_mut().prompt {
            prompt.footer = footer;
        }
    }

    fn handle_set_prompt_toolbar(&mut self, spec: Option<fresh_core::api::WidgetSpec>) {
        if let Some(prompt) = &mut self.active_window_mut().prompt {
            prompt.toolbar_widget = spec;
        }
    }

    fn handle_set_prompt_status(&mut self, status: String) {
        if let Some(prompt) = &mut self.active_window_mut().prompt {
            prompt.status = status;
        }
    }

    fn handle_set_prompt_selected_index(&mut self, index: u32) {
        if let Some(prompt) = &mut self.active_window_mut().prompt {
            let len = prompt.suggestions.len();
            if len > 0 {
                prompt.selected_suggestion = Some((index as usize).min(len - 1));
            }
        }
    }

    fn handle_create_window(&mut self, root: std::path::PathBuf, label: String) {
        if !root.is_absolute() {
            tracing::warn!(
                "CreateWindow rejected: root must be absolute, got {:?}",
                root
            );
        } else {
            let _ = self.create_window_at(root, label);
        }
    }

    fn handle_restore_workspace_window(
        &mut self,
        root: std::path::PathBuf,
        label: String,
        stable_id: Option<String>,
        activate: bool,
        callback_id: JsCallbackId,
        context: &PluginCommandContext,
    ) {
        if !self.context_may_manage_workspace_persistence(context) {
            self.reject_callback_for_context(
                context,
                callback_id,
                "only the bundled unscoped Orchestrator may restore workspace windows".into(),
            );
            return;
        }
        if !root.is_absolute() || stable_id.as_deref() == Some("") {
            self.reject_callback_for_context(
                context,
                callback_id,
                "workspace root must be absolute and stable id must be non-empty when present"
                    .into(),
            );
            return;
        }

        let requested_id = stable_id.as_deref().unwrap_or("");
        let root_key = crate::app::orchestrator_persistence::canonical_key(&root);
        if let Some(existing) = self.windows.iter().find_map(|(id, window)| {
            (crate::app::orchestrator_persistence::canonical_key(&window.root) == root_key
                && window.stable_id == requested_id)
                .then_some(*id)
        }) {
            if activate {
                self.set_active_window(existing);
            }
            let stable_id = self
                .windows
                .get(&existing)
                .map(|window| window.stable_id.clone())
                .unwrap_or_default();
            self.resolve_json_for_context(
                context,
                callback_id,
                serde_json::json!({ "windowId": existing.0, "stableId": stable_id }).to_string(),
            );
            return;
        }

        let persisted = if let Some(stable_id) = stable_id.as_deref() {
            crate::workspace::Workspace::load_by_id_in(&self.dir_context, &root, stable_id)
        } else {
            crate::workspace::Workspace::load_in(&self.dir_context, &root)
        };
        match persisted {
            Ok(Some(_)) => {}
            Ok(None) => {
                self.reject_callback_for_context(
                    context,
                    callback_id,
                    "the exact workspace persistence is missing".into(),
                );
                return;
            }
            Err(error) => {
                self.reject_callback_for_context(context, callback_id, error.to_string());
                return;
            }
        }

        let authority = self.local_session_authority(&root);
        let id = self.create_window_with_authority_and_stable_id(
            root,
            label,
            authority,
            Some(stable_id.unwrap_or_default()),
        );
        let restored = self.restore_window(id, false);
        if !matches!(&restored, Ok(true)) {
            self.handle_signal_window(id, "SIGKILL");
            self.windows.remove(&id);
            self.plugin_manager
                .read()
                .unwrap()
                .run_hook("window_closed", HookArgs::WindowClosed { id: id.0 });
            let error = match restored {
                Ok(false) => "the exact workspace persistence could not be restored".into(),
                Err(error) => error.to_string(),
                Ok(true) => unreachable!(),
            };
            self.reject_callback_for_context(context, callback_id, error);
            return;
        }
        if activate {
            self.set_active_window(id);
        }
        let stable_id = self
            .windows
            .get(&id)
            .map(|window| window.stable_id.clone())
            .unwrap_or_default();
        self.resolve_json_for_context(
            context,
            callback_id,
            serde_json::json!({ "windowId": id.0, "stableId": stable_id }).to_string(),
        );
    }

    fn handle_preview_window_in_rect(&mut self, id: Option<fresh_core::WindowId>) {
        // Only honour if the session exists and is not the active one
        // (no point previewing the session whose UI is already on screen).
        self.preview_window_id = match id {
            Some(sid) if sid != self.active_window && self.windows.contains_key(&sid) => Some(sid),
            _ => None,
        };
    }

    fn handle_register_status_bar_element(
        &mut self,
        plugin_name: String,
        token_name: String,
        title: String,
    ) {
        if let Err(e) = self.register_status_bar_element(&plugin_name, &token_name, &title) {
            tracing::warn!("Failed to register statusbar element: {}", e);
        }
    }

    fn handle_set_status_bar_value(&mut self, buffer_id: u64, key: String, value: String) {
        if let Err(e) =
            self.set_status_bar_value(fresh_core::BufferId(buffer_id as usize), &key, value)
        {
            // Plugins compute asynchronously off a lagging state snapshot, so
            // the target buffer may have closed — an expected, benign race.
            tracing::debug!("Skipped statusbar value for stale buffer: {}", e);
        }
    }

    fn handle_cancel_animation(&mut self, id: u64) {
        self.active_window_mut()
            .animations
            .cancel(crate::view::animation::AnimationId::from_raw(id));
    }

    fn handle_clear_authority(&mut self, window_id: fresh_core::WindowId) {
        let Some(root) = self
            .windows
            .get(&window_id)
            .map(|window| window.root.clone())
        else {
            tracing::warn!(?window_id, "ClearAuthority targeted a closed window");
            return;
        };
        tracing::info!(?window_id, "Plugin cleared authority; restarting locally");
        let authority = self.local_session_authority(&root);
        self.session_keepalives.remove(&window_id);
        self.set_session_authority_spec(
            window_id,
            crate::services::authority::SessionAuthoritySpec::Local,
        );
        // A cross-backend pointer swap cannot synchronously fence every live
        // PTY, LSP, plugin task, and process. Make the exact target active only
        // for restart routing, then let the established full-editor cutover
        // drop the old runtime before publishing the local authority.
        self.switch_active_window_pointer(window_id);
        self.install_authority(authority);
    }

    fn handle_set_review_diff_hunks(&mut self, hunks: Vec<fresh_core::api::ReviewHunk>) {
        self.active_window_mut().review_hunks = hunks;
        tracing::debug!(
            "Set {} review hunks",
            self.active_window_mut().review_hunks.len()
        );
    }

    fn handle_composite_next_hunk(&mut self, buffer_id: fresh_core::BufferId) {
        // Inner group leaf: the Review Diff composite lives in the focused
        // group leaf, not the outer active split (see `handle_composite_action`).
        let split_id = self.active_window().effective_active_pair().0;
        self.active_window_mut()
            .composite_next_hunk(split_id, buffer_id);
    }

    fn handle_composite_prev_hunk(&mut self, buffer_id: fresh_core::BufferId) {
        let split_id = self.active_window().effective_active_pair().0;
        self.active_window_mut()
            .composite_prev_hunk(split_id, buffer_id);
    }

    // ── Virtual-buffer display configuration ────────────────────────────

    /// Apply the three display flags (line numbers, cursor visibility,
    /// editing lock) that every `create_virtual_buffer_*` command sets
    /// on the newly-created buffer's state.
    fn configure_vbuf_display(
        &mut self,
        buffer_id: crate::model::event::BufferId,
        show_line_numbers: bool,
        show_cursors: bool,
        editing_disabled: bool,
        indentation_guide: Option<bool>,
        scrollable: bool,
    ) {
        if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&buffer_id)
        {
            state.margins.configure_for_line_numbers(show_line_numbers);
            state.show_cursors = show_cursors;
            state.editing_disabled = editing_disabled;
            // `None` leaves the default (virtual buffers get no guides); a
            // plugin that shows real source in a virtual buffer (e.g. git log's
            // file-at-commit view) passes `Some(true)` to keep them.
            if let Some(enabled) = indentation_guide {
                state.indentation_guide_override = Some(enabled);
            }
            // Self-managing widget panels pass `scrollable=false` so no
            // buffer scrollbar is drawn and the viewport can't be dragged
            // off the panel's own content (issue #2434 follow-up).
            state.scrollable = scrollable;
        }
    }

    // ── Virtual-buffer-in-split sub-paths ───────────────────────────────

    /// Utility-dock fast path: a leaf with `dock_leaf`'s role already exists,
    /// so attach the new virtual buffer there instead of spawning a new split.
    #[allow(clippy::too_many_arguments)]
    fn route_vbuf_to_existing_dock(
        &mut self,
        dock_leaf: crate::model::event::LeafId,
        name: String,
        mode: String,
        read_only: bool,
        entries: Vec<fresh_core::text_property::TextPropertyEntry>,
        panel_id: Option<&str>,
        show_line_numbers: bool,
        show_cursors: bool,
        editing_disabled: bool,
        scrollable: bool,
        request_id: Option<u64>,
    ) {
        // Capture the source split *before* create_virtual_buffer tabs the
        // new buffer into it; we drop that phantom tab after the dock attach.
        let source_split_before_create = self.split_manager().active_split();
        let buffer_id =
            self.active_window_mut()
                .create_virtual_buffer(name.clone(), mode, read_only);
        self.configure_vbuf_display(
            buffer_id,
            show_line_numbers,
            show_cursors,
            editing_disabled,
            None,
            scrollable,
        );
        if let Some(pid) = panel_id {
            self.panel_ids_mut().insert(pid.to_string(), buffer_id);
        }
        if let Err(e) = self.set_virtual_buffer_content(buffer_id, entries) {
            tracing::error!("Failed to set virtual buffer content (dock route): {}", e);
            return;
        }
        // Swap the dock leaf's active buffer to the new one and add it as a tab.
        self.split_manager_mut().set_active_split(dock_leaf);
        self.active_window_mut()
            .set_pane_buffer(dock_leaf, buffer_id);
        // `show_line_numbers` is per (split, buffer), and the dock leaf's view
        // state already exists — it was built for whichever panel opened the
        // dock — so it defaults this buffer to "on". Fresh-split creation sets
        // it explicitly; this path did not, which is why the *second* panel
        // routed into the dock (a second tour, diagnostics beside
        // search/replace) rendered with a line-number gutter the first one
        // lacked. The gutter also stole the columns the panel had already laid
        // its widgets out for, so the content wrapped.
        if let Some(view_state) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_view_states_mut())
            .expect("active window must have a populated split layout")
            .get_mut(&dock_leaf)
        {
            view_state.ensure_buffer_state(buffer_id).show_line_numbers = show_line_numbers;
        }
        // Drop the phantom tab from the source split.
        if dock_leaf != source_split_before_create {
            if let Some(source_view_state) = self
                .windows
                .get_mut(&self.active_window)
                .and_then(|w| w.split_view_states_mut())
                .expect("active window must have a populated split layout")
                .get_mut(&source_split_before_create)
            {
                source_view_state.remove_buffer(buffer_id);
            }
        }
        if let Some(req_id) = request_id {
            let result = fresh_core::api::VirtualBufferResult {
                buffer_id: buffer_id.0 as u64,
                split_id: Some(dock_leaf.0 .0 as u64),
            };
            self.plugin_manager.read().unwrap().resolve_callback(
                fresh_core::api::JsCallbackId::from(req_id),
                serde_json::to_string(&result).unwrap_or_default(),
            );
        }
        tracing::info!(
            "Routed virtual buffer '{}' into existing utility dock {:?}",
            name,
            dock_leaf
        );
    }

    /// Idempotent panel update: `panel_name` already maps to a live buffer,
    /// so just refresh its content and focus the split it lives in.
    fn update_existing_vbuf_panel(
        &mut self,
        existing_buffer_id: crate::model::event::BufferId,
        entries: Vec<fresh_core::text_property::TextPropertyEntry>,
        request_id: Option<u64>,
        panel_name: &str,
    ) {
        match self.set_virtual_buffer_content(existing_buffer_id, entries) {
            Ok(()) => tracing::info!("Updated existing panel '{}' content", panel_name),
            Err(e) => tracing::error!("Failed to update panel content: {}", e),
        }
        let splits = self.split_manager().splits_for_buffer(existing_buffer_id);
        if let Some(&split_id) = splits.first() {
            self.split_manager_mut().set_active_split(split_id);
            // Route through set_pane_buffer so tree + SVS stay consistent.
            self.active_window_mut()
                .set_pane_buffer(split_id, existing_buffer_id);
            tracing::debug!("Focused split {:?} containing panel buffer", split_id);
        }
        if let Some(req_id) = request_id {
            let result = fresh_core::api::VirtualBufferResult {
                buffer_id: existing_buffer_id.0 as u64,
                split_id: splits.first().map(|s| s.0 .0 as u64),
            };
            self.plugin_manager.read().unwrap().resolve_callback(
                fresh_core::api::JsCallbackId::from(req_id),
                serde_json::to_string(&result).unwrap_or_default(),
            );
        }
    }

    // ── Line-position shared implementation ─────────────────────────────

    /// Shared implementation for `handle_get_line_start_position` and
    /// `handle_get_line_end_position`. When `want_end` is false the byte
    /// offset of the line's first character is returned; when true, the
    /// byte offset of its terminating newline (or `buffer_len` for the
    /// last line without a trailing newline).
    fn handle_get_line_position(
        &mut self,
        buffer_id: crate::model::event::BufferId,
        line: u32,
        request_id: u64,
        want_end: bool,
    ) {
        let actual_buffer_id = self.resolve_buffer_id(buffer_id);
        let result = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&actual_buffer_id)
            .and_then(|state| {
                let len = state.buffer.len();
                let content = state.get_text_range(0, len);
                buffer_line_byte_offset(&content, len, line as usize, want_end)
            });
        self.resolve_json_callback(request_id, result);
    }

    /// Save a buffer to a specific file path (for :w filename)
    fn handle_save_buffer_to_path(&mut self, buffer_id: BufferId, path: std::path::PathBuf) {
        if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&buffer_id)
        {
            // Save to the specified path
            match state.buffer.save_to_file(&path) {
                Ok(()) => {
                    // save_to_file already updates file_path internally via finalize_save
                    // Run on-save actions (formatting, etc.)
                    if let Err(e) = self.finalize_save(Some(path)) {
                        tracing::warn!("Failed to finalize save: {}", e);
                    }
                    tracing::debug!("Saved buffer {:?} to path", buffer_id);
                }
                Err(e) => {
                    self.handle_set_status(format!("Error saving: {}", e));
                    tracing::error!("Failed to save buffer to path: {}", e);
                }
            }
        } else {
            self.handle_set_status(format!("Buffer {:?} not found", buffer_id));
            tracing::warn!("SaveBufferToPath: buffer {:?} not found", buffer_id);
        }
    }

    /// Load a plugin from a file path
    #[cfg(feature = "plugins")]
    fn handle_load_plugin(&mut self, path: std::path::PathBuf, callback_id: JsCallbackId) {
        let load_result = self.plugin_manager.read().unwrap().load_plugin(&path);
        match load_result {
            Ok(()) => {
                tracing::info!("Loaded plugin from {:?}", path);
                self.plugin_manager
                    .read()
                    .unwrap()
                    .resolve_callback(callback_id, "true".to_string());
            }
            Err(e) => {
                tracing::error!("Failed to load plugin from {:?}: {}", path, e);
                self.plugin_manager
                    .read()
                    .unwrap()
                    .reject_callback(callback_id, format!("{}", e));
            }
        }
    }

    /// Unload a plugin by name
    #[cfg(feature = "plugins")]
    fn handle_unload_plugin(
        &mut self,
        name: String,
        callback_id: JsCallbackId,
        context: &PluginCommandContext,
    ) {
        if name == "orchestrator" && !Self::context_may_manage_bundled_plugins(context) {
            self.reject_callback_for_context(
                context,
                callback_id,
                "bundled built-in plugins may only be unloaded by the host".to_string(),
            );
            return;
        }
        // Drop the write guard before the read lock below (match-scrutinee
        // temporaries would otherwise live until end-of-match).
        let result = self.plugin_manager.write().unwrap().unload_plugin(&name);
        match result {
            Ok(()) => {
                tracing::info!("Unloaded plugin: {}", name);
                if let Ok(mut schemas) = self.plugin_schemas.write() {
                    schemas.remove(&name);
                }
                self.plugin_manager
                    .read()
                    .unwrap()
                    .resolve_callback(callback_id, "true".to_string());
            }
            Err(e) => {
                tracing::error!("Failed to unload plugin '{}': {}", name, e);
                self.plugin_manager
                    .read()
                    .unwrap()
                    .reject_callback(callback_id, format!("{}", e));
            }
        }
    }

    /// Reload a plugin by name
    #[cfg(feature = "plugins")]
    fn handle_reload_plugin(
        &mut self,
        name: String,
        callback_id: JsCallbackId,
        context: &PluginCommandContext,
    ) {
        if name == "orchestrator" {
            if !Self::context_may_manage_bundled_plugins(context) {
                self.reject_callback_for_context(
                    context,
                    callback_id,
                    "bundled built-in plugins may only be reloaded by the host".to_string(),
                );
                return;
            }
            #[cfg(feature = "embed-plugins")]
            if crate::services::plugins::embedded::get_embedded_plugins_dir().is_none() {
                self.reject_callback_for_context(
                    context,
                    callback_id,
                    "bundled plugin integrity verification failed".to_string(),
                );
                return;
            }
        }
        let reload_result = self.plugin_manager.read().unwrap().reload_plugin(&name);
        match reload_result {
            Ok(()) => {
                tracing::info!("Reloaded plugin: {}", name);
                self.plugin_manager
                    .read()
                    .unwrap()
                    .resolve_callback(callback_id, "true".to_string());
            }
            Err(e) => {
                tracing::error!("Failed to reload plugin '{}': {}", name, e);
                self.plugin_manager
                    .read()
                    .unwrap()
                    .reject_callback(callback_id, format!("{}", e));
            }
        }
    }

    /// List all loaded plugins
    #[cfg(feature = "plugins")]
    fn handle_list_plugins(&mut self, callback_id: JsCallbackId) {
        let plugins = self.plugin_manager.read().unwrap().list_plugins();
        // Serialize to JSON array of { name, path, enabled }
        let json_array: Vec<serde_json::Value> = plugins
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "path": if p.name == "orchestrator" { "<bundled>".into() } else { p.path.to_string_lossy().into_owned() },
                    "enabled": p.enabled
                })
            })
            .collect();
        let json_str = serde_json::to_string(&json_array).unwrap_or_else(|_| "[]".to_string());
        self.plugin_manager
            .read()
            .unwrap()
            .resolve_callback(callback_id, json_str);
    }

    /// Register an `editor.setInterval` / `setTimeout` timer.
    ///
    /// Re-registering an id replaces it rather than adding a second entry, so
    /// a plugin that re-runs its setup (a reload racing its own cleanup)
    /// cannot end up with the same timer firing twice per period.
    fn handle_set_plugin_timer(
        &mut self,
        timer_id: u64,
        plugin_name: String,
        handler_name: String,
        interval_ms: u64,
        repeat: bool,
    ) {
        use crate::app::plugin_timers::PluginTimer;

        let now = self.time_source.now();
        self.plugin_timers.retain(|t| t.id != timer_id);
        self.plugin_timers.push(PluginTimer::new(
            timer_id,
            plugin_name,
            handler_name,
            interval_ms,
            repeat,
            now,
        ));
    }

    /// Cancel a timer. Unknown ids are ignored: `clearInterval` on an
    /// already-fired one-shot is normal, not an error.
    fn handle_clear_plugin_timer(&mut self, timer_id: u64) {
        self.plugin_timers.retain(|t| t.id != timer_id);
    }

    /// Fire every timer that has come due — called once per editor tick.
    ///
    /// Returns whether anything fired, so the tick can decide to render. The
    /// handler runs through `Action::PluginAction`, the same dispatch a
    /// command or keybinding uses — so a handler that throws is caught and
    /// logged here, and the timer's next tick still fires. That is the
    /// property a detached `delay` loop lacks: one unguarded rejection there
    /// ends the loop for good, with nothing to report it.
    ///
    /// Due timers are collected, and one-shots retired, before any handler
    /// runs. Handlers reach this table only by sending a plugin command that
    /// a later tick drains, so they cannot mutate it underneath us — but
    /// collecting first keeps that a property of this function rather than of
    /// the channel's timing.
    pub fn check_plugin_timers(&mut self) -> bool {
        use crate::input::keybindings::Action;

        if self.plugin_timers.is_empty() {
            return false;
        }
        let now = self.time_source.now();

        let mut due: Vec<(u64, String, String)> = Vec::new();
        for timer in &mut self.plugin_timers {
            if timer.next_fire <= now {
                due.push((
                    timer.id,
                    timer.plugin_name.clone(),
                    timer.handler_name.clone(),
                ));
                timer.rearm(now);
            }
        }
        if due.is_empty() {
            return false;
        }
        // A one-shot is retired before its handler runs, so a handler that
        // throws can't leave a dead timer armed, and one that re-arms itself
        // gets the new registration rather than having it dropped here.
        let fired: std::collections::HashSet<u64> = due.iter().map(|(id, _, _)| *id).collect();
        self.plugin_timers
            .retain(|t| t.repeat || !fired.contains(&t.id));

        for (id, plugin_name, handler_name) in due {
            if let Err(e) = self.handle_action(Action::PluginAction(handler_name.clone())) {
                tracing::warn!(
                    "plugin timer {id} ({plugin_name}) handler '{handler_name}' failed: {e}"
                );
            }
        }
        true
    }

    /// Re-read and run `~/.config/fresh/init.ts` — `editor.reloadInit()`, and
    /// through it `fresh --cmd init reload`.
    ///
    /// Deliberately the same two steps as `Action::InitReload` (the palette's
    /// "init: Reload"), so what an agent exercises headlessly is what the user
    /// gets interactively: the load, then `plugins_loaded` re-fired for
    /// handlers that expect a post-load environment.
    #[cfg(feature = "plugins")]
    fn handle_reload_init(&mut self, callback_id: JsCallbackId) {
        use crate::init_script::InitOutcome;

        let outcome = self.load_init_script(true);
        self.fire_plugins_loaded_hook();

        let manager = self.plugin_manager.read().unwrap();
        match outcome {
            InitOutcome::Loaded => manager.resolve_callback(callback_id, "true".to_string()),
            // "Nothing to load" is not a failure: an agent that reloads after
            // writing init.ts for the first time, or on a `--safe` launch,
            // gets an answer it can branch on rather than an exception.
            InitOutcome::NotFound | InitOutcome::Disabled => {
                manager.resolve_callback(callback_id, "false".to_string())
            }
            // A parse/eval failure is the case worth interrupting for: the
            // previous init.ts is still live, and the caller's edit did not
            // take effect.
            other => manager.reject_callback(callback_id, crate::init_script::describe(&other)),
        }
    }

    /// Run a registered command by its palette name — `editor.runCommand()`,
    /// and through it `fresh --cmd command run "<name>"`.
    ///
    /// Dispatches the command's own `Action`, which for a plugin command is
    /// `PluginAction(handler)` — the identical path the palette takes when
    /// the user picks that row, rather than calling the plugin's exported
    /// function directly. That is the whole point: it exercises registration,
    /// name resolution and dispatch, not just the handler body.
    #[cfg(feature = "plugins")]
    fn handle_run_editor_command(&mut self, name: String, callback_id: JsCallbackId) {
        let command = self
            .command_registry
            .read()
            .unwrap()
            .resolve_by_display_name(&name);

        let Some(command) = command else {
            self.plugin_manager.read().unwrap().reject_callback(
                callback_id,
                format!(
                    "no registered command named '{name}' \
                     (list them with editor.listCommands() or `fresh --cmd command list`)"
                ),
            );
            return;
        };

        // Mirror the palette: a run counts as usage, so recency ordering
        // reflects agent-driven runs the same way it reflects the user's.
        self.command_registry
            .write()
            .unwrap()
            .record_usage(&command.name);

        let result = self.handle_action(command.action);
        let manager = self.plugin_manager.read().unwrap();
        match result {
            Ok(()) => manager.resolve_callback(callback_id, "true".to_string()),
            Err(e) => manager.reject_callback(callback_id, format!("command '{name}' failed: {e}")),
        }
    }

    /// Every registered command, built-in and plugin alike —
    /// `editor.listCommands()` / `fresh --cmd command list`.
    #[cfg(feature = "plugins")]
    fn handle_list_editor_commands(&mut self, callback_id: JsCallbackId) {
        use crate::input::commands::CommandSource;

        let commands = self.command_registry.read().unwrap().get_all();
        let json_array: Vec<serde_json::Value> = commands
            .iter()
            .map(|c| {
                let (source, plugin) = match &c.source {
                    CommandSource::Builtin => ("builtin", String::new()),
                    CommandSource::Plugin(name) => ("plugin", name.clone()),
                };
                serde_json::json!({
                    // The displayed form, because that is the name
                    // `runCommand` resolves and the palette shows.
                    "name": c.get_localized_name(),
                    "description": c.get_localized_description(),
                    "source": source,
                    "plugin": plugin,
                })
            })
            .collect();
        let json_str = serde_json::to_string(&json_array).unwrap_or_else(|_| "[]".to_string());
        self.plugin_manager
            .read()
            .unwrap()
            .resolve_callback(callback_id, json_str);
    }

    /// Execute an editor action by name (for vi mode plugin)
    fn handle_execute_action(&mut self, action_name: String) {
        use crate::input::keybindings::Action;
        use std::collections::HashMap;

        // Parse the action name into an Action enum
        if let Some(action) = Action::from_str(&action_name, &HashMap::new()) {
            // Execute the action
            if let Err(e) = self.handle_action(action) {
                tracing::warn!("Failed to execute action '{}': {}", action_name, e);
            } else {
                tracing::debug!("Executed action: {}", action_name);
            }
        } else {
            tracing::warn!("Unknown action: {}", action_name);
        }
    }

    /// Execute multiple actions in sequence, each with an optional repeat count
    /// Used by vi mode for count prefix (e.g., "3dw" = delete 3 words)
    fn handle_execute_actions(&mut self, actions: Vec<fresh_core::api::ActionSpec>) {
        use crate::input::keybindings::Action;

        // Plugins may *request* the trust prompt (`workspace_trust_prompt`,
        // which asks the user) but must never *set* the trust level
        // themselves. Granting/lowering trust is a user+core decision — the
        // same boundary VS Code, JetBrains, and Zed enforce: an extension can
        // open the prompt, the user decides. Silently drop any attempt to
        // dispatch the level-setting actions through this generic channel.
        const PLUGIN_FORBIDDEN_ACTIONS: &[&str] = &[
            "workspace_trust_trust",
            "workspace_trust_restrict",
            "workspace_trust_block",
        ];

        for action_spec in actions {
            if PLUGIN_FORBIDDEN_ACTIONS.contains(&action_spec.action.as_str()) {
                tracing::warn!(
                    "plugin attempted to set workspace trust via '{}' — denied; \
                     plugins may request the prompt (workspace_trust_prompt), not set the level",
                    action_spec.action
                );
                continue;
            }
            if let Some(action) = Action::from_str(&action_spec.action, &action_spec.args) {
                // Execute the action `count` times
                for _ in 0..action_spec.count {
                    if let Err(e) = self.handle_action(action.clone()) {
                        tracing::warn!("Failed to execute action '{}': {}", action_spec.action, e);
                        return; // Stop on first error
                    }
                }
                tracing::debug!(
                    "Executed action '{}' {} time(s)",
                    action_spec.action,
                    action_spec.count
                );
            } else {
                tracing::warn!("Unknown action: {}", action_spec.action);
                return; // Stop on unknown action
            }
        }
    }

    /// Define (or replace) an in-memory macro under `register` from a step
    /// list supplied by a plugin (`editor.defineMacro`). Each step is parsed
    /// through `Action::from_str` (with its args), so payload actions like
    /// `insert_char` reconstruct faithfully. Unknown action names are skipped
    /// with a warning rather than aborting, so one typo in a hand-edited
    /// `init.ts` macro doesn't discard the whole register.
    fn handle_define_macro(&mut self, register: String, steps: Vec<fresh_core::api::ActionSpec>) {
        use crate::input::keybindings::Action;

        let Some(key) = register.chars().next() else {
            tracing::warn!("defineMacro: empty register key, ignoring");
            return;
        };

        let mut actions = Vec::with_capacity(steps.len());
        for spec in &steps {
            match Action::from_str(&spec.action, &spec.args) {
                Some(action) => {
                    for _ in 0..spec.count.max(1) {
                        actions.push(action.clone());
                    }
                }
                None => {
                    tracing::warn!(
                        "defineMacro['{}']: unknown action '{}' skipped",
                        key,
                        spec.action
                    );
                }
            }
        }

        let count = actions.len();
        self.active_window_mut().macros.define(key, actions);
        tracing::debug!("defineMacro['{}']: stored {} action(s)", key, count);
    }

    /// Get text from a buffer range (for vi mode yank operations).
    ///
    /// See [`clamp_buffer_text_range`] for why the requested range is
    /// clamped rather than rejected.
    fn handle_get_buffer_text(
        &mut self,
        buffer_id: BufferId,
        start: usize,
        end: usize,
        request_id: u64,
    ) {
        let result = if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&buffer_id)
        {
            // Plugins derive `end` from a snapshot length (see
            // `get_buffer_length`) that lags the live buffer, so when the
            // buffer shrinks between the length read and this fetch — e.g.
            // concurrent edits from the editor and an external process
            // rewriting the file on disk — the requested end can briefly
            // exceed the live length. Clamp to the current bounds and return
            // what's there; the plugin recomputes on the next change event.
            let (start, end) = clamp_buffer_text_range(start, end, state.buffer.len());
            Ok(state.get_text_range(start, end))
        } else {
            Err(format!("Buffer {:?} not found", buffer_id))
        };

        // Resolve the JavaScript Promise callback directly
        let callback_id = fresh_core::api::JsCallbackId::from(request_id);
        match result {
            Ok(text) => {
                // Serialize text as JSON string
                let json = serde_json::to_string(&text).unwrap_or_else(|_| "null".to_string());
                self.plugin_manager
                    .read()
                    .unwrap()
                    .resolve_callback(callback_id, json);
            }
            Err(error) => {
                self.plugin_manager
                    .read()
                    .unwrap()
                    .reject_callback(callback_id, error);
            }
        }
    }

    /// Set the global editor mode (for vi mode)
    fn handle_set_editor_mode(&mut self, mode: Option<String>) {
        self.active_window_mut().editor_mode = mode.clone();
        tracing::debug!("Set editor mode: {:?}", mode);
    }

    /// Normalize a plugin-supplied `BufferId`: treat id 0 as "use the active buffer".
    pub(super) fn resolve_buffer_id(&self, buffer_id: BufferId) -> BufferId {
        if buffer_id.0 == 0 {
            self.active_buffer()
        } else {
            buffer_id
        }
    }

    /// Serialize `value` as JSON and resolve `request_id` as a JS Promise callback.
    fn resolve_json_callback<T: serde::Serialize>(&mut self, request_id: u64, value: T) {
        let callback_id = fresh_core::api::JsCallbackId::from(request_id);
        let json = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
        self.plugin_manager
            .read()
            .unwrap()
            .resolve_callback(callback_id, json);
    }

    /// Get the byte offset of the start of a line in the active buffer.
    fn handle_get_line_start_position(&mut self, buffer_id: BufferId, line: u32, request_id: u64) {
        self.handle_get_line_position(buffer_id, line, request_id, false);
    }

    /// Get the byte offset of the end of a line (position of its terminating newline,
    /// or `buffer_len` for the last line without a trailing newline).
    fn handle_get_line_end_position(&mut self, buffer_id: BufferId, line: u32, request_id: u64) {
        self.handle_get_line_position(buffer_id, line, request_id, true);
    }

    /// Get the total number of lines in a buffer
    fn handle_get_buffer_line_count(&mut self, buffer_id: BufferId, request_id: u64) {
        let actual_buffer_id = self.resolve_buffer_id(buffer_id);

        let result = if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&actual_buffer_id)
        {
            let buffer_len = state.buffer.len();
            let content = state.get_text_range(0, buffer_len);
            let newlines = content.bytes().filter(|&b| b == b'\n').count();
            Some(if content.is_empty() {
                1
            } else {
                newlines + usize::from(!content.ends_with('\n'))
            })
        } else {
            None
        };

        self.resolve_json_callback(request_id, result);
    }

    /// Resolve cursor info for the active composite (side-by-side diff)
    /// buffer. Returns `null` to the plugin when the active buffer isn't a
    /// composite buffer; otherwise an object with the focused pane index,
    /// pane count, and the 0-indexed source line shown in each pane on the
    /// cursor's aligned row (`null` per-pane where that side is blank).
    fn handle_get_composite_cursor_info(&mut self, request_id: u64) {
        let info = self.active_window().active_composite_cursor_info();
        let value = info.map(|(focused_pane, pane_count, lines)| {
            serde_json::json!({
                "focusedPane": focused_pane,
                "paneCount": pane_count,
                "lines": lines,
            })
        });
        self.resolve_json_callback(request_id, value);
    }

    /// Open `path` as a regular buffer for plugin-driven streaming
    /// display. The file is created (empty) if missing.
    ///
    /// Routes through the same `open_file_no_focus` orchestrator that
    /// `editor.openFile` uses, so the buffer gets the full setup
    /// (encoding/binary detection, language detection, buffer settings,
    /// margin config, per-split BufferViewState defaults). This is
    /// critical for things like the scrollbar's visual-row index —
    /// bypassing this setup and going straight to `BufferData::Unloaded`
    /// breaks `line_count()` and any code that depends on it.
    ///
    /// Designed for buffers that will be filled by a concurrent
    /// `spawnProcess` with `stdoutTo`. Pair with `RefreshBufferFromDisk`
    /// to grow the buffer as the file is written; `extend_streaming`
    /// (called by that path) counts newlines in the appended region
    /// so the buffer's line index stays correct as it grows.
    fn handle_open_file_streaming(&mut self, path: std::path::PathBuf, request_id: u64) {
        // Ensure the file exists at 0 bytes if missing, so the open
        // path has something to load.
        if !self.authority().filesystem.exists(&path) {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        tracing::warn!(
                            "openFileStreaming: failed to create parent dir {:?}: {}",
                            parent,
                            e
                        );
                        self.resolve_json_callback::<Option<u64>>(request_id, None);
                        return;
                    }
                }
            }
            if let Err(e) = std::fs::write(&path, b"") {
                tracing::warn!(
                    "openFileStreaming: failed to create empty file at {:?}: {}",
                    path,
                    e
                );
                self.resolve_json_callback::<Option<u64>>(request_id, None);
                return;
            }
        }

        // Use the same orchestrator that backs `editor.openFile`. This
        // ensures the buffer is set up identically to a user-opened
        // file (settings, language, view-state defaults, line indexing).
        let buffer_id = match self.open_file_no_focus(&path) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    "openFileStreaming: open_file_no_focus failed for {:?}: {}",
                    path,
                    e
                );
                self.resolve_json_callback::<Option<u64>>(request_id, None);
                return;
            }
        };

        // Plugin-managed surfaces (typically buffer-group panel
        // targets) shouldn't show up in quick-switch / tab strip, and
        // shouldn't be auto-reverted on file change — the plugin is
        // driving the file's contents itself via `extend_streaming`.
        if let Some(meta) = self.active_window_mut().buffer_metadata.get_mut(&buffer_id) {
            meta.hidden_from_tabs = true;
            meta.auto_revert_enabled = false;
        }
        let active_split = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .active_split();
        if let Some(vs) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_view_states_mut())
            .expect("active window must have a populated split layout")
            .get_mut(&active_split)
        {
            use crate::view::split::TabTarget;
            vs.open_buffers
                .retain(|t| !matches!(t, TabTarget::Buffer(b) if *b == buffer_id));
        }

        self.resolve_json_callback(request_id, Some(buffer_id.0));
    }

    /// Re-point a buffer-group's panel at a different buffer id.
    /// Delegates to `BufferGroupOps::set_buffer_group_panel_buffer`.
    fn handle_set_buffer_group_panel_buffer(
        &mut self,
        group_id: usize,
        panel_name: String,
        buffer_id: BufferId,
        request_id: u64,
    ) {
        let actual_buffer_id = self.resolve_buffer_id(buffer_id);
        let ok = self.set_buffer_group_panel_buffer(group_id, panel_name, actual_buffer_id);
        self.resolve_json_callback(request_id, ok);
    }

    /// Re-stat the file backing `buffer_id` and extend the buffer if
    /// the file has grown. No-op if the buffer has no file path or the
    /// file didn't grow. Resolves with the new total byte length.
    fn handle_refresh_buffer_from_disk(&mut self, buffer_id: BufferId, request_id: u64) {
        let actual_buffer_id = self.resolve_buffer_id(buffer_id);

        let path = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, _)| ())
            .and_then(|_| {
                self.windows
                    .get(&self.active_window)?
                    .buffers
                    .get(&actual_buffer_id)?
                    .buffer
                    .file_path()
                    .map(|p| p.to_path_buf())
            });

        let Some(path) = path else {
            // No file path — nothing to refresh.
            self.resolve_json_callback::<Option<usize>>(request_id, None);
            return;
        };

        let new_size = match self.authority().filesystem.metadata(&path) {
            Ok(m) => m.size as usize,
            Err(_) => {
                self.resolve_json_callback::<Option<usize>>(request_id, None);
                return;
            }
        };

        let new_total = if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&actual_buffer_id)
        {
            let old = state.buffer.total_bytes();
            if new_size > old {
                state.buffer.extend_streaming(&path, new_size);
            }
            state.buffer.total_bytes()
        } else {
            self.resolve_json_callback::<Option<usize>>(request_id, None);
            return;
        };

        self.resolve_json_callback(request_id, Some(new_total));
    }

    /// Scroll a split to center a specific line in the viewport
    fn handle_scroll_to_line_center(
        &mut self,
        split_id: SplitId,
        buffer_id: BufferId,
        line: usize,
    ) {
        let actual_split_id = if split_id.0 == 0 {
            self.windows
                .get(&self.active_window)
                .and_then(|w| w.buffers.splits())
                .map(|(mgr, _)| mgr)
                .expect("active window must have a populated split layout")
                .active_split()
        } else {
            LeafId(split_id)
        };
        let actual_buffer_id = self.resolve_buffer_id(buffer_id);

        // Get viewport height
        let viewport_height = if let Some(view_state) = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(_, vs)| vs)
            .expect("active window must have a populated split layout")
            .get(&actual_split_id)
        {
            view_state.viewport.height as usize
        } else {
            return;
        };

        // Calculate the target line to scroll to (center the requested line)
        let lines_above = viewport_height / 2;
        let target_line = line.saturating_sub(lines_above);

        self.active_window_mut().scroll_split_viewport_to(
            actual_buffer_id,
            actual_split_id,
            target_line,
            true,
        );
    }

    /// Scroll every split whose active buffer is `buffer_id` so that
    /// `line` is within the viewport. Used by plugin panels (buffer
    /// groups) whose plugin-side "selected row" doesn't drive the
    /// buffer cursor — after updating the selection, the plugin calls
    /// this to bring the selected row into view.
    ///
    /// Walks both the main split tree's leaves AND the inner leaves of
    /// all Grouped subtrees stored in `grouped_subtrees`, because the
    /// latter are not represented in `split_manager`'s tree.
    fn handle_scroll_buffer_to_line(&mut self, buffer_id: BufferId, line: usize) {
        if !self
            .windows
            .get(&self.active_window)
            .map(|w| &w.buffers)
            .expect("active window present")
            .contains_key(&buffer_id)
        {
            return;
        }

        // Collect the leaf ids whose active buffer is `buffer_id`.
        let mut target_leaves: Vec<LeafId> = Vec::new();

        // Main tree: walk its leaves.
        for leaf_id in self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .root()
            .leaf_split_ids()
        {
            if let Some(vs) = self
                .windows
                .get(&self.active_window)
                .and_then(|w| w.buffers.splits())
                .map(|(_, vs)| vs)
                .expect("active window must have a populated split layout")
                .get(&leaf_id)
            {
                if vs.active_buffer == buffer_id {
                    target_leaves.push(leaf_id);
                }
            }
        }

        // Grouped subtrees: walk each group's inner leaves.
        for (_group_leaf_id, node) in self.active_window().grouped_subtrees.iter() {
            if let crate::view::split::SplitNode::Grouped { layout, .. } = node {
                for inner_leaf in layout.leaf_split_ids() {
                    if let Some(vs) = self
                        .windows
                        .get(&self.active_window)
                        .and_then(|w| w.buffers.splits())
                        .map(|(_, vs)| vs)
                        .expect("active window must have a populated split layout")
                        .get(&inner_leaf)
                    {
                        if vs.active_buffer == buffer_id && !target_leaves.contains(&inner_leaf) {
                            target_leaves.push(inner_leaf);
                        }
                    }
                }
            }
        }

        if target_leaves.is_empty() {
            return;
        }

        self.active_window_mut()
            .scroll_buffer_to_line_in_splits(buffer_id, &target_leaves, line);
    }

    fn handle_spawn_host_process(
        &mut self,
        window_id: fresh_core::WindowId,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        callback_id: JsCallbackId,
    ) {
        // Bypass the active authority on purpose: this is
        // reserved for plugin internals that must run host-side
        // work (e.g. `devcontainer up`) before the authority
        // they want is even built. Uses the same callback shape
        // as `SpawnProcess` so the plugin-facing API is
        // symmetric.
        //
        // Kill handle: we store a oneshot sender in
        // `host_process_handles` keyed by the callback id. A
        // `KillHostProcess` dispatch sends on it; the spawn
        // task's `tokio::select!` then start_kill()s the
        // child. This lets a plugin cancel a long-running
        // spawn (e.g. "Cancel Startup" on the Remote
        // Indicator popup during `devcontainer up`).
        let Some(window) = self.windows.get(&window_id) else {
            self.plugin_manager.read().unwrap().reject_callback(
                callback_id,
                format!(
                    "spawnHostProcess: window {} is no longer available",
                    window_id.0
                ),
            );
            return;
        };
        let default_cwd = window.root.to_string_lossy().into_owned();
        let workspace_trust = std::sync::Arc::clone(&window.authority().workspace_trust);
        if let (Some(runtime), Some(bridge)) = (&self.tokio_runtime, &self.async_bridge) {
            use tokio::io::{AsyncReadExt, BufReader};
            use tokio::process::Command as TokioCommand;

            let effective_cwd = cwd.or(Some(default_cwd));
            let sender = bridge.sender();
            let process_id = callback_id.as_u64();

            // Workspace Trust gates host spawns too. `spawnHostProcess`
            // deliberately bypasses the authority spawner, so the choke-point
            // guard never sees it — enforce the level here directly. Blocked
            // fails every host spawn; Restricted refuses repo-local
            // executables. Without this, Blocked wouldn't actually block
            // everything.
            if let crate::services::workspace_trust::SpawnDecision::Deny(reason) =
                workspace_trust.decide(&command, effective_cwd.as_deref())
            {
                #[allow(clippy::let_underscore_must_use)]
                let _ = sender.send(AsyncMessage::PluginProcessOutput {
                    process_id,
                    stdout: String::new(),
                    stderr: reason,
                    exit_code: -1,
                });
                return;
            }

            let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
            self.host_process_handles
                .insert(process_id, (window_id, kill_tx));

            runtime.spawn(async move {
                use crate::services::process_hidden::HideWindow;
                let mut cmd = TokioCommand::new(&command);
                cmd.args(&args);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                cmd.hide_window();
                if let Some(ref dir) = effective_cwd {
                    cmd.current_dir(dir);
                }
                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        #[allow(clippy::let_underscore_must_use)]
                        let _ = sender.send(AsyncMessage::PluginProcessOutput {
                            process_id,
                            stdout: String::new(),
                            stderr: e.to_string(),
                            exit_code: -1,
                        });
                        return;
                    }
                };

                // Take the pipes out of the Child so the
                // reader tasks own them; then `child.wait()`
                // has exclusive mutable access for the
                // kill-or-exit select. Matches the
                // fresh-plugin-runtime process.rs pattern.
                let stdout_pipe = child.stdout.take();
                let stderr_pipe = child.stderr.take();

                let stdout_fut = async {
                    let mut buf = String::new();
                    if let Some(s) = stdout_pipe {
                        #[allow(clippy::let_underscore_must_use)]
                        let _ = BufReader::new(s).read_to_string(&mut buf).await;
                    }
                    buf
                };
                let stderr_fut = async {
                    let mut buf = String::new();
                    if let Some(s) = stderr_pipe {
                        #[allow(clippy::let_underscore_must_use)]
                        let _ = BufReader::new(s).read_to_string(&mut buf).await;
                    }
                    buf
                };
                let wait_fut = async {
                    tokio::select! {
                        status = child.wait() => {
                            status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1)
                        }
                        _ = &mut kill_rx => {
                            // Best-effort SIGKILL + reap.
                            // Children of the killed
                            // process may leak (Q-C2).
                            #[allow(clippy::let_underscore_must_use)]
                            let _ = child.start_kill();
                            child
                                .wait()
                                .await
                                .map(|s| s.code().unwrap_or(-1))
                                .unwrap_or(-1)
                        }
                    }
                };
                let (stdout, stderr, exit_code) = tokio::join!(stdout_fut, stderr_fut, wait_fut);

                #[allow(clippy::let_underscore_must_use)]
                let _ = sender.send(AsyncMessage::PluginProcessOutput {
                    process_id,
                    stdout,
                    stderr,
                    exit_code,
                });
            });
        } else {
            self.plugin_manager
                .read()
                .unwrap()
                .reject_callback(callback_id, "Async runtime not available".to_string());
        }
    }

    fn handle_spawn_background_process(
        &mut self,
        window_id: fresh_core::WindowId,
        process_id: u64,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        callback_id: JsCallbackId,
    ) {
        let Some(window) = self.windows.get(&window_id) else {
            self.plugin_manager.read().unwrap().reject_callback(
                callback_id,
                format!(
                    "spawnBackgroundProcess: window {} is no longer available",
                    window_id.0
                ),
            );
            return;
        };
        let spawner = std::sync::Arc::clone(&window.authority().long_running_spawner);
        let effective_cwd = cwd
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| window.root.clone());

        if let (Some(runtime), Some(bridge)) = (&self.tokio_runtime, &self.async_bridge) {
            use tokio::io::{AsyncBufReadExt, BufReader};

            let sender = bridge.sender();
            let sender_stdout = sender.clone();
            let sender_stderr = sender.clone();
            let callback_id_u64 = callback_id.as_u64();

            let handle = runtime.spawn(async move {
                let mut child = match spawner
                    .spawn_stdio(&command, &args, Vec::new(), Some(&effective_cwd), None)
                    .await
                {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = sender.send(crate::services::async_bridge::AsyncMessage::Plugin(
                            fresh_core::api::PluginAsyncMessage::ProcessExit {
                                process_id,
                                callback_id: callback_id_u64,
                                exit_code: -1,
                            },
                        ));
                        tracing::error!(?window_id, "Failed to spawn background process: {error}");
                        return;
                    }
                };

                let stdout = child.take_stdout();
                let stderr = child.take_stderr();

                if let Some(stdout) = stdout {
                    tokio::spawn(async move {
                        let reader = BufReader::new(stdout);
                        let mut lines = reader.lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let _ = sender_stdout.send(
                                crate::services::async_bridge::AsyncMessage::Plugin(
                                    fresh_core::api::PluginAsyncMessage::ProcessStdout {
                                        process_id,
                                        data: line + "\n",
                                    },
                                ),
                            );
                        }
                    });
                }

                if let Some(stderr) = stderr {
                    tokio::spawn(async move {
                        let reader = BufReader::new(stderr);
                        let mut lines = reader.lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let _ = sender_stderr.send(
                                crate::services::async_bridge::AsyncMessage::Plugin(
                                    fresh_core::api::PluginAsyncMessage::ProcessStderr {
                                        process_id,
                                        data: line + "\n",
                                    },
                                ),
                            );
                        }
                    });
                }

                let exit_code = child
                    .wait()
                    .await
                    .ok()
                    .and_then(|status| status.code())
                    .unwrap_or(-1);
                let _ = sender.send(crate::services::async_bridge::AsyncMessage::Plugin(
                    fresh_core::api::PluginAsyncMessage::ProcessExit {
                        process_id,
                        callback_id: callback_id_u64,
                        exit_code,
                    },
                ));
            });

            self.background_process_handles
                .insert(process_id, (window_id, handle.abort_handle()));
        } else {
            self.plugin_manager
                .read()
                .unwrap()
                .reject_callback(callback_id, "Async runtime not available".to_string());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_create_virtual_buffer_with_content(
        &mut self,
        name: String,
        mode: String,
        read_only: bool,
        entries: Vec<fresh_core::text_property::TextPropertyEntry>,
        show_line_numbers: bool,
        show_cursors: bool,
        editing_disabled: bool,
        hidden_from_tabs: bool,
        initial_cursor_line: Option<u32>,
        indentation_guide: Option<bool>,
        request_id: Option<u64>,
    ) {
        // Hidden-from-tabs buffers (e.g. composite source panes) must NOT be
        // attached to the active split or made the active buffer: doing so
        // pollutes the main tab bar and, when they're later closed, leaves
        // auto-created "[No Name]" tabs behind. Create them detached.
        let buffer_id = if hidden_from_tabs {
            self.active_window_mut().create_virtual_buffer_detached(
                name.clone(),
                mode.clone(),
                read_only,
            )
        } else {
            self.active_window_mut()
                .create_virtual_buffer(name.clone(), mode.clone(), read_only)
        };
        tracing::info!(
            "Created virtual buffer '{}' with mode '{}' (id={:?}, detached={})",
            name,
            mode,
            buffer_id,
            hidden_from_tabs
        );

        // TODO: show_line_numbers is duplicated between EditorState.margins and
        // BufferViewState. The renderer reads BufferViewState and overwrites
        // margins each frame via configure_for_line_numbers(), making the margin
        // setting here effectively write-only. Consider removing the margin call
        // and only setting BufferViewState.show_line_numbers.
        self.configure_vbuf_display(
            buffer_id,
            show_line_numbers,
            show_cursors,
            editing_disabled,
            indentation_guide,
            true,
        );
        if !hidden_from_tabs {
            let active_split = self.split_manager().active_split();
            if let Some(view_state) = self
                .windows
                .get_mut(&self.active_window)
                .and_then(|w| w.split_view_states_mut())
                .expect("active window must have a populated split layout")
                .get_mut(&active_split)
            {
                view_state.ensure_buffer_state(buffer_id).show_line_numbers = show_line_numbers;
            }
        } else if let Some(meta) = self.active_window_mut().buffer_metadata.get_mut(&buffer_id) {
            meta.hidden_from_tabs = true;
        }

        // Now set the content
        match self.set_virtual_buffer_content(buffer_id, entries) {
            Ok(()) => {
                tracing::debug!("Set virtual buffer content for {:?}", buffer_id);
                // Switch to the new buffer to display it — but only when it's
                // attached (a detached hidden buffer must not steal the view).
                if !hidden_from_tabs {
                    self.set_active_buffer(buffer_id);
                    tracing::debug!("Switched to virtual buffer {:?}", buffer_id);
                }

                // Apply an initial cursor position. We do this in the same
                // command-processing tick as creation, so the cursor is in
                // place by the time the editor next processes user input —
                // a follow-up SetBufferCursor from the plugin would race
                // against the user. The plugin passes a line index; we
                // resolve it to a byte here using the buffer's own content
                // so UTF-8-byte math never touches JS-side string-length
                // semantics. Done AFTER `set_active_buffer` so the new
                // buffer is actually mounted in a split when
                // `set_buffer_cursor_in_splits` walks the split tree.
                if let Some(line) = initial_cursor_line {
                    let target_line = line as usize;
                    let byte = self
                        .windows
                        .get_mut(&self.active_window)
                        .and_then(|w| w.buffers.get_mut(&buffer_id))
                        .map(|s| {
                            let total = s.buffer.len();
                            let mut iter = s.buffer.line_iterator(0, 80);
                            let mut target_byte = 0;
                            for current_line in 0..=target_line {
                                if let Some((line_start, _)) = iter.next_line() {
                                    if current_line == target_line {
                                        target_byte = line_start;
                                        break;
                                    }
                                } else {
                                    target_byte = total;
                                    break;
                                }
                            }
                            target_byte
                        })
                        .unwrap_or(0);
                    let splits: Vec<super::LeafId> = self
                        .windows
                        .get(&self.active_window)
                        .and_then(|w| w.buffers.splits())
                        .map(|(mgr, _)| mgr)
                        .expect("active window must have a populated split layout")
                        .splits_for_buffer(buffer_id);
                    self.active_window_mut()
                        .set_buffer_cursor_in_splits(buffer_id, byte, &splits);
                }

                // Send response if request_id is present
                if let Some(req_id) = request_id {
                    tracing::info!(
                                "CreateVirtualBufferWithContent: resolving callback for request_id={}, buffer_id={:?}",
                                req_id,
                                buffer_id
                            );
                    // createVirtualBuffer returns VirtualBufferResult: { bufferId, splitId }
                    let result = fresh_core::api::VirtualBufferResult {
                        buffer_id: buffer_id.0 as u64,
                        split_id: None,
                    };
                    self.plugin_manager.read().unwrap().resolve_callback(
                        fresh_core::api::JsCallbackId::from(req_id),
                        serde_json::to_string(&result).unwrap_or_default(),
                    );
                    tracing::info!(
                        "CreateVirtualBufferWithContent: resolve_callback sent for request_id={}",
                        req_id
                    );
                }
            }
            Err(e) => {
                tracing::error!("Failed to set virtual buffer content: {}", e);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_create_virtual_buffer_in_split(
        &mut self,
        name: String,
        mode: String,
        read_only: bool,
        entries: Vec<fresh_core::text_property::TextPropertyEntry>,
        ratio: f32,
        direction: Option<String>,
        panel_id: Option<String>,
        show_line_numbers: bool,
        show_cursors: bool,
        editing_disabled: bool,
        line_wrap: Option<bool>,
        before: bool,
        role: Option<String>,
        scrollable: Option<bool>,
        request_id: Option<u64>,
    ) {
        // Buffers are user-scrollable unless the plugin opts out (widget
        // panels that own their own scroll window).
        let scrollable = scrollable.unwrap_or(true);
        // Resolve the role string. Unknown roles are silently dropped
        // (forward-compat for plugins targeting newer cores).
        let split_role: Option<crate::view::split::SplitRole> = match role.as_deref() {
            Some("utility_dock") => Some(crate::view::split::SplitRole::UtilityDock),
            _ => None,
        };

        // Path 1 — Utility-dock fast path (issue #1796 / Section 2 of the design):
        // if a leaf with this role already exists, attach the new buffer there
        // instead of spawning a fresh split.
        if let Some(dock_leaf) = split_role.and_then(|r| self.split_manager().find_leaf_by_role(r))
        {
            return self.route_vbuf_to_existing_dock(
                dock_leaf,
                name,
                mode,
                read_only,
                entries,
                panel_id.as_deref(),
                show_line_numbers,
                show_cursors,
                editing_disabled,
                scrollable,
                request_id,
            );
            // No dock yet — fall through to normal split creation,
            // then tag the new leaf with the requested role at the end.
        }

        // Path 2 — Idempotent panel update: if this panel_id already maps to a
        // live buffer, refresh its content and re-focus it.
        if let Some(pid) = panel_id.as_deref() {
            let maybe_existing = self.panel_ids().get(pid).copied();
            if let Some(existing_id) = maybe_existing {
                let buffer_alive = self
                    .windows
                    .get(&self.active_window)
                    .map(|w| w.buffers.contains_key(&existing_id))
                    .unwrap_or(false);
                if buffer_alive {
                    return self.update_existing_vbuf_panel(existing_id, entries, request_id, pid);
                }
                // Buffer no longer exists — remove the stale entry and fall through.
                tracing::warn!(
                    "Removing stale panel_id '{}' pointing to non-existent buffer {:?}",
                    pid,
                    existing_id
                );
                self.panel_ids_mut().remove(pid);
            }
        }

        // Path 3 — Fresh split creation.
        //
        // Capture the source split before creating the buffer —
        // `create_virtual_buffer` unconditionally adds the new buffer as a tab
        // to the currently active split, which is wrong for a panel that lives
        // in its own dedicated split (it would appear in BOTH splits — bug #3).
        let source_split_before_create = self.split_manager().active_split();

        let buffer_id =
            self.active_window_mut()
                .create_virtual_buffer(name.clone(), mode.clone(), read_only);
        tracing::info!(
            "Created virtual buffer '{}' with mode '{}' in split (id={:?})",
            name,
            mode,
            buffer_id
        );

        self.configure_vbuf_display(
            buffer_id,
            show_line_numbers,
            show_cursors,
            editing_disabled,
            None,
            scrollable,
        );

        if let Some(pid) = panel_id {
            self.panel_ids_mut().insert(pid, buffer_id);
        }

        if let Err(e) = self.set_virtual_buffer_content(buffer_id, entries) {
            tracing::error!("Failed to set virtual buffer content: {}", e);
            return;
        }

        let split_dir = match direction.as_deref() {
            Some("vertical") => crate::model::event::SplitDirection::Vertical,
            _ => crate::model::event::SplitDirection::Horizontal,
        };

        // When the caller requested `role = "utility_dock"` but no dock leaf
        // existed yet (we fell through the fast path above), split at the
        // *root* so the dock spans the full width — splitting the active leaf
        // would nest it under whichever pane was focused.
        let split_result = if split_role == Some(crate::view::split::SplitRole::UtilityDock) {
            self.split_manager_mut()
                .split_root_positioned(split_dir, buffer_id, ratio, before)
        } else {
            self.split_manager_mut()
                .split_active_positioned(split_dir, buffer_id, ratio, before)
        };

        let created_split_id = match split_result {
            Ok(new_split_id) => {
                // The buffer now lives in its own split — drop its phantom tab
                // from the source split (bug #3). Only when the splits differ;
                // otherwise we'd leave the buffer with no display.
                if new_split_id != source_split_before_create {
                    if let Some(src_vs) = self
                        .windows
                        .get_mut(&self.active_window)
                        .and_then(|w| w.split_view_states_mut())
                        .expect("active window must have a populated split layout")
                        .get_mut(&source_split_before_create)
                    {
                        src_vs.remove_buffer(buffer_id);
                    }
                }

                let mut view_state = SplitViewState::with_buffer(
                    self.terminal_width,
                    self.terminal_height,
                    buffer_id,
                );
                view_state.apply_config_defaults(crate::view::split::ViewConfigDefaults {
                    line_numbers: self.config.editor.line_numbers,
                    highlight_current_line: self.config.editor.highlight_current_line,
                    line_wrap: line_wrap.unwrap_or_else(|| {
                        self.active_window().resolve_line_wrap_for_buffer(buffer_id)
                    }),
                    wrap_indent: self.config.editor.wrap_indent,
                    wrap_column: self
                        .active_window()
                        .resolve_wrap_column_for_buffer(buffer_id),
                    rulers: self.config.editor.rulers.clone(),
                    scroll_offset: self.config.editor.scroll_offset,
                });
                view_state.ensure_buffer_state(buffer_id).show_line_numbers = show_line_numbers;
                self.windows
                    .get_mut(&self.active_window)
                    .and_then(|w| w.split_view_states_mut())
                    .expect("active window must have a populated split layout")
                    .insert(new_split_id, view_state);

                self.split_manager_mut().set_active_split(new_split_id);

                // Tag the new leaf with the requested role so the next
                // utility-dock open lands here. Clear any stale role first
                // to maintain the one-leaf-per-role invariant.
                if let Some(target_role) = split_role {
                    self.split_manager_mut().clear_role(target_role);
                    self.split_manager_mut()
                        .set_leaf_role(new_split_id, Some(target_role));
                    tracing::info!(
                        "Tagged new dock leaf {:?} with role {:?}",
                        new_split_id,
                        target_role
                    );
                }

                tracing::info!(
                    "Created {:?} split with virtual buffer {:?}",
                    split_dir,
                    buffer_id
                );
                Some(new_split_id)
            }
            Err(e) => {
                tracing::error!("Failed to create split: {}", e);
                self.set_active_buffer(buffer_id);
                None
            }
        };

        if let Some(req_id) = request_id {
            tracing::trace!(
                "CreateVirtualBufferInSplit: resolving callback for request_id={}, \
                 buffer_id={:?}, split_id={:?}",
                req_id,
                buffer_id,
                created_split_id
            );
            let result = fresh_core::api::VirtualBufferResult {
                buffer_id: buffer_id.0 as u64,
                split_id: created_split_id.map(|s| s.0 .0 as u64),
            };
            self.plugin_manager.read().unwrap().resolve_callback(
                fresh_core::api::JsCallbackId::from(req_id),
                serde_json::to_string(&result).unwrap_or_default(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_create_virtual_buffer_in_existing_split(
        &mut self,
        name: String,
        mode: String,
        read_only: bool,
        entries: Vec<fresh_core::text_property::TextPropertyEntry>,
        split_id: SplitId,
        show_line_numbers: bool,
        show_cursors: bool,
        editing_disabled: bool,
        line_wrap: Option<bool>,
        initial_cursor_line: Option<u32>,
        request_id: Option<u64>,
    ) {
        // Create the virtual buffer
        let buffer_id =
            self.active_window_mut()
                .create_virtual_buffer(name.clone(), mode.clone(), read_only);
        tracing::info!(
            "Created virtual buffer '{}' with mode '{}' for existing split {:?} (id={:?})",
            name,
            mode,
            split_id,
            buffer_id
        );

        self.configure_vbuf_display(
            buffer_id,
            show_line_numbers,
            show_cursors,
            editing_disabled,
            None,
            true,
        );

        if let Err(e) = self.set_virtual_buffer_content(buffer_id, entries) {
            tracing::error!("Failed to set virtual buffer content: {}", e);
            return;
        }

        // Apply an initial cursor position before the buffer becomes the
        // active buffer in the split. Same rationale as the equivalent
        // path in `handle_create_virtual_buffer_with_content`: a follow-up
        // SetBufferCursor from the plugin would race against user input.
        // Show the buffer in the target split. set_pane_buffer
        // covers the tree + SVS updates the old code did by hand.
        let leaf_id = LeafId(split_id);
        self.windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_manager_mut())
            .expect("active window must have a populated split layout")
            .set_active_split(leaf_id);
        self.active_window_mut().set_pane_buffer(leaf_id, buffer_id);

        // Fall-through to the cursor/open_buffers housekeeping
        // that used to follow the manual switch_buffer. We keep
        // the `if let Some(view_state)` block below — set_pane_buffer
        // already called switch_buffer, but the downstream code
        // also nudges open_buffers and focus_history.
        if let Some(view_state) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.split_view_states_mut())
            .expect("active window must have a populated split layout")
            .get_mut(&leaf_id)
        {
            view_state.switch_buffer(buffer_id);
            view_state.add_buffer(buffer_id);
            view_state.ensure_buffer_state(buffer_id).show_line_numbers = show_line_numbers;

            // Apply line_wrap setting if provided
            if let Some(wrap) = line_wrap {
                view_state.active_state_mut().viewport.line_wrap_enabled = wrap;
            }
        }

        // Apply an initial cursor position after the buffer is mounted in
        // the target split. Done in the same command-processing tick as
        // creation so the cursor is in place before the editor next
        // processes user input — a follow-up SetBufferCursor from the
        // plugin would race against the user. The plugin passes a line
        // index; we resolve it to a byte here using the buffer's content
        // so UTF-8-byte math never touches JS-side string-length
        // semantics.
        if let Some(line) = initial_cursor_line {
            let target_line = line as usize;
            let byte = self
                .windows
                .get_mut(&self.active_window)
                .and_then(|w| w.buffers.get_mut(&buffer_id))
                .map(|s| {
                    let total = s.buffer.len();
                    let mut iter = s.buffer.line_iterator(0, 80);
                    let mut target_byte = 0;
                    for current_line in 0..=target_line {
                        if let Some((line_start, _)) = iter.next_line() {
                            if current_line == target_line {
                                target_byte = line_start;
                                break;
                            }
                        } else {
                            target_byte = total;
                            break;
                        }
                    }
                    target_byte
                })
                .unwrap_or(0);
            // `splits_for_buffer` only walks the main split tree, so a
            // buffer mounted into an inner leaf of a grouped subtree
            // (buffer-group panel) wouldn't be found and the cursor move
            // would silently no-op. Mirror `handle_set_buffer_cursor` and
            // include any matching inner leaves so the cursor lands
            // regardless of where the buffer ended up.
            let mut splits: Vec<LeafId> = self
                .windows
                .get(&self.active_window)
                .and_then(|w| w.buffers.splits())
                .map(|(mgr, _)| mgr)
                .expect("active window must have a populated split layout")
                .splits_for_buffer(buffer_id);
            for node in self.active_window().grouped_subtrees.values() {
                if let crate::view::split::SplitNode::Grouped { layout, .. } = node {
                    for inner_leaf in layout.leaf_split_ids() {
                        if let Some(vs) = self
                            .windows
                            .get(&self.active_window)
                            .and_then(|w| w.buffers.splits())
                            .map(|(_, vs)| vs)
                            .expect("active window must have a populated split layout")
                            .get(&inner_leaf)
                        {
                            if vs.active_buffer == buffer_id && !splits.contains(&inner_leaf) {
                                splits.push(inner_leaf);
                            }
                        }
                    }
                }
            }
            self.active_window_mut()
                .set_buffer_cursor_in_splits(buffer_id, byte, &splits);
        }

        tracing::info!(
            "Displayed virtual buffer {:?} in split {:?}",
            buffer_id,
            split_id
        );

        // Send response with buffer ID and split ID via callback resolution
        if let Some(req_id) = request_id {
            let result = fresh_core::api::VirtualBufferResult {
                buffer_id: buffer_id.0 as u64,
                split_id: Some(split_id.0 as u64),
            };
            self.plugin_manager.read().unwrap().resolve_callback(
                fresh_core::api::JsCallbackId::from(req_id),
                serde_json::to_string(&result).unwrap_or_default(),
            );
        }
    }

    fn handle_show_action_popup(
        &mut self,
        popup_id: String,
        title: String,
        message: String,
        actions: Vec<fresh_core::api::ActionPopupAction>,
        buffer_id: Option<usize>,
    ) {
        tracing::info!(
            "Action popup requested: id={}, title={}, actions={}, buffer_id={:?}",
            popup_id,
            title,
            actions.len(),
            buffer_id,
        );

        // Build popup list items from actions
        let items: Vec<crate::model::event::PopupListItemData> = actions
            .iter()
            .map(|action| crate::model::event::PopupListItemData {
                text: action.label.clone(),
                detail: None,
                icon: None,
                data: Some(action.id.clone()),
            })
            .collect();

        // The popup_id lives on the popup itself via its
        // `PopupResolver::PluginAction` — no side-channel stack.
        // Drop the incoming `actions` vec; its ids are already
        // encoded as each list item's `data` field below.
        drop(actions);

        // Create popup with message + action list
        let popup_data = crate::model::event::PopupData {
            kind: crate::model::event::PopupKindHint::List,
            title: Some(title),
            description: Some(message),
            transient: false,
            content: crate::model::event::PopupContentData::List { items, selected: 0 },
            position: crate::model::event::PopupPositionData::BottomRight,
            width: 60,
            max_height: 15,
            bordered: true,
        };

        // Action popups are buffer-independent notifications; route
        // them to the editor-level popup stack so they remain visible
        // (and dismissible) regardless of which buffer is focused —
        // including virtual buffers like the Dashboard that own the
        // whole split.
        //
        // The resolver carries the popup_id so confirm/cancel fires
        // `action_popup_result` for exactly THIS popup, even when
        // multiple plugin popups are stacked concurrently.
        let (popup_bg, popup_border_fg) = {
            let theme = self.theme();
            (theme.popup_bg, theme.popup_border_fg)
        };
        let mut popup_obj =
            crate::state::convert_popup_data_to_popup(&popup_data, popup_bg, popup_border_fg);
        popup_obj.resolver = crate::view::popup::PopupResolver::PluginAction {
            popup_id: popup_id.clone(),
        };

        // Buffer-scoped popup: a plugin raised this for one specific buffer
        // (e.g. asm-lsp's `.asm-lsp.toml` offer on `after_file_open`), so it
        // belongs on that buffer's popup stack — it then renders only while
        // that buffer is active and is dropped when the buffer closes,
        // instead of floating over every buffer like a global notification.
        // Fall back to the global stack if the buffer has since closed, so
        // the popup is never silently lost.
        if let Some(bid) = buffer_id {
            let bid = BufferId(bid);
            let stack = self
                .windows
                .values_mut()
                .find_map(|w| w.buffers.get_mut(&bid))
                .map(|state| &mut state.popups);
            if let Some(stack) = stack {
                // Dedup by `popup_id` within this buffer's stack — repeated
                // file-open hooks shouldn't pile up duplicate offers.
                let existing_idx = stack.all().iter().position(|p| {
                    matches!(
                        &p.resolver,
                        crate::view::popup::PopupResolver::PluginAction { popup_id: id } if id == &popup_id,
                    )
                });
                match existing_idx.and_then(|idx| stack.get_mut(idx)) {
                    Some(slot) => *slot = popup_obj,
                    None => stack.show(popup_obj),
                }
                tracing::info!("Action popup shown on buffer {:?}: id={}", bid, popup_id,);
                return;
            }
            tracing::warn!(
                "Action popup id={} requested for missing buffer {:?}; showing globally",
                popup_id,
                bid,
            );
        }

        // Dismiss any built-in LSP-status popup that the editor put
        // on `active_state().popups` in response to the same click —
        // the plugin's popup is the contextual answer and stacking
        // ours underneath leaves two popups for one user gesture
        // (#1941 issue 1). Done here (rather than at the
        // `show_lsp_status_popup` call site) because plugin handlers
        // run *asynchronously*: by the time the `ShowActionPopup`
        // command reaches us, the LSP-Servers popup has already
        // landed. Re-run on every plugin push (not just the first
        // dedup'd one) because rapid repeated clicks can re-add the
        // LSP-Servers popup between consecutive plugin commands.
        while self
            .active_state()
            .popups
            .top()
            .is_some_and(|p| matches!(p.resolver, crate::view::popup::PopupResolver::LspStatus))
        {
            self.active_state_mut().popups.hide();
        }

        // Dedup by `popup_id`: if a previous `showActionPopup` with
        // the same id is still on the stack (common: repeated
        // indicator clicks fire `lsp_status_clicked` over and over,
        // each one re-pushing "rust-lsp-help"), replace it in place
        // instead of stacking another copy. Without this, dismissing
        // one reveals the same popup underneath — #1941 issue 4.
        let existing_idx = self.global_popups.all().iter().position(|p| {
            matches!(
                &p.resolver,
                crate::view::popup::PopupResolver::PluginAction { popup_id: id } if id == &popup_id,
            )
        });
        if let Some(idx) = existing_idx {
            if let Some(slot) = self.global_popups.get_mut(idx) {
                *slot = popup_obj;
            }
        } else {
            self.global_popups.show(popup_obj);
        }
        tracing::info!(
            "Action popup shown: id={}, stack_depth={}",
            popup_id,
            self.global_popups.all().len()
        );
    }

    /// Install (or replace, or clear) a plugin's contributions for the
    /// LSP-Servers popup. Passing an empty `items` removes any
    /// previous contribution from this `plugin_id` for this
    /// `language`. Mirrors the editor-side half of
    /// `PluginCommand::SetLspMenuContributions`.
    ///
    /// If the LSP-Servers popup is currently open for this language,
    /// refresh it in place so the new rows show up immediately
    /// rather than only on the next click.
    fn handle_set_lsp_menu_contributions(
        &mut self,
        plugin_id: String,
        language: String,
        items: Vec<fresh_core::api::LspMenuItem>,
    ) {
        let key = (language.clone(), plugin_id.clone());
        if items.is_empty() {
            self.active_window_mut().lsp_menu_contributions.remove(&key);
        } else {
            self.active_window_mut()
                .lsp_menu_contributions
                .insert(key, items);
        }
        // If the popup is on screen right now, re-render it so the
        // change is immediately visible — the alternative is "next
        // click sees it" which feels unresponsive when the plugin
        // is reacting to an event the user just triggered.
        self.refresh_lsp_status_popup_if_open();
    }

    fn handle_send_omp_companion_command(
        &mut self,
        terminal_id: fresh_core::WindowTerminalId,
        command_type: fresh_core::api::OmpCompanionCommandType,
        target: fresh_core::api::OmpCompanionCommandTargetV1,
        request_id: u64,
        context: &PluginCommandContext,
    ) {
        if !self.context_may_use_privileged_terminal_options(context) {
            tracing::warn!(
                plugin = %context.plugin_name,
                "rejected OMP companion control from an untrusted plugin"
            );
            self.resolve_bool_for_context(context, request_id, false);
            return;
        }
        let sent = self.windows.get(&terminal_id.window).is_some_and(|window| {
            window.terminal_companions.get(&terminal_id.terminal)
                == Some(&fresh_core::api::TerminalCompanion::Omp)
                && window
                    .terminal_manager
                    .get(terminal_id.terminal)
                    .is_some_and(|handle| {
                        handle.is_alive()
                            && handle.companion_kind()
                                == Some(fresh_core::api::TerminalCompanion::Omp)
                            && handle.enqueue_omp_companion_command(command_type, &target)
                    })
        });
        self.resolve_bool_for_context(context, request_id, sent);
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_create_window_with_terminal(
        &mut self,
        root: std::path::PathBuf,
        label: String,
        cwd: Option<String>,
        command: Option<Vec<String>>,
        relaunch: Option<Vec<String>>,
        title: Option<String>,
        resume: Option<Vec<String>>,
        env: Option<std::collections::HashMap<String, String>>,
        allow_script: bool,
        companion: Option<fresh_core::api::TerminalCompanion>,
        selected_agent: bool,
        activate: bool,
        initial_state: std::collections::HashMap<String, serde_json::Value>,
        request_id: u64,
        context: &PluginCommandContext,
    ) {
        let callback_id = JsCallbackId::from(request_id);
        if (allow_script || companion.is_some() || selected_agent)
            && !self.context_may_use_privileged_terminal_options(context)
        {
            self.reject_callback_for_context(
                context,
                callback_id,
                "createWindowWithTerminal: privileged terminal options require the trusted bundled Orchestrator"
                    .to_string(),
            );
            return;
        }
        if !root.is_absolute() {
            let error = format!(
                "createWindowWithTerminal: root must be absolute, got {:?}",
                root
            );
            tracing::warn!("{error}");
            self.reject_callback_for_context(context, callback_id, error);
            return;
        }

        let cwd = cwd.map(std::path::PathBuf::from);
        let authority = self.local_session_authority(&root);
        match self.create_window_with_terminal(
            root,
            label,
            cwd,
            command,
            relaunch,
            title,
            authority,
            resume,
            env,
            allow_script,
            companion,
            selected_agent,
            activate,
            Some((context.plugin_name.to_string(), initial_state)),
        ) {
            Ok((window_id, terminal_id, buffer_id)) => {
                let result = fresh_core::api::SessionWithTerminalResult {
                    window_id: window_id.0,
                    stable_id: self
                        .windows
                        .get(&window_id)
                        .map(|window| window.stable_id.clone())
                        .unwrap_or_default(),
                    terminal_id: fresh_core::WindowTerminalId::new(window_id, terminal_id),
                    buffer_id: buffer_id.0 as u64,
                };
                self.send_plugin_response(
                    fresh_core::api::PluginResponse::WindowWithTerminalCreated {
                        request_id,
                        result,
                    },
                );
            }
            Err(error) => {
                tracing::error!("createWindowWithTerminal failed: {error}");
                self.reject_callback_for_context(
                    context,
                    callback_id,
                    format!("createWindowWithTerminal: {error}"),
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_create_terminal(
        &mut self,
        cwd: Option<String>,
        direction: Option<String>,
        ratio: Option<f32>,
        focus: Option<bool>,
        persistent: bool,
        target_id: fresh_core::WindowId,
        command: Option<Vec<String>>,
        relaunch: Option<Vec<String>>,
        title: Option<String>,
        resume: Option<Vec<String>>,
        env: Option<std::collections::HashMap<String, String>>,
        companion: Option<fresh_core::api::TerminalCompanion>,
        allow_script: bool,
        selected_agent: bool,
        request_id: u64,
        context: &PluginCommandContext,
    ) {
        let callback_id = fresh_core::api::JsCallbackId::from(request_id);
        if (allow_script || companion.is_some() || selected_agent)
            && !self.context_may_use_privileged_terminal_options(context)
        {
            self.reject_callback_for_context(
                context,
                callback_id,
                "createTerminal: privileged terminal options require the trusted bundled Orchestrator"
                    .to_string(),
            );
            return;
        }
        // Host-internal commands are intentionally not loader-scoped. Every
        // runtime plugin envelope must still target the exact source window.
        if context.provenance != fresh_core::api::PluginLoadProvenance::Internal
            && context
                .source_window
                .is_some_and(|source_window| source_window != target_id)
        {
            self.reject_callback_for_context(
                context,
                callback_id,
                "createTerminal: target window does not match its command context".to_string(),
            );
            return;
        }
        if !self.windows.contains_key(&target_id) {
            self.reject_callback_for_context(
                context,
                callback_id,
                format!(
                    "createTerminal: window {} is no longer available",
                    target_id.0
                ),
            );
            return;
        }

        let terminal_env =
            match crate::app::terminal::agent_command_env(target_id, env, allow_script) {
                Ok(env) => env,
                Err(error) => {
                    self.reject_callback_for_context(
                        context,
                        callback_id,
                        format!("createTerminal: {error}"),
                    );
                    return;
                }
            };
        let is_active_target = target_id == self.active_window;
        let previous_active_buffer = is_active_target.then(|| self.active_window().active_buffer());
        let direction = direction.as_deref().map(|direction| match direction {
            "horizontal" => crate::model::event::SplitDirection::Horizontal,
            _ => crate::model::event::SplitDirection::Vertical,
        });
        let restore_command = relaunch.or_else(|| command.clone());
        let result = self
            .windows
            .get_mut(&target_id)
            .expect("target window existence checked above")
            .create_plugin_terminal(crate::app::terminal::PluginTerminalSpec {
                cwd: cwd.map(std::path::PathBuf::from),
                direction,
                ratio,
                focus: focus.unwrap_or(true),
                persistent,
                command,
                title: title.filter(|title| !title.is_empty()),
                env: terminal_env.vars.clone(),
                companion,
                script_capability: terminal_env.script_capability(),
            });

        match result {
            Ok((terminal_id, buffer_id, created_split_id)) => {
                let new_active_buffer = {
                    let target = self
                        .windows
                        .get_mut(&target_id)
                        .expect("spawned terminal's window must still exist");
                    target.mark_terminal_restorable(terminal_id, restore_command, resume);
                    target.record_terminal_script_token(
                        terminal_id,
                        terminal_env.script_token.as_deref(),
                    );
                    if selected_agent {
                        target.tracked_agent_terminal = Some(terminal_id);
                    }
                    if let Some(pid) = target
                        .terminal_manager
                        .get(terminal_id)
                        .and_then(|handle| handle.pid())
                    {
                        target
                            .process_groups
                            .register(pid, format!("terminal #{}", terminal_id.0));
                    }
                    target.active_buffer()
                };
                if let Err(error) = self.save_workspace_for(target_id) {
                    terminal_env.revoke();
                    self.handle_close_terminal(fresh_core::WindowTerminalId::new(
                        target_id,
                        terminal_id,
                    ));
                    self.reject_callback_for_context(
                        context,
                        callback_id,
                        format!("Failed to publish created terminal: {error}"),
                    );
                    return;
                }
                if is_active_target && previous_active_buffer != Some(new_active_buffer) {
                    #[cfg(feature = "plugins")]
                    self.update_plugin_state_snapshot();
                    #[cfg(feature = "plugins")]
                    self.plugin_manager.read().unwrap().run_hook(
                        "buffer_activated",
                        crate::services::plugins::hooks::HookArgs::BufferActivated {
                            buffer_id: new_active_buffer,
                        },
                    );
                }
                self.send_plugin_response(fresh_core::api::PluginResponse::TerminalCreated {
                    request_id,
                    buffer_id,
                    terminal_id: fresh_core::WindowTerminalId::new(target_id, terminal_id),
                    split_id: created_split_id.map(|split| split.0),
                });
            }
            Err(error) => {
                terminal_env.revoke();
                self.reject_callback_for_context(
                    context,
                    callback_id,
                    format!("Failed to create terminal: {error}"),
                );
            }
        }
    }

    // ==================== Extracted handlers for previously inline match arms ====================

    fn handle_get_split_by_label(&mut self, label: String, request_id: u64) {
        let split_id = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .map(|(mgr, _)| mgr)
            .expect("active window must have a populated split layout")
            .find_split_by_label(&label);
        let callback_id = fresh_core::api::JsCallbackId::from(request_id);
        let json =
            serde_json::to_string(&split_id.map(|s| s.0 .0)).unwrap_or_else(|_| "null".to_string());
        self.plugin_manager
            .read()
            .unwrap()
            .resolve_callback(callback_id, json);
    }

    fn handle_set_buffer_show_cursors(&mut self, buffer_id: BufferId, show: bool) {
        if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .map(|w| &mut w.buffers)
            .expect("active window present")
            .get_mut(&buffer_id)
        {
            state.show_cursors = show;
            // The plugin now owns this buffer's cursor visibility; stop
            // the widget runtime from overriding it on every repaint.
            state.cursor_visibility_locked = true;
        } else {
            tracing::warn!("SetBufferShowCursors: buffer {:?} not found", buffer_id);
        }
    }

    fn handle_override_theme_colors(
        &mut self,
        overrides: std::collections::HashMap<String, [u8; 3]>,
    ) {
        let pairs = overrides
            .into_iter()
            .map(|(k, [r, g, b])| (k, ratatui::style::Color::Rgb(r, g, b)));
        let applied = self.theme.write().unwrap().override_colors(pairs);
        if applied > 0 {
            // Diagnostics / semantic overlays bake RGB at creation time — rebuild
            // them so the override is visible everywhere on the next frame.
            self.reapply_all_overlays();
        }
    }

    fn handle_await_next_key(&mut self, callback_id: fresh_core::api::JsCallbackId) {
        if let Some(payload) = self
            .active_window_mut()
            .pending_key_capture_buffer
            .pop_front()
        {
            let json = serde_json::to_string(&payload).unwrap_or_else(|_| "null".to_string());
            self.plugin_manager
                .read()
                .unwrap()
                .resolve_callback(callback_id, json);
        } else {
            self.active_window_mut()
                .pending_next_key_callbacks
                .push_back(callback_id);
        }
    }

    fn handle_spawn_process(
        &mut self,
        window_id: fresh_core::WindowId,
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        stdout_to: Option<std::path::PathBuf>,
        callback_id: fresh_core::api::JsCallbackId,
    ) {
        let Some(window) = self.windows.get(&window_id) else {
            self.plugin_manager.read().unwrap().reject_callback(
                callback_id,
                format!(
                    "spawnProcess: window {} is no longer available",
                    window_id.0
                ),
            );
            return;
        };
        let effective_cwd = cwd.or_else(|| Some(window.root.to_string_lossy().into_owned()));
        let spawner = std::sync::Arc::clone(&window.authority().process_spawner);
        if let (Some(runtime), Some(bridge)) = (&self.tokio_runtime, &self.async_bridge) {
            let sender = bridge.sender();
            let process_id = callback_id.as_u64();
            let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
            self.host_process_handles
                .insert(process_id, (window_id, kill_tx));

            runtime.spawn(async move {
                let outcome = spawner
                    .spawn_cancellable(command, args, effective_cwd, stdout_to, kill_rx)
                    .await;
                let (stdout, stderr, exit_code) = match outcome {
                    Ok(result) => (result.stdout, result.stderr, result.exit_code),
                    Err(error) => (String::new(), error.to_string(), -1),
                };
                #[allow(clippy::let_underscore_must_use)]
                let _ = sender.send(AsyncMessage::PluginProcessOutput {
                    process_id,
                    stdout,
                    stderr,
                    exit_code,
                });
            });
        } else {
            self.plugin_manager
                .read()
                .unwrap()
                .reject_callback(callback_id, "Async runtime not available".to_string());
        }
    }

    fn handle_kill_host_process(&mut self, window_id: fresh_core::WindowId, process_id: u64) {
        match self
            .host_process_handles
            .get(&process_id)
            .map(|(owner, _)| *owner)
        {
            Some(owner) if owner == window_id => {
                let (_, tx) = self
                    .host_process_handles
                    .remove(&process_id)
                    .expect("owner checked above");
                #[allow(clippy::let_underscore_must_use)]
                let _ = tx.send(());
                tracing::debug!(?window_id, process_id, "sent process kill");
            }
            Some(owner) => tracing::warn!(
                ?window_id,
                ?owner,
                process_id,
                "refused cross-window process kill"
            ),
            None => tracing::debug!(
                ?window_id,
                process_id,
                "process already exited or was unknown"
            ),
        }
    }

    fn handle_set_authority(
        &mut self,
        window_id: fresh_core::WindowId,
        payload: serde_json::Value,
    ) {
        let Some(window) = self.windows.get(&window_id) else {
            tracing::warn!(?window_id, "SetAuthority targeted a closed window");
            return;
        };
        let trust = std::sync::Arc::clone(&window.authority().workspace_trust);
        let env = std::sync::Arc::clone(&window.authority().env_provider);
        let parsed =
            match serde_json::from_value::<crate::services::authority::AuthorityPayload>(payload) {
                Ok(parsed) => parsed,
                Err(error) => {
                    tracing::warn!(?window_id, "setAuthority: failed to parse payload: {error}");
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        window.status_message = Some(format!("setAuthority rejected: {error}"));
                    }
                    return;
                }
            };
        let spec = crate::services::authority::SessionAuthoritySpec::Plugin(parsed.clone());
        match crate::services::authority::Authority::from_plugin_payload(parsed, trust, env) {
            Ok(authority) => {
                tracing::info!(
                    ?window_id,
                    "Plugin installed new authority; restarting editor"
                );
                self.session_keepalives.remove(&window_id);
                self.set_session_authority_spec(window_id, spec);
                // Route the destructive authority restart through the exact
                // addressed window. The old editor remains wholly on its old
                // authority until it is dropped; no live resource is hot-swapped.
                self.switch_active_window_pointer(window_id);
                self.install_authority(authority);
            }
            Err(error) => {
                tracing::warn!(?window_id, "setAuthority: invalid payload: {error}");
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.status_message = Some(format!("setAuthority rejected: {error}"));
                }
            }
        }
    }

    /// If the just-activated window is a **dormant remote** session — restored
    /// from disk so its backend spec is remote but its live authority is still
    /// the local placeholder (no live keepalive) — start reconnecting it. SSH /
    /// Kubernetes reconnect from core via [`Self::start_remote_connect`]; a
    /// container (`Plugin`) session needs its owning plugin to re-attach (only
    /// the devcontainer plugin can run `devcontainer up`), left to a follow-up.
    /// Idempotent: a reconnect already in flight for this window is a no-op.
    pub(crate) fn reconnect_dormant_session_if_needed(&mut self, window_id: fresh_core::WindowId) {
        // The *activate* path (diving into a session). A live session already
        // holds its connection (keepalive), so leave it alone here — switching
        // to an already-connected window must not tear down and rebuild it. A
        // local session has nothing to reconnect.
        if self.session_keepalives.contains_key(&window_id) {
            return;
        }
        // A session still descriptor-backed in `dormant_remote` (its window,
        // when present, is the disconnected shell a failed connect built)
        // connects through the dive gate (`bring_dormant_remote_online`) or
        // the indicator's explicit Retry. Re-firing here would start a second
        // connect on the very activation that surfaces a failure.
        if self.dormant_remote.contains_key(&window_id) {
            return;
        }
        self.start_remote_reconnect(window_id);
    }

    /// Reconnect a session's remote backend on *explicit user request* — the
    /// remote status indicator's Reconnect / Retry action.
    ///
    /// Unlike [`Self::reconnect_dormant_session_if_needed`] (the activate path,
    /// which no-ops while a keepalive is parked) this forces the connect even
    /// for a *live* remote `Window` that lost its carrier: such a window keeps
    /// its now-stale keepalive and `Window`, so the activate-path guard would
    /// wrongly skip it. On success the `RemoteAttachReady::Reconnect` handler
    /// re-points the window's authority, replaces the stale keepalive, and
    /// respawns its dead embedded terminals (see `async_dispatch.rs`).
    pub(crate) fn force_reconnect_remote_session(&mut self, window_id: fresh_core::WindowId) {
        self.start_remote_reconnect(window_id);
    }

    /// Shared body of the two reconnect entry points: read the window's backend
    /// spec and, for a remote-agent session, kick off the async connect tagged
    /// with this window so its result re-points *this* window.
    fn start_remote_reconnect(&mut self, window_id: fresh_core::WindowId) {
        if self.remote_reconnect_inflight(window_id) {
            return;
        }
        let Some(spec) = self
            .windows
            .get(&window_id)
            .map(|window| window.authority_spec.clone())
        else {
            return;
        };
        match spec {
            crate::services::authority::SessionAuthoritySpec::Local => {}
            crate::services::authority::SessionAuthoritySpec::RemoteAgent(agent_spec) => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.remote_reconnect_error = None;
                }
                self.start_remote_connect(
                    agent_spec,
                    crate::app::RemoteAttachOwner::Reconnect { window_id },
                    true,
                    None,
                );
            }
            crate::services::authority::SessionAuthoritySpec::Plugin(_) => {
                // Container: only the owning plugin can rebuild the backend
                // (`devcontainer up`). TODO(per-session): fire a
                // `session_reattach_requested` hook so it can.
                tracing::debug!("remote session {window_id}: reattach is plugin-driven (TODO)");
            }
        }
    }

    fn handle_attach_remote_agent(
        &mut self,
        plugin_instance_id: fresh_core::api::PluginInstanceId,
        plugin_name: String,
        window_id: fresh_core::WindowId,
        payload: serde_json::Value,
        request_id: u64,
    ) {
        if !self.windows.contains_key(&window_id) {
            self.reject_remote_attach(
                plugin_instance_id,
                request_id,
                "target window closed".to_string(),
            );
            return;
        }
        // Activation and initial plugin state belong to this attach operation,
        // not to the persisted backend reconnect spec.
        let activate = payload
            .get("activate")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let initial_state = match payload.get("initialState") {
            None => None,
            Some(serde_json::Value::Object(values)) => Some((
                plugin_name,
                values
                    .clone()
                    .into_iter()
                    .collect::<std::collections::HashMap<_, _>>(),
            )),
            Some(_) => {
                self.reject_remote_attach(
                    plugin_instance_id,
                    request_id,
                    "initialState must be an object".to_string(),
                );
                return;
            }
        };
        // Opaque at the fresh-core boundary; the concrete schema lives in
        // services::authority so core stays backend-agnostic.
        let spec =
            match serde_json::from_value::<crate::services::authority::RemoteAgentSpec>(payload) {
                Ok(spec) => spec,
                Err(error) => {
                    tracing::warn!("attachRemoteAgent: invalid payload: {error}");
                    self.reject_remote_attach(
                        plugin_instance_id,
                        request_id,
                        format!("invalid attach spec: {error}"),
                    );
                    return;
                }
            };
        self.start_remote_connect(
            spec,
            crate::app::RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                window_id,
            },
            activate,
            initial_state,
        );
    }

    /// Spawn one async remote connection attempt for an exact plugin request
    /// or reconnecting window. Host attempt ids are independent of JS callback
    /// ids, so equal numeric ids in separate plugin runtimes cannot collide.
    pub(crate) fn start_remote_connect(
        &mut self,
        spec: crate::services::authority::RemoteAgentSpec,
        owner: crate::app::RemoteAttachOwner,
        activate: bool,
        initial_state: Option<(String, std::collections::HashMap<String, serde_json::Value>)>,
    ) {
        let owner_window = match owner {
            crate::app::RemoteAttachOwner::Plugin { window_id, .. }
            | crate::app::RemoteAttachOwner::Reconnect { window_id }
            | crate::app::RemoteAttachOwner::Switch { window_id } => window_id,
        };
        if matches!(owner, crate::app::RemoteAttachOwner::Plugin { .. })
            && !self.windows.contains_key(&owner_window)
        {
            if let crate::app::RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                ..
            } = owner
            {
                self.reject_remote_attach(
                    plugin_instance_id,
                    request_id,
                    "target window closed".to_string(),
                );
            }
            return;
        }

        let runtime = self.tokio_runtime.clone();
        let sender = self.async_bridge.as_ref().map(|bridge| bridge.sender());
        let (Some(runtime), Some(sender)) = (runtime, sender) else {
            let error = "async runtime not available".to_string();
            match owner {
                crate::app::RemoteAttachOwner::Plugin {
                    plugin_instance_id,
                    request_id,
                    ..
                } => self.reject_remote_attach(plugin_instance_id, request_id, error),
                crate::app::RemoteAttachOwner::Reconnect { window_id } => {
                    if self.dormant_remote.contains_key(&window_id)
                        && !self.windows.contains_key(&window_id)
                    {
                        self.ensure_dormant_shell(window_id);
                    }
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        window.remote_reconnect_error = Some(error.clone());
                        window.set_status_message(format!("Connection failed: {error}"));
                    }
                }
                crate::app::RemoteAttachOwner::Switch { window_id } => {
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        window.set_status_message(format!("Project switch failed: {error}"));
                    }
                }
            }
            return;
        };
        let Some(attempt_id) = self.begin_remote_attach_attempt(owner) else {
            return;
        };
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        self.remote_attach_cancels.insert(attempt_id, cancel_tx);

        let window_mode = spec.window;
        let window_label = spec.label.clone();
        let window_command = spec.command.clone();
        let window_initial_state = initial_state.clone();
        let mode_for = |label: &str| match owner {
            crate::app::RemoteAttachOwner::Reconnect { window_id } => {
                crate::services::async_bridge::RemoteAttachMode::Reconnect { window_id }
            }
            crate::app::RemoteAttachOwner::Switch { window_id } => {
                crate::services::async_bridge::RemoteAttachMode::Switch { window_id }
            }
            crate::app::RemoteAttachOwner::Plugin { .. } if window_mode => {
                crate::services::async_bridge::RemoteAttachMode::Window {
                    label: window_label.clone().unwrap_or_else(|| label.to_string()),
                    command: window_command.clone(),
                    activate,
                    initial_state: window_initial_state.clone(),
                }
            }
            crate::app::RemoteAttachOwner::Plugin { .. } => {
                crate::services::async_bridge::RemoteAttachMode::Restart
            }
        };

        use crate::services::authority::RemoteTransportSpec;
        if spec.has_identity_claim() && spec.verified_identity().is_none() {
            #[allow(clippy::let_underscore_must_use)]
            let _ = sender.send(AsyncMessage::RemoteAttachFailed {
                error: "saved remote tenant identity is incomplete or invalid".to_string(),
                attempt_id,
            });
            return;
        }
        match spec.transport.clone() {
            RemoteTransportSpec::KubectlExec { .. } => {
                let expected_identity = spec.verified_identity();
                let (target, base_env) = spec.clone().into_kube_target();
                let label = target.display();
                let mode = mode_for(&label);
                let dir_context = self.dir_context.clone();
                let expected_spec = spec;
                if let Some(window) = self.windows.get_mut(&owner_window) {
                    window.set_status_message(format!("Connecting to {label}…"));
                }
                runtime.spawn(async move {
                    let outcome = crate::services::authority::connect_kube_authority(
                        target,
                        base_env,
                        expected_identity,
                        dir_context,
                        Some(cancel_rx),
                    )
                    .await;
                    let message = match outcome {
                        Ok((mut authority, keepalive, identity)) => {
                            let restore_allowed = expected_spec.identity_matches(&identity);
                            let mut verified_spec = expected_spec;
                            verified_spec.set_verified_identity(&identity);
                            authority.set_remote_session_spec(verified_spec.clone());
                            AsyncMessage::RemoteAttachReady(
                                crate::services::async_bridge::RemoteAttachReady {
                                    authority,
                                    keepalive: Box::new(keepalive),
                                    working_dir: Some(identity.canonical_root),
                                    mode,
                                    spec: crate::services::authority::SessionAuthoritySpec::RemoteAgent(
                                        verified_spec,
                                    ),
                                    restore_allowed,
                                    attempt_id,
                                },
                            )
                        }
                        Err(error) => AsyncMessage::RemoteAttachFailed {
                            error: error.to_string(),
                            attempt_id,
                        },
                    };
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = sender.send(message);
                });
            }
            RemoteTransportSpec::Ssh {
                user,
                host,
                port,
                identity_file,
                remote_path,
                extra_args,
            } => {
                let expected_identity = spec.verified_identity();
                let params = crate::services::remote::ConnectionParams {
                    user: user.filter(|value| !value.is_empty()),
                    host,
                    port,
                    identity_file: identity_file.map(std::path::PathBuf::from),
                    extra_args,
                };
                let target = params.ssh_target();
                let label = match port {
                    Some(port) => format!("ssh:{target}:{port}"),
                    None => format!("ssh:{target}"),
                };
                let mode = mode_for(&label);
                let dir_context = self.dir_context.clone();
                let expected_spec = spec;
                if let Some(window) = self.windows.get_mut(&owner_window) {
                    window.set_status_message(format!("Connecting to {label}…"));
                }
                runtime.spawn(async move {
                    let outcome = crate::services::authority::connect_ssh_authority(
                        params,
                        remote_path,
                        expected_identity,
                        dir_context,
                        Some(cancel_rx),
                    )
                    .await;
                    let message = match outcome {
                        Ok((mut authority, keepalive, identity)) => {
                            let restore_allowed = expected_spec.identity_matches(&identity);
                            let mut verified_spec = expected_spec;
                            verified_spec.set_verified_identity(&identity);
                            authority.set_remote_session_spec(verified_spec.clone());
                            AsyncMessage::RemoteAttachReady(
                                crate::services::async_bridge::RemoteAttachReady {
                                    authority,
                                    keepalive: Box::new(keepalive),
                                    working_dir: Some(identity.canonical_root),
                                    mode,
                                    spec: crate::services::authority::SessionAuthoritySpec::RemoteAgent(
                                        verified_spec,
                                    ),
                                    restore_allowed,
                                    attempt_id,
                                },
                            )
                        }
                        Err(error) => AsyncMessage::RemoteAttachFailed {
                            error: error.to_string(),
                            attempt_id,
                        },
                    };
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = sender.send(message);
                });
            }
        }
    }

    fn handle_set_remote_indicator_state(
        &mut self,
        window_id: fresh_core::WindowId,
        state: serde_json::Value,
    ) {
        let parsed =
            serde_json::from_value::<crate::view::ui::status_bar::RemoteIndicatorOverride>(state);
        match (self.windows.get_mut(&window_id), parsed) {
            (Some(window), Ok(override_state)) => {
                window.remote_indicator_override = Some(override_state);
            }
            (Some(window), Err(error)) => {
                tracing::warn!(
                    ?window_id,
                    "setRemoteIndicatorState: invalid payload: {error}"
                );
                window.status_message = Some(format!("setRemoteIndicatorState rejected: {error}"));
            }
            (None, _) => tracing::warn!(
                ?window_id,
                "SetRemoteIndicatorState targeted a closed window"
            ),
        }
    }

    fn handle_spawn_process_wait(
        &mut self,
        window_id: fresh_core::WindowId,
        process_id: u64,
        callback_id: fresh_core::api::JsCallbackId,
    ) {
        tracing::warn!(
            ?window_id,
            process_id,
            "SpawnProcessWait is not implemented"
        );
        self.plugin_manager.read().unwrap().reject_callback(
            callback_id,
            format!(
                "SpawnProcessWait is not implemented for window {} process {}",
                window_id.0, process_id
            ),
        );
    }

    fn handle_delay(&mut self, callback_id: fresh_core::api::JsCallbackId, duration_ms: u64) {
        if let (Some(runtime), Some(bridge)) = (&self.tokio_runtime, &self.async_bridge) {
            let sender = bridge.sender();
            let callback_id_u64 = callback_id.as_u64();
            runtime.spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(duration_ms)).await;
                #[allow(clippy::let_underscore_must_use)]
                let _ = sender.send(crate::services::async_bridge::AsyncMessage::Plugin(
                    fresh_core::api::PluginAsyncMessage::DelayComplete {
                        callback_id: callback_id_u64,
                    },
                ));
            });
        } else {
            std::thread::sleep(std::time::Duration::from_millis(duration_ms));
            self.plugin_manager
                .read()
                .unwrap()
                .resolve_callback(callback_id, "null".to_string());
        }
    }

    fn handle_http_fetch(
        &mut self,
        url: String,
        target_path: std::path::PathBuf,
        callback_id: fresh_core::api::JsCallbackId,
    ) {
        if let (Some(runtime), Some(bridge)) = (&self.tokio_runtime, &self.async_bridge) {
            let sender = bridge.sender();
            let process_id = callback_id.as_u64();

            runtime.spawn(async move {
                let fetch = tokio::task::spawn_blocking(move || {
                    crate::services::http::download_to_file(&url, &target_path)
                })
                .await;

                let (stdout, stderr, exit_code) = match fetch {
                    Ok(Ok(status)) => {
                        if (200..300).contains(&status) {
                            (String::new(), String::new(), 0)
                        } else {
                            (String::new(), format!("HTTP {}", status), i32::from(status))
                        }
                    }
                    Ok(Err(e)) => (String::new(), e, -1),
                    Err(e) => (String::new(), format!("fetch task failed: {}", e), -1),
                };

                #[allow(clippy::let_underscore_must_use)]
                let _ = sender.send(AsyncMessage::PluginProcessOutput {
                    process_id,
                    stdout,
                    stderr,
                    exit_code,
                });
            });
        } else {
            self.plugin_manager
                .read()
                .unwrap()
                .reject_callback(callback_id, "Async runtime not available".to_string());
        }
    }

    fn handle_kill_background_process(&mut self, window_id: fresh_core::WindowId, process_id: u64) {
        match self
            .background_process_handles
            .get(&process_id)
            .map(|(owner, _)| *owner)
        {
            Some(owner) if owner == window_id => {
                let (_, handle) = self
                    .background_process_handles
                    .remove(&process_id)
                    .expect("owner checked above");
                handle.abort();
                tracing::debug!(?window_id, process_id, "killed background process");
            }
            Some(owner) => tracing::warn!(
                ?window_id,
                ?owner,
                process_id,
                "refused cross-window background process kill"
            ),
            None => tracing::debug!(
                ?window_id,
                process_id,
                "background process already exited or was unknown"
            ),
        }
    }

    fn handle_create_virtual_buffer(&mut self, name: String, mode: String, read_only: bool) {
        let buffer_id =
            self.active_window_mut()
                .create_virtual_buffer(name.clone(), mode.clone(), read_only);
        tracing::info!(
            "Created virtual buffer '{}' with mode '{}' (id={:?})",
            name,
            mode,
            buffer_id
        );
        // TODO: Return buffer_id to plugin via callback or hook
    }

    fn handle_set_virtual_buffer_content(
        &mut self,
        buffer_id: BufferId,
        entries: Vec<fresh_core::text_property::TextPropertyEntry>,
    ) {
        match self.set_virtual_buffer_content(buffer_id, entries) {
            Ok(()) => {
                tracing::debug!("Set virtual buffer content for {:?}", buffer_id);
            }
            Err(e) => {
                tracing::error!("Failed to set virtual buffer content: {}", e);
            }
        }
    }

    fn handle_mount_widget_panel(
        &mut self,
        panel_key: crate::widgets::PanelKey,
        buffer_id: BufferId,
        spec: fresh_core::api::WidgetSpec,
    ) {
        // Mount = clean slate. Instance state and focus key reset
        // so a plugin that re-mounts (e.g. reopening a panel with
        // a fresh prefill) sees its spec values take effect. To
        // *preserve* state across renders, the plugin uses Update.
        let prev = std::collections::HashMap::new();
        let prev_focus = String::new();
        let panel_width = self.widget_panel_width(buffer_id);
        let out = self.render_panel_spec(&spec, &prev, &prev_focus, panel_width);
        let focus_cursor = out.focus_cursor;
        self.widget_registry.mount(
            panel_key.clone(),
            buffer_id,
            spec,
            out.hits,
            out.instance_states,
            out.focus_key,
            out.tabbable,
            out.scroll_regions,
        );
        // Mark the buffer as hosting an interactive widget panel so the
        // focus/click paths keep routing focus to it even when it opts out
        // of buffer scrolling (a non-scrollable widget panel is still an
        // interactive target, unlike a fixed buffer-group toolbar).
        if let Some(state) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.buffers.get_mut(&buffer_id))
        {
            state.interactive_widget_panel = true;
        }
        let entries = out.entries;
        if let Err(e) = self.set_virtual_buffer_content(buffer_id, entries.clone()) {
            tracing::error!(
                "Failed to render mounted widget panel {} into {:?}: {}",
                panel_key,
                buffer_id,
                e
            );
        } else {
            tracing::debug!(
                "Mounted widget panel {} into buffer {:?}",
                panel_key,
                buffer_id
            );
        }
        self.apply_widget_focus_cursor(buffer_id, &entries, focus_cursor);
    }

    fn handle_update_widget_panel(
        &mut self,
        panel_key: &crate::widgets::PanelKey,
        spec: fresh_core::api::WidgetSpec,
    ) {
        let prev = match self.widget_registry.instance_states(panel_key) {
            Some(s) => s.clone(),
            None => {
                tracing::debug!(
                    "UpdateWidgetPanel for unknown panel {} ignored (not mounted)",
                    panel_key
                );
                return;
            }
        };
        let prev_focus = self
            .widget_registry
            .focus_key(panel_key)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let buffer_id_for_width = self
            .widget_registry
            .buffer_and_spec(panel_key)
            .map(|(b, _)| b)
            .unwrap_or(BufferId(0));
        let panel_width = self.widget_panel_width(buffer_id_for_width);
        let out = self.render_panel_spec(&spec, &prev, &prev_focus, panel_width);
        let focus_cursor = out.focus_cursor;
        let entries = out.entries;
        match self.widget_registry.update(
            panel_key,
            spec,
            out.hits,
            out.instance_states,
            out.focus_key,
            out.tabbable,
            out.scroll_regions,
        ) {
            Ok(buffer_id) => {
                if let Err(e) = self.set_virtual_buffer_content(buffer_id, entries.clone()) {
                    tracing::error!("Failed to render updated widget panel {}: {}", panel_key, e);
                }
                self.apply_widget_focus_cursor(buffer_id, &entries, focus_cursor);
            }
            Err(()) => {
                tracing::debug!(
                    "UpdateWidgetPanel for unknown panel {} ignored (not mounted)",
                    panel_key
                );
            }
        }
    }

    /// Apply a `WidgetMutation` in place, then re-render the panel.
    /// This is the IPC fast path: the plugin doesn't re-transmit
    /// the full spec; it sends one targeted change. The host
    /// mutates the registry's spec / instance state and re-renders
    /// against the just-mutated state.
    fn handle_widget_mutate(
        &mut self,
        panel_key: &crate::widgets::PanelKey,
        mutation: fresh_core::api::WidgetMutation,
    ) {
        use fresh_core::api::WidgetMutation;

        // Look up the panel; bail if unknown.
        if self.widget_registry.get(panel_key).is_none() {
            tracing::debug!(
                "WidgetMutate for unknown panel {} ignored (not mounted)",
                panel_key
            );
            return;
        }

        match mutation {
            WidgetMutation::SetValue {
                widget_key,
                value,
                cursor_byte,
            } => {
                // Value+cursor live in instance state for the unified
                // Text widget. Preserve `scroll` and `multiline` from
                // the previous editor across the mutation so
                // multi-line viewport offsets don't snap on a
                // plugin-driven update; the renderer re-clamps next
                // render anyway.
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    // Preserve `scroll` + `multiline` so plugin-
                    // driven SetValue doesn't snap the viewport,
                    // and preserve `completions` /
                    // `completion_selected_index` so the popup
                    // (if open) doesn't disappear on a value
                    // mutation that happens to land while the
                    // user is mid-keystroke.
                    let (scroll, multiline, completions, sel_idx, scroll_off, navigated) =
                        match panel.instance_states.get(&widget_key) {
                            Some(crate::widgets::WidgetInstanceState::Text {
                                editor,
                                scroll,
                                completions,
                                completion_selected_index,
                                completion_scroll_offset,
                                completion_navigated,
                                ..
                            }) => (
                                *scroll,
                                editor.multiline,
                                completions.clone(),
                                *completion_selected_index,
                                *completion_scroll_offset,
                                *completion_navigated,
                            ),
                            _ => (0u32, true, Vec::new(), 0usize, 0u32, false),
                        };
                    let mut editor = if multiline {
                        crate::primitives::text_edit::TextEdit::with_text(&value)
                    } else {
                        crate::primitives::text_edit::TextEdit::single_line_with_text(&value)
                    };
                    let target = match cursor_byte {
                        Some(c) if c >= 0 => (c as usize).min(value.len()),
                        _ => value.len(),
                    };
                    editor.set_cursor_from_flat(target);
                    panel.instance_states.insert(
                        widget_key,
                        crate::widgets::WidgetInstanceState::Text {
                            editor,
                            scroll,
                            completions,
                            completion_selected_index: sel_idx,
                            completion_scroll_offset: scroll_off,
                            completion_navigated: navigated,
                            user_scrolled: false,
                        },
                    );
                }
            }
            WidgetMutation::SetChecked {
                widget_key,
                checked,
            } => {
                // Toggle checked lives in the spec (not instance
                // state). Walk the spec, find the Toggle by key,
                // mutate.
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    crate::widgets::set_toggle_checked_in_spec(
                        &mut panel.spec,
                        &widget_key,
                        checked,
                    );
                }
            }
            WidgetMutation::SetSelectedIndex { widget_key, index } => {
                // Selected index lives in instance state for both List
                // and Tree widgets — dispatch on the existing variant so
                // a plugin-driven selection move on a Tree (e.g. Search &
                // Replace "next match") actually updates the Tree instead
                // of clobbering it with a List state (which drops the
                // expanded-keys set and never moves the highlight).
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    Self::set_widget_selected_index_state(panel, &widget_key, index);
                }
            }
            WidgetMutation::SetNumber { widget_key, value } => {
                // Number value is host-owned instance state; clamp to
                // the widget's bounds and write it. The trailing
                // rerender repaints. No `change` event — a plugin-driven
                // set is not a user edit (matches SetValue).
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    let (min, max) =
                        match crate::widgets::find_widget_by_key(&panel.spec, &widget_key) {
                            Some(fresh_core::api::WidgetSpec::Number { min, max, .. }) => {
                                (*min, *max)
                            }
                            _ => (None, None),
                        };
                    let clamped = crate::widgets::clamp_number(value, min, max);
                    panel.instance_states.insert(
                        widget_key.clone(),
                        crate::widgets::WidgetInstanceState::Number { value: clamped },
                    );
                }
            }
            WidgetMutation::SetDropdown { widget_key, index } => {
                // Dropdown selected index is host-owned instance state;
                // clamp to the option set and write it. The trailing
                // rerender repaints. No `change` event (matches SetValue).
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    let len = match crate::widgets::find_widget_by_key(&panel.spec, &widget_key) {
                        Some(fresh_core::api::WidgetSpec::Dropdown { options, .. }) => {
                            options.len()
                        }
                        _ => 0,
                    };
                    let clamped = if len == 0 {
                        0
                    } else {
                        index.clamp(0, len as i32 - 1)
                    };
                    let open = matches!(
                        panel.instance_states.get(&widget_key),
                        Some(crate::widgets::WidgetInstanceState::Dropdown { open: true, .. })
                    );
                    panel.instance_states.insert(
                        widget_key.clone(),
                        crate::widgets::WidgetInstanceState::Dropdown {
                            selected_index: clamped,
                            open,
                        },
                    );
                }
            }
            WidgetMutation::SetDualIncluded {
                widget_key,
                included,
            } => {
                // DualList included order is host-owned instance state;
                // drop unknown values and preserve/reset cursors. The
                // trailing rerender repaints. No `change` event.
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    let sanitized =
                        match crate::widgets::find_widget_by_key(&panel.spec, &widget_key) {
                            Some(fresh_core::api::WidgetSpec::DualList { options, .. }) => {
                                crate::widgets::dual_sanitize_included(options, &included)
                            }
                            _ => included.clone(),
                        };
                    let (active, avail_cur, incl_cur) = match panel.instance_states.get(&widget_key)
                    {
                        Some(crate::widgets::WidgetInstanceState::DualList {
                            active_included,
                            available_cursor,
                            included_cursor,
                            ..
                        }) => (*active_included, *available_cursor, *included_cursor),
                        _ => (false, 0, 0),
                    };
                    panel.instance_states.insert(
                        widget_key.clone(),
                        crate::widgets::WidgetInstanceState::DualList {
                            included: sanitized,
                            active_included: active,
                            available_cursor: avail_cur,
                            included_cursor: incl_cur,
                        },
                    );
                }
            }
            WidgetMutation::SetCompletions { widget_key, items } => {
                // Update completion popup state on a Text widget.
                // Non-empty `items` opens the popup and resets the
                // host-managed selection to the top candidate;
                // empty closes it. The instance state has to
                // exist first (a SetCompletions arriving before
                // any render is dropped on the floor — Text
                // instance state is seeded on first render of
                // the spec).
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    if let Some(crate::widgets::WidgetInstanceState::Text {
                        completions,
                        completion_selected_index,
                        completion_scroll_offset,
                        completion_navigated,
                        ..
                    }) = panel.instance_states.get_mut(&widget_key)
                    {
                        *completions = items;
                        *completion_selected_index = 0;
                        *completion_scroll_offset = 0;
                        // A (re)opened popup is not yet "entered": Tab /
                        // Enter act on the form until the user steps in
                        // with ↑/↓. (Closing — empty `items` — also
                        // resets it, harmlessly.)
                        *completion_navigated = false;
                    }
                }
            }
            WidgetMutation::SetItems {
                widget_key,
                items,
                item_keys,
            } => {
                // List items live in the spec.
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    crate::widgets::set_list_items_in_spec(
                        &mut panel.spec,
                        &widget_key,
                        items,
                        item_keys,
                    );
                }
            }
            WidgetMutation::SetExpandedKeys { widget_key, keys } => {
                // Tree expanded_keys lives in instance state.
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    let (prev_scroll, prev_sel, prev_user_scrolled) =
                        match panel.instance_states.get(&widget_key) {
                            Some(crate::widgets::WidgetInstanceState::Tree {
                                scroll_offset,
                                selected_index,
                                user_scrolled,
                                ..
                            }) => (*scroll_offset, *selected_index, *user_scrolled),
                            _ => (0, -1, false),
                        };
                    let expanded: std::collections::HashSet<String> = keys.into_iter().collect();
                    panel.instance_states.insert(
                        widget_key,
                        crate::widgets::WidgetInstanceState::Tree {
                            scroll_offset: prev_scroll,
                            selected_index: prev_sel,
                            expanded_keys: expanded,
                            user_scrolled: prev_user_scrolled,
                        },
                    );
                }
            }
            WidgetMutation::SetCheckedKeys {
                widget_key,
                checked,
                keys,
            } => {
                // Tree node `checked` lives in the spec (not instance
                // state) — the plugin is the source of truth and can
                // re-derive the boolean from its model on every spec
                // emit. The mutator just stamps the new value into the
                // matching nodes so the next render reflects it
                // immediately, without round-tripping through the
                // plugin.
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    crate::widgets::set_tree_checked_keys_in_spec(
                        &mut panel.spec,
                        &widget_key,
                        checked,
                        &keys,
                    );
                }
            }
            WidgetMutation::AppendTreeNodes {
                widget_key,
                new_nodes,
                new_item_keys,
            } => {
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    crate::widgets::append_tree_nodes_in_spec(
                        &mut panel.spec,
                        &widget_key,
                        new_nodes,
                        new_item_keys,
                    );
                }
            }
            WidgetMutation::SetRawEntries {
                widget_key,
                entries,
            } => {
                if let Some(panel) = self.widget_registry.get_mut(panel_key) {
                    crate::widgets::set_raw_entries_in_spec(&mut panel.spec, &widget_key, entries);
                }
            }
            WidgetMutation::SetFocusKey { widget_key } => {
                // Panel-level focus lives in the registry, not the
                // spec. The renderer reads it on the next paint and
                // re-clamps to the first tabbable if the key isn't a
                // current tabbable, so an unknown key is a safe no-op.
                self.widget_registry.set_focus_key(panel_key, widget_key);
            }
        }

        // Re-render with the mutated state. `rerender_widget_panel`
        // reads the registry's current spec + instance state and
        // pushes the result through the buffer.
        self.rerender_widget_panel(panel_key);
    }

    fn handle_unmount_widget_panel(&mut self, panel_key: &crate::widgets::PanelKey) {
        match self.widget_registry.unmount(panel_key) {
            Some(buffer_id) => {
                tracing::debug!(
                    "Unmounted widget panel {} (was rendering into {:?})",
                    panel_key,
                    buffer_id
                );
                // Buffer lifetime is owned by the plugin (it created the
                // virtual buffer before mounting). The plugin is
                // responsible for closing/clearing it; we only forget our
                // panel state.
            }
            None => {
                tracing::debug!("UnmountWidgetPanel for unknown panel {} ignored", panel_key);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_mount_floating_widget(
        &mut self,
        panel_key: crate::widgets::PanelKey,
        spec: fresh_core::api::WidgetSpec,
        width_pct: u8,
        height_pct: u8,
        as_dock: bool,
        focus_marker: bool,
        // Native modal-frame chrome for a centered panel (ignored for the
        // dock / anchored). See `FloatingWidgetState::{title,closable}`.
        title: Option<String>,
        closable: bool,
        // Mount without taking keyboard focus. The alternative — mount
        // focused, then a follow-up `blur` command — has a window where
        // the panel owns the keyboard, because command dispatch is
        // budgeted across frames and the pair may split across ticks.
        start_blurred: bool,
    ) {
        let width_pct = width_pct.clamp(1, 100);
        let height_pct = height_pct.clamp(1, 100);
        // The dock mounts into its own slot so it coexists with a
        // centered modal; everything else is a centered overlay.
        let slot = if as_dock {
            super::PanelSlot::Dock
        } else {
            super::PanelSlot::Floating
        };
        if !as_dock {
            // A modal becomes the new input owner as soon as it is published.
            // Retire gestures captured by the outgoing surface first.
            self.cancel_active_mouse_gesture();
        }
        let buffer_id = slot.buffer_id();
        // A centered modal owns the keyboard: blur a focused dock so the
        // two slots never both claim input. Without this, a dock key
        // handler (e.g. its Esc→blur) would greedily consume keys the
        // modal deferred to its own mode bindings, stranding the modal
        // open. Fires the dock's `blur` widget_event so the owning plugin
        // can mirror the state. Does nothing when the dock isn't focused.
        if !as_dock && self.dock.as_ref().is_some_and(|f| f.focused) {
            self.blur_floating_panel(super::PanelSlot::Dock);
        }
        let placement = if as_dock {
            let width = self
                .dock_width
                .unwrap_or(32)
                .clamp(10, self.terminal_width.max(20).saturating_sub(20).max(10));
            super::PanelPlacement::LeftDock { width_cols: width }
        } else {
            super::PanelPlacement::Centered
        };
        if let Some(existing) = self.panel_opt_mut(slot).take() {
            if existing.panel_key != panel_key {
                let _ = self.widget_registry.unmount(&existing.panel_key);
            }
        }
        *self.panel_opt_mut(slot) = Some(FloatingWidgetState {
            panel_key: panel_key.clone(),
            width_pct,
            height_pct,
            placement,
            focused: !start_blurred,
            entries: Vec::new(),
            focus_cursor: None,
            embeds: Vec::new(),
            overlays: Vec::new(),
            scroll_regions: Vec::new(),
            scrollbar_tracks: Vec::new(),
            scrollbar_mouse: Default::default(),
            scrollbar_drag_key: None,
            last_outer_rect: None,
            last_inner_rect: None,
            scrollbar_hover_zones: Vec::new(),
            scrollbar_zone_hovered: false,
            scrollbar_flash_until: None,
            fullscreen: false,
            focus_marker,
            // The native modal frame is a centered-modal affordance; the dock
            // (left companion) and anchored (context-menu) placements never
            // draw a title bar or close button, so drop the chrome there.
            title: if as_dock { None } else { title },
            closable: !as_dock && closable,
            close_button_rect: None,
            hovered_widget_key: String::new(),
            dropdown_popup: None,
            dropdown_popup_hits: Vec::new(),
            dropdown_popup_rect: None,
        });
        let prev = std::collections::HashMap::new();
        let prev_focus = String::new();
        let panel_width = self.floating_panel_inner_width(slot);
        // A fresh mount has nothing hovered: the pointer hasn't been
        // resolved against this panel's hit areas yet, and the next
        // `Moved` event will do so.
        let out = {
            let theme_guard = self.theme.read().unwrap();
            super::widget_runtime::render_floating_spec(
                focus_marker,
                &spec,
                &prev,
                &prev_focus,
                panel_width,
                "",
                Some(crate::widgets::MarkdownCtx {
                    theme: &theme_guard,
                    grammars: Some(self.grammar_registry.as_ref()),
                }),
            )
        };
        let focus_cursor = out.focus_cursor;
        let entries = out.entries;
        let embeds = out.embeds;
        let overlays = out.overlays;
        let scroll_regions = out.scroll_regions;
        let dropdown_popup = out.dropdown_popup;
        self.widget_registry.mount(
            panel_key.clone(),
            buffer_id,
            spec,
            out.hits,
            out.instance_states,
            out.focus_key,
            out.tabbable,
            scroll_regions.clone(),
        );
        if let Some(fwp) = self.panel_mut(slot) {
            fwp.entries = entries;
            fwp.focus_cursor = focus_cursor;
            fwp.embeds = embeds;
            fwp.overlays = overlays;
            fwp.scroll_regions = scroll_regions;
            fwp.dropdown_popup = dropdown_popup;
        }
        tracing::debug!(
            "Mounted floating widget panel {} ({}%x{}%)",
            panel_key,
            width_pct,
            height_pct
        );

        // Mounting a panel as the left dock carves a full-height column out
        // of the chrome. Run the single layout funnel so terminals and
        // viewports reflow to the post-dock width right away (a centered
        // panel leaves `dock_cols` at 0, so this is a cheap no-op there).
        if as_dock {
            self.relayout();
        }
    }

    fn handle_update_floating_widget(
        &mut self,
        panel_key: &crate::widgets::PanelKey,
        spec: fresh_core::api::WidgetSpec,
    ) {
        let Some(slot) = self.slot_of_panel(panel_key) else {
            tracing::debug!(
                "UpdateFloatingWidget for unknown / mismatched panel {} ignored",
                panel_key
            );
            return;
        };
        let prev = self
            .widget_registry
            .instance_states(panel_key)
            .cloned()
            .unwrap_or_default();
        let prev_focus = self
            .widget_registry
            .focus_key(panel_key)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let panel_width = self.floating_panel_inner_width(slot);
        let focus_marker = self.panel(slot).map(|f| f.focus_marker).unwrap_or(false);
        // Carry the live hover through a plugin-driven update, so a spec
        // refresh under a stationary pointer doesn't drop the highlight.
        let hover_key = self
            .panel(slot)
            .map(|f| f.hovered_widget_key.clone())
            .unwrap_or_default();
        let out = {
            let theme_guard = self.theme.read().unwrap();
            super::widget_runtime::render_floating_spec(
                focus_marker,
                &spec,
                &prev,
                &prev_focus,
                panel_width,
                &hover_key,
                Some(crate::widgets::MarkdownCtx {
                    theme: &theme_guard,
                    grammars: Some(self.grammar_registry.as_ref()),
                }),
            )
        };
        let focus_cursor = out.focus_cursor;
        let entries = out.entries;
        let embeds = out.embeds;
        let overlays = out.overlays;
        let scroll_regions = out.scroll_regions;
        let dropdown_popup = out.dropdown_popup;
        if self
            .widget_registry
            .update(
                panel_key,
                spec,
                out.hits,
                out.instance_states,
                out.focus_key,
                out.tabbable,
                scroll_regions.clone(),
            )
            .is_err()
        {
            tracing::debug!(
                "UpdateFloatingWidget for unknown panel {} ignored (not in registry)",
                panel_key
            );
            return;
        }
        if let Some(fwp) = self.panel_mut(slot) {
            fwp.entries = entries;
            fwp.focus_cursor = focus_cursor;
            fwp.embeds = embeds;
            fwp.overlays = overlays;
            fwp.scroll_regions = scroll_regions;
            fwp.dropdown_popup = dropdown_popup;
        }
    }

    fn handle_unmount_floating_widget(&mut self, panel_key: &crate::widgets::PanelKey) {
        let Some(slot) = self.slot_of_panel(panel_key) else {
            tracing::debug!(
                "UnmountFloatingWidget for unknown / mismatched panel {} ignored",
                panel_key
            );
            return;
        };
        *self.panel_opt_mut(slot) = None;
        let _ = self.widget_registry.unmount(panel_key);
        // Hiding the left dock frees its full-height column. The next
        // frame's `compute_dock_split` already lays the chrome back out
        // full-width (and the early command drain in `render` makes that
        // happen in the *same* frame as the unmount), so the layout is
        // correct — but the freed strip can still show stale glyphs from
        // the old dock until something repaints those cells. Force a full
        // clear+redraw so the reclaim is unconditional on every terminal,
        // mirroring how a resize relayout clears. Gated to the dock slot:
        // a centered modal overlays the full-width chrome without carving
        // it, so clearing on its close would only cause a visible flicker.
        if slot == super::PanelSlot::Dock {
            self.request_full_redraw();
        }
        // Restore the active window's visible terminal PTYs to their
        // dive-view split rects. The orchestrator picker's preview
        // pane shrinks PTYs to the embed size on every frame while
        // it's up (see `render_session_preview_into_rect`); when the
        // picker closes onto the *same* session the user was
        // previewing, `set_active_window` short-circuits because the
        // active pointer didn't move, and the shrink-down never gets
        // undone — top / vim / etc. keep drawing at the embed's ~15
        // rows. Resizing here on every panel unmount restores the
        // full dive-view dimensions; for panels that didn't preview
        // anything (the new-session form, plugin overlays) this is a
        // cheap no-op because the PTY sizes already match. Unmounting the
        // dock also frees its column, so route through the single layout
        // funnel: it re-derives `dock_cols` (now 0 for a dock unmount) and
        // reflows every window's terminals + viewports to the reclaimed width.
        self.relayout();
        tracing::debug!("Unmounted floating widget panel {}", panel_key);
    }

    /// Apply a `FloatingPanelControl` op. No-op if the panel id
    /// doesn't match the mounted floating panel.
    fn handle_floating_panel_control(
        &mut self,
        panel_key: &crate::widgets::PanelKey,
        op: &str,
        arg: f64,
    ) {
        let Some(slot) = self.slot_of_panel(panel_key) else {
            tracing::warn!("FloatingPanelControl for unknown/mismatched panel {panel_key} ignored");
            return;
        };
        // `blur` fires a widget_event, so handle it before borrowing the
        // panel — it reborrows `self` via the shared helper.
        if op == "blur" {
            self.blur_floating_panel(slot);
            return;
        }
        // Clamp the dock width relative to the terminal so it can never
        // swallow the whole chrome. Read before the &mut borrow below.
        // A user-dragged width (`dock_width`) overrides the plugin's
        // default so the resize survives toggling the dock off/on.
        let max_cols = self.terminal_width.max(20).saturating_sub(20).max(10);
        let persisted = self.dock_width;
        let Some(fwp) = self.panel_mut(slot) else {
            return;
        };
        // Whether this op changed the chrome geometry (dock width/placement),
        // so we know to re-derive the layout once the `fwp` borrow ends.
        let geometry_changed = match op {
            "dock" => {
                let requested = persisted.unwrap_or(arg as u16);
                let width_cols = requested.clamp(10, max_cols);
                fwp.placement = super::PanelPlacement::LeftDock { width_cols };
                fwp.focused = true;
                true
            }
            // Update the dock's width WITHOUT touching focus — used by the
            // plugin to make the dock responsive (re-issued on terminal
            // resize). Unlike "dock" this never steals keyboard focus back
            // from the editor, and it's a no-op unless the panel is already
            // docked. A user-dragged width still wins (persisted override).
            "dock_width" => {
                if let super::PanelPlacement::LeftDock { .. } = fwp.placement {
                    let requested = persisted.unwrap_or(arg as u16);
                    let width_cols = requested.clamp(10, max_cols);
                    fwp.placement = super::PanelPlacement::LeftDock { width_cols };
                    true
                } else {
                    false
                }
            }
            "center" => {
                fwp.placement = super::PanelPlacement::Centered;
                fwp.focused = true;
                true
            }
            // Place the panel as an unobtrusive content-sized popup anchored
            // at a screen cell (a right-click context menu). The (x, y) cell
            // is packed into the single `f64` arg as `y << 16 | x` — both fit
            // a u16 and the sum is exact in `f64`. No chrome-geometry change
            // (the dock/editor layout is untouched), so no relayout.
            "anchor" => {
                let packed = arg.max(0.0) as u64;
                let x = (packed & 0xFFFF) as u16;
                let y = ((packed >> 16) & 0xFFFF) as u16;
                fwp.placement = super::PanelPlacement::Anchored { x, y };
                fwp.focused = true;
                fwp.fullscreen = false;
                false
            }
            "focus" => {
                fwp.focused = true;
                false
            }
            // Render a centered panel over the whole frame (covering the
            // dimmed dock) instead of beside the dock in `chrome_area`.
            // `arg != 0` enables it. No chrome-geometry change (the dock
            // and editor layout are untouched — only where the modal
            // paints), so no relayout; the next frame reads the flag.
            "fullscreen" => {
                fwp.fullscreen = arg != 0.0;
                false
            }
            other => {
                tracing::warn!("FloatingPanelControl: unknown op {other:?}");
                false
            }
        };
        // The `fwp` mutable borrow ends above; now that the dock's
        // placement/width is settled, run the single layout funnel so
        // terminals, viewports and panels all reflow to the new chrome.
        if geometry_changed {
            self.relayout();
        }
    }

    fn handle_get_text_properties_at_cursor(&self, buffer_id: BufferId) {
        if let Some(state) = self
            .windows
            .get(&self.active_window)
            .map(|w| &w.buffers)
            .expect("active window present")
            .get(&buffer_id)
        {
            let cursor_pos = self
                .windows
                .get(&self.active_window)
                .and_then(|w| w.buffers.splits())
                .map(|(_, vs)| vs)
                .expect("active window must have a populated split layout")
                .values()
                .find_map(|vs| vs.buffer_state(buffer_id))
                .map(|bs| bs.cursors.primary().position)
                .unwrap_or(0);
            let properties = state.text_properties.get_at(cursor_pos);
            tracing::debug!(
                "Text properties at cursor in {:?}: {} properties found",
                buffer_id,
                properties.len()
            );
            // TODO: Fire hook with properties data for plugins to consume
        }
    }

    fn handle_set_context(&mut self, name: String, active: bool) {
        if active {
            self.active_window_mut()
                .active_custom_contexts
                .insert(name.clone());
            tracing::debug!("Set custom context: {}", name);
        } else {
            self.active_window_mut()
                .active_custom_contexts
                .remove(&name);
            tracing::debug!("Unset custom context: {}", name);
        }
    }

    fn handle_disable_lsp_for_language(&mut self, language: String) {
        tracing::info!("Disabling LSP for language: {}", language);
        let __active_id = self.active_window;
        if let Some(lsp) = self.windows.get_mut(&__active_id).map(|w| &mut w.lsp) {
            lsp.shutdown_server(&language);
            tracing::info!("Stopped LSP server for {}", language);
        }
        if let Some(lsp_configs) = self.config_mut().lsp.get_mut(&language) {
            for c in lsp_configs.as_mut_slice() {
                c.enabled = false;
                c.auto_start = false;
            }
            tracing::info!("Disabled LSP config for {}", language);
        }
        if let Err(e) = self.save_config() {
            tracing::error!("Failed to save config: {}", e);
            self.active_window_mut().status_message = Some(format!(
                "LSP disabled for {} (config save failed)",
                language
            ));
        } else {
            self.active_window_mut().status_message =
                Some(format!("LSP disabled for {}", language));
        }
        self.active_window_mut().warning_domains.lsp.clear();
    }

    fn handle_restart_lsp_for_language(&mut self, language: String) {
        tracing::info!("Plugin restarting LSP for language: {}", language);
        let file_path = self
            .active_window()
            .buffer_metadata
            .get(&self.active_buffer())
            .and_then(|meta| meta.file_path().cloned());
        let __active_id = self.active_window;
        let success = if let Some(lsp) = self.windows.get_mut(&__active_id).map(|w| &mut w.lsp) {
            let (ok, msg) = lsp.manual_restart(&language, file_path.as_deref());
            self.active_window_mut().status_message = Some(msg);
            ok
        } else {
            self.active_window_mut().status_message = Some("No LSP manager available".to_string());
            false
        };
        if success {
            self.reopen_buffers_for_language(&language);
        }
    }

    /// Mark the buffer backing `path` read-only (plugin `markFileReadOnly`).
    /// Resolved by path so it composes race-free with a preceding `openFile`
    /// command: both are processed in order, so the buffer exists here.
    fn handle_mark_buffer_read_only(&mut self, path: std::path::PathBuf) {
        let buffer_id = self
            .active_window()
            .buffer_metadata
            .iter()
            .find(|(_, m)| m.file_path().map(|p| p == &path).unwrap_or(false))
            .map(|(id, _)| *id);
        match buffer_id {
            Some(id) => {
                self.active_window_mut().mark_buffer_read_only(id, true);
                tracing::debug!("Marked buffer {:?} read-only ({})", id, path.display());
            }
            None => {
                tracing::warn!(
                    "markFileReadOnly: no open buffer for path {}",
                    path.display()
                );
            }
        }
    }

    fn handle_set_lsp_root_uri(&mut self, language: String, uri: String) {
        tracing::info!("Plugin setting LSP root URI for {}: {}", language, uri);
        match uri.parse::<lsp_types::Uri>() {
            Ok(parsed_uri) => {
                let __active_id = self.active_window;
                if let Some(lsp) = self.windows.get_mut(&__active_id).map(|w| &mut w.lsp) {
                    let restarted = lsp.set_language_root_uri(&language, parsed_uri);
                    if restarted {
                        self.active_window_mut().status_message = Some(format!(
                            "LSP root updated for {} (restarting server)",
                            language
                        ));
                    } else {
                        self.active_window_mut().status_message =
                            Some(format!("LSP root set for {}", language));
                    }
                }
            }
            Err(e) => {
                tracing::error!("Invalid LSP root URI '{}': {}", uri, e);
                self.active_window_mut().status_message =
                    Some(format!("Invalid LSP root URI: {}", e));
            }
        }
    }

    fn handle_create_scroll_sync_group(
        &mut self,
        group_id: crate::view::scroll_sync::ScrollSyncGroupId,
        left_split: SplitId,
        right_split: SplitId,
    ) {
        let success = self
            .active_window_mut()
            .scroll_sync_manager
            .create_group_with_id(group_id, left_split, right_split);
        if success {
            tracing::debug!(
                "Created scroll sync group {} for splits {:?} and {:?}",
                group_id,
                left_split,
                right_split
            );
        } else {
            tracing::warn!(
                "Failed to create scroll sync group {} (ID already exists)",
                group_id
            );
        }
    }

    fn handle_set_scroll_sync_anchors(
        &mut self,
        group_id: crate::view::scroll_sync::ScrollSyncGroupId,
        anchors: Vec<(usize, usize)>,
    ) {
        use crate::view::scroll_sync::SyncAnchor;
        let anchor_count = anchors.len();
        let sync_anchors: Vec<SyncAnchor> = anchors
            .into_iter()
            .map(|(left_line, right_line)| SyncAnchor {
                left_line,
                right_line,
            })
            .collect();
        self.active_window_mut()
            .scroll_sync_manager
            .set_anchors(group_id, sync_anchors);
        tracing::debug!(
            "Set {} anchors for scroll sync group {}",
            anchor_count,
            group_id
        );
    }

    fn handle_remove_scroll_sync_group(
        &mut self,
        group_id: crate::view::scroll_sync::ScrollSyncGroupId,
    ) {
        if self
            .active_window_mut()
            .scroll_sync_manager
            .remove_group(group_id)
        {
            tracing::debug!("Removed scroll sync group {}", group_id);
        } else {
            tracing::warn!("Scroll sync group {} not found", group_id);
        }
    }

    fn handle_create_buffer_group(
        &mut self,
        name: String,
        mode: String,
        layout_json: String,
        request_id: Option<u64>,
    ) {
        match self.create_buffer_group(name, mode, layout_json) {
            Ok(result) => {
                if let Some(req_id) = request_id {
                    let json = serde_json::to_string(&result).unwrap_or_default();
                    self.plugin_manager
                        .read()
                        .unwrap()
                        .resolve_callback(fresh_core::api::JsCallbackId::from(req_id), json);
                }
            }
            Err(e) => {
                tracing::error!("Failed to create buffer group: {}", e);
            }
        }
    }

    fn handle_send_terminal_input(&mut self, terminal: fresh_core::WindowTerminalId, data: String) {
        if let Some(handle) = self
            .windows
            .get(&terminal.window)
            .and_then(|window| window.terminal_manager.get(terminal.terminal))
        {
            handle.write(data.as_bytes());
            tracing::trace!(?terminal, bytes = data.len(), "plugin sent terminal input");
        } else {
            tracing::warn!(?terminal, "plugin targeted a missing terminal for input");
        }
    }

    fn handle_close_terminal(&mut self, terminal: fresh_core::WindowTerminalId) {
        let Some(window) = self.windows.get(&terminal.window) else {
            tracing::warn!(?terminal, "plugin targeted a closed window terminal");
            return;
        };
        let buffer_to_close = window
            .terminal_buffers
            .iter()
            .find(|(_, binding)| binding.terminal_id == terminal.terminal)
            .map(|(&buffer_id, _)| buffer_id);

        self.terminal_stop_tombstones.insert(terminal);
        let remove_pending_window = self
            .pending_remote_reattach
            .get_mut(&terminal.window)
            .is_some_and(|pending| {
                pending.remove(&terminal.terminal);
                pending.is_empty()
            });
        if remove_pending_window {
            self.pending_remote_reattach.remove(&terminal.window);
        }

        if let Some(buffer_id) = buffer_to_close {
            let previous = self.active_window;
            self.switch_active_window_pointer(terminal.window);
            if let Err(error) = self.close_buffer(buffer_id) {
                tracing::warn!(?terminal, "failed to close terminal buffer: {error}");
            }
            self.switch_active_window_pointer(previous);
            #[cfg(feature = "plugins")]
            self.update_plugin_state_snapshot();
            return;
        }

        self.purge_omp_companion_terminal(terminal);
        if let Some(window) = self.windows.get_mut(&terminal.window) {
            if window.tracked_agent_terminal == Some(terminal.terminal) {
                window.tracked_agent_terminal = None;
            }
            window.terminal_companions.remove(&terminal.terminal);
            window.terminal_manager.close(terminal.terminal);
        }
        tracing::info!(?terminal, "plugin closed terminal without a buffer");
    }

    /// Fan `signal` out to every process group the window
    /// identified by `id` is tracking. The window's authority-
    /// configured signaller (see `app/window/process_group.rs`)
    /// decides how the signal is delivered. Failures from
    /// individual groups land in the tracing log so a partial
    /// failure surfaces without aborting the rest of the
    /// stop flow.
    fn handle_signal_window(&mut self, id: fresh_core::WindowId, signal: &str) {
        let Some(window) = self.windows.get(&id) else {
            tracing::warn!("Plugin SignalWindow targeted unknown window {:?}", id);
            return;
        };
        let terminal_ids: std::collections::HashSet<_> = window
            .terminal_manager
            .tracked_terminal_ids()
            .into_iter()
            .chain(
                window
                    .terminal_buffers
                    .values()
                    .map(|binding| binding.terminal_id),
            )
            .collect();
        let terminating = matches!(signal, "SIGTERM" | "SIGKILL");

        if terminating {
            self.cancel_remote_reconnect(id);
            self.pending_remote_reattach.remove(&id);
            for terminal_id in &terminal_ids {
                let terminal = fresh_core::WindowTerminalId::new(id, *terminal_id);
                self.terminal_stop_tombstones.insert(terminal);
                self.purge_omp_companion_terminal(terminal);
            }
        }

        let results = {
            let window = self
                .windows
                .get_mut(&id)
                .expect("window identity was checked above");
            let results = window.process_groups.signal_all(signal);
            if terminating {
                for terminal_id in &terminal_ids {
                    window.revoke_terminal_script_token(*terminal_id, false);
                    window.terminal_commands.remove(terminal_id);
                    window.terminal_resume_commands.remove(terminal_id);
                    window.terminal_companions.remove(terminal_id);
                    window.ephemeral_terminals.remove(terminal_id);
                    window.terminal_manager.close(*terminal_id);
                }
                window.tracked_agent_terminal = None;
            }
            results
        };
        for (entry, result) in results {
            match result {
                Ok(true) => tracing::info!(
                    "SignalWindow {:?}: {} → pid {} ({})",
                    id,
                    signal,
                    entry.leader_pid,
                    entry.label
                ),
                Ok(false) => tracing::debug!(
                    "SignalWindow {:?}: pid {} ({}) already exited",
                    id,
                    entry.leader_pid,
                    entry.label
                ),
                Err(e) => tracing::warn!(
                    "SignalWindow {:?}: pid {} ({}): {}",
                    id,
                    entry.leader_pid,
                    entry.label,
                    e
                ),
            }
        }
    }

    fn handle_stop_window(&mut self, id: fresh_core::WindowId, grace_ms: u64) {
        let Some(targets) = self
            .windows
            .get(&id)
            .map(|window| window.process_groups.entries().to_vec())
        else {
            tracing::warn!(?id, "Plugin StopWindow targeted unknown window");
            return;
        };

        // Terminal ids and process incarnations are both captured and retired
        // synchronously by this one command. Only the exact process snapshot is
        // carried across the grace period.
        self.handle_signal_window(id, "SIGTERM");
        if targets.is_empty() {
            return;
        }
        let Some(bridge) = self.async_bridge.as_ref() else {
            self.handle_window_stop_escalation(id, targets);
            return;
        };
        let sender = bridge.sender();
        if let Some(runtime) = self.tokio_runtime.clone() {
            runtime.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(grace_ms)).await;
                let _ = sender.send(AsyncMessage::WindowStopEscalation {
                    window_id: id,
                    targets,
                });
            });
        } else {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(grace_ms));
                let _ = sender.send(AsyncMessage::WindowStopEscalation {
                    window_id: id,
                    targets,
                });
            });
        }
    }

    pub(super) fn handle_window_stop_escalation(
        &mut self,
        id: fresh_core::WindowId,
        targets: Vec<crate::app::window::ProcessGroupEntry>,
    ) {
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        for (entry, result) in window.process_groups.signal_targets("SIGKILL", &targets) {
            match result {
                Ok(true) => tracing::info!(
                    ?id,
                    pid = entry.leader_pid,
                    incarnation = entry.incarnation,
                    "StopWindow escalated exact process group"
                ),
                Ok(false) => tracing::debug!(
                    ?id,
                    pid = entry.leader_pid,
                    incarnation = entry.incarnation,
                    "StopWindow escalation skipped retired process group"
                ),
                Err(error) => tracing::warn!(
                    ?id,
                    pid = entry.leader_pid,
                    incarnation = entry.incarnation,
                    "StopWindow escalation failed: {error}"
                ),
            }
        }
    }
}

/// Clamp a plugin-requested `[start, end)` text range to a buffer's live
/// length.
///
/// `getBufferText` callers size `end` from `getBufferLength`, which reads a
/// state snapshot that lags the authoritative buffer. When the buffer shrinks
/// in between (concurrent editor + external-process edits), the requested end
/// briefly exceeds the live length. Returning the available text is the right
/// behaviour — the plugin recomputes on the next change event — so clamp
/// instead of rejecting. `start` is pinned to `end` so an over-large start
/// yields an empty range rather than `start > end`.
fn clamp_buffer_text_range(start: usize, end: usize, len: usize) -> (usize, usize) {
    let end = end.min(len);
    let start = start.min(end);
    (start, end)
}

#[cfg(test)]
mod tests {
    //! Focused tests for the SpawnHostProcess kill mechanism.
    //!
    //! These don't exercise the full `handle_plugin_command` dispatcher
    //! (which would require scaffolding an Editor with a real tokio
    //! runtime and async_bridge); they replicate the inner
    //! `tokio::select!` pattern directly on a real subprocess. A
    //! regression in the select arms or in the kill-then-wait
    //! sequencing would reproduce here.
    //!
    //! The dispatcher-level integration coverage comes from the e2e
    //! attach-cancel test in `tests/e2e/` — this unit test is the
    //! lower-level pin.
    use tokio::io::{AsyncReadExt, BufReader};
    use tokio::process::Command as TokioCommand;
    use tokio::time::{timeout, Duration};

    #[test]
    fn async_command_ids_are_recoverable_for_rejection() {
        let callback = fresh_core::api::JsCallbackId::from(41);
        assert_eq!(
            super::Editor::async_command_callback_id(&fresh_core::api::PluginCommand::Delay {
                callback_id: callback,
                duration_ms: 1,
            }),
            Some(callback)
        );
        assert_eq!(
            super::Editor::async_command_callback_id(&fresh_core::api::PluginCommand::WatchPath {
                path: std::path::PathBuf::from("owned"),
                recursive: false,
                request_id: 42,
            }),
            Some(fresh_core::api::JsCallbackId::from(42))
        );
        assert_eq!(
            super::Editor::async_command_callback_id(
                &fresh_core::api::PluginCommand::SyncSnapshot { request_id: 43 }
            ),
            Some(fresh_core::api::JsCallbackId::from(43))
        );
        assert_eq!(
            super::Editor::async_command_callback_id(&fresh_core::api::PluginCommand::SetStatus {
                message: "synchronous".to_string(),
            }),
            None
        );
    }

    /// A long-sleep child that runs `tokio::select! { wait | kill_rx }`
    /// terminates when the kill channel fires, and the terminal exit
    /// code reflects signal termination (non-zero / None).
    ///
    /// Spawns `sleep` directly rather than through `sh -c` so SIGKILL
    /// reaches the process whose pipe our reader futures hold —
    /// `sh -c sleep` leaks the sleep child on SIGKILL (Q-C2), the
    /// pipe stays open, and the reader future hangs. That's a
    /// deliberate known limitation of start_kill; this test
    /// exercises the clean path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_via_oneshot_terminates_long_running_child() {
        let mut cmd = TokioCommand::new("sleep");
        cmd.args(["30"]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().expect("spawn sh -c sleep 30");
        let pid = child.id().expect("child has a pid");

        let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let stdout_fut = async {
            let mut buf = String::new();
            if let Some(s) = stdout_pipe {
                #[allow(clippy::let_underscore_must_use)]
                let _ = BufReader::new(s).read_to_string(&mut buf).await;
            }
            buf
        };
        let stderr_fut = async {
            let mut buf = String::new();
            if let Some(s) = stderr_pipe {
                #[allow(clippy::let_underscore_must_use)]
                let _ = BufReader::new(s).read_to_string(&mut buf).await;
            }
            buf
        };
        let wait_fut = async {
            tokio::select! {
                status = child.wait() => {
                    status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1)
                }
                _ = &mut kill_rx => {
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = child.start_kill();
                    child
                        .wait()
                        .await
                        .map(|s| s.code().unwrap_or(-1))
                        .unwrap_or(-1)
                }
            }
        };

        // Give the shell a moment to install itself — firing kill
        // against an not-yet-existent child is still valid (SIGKILL
        // to a zombie is a no-op) but we want to actually exercise
        // the running-child path.
        tokio::time::sleep(Duration::from_millis(50)).await;
        kill_tx.send(()).expect("kill channel send");

        let result = timeout(Duration::from_secs(5), async {
            tokio::join!(stdout_fut, stderr_fut, wait_fut)
        })
        .await;

        let (_stdout, _stderr, exit_code) = result.expect(
            "kill path must resolve within 5s — if this times out the \
             select! arm order or kill-then-wait logic is broken",
        );
        // The cross-platform invariant is "the child did not complete
        // its 30s sleep" — i.e. the exit code is non-success. Platform
        // specifics:
        //   - Unix: `start_kill()` sends SIGKILL; `ExitStatus::code()`
        //     returns None for signal-terminated processes, which our
        //     dispatcher maps to -1 via `.unwrap_or(-1)`.
        //   - Windows: `start_kill()` calls `TerminateProcess(..., 1)`;
        //     `code()` returns `Some(1)`, mapped to 1 by the same
        //     `.unwrap_or(-1)`.
        // A successful 30s sleep would yield 0 — that's the
        // regression case we're guarding against.
        assert_ne!(
            exit_code, 0,
            "killed child must exit non-success (got 0 — did the \
             kill arm fire too late, or did sleep somehow complete?)"
        );

        // Sanity: on Unix the child must be gone. `kill -0 <pid>`
        // returns 0 iff the process still exists; we expect non-zero
        // (No such process) after wait(). This catches a zombie /
        // leaked child that would indicate we skipped the wait() on
        // the kill path. Skipped on Windows — `kill` isn't available
        // and `tasklist` output parsing is more noise than signal
        // for this one-shot check; the wait() having returned is
        // already evidence of reap there.
        #[cfg(unix)]
        {
            let still_alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(
                !still_alive,
                "process {pid} must be reaped after wait() — a still-\
                 alive check means the kill path leaked the child"
            );
        }
        #[cfg(not(unix))]
        {
            // Touch `pid` so the unused-variable lint doesn't fire on
            // non-Unix builds.
            let _ = pid;
        }
    }

    use super::{clamp_buffer_text_range, plugin_context_may_dispatch};
    use fresh_core::api::{
        PluginCommandContext, PluginCommandPurpose, PluginInstanceId, PluginLoadProvenance,
        TrustedBuiltinPlugin,
    };

    #[test]
    fn clamp_text_range_passes_through_in_bounds() {
        assert_eq!(clamp_buffer_text_range(0, 165, 165), (0, 165));
        assert_eq!(clamp_buffer_text_range(10, 50, 165), (10, 50));
    }

    /// The reported regression: `getBufferLength` returned a snapshot
    /// length one byte ahead of the live buffer (the file was shrinking
    /// under concurrent editor + external edits), so `getBufferText`
    /// requested `0..len+1`. Pre-fix this produced "Invalid range
    /// 0..165003 for buffer of length 165002"; now the end clamps down.
    #[test]
    fn clamp_text_range_clamps_stale_end_past_buffer() {
        assert_eq!(clamp_buffer_text_range(0, 165_003, 165_002), (0, 165_002));
    }

    #[test]
    fn clamp_text_range_pins_overlarge_start_to_empty() {
        // start beyond the live length must not yield start > end.
        assert_eq!(clamp_buffer_text_range(200, 250, 165), (165, 165));
    }
    #[test]
    fn unloaded_trusted_instance_cannot_dispatch_queued_privileged_action() {
        let context = PluginCommandContext {
            plugin_name: "orchestrator".into(),
            plugin_instance_id: PluginInstanceId::fresh(),
            provenance: PluginLoadProvenance::Bundled,
            trusted_builtin: Some(TrustedBuiltinPlugin::Orchestrator),
            ..PluginCommandContext::default()
        };

        assert!(plugin_context_may_dispatch(&context, true));
        assert!(!plugin_context_may_dispatch(&context, false));
    }

    #[test]
    fn inactive_instance_allows_loader_owned_compensating_cleanup() {
        let context = PluginCommandContext {
            plugin_name: "orchestrator".into(),
            plugin_instance_id: PluginInstanceId::fresh(),
            provenance: PluginLoadProvenance::Bundled,
            trusted_builtin: Some(TrustedBuiltinPlugin::Orchestrator),
            purpose: PluginCommandPurpose::CompensatingCleanup,
            ..PluginCommandContext::default()
        };

        assert!(plugin_context_may_dispatch(&context, false));
    }
}

impl Window {
    /// Populate the per-window fields of the plugin state snapshot.
    ///
    /// Called by `Editor::update_plugin_state_snapshot` while it holds
    /// the snapshot write lock. Covers everything that a single Window
    /// owns: active buffer/split ids, all this window's buffers (with
    /// per-buffer view-mode, compose state, preview flag, split
    /// membership), per-buffer cursor positions and text properties,
    /// the active buffer's cursors / viewport / selected text, the
    /// per-split snapshot list, this window's active-session plugin
    /// state, this window's authority label, diagnostics, folding
    /// ranges, editor mode, and the per-window plugin view states.
    /// Editor-wide fields (clipboard, windows list, config cache,
    /// user_config_raw, plugin_global_state) are populated by the
    /// Editor coda after this returns.
    #[cfg(feature = "plugins")]
    pub(crate) fn populate_plugin_state_snapshot(
        &mut self,
        snapshot: &mut fresh_core::api::EditorStateSnapshot,
    ) {
        use fresh_core::api::{BufferInfo, CursorInfo, ViewportInfo};

        // Rebuild only on registry mutation. Compares the registry's
        // monotonic catalog_gen against the last-seen value on the
        // snapshot — a single integer check, no allocation, no
        // count-mismatch ambiguity between the syntect set and the
        // unified catalog.
        let current_gen = self.resources.grammar_registry.catalog_gen();
        if snapshot.last_grammar_gen != current_gen {
            snapshot.available_grammars = self
                .resources
                .grammar_registry
                .available_grammar_info()
                .into_iter()
                .map(|g| fresh_core::api::GrammarInfoSnapshot {
                    name: g.name,
                    source: g.source.to_string(),
                    file_extensions: g.file_extensions,
                    short_name: g.short_name,
                })
                .collect();
            snapshot.last_grammar_gen = current_gen;
        }

        snapshot.active_buffer_id = self.active_buffer();

        // Mirror the active session's recorded macros into the snapshot so
        // plugins can read them synchronously via `editor.listMacros()` /
        // `editor.getMacro()`. Computed before the split-layout borrow below
        // to avoid overlapping immutable borrows of `self`. Macros are few and
        // small, so rebuilding each tick is negligible.
        snapshot.macros = {
            let macros = &self.macros;
            macros
                .keys_sorted()
                .into_iter()
                .filter_map(|key| {
                    macros
                        .get(key)
                        .map(|actions| fresh_core::api::MacroSnapshot {
                            register: key.to_string(),
                            steps: actions.iter().map(|a| a.to_action_spec()).collect(),
                        })
                })
                .collect()
        };

        let (mgr_ref, vs_ref) = self
            .buffers
            .splits()
            .expect("active window must have a populated split layout");
        let active_split = mgr_ref.active_split();
        snapshot.active_split_id = active_split.0 .0;

        // Clear and update buffer info
        snapshot.buffers.clear();
        snapshot.buffer_saved_diffs.clear();
        snapshot.buffer_cursor_positions.clear();
        snapshot.buffer_text_properties.clear();

        let active_vs_opt = vs_ref.get(&active_split);
        for (buffer_id, state) in &self.buffers {
            let is_virtual = self
                .buffer_metadata
                .get(buffer_id)
                .map(|m| m.is_virtual())
                .unwrap_or(false);
            // Report the ACTIVE split's view_mode so plugins can distinguish
            // which mode the user is currently in. Separately, report whether
            // ANY split has compose mode so plugins can maintain decorations
            // for compose-mode splits even when a source-mode split is active.
            let view_mode = active_vs_opt
                .and_then(|vs| vs.buffer_state(*buffer_id))
                .map(|bs| match bs.view_mode {
                    crate::state::ViewMode::Source => "source",
                    crate::state::ViewMode::PageView => "compose",
                })
                .unwrap_or("source");
            let compose_width = active_vs_opt
                .and_then(|vs| vs.buffer_state(*buffer_id))
                .and_then(|bs| bs.compose_width);
            let is_composing_in_any_split = vs_ref.values().any(|vs| {
                vs.buffer_state(*buffer_id)
                    .map(|bs| matches!(bs.view_mode, crate::state::ViewMode::PageView))
                    .unwrap_or(false)
            });
            let is_preview = self.is_buffer_preview(*buffer_id);
            // A terminal pane and a plugin scratch pane are both "virtual";
            // only the window knows which one has a PTY behind it.
            let is_terminal = self.is_terminal_buffer(*buffer_id);
            // Which splits currently hold this buffer — lets plugins
            // implement "focus existing if visible, else open new"
            // without tracking split ids across editor restarts
            // (the restart reassigns them). SplitManager has the
            // authoritative map; we just mirror it.
            let splits: Vec<fresh_core::SplitId> = mgr_ref
                .splits_for_buffer(*buffer_id)
                .into_iter()
                .map(|leaf_id| leaf_id.0)
                .collect();
            let buffer_info = BufferInfo {
                id: *buffer_id,
                path: state.buffer.file_path().map(|p| p.to_path_buf()),
                // The tab label. For a virtual buffer this is the `name` the
                // creating plugin chose, which is the only stable way for it
                // to find its own panel again — `path` is empty for every
                // virtual buffer, so it distinguishes nothing.
                name: self
                    .buffer_metadata
                    .get(buffer_id)
                    .map(|m| m.display_name.clone())
                    .unwrap_or_default(),
                modified: state.buffer.is_modified(),
                length: state.buffer.len(),
                line_count: state.buffer.line_count(),
                is_virtual,
                is_terminal,
                editing_disabled: state.editing_disabled,
                view_mode: view_mode.to_string(),
                is_composing_in_any_split,
                compose_width,
                language: state.language.clone(),
                is_preview,
                splits,
            };
            snapshot.buffers.insert(*buffer_id, buffer_info);

            let diff = {
                let diff = state.buffer.diff_since_saved();
                BufferSavedDiff {
                    equal: diff.equal,
                    byte_ranges: diff.byte_ranges.clone(),
                }
            };
            snapshot.buffer_saved_diffs.insert(*buffer_id, diff);

            // Regular buffers live in exactly one split's keyed_states.
            // Panel (hidden) buffers natively live inside a group's inner
            // split — but the close-buffer path can leave a *shadow*
            // entry in the group's host split (from `switch_buffer`'s
            // auto-insert, kept to preserve the
            // `active_buffer ∈ keyed_states` invariant). For hidden
            // buffers we therefore skip group-host splits and pick the
            // inner split, which is the authoritative home.
            let is_hidden = self
                .buffer_metadata
                .get(buffer_id)
                .is_some_and(|m| m.hidden_from_tabs);
            let source_split = vs_ref.iter().find(|(split_id, vs)| {
                vs.keyed_states.contains_key(buffer_id)
                    && !(is_hidden && self.grouped_subtrees.contains_key(split_id))
            });
            let cursor_pos = source_split
                .and_then(|(_, vs)| vs.buffer_state(*buffer_id))
                .map(|bs| bs.cursors.primary().position)
                .unwrap_or(0);
            tracing::trace!(
                "snapshot: buffer {:?} cursor_pos={} (from split {:?})",
                buffer_id,
                cursor_pos,
                source_split.map(|(id, _)| *id),
            );
            snapshot
                .buffer_cursor_positions
                .insert(*buffer_id, cursor_pos);

            // Store text properties if this buffer has any
            if !state.text_properties.is_empty() {
                snapshot
                    .buffer_text_properties
                    .insert(*buffer_id, state.text_properties.all().to_vec());
            }
        }

        // Update cursor information for active buffer.
        //
        // Use `effective_active_pair()` for the split id rather than
        // the split manager's outer `active_split()`. When the active
        // split holds a buffer-group tab, the user's keystrokes (and
        // therefore the meaningful cursor) live in the focused inner
        // panel's leaf — `focused_group_leaf` — not the outer leaf.
        // Reading the outer's cursor here would publish (0, 0) into
        // the snapshot while the user is editing the inner panel,
        // which is what `editor.getCursorPosition()` then sees.
        let active_buf_id = snapshot.active_buffer_id;
        let active_split_id = self.effective_active_pair().0;
        // Captured before the closure borrows `self`: the panes' rects are
        // derived from the same area the renderer lays out into, so the
        // geometry a plugin reads matches the cells actually drawn.
        let content_area = self.editor_content_area();
        self.buffers
            .with_all_mut(|buffers_mut, mgr, vs_map| {
                if let Some(active_vs) = vs_map.get(&active_split_id) {
                    // Primary cursor (from SplitViewState)
                    let active_cursors = &active_vs.cursors;
                    let primary = active_cursors.primary();
                    let primary_position = primary.position;
                    let primary_selection = primary.selection_range();

                    // Resolve a byte offset to its 0-indexed line, but only when the
                    // active buffer has a line index. Huge files load without line
                    // metadata (`line_count() == None`); reporting `0` there would be
                    // a lie, so we surface `None` instead — the same guard the
                    // viewport's `top_line` uses below.
                    let line_of = |offset: usize| -> Option<usize> {
                        buffers_mut.get(&active_buf_id).and_then(|state| {
                            if state.buffer.line_count().is_some() {
                                Some(state.buffer.get_line_number(offset))
                            } else {
                                None
                            }
                        })
                    };

                    snapshot.primary_cursor = Some(CursorInfo {
                        position: primary_position,
                        selection: primary_selection.clone(),
                        line: line_of(primary_position),
                    });

                    // Mirror the editor's cached primary cursor line number so
                    // `getCursorLine()` returns a meaningful value without the
                    // plugin runtime having to scan the buffer. Falls back to
                    // 0 if the active buffer state isn't available.
                    snapshot.primary_cursor_line = Some(
                        buffers_mut
                            .get(&active_buf_id)
                            .map(|s| s.primary_cursor_line_number.value() as u32)
                            .unwrap_or(0),
                    );

                    snapshot.all_cursors = active_cursors
                        .iter()
                        .map(|(_, cursor)| CursorInfo {
                            position: cursor.position,
                            selection: cursor.selection_range(),
                            line: line_of(cursor.position),
                        })
                        .collect();

                    // Selected text from primary cursor (for clipboard plugin)
                    if let Some(range) = primary_selection {
                        if let Some(active_state) = buffers_mut.get_mut(&active_buf_id) {
                            snapshot.selected_text =
                                Some(active_state.get_text_range(range.start, range.end));
                        }
                    }

                    // Viewport — get from SplitViewState (the authoritative source)
                    let top_line = buffers_mut.get(&active_buf_id).and_then(|state| {
                        if state.buffer.line_count().is_some() {
                            Some(state.buffer.get_line_number(active_vs.viewport.top_byte()))
                        } else {
                            None
                        }
                    });
                    snapshot.viewport = Some(ViewportInfo {
                        top_byte: active_vs.viewport.top_byte(),
                        top_line,
                        left_column: active_vs.viewport.left_column,
                        width: active_vs.viewport.width,
                        height: active_vs.viewport.height,
                    });
                } else {
                    snapshot.primary_cursor = None;
                    snapshot.primary_cursor_line = None;
                    snapshot.all_cursors.clear();
                    snapshot.viewport = None;
                    snapshot.selected_text = None;
                }

                // Per-split snapshot.
                //
                // Walked through the layout rather than the view-state map so
                // the list comes out in *visual* order — left to right, top to
                // bottom — and carries each pane's on-screen rect. A caller
                // asking "which pane is on the left" can then compare `x`
                // instead of guessing from an iteration order that used to be
                // a HashMap's.
                snapshot.splits.clear();
                let laid_out = mgr.get_visible_buffers(content_area);
                for (leaf_id, _buf, rect) in laid_out {
                    let Some(vs) = vs_map.get(&leaf_id) else {
                        continue;
                    };
                    let buf_id = vs.active_buffer;
                    let top_line = buffers_mut.get(&buf_id).and_then(|state| {
                        if state.buffer.line_count().is_some() {
                            Some(state.buffer.get_line_number(vs.viewport.top_byte()))
                        } else {
                            None
                        }
                    });
                    snapshot.splits.push(fresh_core::api::SplitSnapshot {
                        split_id: leaf_id.0 .0,
                        buffer_id: buf_id,
                        label: mgr.get_label(leaf_id.0).map(|l| l.to_string()),
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                        viewport: ViewportInfo {
                            top_byte: vs.viewport.top_byte(),
                            top_line,
                            left_column: vs.viewport.left_column,
                            width: vs.viewport.width,
                            height: vs.viewport.height,
                        },
                    });
                }
            })
            .expect("active window must have a populated split layout");

        // Mirror the active session's plugin_state into the snapshot
        // so getWindowState reads cheaply. Cloning is fine here: the
        // per-session state is small; plugins that store megabyte-
        // scale blobs in setWindowState will see proportional snapshot-
        // update cost, which is the desired feedback signal.
        snapshot.active_session_plugin_states = self.plugin_state.clone();
        // `authority_label` is populated by the Editor coda — see the
        // comment there for why it can't come from `self.resources`.

        // Update LSP diagnostics / folding ranges: Arc refcount bumps.
        snapshot.diagnostics = Arc::clone(&self.stored_diagnostics);
        snapshot.folding_ranges = Arc::clone(&self.stored_folding_ranges);

        // Update editor mode (for vi mode and other modal editing)
        snapshot.editor_mode = self.editor_mode.clone();

        // Update plugin view states from active split's BufferViewState.plugin_state.
        // If the active split changed, fully repopulate. Otherwise, merge
        // using or_insert to preserve JS-side write-through entries that
        // haven't round-tripped through the command channel yet.
        let active_split_id_u64 = active_split_id.0 .0;
        let split_changed = snapshot.plugin_view_states_split != active_split_id_u64;
        if split_changed {
            snapshot.plugin_view_states.clear();
            snapshot.plugin_view_states_split = active_split_id_u64;
        }

        // Clean up entries for buffers that are no longer open
        {
            let open_bids: Vec<_> = snapshot.buffers.keys().copied().collect();
            snapshot
                .plugin_view_states
                .retain(|bid, _| open_bids.contains(bid));
        }

        // Merge from Rust-side plugin_state (source of truth for persisted state)
        if let Some(vs_map) = self.buffers.split_view_states() {
            if let Some(active_vs) = vs_map.get(&active_split_id) {
                for (buffer_id, buf_state) in &active_vs.keyed_states {
                    if !buf_state.plugin_state.is_empty() {
                        let entry = snapshot.plugin_view_states.entry(*buffer_id).or_default();
                        for (key, value) in &buf_state.plugin_state {
                            entry.entry(key.clone()).or_insert_with(|| value.clone());
                        }
                    }
                }
            }
        }

        // Update active search state so plugins can query it via hasActiveSearch()
        snapshot.has_active_search = self.search_state.is_some();
    }
}

// `editor.httpFetch` downloads stream through `services::http::download_to_file`,
// which keeps all ureq/TLS usage in one place (gated by the `http` feature).

/// Fixed-capacity sink that captures only the leading identifier of a
/// `Debug` rendering and aborts formatting once the variant name ends.
/// `write!` bails on the first `Err`, so the command's payload — which can be
/// a whole buffer's worth of text — is never formatted just to name it.
struct VariantNameSink {
    buf: [u8; 48],
    len: usize,
}

impl std::fmt::Write for VariantNameSink {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        for b in s.bytes() {
            if !(b.is_ascii_alphanumeric() || b == b'_') || self.len == self.buf.len() {
                return Err(std::fmt::Error);
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
        Ok(())
    }
}

impl VariantNameSink {
    /// Name the `PluginCommand` variant without formatting its payload.
    fn of(command: &PluginCommand) -> Self {
        use std::fmt::Write as _;
        let mut sink = Self {
            buf: [0; 48],
            len: 0,
        };
        // Errors are the expected exit: the sink aborts formatting as soon as
        // the variant name ends, so the `Err` is the success path here.
        let _stopped_at_name_end = write!(sink, "{:?}", command);
        sink
    }

    fn as_str(&self) -> &str {
        if self.len == 0 {
            return "PluginCommand";
        }
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("PluginCommand")
    }
}
