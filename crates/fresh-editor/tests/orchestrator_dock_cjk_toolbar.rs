//! CJK dock toolbar layout runs in its own integration-test process because
//! `rust_i18n` stores the locale globally.

mod common;

use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness};
use crossterm::event::{KeyCode, KeyModifiers};
use fresh::config::{Config, LocaleName};
use std::fs;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;

/// A git project with the orchestrator plugin (+ shared lib) installed.
fn setup_project(name: &str) -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let root = temp_dir.path().join(name);
    fs::create_dir(&root).unwrap();
    let plugins_dir = root.join("plugins");
    fs::create_dir(&plugins_dir).unwrap();
    copy_plugin_lib(&plugins_dir);
    copy_plugin(&plugins_dir, "orchestrator");
    fs::write(root.join("readme.txt"), "hello\n").unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    (temp_dir, root)
}

/// Find text by terminal cells, skipping each wide grapheme's continuation
/// column instead of depending on how the test backend serializes that blank.
fn cell_text_position(harness: &EditorTestHarness, needle: &str) -> Option<(u16, u16)> {
    let area = harness.buffer().area;
    for y in area.y..area.y + area.height {
        'start: for start in area.x..area.x + area.width {
            let mut x = start;
            for grapheme in needle.graphemes(true) {
                if harness.get_cell(x, y).as_deref() != Some(grapheme) {
                    continue 'start;
                }
                x = x.saturating_add(fresh_core::display_width::str_width(grapheme) as u16);
            }
            return Some((start, y));
        }
    }
    None
}

/// CJK toolbar labels consume two terminal cells each. At this width the
/// Japanese New Task button and search field must wrap onto separate rows;
/// UTF-16 `length` would incorrectly keep them on one overflowing row.
#[test]
fn cjk_dock_toolbar_wraps_by_terminal_columns() {
    let (_tmp, root) = setup_project("cjk-toolbar");
    let mut config = Config::default();
    config.locale = LocaleName(Some("ja".into()));
    let mut harness =
        EditorTestHarness::with_config_and_working_dir(100, 30, config, root).unwrap();

    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness.type_text("Orchestrator: ドックの切り替え").unwrap();
    harness
        .wait_until(|h| cell_text_position(h, "Orchestrator: ドックの切り替え").is_some())
        .unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    harness
        .wait_until(|h| {
            cell_text_position(h, "新しいタスク").is_some()
                && cell_text_position(h, "タスクを").is_some()
        })
        .unwrap();

    let new_task_row = cell_text_position(&harness, "新しいタスク").unwrap().1;
    let search_row = cell_text_position(&harness, "タスクを").unwrap().1;
    assert!(
        new_task_row < search_row,
        "CJK New Task button and search field must occupy separate toolbar rows"
    );
}
