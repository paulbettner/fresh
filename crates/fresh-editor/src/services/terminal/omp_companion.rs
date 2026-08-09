//! Private OMP companion transport for terminal sessions.
//!
//! This module owns the authenticated framing capability that is shared by a
//! Fresh-owned OMP child, its PTY reader, and the editor-side parser.

use base64::Engine;
use fresh_core::api::{OmpCompanionCommandTargetV1, OmpCompanionCommandType, TerminalCompanion};
use hmac::{Hmac, Mac};
use interprocess::local_socket::{
    traits::Listener as _, GenericFilePath, ListenerNonblockingMode, ListenerOptions, ToFsName,
};
use sha2::Sha256;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

pub(crate) const OMP_OUTPUT_PREFIX: &[u8] = b"\x1b]777;notify;fresh://omp-companion;v1;";
const OMP_SYNC_DOMAIN: &[u8] = b"fresh-omp/sync/v1\0";
const OMP_OUTPUT_DOMAIN: &[u8] = b"fresh-omp/out/v1\0";
const OMP_COMMAND_DOMAIN: &[u8] = b"fresh-omp/in/v1\0";
pub(crate) const OMP_SYNC_B64_LEN: usize = 22;
pub(crate) const OMP_TAG_B64_LEN: usize = 43;
pub(crate) const OMP_OUTPUT_FRAME_MAX: usize = 12 * 1024;
pub(crate) const OMP_OUTPUT_TERMINATOR: &[u8] = b"\x1b\\";
const OMP_OUTPUT_FRAME_LIFETIME: Duration = Duration::from_secs(1);
const OMP_COMMAND_START: &[u8] = "\u{10ffff}fresh-omp-command:v1:".as_bytes();
const OMP_COMMAND_END: &[u8] = "\u{10fffe}".as_bytes();
const OMP_COMMAND_SEQUENCE_MAX: u64 = 9_007_199_254_740_991;
const OMP_PENDING_CANDIDATES_MAX: usize = 64;
const OMP_RETIRED_INCARNATIONS_MAX: usize = 64;
const OMP_SECRET_CHANNEL_LIFETIME: Duration = Duration::from_secs(5);

struct OmpCompanionSecretChannel {
    endpoint: String,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    committed: bool,
}

impl OmpCompanionSecretChannel {
    fn bind(secret: &[u8; 32]) -> io::Result<Self> {
        let endpoint = omp_companion_secret_endpoint()?;
        let name = std::path::PathBuf::from(&endpoint)
            .to_fs_name::<GenericFilePath>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let options = ListenerOptions::new()
            .name(name)
            .nonblocking(ListenerNonblockingMode::Accept);
        let listener = options.create_sync()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let mut channel_secret = *secret;
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + OMP_SECRET_CHANNEL_LIFETIME;
            while !worker_cancel.load(Ordering::Acquire) && Instant::now() < deadline {
                match listener.accept() {
                    Ok(mut stream) => {
                        if let Err(error) = stream
                            .write_all(&channel_secret)
                            .and_then(|()| stream.flush())
                        {
                            tracing::warn!("OMP companion secret channel write failed: {error}");
                        }
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => {
                        tracing::warn!("OMP companion secret channel accept failed: {error}");
                        break;
                    }
                }
            }
            channel_secret.zeroize();
        });
        Ok(Self {
            endpoint,
            cancel,
            worker: Some(worker),
            committed: false,
        })
    }

    fn commit(&mut self) {
        self.committed = true;
        self.worker.take();
    }
}

impl Drop for OmpCompanionSecretChannel {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn omp_companion_secret_endpoint() -> io::Result<String> {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id)
        .map_err(|error| io::Error::other(format!("endpoint entropy unavailable: {error}")))?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id);
    id.zeroize();
    #[cfg(windows)]
    {
        return Ok(format!(r"\\.\pipe\fresh-omp-{encoded}"));
    }
    #[cfg(unix)]
    {
        return Ok(crate::server::ipc::SocketPaths::socket_directory()?
            .join(format!("fresh-omp-{encoded}.sock"))
            .to_string_lossy()
            .into_owned());
    }
    #[cfg(not(any(unix, windows)))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "OMP companion secret channels are unsupported on this platform",
    ))
}

/// Spawn-only capability for a Fresh-owned OMP companion terminal.
///
/// The creator exposes the secret exactly once over a private local socket.
/// Construction copies the secret only into [`OmpCompanionLiveState`], then
/// drops and zeroes the spawn source before returning.
pub(crate) struct OmpCompanionSpawn {
    pub kind: TerminalCompanion,
    pub secret: [u8; 32],
    pub executable: String,
    pub resume_prefix: Vec<String>,
    launch_channel: Option<OmpCompanionSecretChannel>,
}

impl OmpCompanionSpawn {
    pub(crate) fn new(
        kind: TerminalCompanion,
        executable: String,
        resume_prefix: Vec<String>,
    ) -> Self {
        Self {
            kind,
            secret: [0; 32],
            executable,
            resume_prefix,
            launch_channel: None,
        }
    }

    pub(crate) fn arm_launch_channel(&mut self) -> io::Result<String> {
        let channel = OmpCompanionSecretChannel::bind(&self.secret)?;
        let endpoint = channel.endpoint.clone();
        self.launch_channel = Some(channel);
        Ok(endpoint)
    }
}

impl Drop for OmpCompanionSpawn {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

struct OmpQuarantinedCandidate {
    frame: Vec<u8>,
    started_at: Instant,
}

struct OmpDiscardedCandidate {
    terminator_match: usize,
    nested_probe_len: usize,
}

impl OmpDiscardedCandidate {
    fn new() -> Self {
        Self {
            terminator_match: 0,
            nested_probe_len: 0,
        }
    }
}

struct OmpOutputScanner {
    probe: Vec<u8>,
    quarantine: Option<OmpQuarantinedCandidate>,
    discard: Option<OmpDiscardedCandidate>,
}

impl OmpOutputScanner {
    fn new() -> Self {
        Self {
            probe: Vec::with_capacity(OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 1),
            quarantine: None,
            discard: None,
        }
    }

    fn preamble_len() -> usize {
        OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 1
    }

    fn expected_probe_byte(sync_b64: &[u8; OMP_SYNC_B64_LEN], index: usize) -> Option<u8> {
        if index < OMP_OUTPUT_PREFIX.len() {
            Some(OMP_OUTPUT_PREFIX[index])
        } else if index < OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN {
            Some(sync_b64[index - OMP_OUTPUT_PREFIX.len()])
        } else if index == OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN {
            Some(b';')
        } else {
            None
        }
    }

    fn preamble(sync_b64: &[u8; OMP_SYNC_B64_LEN]) -> Vec<u8> {
        let mut preamble = Vec::with_capacity(Self::preamble_len());
        preamble.extend_from_slice(OMP_OUTPUT_PREFIX);
        preamble.extend_from_slice(sync_b64);
        preamble.push(b';');
        preamble
    }

    fn frame_ends_with_preamble(frame: &[u8], sync_b64: &[u8; OMP_SYNC_B64_LEN]) -> bool {
        let preamble_len = Self::preamble_len();
        frame.len() > preamble_len
            && frame[frame.len() - preamble_len..]
                .iter()
                .copied()
                .enumerate()
                .all(|(index, byte)| Self::expected_probe_byte(sync_b64, index) == Some(byte))
    }

    fn discard_quarantine(&mut self) {
        if let Some(mut candidate) = self.quarantine.take() {
            candidate.frame.zeroize();
            self.discard = Some(OmpDiscardedCandidate::new());
        }
    }

    fn expire_candidate(&mut self, now: Instant) {
        if self.quarantine.as_ref().is_some_and(|candidate| {
            now.saturating_duration_since(candidate.started_at) >= OMP_OUTPUT_FRAME_LIFETIME
        }) {
            self.discard_quarantine();
        }
    }

    fn feed(
        &mut self,
        bytes: &[u8],
        now: Instant,
        sync_b64: &[u8; OMP_SYNC_B64_LEN],
        visible: &mut Vec<u8>,
        boundaries: &mut Vec<usize>,
        completed: &mut Vec<Vec<u8>>,
    ) {
        self.expire_candidate(now);
        if self.probe.is_empty()
            && self.quarantine.is_none()
            && self.discard.is_none()
            && !bytes.contains(&OMP_OUTPUT_PREFIX[0])
        {
            visible.extend_from_slice(bytes);
            return;
        }
        for &byte in bytes {
            self.feed_byte(byte, now, sync_b64, visible, boundaries, completed);
        }
    }

    fn feed_discard_byte(
        &mut self,
        byte: u8,
        now: Instant,
        sync_b64: &[u8; OMP_SYNC_B64_LEN],
        boundaries: &mut Vec<usize>,
        visible_len: usize,
    ) {
        let Some(discard) = self.discard.as_mut() else {
            return;
        };
        // Once discard begins, every byte remains private until an exact
        // terminator, EOF, or authenticated nested preamble ends quarantine.

        discard.terminator_match = match (discard.terminator_match, byte) {
            (0, b'\x1b') => 1,
            (1, b'\\') => {
                self.discard = None;
                return;
            }
            (1, b'\x1b') => 1,
            _ => 0,
        };

        if Self::expected_probe_byte(sync_b64, discard.nested_probe_len) == Some(byte) {
            discard.nested_probe_len += 1;
            if discard.nested_probe_len == Self::preamble_len() {
                self.discard = None;
                self.quarantine = Some(OmpQuarantinedCandidate {
                    frame: Self::preamble(sync_b64),
                    started_at: now,
                });
                boundaries.push(visible_len);
            }
        } else {
            discard.nested_probe_len = usize::from(byte == OMP_OUTPUT_PREFIX[0]);
        }
    }

    fn feed_byte(
        &mut self,
        byte: u8,
        now: Instant,
        sync_b64: &[u8; OMP_SYNC_B64_LEN],
        visible: &mut Vec<u8>,
        boundaries: &mut Vec<usize>,
        completed: &mut Vec<Vec<u8>>,
    ) {
        loop {
            if self.discard.is_some() {
                self.feed_discard_byte(byte, now, sync_b64, boundaries, visible.len());
                return;
            }

            if let Some(mut candidate) = self.quarantine.take() {
                if now.saturating_duration_since(candidate.started_at) >= OMP_OUTPUT_FRAME_LIFETIME
                    || candidate.frame.len() >= OMP_OUTPUT_FRAME_MAX
                {
                    candidate.frame.zeroize();
                    self.discard = Some(OmpDiscardedCandidate::new());
                    continue;
                }
                candidate.frame.push(byte);
                if candidate.frame.ends_with(OMP_OUTPUT_TERMINATOR) {
                    completed.push(candidate.frame);
                } else if Self::frame_ends_with_preamble(&candidate.frame, sync_b64) {
                    let preamble_at = candidate.frame.len() - Self::preamble_len();
                    let nested = candidate.frame.split_off(preamble_at);
                    candidate.frame.zeroize();
                    self.quarantine = Some(OmpQuarantinedCandidate {
                        frame: nested,
                        started_at: now,
                    });
                    boundaries.push(visible.len());
                } else if candidate.frame.len() < OMP_OUTPUT_FRAME_MAX {
                    self.quarantine = Some(candidate);
                } else {
                    candidate.frame.zeroize();
                    self.discard = Some(OmpDiscardedCandidate::new());
                }
                return;
            }

            let Some(expected) = Self::expected_probe_byte(sync_b64, self.probe.len()) else {
                debug_assert!(false, "OMP companion probe exceeded its fixed preamble");
                self.probe.zeroize();
                continue;
            };
            if byte == expected {
                self.probe.push(byte);
                if self.probe.len() == Self::preamble_len() {
                    self.quarantine = Some(OmpQuarantinedCandidate {
                        frame: std::mem::take(&mut self.probe),
                        started_at: now,
                    });
                    self.probe = Vec::with_capacity(Self::preamble_len());
                    boundaries.push(visible.len());
                }
                return;
            }

            if self.probe.is_empty() {
                visible.push(byte);
                return;
            }

            // A failed fixed-prefix or synchronizer match is ordinary output.
            // Replay the matched bytes, then reconsider the mismatching byte so
            // an ESC that begins an adjacent real frame is not lost.
            visible.extend_from_slice(&self.probe);
            self.probe.zeroize();
        }
    }

    fn finish_eof(&mut self) -> Vec<u8> {
        self.clear();
        Vec::new()
    }

    fn clear(&mut self) {
        self.probe.zeroize();
        if let Some(mut candidate) = self.quarantine.take() {
            candidate.frame.zeroize();
        }
        self.discard = None;
    }
}
struct OmpPendingSnapshot {
    incarnation: String,
    sequence: u64,
    session_generation: u64,
    session_id: String,
    work_epoch: u64,
}

impl OmpPendingSnapshot {
    fn matches(&self, snapshot: &fresh_core::hooks::OmpCompanionSnapshotV1) -> bool {
        self.incarnation == snapshot.incarnation
            && self.sequence == snapshot.sequence
            && self.session_generation == snapshot.session_generation
            && self.session_id == snapshot.session_id
            && self.work_epoch == snapshot.work_epoch
    }
}

struct OmpCompanionStateInner {
    secret: [u8; 32],
    sync_b64: [u8; OMP_SYNC_B64_LEN],
    access_revoked: bool,
    command_access_revoked: bool,
    output_filter_finalized: bool,
    scanner: OmpOutputScanner,
    sequence_incarnation: Option<String>,
    current_incarnation: Option<String>,
    retired_incarnations: VecDeque<String>,
    last_sequence: u64,
    last_session_generation: u64,
    current_session_id: Option<String>,
    checkpointed_session_id: Option<String>,
    current_work_epoch: u64,
    pending_snapshot: Option<OmpPendingSnapshot>,
    last_command_sequence: u64,
    candidates: VecDeque<Vec<u8>>,
    notification_pending: bool,
}

/// Ephemeral capability shared by the terminal handle and its PTY reader.
pub(crate) struct OmpCompanionLiveState {
    kind: TerminalCompanion,
    executable: String,
    resume_prefix: Vec<String>,
    inner: Mutex<OmpCompanionStateInner>,
}

impl OmpCompanionLiveState {
    pub(crate) fn new(mut spawn: OmpCompanionSpawn) -> Self {
        if let Some(channel) = spawn.launch_channel.as_mut() {
            channel.commit();
        }
        let mut sync_b64 = omp_companion_synchronizer(&spawn.secret);
        let executable = std::mem::take(&mut spawn.executable);
        let resume_prefix = std::mem::take(&mut spawn.resume_prefix);
        let state = Self {
            kind: spawn.kind,
            executable,
            resume_prefix,
            inner: Mutex::new(OmpCompanionStateInner {
                secret: spawn.secret,
                sync_b64,
                access_revoked: false,
                command_access_revoked: false,
                output_filter_finalized: false,
                scanner: OmpOutputScanner::new(),
                sequence_incarnation: None,
                current_incarnation: None,
                retired_incarnations: VecDeque::new(),
                last_sequence: 0,
                last_session_generation: 0,
                current_session_id: None,
                checkpointed_session_id: None,
                current_work_epoch: 0,
                pending_snapshot: None,
                last_command_sequence: 0,
                candidates: VecDeque::new(),
                notification_pending: false,
            }),
        };
        sync_b64.zeroize();
        drop(spawn);
        state
    }

    pub(crate) fn kind(&self) -> TerminalCompanion {
        self.kind
    }

    pub(crate) fn exact_resume_argv(&self, session_id: &str) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.resume_prefix.len() + 3);
        argv.push(self.executable.clone());
        argv.extend(self.resume_prefix.iter().cloned());
        argv.push("--resume".to_string());
        argv.push(session_id.to_string());
        argv
    }

    fn enqueue_candidates(
        inner: &mut OmpCompanionStateInner,
        mut candidates: Vec<Vec<u8>>,
    ) -> bool {
        if inner.access_revoked {
            for candidate in &mut candidates {
                candidate.zeroize();
            }
            return false;
        }
        candidates.retain_mut(|candidate| {
            if candidate.len() <= OMP_OUTPUT_FRAME_MAX {
                true
            } else {
                candidate.zeroize();
                false
            }
        });
        if inner.candidates.len().saturating_add(candidates.len()) > OMP_PENDING_CANDIDATES_MAX {
            for candidate in &mut candidates {
                candidate.zeroize();
            }
            Self::revoke_access_inner(inner);
            return false;
        }
        inner.candidates.extend(candidates);
        if inner.notification_pending || inner.candidates.is_empty() {
            false
        } else {
            inner.notification_pending = true;
            true
        }
    }

    #[cfg(test)]
    fn enqueue_candidate(&self, candidate: Vec<u8>) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            let mut candidate = candidate;
            candidate.zeroize();
            return false;
        };
        Self::enqueue_candidates(&mut inner, vec![candidate])
    }

    pub(crate) fn take_candidates(&self) -> Vec<Vec<u8>> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        if inner.access_revoked {
            return Vec::new();
        }
        inner.notification_pending = false;
        inner.candidates.drain(..).collect()
    }

    #[cfg(test)]
    pub(crate) fn take_candidate(&self) -> Option<Vec<u8>> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        if inner.access_revoked {
            return None;
        }
        let candidate = inner.candidates.pop_front();
        if inner.candidates.is_empty() {
            inner.notification_pending = false;
        }
        candidate
    }

    pub(crate) fn verify_output_auth(
        &self,
        sync_b64: &[u8],
        body_b64: &[u8],
        tag_b64: &[u8],
    ) -> bool {
        let Ok(inner) = self.inner.lock() else {
            return false;
        };
        if inner.access_revoked {
            return false;
        }
        let mut expected_tag = omp_companion_output_tag(&inner.secret, body_b64);
        let authenticated = inner.sync_b64.ct_eq(sync_b64) & expected_tag.ct_eq(tag_b64);
        expected_tag.zeroize();
        authenticated.into()
    }

    pub(crate) fn admit_sequence(
        &self,
        incarnation: &str,
        sequence: u64,
        session_generation: u64,
        session_id: &str,
        work_epoch: u64,
    ) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner.access_revoked
            || inner
                .retired_incarnations
                .iter()
                .any(|retired| retired == incarnation)
            || session_generation < inner.last_session_generation
        {
            return false;
        }

        if inner.sequence_incarnation.as_deref() == Some(incarnation) {
            if sequence <= inner.last_sequence
                || (session_generation == inner.last_session_generation
                    && (inner.current_session_id.as_deref() != Some(session_id)
                        || work_epoch < inner.current_work_epoch))
            {
                return false;
            }
        } else {
            if inner.sequence_incarnation.is_some()
                && inner.retired_incarnations.len() >= OMP_RETIRED_INCARNATIONS_MAX
            {
                return false;
            }
            if let Some(previous) = inner.sequence_incarnation.replace(incarnation.to_owned()) {
                inner.retired_incarnations.push_back(previous);
            }
            inner.last_sequence = 0;
        }

        inner.last_sequence = sequence;
        let pending = OmpPendingSnapshot {
            incarnation: incarnation.to_owned(),
            sequence,
            session_generation,
            session_id: session_id.to_owned(),
            work_epoch,
        };
        if let Some(mut previous) = inner.pending_snapshot.replace(pending) {
            previous.incarnation.zeroize();
            previous.session_id.zeroize();
        }
        true
    }

    pub(crate) fn resume_checkpointed_for(&self, session_id: &str) -> bool {
        let Ok(inner) = self.inner.lock() else {
            return false;
        };
        !inner.access_revoked && inner.checkpointed_session_id.as_deref() == Some(session_id)
    }

    pub(crate) fn commit_admitted_snapshot(
        &self,
        snapshot: &fresh_core::hooks::OmpCompanionSnapshotV1,
        checkpoint_resume: bool,
    ) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner.access_revoked
            || inner.sequence_incarnation.as_deref() != Some(snapshot.incarnation.as_str())
            || inner.last_sequence != snapshot.sequence
            || !inner
                .pending_snapshot
                .as_ref()
                .is_some_and(|pending| pending.matches(snapshot))
        {
            return false;
        }

        if let Some(mut previous) = inner
            .current_incarnation
            .replace(snapshot.incarnation.clone())
        {
            previous.zeroize();
        }
        inner.last_session_generation = snapshot.session_generation;
        inner.current_session_id = Some(snapshot.session_id.clone());
        inner.current_work_epoch = snapshot.work_epoch;
        if checkpoint_resume {
            inner.checkpointed_session_id = Some(snapshot.session_id.clone());
        }
        if let Some(mut pending) = inner.pending_snapshot.take() {
            pending.incarnation.zeroize();
            pending.session_id.zeroize();
        }
        true
    }

    pub(crate) fn filter_output_into(
        &self,
        bytes: &[u8],
        now: Instant,
        visible: &mut Vec<u8>,
        boundaries: &mut Vec<usize>,
    ) -> bool {
        visible.clear();
        boundaries.clear();
        let Ok(mut inner) = self.inner.lock() else {
            visible.extend_from_slice(bytes);
            return false;
        };
        if inner.output_filter_finalized {
            visible.extend_from_slice(bytes);
            return false;
        }

        let mut completed = Vec::new();
        let OmpCompanionStateInner {
            scanner, sync_b64, ..
        } = &mut *inner;
        scanner.feed(bytes, now, sync_b64, visible, boundaries, &mut completed);
        Self::enqueue_candidates(&mut inner, completed)
    }

    #[cfg(test)]
    fn filter_output(&self, bytes: &[u8], now: Instant) -> OmpFilteredOutput {
        let mut visible = Vec::with_capacity(bytes.len());
        let mut boundaries = Vec::new();
        let notify_candidate = self.filter_output_into(bytes, now, &mut visible, &mut boundaries);
        OmpFilteredOutput {
            visible,
            boundaries,
            notify_candidate,
        }
    }

    pub(crate) fn finish_output(&self) -> Vec<u8> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        if inner.output_filter_finalized {
            return Vec::new();
        }
        inner.scanner.finish_eof()
    }

    pub(crate) fn frame_command_if_active(
        &self,
        command: OmpCompanionCommandType,
        target: &OmpCompanionCommandTargetV1,
        alive: &AtomicBool,
    ) -> Option<Vec<u8>> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        if inner.access_revoked
            || inner.command_access_revoked
            || !alive.load(Ordering::Acquire)
            || inner.current_incarnation.as_deref() != Some(&target.incarnation)
            || inner.last_session_generation != target.session_generation
            || inner.current_session_id.as_deref() != Some(&target.session_id)
            || inner.current_work_epoch != target.work_epoch
        {
            return None;
        }
        let command_sequence = inner.last_command_sequence.checked_add(1)?;
        if command_sequence > OMP_COMMAND_SEQUENCE_MAX {
            return None;
        }
        inner.last_command_sequence = command_sequence;
        Some(frame_omp_companion_command(
            &inner.secret,
            command,
            target,
            command_sequence,
        ))
    }

    pub(crate) fn frame_snapshot_ack_if_current(
        &self,
        snapshot: &fresh_core::hooks::OmpCompanionSnapshotV1,
        accepted: bool,
        alive: &AtomicBool,
    ) -> Option<Vec<u8>> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        let pending_matches = inner
            .pending_snapshot
            .as_ref()
            .is_some_and(|pending| pending.matches(snapshot));
        if inner.access_revoked
            || inner.command_access_revoked
            || !alive.load(Ordering::Acquire)
            || inner.sequence_incarnation.as_deref() != Some(snapshot.incarnation.as_str())
            || inner.last_sequence != snapshot.sequence
            || !pending_matches
        {
            return None;
        }
        let command_sequence = inner.last_command_sequence.checked_add(1)?;
        if command_sequence > OMP_COMMAND_SEQUENCE_MAX {
            return None;
        }
        inner.last_command_sequence = command_sequence;
        let frame =
            frame_omp_companion_snapshot_ack(&inner.secret, snapshot, accepted, command_sequence);
        if !accepted {
            if let Some(mut pending) = inner.pending_snapshot.take() {
                pending.incarnation.zeroize();
                pending.session_id.zeroize();
            }
        }
        Some(frame)
    }

    /// Immediately revoke command authority while preserving completed output
    /// candidates and their verifier until the PTY reader has drained.
    pub(crate) fn revoke_commands(&self) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        Self::revoke_commands_inner(&mut inner);
    }

    /// Revoke authentication and candidate delivery after queued ready events
    /// have been handled. The scanner remains live until the PTY reader exits.
    pub(crate) fn revoke_access(&self) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        Self::revoke_access_inner(&mut inner);
        if inner.output_filter_finalized {
            inner.sync_b64.zeroize();
        }
    }

    /// Clear the output filter after the PTY reader has drained and flushed.
    pub(crate) fn finalize_output_filter(&self) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.scanner.clear();
        inner.output_filter_finalized = true;
        if inner.access_revoked {
            inner.sync_b64.zeroize();
        }
    }

    fn revoke_commands_inner(inner: &mut OmpCompanionStateInner) {
        inner.command_access_revoked = true;
        inner.last_command_sequence = 0;
    }

    fn revoke_access_inner(inner: &mut OmpCompanionStateInner) {
        Self::revoke_commands_inner(inner);
        inner.access_revoked = true;
        inner.secret.zeroize();
        inner.sequence_incarnation.zeroize();
        inner.current_incarnation.zeroize();
        for mut incarnation in inner.retired_incarnations.drain(..) {
            incarnation.zeroize();
        }
        inner.last_sequence = 0;
        inner.last_session_generation = 0;
        inner.current_session_id.zeroize();
        inner.checkpointed_session_id.zeroize();
        inner.current_work_epoch = 0;
        if let Some(mut pending) = inner.pending_snapshot.take() {
            pending.incarnation.zeroize();
            pending.session_id.zeroize();
        }
        for mut candidate in inner.candidates.drain(..) {
            candidate.zeroize();
        }
        inner.notification_pending = false;
    }

    #[cfg(test)]
    pub(crate) fn test_output_filter_finalized(&self) -> bool {
        let inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.output_filter_finalized
            && inner.scanner.probe.is_empty()
            && inner.scanner.quarantine.is_none()
            && inner.scanner.discard.is_none()
    }

    #[cfg(test)]
    pub(crate) fn test_install_snapshot(
        &self,
        snapshot: &fresh_core::hooks::OmpCompanionSnapshotV1,
    ) -> bool {
        let mut secret = match self.inner.lock() {
            Ok(inner) => inner.secret,
            Err(poisoned) => poisoned.into_inner().secret,
        };
        let body = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "type": "snapshot",
            "snapshot": snapshot,
        }))
        .expect("test snapshot must serialize");
        let installed = self.enqueue_candidate(test_output_frame(&secret, &body));
        secret.zeroize();
        installed
    }
}

impl Drop for OmpCompanionLiveState {
    fn drop(&mut self) {
        let inner = match self.inner.get_mut() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        Self::revoke_access_inner(inner);
        inner.sync_b64.zeroize();
        inner.scanner.clear();
        inner.output_filter_finalized = true;
    }
}

#[cfg(test)]
struct OmpFilteredOutput {
    visible: Vec<u8>,
    boundaries: Vec<usize>,
    notify_candidate: bool,
}

fn hmac_sha256(secret: &[u8; 32], domain: &[u8], payload: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .expect("a fixed 32-byte OMP companion secret is a valid HMAC key");
    mac.update(domain);
    mac.update(payload);
    let mut digest = mac.finalize().into_bytes();
    let mut output = [0u8; 32];
    output.copy_from_slice(&digest);
    digest[..].zeroize();
    output
}

pub(crate) fn omp_companion_synchronizer(secret: &[u8; 32]) -> [u8; OMP_SYNC_B64_LEN] {
    let mut mac = hmac_sha256(secret, OMP_SYNC_DOMAIN, &[]);
    let mut encoded = [0u8; OMP_SYNC_B64_LEN];
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode_slice(&mac[..16], &mut encoded)
        .expect("22-byte buffer exactly fits a 16-byte unpadded base64url value");
    mac.zeroize();
    encoded
}

pub(super) fn omp_companion_output_tag(
    secret: &[u8; 32],
    body_b64: &[u8],
) -> [u8; OMP_TAG_B64_LEN] {
    let mut mac = hmac_sha256(secret, OMP_OUTPUT_DOMAIN, body_b64);
    let mut encoded = [0u8; OMP_TAG_B64_LEN];
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode_slice(&mac, &mut encoded)
        .expect("43-byte buffer exactly fits a 32-byte unpadded base64url value");
    mac.zeroize();
    encoded
}

fn omp_companion_command_tag(secret: &[u8; 32], body_b64: &[u8]) -> [u8; OMP_TAG_B64_LEN] {
    let mut mac = hmac_sha256(secret, OMP_COMMAND_DOMAIN, body_b64);
    let mut encoded = [0u8; OMP_TAG_B64_LEN];
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode_slice(&mac, &mut encoded)
        .expect("43-byte buffer exactly fits a 32-byte unpadded base64url value");
    mac.zeroize();
    encoded
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OmpCompanionCommandWire<'a> {
    version: u8,
    #[serde(rename = "type")]
    command_type: OmpCompanionCommandType,
    incarnation: &'a str,
    session_generation: u64,
    session_id: &'a str,
    work_epoch: u64,
    command_sequence: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OmpCompanionSnapshotAckWire<'a> {
    version: u8,
    #[serde(rename = "type")]
    command_type: &'static str,
    incarnation: &'a str,
    sequence: u64,
    session_generation: u64,
    session_id: &'a str,
    work_epoch: u64,
    accepted: bool,
    command_sequence: u64,
}

fn frame_omp_companion_command(
    secret: &[u8; 32],
    command: OmpCompanionCommandType,
    target: &OmpCompanionCommandTargetV1,
    command_sequence: u64,
) -> Vec<u8> {
    let body = serde_json::to_vec(&OmpCompanionCommandWire {
        version: 1,
        command_type: command,
        incarnation: &target.incarnation,
        session_generation: target.session_generation,
        session_id: &target.session_id,
        work_epoch: target.work_epoch,
        command_sequence,
    })
    .expect("closed OMP command wire values always serialize");
    frame_omp_companion_command_body(secret, body)
}

fn frame_omp_companion_snapshot_ack(
    secret: &[u8; 32],
    snapshot: &fresh_core::hooks::OmpCompanionSnapshotV1,
    accepted: bool,
    command_sequence: u64,
) -> Vec<u8> {
    let body = serde_json::to_vec(&OmpCompanionSnapshotAckWire {
        version: 1,
        command_type: "snapshot_ack",
        incarnation: &snapshot.incarnation,
        sequence: snapshot.sequence,
        session_generation: snapshot.session_generation,
        session_id: &snapshot.session_id,
        work_epoch: snapshot.work_epoch,
        accepted,
        command_sequence,
    })
    .expect("closed OMP snapshot acknowledgement values always serialize");
    frame_omp_companion_command_body(secret, body)
}

fn frame_omp_companion_command_body(secret: &[u8; 32], mut body: Vec<u8>) -> Vec<u8> {
    let mut body_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(&body)
        .into_bytes();
    body.zeroize();
    let mut tag_b64 = omp_companion_command_tag(secret, &body_b64);
    let mut frame = Vec::with_capacity(
        OMP_COMMAND_START.len() + body_b64.len() + 1 + tag_b64.len() + OMP_COMMAND_END.len(),
    );
    frame.extend_from_slice(OMP_COMMAND_START);
    frame.extend_from_slice(&body_b64);
    frame.push(b'.');
    frame.extend_from_slice(&tag_b64);
    frame.extend_from_slice(OMP_COMMAND_END);
    body_b64.zeroize();
    tag_b64.zeroize();
    frame
}

#[cfg(test)]
pub(crate) fn test_secret() -> [u8; 32] {
    std::array::from_fn(|index| index as u8)
}

#[cfg(test)]
pub(crate) fn test_companion(secret: [u8; 32]) -> std::sync::Arc<OmpCompanionLiveState> {
    let mut spawn =
        OmpCompanionSpawn::new(TerminalCompanion::Omp, "/opt/omp".to_string(), Vec::new());
    spawn.secret = secret;
    std::sync::Arc::new(OmpCompanionLiveState::new(spawn))
}

#[cfg(test)]
pub(crate) fn test_output_frame(secret: &[u8; 32], body: &[u8]) -> Vec<u8> {
    let body_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body);
    let mut frame = OMP_OUTPUT_PREFIX.to_vec();
    frame.extend_from_slice(&omp_companion_synchronizer(secret));
    frame.push(b';');
    frame.extend_from_slice(body_b64.as_bytes());
    frame.push(b'.');
    frame.extend_from_slice(&omp_companion_output_tag(secret, body_b64.as_bytes()));
    frame.extend_from_slice(OMP_OUTPUT_TERMINATOR);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::{traits::Stream as _, Stream};
    use std::io::Read as _;

    #[test]
    fn launch_channel_delivers_exact_secret_once() {
        let mut spawn =
            OmpCompanionSpawn::new(TerminalCompanion::Omp, "/opt/omp".to_string(), Vec::new());
        spawn.secret = [0x5a; 32];
        let endpoint = spawn.arm_launch_channel().expect("private launch channel");
        let name = std::path::PathBuf::from(endpoint)
            .to_fs_name::<GenericFilePath>()
            .expect("valid local socket name");
        let mut stream = Stream::connect(name).expect("connect to launch channel");
        let mut received = Vec::new();
        stream
            .read_to_end(&mut received)
            .expect("read launch capability");
        assert_eq!(received, spawn.secret);
    }

    fn command_target() -> OmpCompanionCommandTargetV1 {
        OmpCompanionCommandTargetV1 {
            incarnation: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            session_generation: 3,
            session_id: "018f1d74-7f7b-7d31-8d93-9a21c7b95bb1".to_string(),
            work_epoch: 9,
        }
    }

    fn snapshot(
        incarnation: &str,
        sequence: u64,
        session_generation: u64,
        session_id: &str,
        work_epoch: u64,
    ) -> fresh_core::hooks::OmpCompanionSnapshotV1 {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "incarnation": incarnation,
            "sequence": sequence,
            "sessionGeneration": session_generation,
            "workEpoch": work_epoch,
            "timestampMs": 0,
            "ompVersion": "test",
            "processId": 1,
            "sessionId": session_id,
            "cwd": "/tmp",
            "state": "idle",
            "runningTools": 0,
            "pendingApprovals": 0
        }))
        .unwrap()
    }

    fn admit_and_commit(
        companion: &OmpCompanionLiveState,
        incarnation: &str,
        sequence: u64,
        session_generation: u64,
        session_id: &str,
        work_epoch: u64,
        checkpoint_resume: bool,
    ) -> bool {
        let snapshot = snapshot(
            incarnation,
            sequence,
            session_generation,
            session_id,
            work_epoch,
        );
        companion.admit_sequence(
            incarnation,
            sequence,
            session_generation,
            session_id,
            work_epoch,
        ) && companion.commit_admitted_snapshot(&snapshot, checkpoint_resume)
    }

    #[test]
    fn exact_resume_preserves_profile_and_session_scope() {
        let mut spawn = OmpCompanionSpawn::new(
            TerminalCompanion::Omp,
            "/opt/omp".to_string(),
            vec![
                "--profile=work".to_string(),
                "--session-dir".to_string(),
                "/tmp/sessions".to_string(),
            ],
        );
        spawn.secret = [7; 32];
        let companion = OmpCompanionLiveState::new(spawn);
        assert_eq!(
            companion.exact_resume_argv("123e4567-e89b-42d3-a456-426614174099"),
            vec![
                "/opt/omp",
                "--profile=work",
                "--session-dir",
                "/tmp/sessions",
                "--resume",
                "123e4567-e89b-42d3-a456-426614174099",
            ]
        );
    }

    fn output_preamble(secret: &[u8; 32]) -> Vec<u8> {
        let mut preamble = OMP_OUTPUT_PREFIX.to_vec();
        preamble.extend_from_slice(&omp_companion_synchronizer(secret));
        preamble.push(b';');
        preamble
    }

    fn filter_with_split(
        companion: &OmpCompanionLiveState,
        bytes: &[u8],
        split: usize,
    ) -> (Vec<u8>, Vec<usize>, bool) {
        let now = Instant::now();
        let first = companion.filter_output(&bytes[..split], now);
        let second = companion.filter_output(&bytes[split..], now);
        let first_len = first.visible.len();
        let mut visible = first.visible;
        visible.extend(second.visible);
        visible.extend(companion.finish_output());
        let mut boundaries = first.boundaries;
        boundaries.extend(
            second
                .boundaries
                .into_iter()
                .map(|offset| first_len + offset),
        );
        (
            visible,
            boundaries,
            first.notify_candidate || second.notify_candidate,
        )
    }

    #[test]
    fn omp_hmac_domains_and_canonical_command_wire_match_interoperability_contract() {
        let secret = test_secret();
        assert_eq!(
            &omp_companion_synchronizer(&secret),
            b"vjg0jkrY4jwjlVFyghTG5w"
        );
        assert_eq!(
            &omp_companion_output_tag(&secret, b"body"),
            b"eaNCWzMhUp3wPXucyW6D07mEAblq3xADF9HqMrxoW0c"
        );

        let target = command_target();
        let frame =
            frame_omp_companion_command(&secret, OmpCompanionCommandType::Cancel, &target, 1);
        assert!(frame.starts_with(OMP_COMMAND_START));
        assert!(frame.ends_with(OMP_COMMAND_END));
        let payload = &frame[OMP_COMMAND_START.len()..frame.len() - OMP_COMMAND_END.len()];
        let separator = payload.iter().position(|byte| *byte == b'.').unwrap();
        let body_b64 = &payload[..separator];
        assert_eq!(
            &payload[separator + 1..],
            &omp_companion_command_tag(&secret, body_b64)
        );
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body_b64)
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            r#"{"version":1,"type":"cancel","incarnation":"550e8400-e29b-41d4-a716-446655440000","sessionGeneration":3,"sessionId":"018f1d74-7f7b-7d31-8d93-9a21c7b95bb1","workEpoch":9,"commandSequence":1}"#
        );
    }

    #[test]
    fn every_prefix_and_synchronizer_mismatch_replays_across_all_splits() {
        let secret = test_secret();
        let preamble = output_preamble(&secret);
        for mismatch in 0..preamble.len() {
            let mut stream = preamble.clone();
            stream[mismatch] = b'!';
            stream.extend_from_slice(b"ordinary-tail");
            for split in 0..=stream.len() {
                let companion = test_companion(secret);
                let (visible, boundaries, notified) = filter_with_split(&companion, &stream, split);
                assert_eq!(visible, stream, "mismatch={mismatch}, split={split}");
                assert!(boundaries.is_empty(), "mismatch={mismatch}, split={split}");
                assert!(!notified, "mismatch={mismatch}, split={split}");
                assert!(companion.take_candidate().is_none());
            }
        }
    }

    #[test]
    fn mismatch_byte_is_reconsidered_as_adjacent_frame_start() {
        let secret = test_secret();
        let frame = test_output_frame(&secret, br#"{"version":1,"type":"snapshot"}"#);
        let mut stream = OMP_OUTPUT_PREFIX[..8].to_vec();
        stream.extend_from_slice(&frame);
        let companion = test_companion(secret);
        let filtered = companion.filter_output(&stream, Instant::now());
        assert_eq!(filtered.visible, OMP_OUTPUT_PREFIX[..8]);
        assert!(filtered.notify_candidate);
        assert_eq!(companion.take_candidate(), Some(frame));
    }

    #[test]
    fn verified_frame_reassembles_and_stays_private_across_every_split() {
        let secret = test_secret();
        let frame = test_output_frame(&secret, br#"{"version":1,"type":"snapshot"}"#);
        let mut stream = b"before".to_vec();
        stream.extend_from_slice(&frame);
        stream.extend_from_slice(b"after");
        for split in 0..=stream.len() {
            let companion = test_companion(secret);
            let (visible, boundaries, notified) = filter_with_split(&companion, &stream, split);
            assert_eq!(visible, b"beforeafter", "split={split}");
            assert_eq!(boundaries, [6], "split={split}");
            assert!(notified, "split={split}");
            assert_eq!(companion.take_candidate(), Some(frame.clone()));
        }
    }

    #[test]
    fn verified_bad_auth_candidate_is_still_removed_from_public_output() {
        let secret = test_secret();
        let mut frame = test_output_frame(&secret, b"malformed-private");
        let tag_index = frame.len() - OMP_OUTPUT_TERMINATOR.len() - 1;
        frame[tag_index] = if frame[tag_index] == b'A' { b'B' } else { b'A' };
        let companion = test_companion(secret);
        let filtered = companion.filter_output(&frame, Instant::now());
        assert!(filtered.visible.is_empty());
        assert_eq!(filtered.boundaries, [0]);
        assert!(filtered.notify_candidate);
        assert_eq!(companion.take_candidate(), Some(frame));
    }

    #[test]
    fn oversized_candidate_discards_until_exact_terminator() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let mut unterminated = output_preamble(&secret);
        unterminated.resize(OMP_OUTPUT_FRAME_MAX, b'a');
        let now = Instant::now();
        let filtered = companion.filter_output(&unterminated, now);
        assert!(filtered.visible.is_empty());
        assert_eq!(filtered.boundaries, [0]);
        assert!(!filtered.notify_candidate);

        let discarded = vec![b'x'; OMP_OUTPUT_FRAME_MAX * 2];
        assert!(companion.filter_output(&discarded, now).visible.is_empty());
        assert!(companion
            .filter_output(b"private\x1bXstill-private", now)
            .visible
            .is_empty());
        assert_eq!(
            companion
                .filter_output(b"\x1b\\ordinary-after-terminator", now)
                .visible,
            b"ordinary-after-terminator"
        );
        assert!(companion.take_candidate().is_none());
    }

    #[test]
    fn complete_frame_at_exact_cap_is_accepted() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let mut frame = output_preamble(&secret);
        frame.resize(OMP_OUTPUT_FRAME_MAX - OMP_OUTPUT_TERMINATOR.len(), b'a');
        frame.extend_from_slice(OMP_OUTPUT_TERMINATOR);
        assert_eq!(frame.len(), OMP_OUTPUT_FRAME_MAX);

        let filtered = companion.filter_output(&frame, Instant::now());
        assert!(filtered.visible.is_empty());
        assert_eq!(filtered.boundaries, [0]);
        assert!(filtered.notify_candidate);
        assert_eq!(companion.take_candidate(), Some(frame));
    }

    #[test]
    fn delayed_split_frame_tail_stays_private_until_terminator() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let frame = test_output_frame(&secret, b"delayed-private-tail");
        let split = output_preamble(&secret).len() + 4;
        let started = Instant::now();
        let first = companion.filter_output(&frame[..split], started);
        assert!(first.visible.is_empty());
        assert_eq!(first.boundaries, [0]);

        let mut delayed_tail = frame[split..].to_vec();
        delayed_tail.extend_from_slice(b"ordinary-after-frame");
        let late = companion.filter_output(&delayed_tail, started + OMP_OUTPUT_FRAME_LIFETIME);
        assert_eq!(late.visible, b"ordinary-after-frame");
        assert!(late.boundaries.is_empty());
        assert!(!late.notify_candidate);
        assert!(companion.take_candidate().is_none());
    }

    #[test]
    fn eof_discards_every_partial_private_candidate() {
        let secret = test_secret();
        let unverified = test_companion(secret);
        let preamble = output_preamble(&secret);
        let partial_prefix = &preamble[..OMP_OUTPUT_PREFIX.len() + 10];
        assert!(unverified
            .filter_output(partial_prefix, Instant::now())
            .visible
            .is_empty());
        assert!(unverified.finish_output().is_empty());

        let verified = test_companion(secret);
        let mut partial_candidate = output_preamble(&secret);
        partial_candidate.extend_from_slice(b"private-without-terminator");
        let partial = verified.filter_output(&partial_candidate, Instant::now());
        assert!(partial.visible.is_empty());
        assert_eq!(partial.boundaries, [0]);
        assert!(verified.finish_output().is_empty());
        assert!(verified.take_candidate().is_none());
    }

    #[test]
    fn candidate_queue_preserves_receipt_order_with_one_outstanding_notification() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let first = test_output_frame(&secret, b"first");
        let second = test_output_frame(&secret, b"second");
        let third = test_output_frame(&secret, b"third");
        let fourth = test_output_frame(&secret, b"fourth");

        let mut burst = first.clone();
        burst.extend_from_slice(&second);
        let filtered = companion.filter_output(&burst, Instant::now());
        assert!(filtered.visible.is_empty());
        assert_eq!(filtered.boundaries, [0, 0]);
        assert!(filtered.notify_candidate);
        assert!(
            !companion
                .filter_output(&third, Instant::now())
                .notify_candidate
        );
        assert_eq!(companion.take_candidates(), vec![first, second, third]);
        assert!(
            companion
                .filter_output(&fourth, Instant::now())
                .notify_candidate
        );
        assert_eq!(companion.take_candidates(), vec![fourth]);
    }

    #[test]
    fn candidate_queue_overflow_revokes_capability_and_purges_fifo() {
        let secret = test_secret();
        let companion = test_companion(secret);
        for index in 0..OMP_PENDING_CANDIDATES_MAX {
            assert_eq!(companion.enqueue_candidate(vec![index as u8]), index == 0);
        }
        assert!(!companion.enqueue_candidate(vec![255]));
        assert!(companion.take_candidates().is_empty());
        assert!(!companion.verify_output_auth(
            &omp_companion_synchronizer(&secret),
            b"body",
            &omp_companion_output_tag(&secret, b"body")
        ));
        assert!(!companion.enqueue_candidate(vec![42]));
        assert!(companion.take_candidate().is_none());
    }

    #[test]
    fn sequence_state_retires_old_incarnations_and_fences_live_work_identity() {
        let companion = test_companion(test_secret());
        assert!(admit_and_commit(
            &companion,
            "incarnation-a",
            10,
            5,
            "session-a",
            2,
            false,
        ));
        assert!(!companion.admit_sequence("incarnation-a", 10, 5, "session-a", 2));
        assert!(!companion.admit_sequence("incarnation-a", 11, 4, "session-a", 2));
        assert!(!companion.admit_sequence("incarnation-a", 11, 5, "session-b", 2));
        assert!(!companion.admit_sequence("incarnation-a", 11, 5, "session-a", 1));
        assert!(admit_and_commit(
            &companion,
            "incarnation-a",
            11,
            5,
            "session-a",
            3,
            false,
        ));
        assert!(!companion.admit_sequence("incarnation-b", 1, 4, "session-b", 1));
        assert!(admit_and_commit(
            &companion,
            "incarnation-b",
            1,
            5,
            "session-b",
            1,
            false,
        ));
        assert!(!companion.admit_sequence("incarnation-a", 12, 6, "session-a", 4));
        assert!(admit_and_commit(
            &companion,
            "incarnation-b",
            2,
            6,
            "session-c",
            1,
            false,
        ));
        assert!(!companion.admit_sequence("incarnation-b", 3, 5, "session-b", 2));
    }

    #[test]
    fn rejected_transition_allows_retained_session_at_same_generation() {
        let companion = test_companion(test_secret());
        let alive = AtomicBool::new(true);
        let authoritative_incarnation = "incarnation-a";
        let transition_incarnation = "incarnation-b";
        let retained_session = "session-a";
        assert!(admit_and_commit(
            &companion,
            authoritative_incarnation,
            1,
            1,
            retained_session,
            1,
            true,
        ));

        let rejected = snapshot(transition_incarnation, 1, 2, "session-b", 1);
        assert!(companion.admit_sequence(transition_incarnation, 1, 2, "session-b", 1));
        assert!(companion
            .frame_snapshot_ack_if_current(&rejected, false, &alive)
            .is_some());
        let prior_target = OmpCompanionCommandTargetV1 {
            incarnation: authoritative_incarnation.to_string(),
            session_generation: 1,
            session_id: retained_session.to_string(),
            work_epoch: 1,
        };
        assert!(companion
            .frame_command_if_active(
                OmpCompanionCommandType::RequestSnapshot,
                &prior_target,
                &alive,
            )
            .is_some());
        assert!(!companion.admit_sequence(transition_incarnation, 1, 2, "session-b", 1));

        let retained = snapshot(transition_incarnation, 2, 2, retained_session, 1);
        assert!(companion.admit_sequence(transition_incarnation, 2, 2, retained_session, 1));
        assert!(companion
            .frame_snapshot_ack_if_current(&retained, true, &alive)
            .is_some());
        assert!(companion
            .frame_command_if_active(
                OmpCompanionCommandType::RequestSnapshot,
                &prior_target,
                &alive,
            )
            .is_some());
        assert!(companion.commit_admitted_snapshot(&retained, false));
        assert!(!companion.admit_sequence(transition_incarnation, 1, 2, "session-b", 1));
        assert!(companion
            .frame_command_if_active(
                OmpCompanionCommandType::RequestSnapshot,
                &OmpCompanionCommandTargetV1 {
                    incarnation: transition_incarnation.to_string(),
                    session_generation: 2,
                    session_id: retained_session.to_string(),
                    work_epoch: 1,
                },
                &alive,
            )
            .is_some());
    }

    #[test]
    fn retired_incarnation_history_is_bounded_and_refuses_unseen_overflow() {
        let companion = test_companion(test_secret());
        assert!(admit_and_commit(
            &companion,
            "incarnation-0",
            1,
            1,
            "session",
            1,
            false,
        ));
        for index in 1..=OMP_RETIRED_INCARNATIONS_MAX {
            assert!(admit_and_commit(
                &companion,
                &format!("incarnation-{index}"),
                1,
                index as u64 + 1,
                "session",
                1,
                false,
            ));
        }
        assert!(!companion.admit_sequence(
            "incarnation-overflow",
            1,
            OMP_RETIRED_INCARNATIONS_MAX as u64 + 2,
            "session",
            1,
        ));
        assert!(admit_and_commit(
            &companion,
            &format!("incarnation-{}", OMP_RETIRED_INCARNATIONS_MAX),
            2,
            OMP_RETIRED_INCARNATIONS_MAX as u64 + 1,
            "session",
            1,
            false,
        ));
        let retired_count = match companion.inner.lock() {
            Ok(inner) => inner.retired_incarnations.len(),
            Err(poisoned) => poisoned.into_inner().retired_incarnations.len(),
        };
        assert_eq!(retired_count, OMP_RETIRED_INCARNATIONS_MAX);
    }

    #[test]
    fn command_revocation_preserves_completed_candidates_and_verifier_until_full_purge() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let target = command_target();
        assert!(admit_and_commit(
            &companion,
            &target.incarnation,
            1,
            target.session_generation,
            &target.session_id,
            target.work_epoch,
            true,
        ));
        assert!(companion.resume_checkpointed_for(&target.session_id));
        assert!(companion.enqueue_candidate(vec![1, 2, 3]));

        companion.revoke_commands();
        assert!(companion.resume_checkpointed_for(&target.session_id));
        assert!(companion
            .frame_command_if_active(
                OmpCompanionCommandType::RequestSnapshot,
                &target,
                &AtomicBool::new(true),
            )
            .is_none());
        assert_eq!(companion.take_candidate(), Some(vec![1, 2, 3]));
        assert!(companion.verify_output_auth(
            &omp_companion_synchronizer(&secret),
            b"body",
            &omp_companion_output_tag(&secret, b"body")
        ));
        assert!(companion.admit_sequence(
            &target.incarnation,
            2,
            target.session_generation,
            &target.session_id,
            target.work_epoch,
        ));

        companion.revoke_access();
        assert!(!companion.resume_checkpointed_for(&target.session_id));
        assert!(!companion.verify_output_auth(
            &omp_companion_synchronizer(&secret),
            b"body",
            &omp_companion_output_tag(&secret, b"body")
        ));
    }

    #[test]
    fn immediate_revocation_keeps_split_private_frame_filtered_until_finalization() {
        let secret = test_secret();
        let sync_b64 = omp_companion_synchronizer(&secret);
        let companion = test_companion(secret);
        let frame = test_output_frame(&secret, b"split-private-candidate");
        let split = output_preamble(&secret).len() + 4;
        let mut first = b"ordinary-before".to_vec();
        first.extend_from_slice(&frame[..split]);

        assert!(companion.enqueue_candidate(vec![1, 2, 3]));
        assert!(companion.admit_sequence("incarnation", 4, 1, "session", 1));
        let filtered = companion.filter_output(&first, Instant::now());
        assert_eq!(filtered.visible, b"ordinary-before");
        assert_eq!(filtered.boundaries, [15]);
        assert!(!filtered.notify_candidate);

        companion.revoke_access();
        {
            let inner = match companion.inner.lock() {
                Ok(inner) => inner,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert!(inner.access_revoked);
            assert!(inner.command_access_revoked);
            assert!(!inner.output_filter_finalized);
            assert_eq!(inner.secret, [0; 32]);
            assert_eq!(inner.sync_b64, sync_b64);
            assert!(inner.scanner.probe.is_empty());
            assert!(inner.scanner.quarantine.is_some());
            assert!(inner.candidates.is_empty());
            assert!(!inner.notification_pending);
            assert!(inner.sequence_incarnation.is_none());
            assert!(inner.current_incarnation.is_none());
            assert!(inner.retired_incarnations.is_empty());
            assert_eq!(inner.last_sequence, 0);
            assert_eq!(inner.last_session_generation, 0);
            assert!(inner.current_session_id.is_none());
            assert_eq!(inner.current_work_epoch, 0);
            assert!(inner.pending_snapshot.is_none());
            assert_eq!(inner.last_command_sequence, 0);
        }
        assert!(!companion.enqueue_candidate(vec![4]));
        assert!(companion.take_candidate().is_none());
        assert!(!companion.verify_output_auth(
            &sync_b64,
            b"body",
            &omp_companion_output_tag(&secret, b"body")
        ));
        assert!(!companion.admit_sequence("incarnation", 5, 1, "session", 1));
        assert!(companion
            .frame_command_if_active(
                OmpCompanionCommandType::RequestSnapshot,
                &command_target(),
                &AtomicBool::new(true),
            )
            .is_none());

        let mut remainder = frame[split..].to_vec();
        remainder.extend_from_slice(b"ordinary-after");
        let filtered = companion.filter_output(&remainder, Instant::now());
        assert_eq!(filtered.visible, b"ordinary-after");
        assert!(filtered.boundaries.is_empty());
        assert!(!filtered.notify_candidate);
        assert!(companion.take_candidate().is_none());
        assert!(companion.finish_output().is_empty());

        {
            let inner = match companion.inner.lock() {
                Ok(inner) => inner,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert_eq!(inner.sync_b64, sync_b64);
            assert!(!inner.output_filter_finalized);
        }
        companion.finalize_output_filter();
        assert!(companion.test_output_filter_finalized());
    }

    #[test]
    fn complete_frame_queued_after_revocation_stays_private_and_preserves_public_bytes() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let frame = test_output_frame(&secret, b"queued-private-candidate");
        let mut queued = b"left".to_vec();
        queued.extend_from_slice(&frame);
        queued.extend_from_slice(b"right");

        companion.revoke_access();
        let filtered = companion.filter_output(&queued, Instant::now());
        assert_eq!(filtered.visible, b"leftright");
        assert_eq!(filtered.boundaries, [4]);
        assert!(!filtered.notify_candidate);
        assert!(companion.take_candidate().is_none());
        assert!(!companion.test_output_filter_finalized());
    }
}
