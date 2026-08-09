//! E2E: local workspace creation recovery is both resumable and idempotent.
//!
//! We seed two crash states before firing `ready`: an interrupted legacy
//! request with no effects, which must return as a paused row, and a v2 request
//! whose atomically-created workspace already carries `create_attempt` in its
//! persisted session state. The latter must be recognized as committed and
//! retired without creating or surfacing a duplicate row.
//!
//! This drives the real `recoverPendingWorkspaces` path and asserts on both its
//! rendered paused row and its durable per-attempt cleanup.
//!
//! Single test in this binary: `isolated_dir_context` sets the process-global
//! `XDG_DATA_HOME`, keeping all persistence inside the per-test temp tree.
#![cfg(all(target_os = "linux", feature = "plugins"))]

mod common;

use common::dormant_ssh::isolated_dir_context;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use crossterm::event::{KeyCode, KeyModifiers};
use fresh_core::api::PluginCommand;
use serde_json::{json, Value};

fn base36(mut value: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut output = Vec::new();
    while value > 0 {
        output.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    output.reverse();
    String::from_utf8(output).unwrap()
}

fn legacy_attempt_id(index: usize, value: &Value) -> String {
    let input = format!("{index}:{}", serde_json::to_string(value).unwrap());
    let mut hash = 0x811c9dc5_u32;
    for unit in input.encode_utf16() {
        hash = (hash ^ u32::from(unit)).wrapping_mul(0x01000193);
    }
    format!("legacy-{}-{}", base36(index as u32), base36(hash))
}

#[test]
fn interrupted_local_workspace_is_restored_paused_on_launch() {
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let data_dir = dir_context.data_dir.clone();
    let project = base.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();

    let plugins_dir = project.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    copy_plugin_lib(&plugins_dir);
    copy_plugin(&plugins_dir, "orchestrator");

    let mut h = EditorTestHarness::create(
        160,
        50,
        HarnessOptions::new()
            .with_working_dir(project.clone())
            .with_shared_dir_context(dir_context),
    )
    .unwrap();
    h.tick_and_render().unwrap();
    h.wait_until(|h| {
        let reg = h.editor().command_registry().read().unwrap();
        reg.get_all()
            .iter()
            .any(|c| c.get_localized_name() == "Orchestrator: New Workspace")
    })
    .unwrap();

    // The state a previous session left behind: a local workspace that was
    // still being created (its worktree not yet made) when the editor quit,
    // under the orchestrator's pending-workspace key. The persisted `label`
    // ("resumed-alias") is the *resolved* name the row last showed — distinct
    // from the spec's capture-time `displayLabel` ("stale-default", derived
    // from the then-empty name field) and from the project dir basename
    // ("proj_dir"). Recovery must restore the row under the persisted `label`,
    // not re-derive the stale default — so a row showing "resumed-alias" proves
    // the persisted label was honoured.
    let target = project.join("proj_dir");
    let target_str = target.to_string_lossy().to_string();
    let pending = json!([{
        "spec": {
            "backend": "local",
            "projectPath": target_str,
            "name": "",
            "cmd": "",
            "branch": "",
            "createWorktree": false,
            "displayLabel": "stale-default",
            "displayProject": target_str,
        },
        "label": "resumed-alias",
    }]);
    let migrated_attempt = legacy_attempt_id(0, &pending[0]);
    h.editor_mut()
        .handle_plugin_command(PluginCommand::SetGlobalState {
            plugin_name: "orchestrator".to_string(),
            key: "orchestrator.pending".to_string(),
            value: Some(pending.clone()),
        })
        .unwrap();

    // A separate prior attempt crossed the host create boundary but crashed
    // before the plugin learned the returned stable/window ids. The create
    // request atomically persisted this marker with the workspace; recovery
    // must treat it as the completed effect rather than replaying the create.
    let completed_attempt = "completed-attempt";
    let completed_target = project.join("already-created");
    std::fs::create_dir_all(&completed_target).unwrap();
    let completed_target = completed_target.canonicalize().unwrap();
    let completed_target_str = completed_target.to_string_lossy().to_string();
    let completed_spec = json!({
        "backend": "local",
        "projectPath": completed_target_str.clone(),
        "name": "already-created",
        "cmd": "",
        "auto": false,
        "startPrompt": "",
        "teachFreshCli": false,
        "branch": "",
        "newBranch": "",
        "createWorktree": false,
        "displayLabel": "already-created",
        "displayProject": completed_target_str.clone(),
    });
    h.editor_mut()
        .handle_plugin_command(PluginCommand::SetGlobalState {
            plugin_name: "orchestrator".to_string(),
            key: format!("orchestrator.pending:{completed_attempt}"),
            value: Some(json!({
                "version": 2,
                "attemptId": completed_attempt,
                "label": "already-created",
                "spec": completed_spec.clone(),
                "visit": false,
                "phase": "creating",
                "message": "Creating workspace…",
                "updatedAt": 1,
            })),
        })
        .unwrap();
    h.editor_mut()
        .handle_plugin_command(PluginCommand::SetGlobalState {
            plugin_name: "orchestrator".to_string(),
            key: format!("orchestrator.create_journal:{completed_attempt}"),
            value: Some(json!({
                "version": 1,
                "attemptId": completed_attempt,
                "label": "already-created",
                "spec": completed_spec,
                "autoNamed": false,
                "phase": "starting",
                "repoRoot": completed_target_str.clone(),
                "root": completed_target_str.clone(),
            })),
        })
        .unwrap();

    let mut completed_workspace = fresh::workspace::Workspace::new(completed_target.clone());
    completed_workspace.label = Some("already-created".to_string());
    completed_workspace.stable_id = Some("ws-already-created".to_string());
    let orchestrator_state = completed_workspace
        .session_plugin_state
        .entry("orchestrator".to_string())
        .or_default();
    orchestrator_state.insert(
        "project_path".to_string(),
        json!(completed_target_str.clone()),
    );
    orchestrator_state.insert("shared_worktree".to_string(), json!(false));
    orchestrator_state.insert("create_attempt".to_string(), json!(completed_attempt));
    let workspace_dir = data_dir.join("workspaces");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let workspace_file = workspace_dir.join(format!(
        "{}.ws-already-created.json",
        fresh::workspace::encode_path_for_filename(&completed_target),
    ));
    std::fs::write(
        &workspace_file,
        serde_json::to_vec_pretty(&completed_workspace).unwrap(),
    )
    .unwrap();

    // Push the just-set global state into the shared snapshot the plugin thread
    // reads before firing `ready`. In production this ordering is guaranteed —
    // startup runs `update_plugin_state_snapshot` (with the disk-loaded state)
    // several times before `fire_ready_hook` (main.rs), so the `ready` handler
    // always sees the persisted pending specs. Skipping it here left the plugin
    // reading a stale, empty snapshot whenever the fire-and-forget `ready`
    // request beat the test's first `wait_until`/render to the shared lock —
    // `recoverPendingWorkspaces` then found nothing and rendered no dock,
    // hanging the wait (a flaky timeout under load, not a product bug).
    h.editor_mut().update_plugin_state_snapshot();

    // The `ready` lifecycle hook replays persisted pending specs (this is the
    // "editor just launched" signal).
    h.editor_mut().fire_ready_hook();

    // The interrupted workspace comes back — paused and resumable — in the
    // dock, labelled with the resolved name it last showed ("resumed-alias"),
    // NOT the stale capture-time default. Without honouring the persisted
    // `label`, the row would render "stale-default" and this wait would hang.
    h.wait_until(|h| {
        let s = h.screen_to_string();
        s.contains("resumed-alias") && s.contains("Interrupted")
    })
    .unwrap();

    let state = h
        .editor()
        .plugin_global_state()
        .get("orchestrator")
        .expect("orchestrator state must contain the migrated pending attempt");
    assert!(
        !state.contains_key("orchestrator.pending"),
        "legacy source must clear only after its per-attempt write is acknowledged"
    );
    let migrated = state
        .get(&format!("orchestrator.pending:{migrated_attempt}"))
        .expect("legacy migration must use its deterministic attempt id");
    assert_eq!(migrated["attemptId"], migrated_attempt);
    assert_eq!(migrated["phase"], "paused");
    assert_eq!(migrated["updatedAt"], 0);

    // The atomically marked effect is already a real durable workspace, so
    // its pending/journal records disappear and no interrupted duplicate row
    // is surfaced.
    h.wait_until(|h| {
        h.editor()
            .plugin_global_state()
            .get("orchestrator")
            .is_some_and(|state| {
                !state.contains_key("orchestrator.pending:completed-attempt")
                    && !state.contains_key("orchestrator.create_journal:completed-attempt")
            })
    })
    .unwrap();
    assert!(
        workspace_file.exists(),
        "recovery must preserve the created workspace"
    );
    assert!(
        !h.screen_to_string().contains("already-created"),
        "a completed marked attempt must not surface as an interrupted duplicate. Screen:\n{}",
        h.screen_to_string(),
    );
    // And the stale default name is not what surfaced.
    assert!(
        !h.screen_to_string().contains("stale-default"),
        "restored row must show the persisted resolved name, not the stale \
         capture-time default. Screen:\n{}",
        h.screen_to_string(),
    );
    // The card keeps its one-key way out of the interrupted state. The
    // two-row card has no spare line for the old sentence-long hint, so
    // the affordance rides at the right of the name row — losing it
    // would leave a stuck row with no visible way to resume it.
    assert!(
        h.screen_to_string().contains("↵ Retry"),
        "an interrupted workspace's card must still offer its retry key. Screen:\n{}",
        h.screen_to_string(),
    );

    // The modal preview must reflect the placeholder's phase rather than
    // offering lifecycle actions that require a live window. Paused recovery
    // rows expose Retry + Dismiss, and nothing destructive/live-only.
    h.send_key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        .unwrap();
    h.wait_for_prompt().unwrap();
    h.type_text("Orchestrator: Open").unwrap();
    h.wait_until(|h| h.screen_to_string().contains("Orchestrator: Open"))
        .unwrap();
    h.send_key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
    h.wait_until(|h| h.screen_to_string().contains("ORCHESTRATOR :: Workspaces"))
        .unwrap();
    for _ in 0..12 {
        let screen = h.screen_to_string();
        if screen.contains("Retry") && screen.contains("Dismiss") {
            break;
        }
        h.send_key(KeyCode::Down, KeyModifiers::NONE).unwrap();
        h.tick_and_render().ok();
    }
    h.wait_until(|h| {
        let screen = h.screen_to_string();
        screen.contains("Retry") && screen.contains("Dismiss")
    })
    .unwrap_or_else(|_| {
        panic!(
            "paused pending preview must expose Retry and Dismiss. Screen:\n{}",
            h.screen_to_string(),
        )
    });
    let preview = h.screen_to_string();
    for invalid in ["Stop", "Archive", "Delete"] {
        assert!(
            !preview.contains(invalid),
            "paused pending preview must not expose `{invalid}` without a live window. Screen:\n{preview}",
        );
    }
}
