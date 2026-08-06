//! End-to-end coverage for the Orchestrator's native Fresh–OMP companion UI.
//!
//! The authenticated OSC ingress is covered by its native tests. This module
//! deliberately injects the already-validated hook payload into the real
//! Orchestrator plugin, then drives the rendered picker and its Interrupt
//! action. It pins the presentation boundary: structured snapshot metadata is
//! shown, while a refused cancel marks the facet stale without killing the PTY.

#![cfg(all(feature = "plugins", unix))]

use crate::common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness};
use crossterm::event::{KeyCode, KeyModifiers};
use fresh_core::hooks::{
    HookArgs, OmpCompanionAsyncJobs, OmpCompanionContext, OmpCompanionCurrentTool,
    OmpCompanionGoal, OmpCompanionGoalStatus, OmpCompanionModel, OmpCompanionSnapshotV1,
    OmpCompanionState, OmpCompanionThinkingLevel, OmpCompanionTodos,
};
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

    // The OMP launch preset deliberately requires the direct `omp` executable.
    // Keep a PTY alive long enough to exercise the same companion path without
    // depending on an installed real OMP binary.
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

fn snapshot() -> OmpCompanionSnapshotV1 {
    OmpCompanionSnapshotV1 {
        version: 1,
        incarnation: "123e4567-e89b-42d3-a456-426614174000".into(),
        sequence: 1,
        session_generation: 1,
        timestamp_ms: 1,
        omp_version: "0.19.0".into(),
        process_id: 42,
        session_id: "00000000-0000-4000-8000-000000000001".into(),
        session_name: Some("companion-session".into()),
        cwd: "/workspace".into(),
        state: OmpCompanionState::AwaitingApproval,
        model: Some(OmpCompanionModel {
            provider: "openai".into(),
            id: "gpt-5".into(),
        }),
        thinking_level: Some(OmpCompanionThinkingLevel::High),
        running_tools: 2,
        current_tool: Some(OmpCompanionCurrentTool {
            name: "shell".into(),
            intent: Some("build project".into()),
        }),
        goal: Some(OmpCompanionGoal {
            objective: "finish companion UI".into(),
            status: OmpCompanionGoalStatus::Active,
        }),
        todos: Some(OmpCompanionTodos {
            pending: 3,
            in_progress: 1,
            blocked: 0,
            completed: 4,
            abandoned: 0,
            current: Some("render details".into()),
        }),
        context: Some(OmpCompanionContext {
            tokens: 4096,
            context_window: 12288,
            percent_bps: 3333,
        }),
        pending_approvals: 1,
        async_jobs: Some(OmpCompanionAsyncJobs {
            running: 1,
            recent_failures: 0,
            pending_delivery: 2,
        }),
    }
}

fn spawn_omp_session(
    harness: &mut EditorTestHarness,
    workspace: &PathBuf,
) -> (fresh_core::WindowId, fresh_core::TerminalId) {
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
    let window_id = harness.editor().active_window_id();
    let terminal_id = harness
        .editor()
        .active_window()
        .terminal_buffers
        .values()
        .next()
        .expect("OMP window must have its spawned terminal")
        .terminal_id;
    (window_id, terminal_id)
}

fn emit_companion_snapshot(
    harness: &EditorTestHarness,
    window_id: fresh_core::WindowId,
    terminal_id: fresh_core::TerminalId,
    snapshot: OmpCompanionSnapshotV1,
) {
    let received_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    harness.editor().plugin_manager().run_hook(
        "omp_companion_snapshot",
        HookArgs::OmpCompanionSnapshot {
            window_id: window_id.0,
            terminal_id: terminal_id.0 as u64,
            received_at_ms,
            launch_executable: "omp".into(),
            snapshot,
        },
    );
}

#[test]
fn companion_snapshot_notifies_background_approval() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let initial_window = harness.editor().active_window_id();
    let (companion_window, terminal_id) = spawn_omp_session(&mut harness, &workspace);

    harness.editor_mut().set_active_window(initial_window);
    emit_companion_snapshot(&harness, companion_window, terminal_id, snapshot());
    pump_until(&mut harness, 40, |h| {
        h.editor()
            .get_status_message()
            .is_some_and(|message| message.contains("needs approval"))
    });
    harness.assert_no_plugin_errors();
}

#[test]
fn companion_approval_attention_requires_a_zero_to_positive_edge() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let initial_window = harness.editor().active_window_id();
    let (companion_window, terminal_id) = spawn_omp_session(&mut harness, &workspace);

    // Establish the pending approval while this session is active, which must
    // not request background attention.
    emit_companion_snapshot(&harness, companion_window, terminal_id, snapshot());
    pump_until(&mut harness, 40, |h| {
        h.editor()
            .session(companion_window)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id))
            .is_some_and(|argv| {
                argv == &["omp", "--resume", "00000000-0000-4000-8000-000000000001"]
            })
    });
    let status_before = harness.editor().get_status_message().cloned();
    harness.editor_mut().set_active_window(initial_window);

    let mut generation_changed = snapshot();
    generation_changed.sequence = 2;
    generation_changed.session_generation = 2;
    generation_changed.session_id = "00000000-0000-4000-8000-000000000002".into();
    emit_companion_snapshot(&harness, companion_window, terminal_id, generation_changed);
    pump_until(&mut harness, 40, |h| {
        h.editor()
            .session(companion_window)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id))
            .is_some_and(|argv| {
                argv == &["omp", "--resume", "00000000-0000-4000-8000-000000000002"]
            })
    });
    assert_eq!(
        harness.editor().get_status_message().cloned(),
        status_before,
        "a generation change with pending approvals still at one must not notify",
    );

    let mut incarnation_changed = snapshot();
    incarnation_changed.incarnation = "123e4567-e89b-42d3-a456-426614174001".into();
    incarnation_changed.session_generation = 2;
    incarnation_changed.session_id = "00000000-0000-4000-8000-000000000003".into();
    emit_companion_snapshot(&harness, companion_window, terminal_id, incarnation_changed);
    pump_until(&mut harness, 40, |h| {
        h.editor()
            .session(companion_window)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id))
            .is_some_and(|argv| {
                argv == &["omp", "--resume", "00000000-0000-4000-8000-000000000003"]
            })
    });
    assert_eq!(
        harness.editor().get_status_message().cloned(),
        status_before,
        "an incarnation change with pending approvals still at one must not notify",
    );
    harness.assert_no_plugin_errors();
}

#[test]
fn companion_snapshot_renders_and_refused_interrupt_only_stales_the_facet() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let (companion_window, terminal_id) = spawn_omp_session(&mut harness, &workspace);

    // Keep the PTY alive while forcing the exact cancel request to resolve
    // false, proving Interrupt never degrades into a terminal kill.
    harness
        .editor_mut()
        .active_window_mut()
        .terminal_manager
        .get(terminal_id)
        .expect("spawned OMP terminal remains addressable")
        .revoke_omp_companion();
    emit_companion_snapshot(&harness, companion_window, terminal_id, snapshot());
    pump_until(&mut harness, 40, |h| {
        h.editor()
            .session(companion_window)
            .and_then(|window| window.terminal_resume_commands.get(&terminal_id))
            .is_some_and(|argv| {
                argv == &["omp", "--resume", "00000000-0000-4000-8000-000000000001"]
            })
    });

    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("Awaiting approval")
    });

    let mut working = snapshot();
    working.sequence = 2;
    working.state = OmpCompanionState::Working;
    working.pending_approvals = 0;
    emit_companion_snapshot(&harness, companion_window, terminal_id, working);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("working… · build project")
    });

    let mut retrying = snapshot();
    retrying.sequence = 3;
    retrying.state = OmpCompanionState::Retrying;
    retrying.pending_approvals = 0;
    emit_companion_snapshot(&harness, companion_window, terminal_id, retrying);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("Retrying…")
    });

    let mut compacting = snapshot();
    compacting.sequence = 4;
    compacting.state = OmpCompanionState::Compacting;
    compacting.pending_approvals = 0;
    emit_companion_snapshot(&harness, companion_window, terminal_id, compacting);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("Compacting context…")
    });

    let mut awaiting = snapshot();
    awaiting.sequence = 5;
    emit_companion_snapshot(&harness, companion_window, terminal_id, awaiting);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("Awaiting approval")
    });

    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness.type_text("Orchestrator: Open").unwrap();
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("Orchestrator: Open")
    });
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("ORCHESTRATOR :: Workspaces")
    });

    let (details_col, details_row) = harness
        .find_text_on_screen("Details")
        .expect("picker must expose the Details toggle");
    harness.mouse_click(details_col, details_row).unwrap();
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("OMP connected")
    });
    let details = harness.screen_to_string();
    for expected in [
        "state awaiting_approval",
        "00000000 · companion-session",
        "openai/gpt-5 · high",
        "4096/12288 · 33.33%",
        "shell — build project",
        "finish companion UI",
        "P 3 · I 1 · B 0 · C 4 · A 0 · render details",
        "Interrupt",
    ] {
        assert!(
            details.contains(expected),
            "missing `{expected}` in companion details:\n{details}"
        );
    }

    let (interrupt_col, interrupt_row) = harness
        .find_text_on_screen("Interrupt")
        .expect("a live non-idle OMP snapshot must expose Interrupt");
    harness.mouse_click(interrupt_col, interrupt_row).unwrap();
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("OMP companion disconnected")
    });
    let after_refusal = harness.screen_to_string();
    assert!(
        after_refusal.contains("OMP disconnected"),
        "refusal must stale the facet:\n{after_refusal}"
    );
    assert!(
        !after_refusal.contains("Interrupt"),
        "stale facet must hide Interrupt:\n{after_refusal}"
    );
    assert!(
        harness
            .editor()
            .session(companion_window)
            .and_then(|window| window.terminal_manager.get(terminal_id))
            .is_some(),
        "a refused companion cancel must not close or kill the terminal",
    );
    harness.assert_no_plugin_errors();
}
