//! Private OMP companion transport for terminal sessions.
//!
//! This module owns the authenticated framing capability that is shared by a
//! Fresh-owned OMP child, its PTY reader, and the editor-side parser.

use base64::Engine;
use fresh_core::api::{OmpCompanionCommandType, TerminalCompanion};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
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
const OMP_OUTPUT_INACTIVITY: Duration = Duration::from_secs(1);
const OMP_COMMAND_START: &[u8] = "\u{10ffff}fresh-omp-command:v1:".as_bytes();
const OMP_COMMAND_END: &[u8] = "\u{10fffe}".as_bytes();
const OMP_CANCEL_COMMAND_BODY_B64: &[u8] = b"eyJ2ZXJzaW9uIjoxLCJ0eXBlIjoiY2FuY2VsIn0";
const OMP_REQUEST_SNAPSHOT_COMMAND_BODY_B64: &[u8] =
    b"eyJ2ZXJzaW9uIjoxLCJ0eXBlIjoicmVxdWVzdF9zbmFwc2hvdCJ9";

/// Spawn-only capability for a Fresh-owned OMP companion terminal.
///
/// The creator injects the same secret into the direct OMP child's environment.
/// Construction copies the secret only into [`OmpCompanionLiveState`], then
/// drops and zeroes the spawn source before returning.
pub(crate) struct OmpCompanionSpawn {
    pub kind: TerminalCompanion,
    pub secret: [u8; 32],
}

impl Drop for OmpCompanionSpawn {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

struct OmpQuarantinedCandidate {
    frame: Vec<u8>,
    last_byte_at: Instant,
}

struct OmpOutputScanner {
    probe: Vec<u8>,
    quarantine: Option<OmpQuarantinedCandidate>,
}

impl OmpOutputScanner {
    fn new() -> Self {
        Self {
            probe: Vec::with_capacity(OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 1),
            quarantine: None,
        }
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

    fn expire_inactive(&mut self, now: Instant) {
        if self.quarantine.as_ref().is_some_and(|candidate| {
            now.saturating_duration_since(candidate.last_byte_at) >= OMP_OUTPUT_INACTIVITY
        }) {
            if let Some(mut candidate) = self.quarantine.take() {
                candidate.frame.zeroize();
            }
        }
    }

    fn feed(
        &mut self,
        bytes: &[u8],
        now: Instant,
        sync_b64: &[u8; OMP_SYNC_B64_LEN],
        visible: &mut Vec<u8>,
        completed: &mut Option<Vec<u8>>,
    ) {
        self.expire_inactive(now);
        if self.probe.is_empty()
            && self.quarantine.is_none()
            && !bytes.contains(&OMP_OUTPUT_PREFIX[0])
        {
            visible.extend_from_slice(bytes);
            return;
        }
        for &byte in bytes {
            self.feed_byte(byte, now, sync_b64, visible, completed);
        }
    }

    fn feed_byte(
        &mut self,
        byte: u8,
        now: Instant,
        sync_b64: &[u8; OMP_SYNC_B64_LEN],
        visible: &mut Vec<u8>,
        completed: &mut Option<Vec<u8>>,
    ) {
        loop {
            if let Some(mut candidate) = self.quarantine.take() {
                if candidate.frame.len() >= OMP_OUTPUT_FRAME_MAX {
                    // Defensive only: a full non-terminated candidate is reset
                    // immediately below, before another byte is considered.
                    candidate.frame.zeroize();
                    continue;
                }
                candidate.frame.push(byte);
                candidate.last_byte_at = now;
                if candidate.frame.ends_with(OMP_OUTPUT_TERMINATOR) {
                    *completed = Some(candidate.frame);
                } else if candidate.frame.len() < OMP_OUTPUT_FRAME_MAX {
                    self.quarantine = Some(candidate);
                } else {
                    candidate.frame.zeroize();
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
                if self.probe.len() == OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 1 {
                    self.quarantine = Some(OmpQuarantinedCandidate {
                        frame: std::mem::take(&mut self.probe),
                        last_byte_at: now,
                    });
                    self.probe = Vec::with_capacity(OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 1);
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
        if let Some(mut candidate) = self.quarantine.take() {
            candidate.frame.zeroize();
        }
        std::mem::take(&mut self.probe)
    }

    fn clear(&mut self) {
        self.probe.zeroize();
        if let Some(mut candidate) = self.quarantine.take() {
            candidate.frame.zeroize();
        }
    }
}

struct OmpCompanionStateInner {
    secret: [u8; 32],
    sync_b64: [u8; OMP_SYNC_B64_LEN],
    access_revoked: bool,
    output_filter_finalized: bool,
    scanner: OmpOutputScanner,
    last_incarnation: Option<String>,
    last_sequence: u64,
    candidate: Option<Vec<u8>>,
    notification_pending: bool,
}

/// Ephemeral capability shared by the terminal handle and its PTY reader.
pub(crate) struct OmpCompanionLiveState {
    kind: TerminalCompanion,
    inner: Mutex<OmpCompanionStateInner>,
}

impl OmpCompanionLiveState {
    pub(crate) fn new(spawn: OmpCompanionSpawn) -> Self {
        let mut sync_b64 = omp_companion_synchronizer(&spawn.secret);
        let state = Self {
            kind: spawn.kind,
            inner: Mutex::new(OmpCompanionStateInner {
                secret: spawn.secret,
                sync_b64,
                access_revoked: false,
                output_filter_finalized: false,
                scanner: OmpOutputScanner::new(),
                last_incarnation: None,
                last_sequence: 0,
                candidate: None,
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

    pub(crate) fn replace_candidate(&self, mut candidate: Vec<u8>) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            candidate.zeroize();
            return false;
        };
        if inner.access_revoked || candidate.len() > OMP_OUTPUT_FRAME_MAX {
            candidate.zeroize();
            return false;
        }
        if let Some(mut previous) = inner.candidate.replace(candidate) {
            previous.zeroize();
        }
        if inner.notification_pending {
            false
        } else {
            inner.notification_pending = true;
            true
        }
    }

    pub(crate) fn take_candidate(&self) -> Option<Vec<u8>> {
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        if inner.access_revoked {
            return None;
        }
        inner.notification_pending = false;
        inner.candidate.take()
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

    pub(crate) fn accept_sequence(&self, incarnation: &str, sequence: u64) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner.access_revoked {
            return false;
        }
        if inner.last_incarnation.as_deref() == Some(incarnation) {
            if sequence <= inner.last_sequence {
                return false;
            }
        } else {
            inner.last_incarnation = Some(incarnation.to_owned());
        }
        inner.last_sequence = sequence;
        true
    }

    pub(crate) fn filter_output_into(
        &self,
        bytes: &[u8],
        now: Instant,
        visible: &mut Vec<u8>,
    ) -> bool {
        visible.clear();
        let completed = {
            let Ok(mut inner) = self.inner.lock() else {
                visible.extend_from_slice(bytes);
                return false;
            };
            if inner.output_filter_finalized {
                visible.extend_from_slice(bytes);
                return false;
            }

            let mut completed = None;
            let OmpCompanionStateInner {
                scanner, sync_b64, ..
            } = &mut *inner;
            scanner.feed(bytes, now, sync_b64, visible, &mut completed);
            completed
        };

        completed.is_some_and(|candidate| self.replace_candidate(candidate))
    }

    #[cfg(test)]
    fn filter_output(&self, bytes: &[u8], now: Instant) -> OmpFilteredOutput {
        let mut visible = Vec::with_capacity(bytes.len());
        let notify_candidate = self.filter_output_into(bytes, now, &mut visible);
        OmpFilteredOutput {
            visible,
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
        alive: &AtomicBool,
    ) -> Option<Vec<u8>> {
        let Ok(inner) = self.inner.lock() else {
            return None;
        };
        if inner.access_revoked || !alive.load(Ordering::Acquire) {
            return None;
        }
        Some(frame_omp_companion_command(&inner.secret, command))
    }

    /// Immediately revoke commands, authentication, and candidate delivery.
    ///
    /// The synchronizer and scanner deliberately remain live so the PTY reader
    /// can continue removing private frames already read or queued before EOF.
    pub(crate) fn revoke_access(&self) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        Self::revoke_access_inner(&mut inner);
    }

    /// Clear the output filter after the PTY reader has drained and flushed.
    pub(crate) fn finalize_output_filter(&self) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        Self::revoke_access_inner(&mut inner);
        inner.sync_b64.zeroize();
        inner.scanner.clear();
        inner.output_filter_finalized = true;
    }

    fn revoke_access_inner(inner: &mut OmpCompanionStateInner) {
        inner.access_revoked = true;
        inner.secret.zeroize();
        inner.last_incarnation.zeroize();
        inner.last_sequence = 0;
        inner.candidate.zeroize();
        inner.notification_pending = false;
    }

    #[cfg(test)]
    pub(crate) fn test_output_filter_finalized(&self) -> bool {
        let inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.output_filter_finalized
            && inner.sync_b64 == [0; OMP_SYNC_B64_LEN]
            && inner.scanner.probe.is_empty()
            && inner.scanner.quarantine.is_none()
    }
}

impl Drop for OmpCompanionLiveState {
    fn drop(&mut self) {
        let inner = match self.inner.get_mut() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.secret.zeroize();
        inner.sync_b64.zeroize();
        inner.scanner.clear();
        inner.candidate.zeroize();
        inner.last_incarnation.zeroize();
        inner.last_sequence = 0;
        inner.notification_pending = false;
        inner.access_revoked = true;
        inner.output_filter_finalized = true;
    }
}

#[cfg(test)]
struct OmpFilteredOutput {
    visible: Vec<u8>,
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

fn frame_omp_companion_command(secret: &[u8; 32], command: OmpCompanionCommandType) -> Vec<u8> {
    let body_b64 = match command {
        OmpCompanionCommandType::Cancel => OMP_CANCEL_COMMAND_BODY_B64,
        OmpCompanionCommandType::RequestSnapshot => OMP_REQUEST_SNAPSHOT_COMMAND_BODY_B64,
    };
    let mut tag_b64 = omp_companion_command_tag(secret, body_b64);
    let mut frame = Vec::with_capacity(
        OMP_COMMAND_START.len() + body_b64.len() + 1 + tag_b64.len() + OMP_COMMAND_END.len(),
    );
    frame.extend_from_slice(OMP_COMMAND_START);
    frame.extend_from_slice(body_b64);
    frame.push(b'.');
    frame.extend_from_slice(&tag_b64);
    frame.extend_from_slice(OMP_COMMAND_END);
    tag_b64.zeroize();
    frame
}

#[cfg(test)]
pub(crate) fn test_secret() -> [u8; 32] {
    std::array::from_fn(|index| index as u8)
}

#[cfg(test)]
pub(crate) fn test_companion(secret: [u8; 32]) -> std::sync::Arc<OmpCompanionLiveState> {
    std::sync::Arc::new(OmpCompanionLiveState::new(OmpCompanionSpawn {
        kind: TerminalCompanion::Omp,
        secret,
    }))
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
    ) -> (Vec<u8>, bool) {
        let now = Instant::now();
        let first = companion.filter_output(&bytes[..split], now);
        let second = companion.filter_output(&bytes[split..], now);
        let mut visible = first.visible;
        visible.extend(second.visible);
        visible.extend(companion.finish_output());
        (visible, first.notify_candidate || second.notify_candidate)
    }

    #[test]
    fn omp_hmac_domains_and_canonical_base64url_match_interoperability_vectors() {
        let secret = test_secret();
        assert_eq!(
            OMP_CANCEL_COMMAND_BODY_B64,
            b"eyJ2ZXJzaW9uIjoxLCJ0eXBlIjoiY2FuY2VsIn0"
        );
        assert_eq!(
            OMP_REQUEST_SNAPSHOT_COMMAND_BODY_B64,
            b"eyJ2ZXJzaW9uIjoxLCJ0eXBlIjoicmVxdWVzdF9zbmFwc2hvdCJ9"
        );
        assert_eq!(
            &omp_companion_synchronizer(&secret),
            b"vjg0jkrY4jwjlVFyghTG5w"
        );
        assert_eq!(
            &omp_companion_output_tag(&secret, OMP_CANCEL_COMMAND_BODY_B64),
            b"X4WhMmECB-VzIMO-C4ubbhOVbfrkcAah_eFeNGldT3o"
        );
        assert_eq!(
            &omp_companion_command_tag(&secret, OMP_CANCEL_COMMAND_BODY_B64),
            b"pnoZtZh1IvlXyDP3ukIHnEDdSb4vjqePd89L2dcBIWQ"
        );
        assert_eq!(
            &omp_companion_command_tag(&secret, OMP_REQUEST_SNAPSHOT_COMMAND_BODY_B64),
            b"LiAuOJDXE6F2A08ztgCwsivWo1fN5sHRycUrubmt5yA"
        );
    }

    #[test]
    fn command_frames_are_exact_frozen_wire_literals() {
        let secret = test_secret();
        assert_eq!(
            frame_omp_companion_command(&secret, OmpCompanionCommandType::Cancel),
            b"\xf4\x8f\xbf\xbffresh-omp-command:v1:eyJ2ZXJzaW9uIjoxLCJ0eXBlIjoiY2FuY2VsIn0.pnoZtZh1IvlXyDP3ukIHnEDdSb4vjqePd89L2dcBIWQ\xf4\x8f\xbf\xbe"
        );
        assert_eq!(
            frame_omp_companion_command(&secret, OmpCompanionCommandType::RequestSnapshot),
            b"\xf4\x8f\xbf\xbffresh-omp-command:v1:eyJ2ZXJzaW9uIjoxLCJ0eXBlIjoicmVxdWVzdF9zbmFwc2hvdCJ9.LiAuOJDXE6F2A08ztgCwsivWo1fN5sHRycUrubmt5yA\xf4\x8f\xbf\xbe"
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
                let (visible, notified) = filter_with_split(&companion, &stream, split);
                assert_eq!(visible, stream, "mismatch={mismatch}, split={split}");
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
            let (visible, notified) = filter_with_split(&companion, &stream, split);
            assert_eq!(visible, b"beforeafter", "split={split}");
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
        assert!(filtered.notify_candidate);
        assert_eq!(companion.take_candidate(), Some(frame));
    }

    #[test]
    fn quarantine_cap_discards_prefix_and_passes_first_later_byte() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let mut unterminated = output_preamble(&secret);
        unterminated.resize(OMP_OUTPUT_FRAME_MAX, b'a');
        let now = Instant::now();
        let filtered = companion.filter_output(&unterminated, now);
        assert!(filtered.visible.is_empty());
        assert!(!filtered.notify_candidate);
        let scanner_reset = match companion.inner.lock() {
            Ok(inner) => inner.scanner.probe.is_empty() && inner.scanner.quarantine.is_none(),
            Err(poisoned) => {
                let inner = poisoned.into_inner();
                inner.scanner.probe.is_empty() && inner.scanner.quarantine.is_none()
            }
        };
        assert!(scanner_reset);

        let late = companion.filter_output(b"Z\x1b\\", now);
        assert_eq!(late.visible, b"Z\x1b\\");
        assert!(!late.notify_candidate);
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
        assert!(filtered.notify_candidate);
        assert_eq!(companion.take_candidate(), Some(frame));
    }

    #[test]
    fn quarantine_inactivity_discards_old_bytes_and_passes_late_tail() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let mut partial = output_preamble(&secret);
        partial.extend_from_slice(b"unterminated-private-body");
        let started = Instant::now();
        assert!(companion
            .filter_output(&partial, started)
            .visible
            .is_empty());

        let late = companion.filter_output(b"late\x1b\\", started + OMP_OUTPUT_INACTIVITY);
        assert_eq!(late.visible, b"late\x1b\\");
        assert!(!late.notify_candidate);
        assert!(companion.take_candidate().is_none());
    }

    #[test]
    fn eof_replays_unverified_probe_but_discards_verified_candidate() {
        let secret = test_secret();
        let unverified = test_companion(secret);
        let preamble = output_preamble(&secret);
        let partial_prefix = &preamble[..OMP_OUTPUT_PREFIX.len() + 10];
        assert!(unverified
            .filter_output(partial_prefix, Instant::now())
            .visible
            .is_empty());
        assert_eq!(unverified.finish_output(), partial_prefix);

        let verified = test_companion(secret);
        let mut partial_candidate = output_preamble(&secret);
        partial_candidate.extend_from_slice(b"private-without-terminator");
        assert!(verified
            .filter_output(&partial_candidate, Instant::now())
            .visible
            .is_empty());
        assert!(verified.finish_output().is_empty());
        assert!(verified.take_candidate().is_none());
    }

    #[test]
    fn candidate_slot_is_latest_only_with_one_outstanding_notification() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let first = test_output_frame(&secret, b"first");
        let second = test_output_frame(&secret, b"second");
        let third = test_output_frame(&secret, b"third");
        let fourth = test_output_frame(&secret, b"fourth");

        let mut burst = first;
        burst.extend_from_slice(&second);
        let filtered = companion.filter_output(&burst, Instant::now());
        assert!(filtered.visible.is_empty());
        assert!(filtered.notify_candidate);
        assert!(
            !companion
                .filter_output(&third, Instant::now())
                .notify_candidate
        );
        assert_eq!(companion.take_candidate(), Some(third));
        assert!(
            companion
                .filter_output(&fourth, Instant::now())
                .notify_candidate
        );
        assert_eq!(companion.take_candidate(), Some(fourth));
    }

    #[test]
    fn sequence_state_rejects_stale_values_only_within_one_incarnation() {
        let companion = test_companion(test_secret());
        assert!(companion.accept_sequence("incarnation-a", 10));
        assert!(!companion.accept_sequence("incarnation-a", 10));
        assert!(!companion.accept_sequence("incarnation-a", 9));
        assert!(companion.accept_sequence("incarnation-a", 11));
        assert!(companion.accept_sequence("incarnation-b", 1));
        assert!(!companion.accept_sequence("incarnation-b", 1));
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

        assert!(companion.replace_candidate(vec![1, 2, 3]));
        assert!(companion.accept_sequence("incarnation", 4));
        let filtered = companion.filter_output(&first, Instant::now());
        assert_eq!(filtered.visible, b"ordinary-before");
        assert!(!filtered.notify_candidate);

        companion.revoke_access();
        {
            let inner = match companion.inner.lock() {
                Ok(inner) => inner,
                Err(poisoned) => poisoned.into_inner(),
            };
            assert!(inner.access_revoked);
            assert!(!inner.output_filter_finalized);
            assert_eq!(inner.secret, [0; 32]);
            assert_eq!(inner.sync_b64, sync_b64);
            assert!(inner.scanner.probe.is_empty());
            assert!(inner.scanner.quarantine.is_some());
            assert!(inner.candidate.is_none());
            assert!(!inner.notification_pending);
            assert!(inner.last_incarnation.is_none());
            assert_eq!(inner.last_sequence, 0);
        }
        assert!(!companion.replace_candidate(vec![4]));
        assert!(companion.take_candidate().is_none());
        assert!(!companion.verify_output_auth(
            &sync_b64,
            b"body",
            &omp_companion_output_tag(&secret, b"body")
        ));
        assert!(!companion.accept_sequence("incarnation", 5));
        assert!(companion
            .frame_command_if_active(
                OmpCompanionCommandType::RequestSnapshot,
                &AtomicBool::new(true),
            )
            .is_none());

        let mut remainder = frame[split..].to_vec();
        remainder.extend_from_slice(b"ordinary-after");
        let filtered = companion.filter_output(&remainder, Instant::now());
        assert_eq!(filtered.visible, b"ordinary-after");
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
        assert!(!filtered.notify_candidate);
        assert!(companion.take_candidate().is_none());
        assert!(!companion.test_output_filter_finalized());
    }
}
