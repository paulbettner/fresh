//! Race-shaped E2E coverage for Orchestrator's headless launch API.

#![cfg(feature = "plugins")]

use crate::common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness};
use crossterm::event::{KeyCode, KeyModifiers};
use std::fs;
use std::path::PathBuf;

const SIDECAR: &str = r#"/// <reference path="./lib/fresh.d.ts" />
/// @depends-on orchestrator
const editor = getEditor();

type LaunchResult = { workspaceId: string; windowId: number; root: string };
type OrchestratorApi = {
  runAgent(options: { windowId: number; agent?: string }): Promise<LaunchResult>;
  newWorkspace(options: {
    windowId?: number;
    path: string;
    name: string;
    worktree: boolean;
    agent?: string;
    teach?: boolean;
  }): Promise<LaunchResult>;
};

function api(): OrchestratorApi {
  const value = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!value) throw new Error("orchestrator API missing");
  return value;
}

async function headlessFocusRace(): Promise<void> {
  const target = editor.activeWindow();
  const pending = api().runAgent({ windowId: target });
  const result = await pending;
  const resultPath = editor.pathJoin(editor.getCwd(), ".headless-focus-result");
  editor.writeFile(editor.localPath(resultPath), `${target}:${result.windowId}`);
}
registerHandler("headlessFocusRace", headlessFocusRace);
editor.registerCommand("Test: Orchestrator Headless Focus", "", "headlessFocusRace");

async function terminalRejection(): Promise<void> {
  try {
    await api().runAgent({
      windowId: editor.activeWindow(),
      agent: "/definitely/missing/fresh-agent-binary",
    });
    editor.setStatus("FAIL terminal-rejection resolved");
  } catch (error) {
    editor.setStatus(`PASS terminal-rejection ${String(error)}`);
  }
}
registerHandler("terminalRejection", terminalRejection);
editor.registerCommand("Test: Orchestrator Terminal Rejection", "", "terminalRejection");

async function workspaceTerminalRejection(): Promise<void> {
  try {
    await api().newWorkspace({
      windowId: editor.activeWindow(),
      path: editor.getCwd(),
      name: "failed-workspace",
      worktree: false,
      agent: "/definitely/missing/codex",
      teach: true,
    });
    editor.setStatus("FAIL workspace-terminal-rejection resolved");
  } catch (error) {
    editor.setStatus(`PASS workspace-terminal-rejection ${String(error)}`);
  }
}
registerHandler("workspaceTerminalRejection", workspaceTerminalRejection);
editor.registerCommand(
  "Test: Orchestrator Workspace Terminal Rejection",
  "",
  "workspaceTerminalRejection",
);

function missingWorkspaceCaller(): void {
  void api().newWorkspace({
    path: editor.getCwd(),
    name: "must-not-create",
    worktree: false,
  }).then(
    () => editor.setStatus("FAIL missing-workspace-caller resolved"),
    (error) => editor.setStatus(`PASS missing-workspace-caller ${String(error)}`),
  );
}
registerHandler("missingWorkspaceCaller", missingWorkspaceCaller);
editor.registerCommand(
  "Test: Orchestrator Missing Workspace Caller",
  "",
  "missingWorkspaceCaller",
);

function scheduleHeadlessCreateWhileFormOpen(): void {
  const windowId = editor.activeWindow();
  void editor.delay(1_000).then(async () => {
    try {
      await api().newWorkspace({
        windowId,
        path: editor.getCwd(),
        name: "background-failure",
        worktree: false,
        agent: "/definitely/missing/codex",
      });
    } catch {
      // The missing executable is intentional; the form-isolation assertion is
      // that the background attempt never closes the human's open form.
    }
    editor.setStatus("PASS headless-form-isolation");
  });
}
registerHandler(
  "scheduleHeadlessCreateWhileFormOpen",
  scheduleHeadlessCreateWhileFormOpen,
);
editor.registerCommand(
  "Test: Orchestrator Headless Form Isolation",
  "",
  "scheduleHeadlessCreateWhileFormOpen",
);
"#;

fn setup() -> (tempfile::TempDir, PathBuf) {
    fresh::i18n::set_locale("en");
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().canonicalize().unwrap();
    let plugins = workspace.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");
    fs::write(plugins.join("orchestrator_async_probe.ts"), SIDECAR).unwrap();
    (temp, workspace)
}

fn wait_for_command(harness: &mut EditorTestHarness, name: &str) {
    harness
        .wait_until(|h| {
            let registry = h.editor().command_registry().read().unwrap();
            registry
                .get_all()
                .iter()
                .any(|command| command.get_localized_name() == name)
        })
        .unwrap();
}

fn run_command(harness: &mut EditorTestHarness, name: &str) {
    wait_for_command(harness, name);
    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness.type_text(name).unwrap();
    harness
        .wait_until(|h| h.screen_to_string().contains(name))
        .unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
}

#[test]
#[cfg_attr(target_os = "windows", ignore)] // creates a local PTY
fn headless_run_agent_stays_bound_to_captured_window_across_focus_change() {
    let (_temp, workspace) = setup();
    let other_root = workspace.join("other");
    fs::create_dir(&other_root).unwrap();
    let result_path = other_root.join(".headless-focus-result");
    let mut harness = EditorTestHarness::with_working_dir(140, 40, workspace).unwrap();
    harness.tick_and_render().unwrap();
    let original = harness.editor().active_window_id();
    let target = harness
        .editor_mut()
        .create_window_at(other_root, "other".into());
    harness.editor_mut().set_active_window(target);

    run_command(&mut harness, "Test: Orchestrator Headless Focus");
    harness.editor_mut().set_active_window(original);
    harness.wait_until(|_| result_path.exists()).unwrap();
    assert_eq!(
        fs::read_to_string(&result_path).unwrap(),
        format!("{}:{}", target.0, target.0),
        "headless launch must resolve against the command's captured window",
    );
    assert_eq!(harness.editor().active_window_id(), original);
}

#[test]
#[cfg_attr(target_os = "windows", ignore)] // creates a local PTY
fn headless_run_agent_propagates_terminal_creation_rejection() {
    let (_temp, workspace) = setup();
    let mut harness = EditorTestHarness::with_working_dir(140, 40, workspace).unwrap();
    harness.tick_and_render().unwrap();

    run_command(&mut harness, "Test: Orchestrator Terminal Rejection");
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("PASS terminal-rejection"))
        })
        .unwrap_or_else(|_| {
            panic!(
                "terminal creation rejection was swallowed: {:?}",
                harness.editor().get_status_message()
            )
        });
}

#[test]
#[cfg_attr(target_os = "windows", ignore)] // creates a local PTY
fn failed_workspace_creation_removes_its_new_prompt_file() {
    let (_temp, workspace) = setup();
    let mut harness = EditorTestHarness::with_working_dir(140, 40, workspace.clone()).unwrap();
    harness.tick_and_render().unwrap();

    run_command(
        &mut harness,
        "Test: Orchestrator Workspace Terminal Rejection",
    );
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("PASS workspace-terminal-rejection"))
        })
        .unwrap_or_else(|_| {
            panic!(
                "workspace terminal rejection did not settle: {:?}",
                harness.editor().get_status_message()
            )
        });
    assert!(
        !workspace.join("AGENTS.md").exists(),
        "a failed create must remove the prompt file it introduced"
    );
    assert_eq!(
        harness.editor().session_count(),
        1,
        "a rejected atomic create must not leave a window behind"
    );
}

#[test]
fn headless_new_workspace_requires_an_explicit_caller_window() {
    let (_temp, workspace) = setup();
    let mut harness = EditorTestHarness::with_working_dir(140, 40, workspace).unwrap();
    harness.tick_and_render().unwrap();

    run_command(&mut harness, "Test: Orchestrator Missing Workspace Caller");
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("PASS missing-workspace-caller"))
        })
        .unwrap_or_else(|_| {
            panic!(
                "path-only direct create was not rejected: {:?}",
                harness.editor().get_status_message()
            )
        });
    assert_eq!(harness.editor().session_count(), 1);
}

#[test]
#[cfg_attr(target_os = "windows", ignore)] // attempts a local PTY creation
fn headless_workspace_create_does_not_close_a_human_form() {
    let (_temp, workspace) = setup();
    let mut harness = EditorTestHarness::with_working_dir(140, 40, workspace).unwrap();
    harness.tick_and_render().unwrap();

    run_command(&mut harness, "Test: Orchestrator Headless Form Isolation");
    harness.tick_and_render().unwrap();
    run_command(&mut harness, "Orchestrator: New Workspace");
    harness
        .wait_until(|h| {
            h.screen_to_string()
                .contains("ORCHESTRATOR :: New Workspace")
        })
        .unwrap();
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("PASS headless-form-isolation"))
        })
        .unwrap();
    harness.assert_screen_contains("ORCHESTRATOR :: New Workspace");
}
