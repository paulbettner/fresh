//! E2E: uncertain workspace inventory blocks every recovery rollback action.
//!
//! A matching but unreadable workspace record means the host cannot prove that
//! a create attempt has no durable workspace effect. Retry and Dismiss must
//! therefore preserve the journal's owned filesystem effect instead of treating
//! the failed scan as "not found" and rolling it back.
#![cfg(all(target_os = "linux", feature = "plugins"))]

mod common;

use common::dormant_ssh::isolated_dir_context;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use fresh_core::api::PluginCommand;
use serde_json::json;

fn pending_message(h: &EditorTestHarness, attempt_id: &str) -> Option<String> {
    h.editor()
        .plugin_global_state()
        .get("orchestrator")?
        .get(&format!("orchestrator.pending:{attempt_id}"))?
        .get("message")?
        .as_str()
        .map(str::to_owned)
}

fn click_pending_action(h: &mut EditorTestHarness, label: &str, action: &str) {
    let (_, row) = h
        .find_text_on_screen(label)
        .unwrap_or_else(|| panic!("pending row {label:?} missing:\n{}", h.screen_to_string()));
    h.mouse_right_click(4, row).unwrap();
    h.wait_until(|h| h.screen_to_string().contains(action))
        .unwrap();
    let (column, row) = h.find_text_on_screen(action).unwrap();
    h.mouse_click(column, row).unwrap();
}

#[test]
fn unreadable_inventory_refuses_retry_and_dismiss_rollback() {
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let data_dir = dir_context.data_dir.clone();
    let project = base.path().join("project");
    let target = project.join("created-workspace");
    std::fs::create_dir_all(&target).unwrap();
    let project = project.canonicalize().unwrap();
    let target = target.canonicalize().unwrap();

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
        h.editor()
            .command_registry()
            .read()
            .unwrap()
            .get_all()
            .iter()
            .any(|command| command.get_localized_name() == "Orchestrator: New Workspace")
    })
    .unwrap();

    let attempt_id = "uncertain-create-attempt";
    let label = "uncertain-create";
    let target_string = target.to_string_lossy().to_string();
    let protected_prompt = target.join("protected-prompt.txt");
    std::fs::write(&protected_prompt, "attempt-owned prompt").unwrap();
    let spec = json!({
        "backend": "local",
        "projectPath": target_string.clone(),
        "name": label,
        "cmd": "missing-agent",
        "auto": false,
        "startPrompt": "",
        "teachFreshCli": false,
        "branch": "",
        "newBranch": "",
        "createWorktree": false,
        "displayLabel": label,
        "displayProject": target_string.clone(),
    });
    h.editor_mut()
        .handle_plugin_command(PluginCommand::SetGlobalState {
            plugin_name: "orchestrator".to_string(),
            key: format!("orchestrator.pending:{attempt_id}"),
            value: Some(json!({
                "version": 2,
                "attemptId": attempt_id,
                "label": label,
                "spec": spec.clone(),
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
            key: format!("orchestrator.create_journal:{attempt_id}"),
            value: Some(json!({
                "version": 1,
                "attemptId": attempt_id,
                "label": label,
                "spec": spec,
                "autoNamed": false,
                "phase": "mutating",
                "repoRoot": target_string.clone(),
                "root": target_string,
                "prompt": {
                    "path": protected_prompt.to_string_lossy(),
                    "existed": false,
                    "original": "",
                    "written": "attempt-owned prompt",
                    "applied": true,
                },
            })),
        })
        .unwrap();

    let workspaces_dir = data_dir.join("workspaces");
    std::fs::create_dir_all(&workspaces_dir).unwrap();
    let corrupt_workspace = workspaces_dir.join(format!(
        "{}.corrupt.json",
        fresh::workspace::encode_path_for_filename(&target),
    ));
    std::fs::write(&corrupt_workspace, b"{ not valid workspace json").unwrap();

    h.editor_mut().update_plugin_state_snapshot();
    h.editor_mut().fire_ready_hook();
    h.wait_until(|h| {
        h.screen_to_string().contains(label)
            && h.screen_to_string()
                .contains("Workspace recovery is blocked")
    })
    .unwrap();

    click_pending_action(&mut h, label, "Retry");
    h.wait_until(|h| {
        pending_message(h, attempt_id)
            .is_some_and(|message| message.contains("cleanup is incomplete; retry was refused"))
    })
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&protected_prompt).unwrap(),
        "attempt-owned prompt",
        "Retry must not mutate journal effects while workspace inventory is uncertain",
    );

    click_pending_action(&mut h, label, "Dismiss");
    h.wait_until(|h| {
        pending_message(h, attempt_id)
            .is_some_and(|message| message.contains("cleanup is incomplete; dismissal was refused"))
    })
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&protected_prompt).unwrap(),
        "attempt-owned prompt",
        "Dismiss must not mutate journal effects while workspace inventory is uncertain",
    );
    assert!(
        corrupt_workspace.exists(),
        "uncertain inventory must remain inspectable"
    );
    assert!(
        h.editor()
            .plugin_global_state()
            .get("orchestrator")
            .is_some_and(
                |state| state.contains_key(&format!("orchestrator.create_journal:{attempt_id}"))
            ),
        "the recovery journal must remain durable after refused rollback",
    );
}
