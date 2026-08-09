//! Dabbrev expand action: Emacs-style sequential cycling completion.
//!
//! Unlike popup-based completion, dabbrev directly inserts the best match
//! and cycles through alternatives on repeated Alt+/ presses. The session
//! resets when any other action is taken (typing, moving, etc.).

use super::{DabbrevCycleState, Editor};
use crate::model::event::Event;
use crate::services::completion::dabbrev::DabbrevProvider;
use crate::services::completion::provider::{
    CompletionContext, CompletionProvider, OtherBufferSlice, ProviderResult,
};

/// Scan radius for other-buffer slices during dabbrev.
const OTHER_BUFFER_SCAN_RADIUS: usize = 64 * 1024; // 64 KB

impl Editor {
    /// Handle the DabbrevExpand action (Alt+/).
    ///
    /// First invocation: compute candidates from the active buffer (and
    /// other open buffers), insert the top match. Subsequent invocations:
    /// undo the previous insertion and insert the next candidate.
    pub(crate) fn dabbrev_expand(&mut self) {
        if self.active_window().dabbrev_state.is_some() {
            self.dabbrev_cycle();
        } else {
            self.dabbrev_expand_first();
        }
    }

    /// Cycle to the next dabbrev candidate.
    fn dabbrev_cycle(&mut self) {
        // Take state temporarily to satisfy borrow checker.
        let mut state = match self.active_window_mut().dabbrev_state.take() {
            Some(s) => s,
            None => return,
        };

        let cursor_id = self.active_cursors().primary_id();
        let cursor_pos = self.active_cursors().primary().position;
        let word_start = state.word_start;

        // Delete the previously inserted text.
        let prev_text = &state.candidates[state.index];
        let prev_end = word_start + prev_text.len();
        if cursor_pos == prev_end && prev_end <= self.active_state().buffer.len() {
            let deleted_text = self.active_state_mut().get_text_range(word_start, prev_end);
            let delete_event = Event::Delete {
                range: word_start..prev_end,
                deleted_text,
                cursor_id,
            };
            self.log_and_apply_event(&delete_event);
        }

        // Advance index. Wrap → restore original prefix and end session.
        state.index += 1;
        if state.index >= state.candidates.len() {
            // Restore the original prefix the user typed.
            let insert_event = Event::Insert {
                position: word_start,
                text: state.original_prefix.clone(),
                cursor_id,
            };
            self.log_and_apply_event(&insert_event);
            // Session over — don't re-store state.
        } else {
            // Insert the next candidate.
            let next = state.candidates[state.index].clone();
            let insert_event = Event::Insert {
                position: word_start,
                text: next,
                cursor_id,
            };
            self.log_and_apply_event(&insert_event);
            self.active_window_mut().dabbrev_state = Some(state);
        }
    }

    /// First Alt+/ press: scan buffers and insert the best match.
    fn dabbrev_expand_first(&mut self) {
        use crate::primitives::word_navigation::find_completion_word_start;

        let cursor_id = self.active_cursors().primary_id();
        let cursor_pos = self.active_cursors().primary().position;
        let word_start = find_completion_word_start(&self.active_state().buffer, cursor_pos);

        if word_start >= cursor_pos {
            return; // No prefix typed
        }

        let prefix = self
            .active_state_mut()
            .get_text_range(word_start, cursor_pos);
        if prefix.is_empty() {
            return;
        }

        let buffer_len = self.active_state().buffer.len();
        let is_large = self.active_state().buffer.is_large_file();
        let scan_range = CompletionContext::compute_scan_range(cursor_pos, buffer_len, is_large);
        let buffer_window = self.active_state().buffer.slice_bytes(scan_range.clone());

        // Get language-specific word chars from resolved config.
        let word_chars_extra = self.active_state().buffer_settings.word_characters.clone();

        // Build other-buffer slices (MRU order).
        let active_buf_id = self.active_buffer();
        let other_buffers = self.collect_other_buffer_slices(active_buf_id);

        let prefix_has_upper = prefix.chars().any(|c| c.is_uppercase());

        let ctx = CompletionContext {
            prefix: prefix.clone(),
            cursor_byte: cursor_pos,
            word_start_byte: word_start,
            buffer_len,
            is_large_file: is_large,
            scan_range,
            viewport_top_byte: 0,
            viewport_bottom_byte: buffer_len.min(512 * 1024),
            language_id: None,
            word_chars_extra,
            prefix_has_uppercase: prefix_has_upper,
            other_buffers,
        };

        let provider = DabbrevProvider::new();
        let result = provider.provide(&ctx, &buffer_window);

        let candidates: Vec<String> = match result {
            ProviderResult::Ready(c) => c.into_iter().map(|c| c.label).collect(),
            _ => return,
        };

        if candidates.is_empty() {
            return;
        }

        // Delete the prefix and insert the first candidate.
        let deleted_text = prefix.clone();
        let delete_event = Event::Delete {
            range: word_start..cursor_pos,
            deleted_text,
            cursor_id,
        };
        self.log_and_apply_event(&delete_event);

        let first = candidates[0].clone();
        let insert_event = Event::Insert {
            position: word_start,
            text: first,
            cursor_id,
        };
        self.log_and_apply_event(&insert_event);

        self.active_window_mut().dabbrev_state = Some(DabbrevCycleState {
            original_prefix: prefix,
            word_start,
            candidates,
            index: 0,
        });
    }

    /// Collect small byte-windows from other open buffers for multi-buffer scanning.
    pub(crate) fn collect_other_buffer_slices(
        &self,
        exclude_buffer_id: crate::model::event::BufferId,
    ) -> Vec<OtherBufferSlice> {
        self.collect_other_buffer_slices_in_window(self.active_window, exclude_buffer_id)
    }

    pub(crate) fn collect_other_buffer_slices_in_window(
        &self,
        window_id: fresh_core::WindowId,
        exclude_buffer_id: crate::model::event::BufferId,
    ) -> Vec<OtherBufferSlice> {
        let Some(window) = self.windows.get(&window_id) else {
            return Vec::new();
        };
        let mut slices = Vec::new();
        for (&buffer_id, state) in &window.buffers {
            if buffer_id == exclude_buffer_id {
                continue;
            }
            let buffer = &state.buffer;
            let buffer_len = buffer.len();
            if buffer_len == 0 {
                continue;
            }
            let radius = OTHER_BUFFER_SCAN_RADIUS;
            let middle = buffer_len / 2;
            let start = middle.saturating_sub(radius);
            let end = (middle + radius).min(buffer_len);
            let label = buffer
                .file_path()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "untitled".to_string());
            slices.push(OtherBufferSlice {
                buffer_id: buffer_id.0 as u64,
                bytes: buffer.slice_bytes(start..end),
                label,
            });
        }
        slices
    }

    /// Reset the dabbrev cycling session. Called when any non-dabbrev action
    /// is taken (typing, moving cursor, etc.).
    pub(crate) fn reset_dabbrev_state(&mut self) {
        self.active_window_mut().dabbrev_state = None;
    }

    /// Run the `CompletionService` (buffer-words + dabbrev providers) and
    /// return results as `PopupListItemData` items suitable for the
    /// completion popup. The items use icon `"w"` to visually distinguish
    /// them from LSP results.
    ///
    /// Returns an empty vec if the prefix is empty or no candidates match.
    pub(crate) fn get_buffer_completion_popup_items(
        &mut self,
    ) -> Vec<crate::model::event::PopupListItemData> {
        self.get_buffer_completion_popup_items_in_window(self.active_window)
    }

    pub(crate) fn get_buffer_completion_popup_items_in_window(
        &mut self,
        window_id: fresh_core::WindowId,
    ) -> Vec<crate::model::event::PopupListItemData> {
        use crate::model::event::PopupListItemData;
        use crate::primitives::word_navigation::find_completion_word_start;

        let Some(window) = self.windows.get_mut(&window_id) else {
            return Vec::new();
        };
        let buffer_id = window.active_buffer();
        let cursor_pos = window.active_cursors().primary().position;
        let (word_start, prefix, buffer_len, is_large, scan_range, buffer_window, word_chars_extra) = {
            let state = window.active_state_mut();
            let word_start = find_completion_word_start(&state.buffer, cursor_pos);
            if word_start >= cursor_pos {
                return Vec::new();
            }
            let prefix = state.get_text_range(word_start, cursor_pos);
            if prefix.is_empty() {
                return Vec::new();
            }
            let buffer_len = state.buffer.len();
            let is_large = state.buffer.is_large_file();
            let scan_range =
                CompletionContext::compute_scan_range(cursor_pos, buffer_len, is_large);
            let buffer_window = state.buffer.slice_bytes(scan_range.clone());
            (
                word_start,
                prefix,
                buffer_len,
                is_large,
                scan_range,
                buffer_window,
                state.buffer_settings.word_characters.clone(),
            )
        };
        let split_id = window
            .buffers
            .splits()
            .map(|(manager, _)| manager.active_split())
            .expect("window must have a populated split layout");
        let viewport_top_byte = window
            .buffers
            .splits()
            .and_then(|(_, views)| views.get(&split_id))
            .map(|view| view.viewport.top_byte())
            .unwrap_or(0);
        let viewport_bottom_byte = (viewport_top_byte + 8192).min(buffer_len);
        let prefix_has_uppercase = prefix.chars().any(|character| character.is_uppercase());
        let other_buffers = self.collect_other_buffer_slices_in_window(window_id, buffer_id);
        let context = CompletionContext {
            prefix,
            cursor_byte: cursor_pos,
            word_start_byte: word_start,
            buffer_len,
            is_large_file: is_large,
            scan_range,
            viewport_top_byte,
            viewport_bottom_byte,
            language_id: None,
            word_chars_extra,
            prefix_has_uppercase,
            other_buffers,
        };

        let candidates = self
            .windows
            .get_mut(&window_id)
            .expect("source window checked above")
            .completion_service
            .request(&context, &buffer_window);
        candidates
            .into_iter()
            .map(|candidate| PopupListItemData {
                text: candidate.label.clone(),
                detail: candidate.detail.clone(),
                icon: Some("w".to_string()),
                data: candidate.insert_text.or(Some(candidate.label)),
            })
            .collect()
    }
}
