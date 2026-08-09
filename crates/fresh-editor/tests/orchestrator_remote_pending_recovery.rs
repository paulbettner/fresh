//! A crash after remote-window persistence must retire the matching pending
//! attempt without starting a second remote connection.
//!
//! Single test in this binary: XDG data and PATH are process-global.
#![cfg(all(target_os = "linux", feature = "plugins"))]

mod common;

use common::dormant_ssh::{canonical_mkdir, isolated_dir_context, persist_previous_session};
use common::harness::{copy_plugin, copy_plugin_lib, EditorTestHarness, HarnessOptions};
use common::PathGuard;
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn mark_remote_workspace(data_dir: &Path, attempt_id: &str) -> PathBuf {
    let workspace_dir = data_dir.join("workspaces");
    for entry in fs::read_dir(&workspace_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let mut workspace: fresh::workspace::Workspace =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if workspace.label.as_deref() != Some("ssh-dead") {
            continue;
        }
        if workspace.stable_id.is_none() {
            workspace.stable_id = Some("ws-remote-create-completed".to_string());
        }
        workspace
            .session_plugin_state
            .entry("orchestrator".to_string())
            .or_default()
            .insert("create_attempt".to_string(), json!(attempt_id));
        fs::write(&path, serde_json::to_vec_pretty(&workspace).unwrap()).unwrap();
        return path;
    }
    panic!("persisted ssh-dead workspace missing");
}

fn install_observable_ssh(base: &Path) -> (tempfile::TempDir, PathGuard, PathBuf) {
    let shim_dir = tempfile::tempdir().unwrap();
    let called = base.join("ssh-called");
    let shim = shim_dir.path().join("ssh");
    fs::write(
        &shim,
        format!("#!/bin/sh\n: > \"{}\"\nexit 255\n", called.display()),
    )
    .unwrap();
    let mut permissions = fs::metadata(&shim).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&shim, permissions).unwrap();
    let guard = PathGuard::prepend(shim_dir.path());
    (shim_dir, guard, called)
}

#[test]
fn marked_remote_create_is_retired_without_reconnect() {
    fresh::i18n::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(base.path());
    let project = canonical_mkdir(base.path(), "project");
    let remote_root = canonical_mkdir(base.path(), "remote-root");

    persist_previous_session(&dir_context, &project, &remote_root, false);

    let attempt_id = format!(
        "remote-create-{}",
        base.path().file_name().unwrap().to_string_lossy()
    );
    let pending_key = format!("orchestrator.pending:{attempt_id}");
    let workspace_file = mark_remote_workspace(&dir_context.data_dir, &attempt_id);
    let state_file = dir_context
        .data_dir
        .join("orchestrator/state/orchestrator.json");
    fs::create_dir_all(state_file.parent().unwrap()).unwrap();
    let mut state = serde_json::Map::new();
    state.insert(
        pending_key.clone(),
        json!({
            "version": 2,
            "attemptId": attempt_id.clone(),
            "label": "ssh-dead",
            "spec": {
                "backend": "ssh",
                "spec": {
                    "transport": {
                        "kind": "ssh",
                        "user": "root",
                        "host": "dead-host",
                        "port": 2222,
                        "identity_file": null,
                        "remote_path": remote_root.to_string_lossy(),
                    },
                    "base_env": [],
                    "window": true,
                    "label": "ssh-dead",
                },
                "facet": {
                    "kind": "ssh",
                    "detail": "root@dead-host:2222",
                    "state": "starting",
                },
                "displayLabel": "ssh-dead",
                "displayProject": "root@dead-host:2222",
                "persistCmd": "",
            },
            "visit": false,
            "phase": "creating",
            "message": "Connecting…",
            "updatedAt": 1,
        }),
    );
    fs::write(
        &state_file,
        serde_json::to_vec_pretty(&serde_json::Value::Object(state)).unwrap(),
    )
    .unwrap();

    let plugins = project.join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    copy_plugin_lib(&plugins);
    copy_plugin(&plugins, "orchestrator");
    let (_shim_dir, _path_guard, ssh_called) = install_observable_ssh(base.path());

    let mut harness = EditorTestHarness::create(
        140,
        40,
        HarnessOptions::new()
            .with_working_dir(project)
            .with_shared_dir_context(dir_context),
    )
    .unwrap();
    harness.tick_and_render().unwrap();
    harness
        .wait_until(|h| {
            h.editor()
                .plugin_global_state()
                .get("orchestrator")
                .is_some_and(|state| !state.contains_key(&pending_key))
        })
        .unwrap();
    harness.wait_for_async_quiescence(5).unwrap();

    assert!(
        workspace_file.exists(),
        "recovery deleted the remote workspace"
    );
    assert!(
        !ssh_called.exists(),
        "recovery replayed a remote connection after finding its marked effect"
    );
    assert!(
        !harness.screen_to_string().contains("Interrupted"),
        "a completed remote effect surfaced as a duplicate pending row"
    );
}
