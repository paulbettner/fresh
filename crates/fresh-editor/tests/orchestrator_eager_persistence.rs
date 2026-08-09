//! Regression tests for eager Orchestrator-session persistence.
//!
//! Sessions used to be written to the directory-keyed workspace registry
//! (`workspaces/*.json`) only when the editor exited *cleanly* (the quit-time
//! `save_all_windows_workspaces`). A killed or crashed editor therefore forgot
//! every session opened since the last clean quit — the dock came back missing
//! the workspaces the user actually had open.
//!
//! The fix checkpoints a window's workspace at natural points that don't depend
//! on a clean shutdown: switching away from a window, and finalizing a new
//! session's identity (`setWindowState`, which the Orchestrator calls right
//! after creating a window). These tests pin that behavior by asserting the
//! on-disk workspace exists *without any quit having happened*.

use fresh::config::Config;
use fresh::config_io::DirectoryContext;
use fresh::model::filesystem::StdFileSystem;
use fresh::workspace::Workspace;
use std::path::Path;
use std::sync::Arc;

fn editor_in(project: &Path, dir_context: &DirectoryContext) -> fresh::app::Editor {
    let filesystem: Arc<dyn fresh::model::filesystem::FileSystem + Send + Sync> =
        Arc::new(StdFileSystem);
    let config = Config {
        check_for_updates: false,
        ..Config::default()
    };
    fresh::app::Editor::for_test(
        config,
        80,
        24,
        Some(project.to_path_buf()),
        dir_context.clone(),
        fresh::view::color_support::ColorCapability::TrueColor,
        filesystem,
        None,
        None,
        false,
        false,
    )
    .unwrap()
}

fn wait_for_workspace(dir_context: &DirectoryContext, root: &Path) -> Workspace {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(workspace) = Workspace::load_in(dir_context, root).unwrap() {
            return workspace;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "workspace checkpoint was not published for {}",
            root.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Switching away from a window writes its workspace immediately — a later
/// hard kill (no clean quit) still finds it in the registry.
#[test]
fn switching_away_persists_the_outgoing_window_without_a_quit() {
    let sandbox = tempfile::tempdir().unwrap();
    let proj_a = sandbox.path().join("a");
    let proj_b = sandbox.path().join("b");
    let data_home = sandbox.path().join("data-home");
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();
    std::fs::create_dir_all(&data_home).unwrap();
    // The test-scoped context starts empty, so the precondition below is meaningful.
    let proj_a = proj_a.canonicalize().unwrap();
    let proj_b = proj_b.canonicalize().unwrap();
    let file_a = proj_a.join("hello.txt");
    std::fs::write(&file_a, "hi").unwrap();

    let dir_context = DirectoryContext::for_testing(&data_home);
    let mut e = editor_in(&proj_a, &dir_context);
    e.open_file(&file_a).unwrap();

    // No clean quit has happened and we never switched away, so A's session is
    // not yet in the on-disk registry.
    assert!(
        Workspace::load_in(&dir_context, &proj_a).unwrap().is_none(),
        "precondition: window A's workspace must not be on disk before any checkpoint"
    );

    // Open a second window and switch to it. The switch is the checkpoint: it
    // must persist the *outgoing* window (A) before leaving it.
    let win_b = e.create_window_at(proj_b.clone(), "b".into());
    e.set_active_window(win_b);

    let saved = wait_for_workspace(&dir_context, &proj_a);
    assert_eq!(
        saved.working_dir, proj_a,
        "the persisted workspace is window A's, keyed on its own root"
    );
    assert!(
        saved_contains_file(&saved, &file_a),
        "the checkpoint captured A's open file (hello.txt)"
    );
}

/// The file A had open must be recorded somewhere in the saved workspace. It
/// lives under the project root, so it is captured in the split layout rather
/// than `external_files`; a JSON scan for its name is stable regardless of the
/// exact serialized split-node shape.
fn saved_contains_file(ws: &Workspace, file: &Path) -> bool {
    let json = serde_json::to_string(ws).unwrap_or_default();
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !name.is_empty() && json.contains(name)
}

/// Setting editor-global plugin state (what the Orchestrator does when the
/// user organises dock sessions into folders) must be flushed to
/// `<data>/orchestrator/state/<plugin>.json` immediately — before any quit.
/// It used to be written only by the clean-quit `save_orchestrator_state`
/// call, so a killed or crashed editor forgot every folder and
/// session→folder assignment made since the last clean exit (issue #2703).
#[cfg(feature = "plugins")]
#[test]
fn setting_global_state_persists_it_without_a_quit() {
    use fresh_core::api::PluginCommand;

    let sandbox = tempfile::tempdir().unwrap();
    let proj = sandbox.path().join("proj");
    let data_home = sandbox.path().join("data-home");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&data_home).unwrap();
    let proj = proj.canonicalize().unwrap();

    let dir_context = DirectoryContext::for_testing(&data_home);
    let mut e = editor_in(&proj, &dir_context);

    let model = serde_json::json!({
        "version": 1,
        "folders": [{ "id": "df1", "name": "myfolder", "parent": null }],
        "assignments": {},
        "expanded": ["folder:df1"],
        "names": {},
        "folderCounter": 1,
    });
    e.handle_plugin_command(PluginCommand::SetGlobalState {
        plugin_name: "orchestrator".into(),
        key: "orchestrator.dock.model".into(),
        value: Some(model.clone()),
    })
    .unwrap();

    // No quit has happened — the state file must already be on disk.
    let state_path = dir_context
        .data_dir
        .join("orchestrator")
        .join("state")
        .join("orchestrator.json");
    let bytes = std::fs::read(&state_path).unwrap_or_else(|e| {
        panic!(
            "setting global state must persist {} without a quit: {e}",
            state_path.display()
        )
    });
    let map: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        map["orchestrator.dock.model"], model,
        "the persisted state carries the atomic dock model the plugin just set"
    );

    // Deleting the key persists too (an empty map on disk, not the stale
    // envelope) — clearing the model must also survive a crash.
    e.handle_plugin_command(PluginCommand::SetGlobalState {
        plugin_name: "orchestrator".into(),
        key: "orchestrator.dock.model".into(),
        value: None,
    })
    .unwrap();
    let map: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert!(
        map.get("orchestrator.dock.model").is_none(),
        "deleting the key must be flushed as well, got: {map}"
    );
}

/// Two editor processes sharing the data dir must not clobber each other's
/// global plugin state. Each instance loads `orchestrator/state/*.json` once
/// at boot, and the per-change flush used to rewrite the whole file from that
/// instance's in-memory map. An instance that booted before another editor
/// committed the atomic dock envelope must preserve it when writing an
/// unrelated plugin key or saving at quit.
///
/// The fix merges on write: an instance only rewrites the keys *it* changed,
/// on top of whatever is on disk, instead of snapshotting its whole map.
#[cfg(feature = "plugins")]
#[test]
fn concurrent_instances_do_not_clobber_each_others_global_state() {
    use fresh_core::api::PluginCommand;

    let sandbox = tempfile::tempdir().unwrap();
    let proj_a = sandbox.path().join("a");
    let proj_b = sandbox.path().join("b");
    let data_home = sandbox.path().join("data-home");
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();
    std::fs::create_dir_all(&data_home).unwrap();
    let proj_a = proj_a.canonicalize().unwrap();
    let proj_b = proj_b.canonicalize().unwrap();

    let dir_context = DirectoryContext::for_testing(&data_home);
    // Instance A boots first — its in-memory copy of the global state is
    // whatever was on disk now (nothing).
    let mut a = editor_in(&proj_a, &dir_context);
    // Instance B commits the complete dock model in one key.
    let mut assignments = serde_json::Map::new();
    assignments.insert(
        proj_b.to_string_lossy().into_owned(),
        serde_json::json!("df1"),
    );
    let model = serde_json::json!({
        "version": 1,
        "folders": [{ "id": "df1", "name": "Keep", "parent": null }],
        "assignments": assignments,
        "expanded": ["folder:df1"],
        "names": {},
        "folderCounter": 1,
    });
    let mut b = editor_in(&proj_b, &dir_context);
    b.handle_plugin_command(PluginCommand::SetGlobalState {
        plugin_name: "orchestrator".into(),
        key: "orchestrator.dock.model".into(),
        value: Some(model.clone()),
    })
    .unwrap();

    // Instance A — which booted before B's commit — writes an unrelated key.
    let history = serde_json::json!(["recent command"]);
    a.handle_plugin_command(PluginCommand::SetGlobalState {
        plugin_name: "orchestrator".into(),
        key: "orchestrator.history".into(),
        value: Some(history.clone()),
    })
    .unwrap();

    let state_path = dir_context
        .data_dir
        .join("orchestrator")
        .join("state")
        .join("orchestrator.json");
    let read_state = || -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap()
    };
    let map = read_state();
    assert_eq!(
        map["orchestrator.dock.model"], model,
        "A's unrelated write must not clobber B's dock model; got: {map}"
    );
    assert_eq!(
        map["orchestrator.history"], history,
        "A's own change must land alongside B's model; got: {map}"
    );

    // A's clean quit must not resurrect its stale snapshot either — the
    // quit-time save flushes only what A actually changed.
    a.save_orchestrator_state();
    let map = read_state();
    assert_eq!(
        map["orchestrator.dock.model"], model,
        "A's quit-time save must not clobber B's dock model; got: {map}"
    );

    // And a deletion in A removes exactly its key, not B's envelope.
    a.handle_plugin_command(PluginCommand::SetGlobalState {
        plugin_name: "orchestrator".into(),
        key: "orchestrator.history".into(),
        value: None,
    })
    .unwrap();
    let map = read_state();
    assert!(
        map.get("orchestrator.history").is_none(),
        "A's deletion must be flushed; got: {map}"
    );
    assert_eq!(
        map["orchestrator.dock.model"], model,
        "A's deletion must leave B's dock model intact; got: {map}"
    );
}

/// Setting per-session plugin state (what the Orchestrator does right after
/// creating a session, to tag its `project_path`) checkpoints the window, so a
/// freshly created session is in the registry the moment it is tagged — before
/// any switch or quit.
///
/// `SetWindowState` is a plugin command (`dispatch_plugin_command_envelope`
/// only exists with the `plugins` feature), so this test is gated to that
/// build — the min-size / no-plugins configuration has no tagging path.
#[cfg(feature = "plugins")]
#[test]
fn tagging_a_new_session_persists_it_without_a_quit() {
    use fresh_core::api::{
        PluginCommand, PluginCommandContext, PluginCommandEnvelope, PluginInstanceId,
    };

    let sandbox = tempfile::tempdir().unwrap();
    let proj_a = sandbox.path().join("a");
    let proj_b = sandbox.path().join("b");
    let data_home = sandbox.path().join("data-home");
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();
    std::fs::create_dir_all(&data_home).unwrap();
    let proj_a = proj_a.canonicalize().unwrap();
    let proj_b = proj_b.canonicalize().unwrap();
    let file_a = proj_a.join("seed.txt");
    std::fs::write(&file_a, "x").unwrap();

    let dir_context = DirectoryContext::for_testing(&data_home);
    let mut e = editor_in(&proj_a, &dir_context);

    // Create a second session for project B and make it active — this mirrors
    // what `createWindowWithTerminal` does (it dives into the new window). Give
    // it real content so it is savable.
    let win_b = e.create_window_at(proj_b.clone(), "b".into());
    e.set_active_window(win_b);
    let file_b = proj_b.join("seed.txt");
    std::fs::write(&file_b, "y").unwrap();
    e.open_file(&file_b).unwrap();

    // Not yet tagged, and if the harness didn't checkpoint on switch we can't
    // rely on B being on disk — so drive the exact tagging call the plugin
    // makes and require *that* to persist B.
    let before = Workspace::load_in(&dir_context, &proj_b).unwrap();

    e.dispatch_plugin_command_envelope(PluginCommandEnvelope::new(
        PluginCommand::SetWindowState {
            window_id: win_b,
            key: "project_path".into(),
            value: Some(serde_json::Value::String(
                proj_b.to_string_lossy().into_owned(),
            )),
        },
        PluginCommandContext {
            plugin_name: Arc::from("orchestrator"),
            plugin_instance_id: PluginInstanceId::fresh(),
            source_window: Some(win_b),
            ..PluginCommandContext::default()
        },
    ));

    let after = wait_for_workspace(&dir_context, &proj_b);
    assert_eq!(after.working_dir, proj_b);
    assert_eq!(
        after.session_plugin_state["orchestrator"]["project_path"],
        serde_json::Value::String(proj_b.to_string_lossy().into_owned()),
        "the persisted session carries the project_path the plugin just set"
    );
    // Whether or not `before` existed (a switch-away checkpoint may have
    // written it already), the tagging call must leave a complete, identity-
    // carrying record behind.
    let _ = before;
}

/// Deleting an in-place session must forget its exact persisted workspace, or
/// boot-time discovery rediscovers it and the row comes back after restart.
/// Closing remains separate from the durable forget operation.
#[cfg(feature = "plugins")]
#[test]
fn deleting_an_in_place_session_forgets_its_persisted_workspace() {
    use fresh_core::api::PluginCommand;

    let sandbox = tempfile::tempdir().unwrap();
    let proj_a = sandbox.path().join("a");
    let proj_b = sandbox.path().join("b");
    let data_home = sandbox.path().join("data-home");
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();
    std::fs::create_dir_all(&data_home).unwrap();
    // The test-scoped context starts with no persisted workspace.
    let proj_a = proj_a.canonicalize().unwrap();
    let proj_b = proj_b.canonicalize().unwrap();
    let file_a = proj_a.join("hello.txt");
    std::fs::write(&file_a, "hi").unwrap();

    let dir_context = DirectoryContext::for_testing(&data_home);
    let mut e = editor_in(&proj_a, &dir_context);
    let win_a = e.active_window_id();
    let stable_id = e.session(win_a).unwrap().stable_id.clone();
    e.open_file(&file_a).unwrap();

    // A second window, then switch to it: the switch checkpoints A into the
    // registry (and leaves A non-active, non-last so it can be closed).
    let win_b = e.create_window_at(proj_b.clone(), "b".into());
    e.set_active_window(win_b);
    let _ = wait_for_workspace(&dir_context, &proj_a);

    // Closing the window alone does NOT forget the persisted workspace —
    // exactly why a deleted in-place row used to reappear after a restart.
    e.handle_plugin_command(PluginCommand::CloseWindow { id: win_a })
        .unwrap();
    assert!(
        Workspace::load_in(&dir_context, &proj_a).unwrap().is_some(),
        "CloseWindow alone leaves the registry entry that discovery resurrects"
    );

    // The explicit exact-id forget is what makes the deletion stick across a restart.
    Workspace::delete_by_id_in(&dir_context, &proj_a, &stable_id).unwrap();
    assert!(
        Workspace::load_in(&dir_context, &proj_a).unwrap().is_none(),
        "a deleted in-place session must be forgotten so a restart can't rediscover it"
    );
}
