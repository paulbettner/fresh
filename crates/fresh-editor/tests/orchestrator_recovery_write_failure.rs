//! Recovery must not clear a workspace's `create_attempt` marker until its
//! journal/pending retirement is durably acknowledged.
//!
//! Single test in this binary: XDG data is process-global.
#![cfg(all(target_os = "linux", feature = "plugins"))]

mod common;

use common::dormant_ssh::isolated_dir_context;
use common::fail_retirement_write_fs::FailRetirementWriteFs;
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use fresh_core::api::PluginCommand;
use serde_json::json;
use std::fs;
use std::sync::Arc;

#[test]
fn recovery_write_failure_keeps_effect_marker_and_pending_record() {
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let data_dir = dir_context.data_dir.clone();
    let project = base.path().join("project");
    let created_root = base.path().join("created-root");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&created_root).unwrap();
    let project = project.canonicalize().unwrap();
    let created_root = created_root.canonicalize().unwrap();

    let plugins = project.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");

    let root = created_root.to_string_lossy().to_string();
    let state_dir = data_dir.join("orchestrator/state");
    let fault_fs = Arc::new(FailRetirementWriteFs::new(state_dir.clone(), root.clone()));
    let mut harness = EditorTestHarness::create(
        140,
        40,
        HarnessOptions::new()
            .with_working_dir(project)
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
                .any(|command| command.get_localized_name() == "Orchestrator: New Workspace")
        })
        .unwrap();

    let attempt_id = format!(
        "write-failure-{}",
        base.path().file_name().unwrap().to_string_lossy()
    );
    let pending_key = format!("orchestrator.pending:{attempt_id}");
    harness
        .editor_mut()
        .handle_plugin_command(PluginCommand::SetGlobalState {
            plugin_name: "orchestrator".to_string(),
            key: pending_key.clone(),
            value: Some(json!({
                "version": 2,
                "attemptId": attempt_id.clone(),
                "label": "write-failure",
                "spec": {
                    "backend": "local",
                    "projectPath": root,
                    "name": "write-failure",
                    "cmd": "",
                    "auto": false,
                    "startPrompt": "",
                    "teachFreshCli": false,
                    "branch": "",
                    "newBranch": "",
                    "createWorktree": false,
                    "displayLabel": "write-failure",
                    "displayProject": root,
                },
                "visit": false,
                "phase": "creating",
                "message": "Creating workspace…",
                "updatedAt": 1,
            })),
        })
        .unwrap();

    let mut workspace = fresh::workspace::Workspace::new(created_root.clone());
    workspace.label = Some("write-failure".to_string());
    workspace.stable_id = Some("ws-write-failure".to_string());
    workspace
        .session_plugin_state
        .entry("orchestrator".to_string())
        .or_default()
        .insert("create_attempt".to_string(), json!(attempt_id.clone()));
    let workspace_dir = data_dir.join("workspaces");
    fs::create_dir_all(&workspace_dir).unwrap();
    let workspace_file = workspace_dir.join(format!(
        "{}.ws-write-failure.json",
        fresh::workspace::encode_path_for_filename(&created_root)
    ));
    fs::write(
        &workspace_file,
        serde_json::to_vec_pretty(&workspace).unwrap(),
    )
    .unwrap();

    let state_file = state_dir.join("orchestrator.json");
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    assert!(
        persisted.get(pending_key.as_str()).is_some(),
        "pending fixture was not durable before fault injection"
    );

    harness.editor_mut().update_plugin_state_snapshot();
    fault_fs.arm();
    harness.editor_mut().fire_ready_hook();

    let lease_dir = std::env::temp_dir()
        .join("fresh-orchestrator-locks")
        .join(format!("k-v-create-attempt%3A{attempt_id}.lock"));
    harness
        .wait_until(|_| match fs::read_dir(&lease_dir) {
            Ok(entries) => entries.filter_map(Result::ok).any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("heartbeat-") && name.ends_with("-0")
            }),
            Err(_) => false,
        })
        .unwrap();

    let workspace: serde_json::Value =
        serde_json::from_slice(&fs::read(&workspace_file).unwrap()).unwrap();
    assert_eq!(
        workspace["session_plugin_state"]["orchestrator"]["create_attempt"],
        attempt_id.as_str(),
        "recovery cleared the effect marker before durable retirement"
    );
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    assert!(
        persisted.get(pending_key.as_str()).is_some(),
        "failed retirement must leave the durable pending record for retry"
    );
}
