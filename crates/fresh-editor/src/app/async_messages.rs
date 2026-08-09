//! Async message handlers for the Editor
//!
//! This module contains handlers for AsyncMessage variants, grouped by domain:
//! - LSP diagnostics (push and pull models)
//! - LSP feature responses (inlay hints, progress, status)
//! - File system events
//! - File explorer events
//! - Plugin events

use crate::model::buffer::Buffer;
use crate::model::event::BufferId;
use crate::services::async_bridge::{
    LspMessageType, LspProgressValue, LspSemanticTokensResponse, LspServerStatus,
};
use crate::services::lsp::diagnostics::AnchoredDiagnostic;
use crate::state::{SemanticTokenSpan, SemanticTokenStore};
use crate::view::file_tree::{FileTreeView, NodeId};
use lsp_types::{
    Diagnostic, FoldingRange, InlayHint, SemanticToken, SemanticTokensEdit,
    SemanticTokensFullDeltaResult, SemanticTokensLegend, SemanticTokensRangeResult,
    SemanticTokensResult,
};
use rust_i18n::t;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::types::{LspMessageEntry, LspProgressInfo};
use super::Editor;

// =============================================================================
// Shared Helpers
// =============================================================================

impl Editor {
    /// Find a buffer by its LSP URI
    ///
    /// This is a common pattern used by diagnostics, inlay hints, and other LSP handlers
    pub(super) fn find_buffer_by_uri(
        &self,
        window_id: fresh_core::WindowId,
        uri: &str,
    ) -> Option<BufferId> {
        // The incoming URI string came over the LSP wire (e.g. a
        // `publishDiagnostics` notification), so it's already in the
        // server's coordinate space. `BufferMetadata.file_uri` is also
        // wire-side ([`LspUri`]), so a string comparison is the right
        // primitive here — both sides are translated identically and
        // we never accidentally compare a host URI to a wire URI.
        self.windows
            .get(&window_id)?
            .buffer_metadata
            .iter()
            .find(|(_, m)| m.file_uri().map(|u| u.as_str() == uri).unwrap_or(false))
            .map(|(buffer_id, _)| *buffer_id)
    }

    /// Collect `(buffer_id, uri)` pairs for every open buffer whose stored
    /// language matches `language`.
    ///
    /// This is the single correct way to enumerate buffers for sending a
    /// per-URI LSP request (pull diagnostics, inlay hints, semantic tokens,
    /// folding ranges, …) to a language-scoped server. Without the language
    /// filter, a server configured only for e.g. "rust" ends up receiving
    /// requests for every open URI regardless of type, and a responsible
    /// server rejects unknown URIs with `file not found (code -32603)` —
    /// polluting logs and wasting a round-trip per unrelated buffer.
    ///
    /// Callers that need richer per-buffer info (line counts, content, file
    /// paths) can still iterate themselves, but should use the same
    /// `state.language == language` predicate this helper encodes.
    pub(crate) fn buffers_for_language(
        &self,
        language: &str,
    ) -> Vec<(BufferId, crate::app::types::LspUri)> {
        self.buffers_for_language_in_window(self.active_window, language)
    }

    pub(crate) fn buffers_for_language_in_window(
        &self,
        window_id: fresh_core::WindowId,
        language: &str,
    ) -> Vec<(BufferId, crate::app::types::LspUri)> {
        let Some(window) = self.windows.get(&window_id) else {
            return Vec::new();
        };
        window
            .buffers
            .iter()
            .filter_map(|(buffer_id, state)| {
                if state.language != language {
                    return None;
                }
                window
                    .buffer_metadata
                    .get(buffer_id)
                    .and_then(|m| m.file_uri().cloned())
                    .map(|uri| (*buffer_id, uri))
            })
            .collect()
    }

    /// Apply diagnostics to a buffer identified by URI.
    /// Returns `(buffer_id, actually_updated)` if buffer was found, None otherwise.
    /// `actually_updated` is false when the DIAG CACHE determined no overlay changes were needed.
    fn apply_diagnostics_to_buffer(
        &mut self,
        window_id: fresh_core::WindowId,
        uri: &str,
        diagnostics: &[Diagnostic],
    ) -> Option<(BufferId, bool)> {
        let buffer_id = self.find_buffer_by_uri(window_id, uri)?;
        let state = self
            .windows
            .get_mut(&window_id)?
            .buffers
            .get_mut(&buffer_id)?;
        let updated = crate::services::lsp::diagnostics::apply_diagnostics_to_state_cached(
            state,
            diagnostics,
            &self.theme.read().unwrap(),
        );
        Some((buffer_id, updated))
    }
}

// =============================================================================
// LSP Diagnostics Handlers
// =============================================================================

impl Editor {
    /// Anchor freshly received diagnostics to the open buffer for `uri` (if
    /// any), stamping each with the buffer's current version so `CoordMap` can
    /// carry it forward across later edits (#2602).
    fn anchor_diagnostics(
        &self,
        window_id: fresh_core::WindowId,
        uri: &str,
        diagnostics: Vec<Diagnostic>,
    ) -> Vec<AnchoredDiagnostic> {
        let state = self.find_buffer_by_uri(window_id, uri).and_then(|id| {
            self.windows
                .get(&window_id)
                .and_then(|w| w.buffers.get(&id))
        });
        diagnostics
            .into_iter()
            .map(|d| AnchoredDiagnostic::capture(d, state))
            .collect()
    }

    /// Materialise the merged push + pull view (positions mapped to the buffer's
    /// current version) and rebuild the overlays from it.
    fn merge_and_apply_diagnostics(&mut self, window_id: fresh_core::WindowId, uri: &str) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let merged = window.recompute_merged_diagnostics(uri);

        if let Some((buffer_id, updated)) =
            self.apply_diagnostics_to_buffer(window_id, uri, &merged)
        {
            if updated {
                tracing::info!(
                    "Applied {} diagnostics to buffer {:?} (overlays updated)",
                    merged.len(),
                    buffer_id
                );
            } else {
                tracing::debug!(
                    "Diagnostics unchanged for buffer {:?} ({} diagnostics, cache hit)",
                    buffer_id,
                    merged.len()
                );
            }
        } else {
            tracing::debug!("No buffer found for diagnostic URI: {}", uri);
        }

        // Emit diagnostics_updated hook for plugins
        let count = merged.len();
        self.run_plugin_hook_for_window(
            window_id,
            "diagnostics_updated",
            crate::services::plugins::hooks::HookArgs::DiagnosticsUpdated {
                uri: uri.to_string(),
                count,
            },
        );
    }

    /// Handle LSP diagnostics (push model — publishDiagnostics from flycheck/cargo)
    pub(super) fn handle_lsp_diagnostics(
        &mut self,
        window_id: fresh_core::WindowId,
        uri: String,
        diagnostics: Vec<Diagnostic>,
        server_name: String,
    ) {
        // Discard diagnostics from servers that have been shut down.  The async
        // bridge may still contain queued messages from a server that was stopped
        // between the time it sent the notification and when we drain the channel.
        if let Some(lsp) = self.windows.get(&window_id).map(|window| &window.lsp) {
            if !lsp.has_server_named(&server_name) {
                tracing::debug!(
                    "Dropping diagnostics from stopped server '{}' for {}",
                    server_name,
                    uri
                );
                return;
            }
        }

        tracing::debug!(
            "Processing {} push diagnostics from '{}' for {}",
            diagnostics.len(),
            server_name,
            uri
        );

        let anchored = self.anchor_diagnostics(window_id, &uri, diagnostics);
        let server_map = self
            .windows
            .get_mut(&window_id)
            .expect("source window exists")
            .stored_push_diagnostics
            .entry(uri.clone())
            .or_default();
        if anchored.is_empty() {
            server_map.remove(&server_name);
            // Clean up empty outer entry
            if server_map.is_empty() {
                self.windows
                    .get_mut(&window_id)
                    .unwrap()
                    .stored_push_diagnostics
                    .remove(&uri);
            }
        } else {
            server_map.insert(server_name, anchored);
        }

        self.merge_and_apply_diagnostics(window_id, &uri);
    }

    /// Handle LSP pulled diagnostics (pull model — native RA diagnostics, LSP 3.17+)
    pub(super) fn handle_lsp_pulled_diagnostics(
        &mut self,
        window_id: fresh_core::WindowId,
        uri: String,
        server_name: String,
        result_id: Option<String>,
        diagnostics: Vec<Diagnostic>,
        unchanged: bool,
    ) {
        // Drop reports from servers that have since been shut down, matching
        // the push path — queued messages can outlive the server.
        if let Some(lsp) = self.windows.get(&window_id).map(|window| &window.lsp) {
            if !lsp.has_server_named(&server_name) {
                tracing::debug!(
                    "Dropping pulled diagnostics from stopped server '{}' for {}",
                    server_name,
                    uri
                );
                return;
            }
        }

        // Store result_id (per-server) even on an unchanged report so the
        // next pull for this server keeps sending the correct cursor.
        if let Some(result_id) = result_id {
            self.windows
                .get_mut(&window_id)
                .unwrap()
                .diagnostic_result_ids
                .entry(uri.clone())
                .or_default()
                .insert(server_name.clone(), result_id);
        }

        if unchanged {
            tracing::debug!("Diagnostics unchanged for {} from '{}'", uri, server_name);
            return;
        }

        tracing::debug!(
            "Processing {} pulled diagnostics for {} from '{}'",
            diagnostics.len(),
            uri,
            server_name
        );

        let anchored = self.anchor_diagnostics(window_id, &uri, diagnostics);
        let server_map = self
            .windows
            .get_mut(&window_id)
            .unwrap()
            .stored_pull_diagnostics
            .entry(uri.clone())
            .or_default();
        if anchored.is_empty() {
            server_map.remove(&server_name);
            if server_map.is_empty() {
                self.windows
                    .get_mut(&window_id)
                    .unwrap()
                    .stored_pull_diagnostics
                    .remove(&uri);
            }
        } else {
            server_map.insert(server_name, anchored);
        }

        self.merge_and_apply_diagnostics(window_id, &uri);
    }

    /// Clear all diagnostics originating from a specific server.
    ///
    /// Removes the server's entries from both `stored_push_diagnostics`
    /// and `stored_pull_diagnostics` (plus its per-server result_id), then
    /// re-merges and re-applies diagnostics for every affected URI so that
    /// overlays on screen are updated immediately.
    pub(crate) fn clear_diagnostics_for_server(&mut self, server_name: &str) {
        self.clear_diagnostics_for_server_in_window(self.active_window, server_name);
    }

    pub(crate) fn clear_diagnostics_for_server_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        server_name: &str,
    ) {
        // Collect URIs that have push or pull diagnostics from this server.
        let mut affected_uris: Vec<String> = {
            let Some(win) = self.windows.get(&window_id) else {
                return;
            };
            let mut uris: std::collections::HashSet<String> = win
                .stored_push_diagnostics
                .iter()
                .filter(|(_, sm)| sm.contains_key(server_name))
                .map(|(uri, _)| uri.clone())
                .collect();
            uris.extend(
                win.stored_pull_diagnostics
                    .iter()
                    .filter(|(_, sm)| sm.contains_key(server_name))
                    .map(|(uri, _)| uri.clone()),
            );
            uris.into_iter().collect()
        };
        affected_uris.sort();

        if affected_uris.is_empty() {
            return;
        }

        tracing::info!(
            "Clearing diagnostics from server '{}' for {} URIs",
            server_name,
            affected_uris.len()
        );

        for uri in &affected_uris {
            if let Some(server_map) = self
                .windows
                .get_mut(&window_id)
                .unwrap()
                .stored_pull_diagnostics
                .get_mut(uri)
            {
                server_map.remove(server_name);
                if server_map.is_empty() {
                    self.windows
                        .get_mut(&window_id)
                        .unwrap()
                        .stored_pull_diagnostics
                        .remove(uri);
                }
            }
            if let Some(id_map) = self
                .windows
                .get_mut(&window_id)
                .unwrap()
                .diagnostic_result_ids
                .get_mut(uri)
            {
                id_map.remove(server_name);
                if id_map.is_empty() {
                    self.windows
                        .get_mut(&window_id)
                        .unwrap()
                        .diagnostic_result_ids
                        .remove(uri);
                }
            }
            if let Some(server_map) = self
                .windows
                .get_mut(&window_id)
                .unwrap()
                .stored_push_diagnostics
                .get_mut(uri)
            {
                server_map.remove(server_name);
                if server_map.is_empty() {
                    self.windows
                        .get_mut(&window_id)
                        .unwrap()
                        .stored_push_diagnostics
                        .remove(uri);
                }
            }

            // Invalidate the diagnostic overlay cache so the re-merge actually
            // updates on-screen overlays even if the resulting hash happens to
            // match a previous state.
            crate::services::lsp::diagnostics::invalidate_cache_for_file(uri);

            self.merge_and_apply_diagnostics(window_id, uri);
        }
    }
}

// =============================================================================
// LSP Feature Handlers
// =============================================================================

impl Editor {
    /// Handle LSP inlay hints response — thin shim over
    /// [`Window::handle_lsp_inlay_hints`]. The body is purely
    /// window-state mutation.
    pub(super) fn handle_lsp_inlay_hints(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        uri: String,
        hints: Vec<InlayHint>,
    ) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.handle_lsp_inlay_hints(request_id, uri, hints);
        }
    }
}

impl crate::app::window::Window {
    /// Handle LSP inlay hints response. Pure window-state
    /// mutation — pulls the in-flight request from the per-window
    /// pending map, version-checks against the current buffer
    /// state, and applies hints as virtual text.
    pub fn handle_lsp_inlay_hints(
        &mut self,
        request_id: u64,
        uri: String,
        hints: Vec<lsp_types::InlayHint>,
    ) {
        let Some(request) = self.pending_inlay_hints_requests.remove(&request_id) else {
            tracing::debug!(
                "Ignoring stale inlay hints response (request_id={})",
                request_id
            );
            return;
        };

        // Drop responses that raced behind a local edit — the hint
        // positions reference stale byte offsets and would render at
        // the wrong place. A fresh request was (or will be) scheduled
        // by the debounced inlay-hints timer on every didChange.
        let state_version = match self.buffers.get(&request.buffer_id) {
            Some(s) => s.buffer.version(),
            None => return, // Buffer was closed before the response arrived.
        };
        if state_version != request.version {
            tracing::debug!(
                "Ignoring stale inlay hints for {} (request_id={}, version={}, current={})",
                uri,
                request_id,
                request.version,
                state_version
            );
            return;
        }

        tracing::info!(
            "Received {} inlay hints for {} (request_id={})",
            hints.len(),
            uri,
            request_id
        );

        if let Some(state) = self.buffers.get_mut(&request.buffer_id) {
            super::Editor::apply_inlay_hints_to_state(state, &hints);
            tracing::info!(
                "Applied {} inlay hints as virtual text to buffer {:?}",
                hints.len(),
                request.buffer_id
            );
        }
    }
}

impl Editor {
    /// Handle LSP folding ranges response. The Editor wrapper
    /// orchestrates the URI-keyed `stored_folding_ranges` map
    /// (Editor-global because URIs can map to buffers in any
    /// window) and delegates the per-window buffer-state mutation
    /// to [`Window::apply_folding_ranges_response`].
    pub(super) fn handle_lsp_folding_ranges(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        uri: String,
        ranges: Vec<FoldingRange>,
    ) {
        // First peek at the active window to check whether the
        // request is still pending and whether the response is stale
        // (buffer version moved on). Returns the buffer_id +
        // up-to-date status so the editor-global stored_folding_ranges
        // update can happen in this scope.
        enum FoldingDispatch {
            Stale { buffer_id: BufferId },
            Apply { buffer_id: BufferId },
            Skip,
        }
        let dispatch = {
            let Some(win) = self.windows.get_mut(&window_id) else {
                return;
            };
            let Some(request) = win.pending_folding_range_requests.remove(&request_id) else {
                tracing::debug!(
                    "Ignoring folding ranges response without pending request (request_id={})",
                    request_id
                );
                return;
            };
            win.folding_ranges_in_flight.remove(&request.buffer_id);
            match win.buffers.get(&request.buffer_id) {
                Some(state) if state.buffer.version() == request.version => {
                    FoldingDispatch::Apply {
                        buffer_id: request.buffer_id,
                    }
                }
                Some(state) => {
                    tracing::debug!(
                        "Ignoring stale folding ranges for {} (request_id={}, version={}, current={})",
                        uri,
                        request_id,
                        request.version,
                        state.buffer.version()
                    );
                    FoldingDispatch::Stale {
                        buffer_id: request.buffer_id,
                    }
                }
                None => FoldingDispatch::Skip,
            }
        };
        let buffer_id = match dispatch {
            FoldingDispatch::Apply { buffer_id } => buffer_id,
            FoldingDispatch::Stale { buffer_id } => {
                self.windows
                    .get_mut(&window_id)
                    .unwrap()
                    .schedule_folding_ranges_refresh(buffer_id);
                return;
            }
            FoldingDispatch::Skip => return,
        };

        let window = self.windows.get_mut(&window_id).unwrap();
        let stored_folding_ranges = std::sync::Arc::make_mut(&mut window.stored_folding_ranges);
        if ranges.is_empty() {
            stored_folding_ranges.remove(&uri);
        } else {
            stored_folding_ranges.insert(uri.clone(), ranges);
        }

        let lsp_ranges = self
            .windows
            .get(&window_id)
            .and_then(|window| window.stored_folding_ranges.get(&uri))
            .cloned()
            .unwrap_or_default();
        self.windows
            .get_mut(&window_id)
            .unwrap()
            .apply_folding_ranges_response(buffer_id, lsp_ranges);
    }

    /// Handle LSP semantic tokens response
    pub(super) fn handle_lsp_semantic_tokens(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        uri: String,
        response: LspSemanticTokensResponse,
    ) {
        let (
            buffer_id,
            target_version,
            full_request_kind,
            requested_range,
            requested_start_line,
            requested_end_line,
        ) = if let Some(range_request) = self
            .windows
            .get_mut(&window_id)
            .and_then(|window| window.take_pending_semantic_token_range_request(request_id))
        {
            (
                range_request.buffer_id,
                range_request.version,
                None,
                Some(range_request.range),
                Some(range_request.start_line),
                Some(range_request.end_line),
            )
        } else if let Some(full_request) = self
            .windows
            .get_mut(&window_id)
            .and_then(|window| window.take_pending_semantic_token_request(request_id))
        {
            (
                full_request.buffer_id,
                full_request.version,
                Some(full_request.kind),
                None,
                None,
                None,
            )
        } else {
            tracing::debug!(
                "Semantic tokens response {} for {} without pending entry",
                request_id,
                uri
            );
            return;
        };

        // Get language from buffer's stored state
        let Some(language) = self
            .windows
            .get(&window_id)
            .and_then(|window| window.buffers.get(&buffer_id))
            .map(|state| state.language.clone())
        else {
            return;
        };

        let legend = match self
            .windows
            .get(&window_id)
            .and_then(|window| window.lsp.semantic_tokens_legend(&language).cloned())
        {
            Some(legend) => legend,
            None => {
                tracing::debug!("Semantic tokens legend missing for language {}", language);
                return;
            }
        };

        let Some(state) = self
            .windows
            .get_mut(&window_id)
            .and_then(|window| window.buffers.get_mut(&buffer_id))
        else {
            return;
        };

        let current_version = state.buffer.version();
        if current_version != target_version {
            // Stale response - ignore; next render will request fresh tokens.
            return;
        }

        match (requested_range, full_request_kind) {
            (Some(range), None) => {
                let result = match response {
                    LspSemanticTokensResponse::Range(result) => result,
                    _ => {
                        tracing::warn!(
                            "Semantic tokens range response {} for {} had mismatched type",
                            request_id,
                            uri
                        );
                        return;
                    }
                };

                match result {
                    Err(_) => {
                        // Error already logged at the appropriate level by the
                        // generic LSP response handler (debug for ContentModified/
                        // ServerCancelled, warn for real errors).
                    }
                    Ok(tokens_opt) => {
                        let spans = match tokens_opt {
                            Some(SemanticTokensRangeResult::Tokens(tokens)) => {
                                // LSP semantic tokens are always delta-encoded from document
                                // position (0,0), even for range requests. The range only
                                // filters which tokens are returned, not the encoding origin.
                                let decoded = decode_semantic_token_data(
                                    &state.buffer,
                                    &legend,
                                    &tokens.data,
                                    0,
                                );
                                decoded.spans
                            }
                            Some(SemanticTokensRangeResult::Partial(partial)) => {
                                let decoded = decode_semantic_token_data(
                                    &state.buffer,
                                    &legend,
                                    &partial.data,
                                    0,
                                );
                                decoded.spans
                            }
                            None => Vec::new(),
                        };

                        let applied = crate::services::lsp::semantic_tokens::apply_semantic_tokens_range_to_state(
                            state,
                            range.clone(),
                            &spans,
                            &self.theme.read().unwrap(),
                        );
                        if applied {
                            self.windows
                                .get_mut(&window_id)
                                .unwrap()
                                .semantic_tokens_range_applied
                                .insert(
                                    buffer_id,
                                    (
                                        requested_start_line.unwrap_or(0),
                                        requested_end_line.unwrap_or(0),
                                        current_version,
                                    ),
                                );
                        }
                    }
                }
            }
            (None, Some(super::SemanticTokensFullRequestKind::Full)) => {
                let result = match response {
                    LspSemanticTokensResponse::Full(result) => result,
                    _ => {
                        tracing::warn!(
                            "Semantic tokens response {} for {} had mismatched type",
                            request_id,
                            uri
                        );
                        return;
                    }
                };

                match result {
                    Err(_) => {
                        // Error already logged by the generic LSP response handler.
                    }
                    Ok(tokens_opt) => {
                        let decoded = match tokens_opt {
                            Some(SemanticTokensResult::Tokens(tokens)) => {
                                let decoded = decode_semantic_token_data(
                                    &state.buffer,
                                    &legend,
                                    &tokens.data,
                                    0,
                                );
                                SemanticTokensFullDecode {
                                    result_id: tokens.result_id.clone(),
                                    raw_data: decoded.raw,
                                    spans: decoded.spans,
                                }
                            }
                            Some(SemanticTokensResult::Partial(partial)) => {
                                let decoded = decode_semantic_token_data(
                                    &state.buffer,
                                    &legend,
                                    &partial.data,
                                    0,
                                );
                                SemanticTokensFullDecode {
                                    result_id: None,
                                    raw_data: decoded.raw,
                                    spans: decoded.spans,
                                }
                            }
                            None => SemanticTokensFullDecode {
                                result_id: None,
                                raw_data: Vec::new(),
                                spans: Vec::new(),
                            },
                        };

                        crate::services::lsp::semantic_tokens::apply_semantic_tokens_to_state(
                            state,
                            &decoded.spans,
                            &self.theme.read().unwrap(),
                        );

                        state.set_semantic_tokens(SemanticTokenStore {
                            version: current_version,
                            result_id: decoded.result_id,
                            data: decoded.raw_data,
                            tokens: decoded.spans,
                        });
                    }
                }
            }
            (None, Some(super::SemanticTokensFullRequestKind::FullDelta)) => {
                let result = match response {
                    LspSemanticTokensResponse::FullDelta(result) => result,
                    _ => {
                        tracing::warn!(
                            "Semantic tokens delta response {} for {} had mismatched type",
                            request_id,
                            uri
                        );
                        return;
                    }
                };

                match result {
                    Err(_) => {
                        // Error already logged by the generic LSP response handler.
                    }
                    Ok(tokens_opt) => {
                        let existing_store = state.semantic_tokens.as_ref();
                        let existing_result_id =
                            existing_store.and_then(|store| store.result_id.clone());
                        let existing_data = existing_store.map(|store| store.data.clone());

                        let decoded = match tokens_opt {
                            Some(SemanticTokensFullDeltaResult::Tokens(tokens)) => {
                                SemanticTokensDeltaDecode {
                                    result_id: tokens.result_id.clone(),
                                    raw_data: semantic_tokens_to_raw(&tokens.data),
                                }
                            }
                            Some(SemanticTokensFullDeltaResult::TokensDelta(delta)) => {
                                let Some(existing) = existing_data else {
                                    tracing::warn!(
                                        "Semantic tokens delta response {} for {} missing baseline",
                                        request_id,
                                        uri
                                    );
                                    return;
                                };
                                let updated = match apply_semantic_token_edits(
                                    existing,
                                    &delta.edits,
                                ) {
                                    Some(data) => data,
                                    None => {
                                        tracing::warn!(
                                            "Semantic tokens delta response {} for {} had invalid edits",
                                            request_id,
                                            uri
                                        );
                                        return;
                                    }
                                };
                                SemanticTokensDeltaDecode {
                                    result_id: delta.result_id.clone().or(existing_result_id),
                                    raw_data: updated,
                                }
                            }
                            Some(SemanticTokensFullDeltaResult::PartialTokensDelta { edits }) => {
                                let Some(existing) = existing_data else {
                                    tracing::warn!(
                                        "Semantic tokens delta response {} for {} missing baseline",
                                        request_id,
                                        uri
                                    );
                                    return;
                                };
                                let updated = match apply_semantic_token_edits(existing, &edits) {
                                    Some(data) => data,
                                    None => {
                                        tracing::warn!(
                                            "Semantic tokens delta response {} for {} had invalid edits",
                                            request_id,
                                            uri
                                        );
                                        return;
                                    }
                                };
                                SemanticTokensDeltaDecode {
                                    result_id: existing_result_id,
                                    raw_data: updated,
                                }
                            }
                            None => SemanticTokensDeltaDecode {
                                result_id: None,
                                raw_data: Vec::new(),
                            },
                        };

                        let spans = decode_semantic_token_raw_data(
                            &state.buffer,
                            &legend,
                            &decoded.raw_data,
                            0,
                        );

                        crate::services::lsp::semantic_tokens::apply_semantic_tokens_to_state(
                            state,
                            &spans,
                            &self.theme.read().unwrap(),
                        );

                        state.set_semantic_tokens(SemanticTokenStore {
                            version: current_version,
                            result_id: decoded.result_id,
                            data: decoded.raw_data,
                            tokens: spans,
                        });
                    }
                }
            }
            _ => {
                tracing::warn!(
                    "Semantic tokens response {} for {} had mismatched pending state",
                    request_id,
                    uri
                );
            }
        }
    }

    /// Handle LSP server quiescent notification (rust-analyzer project fully loaded)
    pub(super) fn handle_lsp_server_quiescent(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
    ) {
        tracing::info!(
            "LSP ({}) project fully loaded, re-requesting diagnostics and inlay hints",
            language
        );

        self.pull_diagnostics_for_language_in_window(window_id, &language);
        if self.config.editor.enable_inlay_hints {
            self.request_inlay_hints_for_language_in_window(window_id, &language);
        }
        self.request_folding_ranges_for_language_in_window(window_id, &language);
    }

    /// Handle workspace/diagnostic/refresh request from the LSP server.
    /// Re-pulls diagnostics for all open documents of the given language.
    pub(super) fn handle_lsp_diagnostic_refresh(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
    ) {
        tracing::info!(
            "LSP ({}) diagnostic refresh requested, re-pulling diagnostics",
            language
        );
        self.pull_diagnostics_for_language_in_window(window_id, &language);
    }

    pub(super) fn handle_lsp_inlay_hint_refresh(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
    ) {
        tracing::info!(
            "LSP ({}) inlay-hint refresh requested, re-pulling inlay hints",
            language
        );
        self.request_inlay_hints_for_language_in_window(window_id, &language);
    }

    pub(super) fn handle_lsp_semantic_tokens_refresh(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
    ) {
        tracing::info!(
            "LSP ({}) semantic-tokens refresh requested, re-pulling semantic tokens",
            language
        );
        self.request_semantic_tokens_for_language_in_window(window_id, &language);
    }

    /// Apply a dynamic capability (un)registration from the server, then — when
    /// a capability newly turned on — kick off the corresponding requests for
    /// buffers that were already open before the registration arrived (they
    /// would otherwise never be requested, mirroring `LspInitialized`).
    pub(super) fn handle_lsp_dynamic_capabilities(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
        server_name: String,
        register: bool,
        registrations: Vec<(String, Option<serde_json::Value>)>,
    ) {
        tracing::info!(
            "LSP ({}) server '{}' {} {} capability registration(s)",
            language,
            server_name,
            if register {
                "registered"
            } else {
                "unregistered"
            },
            registrations.len()
        );

        let changed = self.windows.get_mut(&window_id).is_some_and(|window| {
            window
                .lsp
                .apply_dynamic_capabilities(&server_name, register, &registrations)
        });

        if changed && register {
            self.request_semantic_tokens_for_language_in_window(window_id, &language);
            self.request_folding_ranges_for_language_in_window(window_id, &language);
            self.request_inlay_hints_for_language_in_window(window_id, &language);
            self.pull_diagnostics_for_language_in_window(window_id, &language);
        }
    }

    /// Re-pull diagnostics for all open buffers associated with the given language.
    pub(super) fn pull_diagnostics_for_language(&mut self, language: &str) {
        self.pull_diagnostics_for_language_in_window(self.active_window, language);
    }

    pub(super) fn pull_diagnostics_for_language_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        language: &str,
    ) {
        let uris: Vec<_> = self
            .buffers_for_language_in_window(window_id, language)
            .into_iter()
            .map(|(_, uri)| uri)
            .collect();

        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        for uri in uris {
            window.pull_diagnostics_for_uri(language, &uri);
        }
    }

    /// Handle LSP progress notification ($/progress)
    pub(super) fn handle_lsp_progress(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
        token: String,
        value: LspProgressValue,
    ) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        match value {
            LspProgressValue::Begin {
                title,
                message,
                percentage,
            } => {
                window.lsp_progress.insert(
                    token.clone(),
                    LspProgressInfo {
                        language,
                        title,
                        message,
                        percentage,
                    },
                );
            }
            LspProgressValue::Report {
                message,
                percentage,
            } => {
                if let Some(info) = window.lsp_progress.get_mut(&token) {
                    info.message = message;
                    info.percentage = percentage;
                }
            }
            LspProgressValue::End { .. } => {
                window.lsp_progress.remove(&token);
            }
        }
        if window_id == self.active_window {
            self.refresh_lsp_status_popup_if_open();
        }
    }

    /// Handle LSP window message (window/showMessage)
    pub(super) fn handle_lsp_window_message(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
        message_type: LspMessageType,
        message: String,
    ) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        window.lsp_window_messages.push(LspMessageEntry {
            language: language.clone(),
            message_type,
            message: message.clone(),
            timestamp: Instant::now(),
        });
        if window.lsp_window_messages.len() > 100 {
            window.lsp_window_messages.remove(0);
        }
        if matches!(
            message_type,
            LspMessageType::Error | LspMessageType::Warning
        ) {
            window.status_message = Some(format!("LSP ({}): {}", language, message));
        }
    }

    /// Handle LSP log message (window/logMessage)
    pub(super) fn handle_lsp_log_message(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
        message_type: LspMessageType,
        message: String,
    ) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        window.lsp_log_messages.push(LspMessageEntry {
            language,
            message_type,
            message,
            timestamp: Instant::now(),
        });
        if window.lsp_log_messages.len() > 500 {
            window.lsp_log_messages.remove(0);
        }
    }

    /// Handle LSP server status update
    pub(super) fn handle_lsp_status_update(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
        server_name: String,
        status: LspServerStatus,
    ) {
        use crate::services::async_bridge::LspServerStatus;

        let server_name_ref = server_name.clone();
        let key = (language.clone(), server_name);
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let old_status = window.lsp_server_statuses.get(&key).cloned();
        window.lsp_server_statuses.insert(key, status);
        window.update_lsp_warning_domain();

        if status == LspServerStatus::Running
            && !old_status
                .as_ref()
                .is_some_and(|old| matches!(old, LspServerStatus::Running))
        {
            let scope = self
                .windows
                .get(&window_id)
                .and_then(|window| window.lsp.server_scope(&server_name_ref).cloned());
            match scope {
                Some(scope) if scope.is_universal() => {
                    let languages: Vec<String> = self
                        .windows
                        .get(&window_id)
                        .map(|window| window.buffers.languages().into_iter().collect())
                        .unwrap_or_default();
                    for lang in languages {
                        self.reopen_buffers_for_language_in_window(window_id, &lang);
                    }
                }
                Some(scope) => {
                    for lang in scope.languages() {
                        self.reopen_buffers_for_language_in_window(window_id, lang);
                    }
                }
                None => self.reopen_buffers_for_language_in_window(window_id, &language),
            }
        }

        if status == LspServerStatus::Error
            && old_status.as_ref().is_some_and(|old| {
                matches!(
                    old,
                    LspServerStatus::Running | LspServerStatus::Initializing
                )
            })
        {
            self.clear_diagnostics_for_server_in_window(window_id, &server_name_ref);
            if let Some(window) = self.windows.get_mut(&window_id) {
                let message = window.lsp.handle_server_crash(&language, &server_name_ref);
                window.status_message = Some(message);
            }
        }

        if matches!(status, LspServerStatus::Error | LspServerStatus::Shutdown) {
            if let Some(window) = self.windows.get_mut(&window_id) {
                let any_running_for_lang =
                    window.lsp_server_statuses.iter().any(|((lang, _), s)| {
                        lang == &language
                            && !matches!(s, LspServerStatus::Error | LspServerStatus::Shutdown)
                    });
                if !any_running_for_lang {
                    window
                        .lsp_progress
                        .retain(|_, info| info.language != language);
                }
            }
            if window_id == self.active_window {
                self.refresh_lsp_status_popup_if_open();
            }
        }

        let status_str = match status {
            LspServerStatus::Starting => "starting",
            LspServerStatus::Initializing => "initializing",
            LspServerStatus::Running => "running",
            LspServerStatus::Error => "error",
            LspServerStatus::Shutdown => "shutdown",
        };
        let old_status_str = old_status
            .map(|old| match old {
                LspServerStatus::Starting => "starting",
                LspServerStatus::Initializing => "initializing",
                LspServerStatus::Running => "running",
                LspServerStatus::Error => "error",
                LspServerStatus::Shutdown => "shutdown",
            })
            .unwrap_or("none");

        self.emit_event(
            crate::model::control_event::events::LSP_STATUS_CHANGED.name,
            serde_json::json!({
                "language": language,
                "old_status": old_status_str,
                "status": status_str
            }),
        );
    }

    /// Handle custom LSP notification
    #[allow(dead_code)] // Prepared for future use when AsyncMessage::LspCustomNotification is added
    pub(super) fn handle_custom_notification(
        &mut self,
        language: String,
        method: String,
        params: Option<Value>,
    ) {
        tracing::debug!("Custom LSP notification {} from {}", method, language);
        let payload = serde_json::json!({
            "language": language,
            "method": method,
            "params": params,
        });
        self.emit_event("lsp/custom_notification", payload);
    }

    /// Handle LSP server request (server -> client)
    /// These are requests from the LSP server that require handling, typically
    /// custom/extension methods specific to certain language servers.
    pub(super) fn handle_lsp_server_request(
        &mut self,
        window_id: fresh_core::WindowId,
        language: String,
        server_command: String,
        method: String,
        params: Option<Value>,
    ) {
        tracing::debug!(
            "LSP server request {} from {} ({})",
            method,
            language,
            server_command
        );

        let params_str = params.map(|p| p.to_string());
        self.run_plugin_hook_for_window(
            window_id,
            "lsp_server_request",
            crate::services::plugins::hooks::HookArgs::LspServerRequest {
                language,
                method,
                server_command,
                params: params_str,
            },
        );
    }

    /// Handle plugin LSP response
    pub(super) fn handle_plugin_lsp_response(
        &mut self,
        request_id: u64,
        result: Result<Value, String>,
    ) {
        use fresh_core::api::JsCallbackId;
        tracing::debug!("Received plugin LSP response (request_id={})", request_id);
        let callback_id = JsCallbackId::from(request_id);
        match result {
            Ok(value) => {
                self.plugin_manager
                    .read()
                    .unwrap()
                    .resolve_callback(callback_id, value.to_string());
            }
            Err(err) => {
                self.plugin_manager
                    .read()
                    .unwrap()
                    .reject_callback(callback_id, err);
            }
        }
    }

    /// Handle generic plugin response (e.g., GetBufferText result)
    pub(super) fn handle_plugin_response(&mut self, response: fresh_core::api::PluginResponse) {
        tracing::debug!("Received plugin response: {:?}", response);
        self.send_plugin_response(response);
    }
}

// =============================================================================
// File System Event Handlers
// =============================================================================

impl Editor {
    /// Handle file changed externally notification (from AsyncMessage)
    ///
    /// Includes debounce logic to prevent rapid auto-reverts from overwhelming the editor.
    /// This is different from `handle_file_changed` which actually reloads the file.
    pub(super) fn handle_async_file_changed(&mut self, path: String) -> bool {
        const DEBOUNCE_WINDOW: Duration = Duration::from_secs(10);
        const RAPID_REVERT_THRESHOLD: u32 = 10; // Require 10 reverts in 10 seconds to disable

        let path_buf = PathBuf::from(&path);

        // Only track events for files that have an open buffer opted into
        // auto-revert. Auto-revert is a per-buffer property, so a change to a
        // terminal backing file (or a streaming buffer's file) is ignored
        // here rather than driving a reload/redraw.
        let is_revertible_open = {
            let window = self.active_window();
            window.buffers.iter().any(|(id, state)| {
                state.buffer.file_path() == Some(&path_buf)
                    && window.buffer_auto_revert_enabled(*id)
            })
        };

        if !is_revertible_open {
            tracing::trace!(
                "Ignoring file change event for non-revertible file: {}",
                path
            );
            return false;
        }

        // Track rapid file change events - only disable after many reverts in short window
        let mut should_disable = false;
        let now = self.time_source.now();
        let elapsed_window_ok = if let Some((window_start, _)) =
            self.active_window().file_rapid_change_counts.get(&path_buf)
        {
            self.time_source.elapsed_since(*window_start) < DEBOUNCE_WINDOW
        } else {
            false
        };
        if let Some((window_start, count)) = self
            .active_window_mut()
            .file_rapid_change_counts
            .get_mut(&path_buf)
        {
            if elapsed_window_ok {
                *count += 1;

                if *count >= RAPID_REVERT_THRESHOLD {
                    should_disable = true;
                    tracing::info!(
                        "Auto-revert disabled for {:?} ({} reverts in {:?})",
                        path_buf,
                        count,
                        DEBOUNCE_WINDOW
                    );
                }
            } else {
                // Reset counter - start a new window
                *count = 1;
                *window_start = now;
            }
        } else {
            // First event for this file
            let now = self.time_source.now();
            self.active_window_mut()
                .file_rapid_change_counts
                .insert(path_buf.clone(), (now, 1));
        }
        if should_disable {
            // Disable auto-revert only for the offending file's buffer(s),
            // not the whole window — auto-revert is a per-buffer property.
            let ids: Vec<BufferId> = {
                let window = self.active_window();
                window
                    .buffers
                    .iter()
                    .filter(|(_, state)| state.buffer.file_path() == Some(&path_buf))
                    .map(|(id, _)| *id)
                    .collect()
            };
            for id in ids {
                if let Some(meta) = self.active_window_mut().buffer_metadata.get_mut(&id) {
                    meta.auto_revert_enabled = false;
                }
            }
            self.active_window_mut().status_message = Some(format!(
                "Auto-revert disabled: {} is updating too frequently (use Ctrl+Shift+R to re-enable)",
                path_buf.file_name().unwrap_or_default().to_string_lossy()
            ));
            return false;
        }

        tracing::info!("File changed externally: {}", path);
        self.handle_file_changed(&path);
        true
    }
}

// =============================================================================
// File Explorer Handlers
// =============================================================================

impl Editor {
    /// Handle file explorer initialized for the exact authority that spawned it.
    pub(super) fn handle_file_explorer_initialized(
        &mut self,
        window: fresh_core::WindowId,
        filesystem_id: usize,
        view: FileTreeView,
    ) {
        let Some(win) = self.windows.get(&window) else {
            return;
        };
        if crate::services::async_bridge::filesystem_identity(&win.authority().filesystem)
            != filesystem_id
        {
            tracing::debug!(?window, "dropping stale file explorer initialization");
            return;
        }
        tracing::info!("File explorer initialized for window {window}");
        let defaults = crate::app::file_explorer::FileExplorerViewDefaults {
            show_hidden: self.config.file_explorer.show_hidden,
            show_gitignored: self.config.file_explorer.show_gitignored,
            respect_gitignore: self.config.file_explorer.respect_gitignore,
            compact_directories: self.config.file_explorer.compact_directories,
            custom_ignore_patterns: self.config.file_explorer.custom_ignore_patterns.clone(),
        };
        let is_active = window == self.active_window_id();
        self.windows
            .get_mut(&window)
            .expect("window checked above")
            .install_initialized_file_explorer(view, defaults);
        if is_active {
            self.set_status_message(t!("status.file_explorer_ready").to_string());
        }
    }

    /// Handle file explorer node toggle completed
    pub(super) fn handle_file_explorer_toggle_node(&mut self, node_id: NodeId) {
        tracing::debug!("File explorer toggle completed for node {:?}", node_id);
    }

    /// Handle file explorer node refresh completed
    pub(super) fn handle_file_explorer_refresh_node(&mut self, node_id: NodeId) {
        tracing::debug!("File explorer refresh completed for node {:?}", node_id);
        self.set_status_message(t!("explorer.refreshed_default").to_string());
    }

    /// Handle file explorer expanded to path for the authority that owned it.
    pub(super) fn handle_file_explorer_expanded_to_path(
        &mut self,
        window: fresh_core::WindowId,
        filesystem_id: usize,
        view: FileTreeView,
    ) {
        tracing::trace!(
            "handle_file_explorer_expanded_to_path: restoring file_explorer for window {window}"
        );
        let Some(win) = self.windows.get_mut(&window) else {
            return;
        };
        if crate::services::async_bridge::filesystem_identity(&win.authority().filesystem)
            != filesystem_id
        {
            tracing::debug!(?window, "dropping stale file explorer expansion");
            return;
        }
        win.install_expanded_file_explorer(view);
    }
}

// =============================================================================
// Plugin Handlers
// =============================================================================

impl Editor {
    /// Handle plugin process output completion
    pub(super) fn handle_plugin_process_output(
        &mut self,
        callback_id: fresh_core::api::JsCallbackId,
        stdout: String,
        stderr: String,
        exit_code: i32,
    ) {
        tracing::debug!(
            "Process {} completed: exit_code={}, stdout_len={}, stderr_len={}",
            callback_id,
            exit_code,
            stdout.len(),
            stderr.len()
        );
        // Resolve the plugin callback with the process output
        // Using SpawnResult struct ensures field names match TypeScript types
        let result = fresh_core::api::SpawnResult {
            stdout,
            stderr,
            exit_code,
        };
        self.plugin_manager
            .read()
            .unwrap()
            .resolve_callback(callback_id, serde_json::to_string(&result).unwrap());
    }

    /// Process TypeScript plugin commands
    ///
    /// Returns true if any visual commands were processed (i.e. a re-render is needed).
    /// Non-visual acknowledgements like `HookCompleted` do not count.
    #[cfg(feature = "plugins")]
    pub(super) fn process_plugin_commands(&mut self) -> bool {
        // Backlog first so a burst spread over several frames keeps arrival
        // order, then whatever the plugin thread has produced since.
        let mut commands: Vec<_> = self.plugin_command_backlog.drain(..).collect();
        commands.extend(
            self.plugin_manager
                .write()
                .unwrap()
                .process_command_envelopes(),
        );
        if commands.is_empty() {
            return false;
        }

        // Classify each command as visual (needs re-render) or not.
        // `HookCompleted` advances hook delivery but is non-visual.
        // `SetStatusBarValue` is treated as
        // visual only when the value actually differs from what's stored —
        // many plugins (e.g. git_statusbar) re-publish the same value on
        // every `render_start` hook, which would otherwise create a
        // render → hook → ack → render feedback loop at ~13Hz forever.
        //
        // The remaining `=> false` arms are side-effecting commands that
        // never touch the rendered buffer: scheduling a timer, spawning
        // processes / HTTP, watching paths, and writing plugin-private
        // state. Any *visual* result they eventually produce arrives as its
        // own command (overlay, virtual text, status value, …) and is
        // counted then. Treating these as visual forced a redraw on every
        // debounce tick — e.g. live_diff's 75ms `editor.delay()` recompute
        // repainted the screen twice per keystroke with no change. Invisible
        // on a fast terminal, but real lag over a serial console (#2100).
        use fresh_core::api::PluginCommand as Pc;
        let has_visual_commands = commands.iter().any(|envelope| match &envelope.command {
            Pc::HookCompleted { .. }
            | Pc::Delay { .. }
            | Pc::SpawnProcess { .. }
            | Pc::SpawnBackgroundProcess { .. }
            | Pc::KillBackgroundProcess { .. }
            | Pc::SpawnProcessWait { .. }
            | Pc::HttpFetch { .. }
            | Pc::WatchPath { .. }
            | Pc::UnwatchPath { .. }
            | Pc::SetGlobalState { .. }
            | Pc::SetWindowState { .. }
            | Pc::SetViewState { .. }
            // Arming or cancelling a timer paints nothing; what the handler
            // eventually draws arrives as its own command and is counted then.
            | Pc::SetPluginTimer { .. }
            | Pc::ClearPluginTimer { .. } => false,
            Pc::SetStatusBarValue {
                buffer_id,
                key,
                value,
            } => {
                self.current_status_bar_value(fresh_core::BufferId(*buffer_id as usize), key)
                    != Some(value.as_str())
            }
            _ => true,
        });

        for envelope in &commands {
            match &envelope.command {
                fresh_core::api::PluginCommand::RegisterGrammar {
                    language,
                    grammar_path,
                    extensions,
                } => {
                    tracing::info!(
                        "[SYNTAX DEBUG] processing RegisterGrammar: lang='{}', path='{}', ext={:?}",
                        language,
                        grammar_path,
                        extensions
                    );
                }
                fresh_core::api::PluginCommand::ReloadGrammars { .. } => {
                    tracing::info!("[SYNTAX DEBUG] processing ReloadGrammars command");
                }
                _ => {}
            }
        }

        // Frame budget: dispatch against a deadline, then stop and re-arm. A
        // plugin that floods the channel costs one budget per frame instead of
        // an unbounded stall, and the unprocessed tail keeps its arrival order
        // in `plugin_command_backlog`. The DRAIN_MIN_PER_PASS floor keeps
        // throughput from collapsing to one item per frame when a single
        // dispatch overruns the whole budget.
        let deadline = std::time::Instant::now() + super::PLUGIN_COMMAND_FRAME_BUDGET;
        let mut iter = commands.into_iter();
        let mut dispatched = 0usize;
        for envelope in iter.by_ref() {
            tracing::trace!(
                "process_plugin_commands: handling command {:?}",
                std::mem::discriminant(&envelope.command)
            );
            self.dispatch_plugin_command_envelope(envelope);
            dispatched += 1;
            if dispatched >= super::DRAIN_MIN_PER_PASS && std::time::Instant::now() >= deadline {
                break;
            }
        }
        let deferred: std::collections::VecDeque<_> = iter.collect();
        if !deferred.is_empty() {
            tracing::debug!(
                dispatched,
                deferred = deferred.len(),
                "plugin command frame budget exhausted — deferring tail"
            );
            // Prepend: these arrived before anything a later tick will read.
            for command in deferred.into_iter().rev() {
                self.plugin_command_backlog.push_front(command);
            }
            self.plugin_render_requested = true;
        }

        // Flush any deferred grammar rebuilds as a single batch
        self.flush_pending_grammars();

        has_visual_commands
    }

    /// Process pending plugin action completions
    #[cfg(feature = "plugins")]
    pub(super) fn process_pending_plugin_actions(&mut self) {
        self.pending_plugin_actions
            .retain(|(action_name, receiver)| {
                match receiver.try_recv() {
                    Ok(result) => {
                        match result {
                            Ok(()) => {
                                tracing::info!(
                                    "Plugin action '{}' executed successfully",
                                    action_name
                                );
                            }
                            Err(e) => {
                                tracing::error!("Plugin action '{}' error: {}", action_name, e);
                            }
                        }
                        false // Remove completed action
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        true // Keep pending action
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        tracing::error!(
                            "Plugin thread disconnected during action '{}'",
                            action_name
                        );
                        false // Remove disconnected action
                    }
                }
            });
    }

    /// True iff no plugin actions are currently in-flight on the
    /// plugin thread. Test harness helper — used by `send_key` to
    /// know when async plugin work queued by the key has fully
    /// settled before returning, so tests see synchronous-looking
    /// behavior between sequential key presses (e.g. a mode-bound
    /// `Home` followed by a synchronous-bypass `Shift+Right`).
    /// Outside tests, the editor's main loop pumps these alongside
    /// other async messages on every frame so there's nothing to
    /// drain explicitly.
    #[cfg(feature = "plugins")]
    #[doc(hidden)]
    pub fn pending_plugin_actions_is_empty(&self) -> bool {
        self.pending_plugin_actions.is_empty()
    }

    /// Stub for builds without plugin support — there are no
    /// plugin actions to track, so we're always "settled".
    #[cfg(not(feature = "plugins"))]
    #[doc(hidden)]
    pub fn pending_plugin_actions_is_empty(&self) -> bool {
        true
    }

    /// Process pending LSP server restarts (with exponential backoff)
    pub(super) fn process_pending_lsp_restarts(&mut self) {
        let __active_id = self.active_window;
        let Some(lsp) = self.windows.get_mut(&__active_id).map(|w| &mut w.lsp) else {
            return;
        };

        let restart_results = lsp.process_pending_restarts();

        for (language, success, message) in restart_results {
            self.active_window_mut().status_message = Some(message.clone());

            if success {
                self.resend_did_open_for_language(&language);
            }
        }
    }

    /// Re-send didOpen notifications for all buffers of a given language.
    pub(super) fn resend_did_open_for_language(&mut self, language: &str) {
        self.resend_did_open_for_language_in_window(self.active_window, language);
    }

    pub(super) fn resend_did_open_for_language_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        language: &str,
    ) {
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        let buffers: Vec<_> = window
            .buffers
            .iter()
            .filter_map(|(buffer_id, state)| {
                if state.language != language {
                    return None;
                }
                let path = window.buffer_metadata.get(buffer_id)?.file_path()?;
                let content = state.buffer.to_string()?;
                let uri = super::types::file_path_to_lsp_uri_with_translation(
                    path,
                    window.authority().path_translation.as_ref(),
                )?;
                Some((*buffer_id, state.language.clone(), content, uri))
            })
            .collect();

        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        for (buffer_id, language, content, uri) in buffers {
            let mut opened_with = Vec::new();
            for server in window.lsp.get_handles_mut(&language) {
                let handle_id = server.handle.id();
                if let Err(error) =
                    server
                        .handle
                        .did_open(uri.clone(), content.clone(), language.clone())
                {
                    tracing::warn!(
                        "LSP did_open failed for '{}' after restart: {}",
                        server.name,
                        error
                    );
                } else {
                    opened_with.push(handle_id);
                }
            }
            if let Some(metadata) = window.buffer_metadata.get_mut(&buffer_id) {
                metadata.lsp_opened_with.extend(opened_with);
            }
        }
    }

    /// Request semantic tokens for all open buffers matching a language.
    pub(super) fn request_semantic_tokens_for_language(&mut self, language: &str) {
        self.request_semantic_tokens_for_language_in_window(self.active_window, language);
    }

    pub(super) fn request_semantic_tokens_for_language_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        language: &str,
    ) {
        let buffer_ids: Vec<_> = self
            .buffers_for_language_in_window(window_id, language)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        if let Some(window) = self.windows.get_mut(&window_id) {
            for buffer_id in buffer_ids {
                window.schedule_semantic_tokens_full_refresh(buffer_id);
            }
        }
    }

    /// Request folding ranges for all open buffers matching a language.
    pub(super) fn request_folding_ranges_for_language(&mut self, language: &str) {
        self.request_folding_ranges_for_language_in_window(self.active_window, language);
    }

    pub(super) fn request_folding_ranges_for_language_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        language: &str,
    ) {
        let buffer_ids: Vec<_> = self
            .buffers_for_language_in_window(window_id, language)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        if let Some(window) = self.windows.get_mut(&window_id) {
            for buffer_id in buffer_ids {
                window.schedule_folding_ranges_refresh(buffer_id);
            }
        }
    }

    /// Request inlay hints for all open buffers matching a language.
    pub(super) fn request_inlay_hints_for_language(&mut self, language: &str) {
        self.request_inlay_hints_for_language_in_window(self.active_window, language);
    }

    pub(super) fn request_inlay_hints_for_language_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        language: &str,
    ) {
        let buffer_ids: Vec<_> = self
            .buffers_for_language_in_window(window_id, language)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for buffer_id in buffer_ids {
            self.request_inlay_hints_for_buffer_in_window(window_id, buffer_id);
        }
    }
}

fn semantic_tokens_to_raw(tokens: &[SemanticToken]) -> Vec<u32> {
    let mut raw = Vec::with_capacity(tokens.len().saturating_mul(5));
    for token in tokens {
        raw.push(token.delta_line);
        raw.push(token.delta_start);
        raw.push(token.length);
        raw.push(token.token_type);
        raw.push(token.token_modifiers_bitset);
    }
    raw
}

fn decode_semantic_token_raw_data(
    buffer: &Buffer,
    legend: &SemanticTokensLegend,
    data: &[u32],
    base_line: usize,
) -> Vec<SemanticTokenSpan> {
    if !data.len().is_multiple_of(5) {
        tracing::warn!(
            "Semantic token data length {} is not divisible by 5",
            data.len()
        );
        return Vec::new();
    }

    let mut result = Vec::with_capacity(data.len() / 5);
    let mut current_line = base_line as u32;
    let mut current_start = 0u32;

    for chunk in data.chunks_exact(5) {
        let delta_line = chunk[0];
        let delta_start = chunk[1];
        let length = chunk[2];
        let token_type = chunk[3];
        let token_modifiers_bitset = chunk[4];

        current_line += delta_line;
        if delta_line == 0 {
            current_start += delta_start;
        } else {
            current_start = delta_start;
        }

        let start_utf16 = current_start as usize;
        let end_utf16 = start_utf16 + length as usize;
        let start_byte = buffer.lsp_position_to_byte(current_line as usize, start_utf16);
        let end_byte = buffer.lsp_position_to_byte(current_line as usize, end_utf16);

        let token_type_name = legend
            .token_types
            .get(token_type as usize)
            .map(|ty| ty.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let mut modifiers = Vec::new();
        for (idx, modifier) in legend.token_modifiers.iter().enumerate() {
            if (token_modifiers_bitset >> idx) & 1 == 1 {
                modifiers.push(modifier.as_str().to_string());
            }
        }

        result.push(SemanticTokenSpan {
            range: start_byte..end_byte,
            token_type: token_type_name,
            modifiers,
        });
    }

    result
}

struct SemanticTokenDecode {
    raw: Vec<u32>,
    spans: Vec<SemanticTokenSpan>,
}

struct SemanticTokensFullDecode {
    result_id: Option<String>,
    raw_data: Vec<u32>,
    spans: Vec<SemanticTokenSpan>,
}

struct SemanticTokensDeltaDecode {
    result_id: Option<String>,
    raw_data: Vec<u32>,
}

fn decode_semantic_token_data(
    buffer: &Buffer,
    legend: &SemanticTokensLegend,
    data: &[SemanticToken],
    base_line: usize,
) -> SemanticTokenDecode {
    let raw = semantic_tokens_to_raw(data);
    let spans = decode_semantic_token_raw_data(buffer, legend, &raw, base_line);
    SemanticTokenDecode { raw, spans }
}

fn apply_semantic_token_edits(
    mut data: Vec<u32>,
    edits: &[SemanticTokensEdit],
) -> Option<Vec<u32>> {
    if edits.is_empty() {
        return Some(data);
    }

    for edit in edits.iter().rev() {
        let start = edit.start as usize;
        let delete_count = edit.delete_count as usize;
        if start > data.len() || start.saturating_add(delete_count) > data.len() {
            return None;
        }

        let insert = edit
            .data
            .as_ref()
            .map(|tokens| semantic_tokens_to_raw(tokens))
            .unwrap_or_default();

        data.splice(start..start + delete_count, insert);
    }

    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_token_delta_edits_apply() {
        let base = vec![0, 0, 2, 0, 0, 0, 3, 4, 1, 0];
        let edit = SemanticTokensEdit {
            start: 5,
            delete_count: 5,
            data: Some(vec![SemanticToken {
                delta_line: 0,
                delta_start: 5,
                length: 1,
                token_type: 2,
                token_modifiers_bitset: 0,
            }]),
        };

        let updated = apply_semantic_token_edits(base, &[edit]).expect("edit should apply");
        assert_eq!(updated.len(), 10);
        assert_eq!(&updated[5..10], &[0, 5, 1, 2, 0]);
    }
}
