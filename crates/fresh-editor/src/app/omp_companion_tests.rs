// Focused tests for the OMP companion boundary and delivery state.
use super::*;
use std::cell::{Cell, RefCell};

const TEST_SYNC_B64: &[u8] = b"AAAAAAAAAAAAAAAAAAAAAA";
const TEST_TAG_B64: &[u8] = b"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

fn valid_omp_companion_envelope() -> serde_json::Value {
    serde_json::json!({
        "version": 1,
        "type": "snapshot",
        "snapshot": {
            "version": 1,
            "incarnation": "550e8400-e29b-41d4-a716-446655440000",
            "sequence": 1,
            "sessionGeneration": 1,
            "timestampMs": 0,
            "ompVersion": "0.52.1",
            "processId": 1234,
            "sessionId": "123e4567-e89b-12d3-a456-426614174000",
            "sessionName": "Companion session",
            "cwd": "/tmp/project",
            "state": "working",
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
    accept_sequence: impl FnOnce(&str, u64) -> bool,
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
        launch_executable: "omp".to_string(),
        snapshot: serde_json::from_value(envelope["snapshot"].clone()).unwrap(),
    }
}

#[test]
fn omp_companion_parser_authenticates_before_json_and_accepts_valid_snapshot() {
    let candidate = omp_candidate(&valid_omp_companion_envelope());
    let snapshot = parse_test_candidate(&candidate, |incarnation, sequence| {
        assert_eq!(incarnation, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(sequence, 1);
        true
    })
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
        |_, _| {
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

    let mut explicit_null = valid_omp_companion_envelope();
    explicit_null["snapshot"]["sessionName"] = serde_json::Value::Null;
    cases.push(explicit_null);

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

    let mut byte_oversize = valid_omp_companion_envelope();
    byte_oversize["snapshot"]["model"]["provider"] = serde_json::json!("🦀".repeat(65));
    cases.push(byte_oversize);

    for invalid in cases {
        let sequence_checked = Cell::new(false);
        assert!(parse_test_candidate(&omp_candidate(&invalid), |_, _| {
            sequence_checked.set(true);
            true
        })
        .is_none());
        assert!(
            !sequence_checked.get(),
            "sequence state must not advance for an invalid snapshot: {invalid}"
        );
    }

    let body = serde_json::to_string(&valid_omp_companion_envelope()).unwrap();
    let duplicate = body.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
    assert!(
        parse_test_candidate(&omp_candidate_from_bytes(duplicate.as_bytes()), |_, _| true)
            .is_none()
    );
}

#[test]
fn omp_companion_parser_enforces_canonical_encoding_and_wire_caps() {
    let body = serde_json::to_vec(&valid_omp_companion_envelope()).unwrap();
    let canonical = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&body);

    let padded = format!("{canonical}=");
    assert!(parse_test_candidate(
        &omp_candidate_from_encoded_body(padded.as_bytes()),
        |_, _| true,
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
    assert!(
        parse_test_candidate(&omp_candidate_from_encoded_body(&noncanonical), |_, _| true,)
            .is_none()
    );

    let auth_checked = Cell::new(false);
    let oversized_body = vec![b'A'; OMP_COMPANION_BODY_B64_MAX + 1];
    assert!(parse_omp_companion_candidate_with(
        &omp_candidate_from_encoded_body(&oversized_body),
        |_, _, _| {
            auth_checked.set(true);
            true
        },
        |_, _| true,
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
        |_, _| true,
    )
    .is_none());
    assert!(!frame_auth_checked.get());

    let mut wrong_terminator = omp_candidate(&valid_omp_companion_envelope());
    wrong_terminator.truncate(wrong_terminator.len() - OMP_OUTPUT_TERMINATOR.len());
    wrong_terminator.push(0x07);
    assert!(parse_test_candidate(&wrong_terminator, |_, _| true).is_none());
}

#[test]
fn omp_companion_parser_rejects_stale_sequence_and_accepts_new_incarnation() {
    let accepted = RefCell::new(None::<(String, u64)>);
    let accept = |incarnation: &str, sequence: u64| {
        let mut last = accepted.borrow_mut();
        if last
            .as_ref()
            .is_some_and(|(last_incarnation, last_sequence)| {
                last_incarnation == incarnation && sequence <= *last_sequence
            })
        {
            return false;
        }
        *last = Some((incarnation.to_string(), sequence));
        true
    };

    let first = valid_omp_companion_envelope();
    assert!(parse_test_candidate(&omp_candidate(&first), accept).is_some());
    assert!(parse_test_candidate(&omp_candidate(&first), accept).is_none());

    let mut higher = first.clone();
    higher["snapshot"]["sequence"] = serde_json::json!(2);
    assert!(parse_test_candidate(&omp_candidate(&higher), accept).is_some());

    let mut new_incarnation = first;
    new_incarnation["snapshot"]["incarnation"] =
        serde_json::json!("6ba7b810-9dad-41d1-80b4-00c04fd430c8");
    assert!(parse_test_candidate(&omp_candidate(&new_incarnation), accept).is_some());
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
