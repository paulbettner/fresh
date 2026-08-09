//! Git gets the final dirty check when a failed create rolls back its worktree.

#![cfg(all(unix, feature = "plugins"))]

mod common;

use common::dormant_ssh::isolated_dir_context;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use common::PathGuard;
use crossterm::event::{KeyCode, KeyModifiers};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

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

async function createDirtyRollback(): Promise<void> {
  const api = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!api) throw new Error("orchestrator API missing");
  try {
    await api.newWorkspace({
      windowId: editor.activeWindow(),
      path: editor.getCwd(),
      name: "dirty-after-check",
      worktree: true,
      agent: "/definitely/missing/fresh-agent",
    });
    editor.setStatus("FAIL dirty rollback resolved");
  } catch (error) {
    editor.setStatus(`PASS dirty rollback ${String(error)}`);
  }
}
registerHandler("createDirtyRollback", createDirtyRollback);
editor.registerCommand("Test: Dirty Create Rollback", "", "createDirtyRollback");
"#;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn dirty_after_precheck_refuses_worktree_rollback_and_keeps_journal() {
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let repo = base.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Test User"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    fs::write(repo.join("README.md"), "rollback\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-qm", "initial"]);
    let repo = repo.canonicalize().unwrap();
    let plugins = repo.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");
    fs::write(plugins.join("dirty_rollback_probe.ts"), SIDECAR).unwrap();

    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    assert!(real_git.status.success());
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let shim_dir = tempfile::tempdir().unwrap();
    let removed_target = base.path().join("rollback-target");
    let shim = shim_dir.path().join("git");
    fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"worktree remove"*)
    target=
    for arg in "$@"; do target="$arg"; done
    printf '%s' "$target" > "{removed_target}"
    printf 'new user data\n' > "$target/user-data.txt"
    ;;
esac
exec "{real_git}" "$@"
"#,
            removed_target = removed_target.display(),
            real_git = real_git.trim(),
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&shim).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&shim, permissions).unwrap();
    let _path_guard = PathGuard::prepend(shim_dir.path());

    let mut harness = EditorTestHarness::create(
        150,
        45,
        HarnessOptions::new()
            .with_working_dir(repo.clone())
            .with_shared_dir_context(dir_context.clone()),
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
                .any(|command| command.get_localized_name() == "Test: Dirty Create Rollback")
        })
        .unwrap();
    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    harness.wait_for_prompt().unwrap();
    harness.type_text("Test: Dirty Create Rollback").unwrap();
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("PASS dirty rollback"))
                && removed_target.exists()
        })
        .unwrap_or_else(|_| {
            panic!(
                "dirty rollback did not settle: {:?}\n{}",
                harness.editor().get_status_message(),
                harness.screen_to_string(),
            )
        });

    let worktree = fs::read_to_string(&removed_target).unwrap();
    let worktree = Path::new(worktree.trim());
    assert!(
        worktree.join("user-data.txt").exists(),
        "Git rollback deleted new user data"
    );
    assert!(
        git(&repo, &["worktree", "list", "--porcelain"])
            .contains(worktree.to_string_lossy().as_ref()),
        "refused rollback detached the dirty worktree",
    );
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(
            dir_context
                .data_dir
                .join("orchestrator/state/orchestrator.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(state.as_object().unwrap().iter().any(|(key, value)| {
        key.starts_with("orchestrator.create_journal:")
            && value["worktree"]["root"].as_str() == Some(worktree.to_string_lossy().as_ref())
    }));
}
