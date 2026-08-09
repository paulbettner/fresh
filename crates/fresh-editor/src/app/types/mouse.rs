use super::drag::TabDragState;
use super::hover::HoverTarget;
use crate::config::ExplorerWidth;
use crate::model::event::{BufferId, ContainerId, LeafId, SplitDirection};
use crate::services::terminal::TerminalId;
use crossterm::event::MouseButton;
use ratatui::layout::Rect;

/// Owner chosen when a button goes down. It stays fixed until the matching
/// release so a modifier change, overlay, or pane crossing cannot split one
/// gesture between Fresh and a PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseGestureOwner {
    Fresh,
    Terminal {
        split_id: LeafId,
        terminal_id: TerminalId,
        content_rect: Rect,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MouseClickTarget {
    Overlay(crate::app::overlay::LayerKind),
    Chrome(HoverTarget),
    Panel {
        slot: crate::app::PanelSlot,
        panel_key: crate::widgets::PanelKey,
        widget_key: Option<String>,
        event_type: Option<&'static str>,
        payload: Option<serde_json::Value>,
    },
    Buffer {
        split_id: LeafId,
        buffer_id: BufferId,
        byte_position: Option<usize>,
    },
    Background,
}

/// Mouse state tracking
#[derive(Debug, Clone, Default)]
pub struct MouseState {
    /// Whether we're currently dragging a vertical scrollbar
    pub dragging_scrollbar: Option<LeafId>,
    /// Whether we're currently dragging a horizontal scrollbar
    pub dragging_horizontal_scrollbar: Option<LeafId>,
    /// Initial mouse column when starting horizontal scrollbar drag
    pub drag_start_hcol: Option<u16>,
    /// Initial left_column when starting horizontal scrollbar drag
    pub drag_start_left_column: Option<usize>,
    /// Last mouse position
    pub last_position: Option<(u16, u16)>,
    /// Mouse hover for LSP: byte position being hovered, timer start, screen
    /// position, and the buffer the mouse is over.
    /// Format: (byte_position, hover_start_instant, screen_x, screen_y, buffer_id)
    ///
    /// `buffer_id` records which split's buffer the pointer is over so the
    /// hover request targets *that* buffer rather than the active one. Without
    /// it, hovering a non-active split (or a UI panel such as the
    /// Search/Replace dock) fired a hover for the active code buffer at a byte
    /// offset taken from the hovered split's geometry — the popup "leaked
    /// through" the panel (#2572).
    pub lsp_hover_state: Option<(usize, std::time::Instant, u16, u16, BufferId)>,
    /// Whether we've already sent a hover request for the current position
    pub lsp_hover_request_sent: bool,
    /// Initial mouse row when starting to drag the scrollbar thumb
    /// Used to calculate relative movement rather than jumping
    pub drag_start_row: Option<u16>,
    /// Initial viewport top_byte when starting to drag the scrollbar thumb
    pub drag_start_top_byte: Option<usize>,
    /// Initial viewport top_view_line_offset when starting to drag the scrollbar thumb
    /// This is needed for proper visual row calculation when scrolled into a wrapped line
    pub drag_start_view_line_offset: Option<usize>,
    /// Whether we're currently dragging a split separator
    /// Stores (split_id, direction) for the separator being dragged
    pub dragging_separator: Option<(ContainerId, SplitDirection)>,
    /// Initial mouse position when starting to drag a separator
    pub drag_start_position: Option<(u16, u16)>,
    /// Initial split ratio when starting to drag a separator
    pub drag_start_ratio: Option<f32>,
    /// Whether we're currently dragging the file explorer border
    pub dragging_file_explorer: bool,
    /// File explorer width at the moment the drag started. Drag
    /// preserves the active variant: a drag that begins in `Percent`
    /// stays in `Percent`, and likewise for `Columns`.
    pub drag_start_explorer_width: Option<ExplorerWidth>,
    /// Current hover target (if any)
    pub hover_target: Option<HoverTarget>,
    /// Whether we're currently doing a text selection drag
    pub dragging_text_selection: bool,
    /// The split where text selection started
    pub drag_selection_split: Option<LeafId>,
    /// The buffer byte position where the selection anchor is
    pub drag_selection_anchor: Option<usize>,
    /// End of the physical anchor cell for a terminal grid drag. Cached once
    /// because resolving it from a very long logical scrollback line on each
    /// motion event is needlessly expensive.
    pub terminal_drag_anchor_end: Option<usize>,
    /// When true, dragging extends selection by whole words (set by double-click)
    pub drag_selection_by_words: bool,
    /// The end of the initially double-clicked word (used as anchor when dragging backward)
    pub drag_selection_word_end: Option<usize>,
    /// Tab drag state (for drag-to-split functionality)
    pub dragging_tab: Option<TabDragState>,
    /// Whether we're currently dragging a popup scrollbar (popup index)
    pub dragging_popup_scrollbar: Option<usize>,
    /// Initial scroll offset when starting to drag popup scrollbar
    pub drag_start_popup_scroll: Option<usize>,
    /// Whether we're currently dragging the prompt's suggestion-list
    /// scrollbar (Live Grep floating overlay, issue #1796). The
    /// rect is held in `ChromeLayout::suggestions_scrollbar_rect`
    /// and the math is shared with the buffer-popup scrollbar via
    /// `view::ui::scrollbar::ScrollbarState::click_to_offset`.
    pub dragging_prompt_scrollbar: bool,
    /// Whether we're currently selecting text in a popup (popup index)
    pub selecting_in_popup: Option<usize>,
    /// Initial composite scroll_row when starting to drag the scrollbar thumb
    /// Used for composite buffer scrollbar drag
    pub drag_start_composite_scroll_row: Option<usize>,
    /// A left press on a live terminal grid: (split, buffer, col, row).
    /// Not a selection yet — a bare click keeps the terminal live
    /// (click-to-focus-and-type). If a `Drag(Left)` follows,
    /// `Editor::begin_terminal_grid_selection` drops that split into
    /// read-only scrollback and starts a real text-selection drag anchored
    /// at this origin. Cleared on mouse-up.
    pub terminal_drag_pending: Option<(LeafId, BufferId, u16, u16)>,
    /// Press-time Fresh-versus-PTY ownership, independently tracked for each
    /// button until its matching release.
    pub gesture_captures: std::collections::HashMap<MouseButton, MouseGestureOwner>,
}

impl MouseState {
    pub fn start_mouse_gesture(&mut self, button: MouseButton) {
        self.gesture_captures
            .insert(button, MouseGestureOwner::Fresh);
    }

    pub fn capture_terminal_mouse_gesture(
        &mut self,
        button: MouseButton,
        split_id: LeafId,
        terminal_id: TerminalId,
        content_rect: Rect,
    ) {
        self.gesture_captures.insert(
            button,
            MouseGestureOwner::Terminal {
                split_id,
                terminal_id,
                content_rect,
            },
        );
    }

    pub fn mouse_gesture_owner(&self, button: MouseButton) -> Option<MouseGestureOwner> {
        self.gesture_captures.get(&button).copied()
    }

    pub fn finish_mouse_gesture(&mut self, button: MouseButton) -> Option<MouseGestureOwner> {
        self.gesture_captures.remove(&button)
    }

    pub fn take_mouse_gestures(
        &mut self,
    ) -> std::collections::HashMap<MouseButton, MouseGestureOwner> {
        std::mem::take(&mut self.gesture_captures)
    }

    pub fn clear_drag_state(&mut self) {
        self.dragging_scrollbar = None;
        self.drag_start_row = None;
        self.drag_start_top_byte = None;
        self.drag_start_view_line_offset = None;
        self.dragging_horizontal_scrollbar = None;
        self.drag_start_hcol = None;
        self.drag_start_left_column = None;
        self.dragging_separator = None;
        self.drag_start_position = None;
        self.drag_start_ratio = None;
        self.dragging_file_explorer = false;
        self.drag_start_explorer_width = None;
        self.dragging_text_selection = false;
        self.drag_selection_split = None;
        self.drag_selection_anchor = None;
        self.terminal_drag_anchor_end = None;
        self.drag_selection_by_words = false;
        self.drag_selection_word_end = None;
        self.dragging_tab = None;
        self.dragging_popup_scrollbar = None;
        self.drag_start_popup_scroll = None;
        self.dragging_prompt_scrollbar = false;
        self.selecting_in_popup = None;
        self.drag_start_composite_scroll_row = None;
        self.terminal_drag_pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_gesture_captures_are_independent_per_button() {
        let mut state = MouseState::default();
        let rect = Rect::new(10, 4, 80, 24);
        state.capture_terminal_mouse_gesture(
            MouseButton::Left,
            LeafId(crate::model::event::SplitId(1)),
            TerminalId(11),
            rect,
        );
        state.capture_terminal_mouse_gesture(
            MouseButton::Right,
            LeafId(crate::model::event::SplitId(2)),
            TerminalId(12),
            rect,
        );
        assert!(matches!(
            state.finish_mouse_gesture(MouseButton::Right),
            Some(MouseGestureOwner::Terminal {
                terminal_id: TerminalId(12),
                ..
            })
        ));
        assert!(matches!(
            state.finish_mouse_gesture(MouseButton::Left),
            Some(MouseGestureOwner::Terminal {
                terminal_id: TerminalId(11),
                ..
            })
        ));
        assert!(state.gesture_captures.is_empty());
    }

    #[test]
    fn drag_cleanup_preserves_independent_button_captures() {
        let mut state = MouseState::default();
        state.dragging_text_selection = true;
        state.terminal_drag_pending =
            Some((LeafId(crate::model::event::SplitId(1)), BufferId(2), 3, 4));
        state.start_mouse_gesture(MouseButton::Left);
        state.start_mouse_gesture(MouseButton::Right);

        state.clear_drag_state();

        assert!(!state.dragging_text_selection);
        assert!(state.terminal_drag_pending.is_none());
        assert_eq!(state.gesture_captures.len(), 2);
    }

    #[test]
    fn taking_mouse_gestures_moves_capture_storage_without_filtering() {
        let mut state = MouseState::default();
        state.start_mouse_gesture(MouseButton::Left);
        state.capture_terminal_mouse_gesture(
            MouseButton::Right,
            LeafId(crate::model::event::SplitId(2)),
            TerminalId(12),
            Rect::new(1, 2, 3, 4),
        );

        let captures = state.take_mouse_gestures();

        assert!(state.gesture_captures.is_empty());
        assert_eq!(captures.len(), 2);
    }
}
