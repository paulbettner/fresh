//! A slow background remote attach preserves focus at installation time.

#![cfg(all(target_os = "linux", feature = "plugins"))]

mod common;

use common::dormant_ssh::{canonical_mkdir, ensure_slow_fake_ssh_on_path, isolated_dir_context};
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use crossterm::event::{KeyCode, KeyModifiers};
use std::fs;

const SIDECAR: &str = r#"/// <reference path="./lib/fresh.d.ts" />
/// @depends-on orchestrator
const editor = getEditor();

type Workspace = { windowId: number; backend?: string };
type OrchestratorApi = { listWorkspaces(): Workspace[] };

async function verifyRemoteOwner(): Promise<void> {
  const api = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!api) throw new Error("orchestrator API missing");
  const active = editor.activeWindow();
  let rows: Workspace[] = [];
  for (let attempt = 0; attempt < 40; attempt++) {
    rows = api.listWorkspaces();
    const activeRow = rows.find((row) => row.windowId === active);
    const remoteRows = rows.filter((row) => row.backend === "ssh");
    if (!activeRow?.backend && remoteRows.length === 1 && remoteRows[0].windowId !== active) {
      editor.setStatus(`PASS remote-owner active=${active} ${JSON.stringify(rows)}`);
      return;
    }
    await editor.delay(50);
  }
  editor.setStatus(`FAIL remote-owner active=${active} ${JSON.stringify(rows)}`);
}

registerHandler("verifyRemoteOwner", verifyRemoteOwner);
editor.registerCommand("Test: Verify Remote Owner", "", "verifyRemoteOwner");
"#;

fn run_command(harness: &mut EditorTestHarness, name: &str) {
    harness
        .wait_until(|h| {
            h.editor()
                .command_registry()
                .read()
                .unwrap()
                .get_all()
                .iter()
                .any(|command| command.get_localized_name() == name)
        })
        .unwrap();
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
fn slow_background_remote_attach_does_not_restore_stale_focus() {
    ensure_slow_fake_ssh_on_path();
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let project = canonical_mkdir(base.path(), "project");
    let other_root = canonical_mkdir(base.path(), "other");
    std::env::set_var("FAKE_SSH_SLOW_READY_DELAY", "1.0");
    std::env::set_var("FAKE_SSH_SLOW_METHODS", "");

    let plugins = project.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");
    fs::write(plugins.join("remote_owner_probe.ts"), SIDECAR).unwrap();

    let mut harness = EditorTestHarness::create(
        160,
        50,
        HarnessOptions::new()
            .with_working_dir(project)
            .with_shared_dir_context(dir_context),
    )
    .unwrap();
    harness.tick_and_render().unwrap();
    harness
        .wait_until(|h| {
            let registry = h.editor().command_registry().read().unwrap();
            registry
                .get_all()
                .iter()
                .any(|command| command.get_localized_name() == "Orchestrator: New Workspace")
        })
        .unwrap();

    let launch_window = harness.editor().active_window_id();
    let other_window = harness
        .editor_mut()
        .create_window_at(other_root, "other".into());
    harness.editor_mut().set_active_window(launch_window);

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

    // Switch Local → SSH, focus Host, and supply a deterministic fake host.
    harness
        .send_key(KeyCode::BackTab, KeyModifiers::NONE)
        .unwrap();
    harness
        .send_key(KeyCode::Right, KeyModifiers::NONE)
        .unwrap();
    harness
        .wait_until(|h| h.screen_to_string().contains("Host  ("))
        .unwrap();
    harness.send_key(KeyCode::Tab, KeyModifiers::NONE).unwrap();
    harness.type_text("slow-host").unwrap();

    let (column, row) = harness
        .find_text_on_screen("Create in Background")
        .expect("background create button");
    harness.mouse_click(column, row).unwrap();
    harness
        .wait_until(|h| h.screen_to_string().contains("Connecting"))
        .unwrap();

    // Navigation happens while the attach is still waiting on the delayed
    // ready line. Completion must preserve THIS window, not the launch window
    // captured a second earlier.
    harness.editor_mut().set_active_window(other_window);
    harness
        .wait_until(|h| h.editor().session_count() == 3)
        .unwrap();
    assert_eq!(
        harness.editor().active_window_id(),
        other_window,
        "background remote attach restored stale submit-time focus"
    );
    run_command(&mut harness, "Test: Verify Remote Owner");
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("PASS remote-owner"))
        })
        .unwrap_or_else(|_| {
            panic!(
                "remote facet was not bound to the born window: {:?}",
                harness.editor().get_status_message()
            )
        });
}
