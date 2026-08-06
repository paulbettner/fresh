//! OMP companion parsing, snapshot delivery, and editor integration.
//!
//! This module owns the authenticated companion boundary and its globally
//! serialized, fair latest-only hook delivery. Async routing and terminal-exit
//! paths delegate here without duplicating companion state.

use base64::Engine as _;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::services::terminal::manager::{
    OMP_OUTPUT_FRAME_MAX, OMP_OUTPUT_PREFIX, OMP_OUTPUT_TERMINATOR, OMP_SYNC_B64_LEN,
    OMP_TAG_B64_LEN,
};

use super::Editor;

const OMP_COMPANION_BODY_B64_MAX: usize = 10_923;
const OMP_COMPANION_BODY_MAX: usize = 8_192;
const JS_SAFE_INTEGER_MAX: u64 = 9_007_199_254_740_991;
const OMP_COMPANION_COUNT_MAX: u64 = 2_147_483_647;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OmpCompanionEnvelopeV1 {
    version: u8,
    #[serde(rename = "type")]
    message_type: OmpCompanionEnvelopeType,
    snapshot: fresh_core::hooks::OmpCompanionSnapshotV1,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum OmpCompanionEnvelopeType {
    Snapshot,
}

fn is_base64url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn is_canonical_base64url(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().copied().all(is_base64url_byte)
}

fn is_forbidden_companion_scalar(ch: char) -> bool {
    let scalar = ch as u32;
    matches!(scalar, 0x0000..=0x001f | 0x007f..=0x009f | 0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069 | 0xfdd0..=0xfdef)
        || scalar & 0xffff == 0xfffe
        || scalar & 0xffff == 0xffff
        || (ch != ' ' && ch.is_whitespace())
}

fn is_normalized_companion_string(value: &str, code_points: usize, utf8_bytes: usize) -> bool {
    if value.is_empty() || value.len() > utf8_bytes {
        return false;
    }
    let mut count = 0usize;
    let mut previous_space = false;
    for ch in value.chars() {
        count += 1;
        if count > code_points || is_forbidden_companion_scalar(ch) {
            return false;
        }
        if ch == ' ' {
            if count == 1 || previous_space {
                return false;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
    }
    !previous_space
}

fn is_canonical_uuid(value: &str, require_v4: bool) -> bool {
    if value.len() != 36 || !value.is_ascii() {
        return false;
    }
    let Ok(uuid) = uuid::Uuid::parse_str(value) else {
        return false;
    };
    if uuid.hyphenated().to_string() != value {
        return false;
    }
    !require_v4 || (uuid.get_version_num() == 4 && uuid.get_variant() == uuid::Variant::RFC4122)
}

fn validate_omp_companion_snapshot(snapshot: &fresh_core::hooks::OmpCompanionSnapshotV1) -> bool {
    use fresh_core::hooks::OmpCompanionSnapshotV1;

    let OmpCompanionSnapshotV1 {
        version,
        incarnation,
        sequence,
        session_generation,
        timestamp_ms,
        omp_version,
        process_id,
        session_id,
        session_name,
        cwd,
        state: _,
        status_text,
        model,
        thinking_level: _,
        running_tools,
        current_tool,
        goal,
        todos,
        context,
        pending_approvals,
        async_jobs,
    } = snapshot;

    if *version != 1
        || !is_canonical_uuid(incarnation, true)
        || !is_canonical_uuid(session_id, false)
        || !(1..=JS_SAFE_INTEGER_MAX).contains(sequence)
        || !(1..=JS_SAFE_INTEGER_MAX).contains(session_generation)
        || *timestamp_ms > JS_SAFE_INTEGER_MAX
        || !(1..=u32::MAX as u64).contains(process_id)
        || !is_normalized_companion_string(omp_version, 64, 128)
        || !is_normalized_companion_string(cwd, 512, 2_048)
        || *running_tools > OMP_COMPANION_COUNT_MAX
        || *pending_approvals > OMP_COMPANION_COUNT_MAX
    {
        return false;
    }
    if session_name
        .as_deref()
        .is_some_and(|value| !is_normalized_companion_string(value, 160, 640))
    {
        return false;
    }
    if status_text
        .as_deref()
        .is_some_and(|value| !is_normalized_companion_string(value, 240, 960))
    {
        return false;
    }
    if let Some(model) = model {
        if !is_normalized_companion_string(&model.provider, 128, 256)
            || !is_normalized_companion_string(&model.id, 128, 256)
        {
            return false;
        }
    }
    if let Some(tool) = current_tool {
        if !is_normalized_companion_string(&tool.name, 80, 320)
            || tool
                .intent
                .as_deref()
                .is_some_and(|value| !is_normalized_companion_string(value, 160, 640))
        {
            return false;
        }
    }
    if let Some(goal) = goal {
        if !is_normalized_companion_string(&goal.objective, 240, 960) {
            return false;
        }
    }
    if let Some(todos) = todos {
        if [
            todos.pending,
            todos.in_progress,
            todos.blocked,
            todos.completed,
            todos.abandoned,
        ]
        .into_iter()
        .any(|count| count > OMP_COMPANION_COUNT_MAX)
            || todos
                .current
                .as_deref()
                .is_some_and(|value| !is_normalized_companion_string(value, 240, 960))
        {
            return false;
        }
    }
    if let Some(context) = context {
        if context.tokens > JS_SAFE_INTEGER_MAX
            || context.context_window > JS_SAFE_INTEGER_MAX
            || context.percent_bps > 10_000
        {
            return false;
        }
    }
    if let Some(async_jobs) = async_jobs {
        if [
            async_jobs.running,
            async_jobs.recent_failures,
            async_jobs.pending_delivery,
        ]
        .into_iter()
        .any(|count| count > OMP_COMPANION_COUNT_MAX)
        {
            return false;
        }
    }
    true
}

fn parse_omp_companion_candidate(
    candidate: &[u8],
    live: &crate::services::terminal::manager::OmpCompanionLiveState,
) -> Option<fresh_core::hooks::OmpCompanionSnapshotV1> {
    parse_omp_companion_candidate_with(
        candidate,
        |sync_b64, body_b64, tag_b64| live.verify_output_auth(sync_b64, body_b64, tag_b64),
        |incarnation, sequence| live.accept_sequence(incarnation, sequence),
    )
}

fn parse_omp_companion_candidate_with<Verify, Accept>(
    candidate: &[u8],
    verify_auth: Verify,
    accept_sequence: Accept,
) -> Option<fresh_core::hooks::OmpCompanionSnapshotV1>
where
    Verify: FnOnce(&[u8], &[u8], &[u8]) -> bool,
    Accept: FnOnce(&str, u64) -> bool,
{
    if candidate.len() > OMP_OUTPUT_FRAME_MAX
        || !candidate.starts_with(OMP_OUTPUT_PREFIX)
        || !candidate.ends_with(OMP_OUTPUT_TERMINATOR)
    {
        return None;
    }
    let frame = &candidate[OMP_OUTPUT_PREFIX.len()..candidate.len() - OMP_OUTPUT_TERMINATOR.len()];
    if frame.len() < OMP_SYNC_B64_LEN + 1 + 1 + OMP_TAG_B64_LEN
        || frame.get(OMP_SYNC_B64_LEN) != Some(&b';')
    {
        return None;
    }
    let sync_b64 = &frame[..OMP_SYNC_B64_LEN];
    let authenticated = &frame[OMP_SYNC_B64_LEN + 1..];
    let separator = authenticated.iter().position(|byte| *byte == b'.')?;
    if authenticated[separator + 1..].contains(&b'.') {
        return None;
    }
    let body_b64 = &authenticated[..separator];
    let tag_b64 = &authenticated[separator + 1..];
    if body_b64.len() > OMP_COMPANION_BODY_B64_MAX
        || tag_b64.len() != OMP_TAG_B64_LEN
        || !is_canonical_base64url(sync_b64)
        || !is_canonical_base64url(body_b64)
        || !is_canonical_base64url(tag_b64)
        || !verify_auth(sync_b64, body_b64, tag_b64)
    {
        return None;
    }

    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body_b64)
        .ok()?;
    if body.len() > OMP_COMPANION_BODY_MAX
        || base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(&body)
            .as_bytes()
            != body_b64
    {
        return None;
    }
    let envelope: OmpCompanionEnvelopeV1 = serde_json::from_slice(&body).ok()?;
    if envelope.version != 1
        || !matches!(envelope.message_type, OmpCompanionEnvelopeType::Snapshot)
        || !validate_omp_companion_snapshot(&envelope.snapshot)
        || !accept_sequence(&envelope.snapshot.incarnation, envelope.snapshot.sequence)
    {
        return None;
    }
    Some(envelope.snapshot)
}

fn editor_receipt_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(JS_SAFE_INTEGER_MAX as u128) as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub(crate) struct OmpCompanionHookPayload {
    pub terminal: fresh_core::WindowTerminalId,
    pub received_at_ms: u64,
    pub launch_executable: String,
    pub snapshot: fresh_core::hooks::OmpCompanionSnapshotV1,
}

/// Editor-thread state for the globally serialized companion hook.
///
/// `latest` is the sole dirty slot per terminal. Queue membership is separate
/// so replacing a hot terminal's payload never moves it ahead of a peer. An
/// in-flight terminal can become dirty again, but is appended only after its
/// current `HookCompleted` arrives.
#[derive(Debug, Default)]
pub(crate) struct OmpCompanionHookDelivery {
    queue: VecDeque<fresh_core::WindowTerminalId>,
    queued: HashSet<fresh_core::WindowTerminalId>,
    in_flight: Option<fresh_core::WindowTerminalId>,
    latest: HashMap<fresh_core::WindowTerminalId, OmpCompanionHookPayload>,
    // The only ambiguous completion is the single name-only hook in flight.
    in_flight_tombstoned: bool,
}

impl OmpCompanionHookDelivery {
    pub(crate) fn push(&mut self, payload: OmpCompanionHookPayload) -> bool {
        let terminal = payload.terminal;
        if self.in_flight == Some(terminal) && self.in_flight_tombstoned {
            return false;
        }
        self.latest.insert(terminal, payload);
        if self.in_flight != Some(terminal) && self.queued.insert(terminal) {
            self.queue.push_back(terminal);
        }
        true
    }

    pub(crate) fn take_next(&mut self) -> Option<OmpCompanionHookPayload> {
        if self.in_flight.is_some() {
            return None;
        }
        while let Some(terminal) = self.queue.pop_front() {
            self.queued.remove(&terminal);
            if let Some(payload) = self.latest.remove(&terminal) {
                self.in_flight = Some(terminal);
                return Some(payload);
            }
        }
        None
    }

    pub(crate) fn complete_in_flight(&mut self) {
        let Some(terminal) = self.in_flight.take() else {
            return;
        };
        // A terminal closed while its hook was in flight remains suppressed
        // through this exact name-only completion, then can be retired.
        let was_tombstoned = std::mem::take(&mut self.in_flight_tombstoned);
        if !was_tombstoned && self.latest.contains_key(&terminal) && self.queued.insert(terminal) {
            self.queue.push_back(terminal);
        }
    }

    pub(crate) fn purge(&mut self, terminal: fresh_core::WindowTerminalId) {
        if self.in_flight == Some(terminal) {
            self.in_flight_tombstoned = true;
        }
        self.latest.remove(&terminal);
        self.queued.remove(&terminal);
        self.queue.retain(|queued| *queued != terminal);
        // Do not clear an in-flight identity. Its completion has no terminal
        // id, so clearing it would let that sentinel acknowledge a peer.
    }

    pub(crate) fn is_tombstoned(&self, terminal: fresh_core::WindowTerminalId) -> bool {
        self.in_flight == Some(terminal) && self.in_flight_tombstoned
    }

    #[cfg(test)]
    fn tombstone_count(&self) -> usize {
        usize::from(self.in_flight_tombstoned)
    }
}

impl Editor {
    pub(super) fn handle_omp_companion_snapshot_ready(
        &mut self,
        terminal: fresh_core::WindowTerminalId,
    ) {
        if self.omp_companion_delivery.is_tombstoned(terminal) {
            return;
        }
        let (live, launch_executable) = {
            let Some(window) = self.windows.get(&terminal.window) else {
                return;
            };
            let Some(handle) = window.terminal_manager.get(terminal.terminal) else {
                return;
            };
            let Some(live) = handle.companion.as_ref().cloned() else {
                return;
            };
            let Some(launch_executable) = window
                .terminal_commands
                .get(&terminal.terminal)
                .and_then(|argv| argv.first())
                .filter(|executable| !executable.is_empty())
                .cloned()
            else {
                return;
            };
            (live, launch_executable)
        };
        let Some(candidate) = live.take_candidate() else {
            return;
        };
        let Some(snapshot) = parse_omp_companion_candidate(&candidate, &live) else {
            return;
        };
        let payload = OmpCompanionHookPayload {
            terminal,
            received_at_ms: editor_receipt_time_ms(),
            launch_executable,
            snapshot,
        };
        if self.omp_companion_delivery.push(payload) {
            self.dispatch_next_omp_companion_hook();
        }
    }

    fn dispatch_next_omp_companion_hook(&mut self) {
        loop {
            let Some(payload) = self.omp_companion_delivery.take_next() else {
                return;
            };
            if !self.plugin_manager.read().unwrap().is_active() {
                self.omp_companion_delivery.complete_in_flight();
                continue;
            }
            self.plugin_manager.read().unwrap().run_hook(
                "omp_companion_snapshot",
                fresh_core::hooks::HookArgs::OmpCompanionSnapshot {
                    window_id: payload.terminal.window.0,
                    terminal_id: payload.terminal.terminal.0 as u64,
                    received_at_ms: payload.received_at_ms,
                    launch_executable: payload.launch_executable,
                    snapshot: payload.snapshot,
                },
            );
            return;
        }
    }

    pub(super) fn complete_omp_companion_hook(&mut self, hook_name: &str) {
        if hook_name != "omp_companion_snapshot" {
            return;
        }
        self.omp_companion_delivery.complete_in_flight();
        self.dispatch_next_omp_companion_hook();
    }

    pub(super) fn purge_omp_companion_terminal(&mut self, terminal: fresh_core::WindowTerminalId) {
        if let Some(handle) = self
            .windows
            .get(&terminal.window)
            .and_then(|window| window.terminal_manager.get(terminal.terminal))
        {
            handle.revoke_omp_companion();
        }
        self.omp_companion_delivery.purge(terminal);
    }
}
#[cfg(test)]
#[path = "omp_companion_tests.rs"]
mod tests;
