// Focused tests for the OMP companion boundary and delivery state.
use super::*;
use std::cell::{Cell, RefCell};

const TEST_SYNC_B64: &[u8] = b"AAAAAAAAAAAAAAAAAAAAAA";
const TEST_TAG_B64: &[u8] = b"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

#[cfg(unix)]
static OMP_PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(unix)]
struct OmpPathGuard {
    previous: Option<std::ffi::OsString>,
    previous_trusted_omp: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl OmpPathGuard {
    fn prepend(dir: &std::path::Path) -> Self {
        let lock = OMP_PATH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("PATH");
        let previous_trusted_omp = std::env::var_os("FRESH_OMP_EXECUTABLE");
        let mut paths = vec![dir.to_path_buf()];
        if let Some(value) = &previous {
            paths.extend(std::env::split_paths(value));
        }
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("FRESH_OMP_EXECUTABLE", dir.join("omp"));
        Self {
            previous,
            previous_trusted_omp,
            _lock: lock,
        }
    }
}

#[cfg(unix)]
impl Drop for OmpPathGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        match &self.previous_trusted_omp {
            Some(value) => std::env::set_var("FRESH_OMP_EXECUTABLE", value),
            None => std::env::remove_var("FRESH_OMP_EXECUTABLE"),
        }
    }
}

fn valid_omp_companion_envelope() -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "type": "snapshot",
        "snapshot": {
            "version": 1,
            "incarnation": "550e8400-e29b-41d4-a716-446655440000",
            "sequence": 1,
            "sessionGeneration": 1,
            "workEpoch": 1,
            "timestampMs": 0,
            "ompVersion": "0.52.1",
            "processId": 1234,
            "sessionId": "123e4567-e89b-12d3-a456-426614174000",
            "sessionName": "Companion session",
            "cwd": "/tmp/project",
            "state": "working",
            "statusText": "Finding top-level files",
            "model": { "provider": "openai", "id": "gpt-5.6" },
            "thinkingLevel": "high",
            "runningTools": 1,
            "currentTool": { "name": "read", "intent": "Inspect files" },
            "goal": { "objective": "Ship companion", "status": "active" },
            "todos": {
                "pending": 1,
                "inProgress": 1,
                "blocked": 0,
                "completed": 2,
                "abandoned": 0,
                "current": "Add tests"
            },
            "context": { "tokens": 123, "contextWindow": 1000, "percentBps": 1230 },
            "pendingApprovals": 1,
            "asyncJobs": { "running": 1, "recentFailures": 0, "pendingDelivery": 0 }
        }
    })
}

fn omp_candidate_from_encoded_body(body_b64: &[u8]) -> Vec<u8> {
    let mut candidate = Vec::with_capacity(
        OMP_OUTPUT_PREFIX.len() + TEST_SYNC_B64.len() + body_b64.len() + TEST_TAG_B64.len() + 4,
    );
    candidate.extend_from_slice(OMP_OUTPUT_PREFIX);
    candidate.extend_from_slice(TEST_SYNC_B64);
    candidate.push(b';');
    candidate.extend_from_slice(body_b64);
    candidate.push(b'.');
    candidate.extend_from_slice(TEST_TAG_B64);
    candidate.extend_from_slice(OMP_OUTPUT_TERMINATOR);
    candidate
}

fn omp_candidate_from_bytes(body: &[u8]) -> Vec<u8> {
    let body_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body);
    omp_candidate_from_encoded_body(body_b64.as_bytes())
}

fn omp_candidate(value: &serde_json::Value) -> Vec<u8> {
    omp_candidate_from_bytes(&serde_json::to_vec(value).unwrap())
}

fn parse_test_candidate(
    candidate: &[u8],
    accept_sequence: impl FnOnce(&str, u64, u64, &str, u64) -> bool,
) -> Option<fresh_core::hooks::OmpCompanionSnapshotV1> {
    parse_omp_companion_candidate_with(
        candidate,
        |sync_b64, _, tag_b64| sync_b64 == TEST_SYNC_B64 && tag_b64 == TEST_TAG_B64,
        accept_sequence,
    )
}

fn hook_payload(terminal: fresh_core::WindowTerminalId, sequence: u64) -> OmpCompanionHookPayload {
    let mut envelope = valid_omp_companion_envelope();
    envelope["snapshot"]["sequence"] = serde_json::json!(sequence);
    OmpCompanionHookPayload {
        terminal,
        received_at_ms: sequence,
        snapshot: serde_json::from_value(envelope["snapshot"].clone()).unwrap(),
    }
}

#[test]
fn omp_companion_parser_authenticates_before_json_and_accepts_valid_snapshot() {
    let candidate = omp_candidate(&valid_omp_companion_envelope());
    let snapshot = parse_test_candidate(
        &candidate,
        |incarnation, sequence, generation, session_id, work_epoch| {
            assert_eq!(incarnation, "550e8400-e29b-41d4-a716-446655440000");
            assert_eq!(sequence, 1);
            assert_eq!(generation, 1);
            assert_eq!(session_id, "123e4567-e89b-12d3-a456-426614174000");
            assert_eq!(work_epoch, 1);
            true
        },
    )
    .expect("valid authenticated snapshot");
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(snapshot.session_generation, 1);

    let malformed = omp_candidate_from_bytes(b"not json");
    let auth_checked = Cell::new(false);
    let sequence_checked = Cell::new(false);
    assert!(parse_omp_companion_candidate_with(
        &malformed,
        |sync_b64, body_b64, tag_b64| {
            auth_checked.set(true);
            assert_eq!(sync_b64, TEST_SYNC_B64);
            assert!(!body_b64.is_empty());
            assert_eq!(tag_b64, TEST_TAG_B64);
            false
        },
        |_, _, _, _, _| {
            sequence_checked.set(true);
            true
        },
    )
    .is_none());
    assert!(auth_checked.get());
    assert!(!sequence_checked.get());
}

#[test]
fn omp_companion_parser_rejects_strict_schema_domain_and_string_violations() {
    let mut cases = Vec::new();

    let mut unknown_envelope = valid_omp_companion_envelope();
    unknown_envelope["unexpected"] = serde_json::json!(true);
    cases.push(unknown_envelope);

    let mut unknown_nested = valid_omp_companion_envelope();
    unknown_nested["snapshot"]["model"]["unexpected"] = serde_json::json!(true);
    cases.push(unknown_nested);

    for field in [
        "sessionName",
        "statusText",
        "model",
        "thinkingLevel",
        "currentTool",
        "goal",
        "todos",
        "context",
        "asyncJobs",
    ] {
        let mut explicit_null = valid_omp_companion_envelope();
        explicit_null["snapshot"][field] = serde_json::Value::Null;
        cases.push(explicit_null);
    }

    let mut null_tool_intent = valid_omp_companion_envelope();
    null_tool_intent["snapshot"]["currentTool"]["intent"] = serde_json::Value::Null;
    cases.push(null_tool_intent);

    let mut null_current_todo = valid_omp_companion_envelope();
    null_current_todo["snapshot"]["todos"]["current"] = serde_json::Value::Null;
    cases.push(null_current_todo);

    let mut fractional_integer = valid_omp_companion_envelope();
    fractional_integer["snapshot"]["sequence"] = serde_json::json!(1.5);
    cases.push(fractional_integer);

    let mut zero_sequence = valid_omp_companion_envelope();
    zero_sequence["snapshot"]["sequence"] = serde_json::json!(0);
    cases.push(zero_sequence);

    let mut unsafe_timestamp = valid_omp_companion_envelope();
    unsafe_timestamp["snapshot"]["timestampMs"] = serde_json::json!(JS_SAFE_INTEGER_MAX + 1);
    cases.push(unsafe_timestamp);

    let mut invalid_process = valid_omp_companion_envelope();
    invalid_process["snapshot"]["processId"] = serde_json::json!(0);
    cases.push(invalid_process);

    let mut invalid_percent = valid_omp_companion_envelope();
    invalid_percent["snapshot"]["context"]["percentBps"] = serde_json::json!(10_001);
    cases.push(invalid_percent);

    let mut invalid_count = valid_omp_companion_envelope();
    invalid_count["snapshot"]["runningTools"] = serde_json::json!(OMP_COMPANION_COUNT_MAX + 1);
    cases.push(invalid_count);

    let mut uppercase_uuid = valid_omp_companion_envelope();
    uppercase_uuid["snapshot"]["incarnation"] =
        serde_json::json!("550E8400-E29B-41D4-A716-446655440000");
    cases.push(uppercase_uuid);

    let mut non_v4_incarnation = valid_omp_companion_envelope();
    non_v4_incarnation["snapshot"]["incarnation"] =
        serde_json::json!("123e4567-e89b-12d3-a456-426614174000");
    cases.push(non_v4_incarnation);

    let mut empty_required = valid_omp_companion_envelope();
    empty_required["snapshot"]["ompVersion"] = serde_json::json!("");
    cases.push(empty_required);

    let mut empty_optional = valid_omp_companion_envelope();
    empty_optional["snapshot"]["sessionName"] = serde_json::json!("");
    cases.push(empty_optional);

    let mut non_normalized = valid_omp_companion_envelope();
    non_normalized["snapshot"]["cwd"] = serde_json::json!(" /tmp  project");
    cases.push(non_normalized);

    let mut forbidden_scalar = valid_omp_companion_envelope();
    forbidden_scalar["snapshot"]["currentTool"]["intent"] = serde_json::json!("Inspect\nfiles");
    cases.push(forbidden_scalar);

    let mut invalid_status_text = valid_omp_companion_envelope();
    invalid_status_text["snapshot"]["statusText"] = serde_json::json!("Finding\nfiles");
    cases.push(invalid_status_text);

    let mut oversized_status_text = valid_omp_companion_envelope();
    oversized_status_text["snapshot"]["statusText"] = serde_json::json!("😀".repeat(241));
    cases.push(oversized_status_text);

    let mut byte_oversize = valid_omp_companion_envelope();
    byte_oversize["snapshot"]["model"]["provider"] = serde_json::json!("🦀".repeat(65));
    cases.push(byte_oversize);

    for invalid in cases {
        let sequence_checked = Cell::new(false);
        assert!(
            parse_test_candidate(&omp_candidate(&invalid), |_, _, _, _, _| {
                sequence_checked.set(true);
                true
            })
            .is_none()
        );
        assert!(
            !sequence_checked.get(),
            "sequence state must not advance for an invalid snapshot: {invalid}"
        );
    }

    let body = serde_json::to_string(&valid_omp_companion_envelope()).unwrap();
    let duplicate = body.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
    assert!(parse_test_candidate(
        &omp_candidate_from_bytes(duplicate.as_bytes()),
        |_, _, _, _, _| true
    )
    .is_none());
}

#[test]
fn omp_companion_parser_enforces_canonical_encoding_and_wire_caps() {
    let body = serde_json::to_vec(&valid_omp_companion_envelope()).unwrap();
    let canonical = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&body);

    let padded = format!("{canonical}=");
    assert!(parse_test_candidate(
        &omp_candidate_from_encoded_body(padded.as_bytes()),
        |_, _, _, _, _| true,
    )
    .is_none());

    let mut body_with_padding_space = body;
    while body_with_padding_space.len() % 3 == 0 {
        body_with_padding_space.push(b' ');
    }
    let mut noncanonical = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(&body_with_padding_space)
        .into_bytes();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let last = noncanonical.len() - 1;
    let value = alphabet
        .iter()
        .position(|byte| *byte == noncanonical[last])
        .unwrap();
    noncanonical[last] = alphabet[value ^ 1];
    assert!(parse_test_candidate(
        &omp_candidate_from_encoded_body(&noncanonical),
        |_, _, _, _, _| true,
    )
    .is_none());

    let auth_checked = Cell::new(false);
    let oversized_body = vec![b'A'; OMP_COMPANION_BODY_B64_MAX + 1];
    assert!(parse_omp_companion_candidate_with(
        &omp_candidate_from_encoded_body(&oversized_body),
        |_, _, _| {
            auth_checked.set(true);
            true
        },
        |_, _, _, _, _| true,
    )
    .is_none());
    assert!(!auth_checked.get());

    let frame_auth_checked = Cell::new(false);
    let oversized_frame_body = vec![b'A'; OMP_OUTPUT_FRAME_MAX];
    assert!(parse_omp_companion_candidate_with(
        &omp_candidate_from_encoded_body(&oversized_frame_body),
        |_, _, _| {
            frame_auth_checked.set(true);
            true
        },
        |_, _, _, _, _| true,
    )
    .is_none());
    assert!(!frame_auth_checked.get());

    let mut wrong_terminator = omp_candidate(&valid_omp_companion_envelope());
    wrong_terminator.truncate(wrong_terminator.len() - OMP_OUTPUT_TERMINATOR.len());
    wrong_terminator.push(0x07);
    assert!(parse_test_candidate(&wrong_terminator, |_, _, _, _, _| true).is_none());
}

#[test]
fn omp_companion_parser_passes_generation_to_the_admission_fence() {
    let accepted = RefCell::new(None::<(String, u64, u64, String, u64)>);
    let accept =
        |incarnation: &str, sequence: u64, generation: u64, session_id: &str, work_epoch: u64| {
            let mut last = accepted.borrow_mut();
            if last.as_ref().is_some_and(
                |(last_incarnation, last_sequence, last_generation, _, _)| {
                    last_incarnation == incarnation
                        && (sequence <= *last_sequence || generation < *last_generation)
                },
            ) {
                return false;
            }
            *last = Some((
                incarnation.to_string(),
                sequence,
                generation,
                session_id.to_string(),
                work_epoch,
            ));
            true
        };

    let first = valid_omp_companion_envelope();
    assert!(parse_test_candidate(&omp_candidate(&first), accept).is_some());
    assert!(parse_test_candidate(&omp_candidate(&first), accept).is_none());

    let mut higher = first.clone();
    higher["snapshot"]["sequence"] = serde_json::json!(2);
    higher["snapshot"]["sessionGeneration"] = serde_json::json!(3);
    assert!(parse_test_candidate(&omp_candidate(&higher), accept).is_some());

    let mut regressed_generation = higher.clone();
    regressed_generation["snapshot"]["sequence"] = serde_json::json!(3);
    regressed_generation["snapshot"]["sessionGeneration"] = serde_json::json!(2);
    assert!(parse_test_candidate(&omp_candidate(&regressed_generation), accept).is_none());

    let mut new_incarnation = first;
    new_incarnation["snapshot"]["incarnation"] =
        serde_json::json!("6ba7b810-9dad-41d1-80b4-00c04fd430c8");
    assert!(parse_test_candidate(&omp_candidate(&new_incarnation), accept).is_some());
}

#[test]
#[cfg(unix)]
fn marked_omp_restore_replaces_a_stale_executable_with_current_trusted_locator() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let old_dir = temp.path().join("old");
    let current_dir = temp.path().join("current");
    std::fs::create_dir(&old_dir).unwrap();
    std::fs::create_dir(&current_dir).unwrap();
    let old = old_dir.join("omp");
    let current = current_dir.join("omp");
    let script = "#!/bin/sh\nif [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\nexec sleep 30\n";
    std::fs::write(&old, script).unwrap();
    std::fs::write(&current, script).unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o755)).unwrap();
    let stale = old.canonicalize().unwrap();
    let current = current.canonicalize().unwrap();
    std::fs::remove_file(&stale).unwrap();
    let _path_guard = OmpPathGuard::prepend(&current_dir);
    let mut argv = vec![
        stale.to_string_lossy().into_owned(),
        "launch".to_string(),
        "--".to_string(),
        "prompt".to_string(),
    ];

    assert!(crate::app::terminal::pin_current_trusted_omp_argv(
        &mut argv, false,
    ));
    assert_eq!(argv[0], current.to_string_lossy());
}

#[test]
fn companion_snapshot_workspace_gate_rejects_a_canonical_peer_root() {
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use crate::model::filesystem::StdFileSystem;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let peer = temp.path().join("peer");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&peer).unwrap();
    let project = project.canonicalize().unwrap();
    let peer = peer.canonicalize().unwrap();
    let filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync> =
        Arc::new(StdFileSystem);
    let editor = Editor::for_test(
        Config {
            check_for_updates: false,
            ..Config::default()
        },
        80,
        24,
        Some(project.clone()),
        DirectoryContext::for_testing(temp.path()),
        crate::view::color_support::ColorCapability::TrueColor,
        filesystem,
        None,
        None,
        false,
        false,
    )
    .unwrap();
    let mut snapshot: fresh_core::hooks::OmpCompanionSnapshotV1 =
        serde_json::from_value(valid_omp_companion_envelope()["snapshot"].clone()).unwrap();
    snapshot.cwd = project.to_string_lossy().into_owned();
    assert!(snapshot_matches_owning_workspace(
        editor.active_window(),
        &snapshot,
    ));

    snapshot.cwd = peer.to_string_lossy().into_owned();
    assert!(!snapshot_matches_owning_workspace(
        editor.active_window(),
        &snapshot,
    ));
}

#[test]
#[cfg(unix)]
fn unsupported_omp_launches_ordinary_without_private_flag() {
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use crate::model::filesystem::StdFileSystem;
    use fresh_core::api::{PluginCommand, TerminalCompanion};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let omp = temp.path().join("omp");
    let argv_log = temp.path().join("ordinary-argv");
    std::fs::write(
        &omp,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 64; fi\nprintf '%s\\n' \"$@\" > '{}'\nexec sleep 30\n",
            argv_log.to_string_lossy(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executable = omp.canonicalize().unwrap().to_string_lossy().into_owned();
    let _path_guard = OmpPathGuard::prepend(temp.path());
    let filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync> =
        Arc::new(StdFileSystem);
    let mut editor = Editor::for_test(
        Config {
            check_for_updates: false,
            ..Config::default()
        },
        80,
        24,
        Some(project),
        DirectoryContext::for_testing(temp.path()),
        crate::view::color_support::ColorCapability::TrueColor,
        filesystem,
        None,
        None,
        false,
        false,
    )
    .unwrap();
    let window_id = editor.active_window_id();
    editor
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: Some(false),
            persistent: false,
            window_id,
            command: Some(vec![
                executable,
                "--fresh-omp-companion".into(),
                "launch".into(),
                "--".into(),
                "prompt".into(),
            ]),
            relaunch: None,
            title: Some("unsupported omp".into()),
            resume: None,
            env: None,
            companion: Some(TerminalCompanion::Omp),
            allow_script: false,
            selected_agent: false,
            request_id: 77,
        })
        .unwrap();
    let terminal_id = editor
        .active_window()
        .terminal_buffers
        .values()
        .next()
        .expect("ordinary OMP terminal must spawn")
        .terminal_id;
    assert!(editor
        .active_window()
        .terminal_manager
        .get(terminal_id)
        .and_then(|handle| handle.companion_kind())
        .is_none());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !argv_log.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "ordinary OMP did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let args = std::fs::read_to_string(argv_log).unwrap();
    assert!(!args.lines().any(|arg| arg == "--fresh-omp-companion"));
    editor
        .active_window_mut()
        .terminal_manager
        .close(terminal_id);
}

#[test]
#[cfg(unix)]
fn completed_snapshot_is_checkpointed_after_child_is_no_longer_alive() {
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use crate::model::filesystem::StdFileSystem;
    use fresh_core::api::{PluginCommand, TerminalCompanion};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let omp = temp.path().join("omp");
    std::fs::write(
        &omp,
        "#!/bin/sh\nif [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\nsleep 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executable = omp.canonicalize().unwrap().to_string_lossy().into_owned();
    let _path_guard = OmpPathGuard::prepend(temp.path());
    let dir_context = DirectoryContext::for_testing(temp.path());
    let filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync> =
        Arc::new(StdFileSystem);
    let mut editor = Editor::for_test(
        Config {
            check_for_updates: false,
            ..Config::default()
        },
        80,
        24,
        Some(project.clone()),
        dir_context.clone(),
        crate::view::color_support::ColorCapability::TrueColor,
        filesystem,
        None,
        None,
        false,
        false,
    )
    .unwrap();
    let stable_id = crate::workspace::generate_stable_id();
    editor.active_window_mut().stable_id = stable_id.clone();
    let window_id = editor.active_window_id();
    editor
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: None,
            persistent: true,
            window_id,
            command: Some(vec![executable.clone(), "launch".into()]),
            relaunch: None,
            title: Some("short omp".into()),
            resume: None,
            env: None,
            companion: Some(TerminalCompanion::Omp),
            allow_script: false,
            selected_agent: false,
            request_id: 78,
        })
        .unwrap();
    let terminal_id = editor
        .active_window()
        .terminal_buffers
        .values()
        .next()
        .expect("OMP terminal must spawn")
        .terminal_id;
    let live = editor
        .active_window()
        .terminal_manager
        .get(terminal_id)
        .and_then(|handle| handle.companion.clone())
        .expect("supported OMP must have a live companion");
    let mut snapshot: fresh_core::hooks::OmpCompanionSnapshotV1 =
        serde_json::from_value(valid_omp_companion_envelope()["snapshot"].clone()).unwrap();
    snapshot.cwd = project.to_string_lossy().into_owned();
    assert!(live.test_install_snapshot(&snapshot));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while editor
        .active_window()
        .terminal_manager
        .get(terminal_id)
        .is_some_and(|handle| handle.is_alive())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "OMP child did not exit"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    editor.handle_omp_companion_snapshot_ready(fresh_core::WindowTerminalId::new(
        window_id,
        terminal_id,
    ));
    let expected_resume = vec![executable, "--resume".to_string(), snapshot.session_id];
    assert_eq!(
        editor
            .active_window()
            .terminal_resume_commands
            .get(&terminal_id),
        Some(&expected_resume),
    );
    let persisted = crate::workspace::Workspace::load_by_id_in(&dir_context, &project, &stable_id)
        .unwrap()
        .expect("snapshot checkpoint must publish workspace metadata")
        .terminals
        .into_iter()
        .find(|terminal| {
            terminal
                .agent_resume
                .as_ref()
                .is_some_and(|resume| resume.argv.as_slice() == expected_resume.as_slice())
        })
        .expect("snapshot checkpoint must durably persist the exact resume argv");
    assert_eq!(
        persisted.agent_resume.as_ref().map(|resume| &resume.argv),
        Some(&expected_resume),
    );
}
#[test]
fn omp_companion_delivery_is_fifo_fair_and_latest_only() {
    let a = fresh_core::WindowTerminalId::new(fresh_core::WindowId(1), fresh_core::TerminalId(7));
    let b = fresh_core::WindowTerminalId::new(fresh_core::WindowId(2), fresh_core::TerminalId(7));
    let mut delivery = OmpCompanionHookDelivery::default();

    assert!(delivery.push(hook_payload(a, 1)));
    assert!(delivery.push(hook_payload(b, 1)));
    assert_eq!(delivery.take_next().unwrap().terminal, a);

    assert!(delivery.push(hook_payload(a, 2)));
    assert!(delivery.push(hook_payload(a, 3)));
    assert!(delivery.push(hook_payload(b, 2)));
    delivery.complete_in_flight();

    let b_latest = delivery.take_next().unwrap();
    assert_eq!(b_latest.terminal, b);
    assert_eq!(b_latest.snapshot.sequence, 2);
    delivery.complete_in_flight();

    let a_latest = delivery.take_next().unwrap();
    assert_eq!(a_latest.terminal, a);
    assert_eq!(a_latest.snapshot.sequence, 3);
    delivery.complete_in_flight();
    assert!(delivery.take_next().is_none());
}

#[test]
fn omp_companion_completion_requeues_latest_delivery() {
    let terminal =
        fresh_core::WindowTerminalId::new(fresh_core::WindowId(1), fresh_core::TerminalId(7));
    let mut delivery = OmpCompanionHookDelivery::default();

    assert!(delivery.push(hook_payload(terminal, 1)));
    assert_eq!(delivery.take_next().unwrap().terminal, terminal);
    assert!(delivery.push(hook_payload(terminal, 2)));
    delivery.complete_in_flight();

    let latest = delivery.take_next().unwrap();
    assert_eq!(latest.terminal, terminal);
    assert_eq!(latest.snapshot.sequence, 2);
    delivery.complete_in_flight();
    assert!(delivery.take_next().is_none());
}

#[test]
fn omp_companion_delivery_keys_tombstones_by_full_identity() {
    let dead =
        fresh_core::WindowTerminalId::new(fresh_core::WindowId(1), fresh_core::TerminalId(0));
    let live =
        fresh_core::WindowTerminalId::new(fresh_core::WindowId(2), fresh_core::TerminalId(0));
    let mut delivery = OmpCompanionHookDelivery::default();

    assert!(delivery.push(hook_payload(dead, 1)));
    assert_eq!(delivery.take_next().unwrap().terminal, dead);
    assert!(delivery.push(hook_payload(live, 1)));
    delivery.purge(dead);

    assert!(delivery.is_tombstoned(dead));
    assert!(!delivery.is_tombstoned(live));
    assert!(!delivery.push(hook_payload(dead, 2)));
    delivery.complete_in_flight();
    assert_eq!(delivery.take_next().unwrap().terminal, live);
}

#[test]
fn ordinary_terminal_exits_do_not_accumulate_companion_tombstones() {
    let mut delivery = OmpCompanionHookDelivery::default();

    for terminal_id in 0..128 {
        delivery.purge(fresh_core::WindowTerminalId::new(
            fresh_core::WindowId(1),
            fresh_core::TerminalId(terminal_id),
        ));
    }
    assert_eq!(delivery.tombstone_count(), 0);

    let completed =
        fresh_core::WindowTerminalId::new(fresh_core::WindowId(2), fresh_core::TerminalId(0));
    assert!(delivery.push(hook_payload(completed, 1)));
    assert_eq!(delivery.take_next().unwrap().terminal, completed);
    delivery.complete_in_flight();
    delivery.purge(completed);
    assert_eq!(delivery.tombstone_count(), 0);
}

#[test]
fn omp_companion_exit_purge_blocks_in_flight_follow_up_without_misacking_peer() {
    let dead =
        fresh_core::WindowTerminalId::new(fresh_core::WindowId(1), fresh_core::TerminalId(0));
    let peer =
        fresh_core::WindowTerminalId::new(fresh_core::WindowId(2), fresh_core::TerminalId(0));
    let mut delivery = OmpCompanionHookDelivery::default();

    assert!(delivery.push(hook_payload(dead, 1)));
    assert_eq!(delivery.take_next().unwrap().terminal, dead);
    assert!(delivery.push(hook_payload(dead, 2)));
    assert!(delivery.push(hook_payload(peer, 1)));
    delivery.purge(dead);

    assert!(delivery.is_tombstoned(dead));
    assert!(!delivery.push(hook_payload(dead, 3)));
    assert!(delivery.take_next().is_none());
    delivery.complete_in_flight();
    assert!(!delivery.is_tombstoned(dead));
    assert_eq!(delivery.tombstone_count(), 0);
    assert_eq!(delivery.take_next().unwrap().terminal, peer);
    delivery.complete_in_flight();
    assert!(delivery.take_next().is_none());
}

#[test]
#[cfg(unix)]
fn provisional_profiled_restore_keeps_companion_and_checkpoints_exact_resume() {
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use crate::model::filesystem::StdFileSystem;
    use fresh_core::api::{PluginCommand, TerminalCompanion};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let session_dir = temp.path().join("sessions");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&session_dir).unwrap();
    let project = project.canonicalize().unwrap();
    let session_dir_arg = format!("--session-dir={}", session_dir.to_string_lossy());
    let omp = temp.path().join("omp");
    std::fs::write(
        &omp,
        "#!/bin/sh\nif [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let omp = omp.canonicalize().unwrap();
    let executable = omp.to_string_lossy().into_owned();
    let _path_guard = OmpPathGuard::prepend(temp.path());
    let dir_context = DirectoryContext::for_testing(temp.path());
    let make_editor = || {
        let filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync> =
            Arc::new(StdFileSystem);
        Editor::for_test(
            Config {
                check_for_updates: false,
                ..Config::default()
            },
            80,
            24,
            Some(project.clone()),
            dir_context.clone(),
            crate::view::color_support::ColorCapability::TrueColor,
            filesystem,
            None,
            None,
            false,
            false,
        )
        .unwrap()
    };
    let stable_id = crate::workspace::generate_stable_id();

    let provisional_resume = vec![
        executable.clone(),
        "--profile".to_string(),
        "work".to_string(),
        session_dir_arg.clone(),
        "--continue".to_string(),
    ];
    let expected_resume = vec![
        executable.clone(),
        "--profile".to_string(),
        "work".to_string(),
        session_dir_arg.clone(),
        "--resume".to_string(),
        "123e4567-e89b-12d3-a456-426614174000".to_string(),
    ];
    let (checkpoint_boundary, checkpoint_path, checkpoint_generation, history_path) = {
        let mut editor = make_editor();
        editor.active_window_mut().stable_id = stable_id.clone();
        let window_id = editor.active_window_id();
        editor
            .handle_plugin_command(PluginCommand::CreateTerminal {
                cwd: None,
                direction: None,
                ratio: None,
                focus: Some(true),
                persistent: false,
                window_id,
                command: Some(vec![
                    executable.clone(),
                    "launch".into(),
                    "--profile".into(),
                    "work".into(),
                    session_dir_arg.clone(),
                    "--".into(),
                    "prompt".into(),
                ]),
                relaunch: None,
                title: Some("omp".into()),
                resume: Some(provisional_resume.clone()),
                env: None,
                companion: Some(TerminalCompanion::Omp),
                allow_script: false,
                selected_agent: false,
                request_id: 1,
            })
            .unwrap();
        let buffer_id = editor.active_buffer_id();
        let terminal_id = editor
            .active_window()
            .get_terminal_id(buffer_id)
            .expect("OMP terminal must be live");
        assert!(
            editor
                .active_window()
                .terminal_resume_commands
                .get(&terminal_id)
                .is_none(),
            "marked OMP must ignore non-exact resume before authentication"
        );
        assert!(editor
            .active_window()
            .terminal_manager
            .get(terminal_id)
            .and_then(|handle| handle.companion_kind())
            .is_some());
        let history_path = editor.active_window().terminal_history_files[&terminal_id].clone();
        editor.save_workspace_for(window_id).unwrap();
        let terminal = crate::workspace::Workspace::load_by_id_in(
            &dir_context,
            &project,
            &editor.active_window().stable_id,
        )
        .unwrap()
        .unwrap()
        .terminals
        .into_iter()
        .find(|terminal| terminal.history_path.as_ref() == Some(&history_path))
        .expect("full save must persist the live terminal");
        let checkpoint_boundary = terminal
            .backing_history_end
            .expect("full save must record the checkpoint's history boundary");
        let checkpoint_generation = terminal
            .checkpoint_generation
            .expect("full save must record the checkpoint generation");
        (
            checkpoint_boundary,
            terminal.backing_path,
            checkpoint_generation,
            history_path,
        )
    };

    // A metadata checkpoint can be overtaken by later scrollback before an
    // editor crash. Restore must preserve that newer append-only history and
    // still resume the authenticated OMP session.
    let checkpoint_len = std::fs::metadata(&checkpoint_path).unwrap().len();
    let mut history = std::fs::OpenOptions::new()
        .append(true)
        .open(&history_path)
        .unwrap();
    history
        .write_all(&vec![b'\n'; checkpoint_len as usize + 1])
        .unwrap();
    history.sync_all().unwrap();
    drop(history);
    let advanced_history = std::fs::read(&history_path).unwrap();

    {
        let mut editor = make_editor();
        editor.active_window_mut().stable_id = stable_id.clone();
        assert!(editor.restore_active_window_on_launch(false).unwrap());
        assert_eq!(
            std::fs::read(&history_path).unwrap(),
            advanced_history,
            "restore must not overwrite newer append-only history with a stale screen checkpoint",
        );
        let (buffer_id, terminal_id) = {
            let (buffer_id, binding) = editor
                .active_window()
                .terminal_buffers
                .iter()
                .next()
                .expect("provisional OMP restore must stay live");
            (*buffer_id, binding.terminal_id)
        };
        assert_eq!(
            editor.active_window().terminal_companions.get(&terminal_id),
            Some(&TerminalCompanion::Omp)
        );
        assert!(
            editor
                .active_window()
                .terminal_resume_commands
                .get(&terminal_id)
                .is_none(),
            "bare --continue must not become durable resume authority before a snapshot"
        );
        let live = editor
            .active_window()
            .terminal_manager
            .get(terminal_id)
            .and_then(|handle| handle.companion.clone())
            .expect("provisional restore must reactivate the companion");
        let checkpoint_before = std::fs::read(&checkpoint_path).unwrap_or_default();
        let mut snapshot: fresh_core::hooks::OmpCompanionSnapshotV1 =
            serde_json::from_value(valid_omp_companion_envelope()["snapshot"].clone()).unwrap();
        snapshot.cwd = project.to_string_lossy().into_owned();
        assert!(live.test_install_snapshot(&snapshot));

        let window_id = editor.active_window_id();
        editor.handle_omp_companion_snapshot_ready(fresh_core::WindowTerminalId::new(
            window_id,
            terminal_id,
        ));
        assert_eq!(
            editor
                .active_window()
                .terminal_resume_commands
                .get(&terminal_id),
            Some(&expected_resume)
        );
        assert_eq!(
            std::fs::read(&checkpoint_path).unwrap_or_default(),
            checkpoint_before,
            "exact OMP resume checkpoint must not rewrite the visible-screen generation",
        );
        let persisted = crate::workspace::Workspace::load_by_id_in(
            &dir_context,
            &project,
            &editor.active_window().stable_id,
        )
        .unwrap()
        .unwrap()
        .terminals
        .into_iter()
        .find(|terminal| terminal.history_path.as_ref() == Some(&history_path))
        .expect("metadata-only save must preserve the terminal");
        assert_eq!(persisted.backing_history_end, Some(checkpoint_boundary));
        assert_eq!(persisted.backing_path, checkpoint_path);
        assert_eq!(
            persisted.checkpoint_generation.as_deref(),
            Some(checkpoint_generation.as_str()),
            "metadata-only resume saves must preserve the last full checkpoint generation",
        );

        let mut rejected_snapshot = snapshot.clone();
        rejected_snapshot.sequence += 1;
        rejected_snapshot.session_generation += 1;
        rejected_snapshot.session_id = "123e4567-e89b-12d3-a456-426614174001".to_string();
        assert!(live.test_install_snapshot(&rejected_snapshot));
        let workspaces_dir = dir_context.workspaces_dir();
        let displaced_workspaces_dir = temp.path().join("workspaces-before-failed-ack");
        std::fs::rename(&workspaces_dir, &displaced_workspaces_dir).unwrap();
        std::fs::write(&workspaces_dir, b"not a directory").unwrap();
        editor.handle_omp_companion_snapshot_ready(fresh_core::WindowTerminalId::new(
            window_id,
            terminal_id,
        ));
        std::fs::remove_file(&workspaces_dir).unwrap();
        std::fs::rename(&displaced_workspaces_dir, &workspaces_dir).unwrap();
        assert_eq!(
            editor
                .active_window()
                .terminal_resume_commands
                .get(&terminal_id),
            Some(&expected_resume),
            "failed durable checkpoint must restore the prior exact resume"
        );
        let persisted_after_failure = crate::workspace::Workspace::load_by_id_in(
            &dir_context,
            &project,
            &editor.active_window().stable_id,
        )
        .unwrap()
        .unwrap()
        .terminals
        .into_iter()
        .find(|terminal| terminal.history_path.as_ref() == Some(&history_path))
        .expect("failed metadata save must retain the previously published terminal");
        assert_eq!(
            persisted_after_failure
                .agent_resume
                .as_ref()
                .map(|resume| &resume.argv),
            Some(&expected_resume),
            "negative acknowledgement must leave durable resume metadata unchanged"
        );

        let mut retained_snapshot = snapshot.clone();
        retained_snapshot.sequence = rejected_snapshot.sequence + 1;
        retained_snapshot.session_generation = rejected_snapshot.session_generation;
        assert!(live.test_install_snapshot(&retained_snapshot));
        editor.handle_omp_companion_snapshot_ready(fresh_core::WindowTerminalId::new(
            window_id,
            terminal_id,
        ));
        assert_eq!(
            editor
                .active_window()
                .terminal_resume_commands
                .get(&terminal_id),
            Some(&expected_resume),
            "retained session must be accepted at the rejected transition generation"
        );
        let retained_target = fresh_core::api::OmpCompanionCommandTargetV1 {
            incarnation: retained_snapshot.incarnation.clone(),
            session_generation: retained_snapshot.session_generation,
            session_id: retained_snapshot.session_id.clone(),
            work_epoch: retained_snapshot.work_epoch,
        };
        let alive = std::sync::atomic::AtomicBool::new(true);
        assert!(live
            .frame_command_if_active(
                fresh_core::api::OmpCompanionCommandType::RequestSnapshot,
                &retained_target,
                &alive,
            )
            .is_some());

        assert!(live.test_install_snapshot(&rejected_snapshot));
        editor.handle_omp_companion_snapshot_ready(fresh_core::WindowTerminalId::new(
            window_id,
            terminal_id,
        ));
        assert!(live
            .frame_command_if_active(
                fresh_core::api::OmpCompanionCommandType::RequestSnapshot,
                &retained_target,
                &alive,
            )
            .is_some());

        editor
            .active_window_mut()
            .terminal_manager
            .close(terminal_id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while editor.active_window().exited_terminal(buffer_id).is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "OMP terminal exit did not settle"
            );
            editor.process_async_messages();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            editor
                .active_window()
                .exited_terminal(buffer_id)
                .and_then(|record| record.resume.as_ref()),
            Some(&expected_resume)
        );
        editor.save_workspace().unwrap();
    }

    let mut restored = make_editor();
    restored.active_window_mut().stable_id = stable_id.clone();
    assert!(restored.restore_active_window_on_launch(false).unwrap());
    assert_eq!(
        restored
            .active_window()
            .exited_terminals
            .values()
            .next()
            .and_then(|record| record.resume.as_ref()),
        Some(&expected_resume)
    );
}

#[test]
#[cfg(unix)]
fn unambiguous_markerless_exact_omp_restore_migrates_and_rotates() {
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use crate::model::filesystem::StdFileSystem;
    use fresh_core::api::{PluginCommand, TerminalCompanion};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let omp = temp.path().join("omp");
    std::fs::write(
        &omp,
        "#!/bin/sh\nif [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let omp = omp.canonicalize().unwrap();
    let executable = omp.to_string_lossy().into_owned();
    let _path_guard = OmpPathGuard::prepend(temp.path());
    let dir_context = DirectoryContext::for_testing(temp.path());
    let make_editor = || {
        let filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync> =
            Arc::new(StdFileSystem);
        Editor::for_test(
            Config {
                check_for_updates: false,
                ..Config::default()
            },
            80,
            24,
            Some(project.clone()),
            dir_context.clone(),
            crate::view::color_support::ColorCapability::TrueColor,
            filesystem,
            None,
            None,
            false,
            false,
        )
        .unwrap()
    };
    let stable_id = crate::workspace::generate_stable_id();

    let legacy_resume = vec![
        executable.clone(),
        "--profile=legacy".to_string(),
        "--resume".to_string(),
        "123e4567-e89b-42d3-a456-426614174099".to_string(),
    ];
    {
        let mut editor = make_editor();
        editor.active_window_mut().stable_id = stable_id.clone();
        let window_id = editor.active_window_id();
        editor
            .handle_plugin_command(PluginCommand::CreateTerminal {
                cwd: None,
                direction: None,
                ratio: None,
                focus: Some(true),
                persistent: false,
                window_id,
                command: Some(vec![
                    executable.clone(),
                    "launch".into(),
                    "--profile=legacy".into(),
                ]),
                relaunch: None,
                title: Some("legacy omp".into()),
                resume: Some(legacy_resume.clone()),
                env: None,
                companion: None,
                allow_script: false,
                selected_agent: false,
                request_id: 2,
            })
            .unwrap();
        assert!(editor.active_window().terminal_companions.is_empty());
        editor.save_workspace_for(window_id).unwrap();
    }

    let rotated_resume = vec![
        executable.clone(),
        "--profile=legacy".to_string(),
        "--resume".to_string(),
        "123e4567-e89b-12d3-a456-426614174000".to_string(),
    ];
    {
        let mut editor = make_editor();
        editor.active_window_mut().stable_id = stable_id.clone();
        assert!(editor.restore_active_window_on_launch(false).unwrap());
        let terminal_id = editor
            .active_window()
            .terminal_buffers
            .values()
            .next()
            .expect("legacy exact OMP must restore live")
            .terminal_id;
        assert_eq!(
            editor.active_window().terminal_companions.get(&terminal_id),
            Some(&TerminalCompanion::Omp)
        );
        assert_eq!(
            editor.active_window().tracked_agent_terminal,
            Some(terminal_id),
            "the only trusted legacy candidate becomes the authoritative agent terminal"
        );
        let live = editor
            .active_window()
            .terminal_manager
            .get(terminal_id)
            .and_then(|handle| handle.companion.clone())
            .expect("legacy promotion must mint a live companion");
        assert_eq!(
            editor
                .active_window()
                .terminal_resume_commands
                .get(&terminal_id),
            Some(&legacy_resume)
        );
        let mut snapshot: fresh_core::hooks::OmpCompanionSnapshotV1 =
            serde_json::from_value(valid_omp_companion_envelope()["snapshot"].clone()).unwrap();
        snapshot.cwd = project.to_string_lossy().into_owned();
        assert!(live.test_install_snapshot(&snapshot));
        let window_id = editor.active_window_id();
        editor.handle_omp_companion_snapshot_ready(fresh_core::WindowTerminalId::new(
            window_id,
            terminal_id,
        ));
        assert_eq!(
            editor
                .active_window()
                .terminal_resume_commands
                .get(&terminal_id),
            Some(&rotated_resume)
        );
        editor.save_workspace().unwrap();
    }

    let mut restored = make_editor();
    restored.active_window_mut().stable_id = stable_id.clone();
    assert!(restored.restore_active_window_on_launch(false).unwrap());
    let terminal_id = restored
        .active_window()
        .terminal_buffers
        .values()
        .next()
        .expect("migrated exact OMP must stay live on the next restore")
        .terminal_id;
    assert_eq!(
        restored
            .active_window()
            .terminal_companions
            .get(&terminal_id),
        Some(&TerminalCompanion::Omp)
    );
    assert_eq!(
        restored
            .active_window()
            .terminal_resume_commands
            .get(&terminal_id),
        Some(&rotated_resume)
    );
}

#[test]
#[cfg(unix)]
fn multiple_markerless_exact_omp_restores_remain_ordinary() {
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use crate::model::filesystem::StdFileSystem;
    use fresh_core::api::PluginCommand;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let omp = temp.path().join("omp");
    std::fs::write(&omp, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let omp = omp.canonicalize().unwrap();
    let executable = omp.to_string_lossy().into_owned();
    let _path_guard = OmpPathGuard::prepend(temp.path());
    let dir_context = DirectoryContext::for_testing(temp.path());
    let make_editor = || {
        let filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync> =
            Arc::new(StdFileSystem);
        Editor::for_test(
            Config {
                check_for_updates: false,
                ..Config::default()
            },
            80,
            24,
            Some(project.clone()),
            dir_context.clone(),
            crate::view::color_support::ColorCapability::TrueColor,
            filesystem,
            None,
            None,
            false,
            false,
        )
        .unwrap()
    };
    let stable_id = crate::workspace::generate_stable_id();

    {
        let mut editor = make_editor();
        editor.active_window_mut().stable_id = stable_id.clone();
        let window_id = editor.active_window_id();
        for (request_id, session_id) in [
            (10, "123e4567-e89b-42d3-a456-426614174010"),
            (11, "123e4567-e89b-42d3-a456-426614174011"),
        ] {
            editor
                .handle_plugin_command(PluginCommand::CreateTerminal {
                    cwd: None,
                    direction: None,
                    ratio: None,
                    focus: Some(false),
                    persistent: false,
                    window_id,
                    command: Some(vec![executable.clone(), "launch".into()]),
                    relaunch: None,
                    title: Some(format!("legacy omp {request_id}")),
                    resume: Some(vec![
                        executable.clone(),
                        "--resume".into(),
                        session_id.into(),
                    ]),
                    env: None,
                    companion: None,
                    allow_script: false,
                    selected_agent: false,
                    request_id,
                })
                .unwrap();
        }
        assert!(editor.active_window().terminal_companions.is_empty());
        editor.save_workspace_for(window_id).unwrap();
    }

    let mut restored = make_editor();
    restored.active_window_mut().stable_id = stable_id;
    assert!(restored.restore_active_window_on_launch(false).unwrap());
    assert_eq!(restored.active_window().terminal_buffers.len(), 2);
    assert!(restored.active_window().terminal_companions.is_empty());
    assert!(restored.active_window().tracked_agent_terminal.is_none());
    assert!(restored
        .active_window()
        .terminal_buffers
        .values()
        .all(|binding| restored
            .active_window()
            .terminal_manager
            .get(binding.terminal_id)
            .and_then(|handle| handle.companion_kind())
            .is_none()));
}

#[test]
#[cfg(unix)]
fn snapshot_bridge_routes_same_numeric_terminal_to_owning_window() {
    use crate::config::Config;
    use crate::config_io::DirectoryContext;
    use crate::model::filesystem::StdFileSystem;
    use crate::services::async_bridge::AsyncMessage;
    use fresh_core::api::{PluginCommand, TerminalCompanion};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let project_cwd = project.to_string_lossy().into_owned();
    let omp = temp.path().join("omp");
    std::fs::write(
        &omp,
        "#!/bin/sh\nif [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\nexec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let omp = omp.canonicalize().unwrap();
    let _path_guard = OmpPathGuard::prepend(temp.path());
    let executable = omp.to_string_lossy().into_owned();
    let launch = vec![
        executable.clone(),
        "launch".into(),
        "--".into(),
        "prompt".into(),
    ];
    let fallback_resume = vec![executable.clone(), "--continue".into()];
    let filesystem: Arc<dyn crate::model::filesystem::FileSystem + Send + Sync> =
        Arc::new(StdFileSystem);
    let mut editor = Editor::for_test(
        Config {
            check_for_updates: false,
            ..Config::default()
        },
        80,
        24,
        Some(project.clone()),
        DirectoryContext::for_testing(temp.path()),
        crate::view::color_support::ColorCapability::TrueColor,
        filesystem,
        None,
        None,
        false,
        false,
    )
    .unwrap();

    let target_window = editor.active_window_id();
    editor
        .handle_plugin_command(PluginCommand::CreateTerminal {
            cwd: None,
            direction: None,
            ratio: None,
            focus: Some(true),
            persistent: false,
            window_id: target_window,
            command: Some(launch.clone()),
            relaunch: None,
            title: Some("target omp".into()),
            resume: Some(fallback_resume.clone()),
            env: None,
            companion: Some(TerminalCompanion::Omp),
            allow_script: false,
            selected_agent: false,
            request_id: 1,
        })
        .unwrap();
    let target_terminal = editor
        .active_window()
        .get_terminal_id(editor.active_buffer_id())
        .unwrap();

    let authority = editor.local_session_authority(&project);
    let (peer_window, peer_terminal, _) = editor
        .create_window_with_terminal(
            project.clone(),
            "peer".into(),
            Some(project),
            Some(launch),
            None,
            Some("peer omp".into()),
            authority,
            Some(fallback_resume.clone()),
            None,
            false,
            Some(TerminalCompanion::Omp),
            false,
            true,
            None,
        )
        .unwrap();
    assert_eq!(
        target_terminal, peer_terminal,
        "per-window managers should reproduce the numeric-id collision"
    );
    let peer_resume_before = editor
        .session(peer_window)
        .unwrap()
        .terminal_resume_commands
        .get(&peer_terminal)
        .cloned();
    assert!(
        peer_resume_before.is_none(),
        "provisional --continue must not be durable before authentication",
    );

    let target_live = editor
        .session(target_window)
        .and_then(|window| window.terminal_manager.get(target_terminal))
        .and_then(|handle| handle.companion.clone())
        .unwrap();
    let peer_live = editor
        .session(peer_window)
        .and_then(|window| window.terminal_manager.get(peer_terminal))
        .and_then(|handle| handle.companion.clone())
        .unwrap();
    let mut target_json = valid_omp_companion_envelope()["snapshot"].clone();
    target_json["cwd"] = serde_json::json!(project_cwd);
    let target_snapshot: fresh_core::hooks::OmpCompanionSnapshotV1 =
        serde_json::from_value(target_json).unwrap();
    let mut peer_json = valid_omp_companion_envelope()["snapshot"].clone();
    peer_json["sessionId"] = serde_json::json!("123e4567-e89b-42d3-a456-426614174001");
    peer_json["cwd"] = serde_json::json!(project_cwd);
    let peer_snapshot: fresh_core::hooks::OmpCompanionSnapshotV1 =
        serde_json::from_value(peer_json).unwrap();
    assert!(target_live.test_install_snapshot(&target_snapshot));
    assert!(peer_live.test_install_snapshot(&peer_snapshot));

    editor
        .session(target_window)
        .unwrap()
        .bridge
        .sender()
        .send(AsyncMessage::OmpCompanionSnapshotReady {
            terminal: fresh_core::WindowTerminalId::new(target_window, target_terminal),
        })
        .unwrap();
    editor.process_async_messages();

    assert_eq!(
        editor
            .session(target_window)
            .unwrap()
            .terminal_resume_commands
            .get(&target_terminal),
        Some(&vec![
            executable.clone(),
            "--resume".into(),
            target_snapshot.session_id,
        ])
    );
    assert_eq!(
        editor
            .session(peer_window)
            .unwrap()
            .terminal_resume_commands
            .get(&peer_terminal)
            .cloned(),
        peer_resume_before,
        "the active peer with the same terminal id must remain untouched",
    );
}
