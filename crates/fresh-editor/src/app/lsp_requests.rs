//! LSP (Language Server Protocol) request handling for the Editor.
//!
//! This module contains all methods related to LSP operations including:
//! - Completion requests and response handling
//! - Go-to-definition
//! - Hover documentation
//! - Find references
//! - Signature help
//! - Code actions
//! - Rename operations
//! - Inlay hints

use anyhow::Result as AnyhowResult;
use rust_i18n::t;
use std::io;
use std::time::{Duration, Instant};

use crate::model::event::{BufferId, Event};
use crate::primitives::word_navigation::{find_word_end, find_word_start};
use crate::view::prompt::{Prompt, PromptType};

use crate::services::lsp::async_handler::LspHandle;
use crate::types::LspFeature;

use super::{Editor, SemanticTokenRangeRequest};

/// Ensure every line in a docstring is separated by a blank line.
///
/// LSP documentation (e.g. from pyright) often uses single newlines between
/// lines, which markdown treats as soft breaks within one paragraph. This
/// doubles all single newlines so each line becomes its own paragraph with
/// spacing between them.
fn space_doc_paragraphs(text: &str) -> String {
    text.replace("\n\n", "\x00").replace(['\n', '\x00'], "\n\n")
}

/// Whether an LSP range (half-open end, like `[start, end)`) contains the given
/// `(line, character)` LSP position. Zero-length ranges (start == end) are
/// treated as containing their single anchor point so point-style diagnostics
/// still match a hover that lands exactly on them.
fn lsp_range_contains(range: &lsp_types::Range, line: u32, character: u32) -> bool {
    let start = range.start;
    let end = range.end;
    // Before start?
    if line < start.line || (line == start.line && character < start.character) {
        return false;
    }
    // Zero-length range: accept exact anchor match.
    if start.line == end.line && start.character == end.character {
        return line == start.line && character == start.character;
    }
    // After end? (half-open)
    if line > end.line || (line == end.line && character >= end.character) {
        return false;
    }
    true
}

/// Whether an LSP range overlaps the half-open range
/// `[(b_start_line, b_start_char), (b_end_line, b_end_char))`. Zero-length
/// ranges on either side are treated as their single anchor point — so a
/// point cursor matches any diagnostic whose range covers that point, and a
/// point-style diagnostic matches a selection that covers its anchor.
fn lsp_range_overlaps(
    a: &lsp_types::Range,
    b_start_line: u32,
    b_start_char: u32,
    b_end_line: u32,
    b_end_char: u32,
) -> bool {
    let b_is_point = b_start_line == b_end_line && b_start_char == b_end_char;
    if b_is_point {
        return lsp_range_contains(a, b_start_line, b_start_char);
    }
    let a_is_point = a.start.line == a.end.line && a.start.character == a.end.character;
    if a_is_point {
        let (p_line, p_char) = (a.start.line, a.start.character);
        if p_line < b_start_line || (p_line == b_start_line && p_char < b_start_char) {
            return false;
        }
        if p_line > b_end_line || (p_line == b_end_line && p_char >= b_end_char) {
            return false;
        }
        return true;
    }
    // Both have extent. A entirely before B?
    if a.end.line < b_start_line || (a.end.line == b_start_line && a.end.character <= b_start_char)
    {
        return false;
    }
    // A entirely after B?
    if a.start.line > b_end_line || (a.start.line == b_end_line && a.start.character >= b_end_char)
    {
        return false;
    }
    true
}

const SEMANTIC_TOKENS_RANGE_DEBOUNCE_MS: u64 = 50;
const SEMANTIC_TOKENS_RANGE_PADDING_LINES: usize = 10;

impl Editor {
    /// Handle LSP completion response.
    /// Supports merging from multiple servers: first response creates the menu,
    /// subsequent responses extend it.
    pub(crate) fn handle_completion_response(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        items: Vec<lsp_types::CompletionItem>,
    ) -> AnyhowResult<()> {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return Ok(());
        };
        if !window.pending_completion_requests.remove(&request_id) {
            tracing::debug!(
                "Ignoring completion response for outdated request {}",
                request_id
            );
            return Ok(());
        }

        if items.is_empty() {
            tracing::debug!("No completion items received");
            if window.pending_completion_requests.is_empty() && window.completion_items.is_none() {
                self.show_buffer_word_completion_popup_in_window(window_id);
            }
            return Ok(());
        }

        use crate::primitives::word_navigation::find_completion_word_start;
        let cursor_pos = window.active_cursors().primary().position;
        let word_start = find_completion_word_start(&window.active_state().buffer, cursor_pos);
        let prefix = if word_start < cursor_pos {
            window
                .active_state_mut()
                .get_text_range(word_start, cursor_pos)
                .to_lowercase()
        } else {
            String::new()
        };
        let matches_prefix = |item: &lsp_types::CompletionItem| -> bool {
            prefix.is_empty()
                || item.label.to_lowercase().starts_with(&prefix)
                || item
                    .filter_text
                    .as_ref()
                    .is_some_and(|text| text.to_lowercase().starts_with(&prefix))
        };

        if !items.iter().any(|item| matches_prefix(item)) && window.completion_items.is_none() {
            tracing::debug!("No completion items match prefix '{}'", prefix);
            return Ok(());
        }
        match &mut window.completion_items {
            Some(existing) => existing.extend(items),
            None => window.completion_items = Some(items),
        }
        let all_items: Vec<lsp_types::CompletionItem> = window
            .completion_items
            .as_ref()
            .unwrap()
            .iter()
            .filter(|item| matches_prefix(item))
            .cloned()
            .collect();
        if all_items.is_empty() {
            tracing::debug!("No completion items match prefix '{}'", prefix);
            return Ok(());
        }

        let item_refs: Vec<&lsp_types::CompletionItem> = all_items.iter().collect();
        let mut popup_items = crate::app::popup_actions::lsp_items_to_popup_items(&item_refs);
        let buffer_word_items = self.get_buffer_completion_popup_items_in_window(window_id);
        let lsp_labels: std::collections::HashSet<String> = popup_items
            .iter()
            .map(|item| item.text.to_lowercase())
            .collect();
        popup_items.extend(
            buffer_word_items
                .into_iter()
                .filter(|item| !lsp_labels.contains(&item.text.to_lowercase())),
        );

        let popup_data =
            crate::app::popup_actions::build_completion_popup_from_items(popup_items, 0);
        let accept_hint = self.completion_accept_key_hint();
        let focus_hint = self.popup_focus_key_hint();
        let (popup_bg, popup_border_fg) = {
            let theme = self.theme();
            (theme.popup_bg, theme.popup_border_fg)
        };
        let window = self
            .windows
            .get_mut(&window_id)
            .expect("source window checked above");
        let buffer_id = window.active_buffer();
        let state = window
            .buffers
            .get_mut(&buffer_id)
            .expect("active buffer must exist");
        let mut popup =
            crate::state::convert_popup_data_to_popup(&popup_data, popup_bg, popup_border_fg);
        popup.accept_key_hint = accept_hint;
        popup.resolver = crate::view::popup::PopupResolver::Completion;
        popup.focus_key_hint = focus_hint;
        state.popups.show_or_replace(popup);
        tracing::info!(
            "Showing completion popup with {} items",
            window.completion_items.as_ref().map_or(0, Vec::len)
        );
        Ok(())
    }

    /// Handle LSP go-to-definition response
    pub(crate) fn handle_goto_definition_response(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        locations: Vec<lsp_types::Location>,
    ) -> AnyhowResult<()> {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return Ok(());
        };
        if window.pending_goto_definition_request != Some(request_id) {
            tracing::debug!(
                "Ignoring go-to-definition response for outdated request {}",
                request_id
            );
            return Ok(());
        }
        window.pending_goto_definition_request = None;
        let Some(location) = locations.first() else {
            window.status_message = Some(t!("lsp.no_definition").to_string());
            return Ok(());
        };

        if let Some(scheme) = location
            .uri
            .scheme()
            .map(|scheme| scheme.as_str().to_string())
            .filter(|scheme| scheme != "file")
        {
            let uri = location.uri.as_str().to_string();
            let line = location.range.start.line;
            let character = location.range.start.character;
            if self.lsp_uri_schemes.contains(&scheme) {
                let (language, server_name) = self
                    .windows
                    .get(&window_id)
                    .map(|window| {
                        let language = window.active_state().language.clone();
                        let server_name = window
                            .lsp
                            .server_names_for_language(&language)
                            .into_iter()
                            .next()
                            .unwrap_or_default();
                        (language, server_name)
                    })
                    .unwrap_or_default();
                self.plugin_manager.read().unwrap().run_hook(
                    "lsp_open_external_uri",
                    crate::services::plugins::hooks::HookArgs::LspOpenExternalUri {
                        uri,
                        scheme,
                        line,
                        character,
                        language,
                        server_name,
                    },
                );
                return Ok(());
            }
            tracing::warn!(
                "Go-to-definition target is a non-file URI '{}'; no local source to open",
                uri
            );
            if let Some(window) = self.windows.get_mut(&window_id) {
                window.status_message =
                    Some(t!("lsp.definition_external_uri", uri = &uri).to_string());
            }
            return Ok(());
        }

        let wire = crate::app::types::LspUri::from_wire(location.uri.clone());
        let buffer_id = match self.open_lsp_uri_target_in_window(window_id, &wire) {
            Ok(buffer_id) => buffer_id,
            Err(error) => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.status_message =
                        Some(t!("file.error_opening", error = error.to_string()).to_string());
                }
                return Ok(());
            }
        };

        let line = location.range.start.line as usize;
        let character = location.range.start.character as usize;
        let Some(window) = self.windows.get_mut(&window_id) else {
            return Ok(());
        };
        window.set_active_buffer(buffer_id);
        let position = window
            .buffers
            .get(&buffer_id)
            .map(|state| state.buffer.line_col_to_position(line, character));
        if let Some(position) = position {
            let cursors = window.active_cursors();
            let primary = cursors.primary();
            let event = crate::model::event::Event::MoveCursor {
                cursor_id: cursors.primary_id(),
                old_position: primary.position,
                new_position: position,
                old_anchor: primary.anchor,
                new_anchor: None,
                old_sticky_column: primary.sticky_column,
                new_sticky_column: None,
            };
            let split_id = window
                .buffers
                .splits()
                .map(|(manager, _)| manager.active_split())
                .expect("window must have a populated split layout");
            window.apply_event_to_buffer(buffer_id, split_id, &event);
            window.ensure_active_cursor_visible_for_navigation(true);
        }
        let display_path = window
            .buffers
            .get(&buffer_id)
            .and_then(|state| state.buffer.file_path())
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        window.status_message = Some(
            t!(
                "lsp.jumped_to_definition",
                path = display_path,
                line = line + 1
            )
            .to_string(),
        );
        Ok(())
    }

    /// Dispatch an exclusive LSP feature request to the first handle that allows the feature.
    ///
    /// Ensures all handles receive didOpen first, then calls the closure with the first
    /// handle matching the feature filter. For features like hover, definition, rename, etc.
    pub(crate) fn with_lsp_for_buffer<F, R>(
        &mut self,
        buffer_id: BufferId,
        feature: LspFeature,
        f: F,
    ) -> Option<R>
    where
        F: FnOnce(&LspHandle, &crate::app::types::LspUri, &str) -> R,
    {
        self.with_lsp_for_buffer_in_window(self.active_window, buffer_id, feature, f)
    }

    pub(crate) fn with_lsp_for_buffer_in_window<F, R>(
        &mut self,
        window_id: fresh_core::WindowId,
        buffer_id: BufferId,
        feature: LspFeature,
        f: F,
    ) -> Option<R>
    where
        F: FnOnce(&LspHandle, &crate::app::types::LspUri, &str) -> R,
    {
        use crate::services::lsp::manager::LspSpawnResult;

        let (uri, language, file_path) = {
            let window = self.windows.get(&window_id)?;
            let metadata = window.buffer_metadata.get(&buffer_id)?;
            if !metadata.lsp_enabled {
                return None;
            }
            (
                metadata.file_uri()?.clone(),
                window.buffers.get(&buffer_id)?.language.clone(),
                metadata.file_path().cloned(),
            )
        };
        let window = self.windows.get_mut(&window_id)?;
        if window.lsp.try_spawn(&language, file_path.as_deref()) != LspSpawnResult::Spawned {
            return None;
        }
        self.ensure_did_open_all_in_window(window_id, buffer_id, &uri, &language)?;
        let server = self
            .windows
            .get_mut(&window_id)?
            .lsp
            .handle_for_feature_mut(&language, feature)?;
        Some(f(&server.handle, &uri, &language))
    }

    /// Dispatch a merged LSP feature request to all handles that allow the feature.
    ///
    /// Ensures all handles receive didOpen first, then calls the closure for each
    /// handle matching the feature filter, collecting all results. For features like
    /// completion, code actions, diagnostics, etc.
    pub(crate) fn with_all_lsp_for_buffer_feature<F, R>(
        &mut self,
        buffer_id: BufferId,
        feature: LspFeature,
        f: F,
    ) -> Vec<R>
    where
        F: Fn(&LspHandle, &crate::app::types::LspUri, &str) -> R,
    {
        use crate::services::lsp::manager::LspSpawnResult;

        let (uri, language, file_path) = match (|| {
            let metadata = self.active_window().buffer_metadata.get(&buffer_id)?;
            if !metadata.lsp_enabled {
                return None;
            }
            let uri = metadata.file_uri()?.clone();
            let file_path = metadata.file_path().cloned();
            let language = self
                .windows
                .get(&self.active_window)
                .map(|w| &w.buffers)
                .expect("active window present")
                .get(&buffer_id)?
                .language
                .clone();
            Some((uri, language, file_path))
        })() {
            Some(v) => v,
            None => return Vec::new(),
        };

        let lsp = match self.lsp_mut() {
            Some(l) => l,
            None => return Vec::new(),
        };
        if lsp.try_spawn(&language, file_path.as_deref()) != LspSpawnResult::Spawned {
            return Vec::new();
        }

        // Ensure didOpen is sent to all handles
        if self
            .ensure_did_open_all(buffer_id, &uri, &language)
            .is_none()
        {
            return Vec::new();
        }

        // Dispatch to all handles that allow this feature
        let lsp = match self.lsp_mut() {
            Some(l) => l,
            None => return Vec::new(),
        };
        lsp.handles_for_feature_mut(&language, feature)
            .into_iter()
            .map(|sh| f(&sh.handle, &uri, &language))
            .collect()
    }

    /// Like `with_all_lsp_for_buffer_feature`, but also passes the server name
    /// to the closure for attribution purposes.
    pub(crate) fn with_all_lsp_for_buffer_feature_named<F, R>(
        &mut self,
        buffer_id: BufferId,
        feature: LspFeature,
        f: F,
    ) -> Vec<R>
    where
        F: Fn(&LspHandle, &crate::app::types::LspUri, &str, &str) -> R,
    {
        use crate::services::lsp::manager::LspSpawnResult;

        let (uri, language, file_path) = match (|| {
            let metadata = self.active_window().buffer_metadata.get(&buffer_id)?;
            if !metadata.lsp_enabled {
                return None;
            }
            let uri = metadata.file_uri()?.clone();
            let file_path = metadata.file_path().cloned();
            let language = self
                .windows
                .get(&self.active_window)
                .map(|w| &w.buffers)
                .expect("active window present")
                .get(&buffer_id)?
                .language
                .clone();
            Some((uri, language, file_path))
        })() {
            Some(v) => v,
            None => return Vec::new(),
        };

        let lsp = match self.lsp_mut() {
            Some(l) => l,
            None => return Vec::new(),
        };
        if lsp.try_spawn(&language, file_path.as_deref()) != LspSpawnResult::Spawned {
            return Vec::new();
        }

        if self
            .ensure_did_open_all(buffer_id, &uri, &language)
            .is_none()
        {
            return Vec::new();
        }

        let lsp = match self.lsp_mut() {
            Some(l) => l,
            None => return Vec::new(),
        };
        lsp.handles_for_feature_mut(&language, feature)
            .into_iter()
            .map(|sh| f(&sh.handle, &uri, &language, &sh.name))
            .collect()
    }

    /// Ensure didOpen has been sent to all handles for the given buffer's language.
    /// Returns Some(()) on success, None if we can't access required state.
    fn ensure_did_open_all(
        &mut self,
        buffer_id: BufferId,
        uri: &crate::app::types::LspUri,
        language: &str,
    ) -> Option<()> {
        self.ensure_did_open_all_in_window(self.active_window, buffer_id, uri, language)
    }

    fn ensure_did_open_all_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        buffer_id: BufferId,
        uri: &crate::app::types::LspUri,
        language: &str,
    ) -> Option<()> {
        let window = self.windows.get(&window_id)?;
        let needs_open: Vec<u64> = window
            .lsp
            .get_handles(language)
            .iter()
            .map(|server| server.handle.id())
            .filter(|id| {
                window
                    .buffer_metadata
                    .get(&buffer_id)
                    .is_some_and(|metadata| !metadata.lsp_opened_with.contains(id))
            })
            .collect();
        if needs_open.is_empty() {
            return Some(());
        }
        let text = window.buffers.get(&buffer_id)?.buffer.to_string()?;
        let window = self.windows.get_mut(&window_id)?;
        let mut opened = Vec::new();
        for server in window.lsp.get_handles_mut(language) {
            if !needs_open.contains(&server.handle.id()) {
                continue;
            }
            if let Err(error) =
                server
                    .handle
                    .did_open(uri.as_uri().clone(), text.clone(), language.to_string())
            {
                tracing::warn!("Failed to send didOpen to '{}': {}", server.name, error);
            } else {
                opened.push(server.handle.id());
            }
        }
        window
            .buffer_metadata
            .get_mut(&buffer_id)?
            .lsp_opened_with
            .extend(opened);
        Some(())
    }

    /// Request LSP completion at current cursor position.
    /// Sends completion requests to all eligible servers for merged results.
    pub(crate) fn request_completion(&mut self) {
        // A new completion request starts a fresh batch. Cancel any
        // previous in-flight completion requests so their late responses
        // are ignored (handle_completion_response drops responses whose
        // request_id isn't in pending_completion_requests), and drop any
        // leftover items from a previous popup that was closed via the
        // "pass-through" path (hide_popup() without handle_popup_cancel,
        // e.g. Enter or a non-word character while the popup was open).
        // Without this, the new response would be merged into the stale
        // items by `handle_completion_response`'s extend branch, leading
        // to duplicate / stale entries in the rendered popup — see the
        // regression test in
        // crates/fresh-editor/tests/e2e/lsp_completion_duplicate_entries_1514.rs
        // and sinelaw/fresh#1514.
        if !self.active_window().pending_completion_requests.is_empty() {
            let ids: Vec<u64> = self
                .active_window_mut()
                .pending_completion_requests
                .drain()
                .collect();
            for request_id in ids {
                tracing::debug!(
                    "Canceling previous pending LSP completion request {}",
                    request_id
                );
                self.active_window_mut().send_lsp_cancel_request(request_id);
            }
        }
        self.active_window_mut().completion_items = None;

        // Get the current buffer and cursor position
        let cursor_pos = self.active_cursors().primary().position;
        let state = self.active_state();

        // Convert byte position to LSP position (line, UTF-16 code units)
        let (line, character) = state.buffer.position_to_lsp_position(cursor_pos);
        let buffer_id = self.active_buffer();

        // Pre-allocate request IDs for all eligible servers
        let base_request_id = self.active_window_mut().next_lsp_request_id;
        // Use an atomic counter in the closure
        let counter = std::sync::atomic::AtomicU64::new(0);

        let results = self.with_all_lsp_for_buffer_feature(
            buffer_id,
            LspFeature::Completion,
            |handle, uri, _language| {
                let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let request_id = base_request_id + idx;
                let result = handle.completion(
                    request_id,
                    uri.as_uri().clone(),
                    line as u32,
                    character as u32,
                );
                if result.is_ok() {
                    tracing::info!(
                        "Requested completion at {}:{}:{} (request_id={})",
                        uri.as_str(),
                        line,
                        character,
                        request_id
                    );
                }
                (request_id, result.is_ok())
            },
        );

        let mut sent_ids = Vec::new();
        for (request_id, ok) in &results {
            if *ok {
                sent_ids.push(*request_id);
            }
        }
        // Advance the ID counter past all allocated IDs
        self.active_window_mut().next_lsp_request_id = base_request_id + results.len() as u64;

        if !sent_ids.is_empty() {
            self.active_window_mut()
                .pending_completion_requests
                .extend(sent_ids);
        } else {
            // No LSP servers available — show buffer-word completions as popup.
            self.show_buffer_word_completion_popup();
        }
    }

    /// Show a completion popup with buffer-word results only (no LSP).
    ///
    /// Called when no LSP servers are available for the current buffer.
    fn show_buffer_word_completion_popup(&mut self) {
        self.show_buffer_word_completion_popup_in_window(self.active_window);
    }

    fn show_buffer_word_completion_popup_in_window(&mut self, window_id: fresh_core::WindowId) {
        let items = self.get_buffer_completion_popup_items_in_window(window_id);
        if items.is_empty() {
            return;
        }
        let popup_data = crate::app::popup_actions::build_completion_popup_from_items(items, 0);
        let accept_hint = self.completion_accept_key_hint();
        let focus_hint = self.popup_focus_key_hint();
        let (popup_bg, popup_border_fg) = {
            let theme = self.theme();
            (theme.popup_bg, theme.popup_border_fg)
        };
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let buffer_id = window.active_buffer();
        let Some(state) = window.buffers.get_mut(&buffer_id) else {
            return;
        };
        let mut popup =
            crate::state::convert_popup_data_to_popup(&popup_data, popup_bg, popup_border_fg);
        popup.accept_key_hint = accept_hint;
        popup.resolver = crate::view::popup::PopupResolver::Completion;
        popup.focus_key_hint = focus_hint;
        state.popups.show_or_replace(popup);
    }

    /// Check if the inserted character should trigger completion
    /// and if so, request completion automatically (possibly after a delay).
    ///
    /// Only triggers when `completion_popup_auto_show` is enabled. Then:
    /// 1. Trigger characters (like `.`, `::`, etc.): immediate if suggest_on_trigger_characters is enabled
    /// 2. Word characters: delayed by quick_suggestions_delay_ms if quick_suggestions is enabled
    ///
    /// This provides VS Code-like behavior where suggestions appear while typing,
    /// with debouncing to avoid spamming the LSP server.
    pub(crate) fn maybe_trigger_completion(&mut self, c: char) {
        // Auto-show must be enabled for any automatic triggering
        if !self.config.editor.completion_popup_auto_show {
            return;
        }

        // Get the active buffer's language
        let language = self.active_state().language.clone();

        // Check if this character is a trigger character for this language
        let is_lsp_trigger = self
            .lsp()
            .as_ref()
            .map(|lsp| lsp.is_completion_trigger_char(c, &language))
            .unwrap_or(false);

        // Check if quick suggestions is enabled and this is a word character
        let quick_suggestions_enabled = self.config.editor.quick_suggestions;
        let suggest_on_trigger_chars = self.config.editor.suggest_on_trigger_characters;
        let is_word_char = c.is_alphanumeric() || c == '_';

        // Case 1: Trigger character - immediate trigger (bypasses delay)
        if is_lsp_trigger && suggest_on_trigger_chars {
            tracing::debug!(
                "Trigger character '{}' immediately triggers completion for language {}",
                c,
                language
            );
            // Cancel any pending scheduled trigger
            self.active_window_mut().scheduled_completion_trigger = None;
            self.request_completion();
            return;
        }

        // Case 2: Word character with quick suggestions - schedule delayed trigger
        if quick_suggestions_enabled && is_word_char {
            let delay_ms = self.config.editor.quick_suggestions_delay_ms;
            let trigger_time = Instant::now() + Duration::from_millis(delay_ms);

            tracing::debug!(
                "Scheduling completion trigger in {}ms for language {} (char '{}')",
                delay_ms,
                language,
                c
            );

            // Schedule (or reschedule) the completion trigger
            // This effectively debounces - each keystroke resets the timer
            self.active_window_mut().scheduled_completion_trigger = Some(trigger_time);
        } else {
            // Non-word, non-trigger character (space, punctuation, etc.) —
            // cancel any pending scheduled trigger so a stale timer from the
            // previous word doesn't fire at the wrong cursor position.
            self.active_window_mut().scheduled_completion_trigger = None;
        }
    }

    /// Request LSP go-to-definition at current cursor position
    pub(crate) fn request_goto_definition(&mut self) -> AnyhowResult<()> {
        // Get the current buffer and cursor position
        let cursor_pos = self.active_cursors().primary().position;
        let state = self.active_state();

        // Convert byte position to LSP position (line, UTF-16 code units)
        let (line, character) = state.buffer.position_to_lsp_position(cursor_pos);
        let buffer_id = self.active_buffer();
        let request_id = self.active_window_mut().next_lsp_request_id;

        // Use helper to ensure didOpen is sent before the request
        let sent = self
            .with_lsp_for_buffer(
                buffer_id,
                LspFeature::Definition,
                |handle, uri, _language| {
                    let result = handle.goto_definition(
                        request_id,
                        uri.as_uri().clone(),
                        line as u32,
                        character as u32,
                    );
                    if result.is_ok() {
                        tracing::info!(
                            "Requested go-to-definition at {}:{}:{}",
                            uri.as_str(),
                            line,
                            character
                        );
                    }
                    result.is_ok()
                },
            )
            .unwrap_or(false);

        if sent {
            self.active_window_mut().next_lsp_request_id += 1;
            self.active_window_mut().pending_goto_definition_request = Some(request_id);
        }

        Ok(())
    }

    /// Request LSP hover documentation at current cursor position
    pub fn request_hover(&mut self) -> AnyhowResult<()> {
        // Get the current buffer and cursor position
        let cursor_pos = self.active_cursors().primary().position;
        let state = self.active_state();

        // Convert byte position to LSP position (line, UTF-16 code units)
        let (line, character) = state.buffer.position_to_lsp_position(cursor_pos);

        // Debug: Log the position conversion details
        if let Some(pos) = state.buffer.offset_to_position(cursor_pos) {
            tracing::debug!(
                "Hover request: cursor_byte={}, line={}, byte_col={}, utf16_col={}",
                cursor_pos,
                pos.line,
                pos.column,
                character
            );
        }

        let buffer_id = self.active_buffer();

        // Fan out to every capable server so non-null hovers can be merged
        // (a first server returning null must not hide a second server's
        // hover — sinelaw/fresh#2635). Pre-allocate ids and advance the
        // counter past all of them.
        let base_request_id = self.active_window_mut().next_lsp_request_id;
        let counter = std::sync::atomic::AtomicU64::new(0);

        let results = self.with_all_lsp_for_buffer_feature(
            buffer_id,
            LspFeature::Hover,
            |handle, uri, _language| {
                let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let request_id = base_request_id + idx;
                let result = handle.hover(
                    request_id,
                    uri.as_uri().clone(),
                    line as u32,
                    character as u32,
                );
                if result.is_ok() {
                    tracing::info!(
                        "Requested hover at {}:{}:{} (request_id={}, byte_pos={})",
                        uri.as_str(),
                        line,
                        character,
                        request_id,
                        cursor_pos
                    );
                }
                (request_id, result.is_ok())
            },
        );

        let sent_ids: Vec<u64> = results
            .iter()
            .filter_map(|(id, ok)| ok.then_some(*id))
            .collect();
        self.active_window_mut().next_lsp_request_id = base_request_id + results.len() as u64;

        if !sent_ids.is_empty() {
            self.active_window_mut().hover.record_requests(
                &sent_ids,
                line as u32,
                character as u32,
            );
        }

        Ok(())
    }

    /// Request LSP hover documentation at a specific byte position
    /// Used for mouse-triggered hover
    /// Returns `Ok(true)` if the request was dispatched, `Ok(false)` if no
    /// eligible server was available (e.g. not yet initialized).
    pub(crate) fn request_hover_at_position(
        &mut self,
        byte_pos: usize,
        buffer_id: BufferId,
    ) -> AnyhowResult<bool> {
        // Resolve against the buffer the pointer is over — not necessarily the
        // active one. Hovering a non-active split (or a virtual UI buffer with
        // no language server, like the Search/Replace panel) must query that
        // buffer, so the hover card never leaks in from the active buffer
        // (#2572). A virtual buffer has no `file_uri`, so `with_lsp_for_buffer`
        // below returns `None` and nothing pops up.
        let Some(state) = self.buffers().get(&buffer_id) else {
            return Ok(false);
        };

        // Convert byte position to LSP position (line, UTF-16 code units)
        let (line, character) = state.buffer.position_to_lsp_position(byte_pos);

        // Debug: Log the position conversion details
        if let Some(pos) = state.buffer.offset_to_position(byte_pos) {
            tracing::trace!(
                "Mouse hover request: byte_pos={}, line={}, byte_col={}, utf16_col={}",
                byte_pos,
                pos.line,
                pos.column,
                character
            );
        }

        // Fan out to every capable server (see `request_hover`). Query the
        // buffer the pointer is over (the `buffer_id` argument), not the active
        // buffer, so the hover card never leaks in from another buffer (#2572).
        // (Reassigning `buffer_id` to `self.active_buffer()` here was the source
        // of that leak.)
        let base_request_id = self.active_window_mut().next_lsp_request_id;
        let counter = std::sync::atomic::AtomicU64::new(0);

        let results = self.with_all_lsp_for_buffer_feature(
            buffer_id,
            LspFeature::Hover,
            |handle, uri, _language| {
                let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let request_id = base_request_id + idx;
                let result = handle.hover(
                    request_id,
                    uri.as_uri().clone(),
                    line as u32,
                    character as u32,
                );
                if result.is_ok() {
                    tracing::trace!(
                        "Mouse hover requested at {}:{}:{} (request_id={}, byte_pos={})",
                        uri.as_str(),
                        line,
                        character,
                        request_id,
                        byte_pos
                    );
                }
                (request_id, result.is_ok())
            },
        );

        let sent_ids: Vec<u64> = results
            .iter()
            .filter_map(|(id, ok)| ok.then_some(*id))
            .collect();
        self.active_window_mut().next_lsp_request_id = base_request_id + results.len() as u64;

        if !sent_ids.is_empty() {
            self.active_window_mut().hover.record_requests(
                &sent_ids,
                line as u32,
                character as u32,
            );
        }

        Ok(!sent_ids.is_empty())
    }

    /// Handle hover response from LSP
    pub(crate) fn handle_hover_response(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        contents: String,
        is_markdown: bool,
        range: Option<((u32, u32), (u32, u32))>,
    ) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let Some(position) = window.hover.claim_pending(request_id) else {
            tracing::debug!("Ignoring stale hover response: {}", request_id);
            return;
        };
        if window.active_state().popups.top().is_some_and(|popup| {
            matches!(popup.resolver, crate::view::popup::PopupResolver::LspStatus)
        }) {
            window.hover.set_symbol_range(None);
            return;
        }
        if !contents.is_empty() {
            window.hover.push_payload(crate::app::hover::HoverPayload {
                contents,
                is_markdown,
                range,
                server_name: None,
            });
        }
        let all_in = window.hover.pending_is_empty();
        let accumulated_empty = window.hover.accumulated().is_empty();
        let diagnostic_lines = self.compose_hover_diagnostic_lines(window_id, position);
        if all_in && accumulated_empty && diagnostic_lines.is_empty() {
            if let Some(window) = self.windows.get_mut(&window_id) {
                window.status_message = Some(t!("lsp.no_hover").to_string());
                window.hover.set_symbol_range(None);
            }
            return;
        }
        if accumulated_empty && !all_in {
            return;
        }
        let payloads: Vec<crate::app::hover::HoverPayload> = self
            .windows
            .get(&window_id)
            .expect("source window checked above")
            .hover
            .accumulated()
            .to_vec();

        // Symbol range/overlay: use the FIRST payload that carries a range.
        // Because we always scan the whole accumulator, a range set by an
        // earlier server is never clobbered by a later rangeless server's
        // word-boundary fallback.
        let first_range = payloads.iter().find_map(|payload| payload.range);
        let window = self
            .windows
            .get_mut(&window_id)
            .expect("source window checked above");
        if let Some(((start_line, start_char), (end_line, end_char))) = first_range {
            let start_byte = window
                .active_state()
                .buffer
                .lsp_position_to_byte(start_line as usize, start_char as usize);
            let end_byte = window
                .active_state()
                .buffer
                .lsp_position_to_byte(end_line as usize, end_char as usize);
            window.hover.set_symbol_range(Some((start_byte, end_byte)));
            if let Some(old_handle) = window.hover.take_symbol_overlay() {
                let buffer_id = window.active_buffer();
                let split_id = window
                    .buffers
                    .splits()
                    .map(|(manager, _)| manager.active_split())
                    .expect("window must have a populated split layout");
                window.apply_event_to_buffer(
                    buffer_id,
                    split_id,
                    &crate::model::event::Event::RemoveOverlay { handle: old_handle },
                );
            }
            let handle = window.active_state_mut().add_overlay(
                None,
                start_byte..end_byte,
                crate::model::event::OverlayFace::Background {
                    color: (80, 80, 120),
                },
                90,
                None,
                false,
                None,
            );
            window.hover.set_symbol_overlay(handle);
        } else {
            let computed_range = window.mouse_state.lsp_hover_state.and_then(
                |(hover_byte_pos, _, _, _, hover_buffer)| {
                    let state = window
                        .buffers
                        .get(&hover_buffer)
                        .unwrap_or_else(|| window.active_state());
                    let start_byte = find_word_start(&state.buffer, hover_byte_pos);
                    let end_byte = find_word_end(&state.buffer, hover_byte_pos);
                    (start_byte < end_byte).then_some((start_byte, end_byte))
                },
            );
            window.hover.set_symbol_range(computed_range);
        }

        // Create a popup with the merged hover contents.
        //
        // When a diagnostic overlaps the hover position, we pre-style its
        // lines (severity-colored header + plain message) and concatenate
        // with the parsed hover body into a single `PopupContent::Markdown`
        // vector. This avoids the previous approach of injecting a
        // `**bold**` heading and a `---` horizontal rule into the markdown
        // input — which rendered as uncolored bold text + a thick 40-cell
        // divider with blank-line padding, wasting vertical space and
        // losing the "this is an error" visual signal.
        //
        // Multiple servers' hover bodies are joined with the same one-row
        // separator used between diagnostics and hover.
        use crate::view::markdown::{parse_markdown, StyledLine};
        use crate::view::popup::{Popup, PopupContent, PopupPosition};
        use ratatui::style::Style;
        use unicode_width::UnicodeWidthStr;

        let is_markdown = payloads.iter().any(|p| p.is_markdown);

        // Build one styled block per non-empty payload.
        let mut bodies: Vec<Vec<StyledLine>> = Vec::new();
        for payload in &payloads {
            if payload.contents.is_empty() {
                continue;
            }
            let lines: Vec<StyledLine> = if payload.is_markdown {
                parse_markdown(
                    &payload.contents,
                    &self.theme.read().unwrap(),
                    Some(&self.grammar_registry),
                )
            } else {
                payload
                    .contents
                    .lines()
                    .map(|s| {
                        let mut sl = StyledLine::new();
                        sl.push(
                            s.to_string(),
                            Style::default().fg(self.theme.read().unwrap().popup_text_fg),
                        );
                        sl
                    })
                    .collect()
            };
            if !lines.is_empty() {
                bodies.push(lines);
            }
        }

        // One-row separator between sections (diagnostics ↔ hover, and hover ↔
        // hover when more than one server answered). Compact — no blank
        // padding, no 40-cell dash run; popup border color so it reads as
        // "same card, new section."
        let make_separator = || {
            let mut sep = StyledLine::new();
            sep.push(
                "─".repeat(12),
                Style::default().fg(self.theme.read().unwrap().popup_border_fg),
            );
            sep
        };

        let has_diagnostic = !diagnostic_lines.is_empty();
        let mut all_lines: Vec<StyledLine> = Vec::new();
        all_lines.extend(diagnostic_lines);
        let mut need_separator = has_diagnostic;
        for body in bodies {
            if need_separator {
                all_lines.push(make_separator());
            }
            all_lines.extend(body);
            need_separator = true;
        }

        // Drop trailing empty lines that some markdown payloads carry.
        while all_lines
            .last()
            .map(|l| l.spans.iter().all(|s| s.text.trim().is_empty()))
            .unwrap_or(false)
        {
            all_lines.pop();
        }

        // Fit width to content so short hovers stop rendering in an 80-col
        // card with half the width empty. Measured as the widest styled
        // line (display cells, not bytes), plus 4 for borders + padding,
        // clamped to [30, 80]. Height stays dynamic on terminal size.
        let content_width: usize = all_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.text.as_str()))
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        let popup_width = (content_width as u16 + 4).clamp(30, 80);
        let dynamic_height = (self.terminal_height * 60 / 100).clamp(15, 40);

        // Construct the popup with the fused content.
        let mut popup = Popup::text(Vec::new(), &self.theme.read().unwrap());
        popup.content = PopupContent::Markdown(all_lines);
        popup.title = Some(t!("lsp.popup_hover").to_string());
        popup.transient = true;
        popup.position = self
            .windows
            .get_mut(&window_id)
            .and_then(|window| window.hover.take_screen_position())
            .map_or(PopupPosition::BelowCursor, |(x, y)| PopupPosition::Fixed {
                x,
                y: y + 1,
            });
        popup.width = popup_width;
        popup.max_height = dynamic_height;
        popup.border_style = Style::default().fg(self.theme.read().unwrap().popup_border_fg);
        popup.background_style = Style::default().bg(self.theme.read().unwrap().popup_bg);
        popup.focus_key_hint = self.popup_focus_key_hint();

        if let Some(window) = self.windows.get_mut(&window_id) {
            let buffer_id = window.active_buffer();
            if let Some(state) = window.buffers.get_mut(&buffer_id) {
                while state.popups.top().is_some_and(|popup| popup.transient) {
                    state.popups.hide();
                }
                state.popups.show(popup);
                tracing::info!("Showing hover popup (markdown={})", is_markdown);
            }
            window.mouse_state.lsp_hover_request_sent = true;
        }
    }

    /// Pre-style any diagnostics overlapping the hover position into lines
    /// ready to stack into the hover popup. Each diagnostic yields two or
    /// more styled lines:
    ///   1. severity marker + label in `diagnostic_*_fg`, followed by
    ///      `  (source)` dimmed — italic on theme-default foreground,
    ///   2. one styled line per message line, in `popup_text_fg`.
    ///
    /// Multiple overlapping diagnostics are separated by a blank line.
    /// Returns an empty vec when there are no overlapping diagnostics,
    /// or no buffer/URI resolves.
    fn compose_hover_diagnostic_lines(
        &self,
        window_id: fresh_core::WindowId,
        lsp_pos: (u32, u32),
    ) -> Vec<crate::view::markdown::StyledLine> {
        use crate::view::markdown::StyledLine;
        use lsp_types::DiagnosticSeverity;
        use ratatui::style::{Modifier, Style};

        let Some(window) = self.windows.get(&window_id) else {
            return Vec::new();
        };
        let buffer_id = window.active_buffer();
        let Some(uri) = window
            .buffer_metadata
            .get(&buffer_id)
            .and_then(|metadata| metadata.file_uri())
        else {
            return Vec::new();
        };
        let Some(diagnostics) = window.stored_diagnostics.get(uri.as_str()) else {
            return Vec::new();
        };

        let (hover_line, hover_char) = lsp_pos;
        let overlapping: Vec<&lsp_types::Diagnostic> = diagnostics
            .iter()
            .filter(|d| lsp_range_contains(&d.range, hover_line, hover_char))
            .collect();

        if overlapping.is_empty() {
            return Vec::new();
        }

        let mut out: Vec<StyledLine> = Vec::new();
        for (idx, diag) in overlapping.iter().enumerate() {
            if idx > 0 {
                out.push(StyledLine::new());
            }

            let (label, marker, severity_color) = match diag.severity {
                Some(DiagnosticSeverity::ERROR) => {
                    ("Error", "✖", self.theme.read().unwrap().diagnostic_error_fg)
                }
                Some(DiagnosticSeverity::WARNING) => (
                    "Warning",
                    "⚠",
                    self.theme.read().unwrap().diagnostic_warning_fg,
                ),
                Some(DiagnosticSeverity::INFORMATION) => {
                    ("Info", "ℹ", self.theme.read().unwrap().diagnostic_info_fg)
                }
                Some(DiagnosticSeverity::HINT) => {
                    ("Hint", "ℹ", self.theme.read().unwrap().diagnostic_hint_fg)
                }
                _ => ("Diagnostic", "•", self.theme.read().unwrap().popup_text_fg),
            };

            let header_style = Style::default()
                .fg(severity_color)
                .add_modifier(Modifier::BOLD);
            let mut header = StyledLine::new();
            header.push(format!("{} {}", marker, label), header_style);
            if let Some(source) = diag.source.as_deref().filter(|s| !s.is_empty()) {
                // Dim italic source tag — reads as metadata, not as part
                // of the diagnostic text.
                header.push(
                    format!("  ({})", source),
                    Style::default()
                        .fg(self.theme.read().unwrap().tab_inactive_fg)
                        .add_modifier(Modifier::ITALIC),
                );
            }
            out.push(header);

            // Message verbatim: one styled line per message line. Using
            // `popup_text_fg` lets themes override the body color; the
            // severity information is already conveyed by the header.
            for message_line in diag.message.lines() {
                let mut line = StyledLine::new();
                line.push(
                    message_line.to_string(),
                    Style::default().fg(self.theme.read().unwrap().popup_text_fg),
                );
                out.push(line);
            }
        }
        out
    }

    /// Apply inlay hints to editor state as virtual text
    #[doc(hidden)]
    pub fn apply_inlay_hints_to_state(
        state: &mut crate::state::EditorState,
        hints: &[lsp_types::InlayHint],
    ) {
        use crate::view::virtual_text::VirtualTextPosition;
        use ratatui::style::{Color, Style};

        // Clear existing inlay hints
        state.virtual_texts.clear(&mut state.marker_list);

        if hints.is_empty() {
            return;
        }

        // Fallback style for inlay hints - dimmed to not distract from actual
        // code. The actual on-screen color is resolved from the theme key
        // below (`editor.line_number_fg`) so the hints follow the active
        // theme. This fallback only applies when the theme doesn't define
        // the key.
        let hint_style = Style::default().fg(Color::Rgb(128, 128, 128));
        let hint_fg_theme_key = Some("editor.line_number_fg".to_string());

        for hint in hints {
            // Convert LSP position to byte offset
            let byte_offset = state.buffer.lsp_position_to_byte(
                hint.position.line as usize,
                hint.position.character as usize,
            );

            // Extract text from hint label
            let text = match &hint.label {
                lsp_types::InlayHintLabel::String(s) => s.clone(),
                lsp_types::InlayHintLabel::LabelParts(parts) => {
                    parts.iter().map(|p| p.value.as_str()).collect::<String>()
                }
            };

            // LSP inlay hint positions are insertion points between characters.
            // For positions within the buffer, render hints before the character at the
            // byte offset so they appear at the correct location (e.g., before punctuation
            // or newline). Hints at or beyond EOF are anchored to the last character and
            // rendered after it.
            if state.buffer.is_empty() {
                continue;
            }

            // Pick the anchor character for this hint. If the LSP-computed
            // byte lies on a line terminator (\n or the \r of a CRLF), the
            // "following character" is the first byte of the next line.
            // Anchoring to it would make the hint drift one line down on
            // any whitespace edit adjacent to the brace (issue #1572), so
            // instead anchor to the *preceding* non-newline character with
            // `AfterChar`. That keeps the hint stuck to the glyph the LSP
            // intended to annotate even as edits shift bytes around it.
            let buf_len = state.buffer.len();
            let byte_here = if byte_offset < buf_len {
                state
                    .buffer
                    .slice_bytes(byte_offset..byte_offset + 1)
                    .first()
                    .copied()
            } else {
                None
            };
            let at_line_break = matches!(byte_here, Some(b'\n' | b'\r'));

            let (byte_offset, position) = if byte_offset >= buf_len {
                // Hint is at EOF: anchor to last character and render
                // after it.
                (buf_len.saturating_sub(1), VirtualTextPosition::AfterChar)
            } else if at_line_break && byte_offset > 0 {
                // Hint points past the last glyph on a line: anchor to
                // that glyph with AfterChar so the marker cannot drift
                // onto a subsequent line when whitespace is edited.
                (byte_offset - 1, VirtualTextPosition::AfterChar)
            } else {
                (byte_offset, VirtualTextPosition::BeforeChar)
            };

            // Use the hint text as-is - spacing is handled during rendering
            let display_text = text;

            state.virtual_texts.add_with_theme_keys(
                &mut state.marker_list,
                byte_offset,
                display_text,
                hint_style,
                hint_fg_theme_key.clone(),
                None,
                position,
                0, // Default priority
            );
        }

        tracing::debug!("Applied {} inlay hints as virtual text", hints.len());
    }

    /// Request LSP find references at current cursor position
    pub(crate) fn request_references(&mut self) -> AnyhowResult<()> {
        use crate::primitives::word_navigation::{find_word_end, find_word_start};

        let cursor_pos = self.active_cursors().primary().position;
        let (line, character, symbol) = {
            let state = self.active_state();
            let (line, character) = state.buffer.position_to_lsp_position(cursor_pos);
            let word_start = find_word_start(&state.buffer, cursor_pos);
            let word_end = find_word_end(&state.buffer, cursor_pos);
            let symbol = String::from_utf8_lossy(&state.buffer.slice_bytes(word_start..word_end))
                .into_owned();
            (line, character, symbol)
        };

        let buffer_id = self.active_buffer();
        let request_id = self.active_window_mut().next_lsp_request_id;

        // Use helper to ensure didOpen is sent before the request
        let sent = self
            .with_lsp_for_buffer(
                buffer_id,
                LspFeature::References,
                |handle, uri, _language| {
                    let result = handle.references(
                        request_id,
                        uri.as_uri().clone(),
                        line as u32,
                        character as u32,
                    );
                    if result.is_ok() {
                        tracing::info!(
                            "Requested find references at {}:{}:{} (byte_pos={})",
                            uri.as_str(),
                            line,
                            character,
                            cursor_pos
                        );
                    }
                    result.is_ok()
                },
            )
            .unwrap_or(false);

        if sent {
            self.active_window_mut().next_lsp_request_id += 1;
            self.active_window_mut().pending_references_request = Some(request_id);
            self.active_window_mut().pending_references_symbol = symbol;
        }

        Ok(())
    }

    /// Request LSP go-to-implementation at current cursor position
    pub(crate) fn request_implementation(&mut self) -> AnyhowResult<()> {
        use crate::primitives::word_navigation::{find_word_end, find_word_start};

        let cursor_pos = self.active_cursors().primary().position;
        let (line, character, symbol) = {
            let state = self.active_state();
            let (line, character) = state.buffer.position_to_lsp_position(cursor_pos);
            let word_start = find_word_start(&state.buffer, cursor_pos);
            let word_end = find_word_end(&state.buffer, cursor_pos);
            let symbol = String::from_utf8_lossy(&state.buffer.slice_bytes(word_start..word_end))
                .into_owned();
            (line, character, symbol)
        };

        let buffer_id = self.active_buffer();
        let request_id = self.active_window_mut().next_lsp_request_id;

        // Use helper to ensure didOpen is sent before the request
        let sent = self
            .with_lsp_for_buffer(
                buffer_id,
                LspFeature::Implementation,
                |handle, uri, _language| {
                    let result = handle.implementation(
                        request_id,
                        uri.as_uri().clone(),
                        line as u32,
                        character as u32,
                    );
                    if result.is_ok() {
                        tracing::info!(
                            "Requested go-to-implementation at {}:{}:{} (byte_pos={})",
                            uri.as_str(),
                            line,
                            character,
                            cursor_pos
                        );
                    }
                    result.is_ok()
                },
            )
            .unwrap_or(false);

        if sent {
            self.active_window_mut().next_lsp_request_id += 1;
            self.active_window_mut().pending_implementation_request = Some(request_id);
            self.active_window_mut().pending_implementation_symbol = symbol;
        }

        Ok(())
    }

    /// Request LSP signature help at current cursor position
    pub(crate) fn request_signature_help(&mut self) {
        // Get the current buffer and cursor position
        let cursor_pos = self.active_cursors().primary().position;
        let state = self.active_state();

        // Convert byte position to LSP position (line, UTF-16 code units)
        let (line, character) = state.buffer.position_to_lsp_position(cursor_pos);
        let buffer_id = self.active_buffer();
        let request_id = self.active_window_mut().next_lsp_request_id;

        // Use helper to ensure didOpen is sent before the request
        let sent = self
            .with_lsp_for_buffer(
                buffer_id,
                LspFeature::SignatureHelp,
                |handle, uri, _language| {
                    let result = handle.signature_help(
                        request_id,
                        uri.as_uri().clone(),
                        line as u32,
                        character as u32,
                    );
                    if result.is_ok() {
                        tracing::info!(
                            "Requested signature help at {}:{}:{} (byte_pos={})",
                            uri.as_str(),
                            line,
                            character,
                            cursor_pos
                        );
                    }
                    result.is_ok()
                },
            )
            .unwrap_or(false);

        if sent {
            self.active_window_mut().next_lsp_request_id += 1;
            self.active_window_mut().pending_signature_help_request = Some(request_id);
        }
    }

    /// Handle signature help response from LSP
    pub(crate) fn handle_signature_help_response(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        signature_help: Option<lsp_types::SignatureHelp>,
    ) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        if window.pending_signature_help_request != Some(request_id) {
            tracing::debug!("Ignoring stale signature help response: {}", request_id);
            return;
        }
        window.pending_signature_help_request = None;
        let signature_help = match signature_help {
            Some(help) if !help.signatures.is_empty() => help,
            _ => {
                tracing::debug!("No signature help available");
                return;
            }
        };

        // Get the active signature
        let active_signature_idx = signature_help.active_signature.unwrap_or(0) as usize;
        let signature = match signature_help.signatures.get(active_signature_idx) {
            Some(sig) => sig,
            None => return,
        };

        // Build the display content as markdown
        let mut content = String::new();

        // Add the signature label (function signature)
        content.push_str(&signature.label);
        content.push('\n');

        // Add parameter highlighting info
        let active_param = signature_help
            .active_parameter
            .or(signature.active_parameter)
            .unwrap_or(0) as usize;

        // If there are parameters, highlight the active one
        if let Some(params) = &signature.parameters {
            if let Some(param) = params.get(active_param) {
                // Get parameter label
                let param_label = match &param.label {
                    lsp_types::ParameterLabel::Simple(s) => s.clone(),
                    lsp_types::ParameterLabel::LabelOffsets(offsets) => {
                        // Extract substring from signature label
                        let start = offsets[0] as usize;
                        let end = offsets[1] as usize;
                        if end <= signature.label.len() {
                            signature.label[start..end].to_string()
                        } else {
                            String::new()
                        }
                    }
                };

                if !param_label.is_empty() {
                    content.push_str(&format!("\n> {}\n", param_label));
                }

                // Add parameter documentation if available
                if let Some(doc) = &param.documentation {
                    let doc_text = match doc {
                        lsp_types::Documentation::String(s) => s.clone(),
                        lsp_types::Documentation::MarkupContent(m) => m.value.clone(),
                    };
                    if !doc_text.is_empty() {
                        content.push('\n');
                        content.push_str(&doc_text);
                        content.push('\n');
                    }
                }
            }
        }

        // Add function documentation if available
        if let Some(doc) = &signature.documentation {
            let doc_text = match doc {
                lsp_types::Documentation::String(s) => s.clone(),
                lsp_types::Documentation::MarkupContent(m) => m.value.clone(),
            };
            if !doc_text.is_empty() {
                content.push_str("\n---\n\n");
                content.push_str(&space_doc_paragraphs(&doc_text));
            }
        }

        // Create a popup with markdown rendering (like hover popup)
        use crate::view::popup::{Popup, PopupPosition};
        use ratatui::style::Style;

        let mut popup = Popup::markdown(
            &content,
            &self.theme.read().unwrap(),
            Some(&self.grammar_registry),
        );
        popup.title = Some(t!("lsp.popup_signature").to_string());
        popup.transient = true;
        popup.position = PopupPosition::BelowCursor;
        popup.width = 60;
        popup.max_height = 20;
        popup.border_style = Style::default().fg(self.theme.read().unwrap().popup_border_fg);
        popup.background_style = Style::default().bg(self.theme.read().unwrap().popup_bg);
        popup.focus_key_hint = self.popup_focus_key_hint();

        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let buffer_id = window.active_buffer();
        if let Some(state) = window.buffers.get_mut(&buffer_id) {
            state.popups.show(popup);
            tracing::info!(
                "Showing signature help popup for {} signatures",
                signature_help.signatures.len()
            );
        }
    }

    /// Request LSP code actions at current cursor position.
    /// Sends code action requests to all eligible servers for merged results.
    pub(crate) fn request_code_actions(&mut self) -> AnyhowResult<()> {
        // A new invocation starts a fresh batch. Cancel any previous
        // in-flight code-action requests so their late responses are
        // ignored (handle_code_actions_response drops responses whose
        // request_id isn't in pending_code_actions_requests). Without
        // this, actions from a prior cursor position would be merged
        // into the new popup — same bug class we already avoid for
        // completion (sinelaw/fresh#1514) and inlay hints (multi-buffer
        // quiescent).
        if !self
            .active_window()
            .pending_code_actions_requests
            .is_empty()
        {
            let ids: Vec<u64> = self
                .active_window_mut()
                .pending_code_actions_requests
                .drain()
                .collect();
            for request_id in ids {
                tracing::debug!(
                    "Canceling previous pending LSP code actions request {}",
                    request_id
                );
                self.active_window_mut().send_lsp_cancel_request(request_id);
            }
        }
        self.active_window_mut()
            .pending_code_actions_server_names
            .clear();
        self.active_window_mut().pending_code_actions = None;

        // Get the current buffer and cursor position
        let cursor_pos = self.active_cursors().primary().position;
        let selection_range = self.active_cursors().primary().selection_range();
        let state = self.active_state();

        // Convert byte position to LSP position (line, UTF-16 code units)
        let (line, character) = state.buffer.position_to_lsp_position(cursor_pos);

        // Get selection range (if any) or use cursor position
        let (start_line, start_char, end_line, end_char) = if let Some(range) = selection_range {
            let (s_line, s_char) = state.buffer.position_to_lsp_position(range.start);
            let (e_line, e_char) = state.buffer.position_to_lsp_position(range.end);
            (s_line as u32, s_char as u32, e_line as u32, e_char as u32)
        } else {
            (line as u32, character as u32, line as u32, character as u32)
        };

        let buffer_id = self.active_buffer();

        // Populate `context.diagnostics` with stored diagnostics whose ranges
        // overlap the code-action request range. Many servers (clangd,
        // eslint, ...) gate quickfix actions on this — without it, every
        // diagnostic-driven "fix available" produces zero actions
        // (sinelaw/fresh#2212).
        let diagnostics: Vec<lsp_types::Diagnostic> = {
            let window = self.active_window();
            window
                .buffer_metadata
                .get(&buffer_id)
                .and_then(|m| m.file_uri())
                .and_then(|uri| window.stored_diagnostics.get(uri.as_str()))
                .map(|diags| {
                    diags
                        .iter()
                        .filter(|d| {
                            lsp_range_overlaps(&d.range, start_line, start_char, end_line, end_char)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        };

        // Pre-allocate request IDs for all eligible servers
        let base_request_id = self.active_window_mut().next_lsp_request_id;
        let counter = std::sync::atomic::AtomicU64::new(0);

        let results = self.with_all_lsp_for_buffer_feature_named(
            buffer_id,
            LspFeature::CodeAction,
            |handle, uri, _language, server_name| {
                let idx = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let request_id = base_request_id + idx;
                let result = handle.code_actions(
                    request_id,
                    uri.as_uri().clone(),
                    start_line,
                    start_char,
                    end_line,
                    end_char,
                    diagnostics.clone(),
                );
                if result.is_ok() {
                    tracing::info!(
                        "Requested code actions at {}:{}:{}-{}:{} (byte_pos={}, request_id={}, server={})",
                        uri.as_str(),
                        start_line,
                        start_char,
                        end_line,
                        end_char,
                        cursor_pos,
                        request_id,
                        server_name
                    );
                }
                (request_id, result.is_ok(), server_name.to_string())
            },
        );

        let mut sent_ids = Vec::new();
        for (request_id, ok, server_name) in &results {
            if *ok {
                sent_ids.push(*request_id);
                self.active_window_mut()
                    .pending_code_actions_server_names
                    .insert(*request_id, server_name.clone());
            }
        }
        // Advance the ID counter past all allocated IDs
        self.active_window_mut().next_lsp_request_id = base_request_id + results.len() as u64;

        if !sent_ids.is_empty() {
            // pending_code_actions was already cleared above alongside the
            // cancel-previous-requests logic.
            self.active_window_mut()
                .pending_code_actions_requests
                .extend(sent_ids);
        }

        Ok(())
    }

    /// Handle code actions response from LSP.
    /// Supports merging from multiple servers: each response extends the action
    /// list, and the popup is shown/updated with each arriving response.
    pub(crate) fn handle_code_actions_response(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        actions: Vec<lsp_types::CodeActionOrCommand>,
    ) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        if !window.pending_code_actions_requests.remove(&request_id) {
            tracing::debug!("Ignoring stale code actions response: {}", request_id);
            return;
        }
        let server_name = window
            .pending_code_actions_server_names
            .remove(&request_id)
            .unwrap_or_default();
        if actions.is_empty() {
            if window.pending_code_actions_requests.is_empty()
                && window
                    .pending_code_actions
                    .as_ref()
                    .is_none_or(Vec::is_empty)
            {
                window.status_message = Some(t!("lsp.no_code_actions").to_string());
            }
            return;
        }

        let tagged_actions = actions
            .into_iter()
            .map(|action| (server_name.clone(), action));
        match &mut window.pending_code_actions {
            Some(existing) => existing.extend(tagged_actions),
            None => window.pending_code_actions = Some(tagged_actions.collect()),
        }
        use crate::view::popup::{Popup, PopupListItem, PopupPosition};
        use ratatui::style::Style;
        let all_actions = window.pending_code_actions.as_ref().unwrap();
        let multiple_servers = all_actions
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1;
        let items: Vec<PopupListItem> = all_actions
            .iter()
            .enumerate()
            .map(|(index, (server_name, action))| {
                let (title, kind) = match action {
                    lsp_types::CodeActionOrCommand::Command(command) => {
                        (command.title.as_str(), None)
                    }
                    lsp_types::CodeActionOrCommand::CodeAction(action) => (
                        action.title.as_str(),
                        action.kind.as_ref().map(|kind| kind.as_str().to_string()),
                    ),
                };
                let detail = if multiple_servers && !server_name.is_empty() {
                    Some(match kind {
                        Some(kind) => format!("[{}] {}", server_name, kind),
                        None => format!("[{}]", server_name),
                    })
                } else {
                    kind
                };
                PopupListItem {
                    text: format!("{}. {}", index + 1, title),
                    detail,
                    icon: None,
                    data: Some(index.to_string()),
                    disabled: false,
                }
            })
            .collect();
        let action_count = all_actions.len();
        let buffer_id = window.active_buffer();

        let mut popup = Popup::list(items, &self.theme.read().unwrap());
        popup.kind = crate::view::popup::PopupKind::Action;
        popup.title = Some(t!("lsp.popup_code_actions").to_string());
        popup.position = PopupPosition::BelowCursor;
        popup.width = 60;
        popup.max_height = 15;
        popup.border_style = Style::default().fg(self.theme.read().unwrap().popup_border_fg);
        popup.background_style = Style::default().bg(self.theme.read().unwrap().popup_bg);
        popup.resolver = crate::view::popup::PopupResolver::CodeAction;
        popup.focused = true;
        if let Some(state) = self
            .windows
            .get_mut(&window_id)
            .and_then(|window| window.buffers.get_mut(&buffer_id))
        {
            state.popups.show_or_replace(popup);
            tracing::info!("Showing code actions popup with {} actions", action_count);
        }
    }

    /// Execute a code action by index from the stored pending_code_actions.
    pub(crate) fn execute_code_action(&mut self, index: usize) {
        let action = match &self.active_window_mut().pending_code_actions {
            Some(actions) => actions.get(index).map(|(_, a)| a.clone()),
            None => None,
        };

        let Some(action) = action else {
            tracing::warn!("Code action index {} out of range", index);
            return;
        };

        match action {
            lsp_types::CodeActionOrCommand::CodeAction(ca) => {
                // If the action has no edit and no command, it may need resolve first.
                // Only resolve if the action has `data` and the server supports resolveProvider.
                if ca.edit.is_none()
                    && ca.command.is_none()
                    && ca.data.is_some()
                    && self.active_window().server_supports_code_action_resolve()
                {
                    tracing::info!(
                        "Code action '{}' needs resolve, sending codeAction/resolve",
                        ca.title
                    );
                    self.send_code_action_resolve(ca);
                    return;
                }
                self.execute_resolved_code_action(ca);
            }
            lsp_types::CodeActionOrCommand::Command(cmd) => {
                self.send_execute_command(cmd);
            }
        }
    }

    /// Execute a code action that has been fully resolved (has edit and/or command).
    pub(crate) fn execute_resolved_code_action(&mut self, action: lsp_types::CodeAction) {
        self.execute_resolved_code_action_in_window(self.active_window, action);
    }

    pub(crate) fn execute_resolved_code_action_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        action: lsp_types::CodeAction,
    ) {
        let title = action.title.clone();
        if let Some(edit) = action.edit {
            match self.apply_workspace_edit_in_window(window_id, edit) {
                Ok(count) => {
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        window.status_message = Some(
                            t!("lsp.code_action_applied", title = &title, count = count)
                                .to_string(),
                        );
                    }
                }
                Err(error) => {
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        window.status_message = Some(format!("Code action failed: {error}"));
                    }
                    return;
                }
            }
        }
        if let Some(command) = action.command {
            self.send_execute_command_in_window(window_id, command);
        }
    }

    fn send_execute_command(&mut self, command: lsp_types::Command) {
        self.send_execute_command_in_window(self.active_window, command);
    }

    fn send_execute_command_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        command: lsp_types::Command,
    ) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        window.status_message = Some(
            t!(
                "lsp.code_action_applied",
                title = &command.title,
                count = 0_usize
            )
            .to_string(),
        );
        let language = window.active_state().language.clone();
        for server in window.lsp.get_handles_mut(&language) {
            if let Err(error) = server
                .handle
                .execute_command(command.command.clone(), command.arguments.clone())
            {
                tracing::warn!(
                    "Failed to send executeCommand to '{}': {}",
                    server.name,
                    error
                );
            }
        }
    }

    /// Send codeAction/resolve to the LSP server
    fn send_code_action_resolve(&mut self, action: lsp_types::CodeAction) {
        let language = match self
            .buffers()
            .get(&self.active_buffer())
            .map(|s| s.language.clone())
        {
            Some(l) => l,
            None => return,
        };

        self.active_window_mut().next_lsp_request_id += 1;
        let request_id = self.active_window_mut().next_lsp_request_id;

        let __active_id = self.active_window;

        if let Some(lsp) = self.windows.get_mut(&__active_id).map(|w| &mut w.lsp) {
            for sh in lsp.get_handles_mut(&language) {
                if let Err(e) = sh.handle.code_action_resolve(request_id, action.clone()) {
                    tracing::warn!("Failed to send codeAction/resolve to '{}': {}", sh.name, e);
                }
            }
        }
    }

    /// Handle a resolved completion item — apply additional_text_edits (e.g. auto-imports).
    pub(crate) fn handle_completion_resolved(
        &mut self,
        window_id: fresh_core::WindowId,
        item: lsp_types::CompletionItem,
    ) {
        let Some(additional_edits) = item.additional_text_edits else {
            return;
        };
        if additional_edits.is_empty() {
            return;
        }
        let Some(buffer_id) = self
            .windows
            .get(&window_id)
            .map(|window| window.active_buffer())
        else {
            return;
        };
        if let Err(error) =
            self.apply_lsp_text_edits_in_window(window_id, buffer_id, additional_edits)
        {
            tracing::error!(
                "Failed to apply completion additional_text_edits: {}",
                error
            );
        }
    }

    /// Apply formatting edits from textDocument/formatting response.
    pub(crate) fn apply_formatting_edits(
        &mut self,
        window_id: fresh_core::WindowId,
        uri: &str,
        edits: Vec<lsp_types::TextEdit>,
    ) -> AnyhowResult<usize> {
        let buffer_id = self.windows.get(&window_id).and_then(|window| {
            window
                .buffer_metadata
                .iter()
                .find(|(_, metadata)| {
                    metadata
                        .file_uri()
                        .is_some_and(|file_uri| file_uri.as_str() == uri)
                })
                .map(|(id, _)| *id)
        });
        let Some(buffer_id) = buffer_id else {
            tracing::warn!("Cannot apply formatting: no buffer for URI {}", uri);
            return Ok(0);
        };
        let count = self.apply_lsp_text_edits_in_window(window_id, buffer_id, edits)?;
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.status_message = Some(format!("Formatted ({} edits)", count));
        }
        Ok(count)
    }

    /// Request document formatting from LSP.
    ///
    /// When the primary cursor has an active selection and the server
    /// advertises range formatting, only the selected range is formatted
    /// (`textDocument/rangeFormatting`) — mirroring VS Code's "Format
    /// Selection". Otherwise the whole document is formatted
    /// (`textDocument/formatting`).
    pub(crate) fn request_formatting(&mut self) {
        let buffer_id = self.active_buffer();
        let metadata = match self.active_window().buffer_metadata.get(&buffer_id) {
            Some(m) if m.lsp_enabled => m,
            _ => {
                self.set_status_message("LSP not available for this buffer".to_string());
                return;
            }
        };

        let uri = match metadata.file_uri() {
            Some(u) => u.clone(),
            None => return,
        };

        let language = match self
            .windows
            .get(&self.active_window)
            .map(|w| &w.buffers)
            .expect("active window present")
            .get(&buffer_id)
            .map(|s| s.language.clone())
        {
            Some(l) => l,
            None => return,
        };

        let tab_size = self.config.editor.tab_size as u32;
        let insert_spaces = !self.config.editor.use_tabs;

        // Convert the active selection (if any) to LSP positions so we can
        // ask the server to format just that range.
        let selection_range = self.active_cursors().primary().selection_range();
        let selection_lsp = selection_range.map(|range| {
            let buffer = &self.active_state().buffer;
            let (s_line, s_char) = buffer.position_to_lsp_position(range.start);
            let (e_line, e_char) = buffer.position_to_lsp_position(range.end);
            (s_line as u32, s_char as u32, e_line as u32, e_char as u32)
        });

        self.active_window_mut().next_lsp_request_id += 1;
        let request_id = self.active_window_mut().next_lsp_request_id;

        let __active_id = self.active_window;

        if let Some(lsp) = self.windows.get_mut(&__active_id).map(|w| &mut w.lsp) {
            if let Some(sh) = lsp.handle_for_feature_mut(&language, LspFeature::Format) {
                // Prefer range formatting when a selection is active and the
                // server supports it; otherwise format the whole document.
                let result = match selection_lsp {
                    Some((sl, sc, el, ec)) if sh.capabilities.document_range_formatting => {
                        sh.handle.document_range_formatting(
                            request_id,
                            uri.as_uri().clone(),
                            sl,
                            sc,
                            el,
                            ec,
                            tab_size,
                            insert_spaces,
                        )
                    }
                    _ => sh.handle.document_formatting(
                        request_id,
                        uri.as_uri().clone(),
                        tab_size,
                        insert_spaces,
                    ),
                };
                if let Err(e) = result {
                    tracing::warn!("Failed to request formatting: {}", e);
                }
            } else {
                self.set_status_message("Formatting not supported by LSP server".to_string());
            }
        }
    }

    /// Whether the active buffer's LSP server advertises range formatting
    /// (`textDocument/rangeFormatting`). Used to decide whether an active
    /// selection can be range-formatted instead of falling back to a
    /// whole-file external format.
    pub(crate) fn active_lsp_supports_range_formatting(&self) -> bool {
        let buffer_id = self.active_buffer();
        let lsp_enabled = self
            .active_window()
            .buffer_metadata
            .get(&buffer_id)
            .map(|m| m.lsp_enabled)
            .unwrap_or(false);
        if !lsp_enabled {
            return false;
        }
        let language = self.active_state().language.clone();
        self.active_window()
            .lsp
            .handle_for_feature(&language, LspFeature::Format)
            .map(|sh| sh.capabilities.document_range_formatting)
            .unwrap_or(false)
    }

    /// Handle find references response from LSP
    pub(crate) fn handle_references_response(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        locations: Vec<lsp_types::Location>,
    ) -> AnyhowResult<()> {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return Ok(());
        };
        if window.pending_references_request != Some(request_id) {
            tracing::debug!("Ignoring stale references response: {}", request_id);
            return Ok(());
        }
        window.pending_references_request = None;
        if locations.is_empty() {
            window.status_message = Some(t!("lsp.no_references").to_string());
            return Ok(());
        }
        let translation = window.authority().path_translation.clone();
        let lsp_locations: Vec<crate::services::plugins::hooks::LspLocation> = locations
            .iter()
            .map(|location| {
                let wire = crate::app::types::LspUri::from_wire(location.uri.clone());
                let file = if location.uri.scheme().map(|scheme| scheme.as_str()) == Some("file") {
                    wire.to_host_path(translation.as_ref())
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| location.uri.path().as_str().to_string())
                } else {
                    location.uri.as_str().to_string()
                };
                crate::services::plugins::hooks::LspLocation {
                    file,
                    line: location.range.start.line + 1,
                    column: location.range.start.character + 1,
                }
            })
            .collect();
        let count = lsp_locations.len();
        let symbol = std::mem::take(&mut window.pending_references_symbol);
        window.status_message =
            Some(t!("lsp.found_references", count = count, symbol = &symbol).to_string());
        self.plugin_manager.read().unwrap().run_hook(
            "lsp_references",
            crate::services::plugins::hooks::HookArgs::LspReferences {
                symbol: symbol.clone(),
                locations: lsp_locations,
            },
        );
        Ok(())
    }

    /// Handle go-to-implementation response from LSP
    pub(crate) fn handle_implementation_response(
        &mut self,
        window_id: fresh_core::WindowId,
        request_id: u64,
        locations: Vec<lsp_types::Location>,
    ) -> AnyhowResult<()> {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return Ok(());
        };
        if window.pending_implementation_request != Some(request_id) {
            tracing::debug!("Ignoring stale implementation response: {}", request_id);
            return Ok(());
        }
        window.pending_implementation_request = None;
        if locations.is_empty() {
            window.status_message = Some(t!("lsp.no_implementation").to_string());
            return Ok(());
        }
        let translation = window.authority().path_translation.clone();
        let lsp_locations: Vec<crate::services::plugins::hooks::LspLocation> = locations
            .iter()
            .map(|location| {
                let wire = crate::app::types::LspUri::from_wire(location.uri.clone());
                let file = if location.uri.scheme().map(|scheme| scheme.as_str()) == Some("file") {
                    wire.to_host_path(translation.as_ref())
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| location.uri.path().as_str().to_string())
                } else {
                    location.uri.as_str().to_string()
                };
                crate::services::plugins::hooks::LspLocation {
                    file,
                    line: location.range.start.line + 1,
                    column: location.range.start.character + 1,
                }
            })
            .collect();
        let count = lsp_locations.len();
        let symbol = std::mem::take(&mut window.pending_implementation_symbol);
        window.status_message =
            Some(t!("lsp.found_implementations", count = count, symbol = &symbol).to_string());
        self.plugin_manager.read().unwrap().run_hook(
            "lsp_implementation",
            crate::services::plugins::hooks::HookArgs::LspImplementation {
                symbol: symbol.clone(),
                locations: lsp_locations,
            },
        );
        Ok(())
    }

    /// Apply LSP text edits to a buffer and return the number of changes made.
    /// Edits are sorted in reverse order and applied as a batch.
    pub(crate) fn apply_lsp_text_edits(
        &mut self,
        buffer_id: BufferId,
        edits: Vec<lsp_types::TextEdit>,
    ) -> AnyhowResult<usize> {
        self.apply_lsp_text_edits_in_window(self.active_window, buffer_id, edits)
    }

    pub(crate) fn apply_lsp_text_edits_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        buffer_id: BufferId,
        mut edits: Vec<lsp_types::TextEdit>,
    ) -> AnyhowResult<usize> {
        if edits.is_empty() {
            return Ok(0);
        }
        edits.sort_by(|a, b| {
            b.range
                .start
                .line
                .cmp(&a.range.start.line)
                .then(b.range.start.character.cmp(&a.range.start.character))
        });

        let window = self
            .windows
            .get_mut(&window_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Window not found"))?;
        let split_id = window
            .split_manager_mut()
            .expect("window must have a populated split layout")
            .splits_for_buffer(buffer_id)
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                window
                    .buffers
                    .splits()
                    .map(|(manager, _)| manager.active_split())
                    .expect("window must have a populated split layout")
            });
        let cursor_id = window
            .buffers
            .splits()
            .and_then(|(_, views)| views.get(&split_id))
            .map(|view| view.cursors.primary_id())
            .unwrap_or_else(|| window.active_cursors().primary_id());

        let state = window
            .buffers
            .get_mut(&buffer_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Buffer not found"))?;
        let mut batch_events = Vec::new();
        let mut changes = 0;
        for edit in edits {
            let start_line = edit.range.start.line as usize;
            let start_char = edit.range.start.character as usize;
            let end_line = edit.range.end.line as usize;
            let end_char = edit.range.end.character as usize;
            let start_pos = state.buffer.lsp_position_to_byte(start_line, start_char);
            let end_pos = state.buffer.lsp_position_to_byte(end_line, end_char);
            let buffer_len = state.buffer.len();
            let old_text = if start_pos < end_pos && end_pos <= buffer_len {
                state.get_text_range(start_pos, end_pos)
            } else {
                format!(
                    "<invalid range: start={}, end={}, buffer_len={}>",
                    start_pos, end_pos, buffer_len
                )
            };
            tracing::debug!(
                "Converting LSP range line {}:{}-{}:{} to bytes {}..{} (replacing {:?} with {:?})",
                start_line,
                start_char,
                end_line,
                end_char,
                start_pos,
                end_pos,
                old_text,
                edit.new_text
            );
            if start_pos < end_pos {
                batch_events.push(Event::Delete {
                    range: start_pos..end_pos,
                    deleted_text: state.get_text_range(start_pos, end_pos),
                    cursor_id,
                });
            }
            if !edit.new_text.is_empty() {
                batch_events.push(Event::Insert {
                    position: start_pos,
                    text: edit.new_text,
                    cursor_id,
                });
            }
            changes += 1;
        }
        if !batch_events.is_empty() {
            self.apply_events_to_buffer_as_bulk_edit_in_window(
                window_id,
                buffer_id,
                batch_events,
                "LSP Rename".to_string(),
            )?;
        }
        Ok(changes)
    }
    /// and we skip it to avoid corrupting the buffer.
    fn apply_text_document_edit_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        text_doc_edit: lsp_types::TextDocumentEdit,
    ) -> AnyhowResult<usize> {
        let uri = crate::app::types::LspUri::from_wire(text_doc_edit.text_document.uri);
        let translation = self
            .windows
            .get(&window_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Window not found"))?
            .authority()
            .path_translation
            .clone();
        let path = match super::lsp_uri_to_host_path(&uri, translation.as_ref()) {
            Ok(path) => path,
            Err(_) => return Ok(0),
        };

        if let Some(expected_version) = text_doc_edit.text_document.version {
            let window = self
                .windows
                .get(&window_id)
                .expect("source window checked above");
            let language = window
                .buffers
                .iter()
                .find(|(_, state)| state.buffer.file_path() == Some(path.as_path()))
                .map(|(_, state)| state.language.clone())
                .unwrap_or_else(|| window.active_state().language.clone());
            for server in window.lsp.get_handles(&language) {
                if let Some(current_version) = server.handle.document_version(&path) {
                    if i64::from(expected_version) != current_version {
                        tracing::warn!(
                            "Rejecting stale TextDocumentEdit for {:?}: server version {} != our version {}",
                            path,
                            expected_version,
                            current_version
                        );
                        return Ok(0);
                    }
                }
            }
        }

        let buffer_id = match self
            .windows
            .get_mut(&window_id)
            .expect("source window checked above")
            .open_file_no_focus(&path)
        {
            Ok(buffer_id) => buffer_id,
            Err(error) => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.status_message =
                        Some(t!("file.error_opening", error = error.to_string()).to_string());
                }
                return Ok(0);
            }
        };
        let edits = text_doc_edit
            .edits
            .into_iter()
            .map(|edit| match edit {
                lsp_types::OneOf::Left(edit) => edit,
                lsp_types::OneOf::Right(annotated) => annotated.text_edit,
            })
            .collect();
        self.apply_lsp_text_edits_in_window(window_id, buffer_id, edits)
    }
    /// Apply a resource operation (CreateFile, RenameFile, DeleteFile) from a workspace edit.
    fn apply_resource_operation_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        op: lsp_types::ResourceOp,
    ) -> AnyhowResult<()> {
        let translation = self
            .windows
            .get(&window_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Window not found"))?
            .authority()
            .path_translation
            .clone();
        let to_host = |uri: &lsp_types::Uri| -> std::path::PathBuf {
            crate::app::types::LspUri::from_wire(uri.clone())
                .to_host_path(translation.as_ref())
                .unwrap_or_else(|| std::path::PathBuf::from(uri.path().as_str()))
        };
        match op {
            lsp_types::ResourceOp::Create(create) => {
                let path = to_host(&create.uri);
                let overwrite = create
                    .options
                    .as_ref()
                    .and_then(|o| o.overwrite)
                    .unwrap_or(false);
                let ignore_if_exists = create
                    .options
                    .as_ref()
                    .and_then(|o| o.ignore_if_exists)
                    .unwrap_or(false);

                if path.exists() {
                    if ignore_if_exists {
                        tracing::debug!("CreateFile: {:?} already exists, ignoring", path);
                        return Ok(());
                    }
                    if !overwrite {
                        tracing::warn!("CreateFile: {:?} already exists and overwrite=false", path);
                        return Ok(());
                    }
                }

                // Create parent directories if needed
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, "")?;
                tracing::info!("CreateFile: created {:?}", path);

                if let Err(error) = self
                    .windows
                    .get_mut(&window_id)
                    .expect("source window checked above")
                    .open_file_no_focus(&path)
                {
                    tracing::warn!(
                        "CreateFile: failed to open created file {:?}: {}",
                        path,
                        error
                    );
                }
            }
            lsp_types::ResourceOp::Rename(rename) => {
                let old_path = to_host(&rename.old_uri);
                let new_path = to_host(&rename.new_uri);
                let overwrite = rename
                    .options
                    .as_ref()
                    .and_then(|o| o.overwrite)
                    .unwrap_or(false);
                let ignore_if_exists = rename
                    .options
                    .as_ref()
                    .and_then(|o| o.ignore_if_exists)
                    .unwrap_or(false);

                if new_path.exists() {
                    if ignore_if_exists {
                        tracing::debug!("RenameFile: {:?} already exists, ignoring", new_path);
                        return Ok(());
                    }
                    if !overwrite {
                        tracing::warn!(
                            "RenameFile: {:?} already exists and overwrite=false",
                            new_path
                        );
                        return Ok(());
                    }
                }

                // Create parent directories if needed
                if let Some(parent) = new_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&old_path, &new_path)?;
                tracing::info!("RenameFile: {:?} -> {:?}", old_path, new_path);
            }
            lsp_types::ResourceOp::Delete(delete) => {
                let path = to_host(&delete.uri);
                let recursive = delete
                    .options
                    .as_ref()
                    .and_then(|o| o.recursive)
                    .unwrap_or(false);
                let ignore_if_not_exists = delete
                    .options
                    .as_ref()
                    .and_then(|o| o.ignore_if_not_exists)
                    .unwrap_or(false);

                if !path.exists() {
                    if ignore_if_not_exists {
                        tracing::debug!("DeleteFile: {:?} does not exist, ignoring", path);
                        return Ok(());
                    }
                    tracing::warn!("DeleteFile: {:?} does not exist", path);
                    return Ok(());
                }

                if path.is_dir() && recursive {
                    std::fs::remove_dir_all(&path)?;
                } else if path.is_file() {
                    std::fs::remove_file(&path)?;
                }
                tracing::info!("DeleteFile: deleted {:?}", path);
            }
        }
        Ok(())
    }

    /// Apply an LSP WorkspaceEdit (used by rename, code actions, etc.).
    ///
    /// Returns the total number of text changes applied.
    pub(crate) fn apply_workspace_edit(
        &mut self,
        workspace_edit: lsp_types::WorkspaceEdit,
    ) -> AnyhowResult<usize> {
        self.apply_workspace_edit_in_window(self.active_window, workspace_edit)
    }

    pub(crate) fn apply_workspace_edit_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        workspace_edit: lsp_types::WorkspaceEdit,
    ) -> AnyhowResult<usize> {
        tracing::debug!(
            "Applying WorkspaceEdit: changes={:?}, document_changes={:?}",
            workspace_edit.changes.as_ref().map(|changes| changes.len()),
            workspace_edit
                .document_changes
                .as_ref()
                .map(|changes| match changes {
                    lsp_types::DocumentChanges::Edits(edits) => format!("{} edits", edits.len()),
                    lsp_types::DocumentChanges::Operations(operations) => {
                        format!("{} operations", operations.len())
                    }
                })
        );
        let translation = self
            .windows
            .get(&window_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Window not found"))?
            .authority()
            .path_translation
            .clone();
        let mut total_changes = 0;

        if let Some(changes) = workspace_edit.changes {
            for (uri, edits) in changes {
                let uri = crate::app::types::LspUri::from_wire(uri);
                let Ok(path) = super::lsp_uri_to_host_path(&uri, translation.as_ref()) else {
                    continue;
                };
                let buffer_id = match self
                    .windows
                    .get_mut(&window_id)
                    .expect("source window checked above")
                    .open_file_no_focus(&path)
                {
                    Ok(buffer_id) => buffer_id,
                    Err(error) => {
                        if let Some(window) = self.windows.get_mut(&window_id) {
                            window.status_message = Some(
                                t!("file.error_opening", error = error.to_string()).to_string(),
                            );
                        }
                        return Ok(0);
                    }
                };
                total_changes +=
                    self.apply_lsp_text_edits_in_window(window_id, buffer_id, edits)?;
            }
        }

        if let Some(document_changes) = workspace_edit.document_changes {
            match document_changes {
                lsp_types::DocumentChanges::Edits(edits) => {
                    for edit in edits {
                        total_changes +=
                            self.apply_text_document_edit_in_window(window_id, edit)?;
                    }
                }
                lsp_types::DocumentChanges::Operations(operations) => {
                    for operation in operations {
                        match operation {
                            lsp_types::DocumentChangeOperation::Edit(edit) => {
                                total_changes +=
                                    self.apply_text_document_edit_in_window(window_id, edit)?;
                            }
                            lsp_types::DocumentChangeOperation::Op(operation) => {
                                self.apply_resource_operation_in_window(window_id, operation)?;
                                total_changes += 1;
                            }
                        }
                    }
                }
            }
        }
        Ok(total_changes)
    }

    /// Handle rename response from LSP
    pub fn handle_rename_response(
        &mut self,
        window_id: fresh_core::WindowId,
        _request_id: u64,
        result: Result<lsp_types::WorkspaceEdit, String>,
    ) -> AnyhowResult<()> {
        match result {
            Ok(workspace_edit) => {
                let total_changes =
                    self.apply_workspace_edit_in_window(window_id, workspace_edit)?;
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.status_message =
                        Some(t!("lsp.renamed", count = total_changes).to_string());
                }
            }
            Err(error) => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.status_message =
                        if error.contains("content modified") || error.contains("-32801") {
                            Some(t!("lsp.rename_cancelled").to_string())
                        } else {
                            Some(t!("lsp.rename_failed", error = &error).to_string())
                        };
                }
            }
        }
        Ok(())
    }

    /// Apply events to a specific buffer using bulk edit optimization (O(n) vs O(n²))
    ///
    /// This is similar to `apply_events_as_bulk_edit` but works on a specific buffer
    /// (which may not be the active buffer) and handles LSP notifications correctly.
    pub(crate) fn apply_events_to_buffer_as_bulk_edit(
        &mut self,
        buffer_id: BufferId,
        events: Vec<Event>,
        description: String,
    ) -> AnyhowResult<()> {
        self.apply_events_to_buffer_as_bulk_edit_in_window(
            self.active_window,
            buffer_id,
            events,
            description,
        )
    }

    pub(crate) fn apply_events_to_buffer_as_bulk_edit_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        buffer_id: BufferId,
        events: Vec<Event>,
        description: String,
    ) -> AnyhowResult<()> {
        use crate::model::event::CursorId;

        if events.is_empty() {
            return Ok(());
        }
        let batch_for_lsp = Event::Batch {
            events: events.clone(),
            description: description.clone(),
        };
        let window = self
            .windows
            .get_mut(&window_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Window not found"))?;
        let original_active = window.active_buffer();
        window
            .split_manager_mut()
            .expect("window must have a populated split layout")
            .set_active_buffer_id(buffer_id);
        let lsp_changes = window.collect_lsp_changes(&batch_for_lsp);
        window
            .split_manager_mut()
            .expect("window must have a populated split layout")
            .set_active_buffer_id(original_active);

        let split_id_for_cursors = window
            .split_manager_mut()
            .expect("window must have a populated split layout")
            .splits_for_buffer(buffer_id)
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                window
                    .buffers
                    .splits()
                    .map(|(manager, _)| manager.active_split())
                    .expect("window must have a populated split layout")
            });
        let old_cursors: Vec<(CursorId, usize, Option<usize>)> = window
            .buffers
            .splits()
            .and_then(|(_, views)| views.get(&split_id_for_cursors))
            .and_then(|view| view.keyed_states.get(&buffer_id))
            .map(|buffer_view| {
                buffer_view
                    .cursors
                    .iter()
                    .map(|(id, cursor)| (id, cursor.position, cursor.anchor))
                    .collect()
            })
            .unwrap_or_default();

        let bulk_edit = window
            .buffers
            .with_buffer_and_view_states(buffer_id, |state, views| -> AnyhowResult<Event> {
                let old_snapshot = state.buffer.snapshot_buffer_state();
                let mut edits: Vec<(usize, usize, String)> = events
                    .iter()
                    .filter_map(|event| match event {
                        Event::Insert { position, text, .. } => Some((*position, 0, text.clone())),
                        Event::Delete { range, .. } => {
                            Some((range.start, range.len(), String::new()))
                        }
                        _ => None,
                    })
                    .collect();
                edits.sort_by_key(|edit| std::cmp::Reverse(edit.0));
                let edit_refs: Vec<(usize, usize, &str)> = edits
                    .iter()
                    .map(|(position, delete_len, text)| (*position, *delete_len, text.as_str()))
                    .collect();
                let displaced_markers = state.capture_displaced_markers_bulk(&edits);
                state.buffer.apply_bulk_edits(&edit_refs);

                let mut position_deltas: Vec<(usize, isize)> = edits
                    .iter()
                    .map(|(position, delete_len, text)| {
                        (*position, text.len() as isize - *delete_len as isize)
                    })
                    .collect();
                position_deltas.sort_by_key(|(position, _)| *position);
                let shift_for = |original_position: usize| -> isize {
                    position_deltas
                        .iter()
                        .take_while(|(position, _)| *position < original_position)
                        .map(|(_, delta)| *delta)
                        .sum()
                };
                let buffer_len = state.buffer.len();
                let new_cursors: Vec<(CursorId, usize, Option<usize>)> = old_cursors
                    .iter()
                    .map(|(id, position, anchor)| {
                        let new_position = ((*position as isize + shift_for(*position)).max(0)
                            as usize)
                            .min(buffer_len);
                        let new_anchor = anchor.map(|anchor| {
                            ((anchor as isize + shift_for(anchor)).max(0) as usize).min(buffer_len)
                        });
                        (*id, new_position, new_anchor)
                    })
                    .collect();
                let new_snapshot = state.buffer.snapshot_buffer_state();
                state.highlighter.invalidate_all();
                if let Some(buffer_view) = views
                    .get_mut(&split_id_for_cursors)
                    .and_then(|view| view.keyed_states.get_mut(&buffer_id))
                {
                    for (cursor_id, new_position, new_anchor) in &new_cursors {
                        if let Some(cursor) = buffer_view.cursors.get_mut(*cursor_id) {
                            cursor.position = *new_position;
                            cursor.anchor = *new_anchor;
                        }
                    }
                }

                let mut edit_lengths: Vec<(usize, usize, usize)> = Vec::new();
                for (position, delete_len, text) in &edits {
                    if let Some(last) = edit_lengths.last_mut() {
                        if last.0 == *position {
                            last.1 += delete_len;
                            last.2 += text.len();
                            continue;
                        }
                    }
                    edit_lengths.push((*position, *delete_len, text.len()));
                }
                for &(position, delete_len, insert_len) in &edit_lengths {
                    if delete_len > insert_len {
                        let count = delete_len - insert_len;
                        state.marker_list.adjust_for_delete(position, count);
                        state.margins.adjust_for_delete(position, count);
                        state.scrollbar_markers.adjust_for_delete(position, count);
                    } else if insert_len > delete_len {
                        let count = insert_len - delete_len;
                        state.marker_list.adjust_for_insert(position, count);
                        state.margins.adjust_for_insert(position, count);
                        state.scrollbar_markers.adjust_for_insert(position, count);
                    }
                }
                Ok(Event::BulkEdit {
                    old_snapshot: Some(old_snapshot),
                    new_snapshot: Some(new_snapshot),
                    old_cursors,
                    new_cursors,
                    description,
                    edits: edit_lengths,
                    displaced_markers,
                })
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Buffer not found"))??;
        if let Some(event_log) = window.event_logs.get_mut(&buffer_id) {
            event_log.append(bulk_edit);
        }
        window.send_lsp_changes_for_buffer(buffer_id, lsp_changes);
        Ok(())
    }

    /// Start rename mode - select the symbol at cursor and allow inline editing
    pub(crate) fn start_rename(&mut self) -> AnyhowResult<()> {
        // If server supports prepareRename, validate first
        if self.active_window().server_supports_prepare_rename() {
            self.active_window_mut().send_prepare_rename();
            return Ok(());
        }

        self.show_rename_prompt()
    }

    /// Handle prepareRename response — if valid, show rename prompt; if error, show message.
    pub(crate) fn handle_prepare_rename_response(
        &mut self,
        window_id: fresh_core::WindowId,
        result: Result<serde_json::Value, String>,
    ) {
        match result {
            Ok(value) if !value.is_null() => {
                if let Err(error) = self.show_rename_prompt_in_window(window_id) {
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        window.status_message = Some(format!("Rename failed: {error}"));
                    }
                }
            }
            Ok(_) => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.status_message = Some("Cannot rename at this position".to_string());
                }
            }
            Err(error) => {
                if let Some(window) = self.windows.get_mut(&window_id) {
                    window.status_message = Some(format!("Cannot rename: {error}"));
                }
            }
        }
    }

    fn show_rename_prompt(&mut self) -> AnyhowResult<()> {
        self.show_rename_prompt_in_window(self.active_window)
    }

    fn show_rename_prompt_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
    ) -> AnyhowResult<()> {
        use crate::primitives::word_navigation::{find_word_end, find_word_start};

        let window = self
            .windows
            .get_mut(&window_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Window not found"))?;
        let cursor_pos = window.active_cursors().primary().position;
        let word_start = find_word_start(&window.active_state().buffer, cursor_pos);
        let word_end = find_word_end(&window.active_state().buffer, cursor_pos);
        if word_start >= word_end {
            window.status_message = Some(t!("lsp.no_symbol_at_cursor").to_string());
            return Ok(());
        }
        let word_text = window
            .active_state_mut()
            .get_text_range(word_start, word_end);
        let overlay_handle = window.active_state_mut().add_overlay(
            None,
            word_start..word_end,
            crate::model::event::OverlayFace::Background {
                color: (50, 100, 200),
            },
            100,
            Some(t!("lsp.popup_renaming").to_string()),
            false,
            None,
        );
        let mut prompt = Prompt::new(
            "Rename to: ".to_string(),
            PromptType::LspRename {
                original_text: word_text.clone(),
                start_pos: word_start,
                end_pos: word_end,
                overlay_handle,
            },
        );
        prompt.set_input(word_text);
        window.prompt = Some(prompt);
        Ok(())
    }

    /// Cancel rename mode - removes overlay if the prompt was for LSP rename
    pub(crate) fn cancel_rename_overlay(&mut self, handle: &crate::view::overlay::OverlayHandle) {
        self.remove_overlay(handle.clone());
    }

    /// Perform the actual LSP rename request
    pub(crate) fn perform_lsp_rename(
        &mut self,
        new_name: String,
        original_text: String,
        start_pos: usize,
        overlay_handle: crate::view::overlay::OverlayHandle,
    ) {
        // Remove the overlay first
        self.cancel_rename_overlay(&overlay_handle);

        // Check if the name actually changed
        if new_name == original_text {
            self.active_window_mut().status_message = Some(t!("lsp.name_unchanged").to_string());
            return;
        }

        // Use the position from when we entered rename mode, NOT the current cursor position
        // This ensures we send the rename request for the correct symbol even if cursor moved
        let rename_pos = start_pos;

        // Convert byte position to LSP position (line, UTF-16 code units)
        // LSP uses UTF-16 code units for character offsets, not byte offsets
        let state = self.active_state();
        let (line, character) = state.buffer.position_to_lsp_position(rename_pos);
        let buffer_id = self.active_buffer();
        let request_id = self.active_window_mut().next_lsp_request_id;

        // Use helper to ensure didOpen is sent before the request
        let sent = self
            .with_lsp_for_buffer(buffer_id, LspFeature::Rename, |handle, uri, _language| {
                let result = handle.rename(
                    request_id,
                    uri.as_uri().clone(),
                    line as u32,
                    character as u32,
                    new_name.clone(),
                );
                if result.is_ok() {
                    tracing::info!(
                        "Requested rename at {}:{}:{} to '{}'",
                        uri.as_str(),
                        line,
                        character,
                        new_name
                    );
                }
                result.is_ok()
            })
            .unwrap_or(false);

        if sent {
            self.active_window_mut().next_lsp_request_id += 1;
        } else if self
            .active_window()
            .buffer_metadata
            .get(&buffer_id)
            .and_then(|m| m.file_path())
            .is_none()
        {
            self.active_window_mut().status_message =
                Some(t!("lsp.cannot_rename_unsaved").to_string());
        }
    }

    /// Request inlay hints for the active buffer (if enabled and LSP available)
    pub(crate) fn request_inlay_hints_for_active_buffer(&mut self) {
        let buffer_id = self.active_buffer();
        self.request_inlay_hints_for_buffer(buffer_id);
    }

    /// Request inlay hints for a specific buffer (if enabled and LSP available)
    pub(crate) fn request_inlay_hints_for_buffer(&mut self, buffer_id: BufferId) {
        self.request_inlay_hints_for_buffer_in_window(self.active_window, buffer_id);
    }

    pub(crate) fn request_inlay_hints_for_buffer_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
        buffer_id: BufferId,
    ) {
        if !self.config.editor.enable_inlay_hints {
            return;
        }
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        let Some(state) = window.buffers.get(&buffer_id) else {
            return;
        };
        let line_count = state.buffer.line_count().unwrap_or(1000);
        let version = state.buffer.version();
        let request_id = window.next_lsp_request_id;
        let last_line = line_count.saturating_sub(1) as u32;
        let sent = self
            .with_lsp_for_buffer_in_window(
                window_id,
                buffer_id,
                LspFeature::InlayHints,
                |handle, uri, _language| {
                    handle
                        .inlay_hints(request_id, uri.as_uri().clone(), 0, 0, last_line, 10000)
                        .is_ok()
                },
            )
            .unwrap_or(false);
        if sent {
            let window = self
                .windows
                .get_mut(&window_id)
                .expect("source window checked above");
            window.next_lsp_request_id += 1;
            window
                .pending_inlay_hints_requests
                .insert(request_id, super::InlayHintsRequest { buffer_id, version });
        }
    }

    /// If a per-edit inlay-hints debounce has fired, send a fresh
    /// `textDocument/inlayHint` request for the scheduled buffer. Mirrors
    /// [`Window::check_diagnostic_pull_timer`]: the edit path writes the
    /// debounce slot but nothing consumed it, so hints never refreshed after
    /// an edit (sinelaw/fresh#2744). The response arrives asynchronously and
    /// its handler is version-guarded, so no redraw is triggered here.
    pub(crate) fn check_inlay_hints_timer(&mut self) {
        let Some((buffer_id, trigger_time)) = self.active_window().scheduled_inlay_hints_request
        else {
            return;
        };

        if std::time::Instant::now() < trigger_time {
            return;
        }

        self.active_window_mut().scheduled_inlay_hints_request = None;

        self.request_inlay_hints_for_buffer(buffer_id);
    }

    /// Issue a debounced folding range request if the timer has elapsed.
    pub(crate) fn maybe_request_folding_ranges_debounced(&mut self, buffer_id: BufferId) {
        let Some(ready_at) = self
            .active_window()
            .folding_ranges_debounce
            .get(&buffer_id)
            .copied()
        else {
            return;
        };
        if Instant::now() < ready_at {
            return;
        }

        self.active_window_mut()
            .folding_ranges_debounce
            .remove(&buffer_id);
        self.request_folding_ranges_for_buffer(buffer_id);
    }

    /// Request folding ranges for a buffer if supported and needed.
    pub(crate) fn request_folding_ranges_for_buffer(&mut self, buffer_id: BufferId) {
        if self
            .active_window_mut()
            .folding_ranges_in_flight
            .contains_key(&buffer_id)
        {
            return;
        }

        let Some(metadata) = self.active_window().buffer_metadata.get(&buffer_id) else {
            return;
        };
        if !metadata.lsp_enabled {
            return;
        }
        let Some(uri) = metadata.file_uri().cloned() else {
            return;
        };
        let file_path = metadata.file_path().cloned();

        let Some(language) = self
            .windows
            .get(&self.active_window)
            .map(|w| &w.buffers)
            .expect("active window present")
            .get(&buffer_id)
            .map(|s| s.language.clone())
        else {
            return;
        };

        let __active_id = self.active_window;
        // Pre-collect buffer version so we don't re-read self.buffers
        // while the &mut lsp borrow is alive.
        let __buffer_version_for_request = self
            .windows
            .get(&__active_id)
            .and_then(|w| w.buffers.get(&buffer_id))
            .map(|s| s.buffer.version())
            .unwrap_or(0);

        let Some(__win) = self.windows.get_mut(&__active_id) else {
            return;
        };
        let __next_id = &mut __win.next_lsp_request_id;
        let __pending_folding = &mut __win.pending_folding_range_requests;
        let __folding_in_flight = &mut __win.folding_ranges_in_flight;
        let lsp = &mut __win.lsp;

        if !lsp.folding_ranges_supported(&language) {
            return;
        }

        // Ensure there is a running server
        use crate::services::lsp::manager::LspSpawnResult;
        if lsp.try_spawn(&language, file_path.as_deref()) != LspSpawnResult::Spawned {
            return;
        }

        let Some(sh) = lsp.handle_for_feature_mut(&language, LspFeature::FoldingRange) else {
            return;
        };
        let handle = &mut sh.handle;

        let request_id = {
            let id = *__next_id;
            *__next_id += 1;
            id
        };
        let buffer_version = __buffer_version_for_request;
        let _ = __folding_in_flight;

        match handle.folding_ranges(request_id, uri.as_uri().clone()) {
            Ok(()) => {
                __pending_folding.insert(
                    request_id,
                    super::FoldingRangeRequest {
                        buffer_id,
                        version: buffer_version,
                    },
                );
                __folding_in_flight.insert(buffer_id, (request_id, buffer_version));
            }
            Err(e) => {
                tracing::debug!("Failed to request folding ranges: {}", e);
            }
        }
    }

    /// Request semantic tokens for a specific buffer if supported and needed.
    pub(crate) fn maybe_request_semantic_tokens(&mut self, buffer_id: BufferId) {
        if !self.config.editor.enable_semantic_tokens_full {
            return;
        }

        // Avoid duplicate in-flight requests per buffer
        if self
            .active_window_mut()
            .semantic_tokens_in_flight
            .contains_key(&buffer_id)
        {
            return;
        }

        let Some(metadata) = self.active_window().buffer_metadata.get(&buffer_id) else {
            return;
        };
        if !metadata.lsp_enabled {
            return;
        }
        let Some(uri) = metadata.file_uri().cloned() else {
            return;
        };
        let file_path_for_spawn = metadata.file_path().cloned();
        // Get language from buffer state
        let Some(language) = self
            .windows
            .get(&self.active_window)
            .map(|w| &w.buffers)
            .expect("active window present")
            .get(&buffer_id)
            .map(|s| s.language.clone())
        else {
            return;
        };

        let __active_id = self.active_window;
        // Pre-extract buffer state info so we don't re-borrow self
        // while the &mut lsp borrow is alive.
        let Some((buffer_version, existing_version, previous_result_id)) = self
            .windows
            .get(&__active_id)
            .and_then(|w| w.buffers.get(&buffer_id))
            .map(|state| {
                (
                    state.buffer.version(),
                    state.semantic_tokens.as_ref().map(|s| s.version),
                    state
                        .semantic_tokens
                        .as_ref()
                        .and_then(|s| s.result_id.clone()),
                )
            })
        else {
            return;
        };
        if Some(buffer_version) == existing_version {
            return; // Already up to date
        }

        let Some(__win) = self.windows.get_mut(&__active_id) else {
            return;
        };
        let __next_id = &mut __win.next_lsp_request_id;
        let __pending_st = &mut __win.pending_semantic_token_requests;
        let __st_in_flight = &mut __win.semantic_tokens_in_flight;
        let lsp = &mut __win.lsp;

        // Ensure there is a running server
        use crate::services::lsp::manager::LspSpawnResult;
        if lsp.try_spawn(&language, file_path_for_spawn.as_deref()) != LspSpawnResult::Spawned {
            return;
        }

        // Check that a server actually supports full semantic tokens
        if !lsp.semantic_tokens_full_supported(&language) {
            return;
        }
        if lsp.semantic_tokens_legend(&language).is_none() {
            return;
        }

        let Some(sh) = lsp.handle_for_feature_mut(&language, LspFeature::SemanticTokens) else {
            return;
        };
        // Check capabilities on the specific server we'll send to
        let supports_delta = sh.capabilities.semantic_tokens_full_delta;
        let use_delta = previous_result_id.is_some() && supports_delta;
        let handle = &mut sh.handle;

        let request_id = {
            let id = *__next_id;
            *__next_id += 1;
            id
        };

        let request_kind = if use_delta {
            super::SemanticTokensFullRequestKind::FullDelta
        } else {
            super::SemanticTokensFullRequestKind::Full
        };

        let request_result = if use_delta {
            handle.semantic_tokens_full_delta(
                request_id,
                uri.as_uri().clone(),
                previous_result_id.unwrap(),
            )
        } else {
            handle.semantic_tokens_full(request_id, uri.as_uri().clone())
        };

        match request_result {
            Ok(_) => {
                __pending_st.insert(
                    request_id,
                    super::SemanticTokenFullRequest {
                        buffer_id,
                        version: buffer_version,
                        kind: request_kind,
                    },
                );
                __st_in_flight.insert(buffer_id, (request_id, buffer_version, request_kind));
            }
            Err(e) => {
                tracing::debug!("Failed to request semantic tokens: {}", e);
            }
        }
    }

    /// Issue a debounced full semantic token request if the timer has elapsed.
    pub(crate) fn maybe_request_semantic_tokens_full_debounced(&mut self, buffer_id: BufferId) {
        if !self.config.editor.enable_semantic_tokens_full {
            self.active_window_mut()
                .semantic_tokens_full_debounce
                .remove(&buffer_id);
            return;
        }

        let Some(ready_at) = self
            .active_window()
            .semantic_tokens_full_debounce
            .get(&buffer_id)
            .copied()
        else {
            return;
        };
        if Instant::now() < ready_at {
            return;
        }

        self.active_window_mut()
            .semantic_tokens_full_debounce
            .remove(&buffer_id);
        self.maybe_request_semantic_tokens(buffer_id);
    }

    /// Request semantic tokens for a viewport range (with padding).
    pub(crate) fn maybe_request_semantic_tokens_range(
        &mut self,
        buffer_id: BufferId,
        start_line: usize,
        end_line: usize,
    ) {
        let Some(metadata) = self.active_window().buffer_metadata.get(&buffer_id) else {
            return;
        };
        if !metadata.lsp_enabled {
            return;
        }
        let Some(uri) = metadata.file_uri().cloned() else {
            return;
        };
        let file_path = metadata.file_path().cloned();
        // Get language from buffer state
        let Some(language) = self
            .windows
            .get(&self.active_window)
            .map(|w| &w.buffers)
            .expect("active window present")
            .get(&buffer_id)
            .map(|s| s.language.clone())
        else {
            return;
        };

        let __active_id = self.active_window;
        // Single &mut on the active window — split-borrow into &mut lsp
        // and &buffers so we can use both concurrently.
        let __win = self
            .windows
            .get_mut(&__active_id)
            .expect("active window must exist");
        let __next_id = &mut __win.next_lsp_request_id;
        let __pending_st_range = &mut __win.pending_semantic_token_range_requests;
        let __st_range_in_flight = &mut __win.semantic_tokens_range_in_flight;
        let __st_range_last = &mut __win.semantic_tokens_range_last_request;
        let __st_range_applied = &__win.semantic_tokens_range_applied;
        let lsp = &mut __win.lsp;
        let __buffers_ref: &crate::app::window::WindowBuffers = &__win.buffers;

        // Ensure there is a running server
        use crate::services::lsp::manager::LspSpawnResult;
        if lsp.try_spawn(&language, file_path.as_deref()) != LspSpawnResult::Spawned {
            return;
        }

        if !lsp.semantic_tokens_range_supported(&language) {
            // Fall back to full document tokens if no server supports range.
            self.maybe_request_semantic_tokens(buffer_id);
            return;
        }
        if lsp.semantic_tokens_legend(&language).is_none() {
            return;
        }

        let Some(sh) = lsp.handle_for_feature_mut(&language, LspFeature::SemanticTokens) else {
            return;
        };
        // The handle_for_feature_mut check ensures has_capability(SemanticTokens) which is
        // full || range. Double-check this specific server supports range.
        if !sh.capabilities.semantic_tokens_range {
            return;
        }
        let handle = &mut sh.handle;
        let Some(state) = __buffers_ref.get(&buffer_id) else {
            return;
        };

        let buffer_version = state.buffer.version();
        let mut padded_start = start_line.saturating_sub(SEMANTIC_TOKENS_RANGE_PADDING_LINES);
        let mut padded_end = end_line.saturating_add(SEMANTIC_TOKENS_RANGE_PADDING_LINES);

        if let Some(line_count) = state.buffer.line_count() {
            if line_count == 0 {
                return;
            }
            let max_line = line_count.saturating_sub(1);
            padded_start = padded_start.min(max_line);
            padded_end = padded_end.min(max_line);
        }

        let start_byte = state.buffer.line_start_offset(padded_start).unwrap_or(0);
        let end_char = state
            .buffer
            .get_line(padded_end)
            .map(|line| String::from_utf8_lossy(&line).encode_utf16().count())
            .unwrap_or(0);
        let end_byte = if state.buffer.line_start_offset(padded_end).is_some() {
            state.buffer.lsp_position_to_byte(padded_end, end_char)
        } else {
            state.buffer.len()
        };

        if start_byte >= end_byte {
            return;
        }

        let range = start_byte..end_byte;
        if let Some((in_flight_id, in_flight_start, in_flight_end, in_flight_version)) =
            __st_range_in_flight.get(&buffer_id).copied()
        {
            if in_flight_start == padded_start
                && in_flight_end == padded_end
                && in_flight_version == buffer_version
            {
                return;
            }
            if let Err(e) = handle.cancel_request(in_flight_id) {
                tracing::debug!("Failed to cancel semantic token range request: {}", e);
            }
            __pending_st_range.remove(&in_flight_id);
            __st_range_in_flight.remove(&buffer_id);
        }

        if let Some((applied_start, applied_end, applied_version)) =
            __st_range_applied.get(&buffer_id).copied()
        {
            if applied_start == padded_start
                && applied_end == padded_end
                && applied_version == buffer_version
            {
                return;
            }
        }

        let now = Instant::now();
        if let Some((last_start, last_end, last_version, last_time)) =
            __st_range_last.get(&buffer_id).copied()
        {
            if last_start == padded_start
                && last_end == padded_end
                && last_version == buffer_version
                && now.duration_since(last_time)
                    < Duration::from_millis(SEMANTIC_TOKENS_RANGE_DEBOUNCE_MS)
            {
                return;
            }
        }

        let lsp_range = lsp_types::Range {
            start: lsp_types::Position {
                line: padded_start as u32,
                character: 0,
            },
            end: lsp_types::Position {
                line: padded_end as u32,
                character: end_char as u32,
            },
        };

        let request_id = {
            let id = *__next_id;
            *__next_id += 1;
            id
        };
        let _ = __st_range_applied;

        match handle.semantic_tokens_range(request_id, uri.as_uri().clone(), lsp_range) {
            Ok(_) => {
                __pending_st_range.insert(
                    request_id,
                    SemanticTokenRangeRequest {
                        buffer_id,
                        version: buffer_version,
                        range: range.clone(),
                        start_line: padded_start,
                        end_line: padded_end,
                    },
                );
                __st_range_in_flight.insert(
                    buffer_id,
                    (request_id, padded_start, padded_end, buffer_version),
                );
                __st_range_last.insert(buffer_id, (padded_start, padded_end, buffer_version, now));
            }
            Err(e) => {
                tracing::debug!("Failed to request semantic token range: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::filesystem::StdFileSystem;
    use std::sync::Arc;

    fn test_fs() -> Arc<dyn crate::model::filesystem::FileSystem + Send + Sync> {
        Arc::new(StdFileSystem)
    }
    use super::{lsp_range_contains, lsp_range_overlaps, Editor};

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> lsp_types::Range {
        lsp_types::Range {
            start: lsp_types::Position {
                line: sl,
                character: sc,
            },
            end: lsp_types::Position {
                line: el,
                character: ec,
            },
        }
    }

    #[test]
    fn test_lsp_range_contains_inclusive_start_exclusive_end() {
        let r = range(3, 10, 3, 20);
        // Before start
        assert!(!lsp_range_contains(&r, 3, 9));
        assert!(!lsp_range_contains(&r, 2, 50));
        // At start (inclusive)
        assert!(lsp_range_contains(&r, 3, 10));
        // Inside
        assert!(lsp_range_contains(&r, 3, 15));
        // Just before end (inclusive)
        assert!(lsp_range_contains(&r, 3, 19));
        // At end (exclusive)
        assert!(!lsp_range_contains(&r, 3, 20));
        // After end
        assert!(!lsp_range_contains(&r, 3, 21));
        assert!(!lsp_range_contains(&r, 4, 0));
    }

    #[test]
    fn test_lsp_range_contains_multiline() {
        let r = range(2, 5, 4, 3);
        // Line before start
        assert!(!lsp_range_contains(&r, 1, 100));
        // On start line, before start character
        assert!(!lsp_range_contains(&r, 2, 4));
        // On start line, at start character (inclusive)
        assert!(lsp_range_contains(&r, 2, 5));
        // Interior line — any character is inside.
        assert!(lsp_range_contains(&r, 3, 0));
        assert!(lsp_range_contains(&r, 3, 9999));
        // End line, before end character (inclusive)
        assert!(lsp_range_contains(&r, 4, 2));
        // End line, at end character (exclusive)
        assert!(!lsp_range_contains(&r, 4, 3));
        // Line after end
        assert!(!lsp_range_contains(&r, 5, 0));
    }

    #[test]
    fn test_lsp_range_contains_zero_length_matches_anchor_only() {
        // Point diagnostic: start == end.
        let r = range(7, 4, 7, 4);
        assert!(lsp_range_contains(&r, 7, 4));
        assert!(!lsp_range_contains(&r, 7, 3));
        assert!(!lsp_range_contains(&r, 7, 5));
        assert!(!lsp_range_contains(&r, 6, 4));
        assert!(!lsp_range_contains(&r, 8, 4));
    }

    #[test]
    fn test_lsp_range_overlaps_point_cursor_in_diagnostic_range() {
        // Diagnostic at line 3, cols 10..20. A zero-width cursor anywhere
        // inside (including at start) overlaps; just outside does not.
        let diag = range(3, 10, 3, 20);
        // Cursor on the start anchor.
        assert!(lsp_range_overlaps(&diag, 3, 10, 3, 10));
        // Cursor inside.
        assert!(lsp_range_overlaps(&diag, 3, 15, 3, 15));
        // Cursor at exclusive end — not contained.
        assert!(!lsp_range_overlaps(&diag, 3, 20, 3, 20));
        // Cursor before start.
        assert!(!lsp_range_overlaps(&diag, 3, 9, 3, 9));
        // Cursor on a different line.
        assert!(!lsp_range_overlaps(&diag, 4, 15, 4, 15));
    }

    #[test]
    fn test_lsp_range_overlaps_selection_intersects_diagnostic() {
        // Diagnostic at line 3, cols 10..20.
        let diag = range(3, 10, 3, 20);
        // Selection entirely inside diag.
        assert!(lsp_range_overlaps(&diag, 3, 12, 3, 18));
        // Selection straddling the start of diag.
        assert!(lsp_range_overlaps(&diag, 3, 5, 3, 15));
        // Selection straddling the end of diag.
        assert!(lsp_range_overlaps(&diag, 3, 15, 3, 25));
        // Selection entirely covering diag.
        assert!(lsp_range_overlaps(&diag, 3, 0, 3, 30));
        // Selection adjacent before (touches at start, half-open end).
        assert!(!lsp_range_overlaps(&diag, 3, 0, 3, 10));
        // Selection adjacent after (starts at exclusive end of diag).
        assert!(!lsp_range_overlaps(&diag, 3, 20, 3, 30));
        // Selection on the wrong line.
        assert!(!lsp_range_overlaps(&diag, 4, 0, 4, 100));
    }

    #[test]
    fn test_lsp_range_overlaps_point_diagnostic_within_selection() {
        // Point-style diagnostic at line 3, col 10.
        let diag = range(3, 10, 3, 10);
        // Selection covering the point.
        assert!(lsp_range_overlaps(&diag, 3, 5, 3, 15));
        // Selection starting at the point (half-open includes start).
        assert!(lsp_range_overlaps(&diag, 3, 10, 3, 15));
        // Selection ending at the point (half-open excludes end).
        assert!(!lsp_range_overlaps(&diag, 3, 0, 3, 10));
        // Zero-width cursor on the exact anchor.
        assert!(lsp_range_overlaps(&diag, 3, 10, 3, 10));
        // Zero-width cursor not on the anchor.
        assert!(!lsp_range_overlaps(&diag, 3, 9, 3, 9));
    }

    #[test]
    fn test_lsp_range_overlaps_multiline() {
        // Diagnostic spans line 2 col 5 through line 4 col 3.
        let diag = range(2, 5, 4, 3);
        // Cursor on an interior line, anywhere.
        assert!(lsp_range_overlaps(&diag, 3, 0, 3, 0));
        // Selection crossing line boundary into the diagnostic.
        assert!(lsp_range_overlaps(&diag, 1, 0, 2, 6));
        // Selection that starts at the exclusive end of the diagnostic — no overlap.
        assert!(!lsp_range_overlaps(&diag, 4, 3, 4, 10));
        // Selection entirely after the diagnostic.
        assert!(!lsp_range_overlaps(&diag, 5, 0, 5, 10));
    }

    use crate::model::buffer::Buffer;
    use crate::state::EditorState;
    use crate::view::virtual_text::VirtualTextPosition;
    use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position};

    fn make_hint(line: u32, character: u32, label: &str, kind: Option<InlayHintKind>) -> InlayHint {
        InlayHint {
            position: Position { line, character },
            label: InlayHintLabel::String(label.to_string()),
            kind,
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        }
    }

    #[test]
    fn test_inlay_hint_inserts_before_character() {
        let mut state = EditorState::new(
            80,
            24,
            crate::config::LARGE_FILE_THRESHOLD_BYTES as usize,
            test_fs(),
        );
        state.buffer = Buffer::from_str_test("ab");

        if !state.buffer.is_empty() {
            state.marker_list.adjust_for_insert(0, state.buffer.len());
        }

        let hints = vec![make_hint(0, 1, ": i32", Some(InlayHintKind::TYPE))];
        Editor::apply_inlay_hints_to_state(&mut state, &hints);

        let lookup = state
            .virtual_texts
            .build_lookup(&state.marker_list, 0, state.buffer.len());
        let vtexts = lookup.get(&1).expect("expected hint at byte offset 1");
        assert_eq!(vtexts.len(), 1);
        assert_eq!(vtexts[0].text, ": i32");
        assert_eq!(vtexts[0].position, VirtualTextPosition::BeforeChar);
    }

    #[test]
    fn test_inlay_hint_at_eof_renders_after_last_char() {
        let mut state = EditorState::new(
            80,
            24,
            crate::config::LARGE_FILE_THRESHOLD_BYTES as usize,
            test_fs(),
        );
        state.buffer = Buffer::from_str_test("ab");

        if !state.buffer.is_empty() {
            state.marker_list.adjust_for_insert(0, state.buffer.len());
        }

        let hints = vec![make_hint(0, 2, ": i32", Some(InlayHintKind::TYPE))];
        Editor::apply_inlay_hints_to_state(&mut state, &hints);

        let lookup = state
            .virtual_texts
            .build_lookup(&state.marker_list, 0, state.buffer.len());
        let vtexts = lookup.get(&1).expect("expected hint anchored to last byte");
        assert_eq!(vtexts.len(), 1);
        assert_eq!(vtexts[0].text, ": i32");
        assert_eq!(vtexts[0].position, VirtualTextPosition::AfterChar);
    }

    #[test]
    fn test_inlay_hint_empty_buffer_is_ignored() {
        let mut state = EditorState::new(
            80,
            24,
            crate::config::LARGE_FILE_THRESHOLD_BYTES as usize,
            test_fs(),
        );
        state.buffer = Buffer::from_str_test("");

        let hints = vec![make_hint(0, 0, ": i32", Some(InlayHintKind::TYPE))];
        Editor::apply_inlay_hints_to_state(&mut state, &hints);

        assert!(state.virtual_texts.is_empty());
    }

    #[test]
    fn test_inlay_hint_uses_theme_key_for_foreground() {
        // Verify that apply_inlay_hints_to_state stores the theme key so
        // hints follow the active theme rather than a hardcoded color.
        let mut state = EditorState::new(
            80,
            24,
            crate::config::LARGE_FILE_THRESHOLD_BYTES as usize,
            test_fs(),
        );
        state.buffer = Buffer::from_str_test("ab");

        if !state.buffer.is_empty() {
            state.marker_list.adjust_for_insert(0, state.buffer.len());
        }

        let hints = vec![make_hint(0, 1, ": i32", Some(InlayHintKind::TYPE))];
        Editor::apply_inlay_hints_to_state(&mut state, &hints);

        let lookup = state
            .virtual_texts
            .build_lookup(&state.marker_list, 0, state.buffer.len());
        let vtexts = lookup.get(&1).expect("expected hint at byte offset 1");
        assert_eq!(
            vtexts[0].fg_theme_key.as_deref(),
            Some("editor.line_number_fg")
        );
        assert_eq!(vtexts[0].bg_theme_key, None);
    }

    #[test]
    fn test_inlay_hint_removed_when_its_range_is_deleted() {
        // Regression: deleting a range that covers the anchor byte of an
        // inlay hint used to leave the hint visible (the marker snapped to
        // the deletion start). apply_delete now calls
        // virtual_texts.remove_in_range before adjusting markers, so the
        // hint vanishes immediately. A future LSP refresh can repopulate
        // hints elsewhere.
        let mut state = EditorState::new(
            80,
            24,
            crate::config::LARGE_FILE_THRESHOLD_BYTES as usize,
            test_fs(),
        );
        state.buffer = Buffer::from_str_test("let x = 42;");
        state.marker_list.adjust_for_insert(0, state.buffer.len());

        // Hint anchored at byte 5 (after "let x" -> rendered before '=').
        let hints = vec![make_hint(0, 5, ": i32", Some(InlayHintKind::TYPE))];
        Editor::apply_inlay_hints_to_state(&mut state, &hints);
        assert_eq!(state.virtual_texts.len(), 1);

        // Simulate user deleting "x = 42" (bytes 4..10, half-open) — the
        // hint anchor at byte 5 is inside this range.
        let removed = state
            .virtual_texts
            .remove_in_range(&mut state.marker_list, 4, 10);
        assert_eq!(removed, 1, "hint inside deleted range must be removed");
        assert!(state.virtual_texts.is_empty());
    }

    #[test]
    fn test_marker_delete_after_repeat_clear_recreate() {
        // Regression: simulates what apply_inlay_hints_to_state does on
        // every LSP refresh — clear every virtual_text's marker then
        // recreate markers at fresh positions. After a few rounds,
        // delete one marker and adjust for a deletion and check the
        // remaining markers' positions.
        use crate::model::marker::MarkerList;
        use crate::view::virtual_text::{VirtualTextManager, VirtualTextPosition};
        use ratatui::style::Style;

        let mut markers = MarkerList::new();
        let mut vtexts = VirtualTextManager::new();

        // Initial marker layout at six positions (same as the e2e test).
        let positions = [200usize, 401, 602, 803, 1205, 1406];
        for &p in &positions {
            vtexts.add(
                &mut markers,
                p,
                format!("hint-at-{p}"),
                Style::default(),
                VirtualTextPosition::BeforeChar,
                0,
            );
        }

        // Simulate a couple of clear/recreate cycles (each LSP refresh
        // goes through this exact path via apply_inlay_hints_to_state).
        for _ in 0..3 {
            vtexts.clear(&mut markers);
            for &p in &positions {
                vtexts.add(
                    &mut markers,
                    p,
                    format!("hint-at-{p}"),
                    Style::default(),
                    VirtualTextPosition::BeforeChar,
                    0,
                );
            }
        }

        // remove_in_range + adjust_for_delete equivalent to apply_delete.
        let removed = vtexts.remove_in_range(&mut markers, 1005, 1206);
        assert_eq!(
            removed, 1,
            "exactly one marker inside [1005, 1206) should be removed"
        );
        markers.adjust_for_delete(1005, 201);

        let lookup = vtexts.build_lookup(&markers, 0, 10_000);
        let mut positions: Vec<usize> = lookup.keys().copied().collect();
        positions.sort();
        assert_eq!(
            positions,
            vec![200, 401, 602, 803, 1205],
            "after delete+adjust, expected marker byte positions {:?}, got {:?}",
            vec![200, 401, 602, 803, 1205],
            positions
        );
    }

    #[test]
    fn test_marker_delete_then_adjust_preserves_last_marker_position() {
        // Regression for the user-observed flip of an end-of-line inlay
        // hint to the start of its line after a nearby line is deleted.
        //
        // Scenario (real numbers from the failing e2e test): six markers
        // at byte offsets that correspond to the `\n` of each line,
        // then delete-one-marker (simulating remove_in_range on the
        // line being deleted) followed by adjust_for_delete on the
        // remaining markers.
        //
        // The last marker (at byte 1406) should end up at byte 1205
        // after subtracting the 201-byte deleted range. Observed bug:
        // it ends up at byte 1005 (the deletion start) — exactly as
        // though the delta were applied twice.
        use crate::model::marker::MarkerList;

        let mut markers = MarkerList::new();
        let m0 = markers.create(200, false);
        let m1 = markers.create(401, false);
        let m2 = markers.create(602, false);
        let m3 = markers.create(803, false);
        let m5 = markers.create(1205, false);
        let m6 = markers.create(1406, false);

        // Simulate remove_in_range removing marker m5 inside [1005, 1206).
        markers.delete(m5);

        // Now simulate adjust_for_delete over that range.
        markers.adjust_for_delete(1005, 201);

        assert_eq!(markers.get_position(m0), Some(200), "m0 unchanged");
        assert_eq!(markers.get_position(m1), Some(401), "m1 unchanged");
        assert_eq!(markers.get_position(m2), Some(602), "m2 unchanged");
        assert_eq!(markers.get_position(m3), Some(803), "m3 unchanged");
        assert_eq!(
            markers.get_position(m6),
            Some(1205),
            "m6 must shift from 1406 to 1205 (1406 - 201), not be clamped to delete-start 1005"
        );
    }

    #[test]
    fn test_inlay_hint_outside_deletion_survives() {
        // Anchors outside the deleted range must not be collateral damage.
        let mut state = EditorState::new(
            80,
            24,
            crate::config::LARGE_FILE_THRESHOLD_BYTES as usize,
            test_fs(),
        );
        state.buffer = Buffer::from_str_test("let x = 42; let y = 0;");
        state.marker_list.adjust_for_insert(0, state.buffer.len());

        let hints = vec![
            make_hint(0, 5, ": i32", Some(InlayHintKind::TYPE)), // byte 5 - inside deletion
            make_hint(0, 17, ": i32", Some(InlayHintKind::TYPE)), // byte 17 - outside
        ];
        Editor::apply_inlay_hints_to_state(&mut state, &hints);
        assert_eq!(state.virtual_texts.len(), 2);

        let removed = state
            .virtual_texts
            .remove_in_range(&mut state.marker_list, 4, 10);
        assert_eq!(removed, 1);
        assert_eq!(state.virtual_texts.len(), 1);
    }

    #[test]
    fn test_space_doc_paragraphs_inserts_blank_lines() {
        use super::space_doc_paragraphs;

        // Single newlines become double newlines
        let input = "sep\n  description.\nend\n  another.";
        let result = space_doc_paragraphs(input);
        assert_eq!(result, "sep\n\n  description.\n\nend\n\n  another.");
    }

    #[test]
    fn test_space_doc_paragraphs_preserves_existing_blank_lines() {
        use super::space_doc_paragraphs;

        // Already-double newlines stay double (not quadrupled)
        let input = "First paragraph.\n\nSecond paragraph.";
        let result = space_doc_paragraphs(input);
        assert_eq!(result, "First paragraph.\n\nSecond paragraph.");
    }

    #[test]
    fn test_space_doc_paragraphs_plain_text() {
        use super::space_doc_paragraphs;

        let input = "Just a single line of docs.";
        let result = space_doc_paragraphs(input);
        assert_eq!(result, "Just a single line of docs.");
    }

    fn timer_test_editor() -> Editor {
        use crate::config::Config;
        use crate::config_io::DirectoryContext;
        let temp = tempfile::tempdir().unwrap();
        let dir_context = DirectoryContext::for_testing(temp.path());
        // Keep the temp dir alive for the editor's lifetime.
        std::mem::forget(temp);
        let mut config = Config::default();
        config.editor.enable_inlay_hints = true;
        Editor::for_test(
            config,
            80,
            24,
            None,
            dir_context,
            crate::view::color_support::ColorCapability::TrueColor,
            Arc::new(StdFileSystem),
            None,
            None,
            false,
            false,
        )
        .unwrap()
    }

    #[test]
    fn test_check_inlay_hints_timer_fires_and_clears_slot_after_deadline() {
        // With the debounce slot's deadline already in the past, the consumer
        // must clear the slot (the request is dispatched, but arrives async).
        let mut editor = timer_test_editor();
        let buffer_id = editor.active_buffer();
        editor.active_window_mut().scheduled_inlay_hints_request = Some((
            buffer_id,
            std::time::Instant::now() - std::time::Duration::from_millis(1),
        ));

        editor.check_inlay_hints_timer();

        assert!(
            editor
                .active_window()
                .scheduled_inlay_hints_request
                .is_none(),
            "consumer should clear the debounce slot after the deadline passes"
        );
    }

    #[test]
    fn test_check_inlay_hints_timer_noop_before_deadline() {
        // A future deadline must be left untouched so the debounce actually
        // debounces instead of firing on the first idle tick.
        let mut editor = timer_test_editor();
        let buffer_id = editor.active_buffer();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        editor.active_window_mut().scheduled_inlay_hints_request = Some((buffer_id, deadline));

        editor.check_inlay_hints_timer();

        assert_eq!(
            editor.active_window().scheduled_inlay_hints_request,
            Some((buffer_id, deadline)),
            "consumer must be a no-op while the deadline is still in the future"
        );
    }
}
