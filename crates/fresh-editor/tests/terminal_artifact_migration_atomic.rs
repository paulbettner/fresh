//! Atomic migration regression for legacy terminal transcript paths.
#![cfg(target_os = "linux")]

use fresh::config::Config;
use fresh::config_io::DirectoryContext;
use fresh::model::filesystem::StdFileSystem;
use fresh::workspace::{
    terminal_artifacts_dir, ExitedTerminalState, SerializedSplitNode, SerializedTerminalWorkspace,
    Workspace,
};
use std::path::Path;
use std::sync::Arc;

fn isolated_dir_context(base: &Path) -> DirectoryContext {
    let xdg_data = base.join("xdg-data");
    std::fs::create_dir_all(&xdg_data).unwrap();
    std::env::set_var("XDG_DATA_HOME", &xdg_data);
    DirectoryContext {
        data_dir: xdg_data.join("fresh"),
        config_dir: base.join("config"),
        home_dir: Some(base.join("home")),
        documents_dir: None,
        downloads_dir: None,
    }
}

fn editor_result_in(
    project: &Path,
    dir_context: &DirectoryContext,
) -> anyhow::Result<fresh::app::Editor> {
    fresh::app::Editor::for_test(
        Config {
            check_for_updates: false,
            ..Config::default()
        },
        80,
        24,
        Some(project.to_path_buf()),
        dir_context.clone(),
        fresh::view::color_support::ColorCapability::TrueColor,
        Arc::new(StdFileSystem),
        None,
        None,
        false,
        false,
    )
}

fn editor_in(project: &Path, dir_context: &DirectoryContext) -> fresh::app::Editor {
    editor_result_in(project, dir_context).unwrap()
}

#[test]
fn partial_legacy_copy_is_never_accepted_and_restore_retries() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(sandbox.path());
    let project = sandbox.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();

    let stable_id = "ws-terminal-migration";
    let legacy_root = dir_context.terminal_dir_for(&project);
    let stable_root = terminal_artifacts_dir(&dir_context, &project, stable_id);
    std::fs::create_dir_all(&legacy_root).unwrap();
    std::fs::create_dir_all(&stable_root).unwrap();

    let legacy_backing = legacy_root.join("fresh-terminal-0.txt");
    let final_backing = stable_root.join("fresh-terminal-0.txt");
    let final_history = stable_root.join("fresh-terminal-0.history.txt");
    let final_log = stable_root.join("fresh-terminal-0.log");
    std::fs::write(&final_log, b"").unwrap();

    // Model a crashed prior copy: only a caller-owned partial temp exists. The
    // final path must remain absent and therefore retryable.
    let abandoned_partial = stable_root.join(".fresh-terminal-0.txt.crashed.tmp");
    std::fs::write(&abandoned_partial, b"truncated").unwrap();

    // A directory opens as a file descriptor on Unix but fails when copied,
    // deterministically exercising failure after the new temp is created.
    std::fs::create_dir(&legacy_backing).unwrap();
    let mut workspace = Workspace::new(project.clone());
    workspace.stable_id = Some(stable_id.to_string());
    workspace.split_layout = SerializedSplitNode::Terminal {
        terminal_index: 0,
        split_id: 0,
        label: None,
        role: None,
    };
    workspace.terminals.push(SerializedTerminalWorkspace {
        terminal_index: 0,
        cwd: Some(project.clone()),
        shell: "sh".into(),
        cols: 80,
        rows: 24,
        log_path: final_log,
        backing_path: legacy_backing.clone(),
        history_path: None,
        backing_history_end: None,
        checkpoint_generation: None,
        command: None,
        agent_resume: None,
        exited: Some(ExitedTerminalState { exit_code: Some(0) }),
        title: Some("legacy transcript".into()),
        script_access: false,
        companion: None,
    });
    workspace.save().unwrap();

    {
        let mut editor = editor_in(&project, &dir_context);
        editor.restore_active_window_on_launch(false).unwrap();
        assert!(
            !final_backing.exists(),
            "a failed copy must not publish a truncated final transcript"
        );
        assert!(
            !final_history.exists(),
            "a failed legacy copy must not publish derived history"
        );
        assert!(
            editor
                .terminal_backing_files()
                .values()
                .any(|path| path == &legacy_backing),
            "the failed migration must keep the legacy source authoritative"
        );
        let temps: Vec<_> = std::fs::read_dir(&stable_root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert_eq!(
            temps,
            vec![abandoned_partial.clone()],
            "the failed attempt must clean only its own partial temp"
        );
    }

    std::fs::remove_dir(&legacy_backing).unwrap();
    let complete = b"complete legacy transcript\n";
    std::fs::write(&legacy_backing, complete).unwrap();

    let mut editor = editor_in(&project, &dir_context);
    editor.restore_active_window_on_launch(false).unwrap();
    assert_eq!(std::fs::read(&final_backing).unwrap(), complete);
    assert_eq!(std::fs::read(&legacy_backing).unwrap(), complete);
    assert_eq!(std::fs::read(&final_history).unwrap(), complete);
    assert!(
        editor
            .active_window()
            .terminal_history_files
            .values()
            .any(|path| path == &final_history),
        "legacy restore must adopt the atomically initialized history"
    );
    assert!(
        editor
            .terminal_backing_files()
            .values()
            .any(|path| path == &final_backing),
        "the next restore must adopt only the completely published final"
    );
    assert_eq!(std::fs::read(&abandoned_partial).unwrap(), b"truncated");
}

#[test]
fn unresolved_extraction_recovery_prevents_stale_workspace_restore() {
    let sandbox = tempfile::tempdir().unwrap();
    let dir_context = isolated_dir_context(sandbox.path());
    let source_root = sandbox.path().join("source");
    let target_root = sandbox.path().join("target");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::create_dir_all(&target_root).unwrap();
    let source_id = "ws-source";
    let target_id = "ws-target";
    let source =
        terminal_artifacts_dir(&dir_context, &source_root, source_id).join("terminal.history.txt");
    let destination =
        terminal_artifacts_dir(&dir_context, &target_root, target_id).join("terminal.history.txt");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&source, b"source-copy").unwrap();
    std::fs::write(&destination, b"moved-copy").unwrap();

    let mut source_before = Workspace::new(source_root.clone());
    source_before.stable_id = Some(source_id.to_string());
    let journal_dir = dir_context.data_dir.join("terminal-extractions");
    let journal = journal_dir.join("ambiguous.json");
    std::fs::create_dir_all(&journal_dir).unwrap();
    let intent = serde_json::json!({
        "id": "ambiguous",
        "source_before": source_before,
        "target_root": target_root,
        "target_stable_id": target_id,
        "artifacts": [{
            "source": source.clone(),
            "destination": destination.clone(),
            "after_source_cutover": false
        }],
        "phase": { "phase": "prepared" }
    });
    std::fs::write(&journal, serde_json::to_vec_pretty(&intent).unwrap()).unwrap();

    let error = match editor_result_in(&source_root, &dir_context) {
        Ok(_) => panic!("editor construction must fail while extraction recovery is ambiguous"),
        Err(error) => error,
    };

    assert!(error
        .to_string()
        .contains("terminal extraction recovery remains unresolved"));
    assert!(journal.exists());
    assert_eq!(std::fs::read(&source).unwrap(), b"source-copy");
    assert_eq!(std::fs::read(&destination).unwrap(), b"moved-copy");
}
