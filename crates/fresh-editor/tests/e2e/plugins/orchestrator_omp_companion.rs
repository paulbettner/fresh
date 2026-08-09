//! End-to-end coverage for the Orchestrator's native Fresh–OMP companion UI.
//!
//! The authenticated OSC ingress is covered by its native tests. This module
//! deliberately injects the already-validated hook payload into the real
//! Orchestrator plugin, then drives the rendered picker and its Interrupt
//! action. It pins the presentation boundary: structured snapshot metadata is
//! shown, while a refused cancel marks the facet stale without killing the PTY.

#![cfg(all(feature = "plugins", unix))]

use crate::common::{
    harness::{copy_plugin, copy_plugin_lib, EditorTestHarness},
    PathGuard,
};
use crossterm::event::{KeyCode, KeyModifiers};
use fresh_core::api::{PluginCommand, TerminalCompanion};

use fresh_core::hooks::{
    HookArgs, OmpCompanionAsyncJobs, OmpCompanionContext, OmpCompanionCurrentTool,
    OmpCompanionGoal, OmpCompanionGoalStatus, OmpCompanionModel, OmpCompanionSnapshotV1,
    OmpCompanionState, OmpCompanionThinkingLevel, OmpCompanionTodos,
};
use ratatui::style::{Color, Modifier};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn set_up_workspace() -> (tempfile::TempDir, PathBuf, PathGuard) {
    fresh::i18n::set_locale("en");
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().canonicalize().unwrap();
    let plugins = workspace.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");

    // The literal OMP preset is resolved through an explicit host-owned path.
    // A different PATH-first `omp` proves workspace shadows cannot receive the
    // private companion capability.
    let bin = workspace.join("bin");
    fs::create_dir(&bin).unwrap();
    let omp = bin.join("omp");
    fs::write(
        &omp,
        "#!/bin/sh\nif [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\nwhile :; do sleep 60; done\n",
    )
    .unwrap();
    fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).unwrap();
    let shadow_bin = workspace.join("shadow-bin");
    fs::create_dir(&shadow_bin).unwrap();
    let shadow_omp = shadow_bin.join("omp");
    fs::write(
        &shadow_omp,
        "#!/bin/sh\ntouch \"${0%/*}/shadow-invoked\"\nwhile :; do sleep 60; done\n",
    )
    .unwrap();
    fs::set_permissions(&shadow_omp, fs::Permissions::from_mode(0o755)).unwrap();
    let path_guard = PathGuard::prepend_with_trusted_omp(&shadow_bin, &omp);

    (temp, workspace, path_guard)
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
        work_epoch: 1,
        timestamp_ms: 1,
        omp_version: "0.19.0".into(),
        process_id: 42,
        session_id: "00000000-0000-4000-8000-000000000001".into(),
        session_name: Some("companion-session".into()),
        cwd: "/workspace".into(),
        state: OmpCompanionState::AwaitingApproval,
        status_text: None,
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
    // The window can become active one frame before the queued
    // active_window_changed hook records its activation timestamp.
    harness.process_async_and_render().unwrap();
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
    harness.editor().plugin_manager().run_hook_for_plugin(
        "orchestrator",
        "omp_companion_snapshot",
        HookArgs::OmpCompanionSnapshot {
            window_id: window_id.0,
            terminal_id: terminal_id.0 as u64,
            received_at_ms,
            snapshot,
        },
    );
}

fn emit_terminal_output_with_activity(
    harness: &EditorTestHarness,
    window_id: fresh_core::WindowId,
    terminal_id: fresh_core::TerminalId,
    terminal_title: &str,
    osc_activity: Option<bool>,
) {
    harness.editor().plugin_manager().run_hook(
        "terminal_output",
        HookArgs::TerminalOutput {
            terminal_id: terminal_id.0 as u64,
            window_id: window_id.0,
            last_line: String::new(),
            terminal_title: terminal_title.into(),
            osc_activity,
        },
    );
}

fn emit_terminal_output(
    harness: &EditorTestHarness,
    window_id: fresh_core::WindowId,
    terminal_id: fresh_core::TerminalId,
    terminal_title: &str,
) {
    emit_terminal_output_with_activity(harness, window_id, terminal_id, terminal_title, Some(true));
}

fn status_foregrounds(harness: &EditorTestHarness, text: &str) -> Vec<Color> {
    let (column, row) = harness
        .find_text_on_screen(text)
        .expect("working status must be visible");
    (0..text.chars().count() as u16)
        .map(|offset| {
            harness
                .get_cell_style(column + offset, row)
                .and_then(|style| style.fg)
                .unwrap_or(Color::Reset)
        })
        .collect()
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
fn activation_grace_keeps_owned_ui_secondary_output_and_terminal_zero_actions() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let (window_id, terminal_id) = spawn_omp_session(&mut harness, &workspace);
    assert_eq!(
        terminal_id.0, 0,
        "the regression must exercise terminal id zero"
    );

    let unicode_title = "e\u{301}界👨‍👩‍👧‍👦";
    emit_terminal_output_with_activity(
        &harness,
        window_id,
        terminal_id,
        &format!("π ⠧ {unicode_title}"),
        Some(false),
    );

    harness
        .editor_mut()
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: Some(false),
            persistent: false,
            window_id,
            command: Some(vec!["sh".into(), "-c".into(), "sleep 60".into()]),
            relaunch: None,
            title: Some("secondary shell".into()),
            resume: None,
            env: None,
            companion: None,
            allow_script: false,
            selected_agent: false,
            request_id: 9910,
        })
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        h.editor()
            .session(window_id)
            .is_some_and(|window| window.terminal_buffers.len() == 2)
    });
    let secondary = harness
        .editor()
        .session(window_id)
        .unwrap()
        .terminal_buffers
        .values()
        .map(|binding| binding.terminal_id)
        .find(|id| *id != terminal_id)
        .unwrap();

    std::thread::sleep(Duration::from_millis(1_600));
    emit_terminal_output_with_activity(&harness, window_id, secondary, "", None);

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
        h.screen_to_string().lines().any(|line| {
            line.contains("e\u{301}")
                && line.contains('界')
                && line.contains('👨')
                && (line.contains("│ * ") || line.contains("┃ * "))
        })
    });

    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: secondary.0 as u64,
            window_id: window_id.0,
            exit_code: Some(0),
        },
    );
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().lines().any(|line| {
            line.contains("e\u{301}")
                && line.contains('界')
                && line.contains('👨')
                && (line.contains("│ · ") || line.contains("┃ · "))
        })
    });

    let (stop_col, stop_row) = harness
        .find_text_on_screen("Stop")
        .expect("terminal zero must expose an enabled Stop action");
    harness.mouse_click(stop_col, stop_row).unwrap();
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("Confirm Stop")
    });
    let (confirm_col, confirm_row) = harness
        .find_text_on_screen("Confirm Stop ]")
        .expect("terminal zero Stop must enter confirmation");
    harness.mouse_click(confirm_col, confirm_row).unwrap();
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("omp (exited)")
    });
    harness.assert_no_plugin_errors();
}
#[test]
fn unchanged_explicit_idle_marker_does_not_mask_later_owned_output() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let (window_id, terminal_id) = spawn_omp_session(&mut harness, &workspace);

    std::thread::sleep(Duration::from_millis(1_600));
    emit_terminal_output_with_activity(&harness, window_id, terminal_id, "omp busy", Some(false));
    harness.process_async_and_render().unwrap();
    std::thread::sleep(Duration::from_millis(10));
    emit_terminal_output_with_activity(&harness, window_id, terminal_id, "omp busy", Some(false));

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
        h.screen_to_string().lines().any(|line| {
            line.contains("omp busy") && (line.contains("│ * ") || line.contains("┃ * "))
        })
    });
    harness.assert_no_plugin_errors();
}

#[test]
fn interrupt_after_rebind_targets_only_the_new_owned_terminal() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let (window_id, first) = spawn_omp_session(&mut harness, &workspace);

    let mut first_snapshot = snapshot();
    first_snapshot.state = OmpCompanionState::Working;
    first_snapshot.pending_approvals = 0;
    first_snapshot.status_text = Some("old A working".into());
    emit_companion_snapshot(&harness, window_id, first, first_snapshot);

    harness
        .editor_mut()
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: Some(false),
            persistent: true,
            window_id,
            command: Some(vec!["omp".into()]),
            relaunch: None,
            title: Some("replacement B".into()),
            resume: Some(vec!["omp".into(), "--continue".into()]),
            env: None,
            companion: Some(TerminalCompanion::Omp),
            allow_script: false,
            selected_agent: true,
            request_id: 9911,
        })
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        h.editor()
            .session(window_id)
            .is_some_and(|window| window.terminal_buffers.len() == 2)
    });
    let replacement = harness
        .editor()
        .session(window_id)
        .unwrap()
        .terminal_buffers
        .values()
        .map(|binding| binding.terminal_id)
        .find(|id| *id != first)
        .unwrap();
    harness
        .editor()
        .session(window_id)
        .unwrap()
        .terminal_manager
        .get(replacement)
        .unwrap()
        .revoke_omp_companion();

    let mut stale_first = snapshot();
    stale_first.sequence = 2;
    stale_first.state = OmpCompanionState::Working;
    stale_first.pending_approvals = 0;
    stale_first.status_text = Some("stale A after host rebind".into());
    emit_companion_snapshot(&harness, window_id, first, stale_first);
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
    for _ in 0..10 {
        harness.process_async_and_render().unwrap();
    }
    assert!(
        !harness
            .screen_to_string()
            .contains("stale A after host rebind"),
        "a delayed hook from the old terminal must lose to host ownership",
    );

    let mut replacement_snapshot = snapshot();
    replacement_snapshot.incarnation = "123e4567-e89b-42d3-a456-426614174099".into();
    replacement_snapshot.state = OmpCompanionState::Working;
    replacement_snapshot.pending_approvals = 0;
    replacement_snapshot.status_text = Some("replacement B working".into());
    emit_companion_snapshot(&harness, window_id, replacement, replacement_snapshot);

    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("replacement B working")
    });
    assert!(
        !harness.screen_to_string().contains("old A working"),
        "the old terminal facet must be cleared during rebind",
    );
    let (interrupt_col, interrupt_row) = harness
        .find_text_on_screen("Interrupt")
        .expect("replacement B must expose Interrupt");
    harness.mouse_click(interrupt_col, interrupt_row).unwrap();
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("OMP companion disconnected")
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
    // not request background attention or overwrite unrelated status.
    let active_sentinel = "active approval sentinel";
    harness
        .editor_mut()
        .set_status_message(active_sentinel.to_string());
    let mut active = snapshot();
    active.status_text = Some("active companion status".into());
    emit_companion_snapshot(&harness, companion_window, terminal_id, active);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("active companion status")
    });
    assert_eq!(
        harness.editor().get_status_message().map(String::as_str),
        Some(active_sentinel),
        "an approval first observed in the active workspace must not notify",
    );
    harness.editor_mut().set_active_window(initial_window);

    let generation_sentinel = "generation approval sentinel";
    harness
        .editor_mut()
        .set_status_message(generation_sentinel.to_string());
    let mut generation_changed = snapshot();
    generation_changed.sequence = 2;
    generation_changed.session_generation = 2;
    generation_changed.session_id = "00000000-0000-4000-8000-000000000002".into();
    generation_changed.status_text = Some("generation companion status".into());
    emit_companion_snapshot(&harness, companion_window, terminal_id, generation_changed);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("generation companion status")
    });
    assert_eq!(
        harness.editor().get_status_message().map(String::as_str),
        Some(generation_sentinel),
        "a generation change with pending approvals still at one must not notify",
    );

    let incarnation_sentinel = "incarnation approval sentinel";
    harness
        .editor_mut()
        .set_status_message(incarnation_sentinel.to_string());
    let mut incarnation_changed = snapshot();
    incarnation_changed.incarnation = "123e4567-e89b-42d3-a456-426614174001".into();
    incarnation_changed.session_generation = 2;
    incarnation_changed.session_id = "00000000-0000-4000-8000-000000000003".into();
    incarnation_changed.status_text = Some("incarnation companion status".into());
    emit_companion_snapshot(&harness, companion_window, terminal_id, incarnation_changed);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string()
            .contains("incarnation companion status")
    });
    assert_eq!(
        harness.editor().get_status_message().map(String::as_str),
        Some(incarnation_sentinel),
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
        h.screen_to_string().contains("Awaiting approval")
    });

    let mut working = snapshot();
    working.sequence = 2;
    working.state = OmpCompanionState::Working;
    working.pending_approvals = 0;
    working.status_text = Some("Finding top-level files".into());
    emit_companion_snapshot(&harness, companion_window, terminal_id, working);

    // The activation guard intentionally ignores the terminal's first redraw
    // burst; inject a later OMP title frame exactly as the real PTY hook does.
    harness.sleep(Duration::from_millis(1_600));
    emit_terminal_output(&harness, companion_window, terminal_id, "π ⠧ omp-task");
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string()
            .lines()
            .any(|line| line.contains("⠧ Finding top-level files"))
    });
    let working_screen = harness.screen_to_string();
    let title_line = working_screen
        .lines()
        .find(|line| line.contains(" · omp-task"))
        .expect("working OMP card title must be visible");
    let working_title_row = working_screen
        .lines()
        .position(|line| line.contains(" · omp-task"))
        .unwrap();
    let working_status_row = working_screen
        .lines()
        .position(|line| line.contains("Finding top-level files"))
        .unwrap();
    assert_eq!(
        working_status_row,
        working_title_row + 1,
        "working status must occupy the row immediately below its title"
    );
    assert!(!title_line.contains("Finding top-level files"));
    assert!(
        title_line.contains("│  ") || title_line.contains("┃  "),
        "structured OMP status must reserve a blank first-row gutter:\n{title_line}"
    );
    assert!(
        !title_line.contains("│* ")
            && !title_line.contains("┃* ")
            && !title_line.contains("│· ")
            && !title_line.contains("┃· "),
        "OMP card must not repeat working/idle state on its first row:\n{title_line}"
    );
    assert!(
        working_screen.contains(" · omp-task"),
        "workspace title must retain the OMP task title:\n{working_screen}"
    );
    assert!(
        !working_screen.contains(" · ⠧ omp-task"),
        "loader frame must not animate in the workspace title:\n{working_screen}"
    );
    assert!(!working_screen.contains("working… · build project"));
    assert!(!working_screen.contains("▸ (detached)"));

    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let status_text = "Finding top-level files";
    let mut first_pattern = Vec::new();
    for frame in frames.iter().cycle().take(40) {
        emit_terminal_output(
            &harness,
            companion_window,
            terminal_id,
            &format!("π {frame} omp-task"),
        );
        harness.process_async_and_render().unwrap();
        first_pattern = status_foregrounds(&harness, status_text);
        if first_pattern
            .first()
            .is_some_and(|first| first_pattern.iter().any(|color| color != first))
        {
            break;
        }
        harness.sleep(Duration::from_millis(25));
    }
    assert!(
        first_pattern
            .first()
            .is_some_and(|first| first_pattern.iter().any(|color| color != first)),
        "OMP-style shimmer never painted its moving accent band"
    );
    let (status_column, status_row) = harness.find_text_on_screen(status_text).unwrap();
    for offset in 0..status_text.chars().count() as u16 {
        let style = harness
            .get_cell_style(status_column + offset, status_row)
            .expect("status cell must be rendered");
        assert!(
            !style.add_modifier.contains(Modifier::ITALIC),
            "OMP shimmer text must not retain the old static italic style"
        );
    }

    let mut highlight_moved = false;
    for frame in frames.iter().cycle().take(30) {
        harness.sleep(Duration::from_millis(40));
        emit_terminal_output(
            &harness,
            companion_window,
            terminal_id,
            &format!("π {frame} omp-task"),
        );
        harness.process_async_and_render().unwrap();
        let next_pattern = status_foregrounds(&harness, status_text);
        if next_pattern != first_pattern {
            highlight_moved = true;
            break;
        }
    }
    assert!(
        highlight_moved,
        "OMP-style shimmer accent band did not move"
    );

    let mut retrying = snapshot();
    retrying.sequence = 3;
    retrying.state = OmpCompanionState::Retrying;
    retrying.pending_approvals = 0;
    retrying.status_text = Some("Retrying (1/3) in 2s…".into());
    emit_companion_snapshot(&harness, companion_window, terminal_id, retrying);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("Retrying (1/3) in 2s…")
    });

    let mut compacting = snapshot();
    compacting.sequence = 4;
    compacting.state = OmpCompanionState::Compacting;
    compacting.pending_approvals = 0;
    compacting.status_text = Some("Auto context-full maintenance…".into());
    emit_companion_snapshot(&harness, companion_window, terminal_id, compacting);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string()
            .contains("Auto context-full maintenance…")
    });

    let mut idle = snapshot();
    idle.sequence = 5;
    idle.state = OmpCompanionState::Idle;
    idle.pending_approvals = 0;
    emit_companion_snapshot(&harness, companion_window, terminal_id, idle);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string()
            .lines()
            .any(|line| line.contains("Idle"))
    });
    let idle_screen = harness.screen_to_string();
    let idle_title_row = idle_screen
        .lines()
        .position(|line| line.contains(" · omp-task"))
        .unwrap();
    let idle_status_row = idle_screen
        .lines()
        .position(|line| line.contains("Idle"))
        .unwrap();
    assert_eq!(
        idle_status_row,
        idle_title_row + 1,
        "idle status must occupy the row immediately below its title"
    );
    assert!(!idle_screen
        .lines()
        .nth(idle_title_row)
        .unwrap()
        .contains("Idle"));
    assert!(!idle_screen.contains(" idle"));

    let mut error = snapshot();
    error.sequence = 6;
    error.state = OmpCompanionState::Error;
    error.pending_approvals = 0;
    error.status_text = Some("Model failed".into());
    emit_companion_snapshot(&harness, companion_window, terminal_id, error);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("Model failed")
    });
    let error_screen = harness.screen_to_string();
    let error_title = error_screen
        .lines()
        .find(|line| line.contains(" · omp-task"))
        .expect("errored OMP card title must be visible");
    assert!(
        error_title.contains("│! ") || error_title.contains("┃! "),
        "critical OMP errors must retain the first-row `!` glyph:\n{error_title}"
    );

    let mut awaiting = snapshot();
    awaiting.sequence = 7;
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
        "state Awaiting approval",
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
    // Interrupt owned focus when the refusal snapshot removed that button.
    // Enter must now activate the stable Visit action, not wrap to New Workspace.
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        !h.screen_to_string().contains("ORCHESTRATOR :: Workspaces")
    });
    assert!(
        !harness
            .screen_to_string()
            .contains("ORCHESTRATOR :: New Workspace"),
        "a disappearing transient Interrupt action must not retarget Enter to New Workspace",
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
