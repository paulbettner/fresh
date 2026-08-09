//! Regression test: an Orchestrator agent terminal survives a restart.
//!
//! Orchestrator sessions spawn their agent as an *ephemeral* terminal whose
//! spawn argv is recorded in `Window::terminal_commands` (see
//! `create_window_with_terminal`). Before the fix, workspace-save dropped every
//! ephemeral terminal, so a saved session held no terminal at all and came back
//! as a blank `[No Name]` pane on restore. The fix persists a command-carrying
//! ephemeral terminal and re-runs that command on restore.
//!
//! This test reproduces the round-trip at the window level: spawn an ephemeral
//! terminal with a recognizable command, save, restore in a fresh editor that
//! shares the same data dir, and assert the terminal comes back (a terminal
//! buffer, showing the command's marker) rather than a blank pane.
//!
//! Requires a working PTY (/dev/ptmx); skips when unavailable, like the other
//! terminal e2e tests.

use crate::common::harness::{EditorTestHarness, HarnessOptions};
use fresh::config::Config;
use fresh::config_io::DirectoryContext;
use fresh::workspace::{AgentResume, Workspace};
use fresh_core::api::TerminalCompanion;
use portable_pty::{native_pty_system, PtySize};
use tempfile::TempDir;

fn pty_available() -> bool {
    native_pty_system()
        .openpty(PtySize {
            rows: 1,
            cols: 1,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

fn session_config() -> Config {
    let mut config = Config::default();
    config.editor.hot_exit = true;
    // Isolate the restored-terminal behaviour from the "new output re-enters
    // terminal mode" path so the assertions don't depend on shell timing.
    config.terminal.jump_to_end_on_output = false;
    config
}

/// Spawn an ephemeral, command-carrying terminal into `window` the way
/// `create_window_with_terminal` does: an ephemeral PTY plus a
/// `terminal_commands` entry marking it as a restorable *session* terminal.
fn spawn_agent_terminal(
    window: &mut fresh::app::window::Window,
    argv: &[&str],
) -> fresh_core::TerminalId {
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let (terminal_id, _buffer_id, _leaf) = window
        .create_plugin_terminal(fresh::app::PluginTerminalSpec {
            cwd: None,
            direction: None, // no split direction — seed/attach in the active split
            ratio: None,
            focus: true,       // the agent terminal is the seed
            persistent: false, // ephemeral — exactly the Orchestrator agent case
            command: Some(argv.clone()),
            title: None,
            env: std::collections::HashMap::new(),
            companion: None,
            script_capability: None,
        })
        .expect("agent terminal should spawn");
    // create_window_with_terminal records this marker; mirror it here so the
    // ephemeral terminal is recognised as a restorable session terminal.
    window.terminal_commands.insert(terminal_id, argv);
    terminal_id
}

/// Like `spawn_agent_terminal`, but also records an agent-resume argv — the
/// way `create_window_with_terminal` does when the Orchestrator provisions a
/// resumable agent (launch with `--session-id`, resume with `--resume`).
fn spawn_resumable_agent_terminal(
    window: &mut fresh::app::window::Window,
    launch: &[&str],
    resume: &[&str],
) -> fresh_core::TerminalId {
    let launch: Vec<String> = launch.iter().map(|s| s.to_string()).collect();
    let resume: Vec<String> = resume.iter().map(|s| s.to_string()).collect();
    let (terminal_id, _buffer_id, _leaf) = window
        .create_plugin_terminal(fresh::app::PluginTerminalSpec {
            cwd: None,
            direction: None,
            ratio: None,
            focus: true,
            persistent: false,
            command: Some(launch.clone()),
            title: None,
            env: std::collections::HashMap::new(),
            companion: None,
            script_capability: None,
        })
        .expect("agent terminal should spawn");
    window.terminal_commands.insert(terminal_id, launch);
    window.terminal_resume_commands.insert(terminal_id, resume);
    terminal_id
}

#[test]
#[cfg_attr(target_os = "windows", ignore)] // Uses a Unix shell command
fn test_orchestrator_agent_terminal_restores_after_restart() {
    if !pty_available() {
        eprintln!("Skipping agent-terminal restore test: PTY not available");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let project_dir = temp_dir.path().join("project");
    std::fs::create_dir(&project_dir).unwrap();
    let dir_context = DirectoryContext::for_testing(temp_dir.path());

    // A long-lived command so the terminal is live (not exited) at save time.
    let argv = ["sh", "-c", "exec sleep 30"];

    // ---- Session 1: spawn the agent terminal, then save. ----
    {
        let mut harness = EditorTestHarness::create(
            120,
            30,
            HarnessOptions::new()
                .with_config(session_config())
                .with_working_dir(project_dir.clone())
                .with_shared_dir_context(dir_context.clone())
                .without_empty_plugins_dir(),
        )
        .unwrap();
        harness.editor_mut().set_session_mode(true);

        spawn_agent_terminal(harness.editor_mut().active_window_mut(), &argv);
        harness.render().unwrap();
        // The spawned agent terminal is the active buffer in this session.
        let active = harness.editor().active_buffer_id();
        assert!(
            harness.editor().active_window().is_terminal_buffer(active),
            "agent terminal should be the active buffer before save"
        );

        harness.shutdown(true).unwrap();
    }

    // ---- Session 2: restart sharing the same data dir, then verify the
    // agent terminal is back (not a blank pane). ----
    {
        let mut harness = EditorTestHarness::create(
            120,
            30,
            HarnessOptions::new()
                .with_config(session_config())
                .with_working_dir(project_dir.clone())
                .with_shared_dir_context(dir_context.clone())
                .without_empty_plugins_dir(),
        )
        .unwrap();

        let restored = harness.startup(true, &[]).unwrap();
        assert!(restored, "session should have been restored");
        harness.render().unwrap();

        // The fix: a terminal buffer comes back. Without it, the ephemeral
        // terminal was dropped on save and the restored window holds only an
        // empty `[No Name]` buffer, so the active buffer is NOT a terminal.
        let active = harness.editor().active_buffer_id();
        assert!(
            harness.editor().active_window().is_terminal_buffer(active),
            "restored Orchestrator session should come back as a terminal, not a blank pane"
        );
    }
}

/// On restore, a terminal carrying an agent-resume spec runs the *resume*
/// argv, not the launch command — proving agent sessions rejoin rather than
/// restart. Asserted via a filesystem side effect so there's no dependence on
/// live PTY output timing: launch and resume `touch` different sentinel files;
/// after restart only the resume sentinel should appear.
#[test]
#[cfg_attr(target_os = "windows", ignore)] // Uses a Unix shell command
fn test_agent_resume_runs_resume_command_on_restart() {
    if !pty_available() {
        eprintln!("Skipping agent-resume test: PTY not available");
        return;
    }

    let temp_dir = TempDir::new().unwrap();
    let project_dir = temp_dir.path().join("project");
    std::fs::create_dir(&project_dir).unwrap();
    let dir_context = DirectoryContext::for_testing(temp_dir.path());

    // Sentinels in a dir that survives between the two sessions.
    let sentinels = temp_dir.path().join("sentinels");
    std::fs::create_dir(&sentinels).unwrap();
    let launched = sentinels.join("LAUNCHED");
    let resumed = sentinels.join("RESUMED");
    let launch_cmd = format!("touch '{}'; exec sleep 30", launched.display());
    let resume_cmd = format!("touch '{}'; exec sleep 30", resumed.display());
    let launch = ["sh", "-c", launch_cmd.as_str()];
    let resume = ["sh", "-c", resume_cmd.as_str()];

    // ---- Session 1: launch the resumable agent, then save. ----
    {
        let mut harness = EditorTestHarness::create(
            120,
            30,
            HarnessOptions::new()
                .with_config(session_config())
                .with_working_dir(project_dir.clone())
                .with_shared_dir_context(dir_context.clone())
                .without_empty_plugins_dir(),
        )
        .unwrap();
        harness.editor_mut().set_session_mode(true);

        spawn_resumable_agent_terminal(harness.editor_mut().active_window_mut(), &launch, &resume);
        harness.render().unwrap();
        // The launch command ran (not the resume one).
        harness
            .wait_until(|_| launched.exists())
            .expect("launch command should run in the first session");
        assert!(
            !resumed.exists(),
            "resume command must not run during the initial launch"
        );

        harness.shutdown(true).unwrap();
    }

    // ---- Session 2: restart; the resume argv should run, not the launch. ----
    {
        let mut harness = EditorTestHarness::create(
            120,
            30,
            HarnessOptions::new()
                .with_config(session_config())
                .with_working_dir(project_dir.clone())
                .with_shared_dir_context(dir_context.clone())
                .without_empty_plugins_dir(),
        )
        .unwrap();

        let restored = harness.startup(true, &[]).unwrap();
        assert!(restored, "session should have been restored");
        harness.render().unwrap();

        harness
            .wait_until(|_| resumed.exists())
            .expect("restore should run the agent-resume command, not the launch command");
    }
}

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn failed_live_restore_keeps_a_retryable_dormant_terminal() {
    if !pty_available() {
        return;
    }
    let temp_dir = TempDir::new().unwrap();
    let project_dir = temp_dir.path().join("project");
    std::fs::create_dir(&project_dir).unwrap();
    let dir_context = DirectoryContext::for_testing(temp_dir.path());
    let marker = "FAILED-RESTORE-TRANSCRIPT";
    let command = format!("printf '{marker}\\n'; exec sleep 30");

    let stable_id = {
        let mut harness = EditorTestHarness::create(
            120,
            30,
            HarnessOptions::new()
                .with_config(session_config())
                .with_working_dir(project_dir.clone())
                .with_shared_dir_context(dir_context.clone())
                .without_empty_plugins_dir(),
        )
        .unwrap();
        harness.editor_mut().set_session_mode(true);
        spawn_agent_terminal(
            harness.editor_mut().active_window_mut(),
            &["sh", "-c", &command],
        );
        harness
            .wait_until(|h| h.screen_to_string().contains(marker))
            .unwrap();
        let stable_id = harness.editor().active_window().stable_id.clone();
        harness.shutdown(true).unwrap();
        stable_id
    };

    let mut workspace = Workspace::load_by_id_in(&dir_context, &project_dir, &stable_id)
        .unwrap()
        .expect("saved workspace");
    let terminal = workspace.terminals.first_mut().expect("saved terminal");
    let saved_backing = std::fs::read_to_string(&terminal.backing_path).unwrap_or_else(|error| {
        panic!(
            "failed to read saved backing {:?}: {error}",
            terminal.backing_path
        )
    });
    assert!(
        saved_backing.contains(marker),
        "saved backing lost transcript before restore: {saved_backing:?}"
    );
    let missing = temp_dir
        .path()
        .join("missing-agent")
        .to_string_lossy()
        .into_owned();
    let _path_guard = crate::common::PathGuard::prepend_with_trusted_omp(
        temp_dir.path(),
        std::path::Path::new(&missing),
    );
    terminal.command = Some(vec![missing.clone()]);
    terminal.agent_resume = Some(AgentResume {
        argv: vec![missing.clone(), "--resume".into(), "exact".into()],
    });
    terminal.script_access = true;
    terminal.companion = Some(TerminalCompanion::Omp);
    terminal.title = Some("retryable-agent".into());
    workspace.save_in(&dir_context).unwrap();
    let persisted = Workspace::load_by_id_in(&dir_context, &project_dir, &stable_id)
        .unwrap()
        .expect("modified workspace");
    assert_eq!(persisted.terminals.len(), 1);
    assert_eq!(
        persisted.terminals[0].title.as_deref(),
        Some("retryable-agent")
    );

    let mut harness = EditorTestHarness::create(
        120,
        30,
        HarnessOptions::new()
            .with_config(session_config())
            .with_working_dir(project_dir.clone())
            .with_shared_dir_context(dir_context.clone())
            .without_empty_plugins_dir(),
    )
    .unwrap();
    assert!(harness.startup(true, &[]).unwrap());
    harness.render().unwrap();

    let restored_screen = harness.screen_to_string();
    let window = harness.editor().active_window();
    let (&buffer_id, exited) = window
        .exited_terminals
        .iter()
        .find(|(_, exited)| exited.title.as_deref() == Some("retryable-agent"))
        .unwrap_or_else(|| {
            panic!(
                "failed live restore must become dormant, not disappear: exited={:?}, live={:?}, screen={restored_screen}",
                window.exited_terminals,
                window.terminal_buffers.keys().collect::<Vec<_>>()
            )
        });
    assert_eq!(exited.command, Some(vec![missing]));
    assert!(exited.script_access);
    assert_eq!(exited.companion, Some(TerminalCompanion::Omp));
    assert_eq!(exited.title.as_deref(), Some("retryable-agent"));
    let screen = harness.screen_to_string();
    let backing = exited
        .backing_path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok());
    assert!(
        screen.contains(marker),
        "restored transcript missing from screen:\n{screen}\nbacking: {backing:?}"
    );

    assert!(
        harness
            .editor_mut()
            .active_window_mut()
            .restart_terminal_buffer(buffer_id)
            .is_none(),
        "missing executable should still fail"
    );
    assert!(
        harness
            .editor()
            .active_window()
            .exited_terminal(buffer_id)
            .is_some(),
        "failed restart must leave the dormant record retryable"
    );
}

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn repeated_live_checkpoints_replace_the_visible_screen_tail() {
    if !pty_available() {
        return;
    }
    let temp_dir = TempDir::new().unwrap();
    let project_dir = temp_dir.path().join("project");
    std::fs::create_dir(&project_dir).unwrap();
    let dir_context = DirectoryContext::for_testing(temp_dir.path());
    let marker = "ONE-CHECKPOINT-SCREEN";
    let command = format!("printf '{marker}\\n'; exec sleep 30");
    let mut harness = EditorTestHarness::create(
        120,
        30,
        HarnessOptions::new()
            .with_config(session_config())
            .with_working_dir(project_dir.clone())
            .with_shared_dir_context(dir_context.clone())
            .without_empty_plugins_dir(),
    )
    .unwrap();
    spawn_agent_terminal(
        harness.editor_mut().active_window_mut(),
        &["sh", "-c", &command],
    );
    harness
        .wait_until(|h| h.screen_to_string().contains(marker))
        .unwrap();
    let window_id = harness.editor().active_window_id();
    let stable_id = harness.editor().active_window().stable_id.clone();

    harness.editor_mut().save_workspace_for(window_id).unwrap();
    harness.editor_mut().save_workspace_for(window_id).unwrap();

    let workspace = Workspace::load_by_id_in(&dir_context, &project_dir, &stable_id)
        .unwrap()
        .expect("saved workspace");
    let checkpoint = &workspace
        .terminals
        .first()
        .expect("saved terminal")
        .backing_path;
    let transcript = std::fs::read_to_string(checkpoint).unwrap();
    assert_eq!(transcript.matches(marker).count(), 1);
}

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn omp_exact_resume_wins_on_restore_when_generic_resume_is_disabled() {
    if !pty_available() {
        return;
    }
    let temp_dir = TempDir::new().unwrap();
    let project_dir = temp_dir.path().join("project");
    std::fs::create_dir(&project_dir).unwrap();
    let dir_context = DirectoryContext::for_testing(temp_dir.path());
    let launched = temp_dir.path().join("LAUNCHED");
    let resumed = temp_dir.path().join("RESUMED");
    let omp = temp_dir.path().join("omp");
    std::fs::write(
        &omp,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\n\
             case \"$1\" in\n\
               --resume) touch '{}'; exec sleep 30 ;;\n\
               *) touch '{}'; exec sleep 30 ;;\n\
             esac\n",
            resumed.display(),
            launched.display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _path_guard = crate::common::PathGuard::prepend_with_trusted_omp(temp_dir.path(), &omp);
    let session_id = "123e4567-e89b-42d3-a456-426614174099";
    let mut config = session_config();
    config.terminal.resume_agents = false;

    {
        let mut harness = EditorTestHarness::create(
            120,
            30,
            HarnessOptions::new()
                .with_config(config.clone())
                .with_working_dir(project_dir.clone())
                .with_shared_dir_context(dir_context.clone())
                .without_empty_plugins_dir(),
        )
        .unwrap();
        harness.editor_mut().set_session_mode(true);
        let terminal_id = spawn_resumable_agent_terminal(
            harness.editor_mut().active_window_mut(),
            &["omp", "--initial"],
            &["omp", "--resume", session_id],
        );
        harness
            .editor_mut()
            .active_window_mut()
            .terminal_companions
            .insert(terminal_id, TerminalCompanion::Omp);
        harness.wait_until(|_| launched.exists()).unwrap();
        harness.shutdown(true).unwrap();
    }
    std::fs::remove_file(&launched).unwrap();

    let mut harness = EditorTestHarness::create(
        120,
        30,
        HarnessOptions::new()
            .with_config(config)
            .with_working_dir(project_dir.clone())
            .with_shared_dir_context(dir_context.clone())
            .without_empty_plugins_dir(),
    )
    .unwrap();
    assert!(harness.startup(true, &[]).unwrap());
    harness.wait_until(|_| resumed.exists()).unwrap();
    assert!(
        !launched.exists(),
        "clean relaunch must not replace exact OMP resume"
    );
}
