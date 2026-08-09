//! Ghostty passthrough must not steal mouse input from editor overlays.

#![cfg(unix)]

mod common;

use common::git_test_helper::GitTestRepo;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness};
use crossterm::event::{KeyCode, KeyModifiers};
use fresh::config::Config;
use std::ffi::OsString;
use std::fs;

const GHOSTTY_PASSTHROUGH: &str = "SMARTY_FRESH_GHOSTTY_PASSTHROUGH";
const PREVIEW_MARKER: &str = "GHOSTTY_OVERLAY_TOP";
const DEEP_MARKER: &str = "GHOSTTY_OVERLAY_DEEP";
const NEEDLE: &str = "GHOSTTY_OVERLAY_NEEDLE";

struct EnvGuard(Option<OsString>);

impl EnvGuard {
    fn enable() -> Self {
        let previous = std::env::var_os(GHOSTTY_PASSTHROUGH);
        std::env::set_var(GHOSTTY_PASSTHROUGH, "1");
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            std::env::set_var(GHOSTTY_PASSTHROUGH, previous);
        } else {
            std::env::remove_var(GHOSTTY_PASSTHROUGH);
        }
    }
}

#[test]
fn ghostty_passthrough_does_not_steal_live_grep_preview_wheel() {
    let _env = EnvGuard::enable();
    let repo = GitTestRepo::new();
    let plugins = repo.path.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "live_grep");

    let mut content = format!("{PREVIEW_MARKER}\n{NEEDLE}\n");
    for line in 0..80 {
        content.push_str(&format!("filler line {line:02}\n"));
    }
    content.push_str(&format!("{DEEP_MARKER}\n"));
    repo.create_file("match.txt", &content);
    repo.git_add_all();
    repo.git_commit("seed live grep target");

    let mut harness = EditorTestHarness::with_config_and_working_dir(
        160,
        40,
        Config::default(),
        repo.path.clone(),
    )
    .unwrap();
    harness.editor_mut().open_terminal();
    harness
        .wait_until(|h| h.editor().is_terminal_mode())
        .unwrap();

    // Make the underlying live terminal mouse-hungry. The overlay's wheel
    // must still win before this requested PTY forwarding path.
    let buffer_id = harness.editor().active_buffer_id();
    let terminal_id = harness
        .editor()
        .active_window()
        .get_terminal_id(buffer_id)
        .expect("active buffer should be a terminal");
    if let Some(handle) = harness.editor().terminal_manager().get(terminal_id) {
        if let Ok(mut state) = handle.state.lock() {
            state.process_output(b"\x1b[?1000h");
        }
    }
    assert!(harness
        .editor()
        .active_window()
        .terminal_wants_mouse(buffer_id));

    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness.type_text("Live Grep").unwrap();
    harness
        .wait_until(|h| h.screen_to_string().contains("Live Grep"))
        .unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    harness
        .wait_until(|h| h.screen_to_string().contains("Search in:"))
        .unwrap();
    harness.type_text(NEEDLE).unwrap();
    harness
        .wait_until(|h| {
            let screen = h.screen_to_string();
            screen.contains("match.txt") && screen.contains(PREVIEW_MARKER)
        })
        .unwrap();
    harness.wait_until_stable(|_| true).unwrap();
    assert!(!harness.screen_to_string().contains(DEEP_MARKER));

    for _ in 0..20 {
        harness.mouse_scroll_down(140, 12).unwrap();
    }
    harness.wait_until_stable(|_| true).unwrap();

    let screen = harness.screen_to_string();
    assert!(
        screen.contains(DEEP_MARKER),
        "the Live Grep overlay must consume preview wheel input before Ghostty terminal passthrough:\n{screen}"
    );
}
