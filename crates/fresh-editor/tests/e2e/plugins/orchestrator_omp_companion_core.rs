//! Core regression coverage for the Orchestrator Fresh–OMP companion contract.
//!
//! These tests inject Fresh-validated snapshots into the real bundled plugin.
//! They deliberately assert the state machine boundary rather than the native
//! OMP UI: marker/provisional launch, receipt-time liveness, sequence and exit
//! identity fencing, and exact resume replacement.

#![cfg(all(feature = "plugins", unix))]

use crate::common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness};
use crossterm::event::{KeyCode, KeyModifiers};
use fresh_core::api::{PluginCommand, TerminalCompanion};
use fresh_core::hooks::{HookArgs, OmpCompanionSnapshotV1, OmpCompanionState};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct PathRestore(String);

impl Drop for PathRestore {
    fn drop(&mut self) {
        std::env::set_var("PATH", &self.0);
    }
}

fn set_up_workspace() -> (tempfile::TempDir, PathBuf, PathRestore) {
    fresh::i18n::set_locale("en");
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().canonicalize().unwrap();
    let plugins = workspace.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");

    let bin = workspace.join("bin");
    fs::create_dir(&bin).unwrap();
    let omp = bin.join("omp");
    fs::write(&omp, "#!/bin/sh\nwhile :; do sleep 60; done\n").unwrap();
    fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).unwrap();
    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old_path}", bin.display()));

    (temp, workspace, PathRestore(old_path))
}

fn pump_until(
    harness: &mut EditorTestHarness,
    max_ticks: usize,
    mut ready: impl FnMut(&EditorTestHarness) -> bool,
) {
    for _ in 0..max_ticks {
        harness.process_async_and_render().unwrap();
        if ready(harness) {
            return;
        }
        harness.sleep(Duration::from_millis(25));
    }
    panic!(
        "condition did not become true; screen:\n{}",
        harness.screen_to_string()
    );
}

fn open_new_session_form(harness: &mut EditorTestHarness) {
    pump_until(harness, 40, |h| {
        h.editor()
            .command_registry()
            .read()
            .unwrap()
            .get_all()
            .iter()
            .any(|command| command.get_localized_name() == "Orchestrator: New Workspace")
    });
    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness.type_text("Orchestrator: New Workspace").unwrap();
    pump_until(harness, 40, |h| {
        h.screen_to_string().contains("Orchestrator: New Workspace")
    });
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    pump_until(harness, 40, |h| {
        h.screen_to_string()
            .contains("ORCHESTRATOR :: New Workspace")
    });
}

fn focus_omp_preset(harness: &mut EditorTestHarness) {
    let mut steps = 0;
    while !harness
        .screen_to_string()
        .lines()
        .any(|line| line.contains('▸') && line.contains("Agent:"))
    {
        harness.send_key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        harness.tick_and_render().unwrap();
        steps += 1;
        assert!(steps < 20, "Agent preset was never focused");
    }
    while !harness
        .screen_to_string()
        .lines()
        .any(|line| line.contains('▸') && line.contains("omp"))
    {
        harness
            .send_key(KeyCode::Right, KeyModifiers::NONE)
            .unwrap();
        harness.tick_and_render().unwrap();
        steps += 1;
        assert!(steps < 30, "OMP preset was never selected");
    }
}

fn spawn_omp_session(harness: &mut EditorTestHarness, workspace: &PathBuf) {
    let initial_window = harness.editor().active_window_id();
    open_new_session_form(harness);
    harness.type_text(&workspace.to_string_lossy()).unwrap();
    focus_omp_preset(harness);
    harness
        .send_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();
    pump_until(harness, 160, |h| {
        h.editor().active_window_id() != initial_window
    });
}

fn companion_snapshot(
    incarnation: &str,
    sequence: u64,
    session_id: &str,
    session_name: &str,
    state: OmpCompanionState,
) -> OmpCompanionSnapshotV1 {
    OmpCompanionSnapshotV1 {
        version: 1,
        incarnation: incarnation.into(),
        sequence,
        session_generation: 1,
        timestamp_ms: 1,
        omp_version: "0.19.0".into(),
        process_id: 42,
        session_id: session_id.into(),
        session_name: Some(session_name.into()),
        cwd: "/workspace".into(),
        state,
        status_text: None,
        model: None,
        thinking_level: None,
        running_tools: 0,
        current_tool: None,
        goal: None,
        todos: None,
        context: None,
        pending_approvals: 0,
        async_jobs: None,
    }
}

fn emit_snapshot(
    harness: &EditorTestHarness,
    window_id: u64,
    terminal_id: u64,
    received_at_ms: u64,
    snapshot: OmpCompanionSnapshotV1,
) {
    harness.editor().plugin_manager().run_hook(
        "omp_companion_snapshot",
        HookArgs::OmpCompanionSnapshot {
            window_id,
            terminal_id,
            received_at_ms,
            launch_executable: "omp".into(),
            snapshot,
        },
    );
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn open_details(harness: &mut EditorTestHarness) {
    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness.type_text("Orchestrator: Open").unwrap();
    pump_until(harness, 40, |h| {
        h.screen_to_string().contains("Orchestrator: Open")
    });
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    pump_until(harness, 40, |h| {
        h.screen_to_string().contains("ORCHESTRATOR :: Workspaces")
    });
    let (column, row) = harness
        .find_text_on_screen("Details")
        .expect("picker must expose the Details toggle");
    harness.mouse_click(column, row).unwrap();
}

fn active_terminal(harness: &EditorTestHarness) -> (u64, u64) {
    let window_id = harness.editor().active_window_id();
    let terminal_id = harness
        .editor()
        .active_window()
        .terminal_buffers
        .values()
        .next()
        .expect("OMP window must have its spawned terminal")
        .terminal_id;
    (window_id.0, terminal_id.0 as u64)
}
fn create_omp_terminal(
    harness: &mut EditorTestHarness,
    window_id: fresh_core::WindowId,
    request_id: u64,
    title: &str,
    excluded_terminal_ids: &[u64],
) -> u64 {
    harness
        .editor_mut()
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: Some(false),
            persistent: true,
            window_id: Some(window_id),
            command: Some(vec!["omp".into()]),
            title: Some(title.into()),
            resume: None,
            env: None,
            allow_script: false,
            request_id,
        })
        .unwrap();
    pump_until(harness, 40, |h| {
        h.editor().session(window_id).is_some_and(|window| {
            window
                .terminal_buffers
                .values()
                .any(|terminal| !excluded_terminal_ids.contains(&(terminal.terminal_id.0 as u64)))
        })
    });
    harness
        .editor()
        .session(window_id)
        .unwrap()
        .terminal_buffers
        .values()
        .find(|terminal| !excluded_terminal_ids.contains(&(terminal.terminal_id.0 as u64)))
        .expect("the replacement PTY must be available")
        .terminal_id
        .0 as u64
}

#[test]
fn omp_preset_marks_the_terminal_and_replaces_provisional_resume_exactly() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    spawn_omp_session(&mut harness, &workspace);
    let (window_id, terminal_id) = active_terminal(&harness);
    let terminal_id = fresh_core::TerminalId(terminal_id as usize);
    let window_id_typed = fresh_core::WindowId(window_id);

    let window = harness.editor().session(window_id_typed).unwrap();
    assert_eq!(
        window.terminal_companions.get(&terminal_id),
        Some(&TerminalCompanion::Omp),
        "the OMP preset must request the persisted OMP companion marker",
    );
    assert_eq!(
        window.terminal_resume_commands.get(&terminal_id),
        Some(&vec!["omp".into(), "--continue".into()]),
        "OMP starts with the provisional documented resume argv",
    );
    assert!(
        !window.terminal_commands[&terminal_id]
            .iter()
            .any(|arg| arg == "--no-session"),
        "the OMP preset must never disable OMP session persistence",
    );

    let serialized_workspace = harness.editor().capture_workspace();
    assert!(
        serialized_workspace
            .terminals
            .iter()
            .any(|terminal| terminal.companion == Some(TerminalCompanion::Omp)),
        "the active initial companion marker must survive workspace serialization",
    );

    let session_id = "00000000-0000-4000-8000-000000000001";
    emit_snapshot(
        &harness,
        window_id,
        terminal_id.0 as u64,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174000",
            1,
            session_id,
            "resume-target",
            OmpCompanionState::Idle,
        ),
    );
    pump_until(&mut harness, 80, |h| {
        h.editor()
            .session(window_id_typed)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id))
            .is_some_and(|argv| argv == &["omp", "--resume", session_id])
    });

    let next_session_id = "00000000-0000-4000-8000-000000000002";
    emit_snapshot(
        &harness,
        window_id,
        terminal_id.0 as u64,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174000",
            2,
            next_session_id,
            "resume-target-next",
            OmpCompanionState::Idle,
        ),
    );
    // A newer heartbeat/session replaces the exact argv; an older async
    // callback must never leave the previous session ID behind.
    pump_until(&mut harness, 80, |h| {
        h.editor()
            .session(window_id_typed)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id))
            .is_some_and(|argv| argv == &["omp", "--resume", next_session_id])
    });
    harness.assert_no_plugin_errors();
}

#[test]
fn unsupported_initial_omp_terminal_keeps_marker_without_live_companion() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();

    // A shell wrapper deliberately makes this initial OMP request unsupported.
    // It still exercises the production createWindowWithTerminal path, where
    // the marker must not be inferred from the absent live companion handle.
    let command = vec![
        "sh".into(),
        "-c".into(),
        "while :; do sleep 60; done".into(),
    ];
    let authority = harness.editor().local_session_authority(&workspace);
    let (window_id, terminal_id, _) = harness
        .editor_mut()
        .create_window_with_terminal(
            workspace.clone(),
            "unsupported-omp".into(),
            Some(workspace),
            Some(command),
            Some("unsupported omp".into()),
            authority,
            None,
            None,
            false,
            Some(TerminalCompanion::Omp),
        )
        .expect("unsupported OMP terminal should still spawn as an ordinary terminal");

    let window = harness
        .editor()
        .session(window_id)
        .expect("new session window must remain available");
    assert_eq!(
        window.terminal_companions.get(&terminal_id),
        Some(&TerminalCompanion::Omp),
        "unsupported initial creation keeps the descriptive OMP marker",
    );
    assert_eq!(
        window
            .terminal_manager
            .get(terminal_id)
            .and_then(|handle| handle.companion_kind()),
        None,
        "unsupported initial creation has no live companion to infer from",
    );

    let serialized_workspace = harness.editor().capture_workspace();
    assert_eq!(
        serialized_workspace.terminals.len(),
        1,
        "the initial agent terminal should be captured exactly once",
    );
    assert_eq!(
        serialized_workspace.terminals[0].companion,
        Some(TerminalCompanion::Omp),
        "the unsupported initial marker must survive workspace serialization",
    );
}

#[test]
fn first_companion_snapshot_reconciles_unopened_session() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace).unwrap();
    harness.tick_and_render().unwrap();

    let window_id = harness.editor().active_window_id();
    harness
        .editor_mut()
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: Some(false),
            persistent: true,
            window_id: Some(window_id),
            command: Some(vec!["omp".into()]),
            title: Some("omp".into()),
            resume: None,
            env: None,
            allow_script: false,
            request_id: 9000,
        })
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        !h.editor().active_window().terminal_buffers.is_empty()
    });
    let (_, terminal_id) = active_terminal(&harness);
    let terminal_id_typed = fresh_core::TerminalId(terminal_id as usize);
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_companions
        .insert(terminal_id_typed, TerminalCompanion::Omp);
    let session_id = "00000000-0000-4000-8000-000000000008";

    emit_snapshot(
        &harness,
        window_id.0,
        terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174008",
            1,
            session_id,
            "ready-companion",
            OmpCompanionState::Idle,
        ),
    );
    pump_until(&mut harness, 80, |h| {
        h.editor()
            .session(window_id)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id_typed))
            .is_some_and(|argv| argv == &["omp", "--resume", session_id])
    });
    harness.assert_no_plugin_errors();
}

#[test]
fn reconciled_session_claims_its_authenticated_companion_terminal() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace).unwrap();
    harness.tick_and_render().unwrap();
    open_details(&mut harness);

    let window_id = harness.editor().active_window_id();
    harness
        .editor_mut()
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: Some(false),
            persistent: true,
            window_id: Some(window_id),
            command: Some(vec!["omp".into()]),
            title: Some("omp".into()),
            resume: None,
            env: None,
            allow_script: false,
            request_id: 9001,
        })
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        !h.editor().active_window().terminal_buffers.is_empty()
    });
    let (_, terminal_id) = active_terminal(&harness);
    let terminal_id_typed = fresh_core::TerminalId(terminal_id as usize);
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_companions
        .insert(terminal_id_typed, TerminalCompanion::Omp);
    let session_id = "00000000-0000-4000-8000-000000000009";

    emit_snapshot(
        &harness,
        window_id.0,
        terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174009",
            1,
            session_id,
            "restored-companion",
            OmpCompanionState::Idle,
        ),
    );
    pump_until(&mut harness, 80, |h| {
        h.screen_to_string().contains("OMP connected")
            && h.screen_to_string().contains("restored-companion")
    });
    assert_eq!(
        harness
            .editor()
            .session(window_id)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id_typed)),
        Some(&vec!["omp".into(), "--resume".into(), session_id.into()]),
        "the authenticated snapshot must claim the reconciled terminal and replace its resume argv",
    );
    harness.assert_no_plugin_errors();
}

#[test]
fn reconciled_terminal_exit_before_first_snapshot_fences_delayed_companion_hook() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace).unwrap();
    harness.tick_and_render().unwrap();
    open_details(&mut harness);

    // Reconciliation knows the window but cannot learn its terminal id. The
    // exit must still fence the exact host identity before a snapshot claims it.
    let window_id = harness.editor().active_window_id();
    harness
        .editor_mut()
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: Some(false),
            persistent: true,
            window_id: Some(window_id),
            command: Some(vec!["omp".into()]),
            title: Some("omp".into()),
            resume: None,
            env: None,
            allow_script: false,
            request_id: 9002,
        })
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        !h.editor().active_window().terminal_buffers.is_empty()
    });
    let (_, terminal_id) = active_terminal(&harness);
    let terminal_id_typed = fresh_core::TerminalId(terminal_id as usize);
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_companions
        .insert(terminal_id_typed, TerminalCompanion::Omp);

    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id,
            window_id: window_id.0,
            exit_code: None,
        },
    );
    // A later unrelated exit cannot replace A while reconciliation still
    // lacks a claimed terminal identity.
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: terminal_id + 1,
            window_id: window_id.0,
            exit_code: None,
        },
    );
    emit_snapshot(
        &harness,
        window_id.0,
        terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174010",
            1,
            "00000000-0000-4000-8000-000000000010",
            "late-first-snapshot",
            OmpCompanionState::Idle,
        ),
    );
    for _ in 0..10 {
        harness.process_async_and_render().unwrap();
    }

    assert!(
        !harness.screen_to_string().contains("late-first-snapshot"),
        "an exit before terminal reconciliation must fence a delayed authenticated snapshot",
    );
    assert!(
        harness
            .editor()
            .session(window_id)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id_typed))
            .is_none(),
        "a fenced snapshot must not establish a resume command",
    );
    harness.assert_no_plugin_errors();
}

#[test]
fn receipt_liveness_and_sequence_incarnation_tombstones_fence_companion_state() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    spawn_omp_session(&mut harness, &workspace);
    let (window_id, terminal_id) = active_terminal(&harness);

    // Receipt time, not the emitter timestamp, makes an old delivery stale
    // immediately and leaves the last-known facet visibly disconnected.
    emit_snapshot(
        &harness,
        window_id,
        terminal_id,
        0,
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174000",
            2,
            "00000000-0000-4000-8000-000000000001",
            "old-receipt",
            OmpCompanionState::Working,
        ),
    );
    open_details(&mut harness);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("OMP disconnected")
    });

    // Within one incarnation lower/equal sequence numbers are ignored, while
    // a new incarnation starts a fresh monotonic sequence space.
    emit_snapshot(
        &harness,
        window_id,
        terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174000",
            1,
            "00000000-0000-4000-8000-000000000002",
            "ignored-sequence",
            OmpCompanionState::Idle,
        ),
    );
    emit_snapshot(
        &harness,
        window_id,
        terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174001",
            1,
            "00000000-0000-4000-8000-000000000003",
            "new-incarnation",
            OmpCompanionState::Idle,
        ),
    );
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("new-incarnation")
    });
    let after_new_incarnation = harness.screen_to_string();
    assert!(
        !after_new_incarnation.contains("ignored-sequence"),
        "a stale sequence must not replace the last facet before a new incarnation arrives:\n{after_new_incarnation}",
    );
    // Terminal ids are scoped to their window. A host exit for the same
    // numeric terminal in another window must leave this session untouched.
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id,
            window_id: window_id + 1_000,
            exit_code: None,
        },
    );
    emit_snapshot(
        &harness,
        window_id,
        terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174001",
            2,
            "00000000-0000-4000-8000-000000000003",
            "survives-wrong-window-exit",
            OmpCompanionState::Idle,
        ),
    );
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("survives-wrong-window-exit")
    });

    // An exit tombstone applies to the exact window/terminal identity. A late
    // queued hook cannot revive the facet, even if its incarnation is new.
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id,
            window_id,
            exit_code: None,
        },
    );
    // A non-companion terminal in the same window may exit later. It must
    // neither replace A's one-entry tombstone nor let A's delayed hook live.
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: terminal_id + 1,
            window_id,
            exit_code: None,
        },
    );
    emit_snapshot(
        &harness,
        window_id,
        terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174002",
            1,
            "00000000-0000-4000-8000-000000000004",
            "late-after-exit",
            OmpCompanionState::Working,
        ),
    );
    for _ in 0..10 {
        harness.process_async_and_render().unwrap();
    }
    assert!(
        !harness.screen_to_string().contains("late-after-exit"),
        "an exact terminal-exit tombstone must reject delayed companion hooks",
    );
    harness.assert_no_plugin_errors();
}

#[test]
fn replacement_omp_terminal_epochs_retain_exact_exit_tombstones() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace).unwrap();
    harness.tick_and_render().unwrap();
    // The reconciled base window has no claimed terminal. Create A solely to
    // deliver its host exit before any companion snapshot can claim it.
    let window_id_typed = harness.editor().active_window_id();
    let window_id = window_id_typed.0;
    let a_terminal_id =
        create_omp_terminal(&mut harness, window_id_typed, 9003, "preclaim omp", &[]);
    let a_terminal_id_typed = fresh_core::TerminalId(a_terminal_id as usize);
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_companions
        .insert(a_terminal_id_typed, TerminalCompanion::Omp);
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: a_terminal_id,
            window_id,
            exit_code: None,
        },
    );

    // B claims the still-unreconciled window. Its accepted snapshot must keep
    // A's exact tombstone while establishing the live replacement identity.
    let b_terminal_id = create_omp_terminal(
        &mut harness,
        window_id_typed,
        9004,
        "replacement omp",
        &[a_terminal_id],
    );
    let b_terminal_id_typed = fresh_core::TerminalId(b_terminal_id as usize);
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_companions
        .insert(b_terminal_id_typed, TerminalCompanion::Omp);
    let b_session_id = "00000000-0000-4000-8000-000000000011";
    emit_snapshot(
        &harness,
        window_id,
        b_terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174011",
            1,
            b_session_id,
            "replacement-b",
            OmpCompanionState::Idle,
        ),
    );
    pump_until(&mut harness, 80, |h| {
        h.editor()
            .session(window_id_typed)
            .and_then(|window| window.terminal_resume_commands.get(&b_terminal_id_typed))
            .is_some_and(|argv| argv == &["omp", "--resume", b_session_id])
    });
    open_details(&mut harness);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("replacement-b")
    });

    // B's exit opens another unreconciled epoch. C exits before claiming it,
    // and C's exact tombstone must be retained.
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: b_terminal_id,
            window_id,
            exit_code: None,
        },
    );
    let c_terminal_id = create_omp_terminal(
        &mut harness,
        window_id_typed,
        9005,
        "preclaim replacement omp",
        &[a_terminal_id, b_terminal_id],
    );
    let c_terminal_id_typed = fresh_core::TerminalId(c_terminal_id as usize);
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_companions
        .insert(c_terminal_id_typed, TerminalCompanion::Omp);
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: c_terminal_id,
            window_id,
            exit_code: None,
        },
    );

    // D also exits before any replacement snapshot claims the window and must
    // receive its own exact tombstone.
    let d_terminal_id = create_omp_terminal(
        &mut harness,
        window_id_typed,
        9006,
        "second preclaim replacement omp",
        &[a_terminal_id, b_terminal_id, c_terminal_id],
    );
    let d_terminal_id_typed = fresh_core::TerminalId(d_terminal_id as usize);
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_companions
        .insert(d_terminal_id_typed, TerminalCompanion::Omp);
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: d_terminal_id,
            window_id,
            exit_code: None,
        },
    );

    // A, B, C, and D are all dead exact identities. Delayed snapshots for each
    // must be rejected, proving every unreconciled exit is retained.
    emit_snapshot(
        &harness,
        window_id,
        a_terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174012",
            1,
            "00000000-0000-4000-8000-000000000012",
            "delayed-a",
            OmpCompanionState::Working,
        ),
    );
    emit_snapshot(
        &harness,
        window_id,
        b_terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174013",
            1,
            "00000000-0000-4000-8000-000000000013",
            "delayed-b",
            OmpCompanionState::Working,
        ),
    );
    emit_snapshot(
        &harness,
        window_id,
        c_terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174014",
            1,
            "00000000-0000-4000-8000-000000000014",
            "delayed-c",
            OmpCompanionState::Working,
        ),
    );
    emit_snapshot(
        &harness,
        window_id,
        d_terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174015",
            1,
            "00000000-0000-4000-8000-000000000015",
            "delayed-d",
            OmpCompanionState::Working,
        ),
    );
    for _ in 0..10 {
        harness.process_async_and_render().unwrap();
    }
    let screen = harness.screen_to_string();
    assert!(
        !screen.contains("delayed-a")
            && !screen.contains("delayed-b")
            && !screen.contains("delayed-c")
            && !screen.contains("delayed-d"),
        "every exited terminal identity must continue fencing delayed snapshots",
    );

    // E remains live and claims normally despite the four cumulative exit
    // tombstones.
    let e_terminal_id = create_omp_terminal(
        &mut harness,
        window_id_typed,
        9007,
        "live replacement omp",
        &[a_terminal_id, b_terminal_id, c_terminal_id, d_terminal_id],
    );
    let e_terminal_id_typed = fresh_core::TerminalId(e_terminal_id as usize);
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_companions
        .insert(e_terminal_id_typed, TerminalCompanion::Omp);
    let e_session_id = "00000000-0000-4000-8000-000000000016";
    emit_snapshot(
        &harness,
        window_id,
        e_terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174016",
            1,
            e_session_id,
            "live-e",
            OmpCompanionState::Idle,
        ),
    );
    pump_until(&mut harness, 80, |h| {
        h.editor()
            .session(window_id_typed)
            .and_then(|window| window.terminal_resume_commands.get(&e_terminal_id_typed))
            .is_some_and(|argv| argv == &["omp", "--resume", e_session_id])
    });
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("live-e")
    });
    harness.assert_no_plugin_errors();
}
