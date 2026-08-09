//! Per-window process-group tracking + signalling.
//!
//! Each `Window` owns a [`ProcessGroups`] that records the leader
//! pid of every OS process group the window has spawned (today:
//! pty children from `terminal_manager.spawn`; later: long-running
//! tool agents, language servers spawned by the window's
//! authority, …). The window's authority provides a concrete
//! [`Signaller`] implementation, so "stop everything this window
//! owns" is a single `process_groups.signal_all("SIGTERM")` call
//! regardless of whether the spawns happened locally, inside a
//! container, or on a remote host.
//!
//! ## Why per-window?
//!
//! The Orchestrator lifecycle wants to terminate every process
//! belonging to one workspace (a `Window`) without touching the others.
//! Routing through the window keeps that aggregation in one
//! place — callers don't need to know how many terminals the
//! window has or whether a future feature added another kind of
//! background process; they just say "signal this window".
//!
//! ## Authority pluggability
//!
//! The [`Signaller`] trait is the seam between "I know who I
//! want to signal" and "I know how to deliver that signal in
//! this authority's namespace". Local pty processes are reached
//! by `kill(-pid, …)` on the host kernel. Container / SSH
//! authorities will plug in their own implementations that
//! forward through `docker exec kill -PGRP …` or an SSH
//! channel — see the design doc in
//! `docs/internal/orchestrator-open-dialog-and-lifecycle.md` for
//! how that fits into the broader lifecycle.

use std::sync::Arc;

/// Authority-pluggable mechanism for sending OS signals to a
/// process group whose leader pid is known. Concrete impls live
/// per-authority — see [`LocalSignaller`] for the host shell
/// case; container / SSH variants are tracked as future work.
pub trait Signaller: Send + Sync + std::fmt::Debug {
    /// Send `signal_name` ("SIGTERM" / "SIGKILL" / "SIGINT" /
    /// "SIGHUP") to the process group led by `leader_pid`.
    ///
    /// Returns `Ok(true)` when the signal was delivered to a
    /// live group; `Ok(false)` when the group has already exited
    /// (idempotent no-op so retry loops are safe); and `Err`
    /// for permission / lookup / authority-specific failures.
    fn signal(&self, leader_pid: u32, signal_name: &str) -> Result<bool, String>;
}

/// Local-process signaller. The pty puts every spawned shell at
/// the head of its own session, so `kill(-pid, sig)` targets the
/// shell and every subprocess it forked.
#[derive(Debug, Default)]
pub struct LocalSignaller;

impl Signaller for LocalSignaller {
    #[cfg(unix)]
    fn signal(&self, leader_pid: u32, signal_name: &str) -> Result<bool, String> {
        let sig = match signal_name {
            "SIGTERM" => libc::SIGTERM,
            "SIGKILL" => libc::SIGKILL,
            "SIGINT" => libc::SIGINT,
            "SIGHUP" => libc::SIGHUP,
            other => return Err(format!("unsupported signal: {}", other)),
        };
        // `kill(-pid, sig)` (note the negation) sends `sig` to the
        // process group whose leader is `pid`.
        let rc = unsafe { libc::kill(-(leader_pid as i32), sig) };
        if rc == 0 {
            Ok(true)
        } else {
            let err = std::io::Error::last_os_error();
            // ESRCH = no such process / group. Treat as
            // "nothing to signal" so the caller's stop flow
            // stays idempotent.
            if err.raw_os_error() == Some(libc::ESRCH) {
                Ok(false)
            } else {
                Err(format!("kill(-{}, {}): {}", leader_pid, signal_name, err))
            }
        }
    }

    #[cfg(windows)]
    fn signal(&self, _leader_pid: u32, signal_name: &str) -> Result<bool, String> {
        // Windows has no direct pgrp signaling. Callers wanting
        // a hard kill route through `TerminalManager::close`
        // (which uses the pty child killer).
        Err(format!(
            "Windows LocalSignaller cannot deliver {} — use TerminalManager::close()",
            signal_name
        ))
    }
}

/// One entry in a window's tracked process groups. The `label`
/// is a human-readable hint shown in error messages and the
/// Orchestrator preview pane (e.g. "terminal #3", "lsp:rust").
#[derive(Debug, Clone)]
pub struct ProcessGroupEntry {
    pub leader_pid: u32,
    pub label: String,
    /// Monotonic identity for this registration. A reused pid receives a new
    /// incarnation, fencing delayed signals captured for the previous process.
    pub incarnation: u64,
}

/// Per-window aggregation of process groups. Spawning code
/// (terminal manager, future LSP spawn paths) calls
/// [`ProcessGroups::register`] when a new leader pid is known;
/// `signal_all` fans out through the authority's [`Signaller`]
/// when the window-level lifecycle operation fires.
#[derive(Debug)]
pub struct ProcessGroups {
    signaller: Arc<dyn Signaller>,
    entries: Vec<ProcessGroupEntry>,
    next_incarnation: u64,
}

impl ProcessGroups {
    /// Construct with an explicit [`Signaller`]. Window's
    /// authority decides which signaller — local windows pass
    /// `Arc::new(LocalSignaller)`.
    pub fn new(signaller: Arc<dyn Signaller>) -> Self {
        Self {
            signaller,
            entries: Vec::new(),
            next_incarnation: 1,
        }
    }

    /// Track a new process group leader. Re-registering the same pid for the
    /// same owner is idempotent; a different owner receives a new incarnation
    /// so a delayed signal captured for the retired owner cannot hit it.
    pub fn register(&mut self, leader_pid: u32, label: impl Into<String>) {
        let label = label.into();
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.leader_pid == leader_pid)
        {
            if self.entries[index].label == label {
                return;
            }
            let incarnation = self.next_incarnation;
            self.next_incarnation = self.next_incarnation.saturating_add(1);
            self.entries[index] = ProcessGroupEntry {
                leader_pid,
                label,
                incarnation,
            };
            return;
        }
        let incarnation = self.next_incarnation;
        self.next_incarnation = self.next_incarnation.saturating_add(1);
        self.entries.push(ProcessGroupEntry {
            leader_pid,
            label,
            incarnation,
        });
    }

    /// Drop tracking for `leader_pid`. Doesn't signal — call
    /// when the process has already exited (e.g. from a
    /// `terminal_exit` hook).
    pub fn forget(&mut self, leader_pid: u32) {
        self.entries.retain(|e| e.leader_pid != leader_pid);
    }

    /// Retire only the registration owned by this exact pid/label pair. A
    /// delayed exit for an older terminal must not forget a pid-reused owner.
    pub fn forget_registration(&mut self, leader_pid: u32, label: &str) {
        self.entries
            .retain(|entry| entry.leader_pid != leader_pid || entry.label != label);
    }

    /// Send `signal_name` to every process group registered at call time.
    pub fn signal_all(
        &mut self,
        signal_name: &str,
    ) -> Vec<(ProcessGroupEntry, Result<bool, String>)> {
        let targets = self.entries.clone();
        self.signal_targets(signal_name, &targets)
    }

    /// Signal only registrations whose pid *and incarnation* still match the
    /// supplied snapshot. This is the delayed-escalation fence: if a process
    /// exits and its pid is reused before SIGKILL, the replacement is skipped.
    pub fn signal_targets(
        &mut self,
        signal_name: &str,
        targets: &[ProcessGroupEntry],
    ) -> Vec<(ProcessGroupEntry, Result<bool, String>)> {
        let mut out = Vec::with_capacity(targets.len());
        for target in targets {
            let current = self.entries.iter().any(|entry| {
                entry.leader_pid == target.leader_pid && entry.incarnation == target.incarnation
            });
            if !current {
                out.push((target.clone(), Ok(false)));
                continue;
            }
            let result = self.signaller.signal(target.leader_pid, signal_name);
            if matches!(result, Ok(false)) {
                self.entries.retain(|entry| {
                    entry.leader_pid != target.leader_pid || entry.incarnation != target.incarnation
                });
            }
            out.push((target.clone(), result));
        }
        out
    }

    /// Replace the signaller (e.g. when the window's authority
    /// changes mid-life). Existing entries stay tracked; future
    /// `signal_all` calls go through the new signaller.
    pub fn set_signaller(&mut self, signaller: Arc<dyn Signaller>) {
        self.signaller = signaller;
    }

    pub fn entries(&self) -> &[ProcessGroupEntry] {
        &self.entries
    }
}

impl Default for ProcessGroups {
    fn default() -> Self {
        Self::new(Arc::new(LocalSignaller))
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcessGroups, Signaller};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    struct RecordingSignaller {
        calls: Mutex<Vec<(u32, String)>>,
    }

    impl Signaller for RecordingSignaller {
        fn signal(&self, leader_pid: u32, signal_name: &str) -> Result<bool, String> {
            self.calls
                .lock()
                .unwrap()
                .push((leader_pid, signal_name.to_string()));
            Ok(true)
        }
    }

    #[test]
    fn delayed_signal_and_exit_skip_reused_pid_incarnation() {
        let signaller = Arc::new(RecordingSignaller::default());
        let mut groups = ProcessGroups::new(signaller.clone());
        groups.register(42, "terminal #1");
        let target = groups.entries().to_vec();

        groups.register(42, "terminal #2");
        groups.forget_registration(42, "terminal #1");
        let result = groups.signal_targets("SIGKILL", &target);

        assert!(matches!(result.as_slice(), [(_, Ok(false))]));
        assert!(signaller.calls.lock().unwrap().is_empty());
        assert_eq!(groups.entries()[0].label, "terminal #2");
        assert_ne!(groups.entries()[0].incarnation, target[0].incarnation);
    }
}
