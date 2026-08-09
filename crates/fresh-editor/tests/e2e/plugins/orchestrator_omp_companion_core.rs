//! Core regression coverage for the Orchestrator Fresh–OMP companion contract.
//!
//! These tests inject Fresh-validated snapshots into the real bundled plugin.
//! They deliberately assert the state machine boundary rather than the native
//! OMP UI: marker-only launch, receipt-time liveness, sequence and exit
//! identity fencing, and exact resume replacement.

#![cfg(all(feature = "plugins", unix))]

use crate::common::{
    harness::{copy_plugin, copy_plugin_lib, EditorTestHarness},
    PathGuard,
};
use crossterm::event::{KeyCode, KeyModifiers};
use fresh::config_io::DirectoryContext;
use fresh_core::api::{PluginCommand, TerminalCompanion};
use fresh_core::hooks::{HookArgs, OmpCompanionSnapshotV1, OmpCompanionState};
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

    let bin = workspace.join("bin");
    fs::create_dir(&bin).unwrap();
    let omp = bin.join("omp");
    fs::write(
        &omp,
        "#!/bin/sh\nif [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\nlog=\"${0%/*}/omp-argv\"\n: > \"$log\"\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> \"$log\"; done\nwhile :; do sleep 60; done\n",
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

fn switch_form_to_current_workspace(harness: &mut EditorTestHarness) {
    for _ in 0..2 {
        harness
            .send_key(KeyCode::BackTab, KeyModifiers::NONE)
            .unwrap();
        harness.tick_and_render().unwrap();
    }
    harness.send_key(KeyCode::Left, KeyModifiers::NONE).unwrap();
    pump_until(harness, 40, |h| {
        h.screen_to_string().contains("ORCHESTRATOR :: Run Agent")
    });
}

fn active_terminal_ids(harness: &EditorTestHarness) -> Vec<u64> {
    harness
        .editor()
        .active_window()
        .terminal_buffers
        .values()
        .map(|binding| binding.terminal_id.0 as u64)
        .collect()
}

fn wait_for_new_active_terminal(harness: &mut EditorTestHarness, before: &[u64]) -> u64 {
    pump_until(harness, 80, |h| {
        active_terminal_ids(h).iter().any(|id| !before.contains(id))
    });
    active_terminal_ids(harness)
        .into_iter()
        .find(|id| !before.contains(id))
        .expect("current-workspace launch must create a terminal")
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

fn focus_custom_agent_command(harness: &mut EditorTestHarness) {
    focus_omp_preset(harness);
    let mut steps = 0;
    loop {
        harness
            .send_key(KeyCode::Right, KeyModifiers::NONE)
            .unwrap();
        harness.tick_and_render().unwrap();
        if harness.screen_to_string().contains("Agent: [custom") {
            return;
        }
        steps += 1;
        assert!(
            steps < 20,
            "custom command field was never focused\n{}",
            harness.screen_to_string()
        );
    }
}

fn launch_current_custom_omp(harness: &mut EditorTestHarness, command: &str) -> u64 {
    let before = active_terminal_ids(harness);
    open_new_session_form(harness);
    switch_form_to_current_workspace(harness);
    focus_custom_agent_command(harness);
    harness
        .send_key(KeyCode::Char('a'), KeyModifiers::CONTROL)
        .unwrap();
    harness.type_text(command).unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();
    wait_for_new_active_terminal(harness, &before)
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
        work_epoch: 1,
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
    harness.editor().plugin_manager().run_hook_for_plugin(
        "orchestrator",
        "omp_companion_snapshot",
        HookArgs::OmpCompanionSnapshot {
            window_id,
            terminal_id,
            received_at_ms,
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
            window_id,
            command: Some(vec!["omp".into(), "launch".into()]),
            relaunch: None,
            title: Some(title.into()),
            resume: None,
            env: None,
            companion: Some(TerminalCompanion::Omp),
            allow_script: false,
            selected_agent: true,
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
fn omp_preset_marks_terminal_without_provisional_resume() {
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
        window.tracked_agent_terminal,
        Some(terminal_id),
        "the host must own the selected OMP terminal identity",
    );

    let omp = workspace.join("bin/omp").to_string_lossy().into_owned();
    assert!(
        !window.terminal_resume_commands.contains_key(&terminal_id),
        "OMP must not persist cwd-scoped --continue before an authenticated snapshot",
    );
    assert_eq!(
        window.terminal_manager.get(terminal_id).unwrap().shell(),
        omp,
        "the preset must spawn the host-pinned executable, not PATH's first omp",
    );
    assert!(
        !workspace.join("shadow-bin/shadow-invoked").exists(),
        "the PATH shadow must never receive the companion launch",
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
    let tracked_index = serialized_workspace
        .tracked_agent_terminal
        .expect("the selected OMP terminal must be persisted by terminal index");
    assert_eq!(
        serialized_workspace.terminals[tracked_index].companion,
        Some(TerminalCompanion::Omp),
    );

    harness.assert_no_plugin_errors();
}
#[test]
fn restored_multiple_omp_terminals_keep_one_deterministic_selected_identity() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let state_temp = tempfile::tempdir().unwrap();
    let dir_context = DirectoryContext::for_testing(state_temp.path());

    let stable_id = {
        let mut harness = EditorTestHarness::with_shared_dir_context(
            160,
            50,
            Default::default(),
            workspace.clone(),
            dir_context.clone(),
        )
        .unwrap();
        harness.tick_and_render().unwrap();
        let window_id = harness.editor().active_window_id();
        let first = create_omp_terminal(&mut harness, window_id, 9010, "first omp", &[]);
        let selected = create_omp_terminal(&mut harness, window_id, 9011, "selected omp", &[first]);
        assert_eq!(
            harness.editor().active_window().tracked_agent_terminal,
            Some(fresh_core::TerminalId(selected as usize)),
        );
        harness.editor_mut().save_workspace().unwrap();
        harness.editor().active_window().stable_id.clone()
    };

    {
        let mut harness = EditorTestHarness::with_shared_dir_context(
            160,
            50,
            Default::default(),
            workspace.clone(),
            dir_context,
        )
        .unwrap();
        harness.editor_mut().active_window_mut().stable_id = stable_id;
        assert!(harness.editor_mut().try_restore_workspace().unwrap());
        pump_until(&mut harness, 80, |h| {
            h.editor().active_window().terminal_buffers.len() == 2
        });

        let window_id = harness.editor().active_window_id();
        let selected = harness
            .editor()
            .active_window()
            .tracked_agent_terminal
            .expect("restore must remap the persisted selection to a live terminal id");
        let other = harness
            .editor()
            .active_window()
            .terminal_buffers
            .values()
            .map(|binding| binding.terminal_id)
            .find(|terminal_id| *terminal_id != selected)
            .expect("the other restored OMP terminal must remain unselected");

        open_details(&mut harness);
        emit_snapshot(
            &harness,
            window_id.0,
            other.0 as u64,
            now_ms(),
            companion_snapshot(
                "123e4567-e89b-42d3-a456-426614174021",
                1,
                "00000000-0000-4000-8000-000000000021",
                "wrong-restored-omp",
                OmpCompanionState::Working,
            ),
        );
        emit_snapshot(
            &harness,
            window_id.0,
            selected.0 as u64,
            now_ms(),
            companion_snapshot(
                "123e4567-e89b-42d3-a456-426614174022",
                1,
                "00000000-0000-4000-8000-000000000022",
                "selected-restored-omp",
                OmpCompanionState::Idle,
            ),
        );
        pump_until(&mut harness, 80, |h| {
            h.screen_to_string().contains("selected-restored-omp")
        });
        assert!(
            !harness.screen_to_string().contains("wrong-restored-omp"),
            "an unselected restored OMP terminal must not claim the row",
        );

        let captured = harness.editor().capture_workspace();
        let selected_index = captured
            .tracked_agent_terminal
            .expect("the remapped selection must survive the next save");
        assert_eq!(
            captured.terminals[selected_index].title.as_deref(),
            Some("selected omp"),
        );
        harness.assert_no_plugin_errors();
    }
}

#[test]
fn omp_start_prompt_is_excluded_from_clean_relaunch_and_resume() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let initial_window = harness.editor().active_window_id();

    open_new_session_form(&mut harness);
    harness.type_text(&workspace.to_string_lossy()).unwrap();
    focus_omp_preset(&mut harness);
    let mut steps = 0;
    while !harness
        .screen_to_string()
        .lines()
        .any(|line| line.contains('▸') && line.contains("Initial task for the agent"))
    {
        harness.send_key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        harness.tick_and_render().unwrap();
        steps += 1;
        assert!(
            steps < 10,
            "Start prompt was never focused:\n{}",
            harness.screen_to_string()
        );
    }
    let prompt = "inspect the exact launch argv";
    harness.type_text(prompt).unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();
    pump_until(&mut harness, 160, |h| {
        h.editor().active_window_id() != initial_window
    });

    let (_, terminal_id) = active_terminal(&harness);
    let terminal_id = fresh_core::TerminalId(terminal_id as usize);
    let window = harness.editor().active_window();
    let omp = workspace.join("bin/omp").to_string_lossy().into_owned();
    assert_eq!(
        window.terminal_commands.get(&terminal_id),
        Some(&vec![omp.clone(), "launch".into()]),
        "the one-shot start prompt must not be replayed by a clean relaunch",
    );
    assert!(
        !window.terminal_resume_commands.contains_key(&terminal_id),
        "the launch-only prompt and provisional continue must not be replayed",
    );
    harness.assert_no_plugin_errors();
}

#[test]
fn current_workspace_omp_launch_keeps_live_companion() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace).unwrap();
    harness.tick_and_render().unwrap();
    let window_id = harness.editor().active_window_id();
    let before = active_terminal_ids(&harness);

    open_new_session_form(&mut harness);
    switch_form_to_current_workspace(&mut harness);
    focus_omp_preset(&mut harness);
    harness
        .send_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();
    let terminal_id = wait_for_new_active_terminal(&mut harness, &before);
    assert_eq!(harness.editor().active_window_id(), window_id);

    let terminal_id = fresh_core::TerminalId(terminal_id as usize);
    let window = harness.editor().active_window();
    assert_eq!(
        window.terminal_companions.get(&terminal_id),
        Some(&TerminalCompanion::Omp)
    );
    assert_eq!(
        window
            .terminal_manager
            .get(terminal_id)
            .and_then(|handle| handle.companion_kind()),
        Some(TerminalCompanion::Omp)
    );
    harness.assert_no_plugin_errors();
}

#[test]
fn custom_omp_preserves_scope_continue_aliases_and_raw_prompt_argv() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let omp = workspace.join("bin/omp").to_string_lossy().into_owned();
    let log_path = workspace.join("bin/omp-argv");
    let session_scope = format!(
        "--session-dir={}",
        workspace.join("omp-sessions").to_string_lossy()
    );
    let session_id = "123e4567-e89b-42d3-a456-426614174099";

    let mut assert_launch = |command: String,
                             relaunch_tail: Vec<String>,
                             resume_tail: Option<Vec<String>>,
                             logged: Vec<String>| {
        let terminal_id =
            fresh_core::TerminalId(launch_current_custom_omp(&mut harness, &command) as usize);
        let expected_log = format!("{}\n", logged.join("\n"));
        pump_until(&mut harness, 80, |_| {
            fs::read_to_string(&log_path).is_ok_and(|actual| actual == expected_log)
        });
        let mut expected_relaunch = vec![omp.clone()];
        expected_relaunch.extend(relaunch_tail);
        let expected_resume = resume_tail.map(|tail| {
            let mut argv = vec![omp.clone()];
            argv.extend(tail);
            argv
        });
        let window = harness.editor().active_window();
        assert_eq!(
            window.terminal_commands.get(&terminal_id),
            Some(&expected_relaunch)
        );
        assert_eq!(
            window.terminal_resume_commands.get(&terminal_id),
            expected_resume.as_ref(),
        );
        assert_eq!(
            window
                .terminal_manager
                .get(terminal_id)
                .and_then(|handle| handle.companion_kind()),
            Some(TerminalCompanion::Omp)
        );
    };

    assert_launch(
        format!("omp --profile work {session_scope} --continue"),
        vec![
            "launch".into(),
            "--profile".into(),
            "work".into(),
            session_scope.clone(),
        ],
        None,
        vec![
            "launch".into(),
            "--profile".into(),
            "work".into(),
            session_scope.clone(),
            "--continue".into(),
            "--fresh-omp-companion".into(),
        ],
    );
    assert_launch(
        "omp --profile=work -c".into(),
        vec!["launch".into(), "--profile=work".into()],
        None,
        vec![
            "launch".into(),
            "--profile=work".into(),
            "-c".into(),
            "--fresh-omp-companion".into(),
        ],
    );
    assert_launch(
        format!(
            "omp launch --profile work --model=opus --approval-mode always-ask --no-tools --trusted-extension /tmp/omp-trusted.ts --continue {session_id} -- \"--flag shaped prompt\""
        ),
        vec![
            "launch".into(),
            "--profile".into(),
            "work".into(),
            "--model=opus".into(),
            "--approval-mode".into(),
            "always-ask".into(),
            "--no-tools".into(),
            "--trusted-extension".into(),
            "/tmp/omp-trusted.ts".into(),
        ],
        Some(vec![
            "--profile".into(),
            "work".into(),
            "--model=opus".into(),
            "--approval-mode".into(),
            "always-ask".into(),
            "--no-tools".into(),
            "--trusted-extension".into(),
            "/tmp/omp-trusted.ts".into(),
            "--continue".into(),
            session_id.into(),
        ]),
        vec![
            "launch".into(),
            "--profile".into(),
            "work".into(),
            "--model=opus".into(),
            "--approval-mode".into(),
            "always-ask".into(),
            "--no-tools".into(),
            "--trusted-extension".into(),
            "/tmp/omp-trusted.ts".into(),
            "--continue".into(),
            session_id.into(),
            "--fresh-omp-companion".into(),
            "--".into(),
            "--flag shaped prompt".into(),
        ],
    );
    assert_launch(
        format!("omp --profile=work -c {session_id}"),
        vec!["launch".into(), "--profile=work".into()],
        Some(vec![
            "--profile=work".into(),
            "-c".into(),
            session_id.into(),
        ]),
        vec![
            "launch".into(),
            "--profile=work".into(),
            "-c".into(),
            session_id.into(),
            "--fresh-omp-companion".into(),
        ],
    );
    harness.assert_no_plugin_errors();
}

#[test]
fn custom_omp_rejects_ambiguous_command_and_form_prompts() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace).unwrap();
    harness.tick_and_render().unwrap();
    let before = active_terminal_ids(&harness);

    open_new_session_form(&mut harness);
    switch_form_to_current_workspace(&mut harness);
    focus_custom_agent_command(&mut harness);
    harness
        .send_key(KeyCode::Char('a'), KeyModifiers::CONTROL)
        .unwrap();
    harness.type_text("omp -- \"prompt from command\"").unwrap();
    let mut steps = 0;
    while !harness
        .screen_to_string()
        .lines()
        .any(|line| line.contains('▸') && line.contains("Initial task for the agent"))
    {
        harness.send_key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
        harness.tick_and_render().unwrap();
        steps += 1;
        assert!(steps < 12, "Start prompt was never focused");
    }
    harness.type_text("prompt from form").unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("OMP prompt is ambiguous")
    });
    let after = active_terminal_ids(&harness);
    assert_eq!(after.len(), before.len());
    assert!(after.iter().all(|terminal_id| before.contains(terminal_id)));
}

#[test]
fn omp_no_session_launch_has_no_synthesized_resume() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    let before = active_terminal_ids(&harness);

    open_new_session_form(&mut harness);
    switch_form_to_current_workspace(&mut harness);
    focus_omp_preset(&mut harness);
    // Registry agents can follow OMP. Cycle until "custom…" hands focus to
    // the retained raw command field.
    let mut custom_steps = 0;
    loop {
        harness
            .send_key(KeyCode::Right, KeyModifiers::NONE)
            .unwrap();
        harness.tick_and_render().unwrap();
        if harness.screen_to_string().contains("Agent: [custom") {
            break;
        }
        custom_steps += 1;
        assert!(
            custom_steps < 20,
            "custom command field was never focused\n{}",
            harness.screen_to_string()
        );
    }
    harness
        .send_key(KeyCode::Char('a'), KeyModifiers::CONTROL)
        .unwrap();
    harness.type_text("omp --no-session").unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::CONTROL)
        .unwrap();

    let terminal_id =
        fresh_core::TerminalId(wait_for_new_active_terminal(&mut harness, &before) as usize);
    let window = harness.editor().active_window();
    let omp = workspace.join("bin/omp").to_string_lossy().into_owned();
    assert_eq!(
        window.terminal_commands.get(&terminal_id),
        Some(&vec![omp, "launch".into(), "--no-session".into()])
    );
    assert!(
        !window.terminal_resume_commands.contains_key(&terminal_id),
        "--no-session must not turn into omp --continue"
    );
    assert_eq!(
        window.terminal_companions.get(&terminal_id),
        Some(&TerminalCompanion::Omp),
        "the descriptive marker remains durable for later authoritative launches"
    );
    assert_eq!(
        window
            .terminal_manager
            .get(terminal_id)
            .and_then(|handle| handle.companion_kind()),
        None,
        "--no-session is intentionally ineligible for a live companion"
    );
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
            None,
            Some("unsupported omp".into()),
            authority,
            None,
            None,
            false,
            Some(TerminalCompanion::Omp),
            true,
            true,
            None,
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
            window_id,
            command: Some(vec!["omp".into()]),
            relaunch: None,
            title: Some("omp".into()),
            resume: None,
            env: None,
            companion: Some(TerminalCompanion::Omp),
            allow_script: false,
            selected_agent: true,
            request_id: 9000,
        })
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        !h.editor().active_window().terminal_buffers.is_empty()
    });
    let (_, terminal_id) = active_terminal(&harness);
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
    open_details(&mut harness);
    pump_until(&mut harness, 80, |h| {
        h.screen_to_string().contains("OMP connected")
            && h.screen_to_string().contains("ready-companion")
    });
    harness.assert_no_plugin_errors();
}

#[test]
fn reconciled_session_accepts_validated_companion_terminal() {
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
            window_id,
            command: Some(vec!["omp".into()]),
            relaunch: None,
            title: Some("omp".into()),
            resume: None,
            env: None,
            companion: Some(TerminalCompanion::Omp),
            allow_script: false,
            selected_agent: true,
            request_id: 9001,
        })
        .unwrap();
    pump_until(&mut harness, 40, |h| {
        !h.editor().active_window().terminal_buffers.is_empty()
    });
    let (_, terminal_id) = active_terminal(&harness);
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
    harness.assert_no_plugin_errors();
}

#[test]
fn receipt_liveness_sequence_and_exit_fences_companion_state() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    spawn_omp_session(&mut harness, &workspace);
    let (window_id, terminal_id) = active_terminal(&harness);
    let selected_terminal = {
        let snapshot = harness
            .editor()
            .plugin_manager()
            .state_snapshot_handle()
            .expect("plugin snapshot must exist");
        let snapshot = snapshot.read().unwrap();
        snapshot
            .windows
            .iter()
            .find(|window| window.id.0 == window_id)
            .and_then(|window| window.selected_agent_terminal_id)
    };
    assert_eq!(
        selected_terminal,
        Some(fresh_core::WindowTerminalId::new(
            fresh_core::WindowId(window_id),
            fresh_core::TerminalId(terminal_id as usize),
        )),
        "the plugin snapshot must expose the host-selected OMP terminal before its first companion receipt",
    );

    // A host exit for another terminal in this window is not ownership
    // evidence for this selected OMP terminal and must not advance its fence.
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: terminal_id + 1,
            window_id,
            exit_code: None,
        },
    );

    // Receipt time, not the emitter timestamp, makes an old delivery stale
    // immediately and leaves the last-known facet visibly disconnected.
    let mut old_receipt = companion_snapshot(
        "123e4567-e89b-42d3-a456-426614174000",
        2,
        "00000000-0000-4000-8000-000000000001",
        "old-receipt",
        OmpCompanionState::Working,
    );
    old_receipt.session_generation = 2;
    emit_snapshot(&harness, window_id, terminal_id, 0, old_receipt);
    open_details(&mut harness);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("OMP disconnected")
    });

    // Within one incarnation lower/equal sequence numbers are ignored. A new
    // incarnation gets a fresh sequence space but cannot roll session generation
    // backward; an unseen incarnation at the current generation is accepted.
    let mut ignored_sequence = companion_snapshot(
        "123e4567-e89b-42d3-a456-426614174000",
        1,
        "00000000-0000-4000-8000-000000000002",
        "ignored-sequence",
        OmpCompanionState::Idle,
    );
    ignored_sequence.session_generation = 2;
    emit_snapshot(&harness, window_id, terminal_id, now_ms(), ignored_sequence);
    emit_snapshot(
        &harness,
        window_id,
        terminal_id,
        now_ms(),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174001",
            1,
            "00000000-0000-4000-8000-000000000003",
            "ignored-generation",
            OmpCompanionState::Idle,
        ),
    );
    let mut new_incarnation = companion_snapshot(
        "123e4567-e89b-42d3-a456-426614174002",
        1,
        "00000000-0000-4000-8000-000000000004",
        "new-incarnation",
        OmpCompanionState::Idle,
    );
    new_incarnation.session_generation = 2;
    emit_snapshot(&harness, window_id, terminal_id, now_ms(), new_incarnation);
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("new-incarnation")
    });
    let after_new_incarnation = harness.screen_to_string();
    assert!(
        !after_new_incarnation.contains("ignored-sequence"),
        "a stale sequence must not replace the last facet before a new incarnation arrives:\n{after_new_incarnation}",
    );
    assert!(
        !after_new_incarnation.contains("ignored-generation"),
        "a new process incarnation must not roll session generation backward:\n{after_new_incarnation}",
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
    let mut survives_wrong_window_exit = companion_snapshot(
        "123e4567-e89b-42d3-a456-426614174002",
        2,
        "00000000-0000-4000-8000-000000000004",
        "survives-wrong-window-exit",
        OmpCompanionState::Idle,
    );
    survives_wrong_window_exit.session_generation = 2;
    emit_snapshot(
        &harness,
        window_id,
        terminal_id,
        now_ms(),
        survives_wrong_window_exit,
    );
    pump_until(&mut harness, 40, |h| {
        h.screen_to_string().contains("survives-wrong-window-exit")
    });

    // The bounded exit fence covers this exact window/terminal identity. A
    // late queued hook cannot revive the facet, even with a new incarnation.
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id,
            window_id,
            exit_code: None,
        },
    );
    // A non-companion terminal in the same window may exit later. It must
    // neither weaken A's ownership fence nor let A's delayed hook live.
    harness.editor().plugin_manager().run_hook(
        "terminal_exit",
        HookArgs::TerminalExited {
            terminal_id: terminal_id + 1,
            window_id,
            exit_code: None,
        },
    );
    let mut late_after_exit = companion_snapshot(
        "123e4567-e89b-42d3-a456-426614174002",
        3,
        "00000000-0000-4000-8000-000000000004",
        "late-after-exit",
        OmpCompanionState::Working,
    );
    late_after_exit.session_generation = 2;
    emit_snapshot(&harness, window_id, terminal_id, now_ms(), late_after_exit);
    for _ in 0..10 {
        harness.process_async_and_render().unwrap();
    }
    assert!(
        !harness.screen_to_string().contains("late-after-exit"),
        "the terminal-exit ownership fence must reject delayed companion hooks",
    );
    harness.assert_no_plugin_errors();
}

#[test]
fn live_companion_expires_on_scheduled_wall_clock_deadline() {
    let (_temp, workspace, _path_restore) = set_up_workspace();
    let mut harness = EditorTestHarness::with_working_dir(160, 50, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();
    spawn_omp_session(&mut harness, &workspace);
    let (window_id, terminal_id) = active_terminal(&harness);
    open_details(&mut harness);

    emit_snapshot(
        &harness,
        window_id,
        terminal_id,
        now_ms().saturating_sub(11_900),
        companion_snapshot(
            "123e4567-e89b-42d3-a456-426614174017",
            1,
            "00000000-0000-4000-8000-000000000017",
            "expiring-companion",
            OmpCompanionState::Working,
        ),
    );
    pump_until(&mut harness, 40, |h| {
        let screen = h.screen_to_string();
        screen.contains("OMP connected") && screen.contains("Interrupt")
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        harness.process_async_and_render().unwrap();
        let screen = harness.screen_to_string();
        if screen.contains("OMP disconnected") && !screen.contains("Interrupt") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "scheduled liveness expiry did not refresh the UI:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    harness.assert_no_plugin_errors();
}
