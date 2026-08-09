//! A create outcome must settle even when persisting its terminal error fails.

#![cfg(all(target_os = "linux", feature = "plugins"))]

mod common;

use common::dormant_ssh::isolated_dir_context;
use common::fail_retirement_write_fs::FailRetirementWriteFs;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use crossterm::event::{KeyCode, KeyModifiers};
use std::fs;
use std::sync::Arc;

const SIDECAR: &str = r#"/// <reference path="./lib/fresh.d.ts" />
/// @depends-on orchestrator
const editor = getEditor();

type LaunchResult = { workspaceId: string; windowId: number; root: string };
type OrchestratorApi = {
  newWorkspace(options: {
    windowId: number;
    path: string;
    name: string;
    worktree: boolean;
    agent: string;
  }): Promise<LaunchResult>;
};

async function createWithRejectedErrorPersistence(): Promise<void> {
  const api = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!api) throw new Error("orchestrator API missing");
  try {
    await api.newWorkspace({
      windowId: editor.activeWindow(),
      path: editor.getCwd(),
      name: "pending-error-persistence",
      worktree: false,
      agent: "/definitely/missing/fresh-agent",
    });
    editor.setStatus("FAIL pending error persistence resolved");
  } catch (error) {
    editor.setStatus(`PASS pending error persistence ${String(error)}`);
  }
}
registerHandler("createWithRejectedErrorPersistence", createWithRejectedErrorPersistence);
editor.registerCommand(
  "Test: Create Error Persistence Failure",
  "",
  "createWithRejectedErrorPersistence",
);
"#;

#[test]
fn create_rejection_settles_when_error_persistence_fails() {
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let root = base.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let plugins = root.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");
    fs::write(plugins.join("pending_error_probe.ts"), SIDECAR).unwrap();

    let state_dir = dir_context.data_dir.join("orchestrator/state");
    let fault_fs = Arc::new(FailRetirementWriteFs::pending_error(
        state_dir.clone(),
        root.to_string_lossy().into_owned(),
    ));
    let mut harness = EditorTestHarness::create(
        150,
        45,
        HarnessOptions::new()
            .with_working_dir(root.clone())
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
                .any(|command| {
                    command.get_localized_name() == "Test: Create Error Persistence Failure"
                })
        })
        .unwrap();

    fault_fs.arm();
    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness
        .type_text("Test: Create Error Persistence Failure")
        .unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("PASS pending error persistence"))
        })
        .unwrap_or_else(|_| {
            panic!(
                "create outcome never settled after persistence failure: {:?}\n{}",
                harness.editor().get_status_message(),
                harness.screen_to_string(),
            )
        });

    assert!(
        !fault_fs.is_armed(),
        "pending error persistence fault was not exercised"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(state_dir.join("orchestrator.json")).unwrap()).unwrap();
    assert!(state.as_object().unwrap().iter().any(|(key, value)| {
        key.starts_with("orchestrator.pending:")
            && value["spec"]["projectPath"].as_str() == Some(root.to_string_lossy().as_ref())
            && value["phase"].as_str() == Some("creating")
    }));
}
