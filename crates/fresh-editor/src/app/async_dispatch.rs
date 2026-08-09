//! Async-message dispatch on `Editor`.
//!
//! `process_async_messages` runs each frame and drains the AsyncBridge,
//! routing each AsyncMessage to its handler — LSP responses,
//! initialization/errors, plugin commands, filesystem polling, etc. The
//! `match` is a thin dispatch table: every arm forwards to a `handle_*`
//! method on `Editor` that owns the actual logic for that variant.

use rust_i18n::t;

use crate::services::async_bridge::{AsyncMessage, AsyncMessageEnvelope};
use crate::view::prompt::PromptType;

use super::{Editor, RemoteAttachOwner};
#[derive(Debug)]
struct TerminalOutputHookPayload {
    terminal: fresh_core::WindowTerminalId,
    last_line: String,
    terminal_title: String,
    osc_activity: Option<bool>,
}

/// Latest-only terminal-output delivery. Only the payload is retained while a
/// plugin handler is in flight; the full window snapshot is built immediately
/// before dispatching the next item.
#[derive(Debug, Default)]
pub(crate) struct TerminalOutputHookDelivery {
    queue: std::collections::VecDeque<fresh_core::WindowTerminalId>,
    queued: std::collections::HashSet<fresh_core::WindowTerminalId>,
    in_flight: Option<fresh_core::WindowTerminalId>,
    latest: std::collections::HashMap<fresh_core::WindowTerminalId, TerminalOutputHookPayload>,
    in_flight_tombstoned: bool,
}

impl TerminalOutputHookDelivery {
    fn push(&mut self, payload: TerminalOutputHookPayload) -> bool {
        let terminal = payload.terminal;
        if self.in_flight == Some(terminal) && self.in_flight_tombstoned {
            return false;
        }
        self.latest.insert(terminal, payload);
        if self.in_flight != Some(terminal) && self.queued.insert(terminal) {
            self.queue.push_back(terminal);
        }
        true
    }

    fn take_next(&mut self) -> Option<TerminalOutputHookPayload> {
        if self.in_flight.is_some() {
            return None;
        }
        while let Some(terminal) = self.queue.pop_front() {
            self.queued.remove(&terminal);
            if let Some(payload) = self.latest.remove(&terminal) {
                self.in_flight = Some(terminal);
                return Some(payload);
            }
        }
        None
    }

    fn complete_in_flight(&mut self) {
        let Some(terminal) = self.in_flight.take() else {
            return;
        };
        let was_tombstoned = std::mem::take(&mut self.in_flight_tombstoned);
        if !was_tombstoned && self.latest.contains_key(&terminal) && self.queued.insert(terminal) {
            self.queue.push_back(terminal);
        }
    }

    fn purge(&mut self, terminal: fresh_core::WindowTerminalId) {
        if self.in_flight == Some(terminal) {
            self.in_flight_tombstoned = true;
        }
        self.latest.remove(&terminal);
        self.queued.remove(&terminal);
        self.queue.retain(|queued| *queued != terminal);
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.latest.len()
    }
}

fn coalesce_terminal_output_messages(messages: &mut Vec<AsyncMessageEnvelope>) {
    let mut seen = std::collections::HashSet::new();
    messages.retain(|envelope| match envelope.message() {
        AsyncMessage::TerminalOutput { terminal } => seen.insert(*terminal),
        _ => true,
    });
}
/// How long the editor thread may spend dispatching async messages in one
/// tick. A pathology guard rather than a frame pacer (see
/// `PLUGIN_COMMAND_FRAME_BUDGET`): normal bursts drain in a pass or two;
/// only a flood is deferred.
const ASYNC_MESSAGE_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

impl Editor {
    /// Resolve one plugin instance's `attachRemoteAgent` promise once the
    /// session is fully constructed. Born-attached sessions return their exact
    /// window identity so plugins never infer ownership from mutable focus.
    pub(crate) fn resolve_remote_attach(
        &self,
        plugin_instance_id: fresh_core::api::PluginInstanceId,
        request_id: u64,
        window_id: Option<fresh_core::WindowId>,
    ) {
        let payload = window_id.map_or_else(
            || "{}".to_string(),
            |id| format!(r#"{{"windowId":{}}}"#, id.0),
        );
        self.plugin_manager
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resolve_callback_for(
                plugin_instance_id,
                fresh_core::api::JsCallbackId::from(request_id),
                payload,
            );
    }

    /// Reject one plugin instance's `attachRemoteAgent` promise. Numeric
    /// request ids are runtime-local and cannot identify the owner by themselves.
    pub(crate) fn reject_remote_attach(
        &self,
        plugin_instance_id: fresh_core::api::PluginInstanceId,
        request_id: u64,
        error: String,
    ) {
        tracing::warn!(?plugin_instance_id, "attachRemoteAgent rejected: {error}");
        self.plugin_manager
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reject_callback_for(
                plugin_instance_id,
                fresh_core::api::JsCallbackId::from(request_id),
                error,
            );
    }

    fn remote_attempt_is_current(&self, attempt_id: u64, owner: RemoteAttachOwner) -> bool {
        match owner {
            RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                ..
            } => {
                self.remote_attach_plugin_requests
                    .get(&(plugin_instance_id, request_id))
                    == Some(&attempt_id)
            }
            RemoteAttachOwner::Reconnect { window_id }
            | RemoteAttachOwner::Switch { window_id } => {
                self.remote_reconnect_attempts.get(&window_id) == Some(&attempt_id)
            }
        }
    }

    pub(crate) fn begin_remote_attach_attempt(&mut self, owner: RemoteAttachOwner) -> Option<u64> {
        let already_running = match owner {
            RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                ..
            } => self
                .remote_attach_plugin_requests
                .contains_key(&(plugin_instance_id, request_id)),
            RemoteAttachOwner::Reconnect { window_id }
            | RemoteAttachOwner::Switch { window_id } => {
                self.remote_reconnect_attempts.contains_key(&window_id)
            }
        };
        if already_running {
            return None;
        }

        let attempt_id = self.next_remote_attach_attempt;
        self.next_remote_attach_attempt = self
            .next_remote_attach_attempt
            .checked_add(1)
            .expect("remote attach attempt id exhausted");
        self.remote_attach_attempts.insert(attempt_id, owner);
        match owner {
            RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                ..
            } => {
                self.remote_attach_plugin_requests
                    .insert((plugin_instance_id, request_id), attempt_id);
            }
            RemoteAttachOwner::Reconnect { window_id }
            | RemoteAttachOwner::Switch { window_id } => {
                self.remote_reconnect_attempts.insert(window_id, attempt_id);
            }
        }
        Some(attempt_id)
    }

    fn remove_remote_attach_attempt(
        &mut self,
        attempt_id: u64,
    ) -> Option<(RemoteAttachOwner, Option<tokio::sync::oneshot::Sender<()>>)> {
        let owner = self.remote_attach_attempts.remove(&attempt_id)?;
        match owner {
            RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                ..
            } => {
                if self
                    .remote_attach_plugin_requests
                    .get(&(plugin_instance_id, request_id))
                    == Some(&attempt_id)
                {
                    self.remote_attach_plugin_requests
                        .remove(&(plugin_instance_id, request_id));
                }
            }
            RemoteAttachOwner::Reconnect { window_id }
            | RemoteAttachOwner::Switch { window_id } => {
                if self.remote_reconnect_attempts.get(&window_id) == Some(&attempt_id) {
                    self.remote_reconnect_attempts.remove(&window_id);
                }
            }
        }
        let cancel = self.remote_attach_cancels.remove(&attempt_id);
        Some((owner, cancel))
    }

    pub(crate) fn settle_remote_attach_attempt(
        &mut self,
        attempt_id: u64,
    ) -> Option<RemoteAttachOwner> {
        let owner = *self.remote_attach_attempts.get(&attempt_id)?;
        if !self.remote_attempt_is_current(attempt_id, owner) {
            self.remove_remote_attach_attempt(attempt_id);
            return None;
        }
        self.remove_remote_attach_attempt(attempt_id)
            .map(|(owner, _)| owner)
    }

    pub(crate) fn cancel_plugin_remote_attach(
        &mut self,
        plugin_instance_id: fresh_core::api::PluginInstanceId,
        request_id: u64,
    ) {
        let Some(attempt_id) = self
            .remote_attach_plugin_requests
            .get(&(plugin_instance_id, request_id))
            .copied()
        else {
            return;
        };
        let Some((owner, cancel)) = self.remove_remote_attach_attempt(attempt_id) else {
            return;
        };
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
        if let RemoteAttachOwner::Plugin {
            plugin_instance_id: owner_instance,
            request_id,
            window_id,
        } = owner
        {
            self.reject_remote_attach(owner_instance, request_id, "cancelled".to_string());
            if let Some(window) = self.windows.get_mut(&window_id) {
                window.set_status_message("Connection cancelled".to_string());
            }
        }
    }

    pub(crate) fn cancel_plugin_remote_attaches(
        &mut self,
        plugin_instance_id: fresh_core::api::PluginInstanceId,
    ) {
        let requests: Vec<u64> = self
            .remote_attach_plugin_requests
            .keys()
            .filter_map(|(owner_instance, request_id)| {
                (*owner_instance == plugin_instance_id).then_some(*request_id)
            })
            .collect();
        for request_id in requests {
            self.cancel_plugin_remote_attach(plugin_instance_id, request_id);
        }
    }
    pub(crate) fn cancel_remote_attaches_for_window(&mut self, window_id: fresh_core::WindowId) {
        let attempts: Vec<u64> = self
            .remote_attach_attempts
            .iter()
            .filter_map(|(attempt_id, owner)| match owner {
                RemoteAttachOwner::Plugin {
                    window_id: owner_window,
                    ..
                } if *owner_window == window_id => Some(*attempt_id),
                _ => None,
            })
            .collect();
        for attempt_id in attempts {
            let Some((owner, cancel)) = self.remove_remote_attach_attempt(attempt_id) else {
                continue;
            };
            if let Some(cancel) = cancel {
                let _ = cancel.send(());
            }
            if let RemoteAttachOwner::Plugin {
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
        }
    }

    pub(crate) fn cancel_remote_reconnect(&mut self, window_id: fresh_core::WindowId) {
        let Some(attempt_id) = self.remote_reconnect_attempts.get(&window_id).copied() else {
            return;
        };
        let Some((_, cancel)) = self.remove_remote_attach_attempt(attempt_id) else {
            return;
        };
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
    }

    pub(crate) fn remote_reconnect_inflight(&self, window_id: fresh_core::WindowId) -> bool {
        self.remote_reconnect_attempts.contains_key(&window_id)
    }

    /// Drain pending async messages and plugin commands to completion.
    ///
    /// This is the historical contract of this method and what direct
    /// callers (tests, the test API, one-shot tools) rely on: after it
    /// returns, everything that was pending at call time — including work
    /// the per-pass frame budget deferred — has been dispatched. The
    /// interactive loops must NOT use this; they call
    /// [`Self::process_async_messages_budgeted`] so one burst is spread
    /// across frames instead of stalling one.
    pub fn process_async_messages(&mut self) -> bool {
        let mut needs_render = false;
        // The cap is a backstop against a plugin that emits continuously —
        // each pass drains everything that had arrived when it started, so
        // legitimate cascades settle in a handful of passes.
        for _ in 0..64 {
            needs_render |= self.process_async_messages_budgeted();
            if self.async_message_backlog.is_empty() && !self.plugin_backlog_pending() {
                break;
            }
        }
        needs_render
    }

    /// Process pending async messages from the async bridge, against a
    /// frame budget.
    ///
    /// This is what the interactive loops call each frame:
    /// - LSP diagnostics
    /// - LSP initialization/errors
    /// - File system changes (future)
    /// - Git status updates
    ///
    /// A burst larger than the budget is deferred in arrival order and the
    /// return value stays `true` until the backlog drains, keeping the loop
    /// on its frame cadence.
    pub fn process_async_messages_budgeted(&mut self) -> bool {
        // Check plugin thread health - will panic if thread died due to error
        // This ensures plugin errors surface quickly instead of causing silent hangs
        self.plugin_manager.write().unwrap().check_thread_health();

        // Lazily wire an event-driven reconnect forwarder for each remote
        // window (idempotent; cheap when already wired). This replaces the old
        // per-frame connection-state poll: the forwarder awaits the channel's
        // reconnect notification and posts `RemoteReconnected`.
        self.ensure_remote_reconnect_forwarders();

        // Kick off off-loop content loads for any freshly-restored remote
        // placeholder buffers (idempotent; drains each window's queue). This is
        // what makes a dived-into remote session's files fill in without ever
        // reading them on the editor loop.
        self.drive_pending_content_loads();

        let Some(bridge) = &self.async_bridge else {
            return false;
        };

        // Merge the global bridge, live-window bridges, and closing-window
        // bridges without erasing their origin. The source tag stays on any
        // frame-budget backlog entry, so a later tick cannot reinterpret a
        // response against whichever window happens to be active then.
        let mut messages: Vec<AsyncMessageEnvelope> =
            std::mem::take(&mut self.async_message_backlog).into();
        {
            let _s = tracing::info_span!("try_recv_all").entered();
            messages.extend(
                bridge
                    .try_recv_all()
                    .into_iter()
                    .map(AsyncMessageEnvelope::Global),
            );
        }
        for (&window_id, window) in &self.windows {
            messages.extend(
                window
                    .bridge
                    .try_recv_all()
                    .into_iter()
                    .map(|message| AsyncMessageEnvelope::Window(window_id, message)),
            );
        }
        for (&window_id, closing) in &self.closing_windows {
            messages.extend(
                closing
                    .bridge
                    .try_recv_all()
                    .into_iter()
                    .map(|message| AsyncMessageEnvelope::Window(window_id, message)),
            );
        }
        // PTY readers may publish many batches before one editor frame. The
        // handler reads the terminal's current grid, so one message per exact
        // terminal preserves the latest observable payload without building a
        // private plugin snapshot for every read.
        coalesce_terminal_output_messages(&mut messages);
        // A render is only warranted if a message can actually change the
        // screen. A `DelayComplete` just resolves a debounced
        // `editor.delay()` callback in the plugin runtime; on its own it
        // paints nothing. Any visual outcome of the resumed plugin code
        // arrives as a follow-up plugin *command* and is caught by
        // `process_plugin_commands`'s `has_visual_commands` check below (or
        // on the next tick). Forcing a render for the bare completion made
        // live_diff's per-keystroke debounce repaint the screen with no
        // change — invisible locally, but real lag over serial (#2100).
        let needs_render = messages.iter().any(|envelope| {
            !matches!(
                envelope.message(),
                AsyncMessage::Plugin(fresh_core::api::PluginAsyncMessage::DelayComplete { .. })
            )
        });
        tracing::trace!(
            async_message_count = messages.len(),
            "received async messages"
        );

        // Frame budget, same contract as the plugin command drain: dispatch
        // against a deadline, then defer the tail in arrival order. A burst
        // (spawn storm, LSP flood) is spread over frames instead of being
        // fully absorbed before the next one.
        let deadline = std::time::Instant::now() + ASYNC_MESSAGE_FRAME_BUDGET;
        let mut handled = 0usize;
        let mut messages = messages.into_iter();
        for envelope in messages.by_ref() {
            let (source_window, message) = envelope.into_parts();
            // Once a window is gone, only its concrete terminal exit may pass
            // the close barrier. Every other late per-window message is stale
            // and must not be re-attributed to a surviving active window.
            if source_window.is_some_and(|window| !self.windows.contains_key(&window))
                && !matches!(&message, AsyncMessage::TerminalExited { .. })
            {
                continue;
            }
            macro_rules! lsp_source {
                () => {
                    match source_window {
                        Some(window) => window,
                        None => {
                            tracing::warn!("Dropping LSP async message without a window source");
                            continue;
                        }
                    }
                };
            }
            match message {
                AsyncMessage::LspDiagnostics {
                    uri,
                    diagnostics,
                    server_name,
                } => {
                    self.handle_lsp_diagnostics(lsp_source!(), uri, diagnostics, server_name);
                }
                AsyncMessage::LspInitialized {
                    language,
                    server_name,
                    capabilities,
                } => {
                    self.handle_lsp_initialized(lsp_source!(), language, server_name, capabilities);
                }
                AsyncMessage::LspError {
                    language,
                    error,
                    stderr_log_path,
                } => {
                    self.handle_lsp_error(lsp_source!(), language, error, stderr_log_path);
                }
                AsyncMessage::LspCompletion { request_id, items } => {
                    if let Err(e) =
                        self.handle_completion_response(lsp_source!(), request_id, items)
                    {
                        tracing::error!("Error handling completion response: {}", e);
                    }
                }
                AsyncMessage::LspGotoDefinition {
                    request_id,
                    locations,
                } => {
                    if let Err(e) =
                        self.handle_goto_definition_response(lsp_source!(), request_id, locations)
                    {
                        tracing::error!("Error handling goto definition response: {}", e);
                    }
                }
                AsyncMessage::LspImplementation {
                    request_id,
                    locations,
                } => {
                    if let Err(e) =
                        self.handle_implementation_response(lsp_source!(), request_id, locations)
                    {
                        tracing::error!("Error handling implementation response: {}", e);
                    }
                }
                AsyncMessage::LspRename { request_id, result } => {
                    if let Err(e) = self.handle_rename_response(lsp_source!(), request_id, result) {
                        tracing::error!("Error handling rename response: {}", e);
                    }
                }
                AsyncMessage::LspHover {
                    request_id,
                    contents,
                    is_markdown,
                    range,
                } => {
                    self.handle_hover_response(
                        lsp_source!(),
                        request_id,
                        contents,
                        is_markdown,
                        range,
                    );
                }
                AsyncMessage::LspReferences {
                    request_id,
                    locations,
                } => {
                    if let Err(e) =
                        self.handle_references_response(lsp_source!(), request_id, locations)
                    {
                        tracing::error!("Error handling references response: {}", e);
                    }
                }
                AsyncMessage::LspSignatureHelp {
                    request_id,
                    signature_help,
                } => {
                    self.handle_signature_help_response(lsp_source!(), request_id, signature_help);
                }
                AsyncMessage::LspCodeActions {
                    request_id,
                    actions,
                } => {
                    self.handle_code_actions_response(lsp_source!(), request_id, actions);
                }
                AsyncMessage::LspApplyEdit { edit, label } => {
                    self.handle_lsp_apply_edit(lsp_source!(), edit, label);
                }
                AsyncMessage::LspCodeActionResolved {
                    request_id: _,
                    action,
                } => {
                    self.handle_lsp_code_action_resolved(lsp_source!(), action);
                }
                AsyncMessage::LspCompletionResolved {
                    request_id: _,
                    item,
                } => {
                    if let Ok(resolved) = item {
                        self.handle_completion_resolved(lsp_source!(), resolved);
                    }
                }
                AsyncMessage::LspFormatting {
                    request_id: _,
                    uri,
                    edits,
                } => {
                    if !edits.is_empty() {
                        if let Err(e) = self.apply_formatting_edits(lsp_source!(), &uri, edits) {
                            tracing::error!("Failed to apply formatting: {}", e);
                        }
                    }
                }
                AsyncMessage::LspPrepareRename {
                    request_id: _,
                    result,
                } => {
                    self.handle_prepare_rename_response(lsp_source!(), result);
                }
                AsyncMessage::LspPulledDiagnostics {
                    request_id: _,
                    uri,
                    server_name,
                    result_id,
                    diagnostics,
                    unchanged,
                } => {
                    self.handle_lsp_pulled_diagnostics(
                        lsp_source!(),
                        uri,
                        server_name,
                        result_id,
                        diagnostics,
                        unchanged,
                    );
                }
                AsyncMessage::LspInlayHints {
                    request_id,
                    uri,
                    hints,
                } => {
                    self.handle_lsp_inlay_hints(lsp_source!(), request_id, uri, hints);
                }
                AsyncMessage::LspFoldingRanges {
                    request_id,
                    uri,
                    ranges,
                } => {
                    self.handle_lsp_folding_ranges(lsp_source!(), request_id, uri, ranges);
                }
                AsyncMessage::LspSemanticTokens {
                    request_id,
                    uri,
                    response,
                } => {
                    self.handle_lsp_semantic_tokens(lsp_source!(), request_id, uri, response);
                }
                AsyncMessage::LspServerQuiescent { language } => {
                    self.handle_lsp_server_quiescent(lsp_source!(), language);
                }
                AsyncMessage::LspDiagnosticRefresh { language } => {
                    self.handle_lsp_diagnostic_refresh(lsp_source!(), language);
                }
                AsyncMessage::LspInlayHintRefresh { language } => {
                    self.handle_lsp_inlay_hint_refresh(lsp_source!(), language);
                }
                AsyncMessage::LspSemanticTokensRefresh { language } => {
                    self.handle_lsp_semantic_tokens_refresh(lsp_source!(), language);
                }
                AsyncMessage::LspDynamicCapabilities {
                    language,
                    server_name,
                    register,
                    registrations,
                } => {
                    self.handle_lsp_dynamic_capabilities(
                        lsp_source!(),
                        language,
                        server_name,
                        register,
                        registrations,
                    );
                }
                AsyncMessage::FileChanged { path } => {
                    self.handle_async_file_changed(path);
                }
                AsyncMessage::GitStatusChanged { status } => {
                    tracing::info!("Git status changed: {}", status);
                    // TODO: Handle git status changes
                }
                AsyncMessage::FileExplorerInitialized {
                    window,
                    filesystem_id,
                    view,
                } => {
                    self.handle_file_explorer_initialized(window, filesystem_id, view);
                }
                AsyncMessage::FileExplorerInitFailed {
                    window,
                    filesystem_id,
                } => {
                    if let Some(win) = self.windows.get_mut(&window) {
                        let current = crate::services::async_bridge::filesystem_identity(
                            &win.authority().filesystem,
                        );
                        if current == filesystem_id {
                            win.file_explorer_init_failed();
                        }
                    }
                }
                AsyncMessage::FileExplorerToggleNode(node_id) => {
                    self.handle_file_explorer_toggle_node(node_id);
                }
                AsyncMessage::FileExplorerRefreshNode(node_id) => {
                    self.handle_file_explorer_refresh_node(node_id);
                }
                AsyncMessage::FileExplorerExpandedToPath {
                    window,
                    filesystem_id,
                    view,
                } => {
                    self.handle_file_explorer_expanded_to_path(window, filesystem_id, view);
                }
                AsyncMessage::Plugin(plugin_msg) => {
                    self.handle_plugin_async_message(plugin_msg);
                }
                AsyncMessage::LspProgress {
                    language,
                    token,
                    value,
                } => {
                    self.handle_lsp_progress(lsp_source!(), language, token, value);
                }
                AsyncMessage::LspWindowMessage {
                    language,
                    message_type,
                    message,
                } => {
                    self.handle_lsp_window_message(lsp_source!(), language, message_type, message);
                }
                AsyncMessage::LspLogMessage {
                    language,
                    message_type,
                    message,
                } => {
                    self.handle_lsp_log_message(lsp_source!(), language, message_type, message);
                }
                AsyncMessage::LspStatusUpdate {
                    language,
                    server_name,
                    status,
                    message: _,
                } => {
                    self.handle_lsp_status_update(lsp_source!(), language, server_name, status);
                }
                AsyncMessage::FileOpenDirectoryLoaded(result) => {
                    self.handle_file_open_directory_loaded(result);
                }
                AsyncMessage::FileOpenShortcutsLoaded(shortcuts) => {
                    self.handle_file_open_shortcuts_loaded(shortcuts);
                }
                AsyncMessage::ClipboardPasteResult { request_id, text } => {
                    self.resolve_pending_paste(request_id, text);
                }
                AsyncMessage::TerminalOutput { terminal } => {
                    self.handle_terminal_output(terminal);
                }
                AsyncMessage::OmpCompanionSnapshotReady { terminal } => {
                    self.handle_omp_companion_snapshot_ready(terminal);
                }
                AsyncMessage::PathChanged { handle, path, kind } => {
                    self.handle_path_changed(handle, path, kind);
                }
                AsyncMessage::TerminalExited {
                    terminal,
                    exit_code,
                } => {
                    // If this is the interactive self-update terminal, move the
                    // status-bar indicator to its terminal state. The exit code
                    // distinguishes all three: installed, action-required, failed.
                    if self.self_update_terminal == Some(terminal) {
                        self.finish_self_update(exit_code);
                        self.self_update_terminal = None;
                    }
                    self.handle_terminal_exited(terminal, exit_code);
                }

                AsyncMessage::LspServerRequest {
                    language,
                    server_command,
                    method,
                    params,
                } => {
                    self.handle_lsp_server_request(
                        lsp_source!(),
                        language,
                        server_command,
                        method,
                        params,
                    );
                }
                AsyncMessage::PluginLspResponse {
                    language: _,
                    request_id,
                    result,
                } => {
                    self.handle_plugin_lsp_response(request_id, result);
                }
                AsyncMessage::RemoteAttachReady(ready) => {
                    self.handle_remote_attach_ready(ready);
                }
                AsyncMessage::RemoteReconnected {
                    connection_id,
                    generation,
                } => {
                    self.handle_remote_reconnected(connection_id, generation);
                }
                AsyncMessage::RemoteBufferContentLoaded {
                    window_id,
                    buffer_id,
                    path,
                    filesystem_id,
                    content,
                } => {
                    self.handle_remote_buffer_content_loaded(
                        window_id,
                        buffer_id,
                        path,
                        filesystem_id,
                        content,
                    );
                }
                AsyncMessage::RemoteAttachFailed { error, attempt_id } => {
                    self.handle_remote_attach_failed(error, attempt_id);
                }
                AsyncMessage::WindowStopEscalation { window_id, targets } => {
                    self.handle_window_stop_escalation(window_id, targets);
                }
                AsyncMessage::PluginProcessOutput {
                    process_id,
                    stdout,
                    stderr,
                    exit_code,
                } => {
                    // Drop any host-process kill handle tied to this
                    // id. The spawn task has exited (that's what this
                    // event means) so the handle is stale; a late
                    // `KillHostProcess` from the plugin should be a
                    // silent no-op rather than a dangling send. For
                    // non-host-process spawns the key won't be in
                    // the map and the remove is a no-op.
                    self.host_process_handles.remove(&process_id);
                    self.handle_plugin_process_output(
                        fresh_core::api::JsCallbackId::from(process_id),
                        stdout,
                        stderr,
                        exit_code,
                    );
                }
                AsyncMessage::GrammarRegistryBuilt {
                    registry,
                    callback_ids,
                } => {
                    self.handle_grammar_registry_built(registry, callback_ids);
                }
                AsyncMessage::QuickOpenFilesLoaded {
                    cwd,
                    files,
                    complete,
                } => {
                    self.handle_quick_open_files_loaded(cwd, files, complete);
                }
                AsyncMessage::PluginsDirLoaded {
                    dir,
                    errors,
                    discovered_plugins,
                } => {
                    self.handle_plugins_dir_loaded(dir, errors, discovered_plugins);
                }
                AsyncMessage::PluginDeclarationsReady { declarations } => {
                    self.handle_plugin_declarations_ready(declarations);
                }
                AsyncMessage::PluginInitScriptLoaded(outcome) => {
                    self.handle_plugin_init_script_loaded(outcome);
                }
            }
            handled += 1;
            // Same floor as the plugin-command drain: a message whose handler
            // overruns the budget must not throttle the queue to 1/frame.
            if handled >= super::DRAIN_MIN_PER_PASS && std::time::Instant::now() >= deadline {
                break;
            }
        }
        self.async_message_backlog = messages.collect();
        if !self.async_message_backlog.is_empty() {
            tracing::debug!(
                deferred = self.async_message_backlog.len(),
                "async message frame budget exhausted — deferring tail"
            );
        }

        // Update plugin state snapshot BEFORE processing commands
        // This ensures plugins have access to current editor state (cursor positions, etc.)
        #[cfg(feature = "plugins")]
        {
            let _s = tracing::info_span!("update_plugin_state_snapshot").entered();
            self.update_plugin_state_snapshot();
        }

        // Process TypeScript plugin commands
        #[cfg(not(feature = "plugins"))]
        let processed_any_commands = false;
        #[cfg(feature = "plugins")]
        let processed_any_commands = {
            let _s = tracing::info_span!("process_plugin_commands").entered();
            self.process_plugin_commands()
        };

        // Re-sync snapshot after commands — commands like SetViewMode change
        // state that plugins read via getBufferInfo().  Without this, a
        // subsequent lines_changed callback would see stale values.
        #[cfg(feature = "plugins")]
        if processed_any_commands {
            let _s = tracing::info_span!("update_plugin_state_snapshot_post").entered();
            self.update_plugin_state_snapshot();
        }

        // Process pending plugin action completions
        #[cfg(feature = "plugins")]
        {
            let _s = tracing::info_span!("process_pending_plugin_actions").entered();
            self.process_pending_plugin_actions();
        }

        // Process pending LSP server restarts (with exponential backoff)
        {
            let _s = tracing::info_span!("process_pending_lsp_restarts").entered();
            self.process_pending_lsp_restarts();
        }

        // Check and clear the plugin render request flag
        #[cfg(feature = "plugins")]
        let plugin_render = {
            let render = self.plugin_render_requested;
            self.plugin_render_requested = false;
            render
        };
        #[cfg(not(feature = "plugins"))]
        let plugin_render = false;

        // Poll periodic update checker for new results
        if let Some(ref mut checker) = self.update_checker {
            // Poll for results but don't act on them - just cache
            let _ = checker.poll_result();
        }

        // Poll for file changes (auto-revert) and file tree changes
        let file_changes = {
            let _s = tracing::info_span!("poll_file_changes").entered();
            self.poll_file_changes()
        };
        let tree_changes = {
            let _s = tracing::info_span!("poll_file_tree_changes").entered();
            self.poll_file_tree_changes()
        };

        // Trigger render if any async messages, plugin commands were processed, or plugin requested render
        //
        // A non-empty backlog also counts: the frame budget deferred work, and
        // returning `true` keeps the main loop on its frame cadence so the tail
        // drains at ~60Hz instead of at the 50ms idle poll.
        let backlogged = !self.async_message_backlog.is_empty() || self.plugin_backlog_pending();
        needs_render
            || processed_any_commands
            || plugin_render
            || file_changes
            || tree_changes
            || backlogged
    }

    /// Whether the plugin command frame budget left work for the next tick.
    fn plugin_backlog_pending(&self) -> bool {
        #[cfg(feature = "plugins")]
        {
            !self.plugin_command_backlog.is_empty()
        }
        #[cfg(not(feature = "plugins"))]
        {
            false
        }
    }

    /// Handle a server's `initialize` response: record capabilities and kick off
    /// the deferred per-language requests that were gated on them.
    fn handle_lsp_initialized(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
        server_name: String,
        capabilities: crate::services::lsp::manager::ServerCapabilitySummary,
    ) {
        tracing::info!(
            "LSP server '{}' initialized for language: {}",
            server_name,
            language
        );
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        window.status_message = Some(format!("LSP ({}) ready", language));
        window
            .lsp
            .set_server_capabilities(&language, &server_name, capabilities);

        self.resend_did_open_for_language_in_window(window_id, &language);
        self.request_semantic_tokens_for_language_in_window(window_id, &language);
        self.request_folding_ranges_for_language_in_window(window_id, &language);
        self.request_inlay_hints_for_language_in_window(window_id, &language);
        self.pull_diagnostics_for_language_in_window(window_id, &language);
    }

    /// Handle an LSP server crash/spawn failure: surface it, fire the
    /// `lsp_server_error` hook, and open the stderr log in the background.
    fn handle_lsp_error(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
        error: String,
        stderr_log_path: Option<std::path::PathBuf>,
    ) {
        tracing::error!("LSP error for {}: {}", language, error);
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        window.status_message = Some(format!("LSP error ({}): {}", language, error));

        // Get server command from config for the hook
        let server_command = self
            .config
            .lsp
            .get(&language)
            .and_then(|configs| configs.as_slice().first())
            .map(|c| c.command.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Determine error type from error message
        let error_type = if error.contains("not found") || error.contains("NotFound") {
            "not_found"
        } else if error.contains("permission") || error.contains("PermissionDenied") {
            "spawn_failed"
        } else if error.contains("timeout") {
            "timeout"
        } else {
            "spawn_failed"
        }
        .to_string();

        // Fire the hook through the window and authority that owned the LSP
        // request, even if another session gained focus before the error
        // arrived.
        self.run_plugin_hook_for_window(
            window_id,
            "lsp_server_error",
            crate::services::plugins::hooks::HookArgs::LspServerError {
                language: language.clone(),
                server_command,
                error_type,
                message: error.clone(),
            },
        );

        // Open stderr log as read-only buffer if it exists and has content
        // Opens in background (new tab) without stealing focus
        if let Some(log_path) = stderr_log_path {
            let has_content = log_path.metadata().map(|m| m.len() > 0).unwrap_or(false);
            if has_content {
                tracing::info!("Opening LSP stderr log in background: {:?}", log_path);
                match self
                    .windows
                    .get_mut(&window_id)
                    .expect("source window checked above")
                    .open_file_no_focus(&log_path)
                {
                    Ok(buffer_id) => {
                        let window = self.windows.get_mut(&window_id).unwrap();
                        window.mark_buffer_read_only(buffer_id, true);
                        window.status_message = Some(format!(
                            "LSP error ({}): {} - See stderr log",
                            language, error
                        ));
                    }
                    Err(e) => {
                        tracing::error!("Failed to open LSP stderr log: {}", e);
                    }
                }
            }
        }
    }

    /// Apply a server-initiated `workspace/applyEdit`.
    fn handle_lsp_apply_edit(
        &mut self,
        window_id: fresh_core::WindowId,
        edit: lsp_types::WorkspaceEdit,
        label: Option<String>,
    ) {
        tracing::info!("Applying workspace edit from server (label: {:?})", label);
        match self.apply_workspace_edit_in_window(window_id, edit) {
            Ok(n) => {
                if let Some(label) = label {
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        window.set_status_message(
                            t!("lsp.code_action_applied", title = &label, count = n).to_string(),
                        );
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to apply workspace edit: {}", e);
            }
        }
    }

    /// Execute a resolved code action, or report the `codeAction/resolve` error.
    fn handle_lsp_code_action_resolved(
        &mut self,
        window_id: fresh_core::WindowId,
        action: Result<lsp_types::CodeAction, String>,
    ) {
        match action {
            Ok(resolved) => {
                self.execute_resolved_code_action_in_window(window_id, resolved);
            }
            Err(e) => {
                tracing::warn!("codeAction/resolve failed: {}", e);
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.set_status_message(format!("Code action resolve failed: {e}"));
                }
            }
        }
    }

    /// Route a plugin-runtime async message (process I/O, delays, LSP and
    /// generic plugin responses) to its handler/hook.
    fn handle_plugin_async_message(&mut self, plugin_msg: fresh_core::api::PluginAsyncMessage) {
        use fresh_core::api::{JsCallbackId, PluginAsyncMessage};
        match plugin_msg {
            PluginAsyncMessage::ProcessOutput {
                process_id,
                stdout,
                stderr,
                exit_code,
            } => {
                self.handle_plugin_process_output(
                    JsCallbackId::from(process_id),
                    stdout,
                    stderr,
                    exit_code,
                );
            }
            PluginAsyncMessage::DelayComplete { callback_id } => {
                self.plugin_manager
                    .read()
                    .unwrap()
                    .resolve_callback(JsCallbackId::from(callback_id), "null".to_string());
            }
            PluginAsyncMessage::ProcessStdout { process_id, data } => {
                if let Some(window_id) = self
                    .background_process_handles
                    .get(&process_id)
                    .map(|(window_id, _)| *window_id)
                {
                    self.run_plugin_hook_for_window(
                        window_id,
                        "onProcessStdout",
                        crate::services::plugins::hooks::HookArgs::ProcessOutput {
                            process_id,
                            data,
                        },
                    );
                }
            }
            PluginAsyncMessage::ProcessStderr { process_id, data } => {
                if let Some(window_id) = self
                    .background_process_handles
                    .get(&process_id)
                    .map(|(window_id, _)| *window_id)
                {
                    self.run_plugin_hook_for_window(
                        window_id,
                        "onProcessStderr",
                        crate::services::plugins::hooks::HookArgs::ProcessOutput {
                            process_id,
                            data,
                        },
                    );
                }
            }
            PluginAsyncMessage::ProcessExit {
                process_id,
                callback_id,
                exit_code,
            } => {
                self.background_process_handles.remove(&process_id);
                let result = fresh_core::api::BackgroundProcessResult {
                    process_id,
                    exit_code,
                };
                self.plugin_manager.read().unwrap().resolve_callback(
                    JsCallbackId::from(callback_id),
                    serde_json::to_string(&result).unwrap(),
                );
            }
            PluginAsyncMessage::LspResponse {
                language: _,
                request_id,
                result,
            } => {
                self.handle_plugin_lsp_response(request_id, result);
            }
            PluginAsyncMessage::PluginResponse(response) => {
                self.handle_plugin_response(response);
            }
            PluginAsyncMessage::OffLoopSettled {
                callback_id,
                result,
            } => {
                let pm = self.plugin_manager.read().unwrap();
                match result {
                    Ok(json) => pm.resolve_callback(JsCallbackId::from(callback_id), json),
                    Err(e) => pm.reject_callback(JsCallbackId::from(callback_id), e),
                }
            }
        }
    }

    /// Handle new terminal output: follow the bottom when appropriate and fire
    /// the `terminal_output` hook, attributing it to the owning session.
    fn handle_terminal_output(&mut self, terminal: fresh_core::WindowTerminalId) {
        // The message carries its owning window: terminal ids
        // collide across windows, so we trust the tag rather
        // than scanning windows for a matching id (which would
        // attribute output to the wrong session).
        let terminal_id = terminal.terminal;
        let owner = terminal.window;
        let owner_is_active = owner == self.active_window;
        // Terminal output received - check if we should auto-jump back to terminal mode
        tracing::trace!("Terminal output received for {}", terminal);

        // If the focused split is viewing this terminal in scrollback and
        // jump_to_end_on_output is enabled, snap it back to the live grid.
        //
        // ...but never yank the view away from a text selection: a drag that
        // just started on the live grid (`terminal_drag_pending`), an
        // in-progress selection drag, or a completed selection waiting to be
        // copied all pin the scrollback view. A chatty program would
        // otherwise destroy the selection the instant its next output
        // arrived — the exact case drag-to-select exists for. Output keeps
        // streaming underneath; the auto-jump resumes once the selection is
        // gone (Ctrl+Space, typing, or a click that collapses it).
        let selection_active = owner_is_active && {
            let win = self.active_window();
            win.mouse_state.dragging_text_selection
                || win.mouse_state.terminal_drag_pending.is_some()
                || win
                    .buffers
                    .splits()
                    .and_then(|(mgr, view_states)| view_states.get(&mgr.active_split()))
                    .map(|vs| {
                        let c = vs.cursors.primary();
                        c.anchor.is_some_and(|a| a != c.position)
                    })
                    .unwrap_or(false)
        };
        if owner_is_active
            && self.config.terminal.jump_to_end_on_output
            && !self.active_window().focused_terminal_live()
            && !selection_active
        {
            // Check if active buffer is this terminal
            if let Some(active_terminal_id) =
                self.active_window().get_terminal_id(self.active_buffer())
            {
                if active_terminal_id == terminal_id {
                    self.enter_terminal_mode();
                }
            }
        }

        // Alacritty follows new output while its display offset is zero and
        // preserves a nonzero offset while the user is reading history. Do not
        // force a focused live terminal back to the bottom here: animated
        // status output would otherwise undo every wheel-up event.

        // Snapshot and enqueue work only exists when a plugin consumes it.
        if !self
            .plugin_manager
            .read()
            .unwrap()
            .has_subscribers("terminal_output")
        {
            return;
        }

        // Attribute output to the owning session even when it is in the
        // background. The grid lock is released before plugin dispatch.
        let last_line = self
            .windows
            .get(&owner)
            .and_then(|w| w.terminal_manager.get(terminal_id))
            .and_then(|handle| handle.state.lock().ok().map(|s| s.last_visible_line()))
            .unwrap_or_default();
        let terminal_title = self
            .windows
            .get(&owner)
            .and_then(|w| w.terminal_tab_title(terminal_id))
            .filter(|t| !(t.starts_with("*Terminal ") && t.ends_with('*')))
            .unwrap_or_default();
        let osc_activity = self
            .windows
            .get(&owner)
            .and_then(|w| w.terminal_manager.get(terminal_id))
            .and_then(|handle| handle.state.lock().ok().and_then(|s| s.osc_activity()));

        if self
            .terminal_output_delivery
            .push(TerminalOutputHookPayload {
                terminal,
                last_line,
                terminal_title,
                osc_activity,
            })
        {
            self.dispatch_next_terminal_output_hook();
        }
    }

    fn dispatch_next_terminal_output_hook(&mut self) {
        loop {
            let Some(payload) = self.terminal_output_delivery.take_next() else {
                return;
            };
            if self.run_plugin_hook_for_window(
                payload.terminal.window,
                "terminal_output",
                crate::services::plugins::hooks::HookArgs::TerminalOutput {
                    terminal_id: payload.terminal.terminal.0 as u64,
                    window_id: payload.terminal.window.0,
                    last_line: payload.last_line,
                    terminal_title: payload.terminal_title,
                    osc_activity: payload.osc_activity,
                },
            ) {
                return;
            }
            self.terminal_output_delivery.complete_in_flight();
        }
    }

    pub(super) fn complete_terminal_output_hook(&mut self, hook_name: &str) {
        if hook_name != "terminal_output" {
            return;
        }
        self.terminal_output_delivery.complete_in_flight();
        self.dispatch_next_terminal_output_hook();
    }

    fn purge_terminal_output_hook(&mut self, terminal: fresh_core::WindowTerminalId) {
        self.terminal_output_delivery.purge(terminal);
    }

    /// Forward a watched-path filesystem event to the `path_changed` hook.
    fn handle_path_changed(
        &mut self,
        handle: u64,
        path: std::path::PathBuf,
        kind: crate::services::async_bridge::PathChangeKind,
    ) {
        let Some(owner) = self.file_watcher_manager.owner(handle) else {
            return;
        };
        if owner.plugin_instance_id.is_some_and(|instance| {
            !self
                .plugin_manager
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_plugin_instance_active(instance)
        }) {
            self.file_watcher_manager.unwatch(handle);
            return;
        }
        let authority_matches = self
            .windows
            .get(&owner.window_id)
            .is_some_and(|window| window.authority().stamp() == owner.authority);
        if !authority_matches {
            self.file_watcher_manager.unwatch(handle);
            return;
        }
        self.path_changes_for_test
            .push((handle, path.clone(), kind.as_str()));
        self.run_plugin_hook_for_window(
            owner.window_id,
            "path_changed",
            crate::services::plugins::hooks::HookArgs::PathChanged {
                handle,
                path: path.to_string_lossy().into_owned(),
                kind: kind.as_str().to_owned(),
            },
        );
    }

    /// Tear down (or preserve, for a pending remote reconnect) a terminal whose
    /// process exited, then fire the `terminal_exit` hook.
    /// Per-frame detector for *silent* agent-channel reconnects.
    ///
    /// The SSH / Kubernetes agent channel re-establishes itself in the
    /// background by hot-swapping its transport (`spawn_reconnect_task`),
    /// without ever routing through the app-level `RemoteAttachMode::Reconnect`
    /// flow — and that flow is the only thing that respawns the embedded
    /// terminal PTYs. Those PTYs are a *separate* `ssh -t` / `kubectl exec`
    /// carrier from the agent channel, so they die when the link drops and,
    /// on the automatic recovery path, would otherwise stay dead even though
    /// the filesystem/LSP came back.
    ///
    /// Bring `window_id`'s remote session back to life after its carrier
    /// reconnected. The single convergence point for *every* reconnect path:
    ///
    ///   * the silent background transport hot-swap (`spawn_reconnect_task`),
    ///     which keeps the existing authority and notifies via
    ///     `AsyncMessage::RemoteReconnected`; and
    ///   * the app-level rebuild (`RemoteAttachMode::Reconnect`), which installs
    ///     a fresh authority first and then calls this.
    ///
    /// Either way the embedded terminal PTYs died with the old carrier (a
    /// separate `ssh -t` / `kubectl exec` from the agent channel), so we respawn
    /// them in place through the now-live authority, reusing each backing file
    /// so scrollback continues. `respawn_terminals_through_authority` skips
    /// still-live terminals, so this is idempotent under duplicate signals.
    pub(crate) fn reattach_window(&mut self, window_id: fresh_core::WindowId) -> usize {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return 0;
        };
        window.remote_reconnect_error = None;
        let revived = window.respawn_terminals_through_authority();
        if revived > 0 {
            let label = window.label.clone();
            window.set_status_message(format!("Reconnected: {label}"));
            // Reactivate the focused buffer. A terminal that was focused (and in
            // terminal input mode) when the carrier dropped had `terminal_mode`
            // cleared by `handle_terminal_exited`; respawning the PTY doesn't
            // restore it, so without this the reborn terminal is stranded in
            // scrollback / Normal mode until the user clicks it. Re-deriving the
            // flags from the active buffer's remembered (still-`Live`) mode
            // brings it back live in place. Only the active window owns the
            // `terminal_mode` / `key_context` input state; a background window
            // re-syncs when the user next focuses it.
            if window_id == self.active_window {
                self.sync_terminal_mode_to_active_buffer();
            }
        }
        revived
    }

    /// Start off-loop content reads for any window's freshly-restored remote
    /// placeholder buffers, delivering each via `RemoteBufferContentLoaded`.
    /// Idempotent: it drains each window's `pending_content_load`, so a buffer
    /// is scheduled exactly once. Runs the blocking `read_file` on a plain
    /// thread (never a runtime worker — the remote read uses `block_on`), so a
    /// slow link never touches the editor loop.
    fn drive_pending_content_loads(&mut self) {
        if self.async_bridge.is_none() {
            return;
        }
        type Job = (
            fresh_core::WindowId,
            fresh_core::BufferId,
            std::path::PathBuf,
            std::sync::Arc<dyn crate::model::filesystem::FileSystem + Send + Sync>,
        );
        let mut jobs: Vec<Job> = Vec::new();
        for (wid, window) in self.windows.iter_mut() {
            if window.pending_content_load.is_empty() {
                continue;
            }
            if window
                .authority()
                .filesystem
                .remote_connection_info()
                .is_some()
                && !window.authority().filesystem.is_remote_connected()
            {
                continue;
            }
            let fs = std::sync::Arc::clone(&window.authority().filesystem);
            for (bid, path) in window.pending_content_load.drain(..) {
                jobs.push((*wid, bid, path, std::sync::Arc::clone(&fs)));
            }
        }
        if jobs.is_empty() {
            return;
        }
        let sender = self.async_bridge.as_ref().unwrap().sender();
        for (window_id, buffer_id, path, fs) in jobs {
            let sender = sender.clone();
            std::thread::Builder::new()
                .name("remote-buffer-load".to_string())
                .spawn(move || {
                    let content = fs.read_file(&path).map_err(|e| e.to_string());
                    let filesystem_id = crate::services::async_bridge::filesystem_identity(&fs);
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = sender.send(AsyncMessage::RemoteBufferContentLoaded {
                        window_id,
                        buffer_id,
                        path,
                        filesystem_id,
                        content,
                    });
                })
                .ok();
        }
    }

    /// Install content read off-loop into a remote session's placeholder buffer.
    /// Both path and authority must still match; stale/disconnected reads remain
    /// queued for the replacement transport instead of installing empty data.
    fn handle_remote_buffer_content_loaded(
        &mut self,
        window_id: fresh_core::WindowId,
        buffer_id: fresh_core::BufferId,
        requested_path: std::path::PathBuf,
        filesystem_id: usize,
        content: Result<Vec<u8>, String>,
    ) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let (modified, current_path) = {
            let Some(state) = window.buffers.get(&buffer_id) else {
                return;
            };
            (
                state.buffer.is_modified(),
                state.buffer.file_path().map(|path| path.to_path_buf()),
            )
        };
        if modified || current_path.as_ref() != Some(&requested_path) {
            return;
        }

        let current_filesystem_id =
            crate::services::async_bridge::filesystem_identity(&window.authority().filesystem);
        let current_remote_disconnected = window
            .authority()
            .filesystem
            .remote_connection_info()
            .is_some()
            && !window.authority().filesystem.is_remote_connected();
        if current_filesystem_id != filesystem_id || current_remote_disconnected {
            if !window
                .pending_content_load
                .iter()
                .any(|(pending_buffer, pending_path)| {
                    *pending_buffer == buffer_id && pending_path == &requested_path
                })
            {
                window
                    .pending_content_load
                    .push((buffer_id, requested_path));
            }
            return;
        }

        let content = match content {
            Ok(content) => content,
            Err(error) => {
                tracing::warn!(
                    "remote buffer {buffer_id:?} content load failed for {}: {error}",
                    requested_path.display()
                );
                return;
            }
        };
        let state = window.build_workspace_file_state(&requested_path, Some(content));
        window.finalize_file_buffer(
            buffer_id,
            state,
            &requested_path,
            &requested_path,
            &requested_path,
            true,
        );
    }

    pub(crate) fn stop_remote_reconnect_forwarder(&mut self, connection_id: u64) {
        if let Some(forwarder) = self.remote_reconnect_forwarders.remove(&connection_id) {
            forwarder.abort();
        }
        self.remote_reconnect_generations.remove(&connection_id);
    }

    /// Ensure each remote channel has one owned task forwarding reconnect
    /// notifications onto the async bridge. Rebuilt authorities get a new
    /// channel id; close/replacement aborts the prior channel's task through
    /// [`Self::stop_remote_reconnect_forwarder`].
    fn ensure_remote_reconnect_forwarders(&mut self) {
        let (Some(runtime), Some(bridge)) =
            (self.tokio_runtime.as_ref(), self.async_bridge.as_ref())
        else {
            return;
        };
        type ReconnectWatch = (
            u64,
            std::sync::Arc<tokio::sync::Notify>,
            std::sync::Arc<std::sync::atomic::AtomicU64>,
        );
        // Collect first to avoid spawning while holding the `windows` borrow.
        let mut to_spawn: Vec<ReconnectWatch> = Vec::new();
        for window in self.windows.values() {
            let fs = &window.authority().filesystem;
            if let (Some(id), Some(notify), Some(generation)) = (
                fs.remote_channel_id(),
                fs.remote_reconnect_notify(),
                fs.remote_reconnect_generation_counter(),
            ) {
                if !self.remote_reconnect_forwarders.contains_key(&id) {
                    to_spawn.push((id, notify, generation));
                }
            }
        }
        for (id, notify, generation_counter) in to_spawn {
            let sender = bridge.sender();
            let forwarder = runtime.spawn(async move {
                loop {
                    notify.notified().await;
                    let generation = generation_counter.load(std::sync::atomic::Ordering::SeqCst);
                    if sender
                        .send(AsyncMessage::RemoteReconnected {
                            connection_id: id,
                            generation,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            self.remote_reconnect_forwarders.insert(id, forwarder);
        }
    }

    /// Detect remote-connection state changes (a link dropped or came back)
    /// across windows and report whether any changed since the last poll.
    ///
    /// The background reconnect task flips `is_remote_connected()` on its own
    /// timeline: a plain drop fires no async message at all, and the eventual
    /// settle after a flap may land between the `RemoteReconnected` events that
    /// do fire. Neither is tied to an input event, so without this poll the
    /// status-bar remote indicator goes stale — still reading "connected" after
    /// a drop, or "(Disconnected)" after the link came back — until the user
    /// happens to press a key. Called once per editor tick; when it returns
    /// `true` the caller re-renders. Cheap: a bool load per remote window, and
    /// no allocation at all when there are no remote windows.
    pub(crate) fn poll_remote_connection_changes(&mut self) -> bool {
        let current: std::collections::HashMap<fresh_core::WindowId, bool> = self
            .windows
            .iter()
            .filter_map(|(id, w)| {
                let fs = &w.authority().filesystem;
                // Only windows with a real remote authority matter; a local
                // filesystem's default `is_remote_connected() == true` is noise.
                fs.remote_connection_info()
                    .is_some()
                    .then(|| (*id, fs.is_remote_connected()))
            })
            .collect();
        if current != self.remote_connected_cache {
            self.remote_connected_cache = current;
            true
        } else {
            false
        }
    }

    /// Test-only seam: drive the reconnect dispatch directly, as if a
    /// `RemoteReconnected` event for `connection_id` had arrived on the bridge.
    /// Lets component tests exercise the id→window→reattach mapping without a
    /// live agent channel or tokio runtime.
    #[doc(hidden)]
    pub fn test_dispatch_remote_reconnected(&mut self, connection_id: u64) {
        let generation = self
            .windows
            .values()
            .find(|window| window.authority().filesystem.remote_channel_id() == Some(connection_id))
            .and_then(|window| window.authority().filesystem.remote_reconnect_generation())
            .unwrap_or(0);
        self.handle_remote_reconnected(connection_id, generation);
    }

    /// Map a reconnected agent channel (identified by its stable connection id)
    /// back to the window whose live authority owns it, and reattach. Driven by
    /// the background reconnect task via `AsyncMessage::RemoteReconnected`.
    fn handle_remote_reconnected(&mut self, connection_id: u64, generation: u64) {
        let Some(window_id) = self.windows.iter().find_map(|(id, w)| {
            (w.authority().filesystem.remote_channel_id() == Some(connection_id)).then_some(*id)
        }) else {
            // The window was closed, or its authority was swapped out from under
            // this connection — nothing to reattach.
            return;
        };
        // The channel publishes its generation before its reader may mark the
        // replacement transport connected. An older queued notify can arrive
        // after a newer hot-swap, so always fence on the authority's current
        // generation rather than trusting message order.
        let generation = generation.max(
            self.windows
                .get(&window_id)
                .and_then(|window| window.authority().filesystem.remote_reconnect_generation())
                .unwrap_or(generation),
        );
        if self
            .remote_reconnect_generations
            .get(&connection_id)
            .is_some_and(|seen| *seen >= generation)
        {
            return;
        }
        self.remote_reconnect_generations
            .insert(connection_id, generation);

        let pending = self
            .windows
            .get(&window_id)
            .map(|window| {
                window
                    .terminal_buffers
                    .values()
                    .map(|binding| binding.terminal_id)
                    .filter(|terminal_id| {
                        window
                            .terminal_manager
                            .get(*terminal_id)
                            .is_some_and(|handle| handle.is_alive())
                    })
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        tracing::info!(
            "agent channel {connection_id} generation {generation} reconnected; reattaching window {window_id}"
        );
        if pending.is_empty() {
            self.pending_remote_reattach.remove(&window_id);
            self.reattach_window(window_id);
        } else {
            // Every identity armed for reconnect is explicitly retired now.
            // Pending therefore means "an exit we requested for this exact
            // reconnect epoch", never "any currently-live terminal that might
            // exit normally hours later" and resurrect unexpectedly.
            let retiring: Vec<_> = pending.iter().copied().collect();
            self.pending_remote_reattach.insert(window_id, pending);
            if let Some(window) = self.windows.get_mut(&window_id) {
                for terminal_id in retiring {
                    window.terminal_manager.close(terminal_id);
                }
            }
        }
    }

    fn handle_terminal_exited(
        &mut self,
        terminal: fresh_core::WindowTerminalId,
        exit_code: Option<i32>,
    ) {
        tracing::info!("Terminal {} exited", terminal);
        if let Some(cleanup) = self.pending_terminal_artifact_cleanup.remove(&terminal) {
            self.schedule_terminal_artifact_cleanup(cleanup);
        }

        let explicitly_stopped = self.terminal_stop_tombstones.remove(&terminal);
        if explicitly_stopped {
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
        }

        // Revoke and purge before any terminal-exit hook can be queued. The
        // companion delegate retains a tombstone only for an already in-flight
        // hook, whose delayed name-only completion must not acknowledge a peer.
        self.purge_omp_companion_terminal(terminal);
        self.purge_terminal_output_hook(terminal);

        if !self.windows.contains_key(&terminal.window) {
            let Some(closing) = self.closing_windows.get_mut(&terminal.window) else {
                return;
            };
            if !closing.terminal_ids.remove(&terminal.terminal) {
                return;
            }
            let window_drained = closing.terminal_ids.is_empty();
            let invocation = closing.invocation.clone();
            self.plugin_manager
                .read()
                .unwrap()
                .run_hook_with_invocation(
                    "terminal_exit",
                    crate::services::plugins::hooks::HookArgs::TerminalExited {
                        terminal_id: terminal.terminal.0 as u64,
                        window_id: terminal.window.0,
                        exit_code,
                    },
                    Some(invocation.clone()),
                );
            if window_drained {
                self.closing_windows.remove(&terminal.window);
                self.plugin_manager
                    .read()
                    .unwrap()
                    .run_hook_with_invocation(
                        "window_closed",
                        crate::services::plugins::hooks::HookArgs::WindowClosed {
                            id: terminal.window.0,
                        },
                        Some(invocation),
                    );
            }
            return;
        }

        let pending_reconnect = self
            .pending_remote_reattach
            .get(&terminal.window)
            .is_some_and(|pending| pending.contains(&terminal.terminal));
        let reconnect_generation_pending = self
            .windows
            .get(&terminal.window)
            .and_then(|window| {
                let filesystem = &window.authority().filesystem;
                let connection_id = filesystem.remote_channel_id()?;
                let published = filesystem.remote_reconnect_generation()?;
                let handled = self
                    .remote_reconnect_generations
                    .get(&connection_id)
                    .copied()
                    .unwrap_or(0);
                Some(filesystem.is_remote_connected() && published > handled)
            })
            .unwrap_or(false);

        let Some(window) = self.windows.get_mut(&terminal.window) else {
            return;
        };
        if let Some(pid) = window
            .terminal_manager
            .get(terminal.terminal)
            .and_then(|handle| handle.pid())
        {
            let label = format!("terminal #{}", terminal.terminal.0);
            window.process_groups.forget_registration(pid, &label);
        }
        // The child is gone: invalidate its capability immediately. Keep only
        // the durable grant so an explicit restart can mint a fresh token.
        window.revoke_terminal_script_token(terminal.terminal, true);
        let filesystem = &window.authority().filesystem;
        let preserve_for_reconnect = !explicitly_stopped
            && (pending_reconnect
                || reconnect_generation_pending
                || (filesystem.remote_connection_info().is_some()
                    && !filesystem.is_remote_connected()));

        let finalized = window.finalize_terminal_exit_buffer(
            terminal.terminal,
            exit_code,
            preserve_for_reconnect,
        );
        if explicitly_stopped {
            if let Some(buffer_id) = finalized {
                window.exited_terminals.remove(&buffer_id);
            }
            window.revoke_terminal_script_token(terminal.terminal, false);
            window.terminal_commands.remove(&terminal.terminal);
            window.terminal_resume_commands.remove(&terminal.terminal);
            window.ephemeral_terminals.remove(&terminal.terminal);
        }
        if !preserve_for_reconnect {
            window.terminal_companions.remove(&terminal.terminal);
        }
        if finalized.is_some() {
            window.set_status_message(t!("terminal.exited", id = terminal.terminal.0).to_string());
        }
        window.terminal_manager.reap(terminal.terminal);

        self.run_plugin_hook_for_window(
            terminal.window,
            "terminal_exit",
            crate::services::plugins::hooks::HookArgs::TerminalExited {
                terminal_id: terminal.terminal.0 as u64,
                window_id: terminal.window.0,
                exit_code,
            },
        );

        if pending_reconnect {
            self.reattach_window(terminal.window);
            let old_identity_gone = self.windows.get(&terminal.window).is_none_or(|window| {
                !window
                    .terminal_buffers
                    .values()
                    .any(|binding| binding.terminal_id == terminal.terminal)
            });
            if old_identity_gone {
                let remove_window_entry = self
                    .pending_remote_reattach
                    .get_mut(&terminal.window)
                    .is_some_and(|pending| {
                        pending.remove(&terminal.terminal);
                        pending.is_empty()
                    });
                if remove_window_entry {
                    self.pending_remote_reattach.remove(&terminal.window);
                }
            }
        }
    }

    /// Install a completed remote connection only when its host attempt is
    /// still current for the exact plugin owner or reconnecting window.
    fn handle_remote_attach_ready(
        &mut self,
        ready: crate::services::async_bridge::RemoteAttachReady,
    ) {
        let crate::services::async_bridge::RemoteAttachReady {
            authority,
            keepalive,
            working_dir,
            mode,
            spec,
            restore_allowed,
            attempt_id,
        } = ready;
        let Some(owner) = self.settle_remote_attach_attempt(attempt_id) else {
            tracing::info!(attempt_id, "discarding stale remote attach completion");
            drop(keepalive);
            drop(authority);
            return;
        };
        if let RemoteAttachOwner::Plugin {
            plugin_instance_id, ..
        } = owner
        {
            let plugin_is_active = self
                .plugin_manager
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_plugin_instance_active(plugin_instance_id);
            if !plugin_is_active {
                tracing::info!(
                    ?plugin_instance_id,
                    attempt_id,
                    "discarding remote attach from an unloaded plugin instance"
                );
                drop(keepalive);
                drop(authority);
                return;
            }
        }
        let owner_matches_mode = match (owner, &mode) {
            (
                RemoteAttachOwner::Plugin { .. },
                crate::services::async_bridge::RemoteAttachMode::Restart,
            )
            | (
                RemoteAttachOwner::Plugin { .. },
                crate::services::async_bridge::RemoteAttachMode::Window { .. },
            ) => true,
            (
                RemoteAttachOwner::Reconnect {
                    window_id: owner_window,
                },
                crate::services::async_bridge::RemoteAttachMode::Reconnect { window_id },
            )
            | (
                RemoteAttachOwner::Switch {
                    window_id: owner_window,
                },
                crate::services::async_bridge::RemoteAttachMode::Switch { window_id },
            ) => owner_window == *window_id,
            _ => false,
        };
        if !owner_matches_mode {
            tracing::warn!(attempt_id, "remote attach completion owner/mode mismatch");
            if let RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                ..
            } = owner
            {
                self.reject_remote_attach(
                    plugin_instance_id,
                    request_id,
                    "remote attach completion mode mismatch".to_string(),
                );
            }
            drop(keepalive);
            drop(authority);
            return;
        }

        let plugin_request = match owner {
            RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                window_id,
            } => {
                if !self.windows.contains_key(&window_id) {
                    self.reject_remote_attach(
                        plugin_instance_id,
                        request_id,
                        "target window closed".to_string(),
                    );
                    drop(keepalive);
                    drop(authority);
                    return;
                }
                Some((plugin_instance_id, request_id, window_id))
            }
            RemoteAttachOwner::Reconnect { .. } | RemoteAttachOwner::Switch { .. } => None,
        };
        let root = working_dir
            .or_else(|| authority.filesystem.home_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        authority.workspace_trust.set_root(Some(root.clone()));

        match mode {
            crate::services::async_bridge::RemoteAttachMode::Restart => {
                let Some((plugin_instance_id, request_id, target_window)) = plugin_request else {
                    drop(keepalive);
                    drop(authority);
                    return;
                };
                if self.active_window != target_window {
                    self.reject_remote_attach(
                        plugin_instance_id,
                        request_id,
                        "target window is no longer active".to_string(),
                    );
                    drop(keepalive);
                    drop(authority);
                    return;
                }
                self.resolve_remote_attach(plugin_instance_id, request_id, None);
                self.active_window_mut().authority_spec = spec;
                self.install_authority_with_keepalive(authority, keepalive, root);
            }
            crate::services::async_bridge::RemoteAttachMode::Window {
                label,
                command,
                activate,
                initial_state,
            } => {
                let Some((plugin_instance_id, request_id, _)) = plugin_request else {
                    drop(keepalive);
                    drop(authority);
                    return;
                };
                match self.create_remote_session_window(
                    authority,
                    keepalive,
                    root,
                    label,
                    command,
                    activate,
                    spec,
                    initial_state,
                ) {
                    Ok(window_id) => {
                        self.resolve_remote_attach(plugin_instance_id, request_id, Some(window_id));
                    }
                    Err(error) => {
                        self.reject_remote_attach(plugin_instance_id, request_id, error);
                    }
                }
            }
            crate::services::async_bridge::RemoteAttachMode::Reconnect { window_id } => {
                if self.dormant_remote.contains_key(&window_id) {
                    self.promote_dormant_remote(
                        window_id,
                        authority,
                        keepalive,
                        root,
                        spec,
                        restore_allowed,
                    );
                } else if self.windows.contains_key(&window_id) {
                    if restore_allowed {
                        if let Some(connection_id) = self.windows[&window_id]
                            .authority()
                            .filesystem
                            .remote_channel_id()
                        {
                            self.stop_remote_reconnect_forwarder(connection_id);
                        }
                        self.set_session_authority_spec(window_id, spec);
                        self.set_session_authority(window_id, authority);
                        self.session_keepalives.insert(window_id, keepalive);
                        self.reattach_window(window_id);
                    } else {
                        self.replace_remote_window_without_restore(
                            window_id, authority, keepalive, root, None, spec,
                        );
                    }
                } else {
                    drop(authority);
                    drop(keepalive);
                }
            }
            crate::services::async_bridge::RemoteAttachMode::Switch { window_id } => {
                let label = root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.to_string_lossy().into_owned());
                self.replace_remote_window_without_restore(
                    window_id,
                    authority,
                    keepalive,
                    root,
                    Some(label),
                    spec,
                );
            }
        }
    }

    /// Surface a failure only to the attempt's exact owner. A late completion
    /// for a cancelled/retried/closed attempt has no owner entry and vanishes.
    fn handle_remote_attach_failed(&mut self, error: String, attempt_id: u64) {
        let Some(owner) = self.settle_remote_attach_attempt(attempt_id) else {
            tracing::info!(attempt_id, "discarding stale remote attach failure");
            return;
        };
        if let RemoteAttachOwner::Plugin {
            plugin_instance_id, ..
        } = owner
        {
            let plugin_is_active = self
                .plugin_manager
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_plugin_instance_active(plugin_instance_id);
            if !plugin_is_active {
                tracing::info!(
                    ?plugin_instance_id,
                    attempt_id,
                    "discarding remote attach failure from an unloaded plugin instance"
                );
                return;
            }
        }
        tracing::warn!(attempt_id, "Remote attach failed: {error}");
        let reason = error.lines().next().unwrap_or(&error).to_string();
        match owner {
            RemoteAttachOwner::Plugin {
                plugin_instance_id,
                request_id,
                window_id,
            } => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.set_status_message(format!("Connection failed: {reason}"));
                }
                self.reject_remote_attach(plugin_instance_id, request_id, error);
            }
            RemoteAttachOwner::Reconnect { window_id } => {
                if self.dormant_remote.contains_key(&window_id)
                    && !self.windows.contains_key(&window_id)
                {
                    self.ensure_dormant_shell(window_id);
                }
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.remote_reconnect_error = Some(reason.clone());
                    window.set_status_message(format!("Connection failed: {reason}"));
                }
            }
            RemoteAttachOwner::Switch { window_id } => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.set_status_message(format!("Project switch failed: {reason}"));
                }
            }
        }
    }

    /// Swap in a freshly-built grammar registry, re-detect syntax for open
    /// buffers, and resolve any plugin callbacks that awaited the build.
    fn handle_grammar_registry_built(
        &mut self,
        registry: std::sync::Arc<crate::primitives::grammar::GrammarRegistry>,
        callback_ids: Vec<fresh_core::api::JsCallbackId>,
    ) {
        tracing::info!(
            "Background grammar build completed ({} syntaxes)",
            registry.available_syntaxes().len()
        );
        // Merge user `[languages]` config into the catalog so
        // find_by_path honours user globs/filenames/extensions.
        // The background thread just sent the Arc through the
        // channel, so we're the sole owner here. Assert rather
        // than silently drop config.
        let mut registry = registry;
        crate::primitives::grammar::GrammarRegistry::apply_languages(
            &mut registry,
            &self.config.languages,
        );
        crate::config::reload_indent_overrides(&self.config.languages);
        self.grammar_registry = registry;
        // Propagate the new grammar registry to every window's
        // resources so window-side syntax detection picks up the
        // freshly-built grammars without waiting for a restart.
        for w in self.windows.values_mut() {
            w.resources.grammar_registry = self.grammar_registry.clone();
        }
        self.grammar_build_in_progress = false;

        // Re-detect syntax for all open buffers with the full registry
        let buffers_to_update: Vec<_> = self
            .active_window()
            .buffer_metadata
            .iter()
            .filter_map(|(id, meta)| meta.file_path().map(|p| (*id, p.to_path_buf())))
            .collect();

        for (buf_id, path) in buffers_to_update {
            if let Some(state) = self
                .windows
                .get_mut(&self.active_window)
                .map(|w| &mut w.buffers)
                .expect("active window present")
                .get_mut(&buf_id)
            {
                let first_line = state.buffer.first_line_lossy();
                let detected = crate::primitives::detected_language::DetectedLanguage::from_path(
                    &path,
                    first_line.as_deref(),
                    &self.grammar_registry,
                    &self.config.languages,
                );

                if detected.highlighter.has_highlighting() || !state.highlighter.has_highlighting()
                {
                    state.apply_language(detected);
                    state.apply_buffer_config(&self.config);
                }
            }
        }

        // Resolve plugin callbacks that were waiting for this build
        #[cfg(feature = "plugins")]
        for cb_id in callback_ids {
            self.plugin_manager
                .read()
                .unwrap()
                .resolve_callback(cb_id, "null".to_string());
        }

        // Flush any plugin grammars that arrived during the build
        self.flush_pending_grammars();
    }

    /// Update the Quick Open file cache from a background scan and refresh the
    /// open prompt's suggestions.
    fn handle_quick_open_files_loaded(
        &mut self,
        cwd: String,
        files: std::sync::Arc<Vec<crate::input::quick_open::providers::FileEntry>>,
        complete: bool,
    ) {
        // Update the file provider cache and refresh suggestions
        // if Quick Open is currently showing file mode (empty prefix).
        if let Some((provider, _)) = self.quick_open_registry.get_provider_for_input("") {
            if let Some(fp) = provider
                .as_any()
                .downcast_ref::<crate::input::quick_open::providers::FileProvider>()
            {
                if complete {
                    fp.set_cache(&cwd, files);
                } else {
                    fp.set_partial_cache(&cwd, files);
                }
            }
        }
        // Refresh the Quick Open suggestions if the prompt is open
        if let Some(prompt) = &self.active_window_mut().prompt {
            if prompt.prompt_type == PromptType::QuickOpen {
                let input = prompt.input.clone();
                self.update_quick_open_suggestions(&input);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use std::sync::Arc;

    fn test_editor() -> Editor {
        let temp = tempfile::tempdir().unwrap();
        let dir_context = DirectoryContext::for_testing(temp.path());
        // Keep the temp dir alive for the editor's lifetime.
        std::mem::forget(temp);
        // Plugins disabled: an enabled plugin can set its own status on its
        // first tick (the bundled i18n test plugin does), which would clobber
        // the status this test asserts on. The handler under test is core, not
        // plugin-gated, so this isolates it cleanly.
        Editor::for_test(
            Config::default(),
            80,
            24,
            None,
            dir_context,
            crate::view::color_support::ColorCapability::TrueColor,
            Arc::new(crate::model::filesystem::StdFileSystem),
            None,
            None,
            false,
            false,
        )
        .unwrap()
    }

    #[test]
    fn same_numeric_terminal_exit_routes_buffer_status_and_self_update_by_window() {
        let mut editor = test_editor();
        let active = editor.active_window;
        let second_root = tempfile::tempdir().unwrap();
        let second =
            editor.create_window_at(second_root.path().to_path_buf(), "second".to_string());
        std::mem::forget(second_root);

        let terminal_id = fresh_core::TerminalId(0);
        let active_buffer = editor
            .windows
            .get_mut(&active)
            .unwrap()
            .create_terminal_buffer_detached(terminal_id);
        let second_buffer = editor
            .windows
            .get_mut(&second)
            .unwrap()
            .create_terminal_buffer_detached(terminal_id);
        let active_terminal = fresh_core::WindowTerminalId::new(active, terminal_id);
        let second_terminal = fresh_core::WindowTerminalId::new(second, terminal_id);

        editor.begin_self_update(active_terminal, active_buffer);
        let active_status = editor.windows.get(&active).unwrap().status_message.clone();
        editor
            .async_bridge
            .as_ref()
            .unwrap()
            .sender()
            .send(AsyncMessage::TerminalExited {
                terminal: second_terminal,
                exit_code: Some(0),
            })
            .unwrap();
        editor.process_async_messages();
        assert_eq!(editor.active_window, active);

        assert_eq!(editor.self_update_terminal, Some(active_terminal));
        assert_eq!(
            editor.self_update_phase,
            crate::services::release_checker::SelfUpdatePhase::Running
        );
        assert!(editor
            .windows
            .get(&active)
            .unwrap()
            .terminal_buffers
            .contains_key(&active_buffer));
        assert!(!editor
            .windows
            .get(&second)
            .unwrap()
            .terminal_buffers
            .contains_key(&second_buffer));
        assert_eq!(
            editor.windows.get(&active).unwrap().status_message,
            active_status
        );
        assert!(editor
            .windows
            .get(&second)
            .unwrap()
            .status_message
            .is_some());
        assert!(!editor.omp_companion_delivery.is_tombstoned(active_terminal));
        assert!(!editor.omp_companion_delivery.is_tombstoned(second_terminal));

        editor
            .async_bridge
            .as_ref()
            .unwrap()
            .sender()
            .send(AsyncMessage::TerminalExited {
                terminal: active_terminal,
                exit_code: Some(0),
            })
            .unwrap();
        editor.process_async_messages();

        assert_eq!(editor.self_update_terminal, None);
        assert_eq!(
            editor.self_update_phase,
            crate::services::release_checker::SelfUpdatePhase::Succeeded
        );
        assert!(!editor.omp_companion_delivery.is_tombstoned(active_terminal));
        assert!(!editor.omp_companion_delivery.is_tombstoned(second_terminal));
        assert!(!editor
            .windows
            .get(&active)
            .unwrap()
            .terminal_buffers
            .contains_key(&active_buffer));
    }

    #[test]
    fn dive_reconnect_failure_records_error_on_its_window() {
        let mut editor = test_editor();
        let win = editor.active_window;
        let attempt_id = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Reconnect { window_id: win })
            .unwrap();

        editor
            .async_bridge
            .as_ref()
            .unwrap()
            .sender()
            .send(AsyncMessage::RemoteAttachFailed {
                error: "Agent failed to start: SSH could not connect\nsecond line".to_string(),
                attempt_id,
            })
            .unwrap();
        editor.process_async_messages();

        assert_eq!(
            editor
                .windows
                .get(&win)
                .unwrap()
                .remote_reconnect_error
                .as_deref(),
            Some("Agent failed to start: SSH could not connect")
        );
    }

    #[test]
    fn project_switch_failure_keeps_live_window_connected() {
        let mut editor = test_editor();
        let window_id = editor.active_window;
        let attempt_id = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Switch { window_id })
            .unwrap();

        editor.handle_remote_attach_failed("replacement host refused".to_string(), attempt_id);

        let window = editor.windows.get(&window_id).unwrap();
        assert!(window.remote_reconnect_error.is_none());
        assert_eq!(
            window.status_message.as_deref(),
            Some("Project switch failed: replacement host refused")
        );
    }

    #[test]
    fn unloaded_plugin_attach_failure_has_no_window_side_effect() {
        let mut editor = test_editor();
        let active = editor.active_window;
        let second_root = tempfile::tempdir().unwrap();
        let target = editor.create_window_at(second_root.path().to_path_buf(), "second".into());
        std::mem::forget(second_root);
        editor.active_window = active;
        let active_status = editor.windows.get(&active).unwrap().status_message.clone();
        let target_status = editor.windows.get(&target).unwrap().status_message.clone();
        let unloaded_instance = fresh_core::api::PluginInstanceId::fresh();
        let attempt_id = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Plugin {
                plugin_instance_id: unloaded_instance,
                request_id: 7,
                window_id: target,
            })
            .unwrap();

        editor.handle_remote_attach_failed("boom".to_string(), attempt_id);

        assert_eq!(
            editor.windows.get(&active).unwrap().status_message,
            active_status
        );
        assert_eq!(
            editor.windows.get(&target).unwrap().status_message,
            target_status
        );
        assert!(!editor.remote_attach_attempts.contains_key(&attempt_id));
    }

    #[test]
    fn same_request_id_cancels_only_the_owning_plugin_attempt() {
        let mut editor = test_editor();
        let window_id = editor.active_window;
        let owner_a = fresh_core::api::PluginInstanceId::fresh();
        let owner_b = fresh_core::api::PluginInstanceId::fresh();
        let attempt_a = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Plugin {
                plugin_instance_id: owner_a,
                request_id: 9,
                window_id,
            })
            .unwrap();
        let attempt_b = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Plugin {
                plugin_instance_id: owner_b,
                request_id: 9,
                window_id,
            })
            .unwrap();
        let (cancel_a, mut cancelled_a) = tokio::sync::oneshot::channel();
        let (cancel_b, mut cancelled_b) = tokio::sync::oneshot::channel();
        editor.remote_attach_cancels.insert(attempt_a, cancel_a);
        editor.remote_attach_cancels.insert(attempt_b, cancel_b);

        editor.cancel_plugin_remote_attach(owner_a, 9);

        assert!(!editor.remote_attach_attempts.contains_key(&attempt_a));
        assert!(editor.remote_attach_attempts.contains_key(&attempt_b));
        assert_eq!(cancelled_a.try_recv(), Ok(()));
        assert!(matches!(
            cancelled_b.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn plugin_unload_cancels_all_old_instance_attaches_only() {
        let mut editor = test_editor();
        let window_id = editor.active_window;
        let unloaded = fresh_core::api::PluginInstanceId::fresh();
        let reloaded = fresh_core::api::PluginInstanceId::fresh();
        let old_a = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Plugin {
                plugin_instance_id: unloaded,
                request_id: 10,
                window_id,
            })
            .unwrap();
        let old_b = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Plugin {
                plugin_instance_id: unloaded,
                request_id: 11,
                window_id,
            })
            .unwrap();
        let new_request = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Plugin {
                plugin_instance_id: reloaded,
                request_id: 10,
                window_id,
            })
            .unwrap();
        let (cancel_old_a, mut cancelled_old_a) = tokio::sync::oneshot::channel();
        let (cancel_old_b, mut cancelled_old_b) = tokio::sync::oneshot::channel();
        let (cancel_new, mut cancelled_new) = tokio::sync::oneshot::channel();
        editor.remote_attach_cancels.insert(old_a, cancel_old_a);
        editor.remote_attach_cancels.insert(old_b, cancel_old_b);
        editor.remote_attach_cancels.insert(new_request, cancel_new);

        editor.cancel_plugin_remote_attaches(unloaded);

        assert!(!editor.remote_attach_attempts.contains_key(&old_a));
        assert!(!editor.remote_attach_attempts.contains_key(&old_b));
        assert!(editor.remote_attach_attempts.contains_key(&new_request));
        assert_eq!(cancelled_old_a.try_recv(), Ok(()));
        assert_eq!(cancelled_old_b.try_recv(), Ok(()));
        assert!(matches!(
            cancelled_new.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn cancelled_reconnect_completion_cannot_clobber_new_generation() {
        let mut editor = test_editor();
        let window_id = editor.active_window;
        let stale = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Reconnect { window_id })
            .unwrap();
        editor.cancel_remote_reconnect(window_id);
        let current = editor
            .begin_remote_attach_attempt(RemoteAttachOwner::Reconnect { window_id })
            .unwrap();

        editor.handle_remote_attach_failed("stale".to_string(), stale);

        assert_eq!(
            editor.remote_reconnect_attempts.get(&window_id),
            Some(&current)
        );
        assert!(editor
            .windows
            .get(&window_id)
            .unwrap()
            .remote_reconnect_error
            .is_none());
    }

    #[test]
    fn completion_response_keeps_window_ownership_through_backlog() {
        let mut editor = test_editor();
        let source = editor.active_window;
        let second_root = tempfile::tempdir().unwrap();
        let active = editor.create_window_at(second_root.path().to_path_buf(), "second".into());
        std::mem::forget(second_root);
        editor.active_window = active;

        editor
            .windows
            .get_mut(&source)
            .unwrap()
            .pending_completion_requests
            .insert(0);
        editor
            .windows
            .get_mut(&active)
            .unwrap()
            .pending_completion_requests
            .insert(0);
        editor.async_message_backlog.push_back(
            crate::services::async_bridge::AsyncMessageEnvelope::Window(
                source,
                AsyncMessage::LspCompletion {
                    request_id: 0,
                    items: vec![lsp_types::CompletionItem {
                        label: "owned-by-source".to_string(),
                        ..Default::default()
                    }],
                },
            ),
        );

        editor.process_async_messages();

        assert_eq!(editor.active_window, active);
        let source_window = editor.windows.get(&source).unwrap();
        assert!(!source_window.pending_completion_requests.contains(&0));
        assert_eq!(
            source_window
                .completion_items
                .as_ref()
                .unwrap()
                .first()
                .unwrap()
                .label,
            "owned-by-source"
        );
        let active_window = editor.windows.get(&active).unwrap();
        assert!(active_window.pending_completion_requests.contains(&0));
        assert!(active_window.completion_items.is_none());
    }

    #[test]
    fn apply_edit_uses_source_window_path_authority_without_changing_focus() {
        use crate::services::authority::{Authority, PathTranslation};
        use lsp_types::{Position, Range, TextEdit, WorkspaceEdit};

        let mut editor = test_editor();
        let source_root = tempfile::tempdir().unwrap();
        let active_root = tempfile::tempdir().unwrap();
        let source_path = source_root.path().join("target.txt");
        let active_path = active_root.path().join("target.txt");
        std::fs::write(&source_path, "alpha").unwrap();
        std::fs::write(&active_path, "bravo").unwrap();
        let remote_root = std::path::PathBuf::from("/workspace");

        let mut source_authority =
            Authority::local_scoped(editor.session_scope_for(source_root.path()));
        source_authority.path_translation = Some(PathTranslation {
            host_root: source_root.path().to_path_buf(),
            remote_root: remote_root.clone(),
        });
        let source = editor.create_window_with_authority(
            source_root.path().to_path_buf(),
            "source".into(),
            source_authority,
        );
        let mut active_authority =
            Authority::local_scoped(editor.session_scope_for(active_root.path()));
        active_authority.path_translation = Some(PathTranslation {
            host_root: active_root.path().to_path_buf(),
            remote_root,
        });
        let active = editor.create_window_with_authority(
            active_root.path().to_path_buf(),
            "active".into(),
            active_authority,
        );
        let source_buffer = editor
            .windows
            .get_mut(&source)
            .unwrap()
            .open_file_no_focus(&source_path)
            .unwrap();
        let active_buffer = editor
            .windows
            .get_mut(&active)
            .unwrap()
            .open_file_no_focus(&active_path)
            .unwrap();
        let source_focus = editor.windows.get(&source).unwrap().active_buffer();
        let active_focus = editor.windows.get(&active).unwrap().active_buffer();
        editor.active_window = active;

        let uri: lsp_types::Uri = "file:///workspace/target.txt".parse().unwrap();
        let edit = WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(
                uri,
                vec![TextEdit {
                    range: Range::new(Position::new(0, 0), Position::new(0, 5)),
                    new_text: "source-only".to_string(),
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        };
        editor
            .windows
            .get(&source)
            .unwrap()
            .bridge
            .sender()
            .send(AsyncMessage::LspApplyEdit { edit, label: None })
            .unwrap();

        editor.process_async_messages();

        assert_eq!(editor.active_window, active);
        assert_eq!(
            editor.windows.get(&source).unwrap().active_buffer(),
            source_focus
        );
        assert_eq!(
            editor.windows.get(&active).unwrap().active_buffer(),
            active_focus
        );
        assert_eq!(
            editor.windows[&source]
                .buffers
                .get(&source_buffer)
                .unwrap()
                .buffer
                .to_string()
                .as_deref(),
            Some("source-only")
        );
        assert_eq!(
            editor.windows[&active]
                .buffers
                .get(&active_buffer)
                .unwrap()
                .buffer
                .to_string()
                .as_deref(),
            Some("bravo")
        );
    }

    #[test]
    fn explicit_stop_tombstone_wins_over_pending_remote_reattach() {
        let mut editor = test_editor();
        let window_id = editor.active_window;
        let terminal_id = fresh_core::TerminalId(17);
        let terminal = fresh_core::WindowTerminalId::new(window_id, terminal_id);
        let buffer_id = editor
            .windows
            .get_mut(&window_id)
            .unwrap()
            .create_terminal_buffer_detached(terminal_id);
        {
            let window = editor.windows.get_mut(&window_id).unwrap();
            window
                .terminal_commands
                .insert(terminal_id, vec!["agent".to_string()]);
            window.terminal_resume_commands.insert(
                terminal_id,
                vec!["agent".to_string(), "--resume".to_string()],
            );
            window.remember_terminal_script_access(terminal_id);
        }
        editor
            .pending_remote_reattach
            .insert(window_id, std::collections::HashSet::from([terminal_id]));
        editor.terminal_stop_tombstones.insert(terminal);

        editor.handle_terminal_exited(terminal, Some(143));

        let window = editor.windows.get(&window_id).unwrap();
        assert!(!window.terminal_buffers.contains_key(&buffer_id));
        assert!(!window.terminal_commands.contains_key(&terminal_id));
        assert!(!window.terminal_resume_commands.contains_key(&terminal_id));
        assert!(!window.terminal_has_script_access(terminal_id));
        assert!(!editor.pending_remote_reattach.contains_key(&window_id));
        assert!(!editor.terminal_stop_tombstones.contains(&terminal));
    }
    #[test]
    fn terminal_output_delivery_is_fifo_fair_latest_only_and_bounded() {
        let a =
            fresh_core::WindowTerminalId::new(fresh_core::WindowId(1), fresh_core::TerminalId(7));
        let b =
            fresh_core::WindowTerminalId::new(fresh_core::WindowId(2), fresh_core::TerminalId(7));
        let payload = |terminal, line: &str| TerminalOutputHookPayload {
            terminal,
            last_line: line.to_string(),
            terminal_title: String::new(),
            osc_activity: None,
        };
        let mut delivery = TerminalOutputHookDelivery::default();

        assert!(delivery.push(payload(a, "a1")));
        assert!(delivery.push(payload(b, "b1")));
        assert_eq!(delivery.take_next().unwrap().terminal, a);

        for line in 2..=1_000 {
            assert!(delivery.push(payload(a, &format!("a{line}"))));
        }
        assert!(delivery.push(payload(b, "b2")));
        assert_eq!(delivery.pending_len(), 2);
        delivery.complete_in_flight();

        let b_latest = delivery.take_next().unwrap();
        assert_eq!(b_latest.terminal, b);
        assert_eq!(b_latest.last_line, "b2");
        delivery.complete_in_flight();

        let a_latest = delivery.take_next().unwrap();
        assert_eq!(a_latest.terminal, a);
        assert_eq!(a_latest.last_line, "a1000");
        delivery.complete_in_flight();
        assert!(delivery.take_next().is_none());
        assert_eq!(delivery.pending_len(), 0);
    }

    #[test]
    fn terminal_output_messages_coalesce_by_full_terminal_identity_per_frame() {
        let a =
            fresh_core::WindowTerminalId::new(fresh_core::WindowId(1), fresh_core::TerminalId(7));
        let b =
            fresh_core::WindowTerminalId::new(fresh_core::WindowId(2), fresh_core::TerminalId(7));
        let mut messages = (0..1_000)
            .map(|_| AsyncMessageEnvelope::Global(AsyncMessage::TerminalOutput { terminal: a }))
            .collect::<Vec<_>>();
        messages.push(AsyncMessageEnvelope::Global(AsyncMessage::TerminalOutput {
            terminal: b,
        }));

        coalesce_terminal_output_messages(&mut messages);

        let terminals = messages
            .iter()
            .filter_map(|envelope| match envelope.message() {
                AsyncMessage::TerminalOutput { terminal } => Some(*terminal),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(terminals, vec![a, b]);
    }

    #[test]
    fn unsubscribed_window_hook_does_not_build_or_enqueue_an_invocation() {
        let mut editor = test_editor();
        let window_id = editor.active_window;

        assert!(!editor.run_plugin_hook_for_window(
            window_id,
            "terminal_output",
            fresh_core::hooks::HookArgs::TerminalOutput {
                terminal_id: 1,
                window_id: window_id.0,
                last_line: String::new(),
                terminal_title: String::new(),
                osc_activity: None,
            },
        ));
    }
}
