//! Headless Orchestrator creates must reserve auto-names before their first await.

#![cfg(all(unix, feature = "plugins"))]

mod common;

use common::dormant_ssh::isolated_dir_context;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use crossterm::event::{KeyCode, KeyModifiers};
use portable_pty::{native_pty_system, PtySize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

const SIDECAR: &str = r#"/// <reference path="./lib/fresh.d.ts" />
/// @depends-on orchestrator
const editor = getEditor();

type LaunchResult = { workspaceId: string; windowId: number; root: string };
type WorkspaceOptions = {
  windowId: number;
  path?: string;
  name?: string;
  newBranch?: string;
  worktree?: boolean;
  idempotencyKey?: string;
};
type OrchestratorApi = {
  newWorkspace(options: WorkspaceOptions): Promise<LaunchResult>;
};

async function parallelUnnamedCreates(): Promise<void> {
  const api = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!api) throw new Error("orchestrator API missing");
  const windowId = editor.activeWindow();
  try {
    const results = await Promise.all([
      api.newWorkspace({ windowId }),
      api.newWorkspace({ windowId }),
    ]);
    const distinct = results[0].root !== results[1].root &&
      results[0].workspaceId !== results[1].workspaceId;
    editor.setStatus(
      `${distinct ? "PASS" : "FAIL"} parallel-names ${JSON.stringify(results)}`,
    );
  } catch (error) {
    editor.setStatus(`FAIL parallel-names ${String(error)}`);
  }
}
registerHandler("parallelUnnamedCreates", parallelUnnamedCreates);

async function idempotentCreate(): Promise<void> {
  const api = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!api) throw new Error("orchestrator API missing");
  const windowId = editor.activeWindow();
  try {
    const options = { windowId, idempotencyKey: "same-create-request" };
    const results = await Promise.all([
      api.newWorkspace(options),
      api.newWorkspace(options),
    ]);
    let conflictRejected = false;
    try {
      await api.newWorkspace({
        windowId,
        name: "different-request",
        idempotencyKey: "same-create-request",
      });
    } catch {
      conflictRejected = true;
    }
    const same = results[0].root === results[1].root &&
      results[0].workspaceId === results[1].workspaceId &&
      results[0].windowId === results[1].windowId && conflictRejected;
    editor.setStatus(
      `${same ? "PASS" : "FAIL"} idempotent-create ${JSON.stringify(results)}`,
    );
  } catch (error) {
    editor.setStatus(`FAIL idempotent-create ${String(error)}`);
  }
}
registerHandler("idempotentCreate", idempotentCreate);

async function concurrentBranchClaim(): Promise<void> {
  const api = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!api) throw new Error("orchestrator API missing");
  const path = editor.getEnv("FRESH_ORCH_BRANCH_PROJECT");
  if (!path) throw new Error("branch project missing");
  const launch = (name: string) => api.newWorkspace({
    windowId: editor.activeWindow(),
    path,
    name,
    newBranch: "shared/create-claim",
    worktree: true,
  }).then(
    (value) => ({ ok: true, value }),
    (error) => ({ ok: false, error: String(error) }),
  );
  const results = await Promise.all([launch("branch-a"), launch("branch-b")]);
  const winners = results.filter((result) => result.ok);
  editor.setStatus(
    `${winners.length === 1 ? "PASS" : "FAIL"} branch-cas ${JSON.stringify(results)}`,
  );
}
registerHandler("concurrentBranchClaim", concurrentBranchClaim);

async function createFromAliasSubdir(): Promise<void> {
  const api = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!api) throw new Error("orchestrator API missing");
  const path = editor.getEnv("FRESH_ORCH_ALIAS_SUBDIR");
  if (!path) throw new Error("alias subdir missing");
  try {
    const result = await api.newWorkspace({
      windowId: editor.activeWindow(),
      path,
      name: "alias-root",
      worktree: false,
    });
    editor.setStatus(`PASS alias-root ${JSON.stringify(result)}`);
  } catch (error) {
    editor.setStatus(`FAIL alias-root ${String(error)}`);
  }
}
registerHandler("createFromAliasSubdir", createFromAliasSubdir);
editor.registerCommand(
  "Test: Orchestrator Idempotent Create",
  "",
  "idempotentCreate",
);
editor.registerCommand(
  "Test: Orchestrator Parallel Unnamed Creates",
  "",
  "parallelUnnamedCreates",
);
editor.registerCommand(
  "Test: Orchestrator Concurrent Branch Claim",
  "",
  "concurrentBranchClaim",
);
editor.registerCommand(
  "Test: Orchestrator Alias Subdir Create",
  "",
  "createFromAliasSubdir",
);
"#;

fn pty_available() -> bool {
    let system = native_pty_system();
    system
        .openpty(PtySize {
            rows: 4,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

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
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn run_command(harness: &mut EditorTestHarness, name: &str) {
    harness
        .wait_until(|h| {
            let registry = h.editor().command_registry().read().unwrap();
            registry
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
fn parallel_unnamed_creates_get_distinct_persisted_names_and_roots() {
    if !pty_available() {
        eprintln!("skipping: no PTY available in this environment");
        return;
    }
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let state_file = dir_context
        .data_dir
        .join("orchestrator/state/orchestrator.json");
    let project = base.path().join("parallel-project");
    fs::create_dir(&project).unwrap();
    git(&project, &["init"]);
    git(&project, &["config", "user.email", "test@example.com"]);
    git(&project, &["config", "user.name", "Test"]);
    fs::write(project.join("README.md"), "parallel\n").unwrap();
    git(&project, &["add", "README.md"]);
    git(&project, &["commit", "-m", "init"]);
    let project = project.canonicalize().unwrap();

    let alias_repo = base.path().join("alias-project");
    fs::create_dir(&alias_repo).unwrap();
    git(&alias_repo, &["init"]);
    git(&alias_repo, &["config", "user.email", "test@example.com"]);
    git(&alias_repo, &["config", "user.name", "Test"]);
    fs::write(alias_repo.join("README.md"), "alias\n").unwrap();
    git(&alias_repo, &["add", "README.md"]);
    git(&alias_repo, &["commit", "-m", "init"]);
    let alias_repo = alias_repo.canonicalize().unwrap();
    let alias_subdir = alias_repo.join("nested/alias");
    fs::create_dir_all(&alias_subdir).unwrap();
    std::env::set_var("FRESH_ORCH_BRANCH_PROJECT", &project);
    std::env::set_var("FRESH_ORCH_ALIAS_SUBDIR", &alias_subdir);

    let plugins = project.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");
    fs::write(plugins.join("parallel_create_probe.ts"), SIDECAR).unwrap();

    let mut harness = EditorTestHarness::create(
        160,
        50,
        HarnessOptions::new()
            .with_working_dir(project.clone())
            .with_shared_dir_context(dir_context),
    )
    .unwrap();
    harness.tick_and_render().unwrap();
    run_command(&mut harness, "Test: Orchestrator Parallel Unnamed Creates");

    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("parallel-names"))
        })
        .unwrap();

    let status = harness.editor().get_status_message().unwrap();
    assert!(
        status.starts_with("PASS parallel-names "),
        "parallel creates aliased or failed: {status}; state: {}",
        fs::read_to_string(&state_file).unwrap_or_else(|error| error.to_string())
    );
    let json = status
        .split_once("PASS parallel-names ")
        .expect("PASS status payload")
        .1;
    let results: Value = serde_json::from_str(json).unwrap();
    let roots = results.as_array().unwrap();
    assert_eq!(roots.len(), 2);
    let first = roots[0]["root"].as_str().unwrap();
    let second = roots[1]["root"].as_str().unwrap();
    assert_ne!(first, second);
    assert!(Path::new(first).is_dir(), "first worktree missing: {first}");
    assert!(
        Path::new(second).is_dir(),
        "second worktree missing: {second}"
    );
    assert_eq!(harness.editor().session_count(), 3);

    run_command(&mut harness, "Test: Orchestrator Idempotent Create");
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("idempotent-create"))
        })
        .unwrap();

    let status = harness.editor().get_status_message().unwrap();
    assert!(
        status.starts_with("PASS idempotent-create "),
        "duplicate idempotency key repeated or mismatched the mutation: {status}"
    );
    let json = status
        .split_once("PASS idempotent-create ")
        .expect("PASS idempotency status payload")
        .1;
    let results: Value = serde_json::from_str(json).unwrap();
    let results = results.as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["root"], results[1]["root"]);
    assert_eq!(results[0]["workspaceId"], results[1]["workspaceId"]);
    assert_eq!(results[0]["windowId"], results[1]["windowId"]);
    assert_eq!(harness.editor().session_count(), 4);

    run_command(&mut harness, "Test: Orchestrator Concurrent Branch Claim");
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("branch-cas"))
        })
        .unwrap();
    let status = harness.editor().get_status_message().unwrap();
    assert!(
        status.starts_with("PASS branch-cas "),
        "exactly one create must win an explicit branch CAS: {status}"
    );
    assert_eq!(
        git(
            &project,
            &["rev-parse", "--verify", "refs/heads/shared/create-claim"]
        ),
        git(&project, &["rev-parse", "HEAD"]),
        "the losing attempt must not delete the winning branch"
    );
    assert_eq!(harness.editor().session_count(), 5);

    run_command(&mut harness, "Test: Orchestrator Alias Subdir Create");
    harness
        .wait_until(|h| {
            h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("alias-root"))
        })
        .unwrap();
    let status = harness.editor().get_status_message().unwrap();
    assert!(
        status.starts_with("PASS alias-root "),
        "alias create failed: {status}"
    );
    let result: Value =
        serde_json::from_str(status.split_once("PASS alias-root ").unwrap().1).unwrap();
    assert_eq!(
        Path::new(result["root"].as_str().unwrap()),
        alias_repo.as_path(),
        "an alias subdirectory must lease and create at the canonical git toplevel"
    );
    assert_eq!(harness.editor().session_count(), 6);
}
