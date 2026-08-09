//! The Orchestrator Open dialog defaults to showing every project's
//! sessions, and the scope toggle (Alt+P) narrows to the current
//! project. The Project control + Alt+P switch between the two, and
//! the choice is remembered across opens (module state).
//!
//! Sessions are inherently cross-project — each row can carry its own
//! `project_path`. The picker foregrounds all of them by default; the
//! current-project scope is one keystroke away for when project A's
//! history is in the way.

use crate::common::harness::EditorTestHarness;
use crossterm::event::{KeyCode, KeyModifiers};
use fresh_core::api::{
    PluginCommand, PluginCommandContext, PluginCommandEnvelope, PluginInstanceId,
};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

const WIDTH: u16 = 160;
const HEIGHT: u16 = 40;

const LABEL_B: &str = "zebra-beta-xr";

fn run_palette(harness: &mut EditorTestHarness, command_name: &str) {
    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.render().unwrap();
    harness.type_text(command_name).unwrap();
    harness.render().unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    harness.render().unwrap();
}

fn set_orch_project_path(harness: &mut EditorTestHarness, project_path: &Path) {
    let window_id = harness.editor().active_window_id();
    harness
        .editor_mut()
        .dispatch_plugin_command_envelope(PluginCommandEnvelope::new(
            PluginCommand::SetWindowState {
                window_id,
                key: "project_path".into(),
                value: Some(Value::String(project_path.to_string_lossy().into_owned())),
            },
            PluginCommandContext {
                plugin_name: Arc::from("orchestrator"),
                plugin_instance_id: PluginInstanceId::fresh(),
                source_window: Some(window_id),
                ..PluginCommandContext::default()
            },
        ));
}

#[test]
fn open_dialog_defaults_to_all_projects_then_scopes_to_current() {
    let _locale = crate::common::locale_lock::lock_locale();
    let mut harness = EditorTestHarness::with_temp_project(WIDTH, HEIGHT).unwrap();

    // Project A: the harness's temp project root, owned by the base
    // window (id 1, active at boot).
    let proj_a = harness.project_dir().unwrap().canonicalize().unwrap();
    set_orch_project_path(&mut harness, &proj_a);

    // Project B: a separate tempdir, owned by a second window we create
    // explicitly. Make B active for the picker, then tag that exact captured
    // window through the loader-attributed command envelope.
    let proj_b_dir = tempfile::TempDir::new().unwrap();
    let proj_b = proj_b_dir.path().canonicalize().unwrap();
    let win_b = harness
        .editor_mut()
        .create_window_at(proj_b.clone(), LABEL_B.into());
    harness.editor_mut().set_active_window(win_b);
    set_orch_project_path(&mut harness, &proj_b);

    harness.render().unwrap();

    run_palette(&mut harness, "Orchestrator: Open");
    // The scope now reads off the "Project:" control, not a title suffix
    // (the dialog title is native modal-frame chrome). The control shows
    // "All ▾" in the all-projects view and the current project's basename
    // otherwise, so the "All ▾" marker (with its ▾ glyph, unique to the
    // control) distinguishes the two views — unlike the footer hint, which
    // renders the bare phrase "all projects" as the Alt+P toggle target
    // whenever the scope is "current".
    harness
        .wait_until(|h| h.screen_to_string().contains("All ▾"))
        .expect("Orchestrator Open dialog should default to the all-projects view");

    // Default scope is "all": every session is listed and the Project
    // control shows "All".
    let screen = harness.screen_to_string();
    let scope_label = harness
        .editor()
        .plugin_manager()
        .state_snapshot_handle()
        .and_then(|snapshot| {
            snapshot
                .read()
                .ok()?
                .keybinding_labels
                .get("orchestrator_toggle_scope\0orchestrator-open")
                .cloned()
        })
        .expect("Orchestrator scope binding label must be published");
    let scope_hint = format!("({scope_label})");
    assert!(
        screen.contains("Project:") && screen.contains(&scope_hint),
        "Picker must render the visible Project scope control with its \
         live shortcut hint {scope_hint}.\nScreen:\n{}",
        screen,
    );
    assert!(
        screen.contains("All ▾"),
        "Default view must show the Project control in its 'All' state.\nScreen:\n{}",
        screen,
    );
    assert!(
        screen.contains(LABEL_B),
        "Project B's session must be listed in the all-projects view.\nScreen:\n{}",
        screen,
    );

    // Toggle scope (Alt+P) → current project only (Project B, the
    // active window). B's session stays; the title drops "all
    // projects".
    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::ALT)
        .unwrap();
    harness.render().unwrap();
    // The "Project:" control drops "All ▾" once scoped to the current
    // project (it switches to the project's basename). The footer hint keeps
    // the bare "all projects" affordance, so we key off the control's "All ▾"
    // marker, not the bare phrase.
    harness
        .wait_until(|h| !h.screen_to_string().contains("All ▾"))
        .expect("scope toggle should switch the dialog to the current-project view");

    let screen = harness.screen_to_string();
    assert!(
        screen.contains(LABEL_B),
        "Project B's session must remain listed when scoped to its own \
         project.\nScreen:\n{}",
        screen,
    );

    // `/` focuses the filter; typing then narrows the list live. A
    // query that matches nothing must filter LABEL_B out — proving
    // both that `/` moved focus to the filter and that printable
    // input reaches it.
    harness
        .send_key(KeyCode::Char('/'), KeyModifiers::NONE)
        .unwrap();
    harness.render().unwrap();
    for ch in "qzqz".chars() {
        harness
            .send_key(KeyCode::Char(ch), KeyModifiers::NONE)
            .unwrap();
        harness.render().unwrap();
    }
    let screen = harness.screen_to_string();
    assert!(
        !screen.contains(LABEL_B),
        "Typing a non-matching filter (after `/` focuses it) must hide \
         unmatched sessions.\nScreen:\n{}",
        screen,
    );
}
