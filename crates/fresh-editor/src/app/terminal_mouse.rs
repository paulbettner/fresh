//! Terminal mouse event handling.
//!
//! This module handles forwarding mouse events to the terminal PTY when the terminal
//! is in alternate screen mode (used by programs like vim, less, htop, etc.).
//!
//! When in alternate screen mode, mouse events that fall within the terminal's content
//! area are converted to terminal escape sequences and sent to the PTY, allowing
//! full-screen terminal programs to receive and handle mouse input.

use crate::app::window::Window;
use crate::input::handler::{TerminalMouseButton, TerminalMouseEventKind};
use crate::model::event::BufferId;
use anyhow::Result as AnyhowResult;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
pub(crate) enum CapturedMouseRoute {
    Fresh,
    Terminal(AnyhowResult<bool>),
}

fn captured_button_event(kind: MouseEventKind) -> Option<(MouseButton, bool)> {
    match kind {
        MouseEventKind::Drag(button) => Some((button, false)),
        MouseEventKind::Up(button) => Some((button, true)),
        _ => None,
    }
}

fn clamped_rect_offset(rect: Rect, col: u16, row: u16) -> Option<(u16, u16)> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    Some((
        col.saturating_sub(rect.x).min(rect.width - 1),
        row.saturating_sub(rect.y).min(rect.height - 1),
    ))
}

fn terminal_grid_cell_end_in_buffer(buffer: &crate::model::buffer::Buffer, pos: usize) -> usize {
    let (line, _) = buffer.position_to_line_col(pos);
    // Terminal capture writes LF line endings. The next line start therefore
    // locates this line's stored newline without copying a possibly enormous
    // logical line on every drag event.
    let content_end = buffer
        .line_start_offset(line + 1)
        .map(|next_start| next_start.saturating_sub(1))
        .unwrap_or_else(|| buffer.len());
    if pos >= content_end {
        content_end
    } else {
        buffer.next_grapheme_boundary(pos).min(content_end)
    }
}

impl Window {
    /// Route a drag/release through the owner captured by its matching press.
    /// PTY-owned continuations bypass current modifiers, overlays, and pointer
    /// hit-testing; Fresh-owned continuations are explicitly withheld from the
    /// PTY. A release consumes the capture.
    pub(crate) fn route_captured_mouse_event(
        &mut self,
        mouse_event: &MouseEvent,
    ) -> Option<CapturedMouseRoute> {
        let (button, release) = captured_button_event(mouse_event.kind)?;
        let owner = self.mouse_state.mouse_gesture_owner(button)?;

        match owner {
            super::types::MouseGestureOwner::Fresh => {
                if release {
                    self.mouse_state.finish_mouse_gesture(button);
                }
                Some(CapturedMouseRoute::Fresh)
            }
            super::types::MouseGestureOwner::Terminal {
                split_id,
                terminal_id,
                content_rect,
            } => {
                let current_rect = self.terminal_content_area_for_capture(split_id, terminal_id);
                if release || current_rect.is_none() {
                    self.mouse_state.finish_mouse_gesture(button);
                }
                if let Some(current_rect) = current_rect {
                    return Some(CapturedMouseRoute::Terminal(
                        self.forward_mouse_to_terminal(
                            terminal_id,
                            mouse_event.column,
                            mouse_event.row,
                            current_rect,
                            *mouse_event,
                        ),
                    ));
                }

                let result = clamped_rect_offset(content_rect, mouse_event.column, mouse_event.row)
                    .map(|(col, row)| {
                        self.send_terminal_mouse_to(
                            terminal_id,
                            col,
                            row,
                            TerminalMouseEventKind::Up(convert_button(button)),
                            crossterm::event::KeyModifiers::NONE,
                        );
                        true
                    })
                    .unwrap_or(false);
                Some(CapturedMouseRoute::Terminal(Ok(result)))
            }
        }
    }

    /// Check if mouse event should be forwarded to the terminal.
    /// Returns true if the event was forwarded (and handled).
    /// `forwarding` is the configured `terminal.mouse_forwarding` policy.
    pub(crate) fn try_forward_mouse_to_terminal(
        &mut self,
        col: u16,
        row: u16,
        mouse_event: MouseEvent,
        forwarding: crate::config::TerminalMouseForwarding,
    ) -> Option<AnyhowResult<bool>> {
        // Drag/release reports are valid only when a matching press captured
        // this PTY. `Editor::handle_mouse` routes those before overlays; an
        // orphan continuation must never start a partial PTY gesture.
        if captured_button_event(mouse_event.kind).is_some() {
            return None;
        }

        // Only forward if the focused split is a live terminal.
        if !self.focused_terminal_live() {
            return None;
        }

        // Ctrl reserves the Left-button / motion gesture for Fresh's own
        // file-link hover + open (the xterm/VS Code convention: Ctrl/Cmd+Click
        // opens links, overriding the inner program's mouse capture). Never
        // forward those to the PTY — otherwise a program that enabled mouse
        // reporting (DECSET 1000/1002/1003) or is in the alternate screen would
        // swallow the click before `try_open_terminal_link` /
        // `update_terminal_link_hover` (mouse_input.rs) ever see it, and links
        // would only work at a bare shell prompt. Non-Ctrl events still forward
        // normally, so the program keeps its ordinary mouse behaviour.
        if mouse_event
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
            && matches!(
                mouse_event.kind,
                MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::Moved
            )
        {
            return None;
        }

        let (split_id, buffer_id, content_rect) =
            self.get_terminal_content_area_at_position(col, row)?;
        let terminal_id = self.get_terminal_id(buffer_id)?;
        // A new pointer-hit press belongs only to the focused terminal. If the
        // pointer is over another terminal pane, forwarding would inject its
        // coordinates into the wrong child's stdin; only an already-captured
        // continuation may bypass this focus check.
        if buffer_id != self.active_buffer() {
            return None;
        }

        let forward = match forwarding {
            // Reserve every mouse event for Fresh's own scrollback and
            // selection handling, even when the child requested mouse input.
            crate::config::TerminalMouseForwarding::Never => false,
            // Legacy rule: forward every event to any alternate-screen
            // program, whether or not it asked for the mouse.
            crate::config::TerminalMouseForwarding::AltScreen => {
                self.is_terminal_in_alternate_screen(buffer_id)
            }
            // Button/motion events are forwarded only when the inner program
            // actually subscribed to the mouse (DECSET 1000/1002/1003) —
            // writing mouse escape sequences into a program that never
            // enabled reporting just injects garbage into its stdin. Shift is
            // the universal escape hatch (xterm convention): a shifted
            // press/drag is never forwarded, so text can always be
            // drag-selected even under a mouse-hungry program. Wheel events
            // additionally keep the alternate-screen rule: alternate-scroll
            // mode (arrow-key synthesis for pagers like `less`) lives in
            // `forward_mouse_to_terminal` and must keep seeing them.
            crate::config::TerminalMouseForwarding::Requested => {
                let wants_mouse = self.terminal_wants_mouse(buffer_id);
                let is_scroll = matches!(
                    mouse_event.kind,
                    MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                        | MouseEventKind::ScrollLeft
                        | MouseEventKind::ScrollRight
                );
                let shift = mouse_event
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT);
                if is_scroll {
                    wants_mouse || self.is_terminal_in_alternate_screen(buffer_id)
                } else if matches!(mouse_event.kind, MouseEventKind::Moved) {
                    // Buttonless motion belongs only to all-motion tracking
                    // (1003); spamming it at click-only/button-drag programs
                    // is out of spec (and its echo can churn the PTY).
                    self.terminal_wants_mouse_motion(buffer_id)
                } else {
                    wants_mouse && !shift
                }
            }
        };
        if !forward {
            return None;
        }

        if let MouseEventKind::Down(button) = mouse_event.kind {
            self.mouse_state.capture_terminal_mouse_gesture(
                button,
                split_id,
                terminal_id,
                content_rect,
            );
        }

        // Forward the event.
        Some(self.forward_mouse_to_terminal(terminal_id, col, row, content_rect, mouse_event))
    }

    /// Whether the inner program of `buffer_id`'s terminal enabled any
    /// mouse-reporting mode (DECSET 1000/1002/1003). The mouse belongs to
    /// the program only when it asked for it; otherwise presses stay with
    /// the editor (focus + drag-to-select).
    pub fn terminal_wants_mouse(&self, buffer_id: BufferId) -> bool {
        if let Some(terminal_id) = self.get_terminal_id(buffer_id) {
            if let Some(handle) = self.terminal_manager.get(terminal_id) {
                if let Ok(state) = handle.state.lock() {
                    return state.wants_mouse_events();
                }
            }
        }
        false
    }

    /// Whether the inner program enabled ALL-motion mouse tracking
    /// (DECSET 1003) — the only mode that legitimately receives
    /// buttonless motion reports.
    pub fn terminal_wants_mouse_motion(&self, buffer_id: BufferId) -> bool {
        if let Some(terminal_id) = self.get_terminal_id(buffer_id) {
            if let Some(handle) = self.terminal_manager.get(terminal_id) {
                if let Ok(state) = handle.state.lock() {
                    return state.wants_mouse_motion();
                }
            }
        }
        false
    }

    /// Detect a clickable file-path link in the live terminal grid at the given
    /// screen position.
    ///
    /// Returns the terminal buffer, the content-area-relative grid row, the
    /// detected link (path + optional line/col + column span), and the
    /// terminal's OSC 7 working directory (for resolving relative paths).
    ///
    /// Only fires in live terminal mode (not the read-only scrollback view).
    /// It intentionally *does* fire for alternate-screen / mouse-reporting
    /// programs: the Ctrl gesture is reserved for links and withheld from the
    /// PTY (see `try_forward_mouse_to_terminal`), so a path printed by such a
    /// program stays Ctrl-hoverable / Ctrl-clickable. The returned link is
    /// textual only — the caller resolves and checks it.
    pub(crate) fn detect_terminal_link_at(
        &self,
        col: u16,
        row: u16,
    ) -> Option<(
        BufferId,
        u16,
        crate::services::terminal::path_link::DetectedLink,
        Option<std::path::PathBuf>,
    )> {
        if !self.focused_terminal_live() {
            return None;
        }
        let (_, buffer_id, content_rect) = self.get_terminal_content_area_at_position(col, row)?;
        // Detection runs even for alternate-screen / mouse-reporting programs:
        // this is only reached for Ctrl-held gestures (see the callers in
        // `terminal_link.rs`, both Ctrl-gated), which `try_forward_mouse_to_terminal`
        // deliberately withholds from the PTY so a path shown by vim/less/htop or
        // any mouse-capturing program is still Ctrl-hoverable and Ctrl-clickable.
        let grid_col = col.saturating_sub(content_rect.x) as usize;
        let term_row = row.saturating_sub(content_rect.y);

        let terminal_id = self.get_terminal_id(buffer_id)?;
        let handle = self.terminal_manager.get(terminal_id)?;
        let (line, text_col, cwd) = {
            let state = handle.state.lock().ok()?;
            let cells = state.get_line(term_row);
            let mut line = String::new();
            let mut text_col = 0;
            for (cell_col, cell) in cells.iter().enumerate() {
                if cell_col < grid_col && !cell.wide_spacer {
                    text_col += 1 + cell.zerowidth.len();
                }
                cell.append_text_to(&mut line);
            }
            let cwd = state.cwd().map(|p| p.to_path_buf());
            (line, text_col, cwd)
        };

        let link = crate::services::terminal::path_link::detect_link_at(&line, text_col)?;
        Some((buffer_id, term_row, link, cwd))
    }

    /// Detect a clickable file-path link in the terminal *scrollback* view at
    /// the given screen position.
    ///
    /// The scrollback view is a normal read-only buffer (the synced terminal
    /// history) shown only for the active terminal buffer when not in live
    /// terminal mode. Clicks map through the standard screen→buffer-position
    /// machinery; we then read the buffer line under the cursor and detect a
    /// path link in it.
    ///
    /// Returns the terminal buffer, the detected link, and the terminal's
    /// OSC 7 working directory (for resolving relative paths).
    pub(crate) fn detect_terminal_scrollback_link_at(
        &self,
        col: u16,
        row: u16,
    ) -> Option<(
        BufferId,
        crate::services::terminal::path_link::DetectedLink,
        Option<std::path::PathBuf>,
    )> {
        // Scrollback links exist only when the focused split's terminal is in
        // read-only scrollback (a live terminal shows the grid instead).
        if self.focused_terminal_live() {
            return None;
        }
        let active = self.active_buffer();
        if !self.is_terminal_buffer(active) {
            return None;
        }

        let (split_id, content_rect) =
            self.layout_cache
                .split_areas
                .iter()
                .find_map(|(sid, bid, rect, _, _, _)| {
                    (*bid == active
                        && col >= rect.x
                        && col < rect.x + rect.width
                        && row >= rect.y
                        && row < rect.y + rect.height)
                        .then_some((*sid, *rect))
                })?;

        let state = self.buffers.get(&active)?;
        let gutter_width = state.margins.left_total_width() as u16;
        let cached_mappings = self.layout_cache.view_line_mappings.get(&split_id).cloned();
        let (fallback, compose_width) = self
            .buffers
            .splits()
            .and_then(|(_, vs)| vs.get(&split_id))
            .map(|vs| (vs.viewport.top_byte(), vs.compose_width))
            .unwrap_or((0, None));

        // `allow_gutter_click = false`: a click in the gutter isn't on a path.
        let byte_pos = crate::app::click_geometry::screen_to_buffer_position(
            col,
            row,
            content_rect,
            gutter_width,
            &cached_mappings,
            fallback,
            false,
            compose_width,
        )?;

        let pos = crate::model::buffer_position::byte_to_2d(&state.buffer, byte_pos);
        let line_bytes = state.buffer.get_line(pos.line)?;
        let line = String::from_utf8_lossy(&line_bytes);
        let line = line.strip_suffix('\n').unwrap_or(&line);
        // `pos.column` is a byte offset within the line; convert to a char
        // column for the (char-indexed) detector.
        let char_col = line
            .char_indices()
            .take_while(|(b, _)| *b < pos.column)
            .count();

        let link = crate::services::terminal::path_link::detect_link_at(line, char_col)?;
        let cwd = self
            .get_terminal_id(active)
            .and_then(|tid| self.terminal_manager.get(tid))
            .and_then(|h| {
                h.state
                    .lock()
                    .ok()
                    .and_then(|s| s.cwd().map(|p| p.to_path_buf()))
            });

        Some((active, link, cwd))
    }

    /// Get the terminal split, buffer, and painted content area under the pointer.
    fn get_terminal_content_area_at_position(
        &self,
        col: u16,
        row: u16,
    ) -> Option<(crate::model::event::LeafId, BufferId, Rect)> {
        for (split_id, buffer_id, content_rect, _, _, _) in &self.layout_cache.split_areas {
            if col >= content_rect.x
                && col < content_rect.x + content_rect.width
                && row >= content_rect.y
                && row < content_rect.y + content_rect.height
                && self.is_terminal_buffer(*buffer_id)
            {
                return Some((*split_id, *buffer_id, *content_rect));
            }
        }
        None
    }

    fn terminal_content_area_for_capture(
        &self,
        split_id: crate::model::event::LeafId,
        terminal_id: crate::services::terminal::TerminalId,
    ) -> Option<Rect> {
        self.layout_cache.split_areas.iter().find_map(
            |(candidate_split, buffer_id, content_rect, _, _, _)| {
                (*candidate_split == split_id
                    && self.get_terminal_id(*buffer_id) == Some(terminal_id))
                .then_some(*content_rect)
            },
        )
    }

    /// Scroll the focused live terminal under the pointer while preserving
    /// terminal mode. Returns false when the pointer is not over that terminal.
    pub(crate) fn scroll_live_terminal_at_position(
        &mut self,
        col: u16,
        row: u16,
        delta: i32,
    ) -> bool {
        if !self.focused_terminal_live() {
            return false;
        }
        let Some((_, buffer_id, _)) = self.get_terminal_content_area_at_position(col, row) else {
            return false;
        };
        if buffer_id != self.active_buffer() {
            return false;
        }
        let Some(terminal_id) = self.get_terminal_id(buffer_id) else {
            return false;
        };
        let Some(handle) = self.terminal_manager.get(terminal_id) else {
            return false;
        };
        let Ok(mut state) = handle.state.lock() else {
            return false;
        };
        state.scroll_lines(delta);
        true
    }

    /// Forward a mouse event to the captured terminal PTY. The terminal id is
    /// captured on Down, rather than recovered from whichever buffer is active
    /// when a later Drag/Up reaches us.
    fn forward_mouse_to_terminal(
        &self,
        terminal_id: crate::services::terminal::TerminalId,
        col: u16,
        row: u16,
        content_rect: Rect,
        mouse_event: MouseEvent,
    ) -> AnyhowResult<bool> {
        // Captured releases can arrive outside the pane; terminal protocols
        // still require an in-grid coordinate. The same clamp keeps drags at
        // the nearest edge, and zero-sized geometry safely emits nothing.
        let Some((term_col, term_row)) = clamped_rect_offset(content_rect, col, row) else {
            return Ok(false);
        };

        self.send_terminal_mouse_to(
            terminal_id,
            term_col,
            term_row,
            convert_kind(mouse_event.kind),
            mouse_event.modifiers,
        );
        Ok(true)
    }

    fn retire_mouse_gesture_owner(
        &self,
        button: MouseButton,
        owner: super::types::MouseGestureOwner,
        col: u16,
        row: u16,
    ) {
        let super::types::MouseGestureOwner::Terminal {
            split_id,
            terminal_id,
            content_rect,
        } = owner
        else {
            return;
        };
        let content_rect = self
            .terminal_content_area_for_capture(split_id, terminal_id)
            .unwrap_or(content_rect);
        let Some((col, row)) = clamped_rect_offset(content_rect, col, row) else {
            return;
        };
        self.send_terminal_mouse_to(
            terminal_id,
            col,
            row,
            TerminalMouseEventKind::Up(convert_button(button)),
            crossterm::event::KeyModifiers::NONE,
        );
    }

    pub(crate) fn cancel_mouse_gesture(&mut self, button: MouseButton, col: u16, row: u16) {
        if let Some(owner) = self.mouse_state.finish_mouse_gesture(button) {
            self.retire_mouse_gesture_owner(button, owner, col, row);
        }
    }

    /// Every press delivered to a PTY must be terminated, even when focus or
    /// its visible buffer changes before the OS sends the matching release.
    pub(crate) fn cancel_terminal_mouse_gestures(&mut self) {
        let (col, row) = self.mouse_state.last_position.unwrap_or((0, 0));
        for (button, owner) in self.mouse_state.take_mouse_gestures() {
            self.retire_mouse_gesture_owner(button, owner, col, row);
        }
    }
}

/// Convert crossterm MouseButton to our TerminalMouseButton.
fn convert_button(btn: MouseButton) -> TerminalMouseButton {
    match btn {
        MouseButton::Left => TerminalMouseButton::Left,
        MouseButton::Right => TerminalMouseButton::Right,
        MouseButton::Middle => TerminalMouseButton::Middle,
    }
}

/// Convert a crossterm `MouseEventKind` to the kind the PTY encoders speak.
///
/// Total: every kind the input parser can produce has a wire representation,
/// horizontal wheel included (xterm buttons 6 and 7). This used to drop
/// `ScrollLeft`/`ScrollRight` on the floor — and because the caller reports
/// "handled" either way, a horizontal wheel over a mouse-tracking terminal
/// was swallowed rather than falling through to Fresh's own panning.
fn convert_kind(kind: MouseEventKind) -> TerminalMouseEventKind {
    match kind {
        MouseEventKind::Down(btn) => TerminalMouseEventKind::Down(convert_button(btn)),
        MouseEventKind::Up(btn) => TerminalMouseEventKind::Up(convert_button(btn)),
        MouseEventKind::Drag(btn) => TerminalMouseEventKind::Drag(convert_button(btn)),
        MouseEventKind::Moved => TerminalMouseEventKind::Moved,
        MouseEventKind::ScrollUp => TerminalMouseEventKind::ScrollUp,
        MouseEventKind::ScrollDown => TerminalMouseEventKind::ScrollDown,
        MouseEventKind::ScrollLeft => TerminalMouseEventKind::ScrollLeft,
        MouseEventKind::ScrollRight => TerminalMouseEventKind::ScrollRight,
    }
}

#[cfg(test)]
mod convert_kind_tests {
    use super::*;

    #[test]
    fn horizontal_wheel_has_a_wire_representation() {
        assert_eq!(
            convert_kind(MouseEventKind::ScrollLeft),
            TerminalMouseEventKind::ScrollLeft
        );
        assert_eq!(
            convert_kind(MouseEventKind::ScrollRight),
            TerminalMouseEventKind::ScrollRight
        );
    }

    #[test]
    fn other_kinds_are_unchanged() {
        assert_eq!(
            convert_kind(MouseEventKind::Down(MouseButton::Left)),
            TerminalMouseEventKind::Down(TerminalMouseButton::Left)
        );
        assert_eq!(
            convert_kind(MouseEventKind::Drag(MouseButton::Middle)),
            TerminalMouseEventKind::Drag(TerminalMouseButton::Middle)
        );
        assert_eq!(
            convert_kind(MouseEventKind::Moved),
            TerminalMouseEventKind::Moved
        );
        assert_eq!(
            convert_kind(MouseEventKind::ScrollUp),
            TerminalMouseEventKind::ScrollUp
        );
        assert_eq!(
            convert_kind(MouseEventKind::ScrollDown),
            TerminalMouseEventKind::ScrollDown
        );
    }

    #[test]
    fn captured_terminal_coordinates_clamp_to_visible_rect() {
        let rect = Rect::new(10, 20, 5, 3);
        assert_eq!(clamped_rect_offset(rect, 0, 0), Some((0, 0)));
        assert_eq!(clamped_rect_offset(rect, 99, 99), Some((4, 2)));
        assert_eq!(clamped_rect_offset(Rect::new(0, 0, 0, 3), 0, 0), None);
        assert_eq!(clamped_rect_offset(Rect::new(0, 0, 3, 0), 0, 0), None);
    }

    #[test]
    fn only_drag_and_release_are_captured_continuations() {
        assert_eq!(
            captured_button_event(MouseEventKind::Drag(MouseButton::Left)),
            Some((MouseButton::Left, false))
        );
        assert_eq!(
            captured_button_event(MouseEventKind::Up(MouseButton::Left)),
            Some((MouseButton::Left, true))
        );
        assert_eq!(
            captured_button_event(MouseEventKind::Down(MouseButton::Left)),
            None
        );
    }

    #[test]
    fn blank_terminal_cells_do_not_select_the_stored_newline() {
        use crate::model::filesystem::StdFileSystem;
        use std::sync::Arc;

        let buffer = crate::model::buffer::Buffer::from_bytes(
            "e\u{301}\nnext".as_bytes().to_vec(),
            Arc::new(StdFileSystem),
        );
        assert_eq!(terminal_grid_cell_end_in_buffer(&buffer, 0), 3);
        assert_eq!(terminal_grid_cell_end_in_buffer(&buffer, 3), 3);
    }
}

impl super::Editor {
    /// Begin a text-selection drag on a terminal split that was showing the
    /// live PTY grid when the mouse went down (see
    /// `MouseState::terminal_drag_pending` — a bare click only focuses).
    ///
    /// Live terminals have no cursor/selection model of their own, so the
    /// split is dropped into read-only scrollback first — exactly the
    /// Ctrl+Space / scroll-up transition. `sync_terminal_to_buffer` pins the
    /// scrollback viewport to the just-appended visible screen, making the
    /// scrollback view pixel-identical to the grid the user aimed at: the
    /// view grid-wraps at the capture-time PTY width (fresh#2649), so grid
    /// row r is visual row r of the anchored viewport and grid columns map
    /// 1:1 within a row (no gutter). That lets both the press
    /// origin and the current drag position resolve to exact byte positions
    /// without waiting for a re-render; the standard text-selection drag
    /// machinery then takes over (Ctrl+C copies through the editor
    /// clipboard as usual; Ctrl+Space resumes the live terminal).
    pub(super) fn begin_terminal_grid_selection(
        &mut self,
        split_id: crate::model::event::LeafId,
        buffer_id: BufferId,
        origin_col: u16,
        origin_row: u16,
        col: u16,
        row: u16,
    ) -> AnyhowResult<()> {
        self.active_window_mut().mouse_state.terminal_drag_pending = None;

        let Some(content_rect) =
            self.drop_terminal_grid_into_selection_scrollback(split_id, buffer_id)
        else {
            return Ok(());
        };

        // Cursor selections are end-exclusive, while terminal selections own
        // the cells under both ends of the drag. Resolve the pointer cell to
        // its end when dragging right, and the origin cell to its end when
        // dragging left.
        let anchor_start =
            self.terminal_grid_byte_at(split_id, buffer_id, content_rect, origin_col, origin_row);
        let head_start = self.terminal_grid_byte_at(split_id, buffer_id, content_rect, col, row);
        let (Some(anchor_start), Some(head_start)) = (anchor_start, head_start) else {
            return Ok(());
        };
        let anchor_end = self.terminal_grid_cell_end(buffer_id, anchor_start);
        let (anchor, head) = if head_start >= anchor_start {
            (
                anchor_start,
                self.terminal_grid_cell_end(buffer_id, head_start),
            )
        } else {
            (anchor_end, head_start)
        };

        if let Some(view_state) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.buffers.splits_mut())
            .and_then(|(_, vs)| vs.get_mut(&split_id))
        {
            let cursor = view_state.cursors.primary_mut();
            cursor.position = head;
            cursor.anchor = Some(anchor);
        }

        // Hand off to the standard drag machinery for subsequent motion.
        let ms = &mut self.active_window_mut().mouse_state;
        ms.dragging_text_selection = true;
        ms.drag_selection_split = Some(split_id);
        ms.drag_selection_anchor = Some(anchor);
        ms.terminal_drag_anchor_end = Some(anchor_end);
        Ok(())
    }

    /// Double-click on the live terminal grid: select the word under the
    /// pointer. Same trick as [`Editor::begin_terminal_grid_selection`] — the
    /// live grid has no selection model, so the split first drops into
    /// implicit scrollback (pixel-identical view), then the standard
    /// word-selection and word-wise drag machinery take over.
    pub(super) fn begin_terminal_grid_word_selection(
        &mut self,
        split_id: crate::model::event::LeafId,
        buffer_id: BufferId,
        col: u16,
        row: u16,
    ) -> AnyhowResult<()> {
        self.active_window_mut().mouse_state.terminal_drag_pending = None;

        let Some(content_rect) =
            self.drop_terminal_grid_into_selection_scrollback(split_id, buffer_id)
        else {
            return Ok(());
        };
        let Some(pos) = self.terminal_grid_byte_at(split_id, buffer_id, content_rect, col, row)
        else {
            return Ok(());
        };

        if let Some(view_state) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.buffers.splits_mut())
            .and_then(|(_, vs)| vs.get_mut(&split_id))
        {
            let cursor = view_state.cursors.primary_mut();
            cursor.position = pos;
            cursor.anchor = None;
        }
        self.handle_action(crate::input::keybindings::Action::SelectWord)?;

        // Mirror `handle_editor_double_click`: arm word-wise drag extension.
        if let Some((sel_start, sel_end)) = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .and_then(|(_, vs)| vs.get(&split_id))
            .map(|vs| {
                let c = vs.cursors.primary();
                (c.selection_start(), c.selection_end())
            })
        {
            let ms = &mut self.active_window_mut().mouse_state;
            ms.dragging_text_selection = true;
            ms.drag_selection_split = Some(split_id);
            ms.drag_selection_anchor = Some(sel_start);
            ms.drag_selection_by_words = true;
            ms.drag_selection_word_end = Some(sel_end);
        }
        Ok(())
    }

    /// Triple-click on the live terminal grid: select the whole line under
    /// the pointer, via the same implicit-scrollback detour.
    pub(super) fn begin_terminal_grid_line_selection(
        &mut self,
        split_id: crate::model::event::LeafId,
        buffer_id: BufferId,
        col: u16,
        row: u16,
    ) -> AnyhowResult<()> {
        self.active_window_mut().mouse_state.terminal_drag_pending = None;

        let Some(content_rect) =
            self.drop_terminal_grid_into_selection_scrollback(split_id, buffer_id)
        else {
            return Ok(());
        };
        let Some(pos) = self.terminal_grid_byte_at(split_id, buffer_id, content_rect, col, row)
        else {
            return Ok(());
        };

        if let Some(view_state) = self
            .windows
            .get_mut(&self.active_window)
            .and_then(|w| w.buffers.splits_mut())
            .and_then(|(_, vs)| vs.get_mut(&split_id))
        {
            let cursor = view_state.cursors.primary_mut();
            cursor.position = pos;
            cursor.anchor = None;
        }
        self.handle_action(crate::input::keybindings::Action::SelectLine)?;
        self.arm_terminal_selection_publication(split_id);
        Ok(())
    }

    pub(super) fn arm_terminal_selection_publication(
        &mut self,
        split_id: crate::model::event::LeafId,
    ) {
        let Some(anchor) = self
            .windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.splits())
            .and_then(|(_, vs)| vs.get(&split_id))
            .map(|vs| vs.cursors.primary().selection_start())
        else {
            return;
        };

        let ms = &mut self.active_window_mut().mouse_state;
        ms.dragging_text_selection = true;
        ms.drag_selection_split = Some(split_id);
        ms.drag_selection_anchor = Some(anchor);
        ms.drag_selection_by_words = false;
        ms.drag_selection_word_end = None;
    }

    /// Drop a *live* terminal grid split into read-only scrollback for a
    /// selection gesture, pinned pixel-identical to the grid (see
    /// [`Editor::begin_terminal_grid_selection`]), and mark the visit
    /// implicit (drag-initiated) so completing the gesture — copying, or a
    /// bare click abandoning the selection — resumes the live grid
    /// automatically. Returns the split's content rect, or `None` when the
    /// situation changed under the gesture (buffer no longer a terminal,
    /// split already in scrollback, layout gone) and nothing was done.
    fn drop_terminal_grid_into_selection_scrollback(
        &mut self,
        split_id: crate::model::event::LeafId,
        buffer_id: BufferId,
    ) -> Option<Rect> {
        if !self.active_window().is_terminal_buffer(buffer_id)
            || self
                .active_window()
                .split_terminal_scrollback(split_id, buffer_id)
        {
            return None;
        }
        let content_rect = self
            .active_layout()
            .split_areas
            .iter()
            .find(|(sid, bid, _, _, _, _)| *sid == split_id && *bid == buffer_id)
            .map(|(_, _, rect, _, _, _)| *rect)?;

        // Drop into read-only scrollback. The press already focused the
        // split, so the sync pins THIS split's viewport to the grid's row 0.
        self.active_window_mut()
            .set_split_terminal_scrollback(split_id, buffer_id, true);
        self.active_window_mut()
            .set_split_terminal_drag_scrollback(split_id, buffer_id, true);
        self.active_window_mut().sync_terminal_mode_flags();
        self.set_status_message(
            "Terminal mode disabled - read only (Ctrl+Space to resume)".to_string(),
        );
        Some(content_rect)
    }

    /// Resolve a screen position over a terminal scrollback view to a byte
    /// offset: grid row `r` is buffer line `top_line + r` and grid columns
    /// map 1:1 (wrap off, no gutter). Exact for any scroll position, with no
    /// dependency on render-cached view-line mappings.
    pub(super) fn terminal_grid_byte_at(
        &self,
        split_id: crate::model::event::LeafId,
        buffer_id: BufferId,
        content_rect: Rect,
        col: u16,
        row: u16,
    ) -> Option<usize> {
        let win = self.windows.get(&self.active_window)?;
        let (_, view_states) = win.buffers.splits()?;
        let vs = view_states.get(&split_id)?;
        let state = win.buffers.get(&buffer_id)?;
        let (top_line, _) = state.buffer.position_to_line_col(vs.viewport.top_byte());
        let (grid_col, grid_row) = clamped_rect_offset(content_rect, col, row)?;
        let grid_row = grid_row as usize;
        // Account for horizontal scroll (a pinned view starts at 0, but an
        // explicit scrollback view may have been scrolled right).
        let grid_col = grid_col as usize + vs.viewport.left_column as usize;

        // Grid-wrapped scroll-back (fresh#2649): visual rows are exact-column
        // wrap segments of the logical lines, so walk the segments from the
        // viewport anchor to the clicked row instead of assuming one buffer
        // line per grid row (which was only ever true for unwrapped lines —
        // a grid row that continued a wrapped line used to resolve to the
        // wrong buffer line entirely).
        if vs.viewport.grid_wrap && vs.viewport.line_wrap_enabled {
            let cols = vs.viewport.grid_cols();
            let mut remaining = vs.viewport.top_view_line_offset() + grid_row;
            let mut line_idx = top_line;
            loop {
                let Some(bytes) = state.buffer.get_line(line_idx) else {
                    // Clicked past the buffer's tail: clamp to the end.
                    return Some(state.buffer.len());
                };
                let text = String::from_utf8_lossy(&bytes);
                let trimmed = text.trim_end_matches(['\n', '\r']);
                let rows =
                    crate::view::line_wrap_cache::count_visual_rows_for_text_grid(trimmed, cols)
                        as usize;
                if remaining < rows {
                    let line_start = state.buffer.line_col_to_position(line_idx, 0);
                    let layout =
                        crate::view::line_wrap_cache::layout_for_plain_text_grid(trimmed, cols, 4);
                    // A column past the row's rendered content resolves to
                    // the row segment's END (the next segment's start, or the
                    // line end on the last row) — same as click_geometry's
                    // past-content clamp for ordinary buffer views. It must
                    // NOT go through `source_byte_at_visual_col`, whose
                    // out-of-range fallback clamps to the LAST character's
                    // start byte and would drop the final character from any
                    // selection whose drag overshoots the text.
                    let byte_in_line = match layout.get(remaining) {
                        Some(seg) if grid_col < seg.visual_width() => seg
                            .source_byte_at_visual_col(grid_col)
                            .unwrap_or(trimmed.len()),
                        _ => layout
                            .get(remaining + 1)
                            .and_then(|next| next.source_start_byte)
                            .unwrap_or(trimmed.len()),
                    };
                    return Some(
                        state
                            .buffer
                            .snap_to_char_boundary(line_start + byte_in_line.min(trimmed.len())),
                    );
                }
                remaining -= rows;
                line_idx += 1;
            }
        }

        let pos = state
            .buffer
            .line_col_to_position(top_line + grid_row, grid_col);
        Some(state.buffer.snap_to_char_boundary(pos))
    }

    /// Return the byte boundary after the terminal cell that starts at `pos`.
    /// Empty cells past the rendered line stay collapsed at the line end.
    pub(super) fn terminal_grid_cell_end(&self, buffer_id: BufferId, pos: usize) -> usize {
        self.windows
            .get(&self.active_window)
            .and_then(|w| w.buffers.get(&buffer_id))
            .map(|state| terminal_grid_cell_end_in_buffer(&state.buffer, pos))
            .unwrap_or(pos)
    }
}
