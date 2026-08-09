//! Dismissing or losing ownership of a pending Orchestrator create must fence
//! every awaited git stage.
//!
//! The git shim below pauses the worker during canonical classification and an
//! effectful in-place checkout. Dismissal must win without a late window, the
//! checkout must roll back to its exact predecessor, and a stolen attempt lease
//! must reject without writing a stale error after ownership is lost.

#![cfg(all(unix, feature = "plugins"))]

mod common;

use common::dormant_ssh::isolated_dir_context;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use common::PathGuard;
use crossterm::event::{KeyCode, KeyModifiers};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const SIDECAR: &str = r#"/// <reference path="./lib/fresh.d.ts" />
/// @depends-on orchestrator
const editor = getEditor();

type LaunchResult = { workspaceId: string; windowId: number; root: string };
type OrchestratorApi = {
  newWorkspace(options: {
    windowId: number;
    path: string;
    name: string;
    branch?: string;
    worktree: boolean;
  }): Promise<LaunchResult>;
};

async function begin(stage: "classify" | "checkout" | "lease"): Promise<void> {
  const api = editor.getPluginApi("orchestrator") as OrchestratorApi | null;
  if (!api) throw new Error("orchestrator API missing");
  const target = editor.getEnv("FRESH_ORCHESTRATOR_CANCEL_TARGET");
  if (!target) throw new Error("cancellation target missing");
  const options: {
    windowId: number;
    path: string;
    name: string;
    branch?: string;
    worktree: boolean;
  } = {
    windowId: editor.activeWindow(),
    path: target,
    name: `cancel-${stage}`,
    worktree: false,
  };
  if (stage === "checkout") options.branch = "feature";
  try {
    await api.newWorkspace(options);
    editor.setStatus(`FAIL cancellation ${stage} resolved`);
  } catch (error) {
    editor.setStatus(`CANCELLED ${stage}: ${String(error)}`);
  }
}

registerHandler("cancelDuringCheckout", () => begin("checkout"));
registerHandler("cancelDuringClassification", () => begin("classify"));
registerHandler("loseAttemptLease", () => begin("lease"));
editor.registerCommand(
  "Test: Cancel Create During Checkout",
  "",
  "cancelDuringCheckout",
);
editor.registerCommand(
  "Test: Cancel Create During Classification",
  "",
  "cancelDuringClassification",
);
editor.registerCommand("Test: Lose Create Attempt Lease", "", "loseAttemptLease");
"#;

struct GitGate {
    _shim_dir: tempfile::TempDir,
    _path_guard: PathGuard,
    control: PathBuf,
    gate: PathBuf,
    marker: PathBuf,
    done: PathBuf,
}

impl GitGate {
    fn install(base: &Path) -> Self {
        let real_git = Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        assert!(real_git.status.success());
        let real_git = String::from_utf8(real_git.stdout).unwrap();
        let shim_dir = tempfile::tempdir().unwrap();
        let control = base.join("git-stage");
        let gate = base.join("git-gate");
        let marker = base.join("git-blocked");
        let done = base.join("git-complete");
        let shim = shim_dir.path().join("git");
        fs::write(
            &shim,
            format!(
                r#"#!/bin/sh
stage=$(cat "{control}" 2>/dev/null)
block=
case "$stage" in
  classify|lease) case "$*" in *"rev-parse --show-toplevel"*) block=1;; esac;;
  checkout) case "$*" in *"checkout feature"*) block=1;; esac;;
esac
if [ -n "$block" ]; then
  : > "{marker}"
  while [ -e "{gate}" ]; do sleep 0.02; done
  "{real_git}" "$@"
  status=$?
  : > "{done}"
  exit $status
fi
exec "{real_git}" "$@"
"#,
                control = control.display(),
                marker = marker.display(),
                gate = gate.display(),
                done = done.display(),
                real_git = real_git.trim(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&shim).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&shim, permissions).unwrap();
        let path_guard = PathGuard::prepend(shim_dir.path());
        Self {
            _shim_dir: shim_dir,
            _path_guard: path_guard,
            control,
            gate,
            marker,
            done,
        }
    }

    fn arm(&self, stage: &str) -> ArmedGitGate<'_> {
        let _ = fs::remove_file(&self.marker);
        let _ = fs::remove_file(&self.done);
        fs::write(&self.control, stage).unwrap();
        fs::write(&self.gate, "blocked\n").unwrap();
        ArmedGitGate {
            gate: self,
            released: false,
        }
    }

    fn release(&self) {
        let _ = fs::remove_file(&self.gate);
    }
}

struct ArmedGitGate<'a> {
    gate: &'a GitGate,
    released: bool,
}

impl ArmedGitGate<'_> {
    fn release(&mut self) {
        self.gate.release();
        self.released = true;
    }
}

impl Drop for ArmedGitGate<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.gate.release();
        }
    }
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

fn setup_project() -> (tempfile::TempDir, PathBuf, PathBuf) {
    fresh::i18n::set_locale("en");
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("origin.git");
    fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare", "-q"]);

    // Keep the editor's launch workspace separate from the repository whose
    // branch the transaction mutates. The in-place safety contract correctly
    // refuses to switch a checkout already occupied by a live window.
    let host = temp.path().join("host-workspace");
    fs::create_dir(&host).unwrap();
    let plugins = host.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");
    fs::write(plugins.join("cancellation_probe.ts"), SIDECAR).unwrap();

    let project = temp.path().join("cancel-project");
    fs::create_dir(&project).unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["config", "user.name", "Test User"]);
    git(&project, &["config", "user.email", "test@example.com"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    fs::write(project.join("README.md"), "cancellation\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-qm", "initial"]);
    git(
        &project,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);
    git(&project, &["checkout", "-q", "-b", "feature"]);
    fs::write(project.join("FEATURE.md"), "feature\n").unwrap();
    git(&project, &["add", "FEATURE.md"]);
    git(&project, &["commit", "-qm", "feature"]);
    git(&project, &["push", "-q", "-u", "origin", "feature"]);
    git(&project, &["checkout", "-q", "main"]);
    (
        temp,
        host.canonicalize().unwrap(),
        project.canonicalize().unwrap(),
    )
}

const WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const WAIT_POLL: Duration = Duration::from_millis(20);

fn wait_until_bounded(
    harness: &mut EditorTestHarness,
    description: &str,
    mut condition: impl FnMut(&EditorTestHarness) -> bool,
) {
    let start = Instant::now();
    loop {
        harness.tick_and_render().unwrap();
        if condition(harness) {
            return;
        }
        if start.elapsed() >= WAIT_TIMEOUT {
            panic!(
                "timed out waiting for {description} after {:.1}s; status: {:?}\n{}",
                start.elapsed().as_secs_f64(),
                harness.editor().get_status_message(),
                harness.screen_to_string()
            );
        }
        std::thread::sleep(WAIT_POLL);
        harness.advance_time(WAIT_POLL);
    }
}

fn open_dock(harness: &mut EditorTestHarness) {
    run_command(harness, "Orchestrator: Toggle Dock");
    wait_until_bounded(harness, "orchestrator dock to open", |h| {
        h.screen_to_string().contains("Orchestrator") && h.editor().is_dock_focused()
    });
}

fn run_command(harness: &mut EditorTestHarness, name: &str) {
    wait_until_bounded(harness, &format!("command {name:?} to register"), |h| {
        h.editor()
            .command_registry()
            .read()
            .unwrap()
            .get_all()
            .iter()
            .any(|command| command.get_localized_name() == name)
    });
    harness
        .send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    wait_until_bounded(harness, "command palette to open", |h| {
        h.editor().is_prompting()
    });
    harness.type_text(name).unwrap();
    wait_until_bounded(
        harness,
        &format!("command {name:?} to be selectable"),
        |h| h.screen_to_string().matches(name).count() >= 2,
    );
    harness
        .send_key(KeyCode::Enter, KeyModifiers::NONE)
        .unwrap();
}

fn dismiss_blocked_create(
    harness: &mut EditorTestHarness,
    gate: &GitGate,
    stage: &str,
    command: &str,
) {
    let mut armed_gate = gate.arm(stage);
    run_command(harness, command);
    wait_until_bounded(harness, &format!("{stage} git stage to block"), |_| {
        gate.marker.exists()
    });

    let label = format!("cancel-{stage}");
    wait_until_bounded(harness, &format!("pending row {label:?}"), |h| {
        h.screen_to_string().contains(&label)
    });
    let (_, row) = harness.find_text_on_screen(&label).unwrap_or_else(|| {
        panic!(
            "pending row {label} missing:\n{}",
            harness.screen_to_string()
        )
    });
    harness.mouse_right_click(4, row).unwrap();
    wait_until_bounded(harness, "Dismiss action to appear", |h| {
        h.screen_to_string().contains("Dismiss")
    });
    let (col, row) = harness.find_text_on_screen("Dismiss").unwrap();
    harness.mouse_click(col, row).unwrap();
    wait_until_bounded(
        harness,
        &format!("{stage} dismissal to reject the headless create"),
        |h| {
            !h.screen_to_string().contains(&label)
                && h.editor()
                    .get_status_message()
                    .is_some_and(|status| status.contains(&format!("CANCELLED {stage}")))
        },
    );

    armed_gate.release();
    wait_until_bounded(
        harness,
        &format!("{stage} git stage to complete after release"),
        |_| gate.done.exists(),
    );
    harness.wait_for_async_quiescence(3).unwrap();
    assert_eq!(
        harness.editor().session_count(),
        1,
        "dismissed {stage} create spawned a late window"
    );
}

fn lose_blocked_create_lease(harness: &mut EditorTestHarness, gate: &GitGate) {
    const STAGE: &str = "lease";
    const LABEL: &str = "cancel-lease";
    let mut armed_gate = gate.arm(STAGE);
    run_command(harness, "Test: Lose Create Attempt Lease");
    wait_until_bounded(harness, "lease git stage to block", |_| {
        gate.marker.exists()
    });
    wait_until_bounded(harness, "lease attempt to become durable", |h| {
        h.screen_to_string().contains(LABEL)
            && h.editor()
                .plugin_global_state()
                .get("orchestrator")
                .is_some_and(|state| {
                    state.values().any(|value| {
                        value.get("label").and_then(serde_json::Value::as_str) == Some(LABEL)
                    })
                })
    });

    let attempt_id = harness
        .editor()
        .plugin_global_state()
        .get("orchestrator")
        .and_then(|state| {
            state.values().find_map(|value| {
                if value.get("label").and_then(serde_json::Value::as_str) == Some(LABEL) {
                    value.get("attemptId")?.as_str().map(str::to_owned)
                } else {
                    None
                }
            })
        })
        .expect("blocked attempt must be durable before its lease is stolen");
    let owner = std::env::temp_dir()
        .join("fresh-orchestrator-locks")
        .join(format!("k-v-create-attempt%3A{attempt_id}.lock"))
        .join("owner.json");
    assert!(
        owner.exists(),
        "attempt lease owner missing: {}",
        owner.display()
    );
    fs::write(
        &owner,
        serde_json::to_vec(&json!({
            "key": format!("create-attempt:{attempt_id}"),
            "token": "successor-test-token",
        }))
        .unwrap(),
    )
    .unwrap();

    armed_gate.release();
    wait_until_bounded(harness, "lease loss to reject the create", |h| {
        !h.screen_to_string().contains(LABEL)
            && h.editor()
                .get_status_message()
                .is_some_and(|status| status.contains("CANCELLED lease"))
    });
    wait_until_bounded(harness, "lease git stage to complete after release", |_| {
        gate.done.exists()
    });
    harness.wait_for_async_quiescence(3).unwrap();
    assert_eq!(
        harness.editor().session_count(),
        1,
        "lease-lost create spawned a late window"
    );
    let record = harness
        .editor()
        .plugin_global_state()
        .get("orchestrator")
        .and_then(|state| state.get(&format!("orchestrator.pending:{attempt_id}")))
        .expect("the successor-owned durable attempt must not be deleted");
    assert_eq!(record["phase"], "creating");
    assert_ne!(
        record["message"], "workspace transaction claim was lost",
        "the former owner must not persist after losing its attempt lease"
    );
}

#[test]
fn dismissal_fences_classification_checkout_and_lease_loss() {
    let (temp, host, project) = setup_project();
    let dir_context = isolated_dir_context(temp.path());
    std::env::set_var("FRESH_ORCHESTRATOR_CANCEL_TARGET", &project);
    let gate = GitGate::install(temp.path());
    let mut harness = EditorTestHarness::create(
        160,
        50,
        HarnessOptions::new()
            .with_working_dir(host)
            .with_shared_dir_context(dir_context),
    )
    .unwrap();
    harness.tick_and_render().unwrap();
    open_dock(&mut harness);

    dismiss_blocked_create(
        &mut harness,
        &gate,
        "checkout",
        "Test: Cancel Create During Checkout",
    );
    assert_eq!(
        git(&project, &["branch", "--show-current"]),
        "main",
        "dismissal after checkout must restore the exact original branch"
    );

    dismiss_blocked_create(
        &mut harness,
        &gate,
        "classify",
        "Test: Cancel Create During Classification",
    );
    assert_eq!(git(&project, &["branch", "--show-current"]), "main");
    lose_blocked_create_lease(&mut harness, &gate);
    assert_eq!(git(&project, &["branch", "--show-current"]), "main");
    assert!(
        !project.join("AGENTS.md").exists(),
        "dismissed creates must not leave prompt files behind"
    );
}
