//! A failed Create & Visit transaction must return to the window that was
//! active immediately before the new workspace, not the oldest window.
//!
//! Single test in this binary: XDG data is process-global.
#![cfg(all(target_os = "linux", feature = "plugins"))]

mod common;

use common::dormant_ssh::isolated_dir_context;
use common::fail_retirement_write_fs::FailRetirementWriteFs;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;

#[test]
fn failed_create_and_visit_restores_immediate_predecessor() {
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let launch_root = base.path().join("launch");
    let predecessor_root = base.path().join("predecessor");
    let target_root = base.path().join("visit-target");
    std::fs::create_dir_all(&launch_root).unwrap();
    std::fs::create_dir_all(&predecessor_root).unwrap();
    std::fs::create_dir_all(&target_root).unwrap();
    let launch_root = launch_root.canonicalize().unwrap();
    let predecessor_root = predecessor_root.canonicalize().unwrap();
    let target_root = target_root.canonicalize().unwrap();
    let target = target_root.to_string_lossy().to_string();

    let plugins = launch_root.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");

    let state_dir = dir_context.data_dir.join("orchestrator/state");
    let fault_fs = Arc::new(FailRetirementWriteFs::new(state_dir, target.clone()));
    let mut harness = EditorTestHarness::create(
        160,
        50,
        HarnessOptions::new()
            .with_working_dir(launch_root)
            .with_shared_dir_context(dir_context)
            .with_filesystem(fault_fs.clone()),
    )
    .unwrap();
    harness.tick_and_render().unwrap();
    harness
        .wait_until(|h| {
            h.editor()
                .command_registry()
                .read()
                .unwrap()
                .get_all()
                .iter()
                .any(|command| command.get_localized_name() == "Orchestrator: New Workspace")
        })
        .unwrap();

    let oldest_id = harness.editor().active_window_id();
    let predecessor_id = harness
        .editor_mut()
        .create_window_at(predecessor_root, "predecessor".to_string());
    assert_ne!(oldest_id, predecessor_id);
    harness.editor_mut().set_active_window(predecessor_id);
    assert_eq!(harness.editor().active_window_id(), predecessor_id);
    harness.tick_and_render().unwrap();

    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness.type_text("Orchestrator: New Workspace").unwrap();
    harness
        .wait_until(|h| h.screen_to_string().contains("Orchestrator: New Workspace"))
        .unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    harness
        .wait_until(|h| {
            h.screen_to_string()
                .contains("ORCHESTRATOR :: New Workspace")
        })
        .unwrap();
    harness.type_text(&target).unwrap();
    harness
        .wait_until(|h| h.screen_to_string().contains("visit-target"))
        .unwrap();

    // The fault matches the publication that removes the committed journal
    // while retaining this target's auto-named pending record. That retirement
    // runs only after Create & Visit has activated the new window.
    fault_fs.arm();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();
    harness
        .wait_until(|h| {
            h.editor().session_count() == 2
                && h.editor()
                    .plugin_global_state()
                    .get("orchestrator")
                    .is_some_and(|state| {
                        state.values().any(|value| {
                            value.get("phase").and_then(serde_json::Value::as_str) == Some("error")
                                && value
                                    .get("spec")
                                    .and_then(|spec| spec.get("projectPath"))
                                    .and_then(serde_json::Value::as_str)
                                    == Some(target.as_str())
                        })
                    })
        })
        .unwrap();

    assert_eq!(
        harness.editor().active_window_id(),
        predecessor_id,
        "rollback selected the oldest window instead of the immediate predecessor"
    );
    assert_ne!(harness.editor().active_window_id(), oldest_id);
}
