//! Terminal Manager - manages multiple terminal sessions
//!
//! This module provides a manager for terminal sessions that:
//! - Spawns PTY processes with proper shell detection
//! - Manages multiple concurrent terminals
//! - Routes input/output between the editor and terminal processes
//! - Handles terminal resize events
//!
//! # Role in Incremental Streaming Architecture
//!
//! The manager owns the PTY read loop which is the entry point for incremental
//! scrollback streaming. See `super` module docs for the full architecture overview.
//!
//! ## PTY Read Loop
//!
//! The read loop in `spawn()` performs incremental streaming: for each PTY read,
//! it calls `process_output()` to update the terminal grid, then `flush_new_scrollback()`
//! to append any new scrollback lines to the backing file. This ensures scrollback is
//! written incrementally as lines scroll off screen, avoiding O(n) work on mode switches.

pub(crate) use super::omp_companion::{
    OmpCompanionLiveState, OmpCompanionSpawn, OMP_OUTPUT_FRAME_MAX, OMP_OUTPUT_PREFIX,
    OMP_OUTPUT_TERMINATOR, OMP_SYNC_B64_LEN, OMP_TAG_B64_LEN,
};
use super::term::TerminalState;
use crate::services::async_bridge::AsyncBridge;
use crate::services::authority::TerminalWrapper;
use fresh_core::api::{OmpCompanionCommandTargetV1, OmpCompanionCommandType, TerminalCompanion};
pub use fresh_core::TerminalId;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What a spawning terminal should do with the on-disk transcripts (rendered
/// scrollback + raw PTY log) it is handed.
///
/// The distinction is the difference between "this terminal *is* the one that
/// wrote that file" and "that file just happens to sit on this path". Terminal
/// files are named after the terminal id, and ids restart at 0 every editor
/// run, so a brand-new terminal is regularly handed a path a *different*
/// terminal wrote in a previous run (or, after a restore, is still writing —
/// restored terminals keep their old paths under new ids). Inferring "append
/// and seed the history" from "the file is non-empty" made that a scrollback
/// leak between unrelated terminals (fresh#2836); the intent is now explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingMode {
    /// A new terminal: start its transcripts from empty, discarding whatever
    /// an earlier terminal left on those paths.
    Fresh,
    /// The same terminal continuing its append-only history.
    Continue,
}

fn initialize_backing_state(
    state: &mut TerminalState,
    metadata_len: Option<u64>,
    mode: BackingMode,
) {
    match mode {
        BackingMode::Fresh => {}
        BackingMode::Continue => {
            if let Some(length) = metadata_len.filter(|length| *length > 0) {
                state.set_backing_file_history_end(length);
            }
        }
    }
}

/// Messages sent to terminal I/O thread
enum TerminalCommand {
    /// Write data to PTY
    Write(Vec<u8>),
    /// Resize the PTY
    Resize { cols: u16, rows: u16 },
    /// Shutdown the terminal
    Shutdown,
}

/// One atomic routing decision shared by the terminal handle and its reader /
/// wait threads. Adoption rewrites the window-qualified identity and bridge
/// under the same mutex that fences exit, so a send can never pair a target
/// identity with the source bridge (or vice versa).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalRouteState {
    Live,
    Exiting,
    ExitClaimed,
}

struct TerminalRoute {
    terminal: fresh_core::WindowTerminalId,
    bridge: Option<AsyncBridge>,
    state: TerminalRouteState,
}

type SharedTerminalRoute = Arc<Mutex<TerminalRoute>>;

fn route_destination(
    route: &SharedTerminalRoute,
) -> Option<(
    fresh_core::WindowTerminalId,
    mpsc::Sender<crate::services::async_bridge::AsyncMessage>,
)> {
    let route = match route.lock() {
        Ok(route) => route,
        Err(poisoned) => poisoned.into_inner(),
    };
    Some((route.terminal, route.bridge.as_ref()?.sender()))
}

fn mark_route_exiting(route: &SharedTerminalRoute) {
    let mut route = match route.lock() {
        Ok(route) => route,
        Err(poisoned) => poisoned.into_inner(),
    };
    if route.state == TerminalRouteState::Live {
        route.state = TerminalRouteState::Exiting;
    }
}

#[derive(Clone)]
pub struct TerminalScriptCapability {
    inner: Arc<TerminalScriptCapabilityInner>,
}

struct TerminalScriptCapabilityInner {
    token: Mutex<Option<String>>,
}

impl TerminalScriptCapability {
    pub(crate) fn new(token: String) -> Self {
        Self {
            inner: Arc::new(TerminalScriptCapabilityInner {
                token: Mutex::new(Some(token)),
            }),
        }
    }

    fn revoke(&self) {
        let token = match self.inner.token.lock() {
            Ok(mut token) => token.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(token) = token {
            crate::server::command_access::revoke(&token);
        }
    }
}

impl Drop for TerminalScriptCapabilityInner {
    fn drop(&mut self) {
        let token = match self.token.get_mut() {
            Ok(token) => token.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(token) = token {
            crate::server::command_access::revoke(&token);
        }
    }
}

/// Handle to a running terminal session
pub struct TerminalHandle {
    /// Terminal state (grid, cursor, etc.)
    pub state: Arc<Mutex<TerminalState>>,
    /// Command sender to I/O thread
    command_tx: mpsc::Sender<TerminalCommand>,
    /// Whether the terminal is still alive
    alive: Arc<std::sync::atomic::AtomicBool>,
    /// Current dimensions
    cols: u16,
    rows: u16,
    /// Working directory used for the terminal
    cwd: Option<std::path::PathBuf>,
    /// Shell executable used to spawn the terminal
    shell: String,
    /// PID of the shell child process at the head of the pty's
    /// session. `kill(-pid, signal)` (note the negation) signals
    /// the entire process group, which catches subprocesses the
    /// shell or agent forked. `None` on Windows or when
    /// portable_pty couldn't report the pid.
    pid: Option<u32>,
    /// Shared reap/termination fence. Signal delivery and nonblocking reap
    /// polling take the same lock, so a recycled numeric pid is never targeted.
    child_process: Arc<ChildProcess>,
    /// PTY master file descriptor, captured at spawn. Used to read the
    /// terminal's foreground process group via `tcgetpgrp` for tmux-style
    /// tab auto-naming. `None` on Windows or when the platform doesn't
    /// expose it. Only read on Linux (the only `/proc`-backed target).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    master_fd: Option<i32>,
    /// Atomic destination shared with this terminal's reader/wait threads.
    route: SharedTerminalRoute,
    /// Live capability shared with the reader. The editor bridge may clone the
    /// Arc to drain and authenticate the latest candidate without exposing the
    /// secret itself.
    pub(crate) companion: Option<Arc<OmpCompanionLiveState>>,
    /// Host-minted editor-control capability for this exact PTY root.
    script_capability: Option<TerminalScriptCapability>,
}

impl TerminalHandle {
    /// Write data to the terminal (sends to PTY)
    pub fn write(&self, data: &[u8]) {
        // Receiver may be dropped if terminal exited; nothing to do in that case.
        #[allow(clippy::let_underscore_must_use)]
        let _ = self.command_tx.send(TerminalCommand::Write(data.to_vec()));
    }

    /// Resize the terminal
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            // Receiver may be dropped if terminal exited; nothing to do in that case.
            #[allow(clippy::let_underscore_must_use)]
            let _ = self.command_tx.send(TerminalCommand::Resize { cols, rows });
            // Also resize the terminal state
            if let Ok(mut state) = self.state.lock() {
                state.resize(cols, rows);
            }
        }
    }

    /// Check if the terminal is still running
    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The companion kind attached to this PTY incarnation, if any.
    pub fn companion_kind(&self) -> Option<TerminalCompanion> {
        self.companion.as_ref().map(|companion| companion.kind())
    }

    /// Frame and admit a private OMP command to this terminal's writer queue.
    ///
    /// `true` means only that the exact live writer channel accepted the frame;
    /// OMP handling is observed later through snapshots.
    pub fn enqueue_omp_companion_command(
        &self,
        command: OmpCompanionCommandType,
        target: &OmpCompanionCommandTargetV1,
    ) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        let Some(companion) = &self.companion else {
            return false;
        };
        let Some(frame) = companion.frame_command_if_active(command, target, &self.alive) else {
            return false;
        };
        self.command_tx.send(TerminalCommand::Write(frame)).is_ok()
    }

    /// Enqueue a host-private acknowledgement for the exact authenticated snapshot.
    pub(crate) fn enqueue_omp_companion_snapshot_ack(
        &self,
        snapshot: &fresh_core::hooks::OmpCompanionSnapshotV1,
        accepted: bool,
    ) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        let Some(companion) = &self.companion else {
            return false;
        };
        let Some(frame) = companion.frame_snapshot_ack_if_current(snapshot, accepted, &self.alive)
        else {
            return false;
        };
        self.command_tx.send(TerminalCommand::Write(frame)).is_ok()
    }

    /// Revoke command authority immediately while the reader drains already
    /// completed authenticated output.
    pub fn revoke_omp_companion_commands(&self) {
        if let Some(companion) = &self.companion {
            companion.revoke_commands();
        }
    }

    /// Purge companion authentication and candidate delivery after queued
    /// snapshot-ready events have been handled.
    pub fn revoke_omp_companion(&self) {
        if let Some(companion) = &self.companion {
            companion.revoke_access();
        }
    }

    /// Shutdown the terminal.
    pub fn shutdown(&self) {
        self.revoke_omp_companion_commands();
        if let Some(capability) = &self.script_capability {
            capability.revoke();
        }
        // Receiver may be dropped if terminal already exited; nothing to do in that case.
        #[allow(clippy::let_underscore_must_use)]
        let _ = self.command_tx.send(TerminalCommand::Shutdown);
    }

    /// Pid of the shell at the head of the pty session, when
    /// portable_pty was able to report it. Returns `None` on
    /// platforms / configurations that don't expose a pid.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Name of the command currently in the foreground of this terminal,
    /// e.g. `"bash"` at the prompt or `"python3"` while a REPL runs.
    ///
    /// Derived from the PTY's foreground process *group* (`tcgetpgrp` on
    /// the master fd) rather than the shell pid, so it tracks whatever the
    /// user is actually interacting with — the same signal tmux uses for
    /// `#{pane_current_command}`. This is how a tab can read `python3`
    /// even though `python3` never emits an OSC title sequence.
    ///
    /// Only implemented on Linux (via `/proc/<pgid>/comm`); returns `None`
    /// elsewhere so callers fall back to the OSC title or default name.
    pub fn foreground_process_name(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            let fd = self.master_fd?;
            // SAFETY: `fd` is the PTY master, kept open by the writer
            // thread for the terminal's lifetime. `tcgetpgrp` only reads.
            let pgid = unsafe { libc::tcgetpgrp(fd) };
            if pgid <= 0 {
                return None;
            }
            // Local OS introspection of a local fd. The `FileSystem` trait
            // abstracts the *editing* filesystem (possibly remote); it does
            // not apply to reading this host's `/proc`.
            let comm = std::fs::read_to_string(format!("/proc/{pgid}/comm")).ok()?;
            let name = comm.trim();
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Send `signal` to the terminal's live process group. Returns `Ok(false)`
    /// after the root has been reaped or when no positive process-group pid was
    /// reported. The shell is its own session leader inside a pty, so
    /// `kill(-pid, …)` reaches the shell and any subprocesses it forked.
    ///
    /// Recognised signal names: `"SIGTERM"`, `"SIGKILL"`,
    /// `"SIGINT"`, `"SIGHUP"`. Unknown names return an Err
    /// instead of dropping silently.
    #[cfg(unix)]
    pub fn signal(&self, signal_name: &str) -> Result<bool, String> {
        let signal = match signal_name {
            "SIGTERM" => libc::SIGTERM,
            "SIGKILL" => libc::SIGKILL,
            "SIGINT" => libc::SIGINT,
            "SIGHUP" => libc::SIGHUP,
            other => return Err(format!("unsupported signal: {}", other)),
        };
        self.child_process
            .signal_group(signal)
            .map_err(|error| format!("signal {}: {}", signal_name, error))
    }

    /// Windows fallback: no real signal semantics. SIGKILL is
    /// modelled as the existing `shutdown()` (which calls the
    /// pty child killer); other signals are unsupported and
    /// return Ok(false).
    #[cfg(windows)]
    pub fn signal(&self, signal_name: &str) -> Result<bool, String> {
        if signal_name == "SIGKILL" {
            self.shutdown();
            return Ok(true);
        }
        Ok(false)
    }

    /// Get current dimensions
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Get the working directory configured for the terminal
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd.clone()
    }

    /// The shell's *live* working directory — where the user has `cd`'d to,
    /// not where the terminal was spawned. Read from `/proc/<pid>/cwd` on
    /// Linux; other platforms (and dead pids) fall back to the spawn cwd.
    pub fn current_working_dir(&self) -> Option<std::path::PathBuf> {
        #[cfg(target_os = "linux")]
        {
            if let Some(pid) = self.pid {
                // Local OS introspection, same rationale as
                // `foreground_process_name`: this reads the host's /proc,
                // not the (possibly remote) editing filesystem.
                if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                    return Some(cwd);
                }
            }
        }
        self.cwd.clone()
    }

    /// Get the shell executable path used for this terminal
    pub fn shell(&self) -> &str {
        &self.shell
    }
}

/// Manager for multiple terminal sessions
pub struct TerminalManager {
    /// The window that owns this manager. Terminal IDs are only unique
    /// within a single manager (each starts numbering at 0), so output
    /// messages are tagged with `(window_id, terminal_id)` — see
    /// [`fresh_core::WindowTerminalId`] — to stay unambiguous once they
    /// leave this window's context (e.g. on the async bus).
    window_id: fresh_core::WindowId,
    /// Handles whose PTYs are still live in this window.
    terminals: HashMap<TerminalId, TerminalHandle>,
    /// Terminals removed from the live map after shutdown but whose concrete
    /// `TerminalExited` notification has not yet been consumed.
    pending_reap: HashSet<TerminalId>,
    /// Next terminal ID.
    next_id: usize,
    /// Bridge installed into every route spawned or adopted by this manager.
    async_bridge: Option<AsyncBridge>,
}

impl TerminalManager {
    /// Create a new terminal manager owned by `window_id`. The owner is
    /// required (not defaulted) so output can never be attributed to the
    /// wrong window: every terminal this manager spawns is tagged with
    /// it.
    pub fn new(window_id: fresh_core::WindowId) -> Self {
        Self {
            window_id,
            terminals: HashMap::new(),
            pending_reap: HashSet::new(),
            next_id: 0,
            async_bridge: None,
        }
    }

    /// The window that owns this manager.
    pub fn window_id(&self) -> fresh_core::WindowId {
        self.window_id
    }

    /// Install this window's bridge for future spawns/adoptions and retarget
    /// any existing live routes atomically.
    pub fn set_async_bridge(&mut self, bridge: AsyncBridge) {
        for handle in self.terminals.values() {
            let mut route = match handle.route.lock() {
                Ok(route) => route,
                Err(poisoned) => poisoned.into_inner(),
            };
            if route.state == TerminalRouteState::Live {
                route.bridge = Some(bridge.clone());
            }
        }
        self.async_bridge = Some(bridge);
    }

    /// Peek at the next terminal ID that would be assigned.
    pub fn next_terminal_id(&self) -> TerminalId {
        TerminalId(self.next_id)
    }
    /// Reserve and return the next terminal ID without spawning a PTY.
    /// Restored exited terminals use this to keep their durable records from
    /// colliding with the next live terminal allocated in the same window.
    pub fn reserve_terminal_id(&mut self) -> TerminalId {
        let id = TerminalId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Spawn a new terminal session
    ///
    /// # Arguments
    /// * `cols` - Initial terminal width in columns
    /// * `rows` - Initial terminal height in rows
    /// * `cwd` - Optional working directory (defaults to current directory)
    /// * `log_path` - Optional path for raw PTY log (for session restore)
    /// * `backing_path` - Optional path for append-only rendered history
    /// * `backing_mode` - Whether this terminal continues that history on
    ///   restore/respawn or starts a new transcript
    ///
    /// # Returns
    /// The terminal ID if successful
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        &mut self,
        cols: u16,
        rows: u16,
        cwd: Option<std::path::PathBuf>,
        log_path: Option<std::path::PathBuf>,
        backing_path: Option<std::path::PathBuf>,
        backing_mode: BackingMode,
        terminal_wrapper: crate::services::authority::TerminalWrapper,
        env_delta: crate::services::env_provider::EnvDelta,
        extra_env: HashMap<String, String>,
        companion: Option<OmpCompanionSpawn>,
        script_capability: Option<TerminalScriptCapability>,
    ) -> Result<TerminalId, String> {
        let id = self.reserve_terminal_id();

        let handle = self.build_terminal(
            id,
            cols,
            rows,
            cwd,
            log_path,
            backing_path,
            backing_mode,
            terminal_wrapper,
            env_delta,
            extra_env,
            companion,
            script_capability,
        )?;

        self.terminals.insert(id, handle);
        tracing::info!("Created terminal {:?} ({}x{})", id, cols, rows);

        Ok(id)
    }

    /// Build a PTY-backed terminal: open the pty, launch the shell, and wire up
    /// the three background threads (reader, wait, writer) that drive it. Kept
    /// separate from [`TerminalManager::spawn`] so the happy path reads
    /// top-to-bottom with `?` instead of being buried in an error-handling
    /// closure.
    #[allow(clippy::too_many_arguments)]
    fn build_terminal(
        &self,
        id: TerminalId,
        cols: u16,
        rows: u16,
        cwd: Option<std::path::PathBuf>,
        log_path: Option<std::path::PathBuf>,
        backing_path: Option<std::path::PathBuf>,
        backing_mode: BackingMode,
        terminal_wrapper: TerminalWrapper,
        env_delta: crate::services::env_provider::EnvDelta,
        extra_env: HashMap<String, String>,
        companion: Option<OmpCompanionSpawn>,
        script_capability: Option<TerminalScriptCapability>,
    ) -> Result<TerminalHandle, String> {
        let pty_pair = open_pty(cols, rows)?;
        // Reserve every requested transcript before the child exists. Lock
        // contention is a normal spawn error, never a reason to block after an
        // untracked process has already started.
        let (log_writer, backing_writer) =
            open_transcript_writers(log_path.as_deref(), backing_path.as_deref(), backing_mode)
                .map_err(|error| format!("Failed to reserve terminal artifacts: {error}"))?;

        // The active authority's terminal wrapper drives the shell command
        // unconditionally — local wraps `detect_shell()` with no args;
        // container/remote authorities re-parent into `docker exec -w …`,
        // `ssh …`, etc.
        let (cmd, shell) = build_shell_command(
            terminal_wrapper,
            cwd.as_deref(),
            &env_delta,
            &extra_env,
            companion.is_some(),
        );

        // Spawn the shell process.
        let child = pty_pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("Failed to spawn shell '{}': {}", shell, e))?;
        tracing::debug!("Shell process spawned successfully");

        // Capture the pid (for process-group signalling) and a killer handle
        // before `child` moves into the wait-thread below.
        let child_pid = child.process_id();
        let child_killer = child.clone_killer();

        let state = Arc::new(Mutex::new(TerminalState::new(cols, rows)));

        // A continuing terminal seeds its durable boundary from the append-only
        // rendered history before the reader can append.
        if let Some(path) = backing_path.as_ref() {
            let metadata_len = std::fs::metadata(path).ok().map(|metadata| metadata.len());
            if let Ok(mut state) = state.lock() {
                initialize_backing_state(&mut state, metadata_len, backing_mode);
            }
        }

        let (command_tx, command_rx) = mpsc::channel::<TerminalCommand>();
        let alive = Arc::new(AtomicBool::new(true));

        let master_writer = pty_pair
            .master
            .take_writer()
            .map_err(|e| format!("Failed to get PTY writer: {}", e))?;
        let (reader, reader_cancel) = open_cancellable_reader(&*pty_pair.master)?;

        let child_process = Arc::new(ChildProcess::new(child_pid, child_killer));
        // Fatal reader failures terminate through this same reap/signal fence;
        // queuing a writer command alone would allow EOF cleanup to win the race.

        // Identity, destination, and exit state travel together. Adoption may
        // rewrite the first two only while the route is still live.
        let companion = companion.map(|spawn| Arc::new(OmpCompanionLiveState::new(spawn)));
        let route: SharedTerminalRoute = Arc::new(Mutex::new(TerminalRoute {
            terminal: fresh_core::WindowTerminalId::new(self.window_id, id),
            bridge: self.async_bridge.clone(),
            state: TerminalRouteState::Live,
        }));
        let exit_coordinator = Arc::new(ExitCoordinator::with_script_capability(
            route.clone(),
            id,
            companion.clone(),
            reader_cancel.clone(),
            script_capability.clone(),
        ));

        // Reader thread: drains PTY output, feeds the emulator, streams
        // scrollback / raw log to disk, and pings the main loop to redraw.
        let reader_loop = ReaderLoop {
            reader,
            reader_cancel,
            child_process: child_process.clone(),
            state: state.clone(),
            response_tx: command_tx.clone(),
            backing_writer,
            log_writer,
            route: route.clone(),
            terminal_id: id,
            alive: alive.clone(),
            filtered_output: if companion.is_some() {
                Vec::with_capacity(4096)
            } else {
                Vec::new()
            },
            private_boundaries: Vec::new(),
            companion: companion.clone(),
            exit_coordinator: exit_coordinator.clone(),
        };
        thread::spawn(move || reader_loop.run());

        // Wait thread: records status; the shared barrier emits only after the
        // reader has reached EOF and completed its final flush.
        spawn_wait_thread(child, child_process.clone(), exit_coordinator);

        // Capture the PTY master fd before the master moves into the writer
        // thread. Used later by `foreground_process_name` (tab auto-naming).
        let master_fd: Option<i32> = {
            #[cfg(unix)]
            {
                pty_pair.master.as_raw_fd()
            }
            #[cfg(not(unix))]
            {
                None
            }
        };

        // Writer thread: owns the master, applies queued writes/resizes, and
        // kills the child on shutdown.
        spawn_writer_thread(
            command_rx,
            master_writer,
            pty_pair.master,
            child_process.clone(),
        );

        Ok(TerminalHandle {
            state,
            command_tx,
            alive,
            cols,
            rows,
            cwd,
            shell,
            pid: child_pid,
            child_process,
            master_fd,
            route,
            companion,
            script_capability,
        })
    }

    /// Remove a terminal without shutting it down so another window can adopt
    /// it. Exit may race the gap between release and adoption; the shared route
    /// decides which operation wins.
    pub fn release(&mut self, id: TerminalId) -> Option<TerminalHandle> {
        self.terminals.remove(&id)
    }

    /// Restore a released handle after adoption lost the race with exit.
    pub fn restore_released(
        &mut self,
        id: TerminalId,
        handle: TerminalHandle,
    ) -> Result<(), TerminalHandle> {
        if self.terminals.contains_key(&id) {
            return Err(handle);
        }
        self.terminals.insert(id, handle);
        Ok(())
    }

    /// Adopt a terminal only while its route is live. Whichever side acquires
    /// the route mutex first wins: adoption retargets every later send, while a
    /// begun/claimed exit leaves the handle routed to its source and is refused.
    pub fn adopt(&mut self, handle: TerminalHandle) -> Result<TerminalId, TerminalHandle> {
        let id = TerminalId(self.next_id);
        let mut route = match handle.route.lock() {
            Ok(route) => route,
            Err(poisoned) => poisoned.into_inner(),
        };
        if route.state != TerminalRouteState::Live {
            drop(route);
            return Err(handle);
        }
        route.terminal = fresh_core::WindowTerminalId::new(self.window_id, id);
        route.bridge = self.async_bridge.clone();
        drop(route);

        self.next_id += 1;
        self.terminals.insert(id, handle);
        Ok(id)
    }

    /// Get a terminal handle by ID
    pub fn get(&self, id: TerminalId) -> Option<&TerminalHandle> {
        self.terminals.get(&id)
    }

    /// Get a mutable terminal handle by ID
    pub fn get_mut(&mut self, id: TerminalId) -> Option<&mut TerminalHandle> {
        self.terminals.get_mut(&id)
    }

    /// Close a terminal and retain its id until the concrete exit is consumed.
    pub fn close(&mut self, id: TerminalId) -> bool {
        let Some(handle) = self.terminals.remove(&id) else {
            return false;
        };
        handle.shutdown();
        self.pending_reap.insert(id);
        true
    }

    /// Consume a concrete exit, whether the handle was still live or had
    /// already been removed by an explicit close.
    pub fn reap(&mut self, id: TerminalId) -> bool {
        let live = self.terminals.remove(&id).is_some_and(|handle| {
            handle.revoke_omp_companion();
            true
        });
        self.pending_reap.remove(&id) || live
    }

    /// Get all terminal IDs
    pub fn terminal_ids(&self) -> Vec<TerminalId> {
        self.terminals.keys().copied().collect()
    }

    /// Every identity whose exit can still arrive on this window's bridge.
    pub fn tracked_terminal_ids(&self) -> HashSet<TerminalId> {
        self.terminals
            .keys()
            .chain(self.pending_reap.iter())
            .copied()
            .collect()
    }

    /// Get count of open terminals
    pub fn count(&self) -> usize {
        self.terminals.len()
    }

    /// Shutdown all live terminals while retaining their ids for reap.
    pub fn shutdown_all(&mut self) {
        for (id, handle) in self.terminals.drain() {
            handle.shutdown();
            self.pending_reap.insert(id);
        }
    }

    /// Clean up dead terminals
    pub fn cleanup_dead(&mut self) -> Vec<TerminalId> {
        let dead: Vec<TerminalId> = self
            .terminals
            .iter()
            .filter(|(_, handle)| !handle.is_alive())
            .map(|(id, _)| *id)
            .collect();

        for id in &dead {
            if let Some(handle) = self.terminals.remove(id) {
                handle.revoke_omp_companion();
                self.pending_reap.insert(*id);
            }
        }

        dead
    }
}

/// Open a native PTY of the given size, mapping the platform error into a
/// human-readable string (with a ConPTY hint on Windows).
fn open_pty(cols: u16, rows: u16) -> Result<portable_pty::PtyPair, String> {
    native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| {
            #[cfg(windows)]
            {
                format!(
                    "Failed to open PTY: {}. Note: Terminal requires Windows 10 version 1809 or later with ConPTY support.",
                    e
                )
            }
            #[cfg(not(windows))]
            {
                format!("Failed to open PTY: {}", e)
            }
        })
}

/// Build the shell `CommandBuilder` for a terminal from the active authority's
/// wrapper, returning the command plus the shell executable name (for the
/// handle / diagnostics). `manages_cwd` wrappers (docker/ssh) already establish
/// cwd in their own args, so both cwd and the local `FRESH_SESSION`
/// advertisement are skipped for them — their inner shell runs on another host
/// this `CommandBuilder`'s env can't reach.
fn is_reserved_omp_companion_env(key: &str) -> bool {
    if cfg!(windows) {
        key.to_ascii_uppercase().starts_with("FRESH_OMP_COMPANION")
    } else {
        key.starts_with("FRESH_OMP_COMPANION")
    }
}

fn build_shell_command(
    terminal_wrapper: TerminalWrapper,
    cwd: Option<&std::path::Path>,
    env_delta: &crate::services::env_provider::EnvDelta,
    extra_env: &HashMap<String, String>,
    companion_authorized: bool,
) -> (CommandBuilder, String) {
    let TerminalWrapper {
        command: shell,
        args: cmd_args,
        manages_cwd: skip_cwd,
    } = terminal_wrapper;
    tracing::info!("Spawning terminal with shell: {}", shell);

    let mut cmd = CommandBuilder::new(&shell);
    for arg in &cmd_args {
        cmd.arg(arg);
    }
    if !skip_cwd {
        if let Some(dir) = cwd {
            // Hand the shell a non-verbatim path so PowerShell can infer the
            // drive; a verbatim `\\?\C:\…` path yields provider-prefixed prompts.
            cmd.cwd(strip_verbatim_prefix(dir).as_ref());
        }
    }

    // Never inherit private companion credentials or markers from the editor's
    // own environment. Only a host-minted companion spawn may add them back,
    // from `extra_env`, after every other environment source is applied.
    for (key, _) in std::env::vars_os() {
        if is_reserved_omp_companion_env(&key.to_string_lossy()) {
            cmd.env_remove(key);
        }
    }

    // Apply the activated-environment delta (venv/direnv/mise) before the
    // control vars below, so TERM/FRESH_SESSION win over any same-named key
    // (issue #2355).
    for (k, v) in &env_delta.set {
        if !is_reserved_omp_companion_env(k) {
            cmd.env(k, v);
        }
    }
    for k in &env_delta.unset {
        cmd.env_remove(k);
    }

    // Advertise terminal capabilities; the built-in emulator is alacritty-based.
    cmd.env("TERM", "xterm-256color");

    // Advertise this editor's local control socket so a nested `fresh` forwards
    // file/dir opens back to us instead of starting a second editor.
    if !skip_cwd {
        if let Some(session_id) = crate::server::local_control::local_session_id() {
            cmd.env("FRESH_SESSION", session_id);
        }
        // Advertise the running fresh executable's own path so a nested `fresh`
        // — and, above all, an agent taught the Fresh CLI — invokes the EXACT
        // same binary this editor is running. Its `--cmd` verbs and `--help`
        // then match this build, never some other `fresh` that happens to sit
        // earlier on PATH. Local-only (skip_cwd ⇒ a remote host where this path
        // is meaningless), mirroring FRESH_SESSION.
        if let Ok(exe) = std::env::current_exe() {
            cmd.env("FRESH_BIN", exe);
        }
    }

    // On Windows, ensure PROMPT is set for cmd.exe.
    #[cfg(windows)]
    {
        if shell.to_lowercase().contains("cmd") {
            cmd.env("PROMPT", "$P$G");
        }
    }

    // Caller-supplied extra env (e.g. the Orchestrator's `FRESH_CMD_TOKEN`
    // capability token, or plugin-provided vars). Applied last so these keys
    // are guaranteed present in the child; they intentionally win over the
    // activated env delta above. Normal callers pass an empty map — no change.
    for (k, v) in extra_env {
        if companion_authorized || !is_reserved_omp_companion_env(k) {
            cmd.env(k, v);
        }
    }

    (cmd, shell)
}

/// Cancellation for the exact thread/descriptor blocked in the PTY read.
/// Dropping another clone of the master cannot wake a reader while a slave is
/// still open, so the drain deadline targets this handle directly.
struct ReaderCancellation {
    cancelled: AtomicBool,
    #[cfg(unix)]
    wake: UnixStream,
    #[cfg(windows)]
    thread: Mutex<Option<OwnedHandle>>,
}

impl ReaderCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        #[cfg(unix)]
        {
            let _ = (&self.wake).write(&[1]);
        }
        #[cfg(windows)]
        self.cancel_windows_read();
    }

    #[cfg(windows)]
    fn cancel_windows_read(&self) {
        // The registration/read gap is tiny but real. Repeating the call catches
        // a synchronous pipe read that starts just after the first call observes
        // no pending operation.
        for _ in 0..8 {
            let thread = match self.thread.lock() {
                Ok(thread) => thread,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(thread) = thread.as_ref() {
                unsafe {
                    windows_sys::Win32::System::IO::CancelSynchronousIo(thread.as_raw_handle());
                }
            }
            drop(thread);
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(windows)]
    fn register_current_thread(&self) -> std::io::Result<()> {
        use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};

        let process = unsafe { GetCurrentProcess() };
        let mut duplicated: HANDLE = std::ptr::null_mut();
        let duplicated_ok = unsafe {
            DuplicateHandle(
                process,
                GetCurrentThread(),
                process,
                &mut duplicated,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        if duplicated_ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(duplicated) };
        let mut slot = match self.thread.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(handle);
        drop(slot);
        if self.is_cancelled() {
            self.cancel_windows_read();
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn register_current_thread(&self) -> std::io::Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn detached() -> Arc<Self> {
        #[cfg(unix)]
        {
            let (_reader, wake) = UnixStream::pair().expect("reader cancellation socketpair");
            return Arc::new(Self {
                cancelled: AtomicBool::new(false),
                wake,
            });
        }
        #[cfg(windows)]
        {
            Arc::new(Self {
                cancelled: AtomicBool::new(false),
                thread: Mutex::new(None),
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Arc::new(Self {
                cancelled: AtomicBool::new(false),
            })
        }
    }
}

#[cfg(unix)]
struct UnixPtyReader {
    file: std::fs::File,
    cancel: UnixStream,
}

#[cfg(unix)]
impl Read for UnixPtyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut descriptors = [
                libc::pollfd {
                    fd: self.file.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.cancel.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                    revents: 0,
                },
            ];
            let ready = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    -1,
                )
            };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if descriptors[1].revents != 0 {
                return Ok(0);
            }
            if descriptors[0].revents != 0 {
                return self.file.read(buf);
            }
        }
    }
}

fn open_cancellable_reader(
    master: &dyn portable_pty::MasterPty,
) -> Result<(Box<dyn Read + Send>, Arc<ReaderCancellation>), String> {
    #[cfg(unix)]
    {
        let master_fd = master
            .as_raw_fd()
            .ok_or_else(|| "PTY master did not expose a reader fd".to_string())?;
        let reader_fd = unsafe { libc::fcntl(master_fd, libc::F_DUPFD_CLOEXEC, 0) };
        if reader_fd < 0 {
            return Err(format!(
                "Failed to clone PTY reader: {}",
                std::io::Error::last_os_error()
            ));
        }
        let file = unsafe { std::fs::File::from_raw_fd(reader_fd) };
        let (cancel, wake) = UnixStream::pair()
            .map_err(|error| format!("Failed to create PTY reader cancellation: {}", error))?;
        let cancellation = Arc::new(ReaderCancellation {
            cancelled: AtomicBool::new(false),
            wake,
        });
        return Ok((Box::new(UnixPtyReader { file, cancel }), cancellation));
    }
    #[cfg(windows)]
    {
        let reader = master
            .try_clone_reader()
            .map_err(|error| format!("Failed to get PTY reader: {}", error))?;
        let cancellation = Arc::new(ReaderCancellation {
            cancelled: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        Ok((reader, cancellation))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let reader = master
            .try_clone_reader()
            .map_err(|error| format!("Failed to get PTY reader: {}", error))?;
        let cancellation = Arc::new(ReaderCancellation {
            cancelled: AtomicBool::new(false),
        });
        Ok((reader, cancellation))
    }
}

#[derive(Default)]
struct ChildProcessState {
    reaped: bool,
    termination_started: bool,
}

/// Signal/reap serialization for the PTY root process. `try_wait` and signal
/// delivery run under the same mutex, so the pid cannot be reaped and reused in
/// between the liveness check and `kill(2)`.
struct ChildProcess {
    state: Mutex<ChildProcessState>,
    pid: Option<u32>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
}

const CHILD_STATUS_POLL: Duration = Duration::from_millis(10);
const CHILD_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

impl ChildProcess {
    fn new(pid: Option<u32>, killer: Box<dyn portable_pty::ChildKiller + Send + Sync>) -> Self {
        #[cfg(unix)]
        let pid = pid.filter(|pid| i32::try_from(*pid).is_ok_and(|pid| pid > 0));
        Self {
            state: Mutex::new(ChildProcessState::default()),
            pid,
            killer: Mutex::new(killer),
        }
    }

    fn wait_for_exit(
        &self,
        child: &mut dyn portable_pty::Child,
        terminal_id: TerminalId,
    ) -> Option<i32> {
        loop {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    state.reaped = true;
                    return Some(status.exit_code() as i32);
                }
                Ok(None) => {}
                Err(error) => {
                    // Never signal a numeric pid after a failed reap query: its
                    // ownership can no longer be established safely.
                    state.reaped = true;
                    tracing::warn!("child.try_wait() failed for {:?}: {}", terminal_id, error);
                    return None;
                }
            }
            drop(state);
            thread::sleep(CHILD_STATUS_POLL);
        }
    }

    fn request_termination(&self) {
        {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            if state.reaped || state.termination_started {
                return;
            }
            state.termination_started = true;
            #[cfg(unix)]
            {
                if let Some(pid) = self.pid {
                    if let Err(error) = signal_process_group(pid, libc::SIGHUP) {
                        tracing::warn!("Failed to send terminal SIGHUP: {}", error);
                    }
                } else if let Ok(mut killer) = self.killer.lock() {
                    let _ = killer.kill();
                }
            }
            #[cfg(not(unix))]
            if let Ok(mut killer) = self.killer.lock() {
                // On Windows this is the existing duplicated process handle and
                // TerminateProcess path, not a reusable numeric pid.
                let _ = killer.kill();
            }
        }

        #[cfg(unix)]
        {
            let deadline = Instant::now() + CHILD_SHUTDOWN_GRACE;
            while Instant::now() < deadline {
                let reaped = match self.state.lock() {
                    Ok(state) => state.reaped,
                    Err(poisoned) => poisoned.into_inner().reaped,
                };
                if reaped {
                    return;
                }
                thread::sleep(CHILD_STATUS_POLL);
            }
            let state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !state.reaped {
                if let Some(pid) = self.pid {
                    if let Err(error) = signal_process_group(pid, libc::SIGKILL) {
                        tracing::warn!("Failed to hard-kill terminal process group: {}", error);
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    fn signal_group(&self, signal: libc::c_int) -> std::io::Result<bool> {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.reaped {
            return Ok(false);
        }
        let Some(pid) = self.pid else {
            return Ok(false);
        };
        signal_process_group(pid, signal)
    }

    #[cfg(test)]
    fn mark_reaped(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.reaped = true;
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) -> std::io::Result<bool> {
    let result = unsafe { libc::kill(-(pid as i32), signal) };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// Interprocess lock path shared by live transcript writers and restore-time
/// migration/promotion. The lock file is moved with an extracted artifact so
/// the open lock inode continues fencing the writer under its new namespace.
pub(crate) fn terminal_artifact_lock_path(path: &std::path::Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("terminal-artifact");
    path.with_file_name(format!(".{name}.lock"))
}

pub(crate) struct TerminalArtifactLock {
    _file: std::fs::File,
}

fn reserve_terminal_artifact(path: &std::path::Path) -> std::io::Result<TerminalArtifactLock> {
    try_lock_terminal_artifact(path)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!("terminal artifact is already live: {}", path.display()),
        )
    })
}

pub(crate) fn try_lock_terminal_artifact(
    path: &std::path::Path,
) -> std::io::Result<Option<TerminalArtifactLock>> {
    let lock_path = terminal_artifact_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(TerminalArtifactLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

struct LockedTranscriptFile {
    file: std::fs::File,
    _lock: TerminalArtifactLock,
}

impl Write for LockedTranscriptFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.file.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

struct BackingWriter {
    file: LockedTranscriptFile,
}

impl Write for BackingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.file.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl crate::services::terminal::term::DurableScrollbackWriter for BackingWriter {
    fn len(&self) -> std::io::Result<u64> {
        self.file.file.metadata().map(|metadata| metadata.len())
    }

    fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        self.file.file.set_len(len)
    }

    fn sync_all(&self) -> std::io::Result<()> {
        self.file.file.sync_all()
    }
}

/// Reserve and open the optional raw and rendered transcripts as one ownership
/// bundle. Every nonblocking lock is acquired before any path is truncated.
fn open_transcript_writers(
    log_path: Option<&std::path::Path>,
    backing_path: Option<&std::path::Path>,
    mode: BackingMode,
) -> std::io::Result<(
    Option<std::io::BufWriter<LockedTranscriptFile>>,
    Option<BackingWriter>,
)> {
    if log_path.is_some() && log_path == backing_path {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "terminal log and history paths must differ",
        ));
    }
    let log_lock = log_path.map(reserve_terminal_artifact).transpose()?;
    let backing_lock = backing_path.map(reserve_terminal_artifact).transpose()?;
    let log_writer = match (log_path, log_lock) {
        (Some(path), Some(lock)) => Some(std::io::BufWriter::new(open_transcript_file(
            path, mode, lock,
        )?)),
        _ => None,
    };
    let backing_writer = match (backing_path, backing_lock) {
        (Some(path), Some(lock)) => Some(BackingWriter {
            file: open_transcript_file(path, mode, lock)?,
        }),
        _ => None,
    };
    Ok((log_writer, backing_writer))
}

/// Open a reserved transcript: append when continuing an existing story,
/// truncate when starting a new one. The supplied lock stays owned by the
/// reader loop through its final durable flush.
fn open_transcript_file(
    path: &std::path::Path,
    mode: BackingMode,
    lock: TerminalArtifactLock,
) -> std::io::Result<LockedTranscriptFile> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .truncate(mode == BackingMode::Fresh)
        .open(path)?;
    Ok(LockedTranscriptFile { file, _lock: lock })
}

/// Shared child-status/reader-drained barrier. Only actual reader completion
/// can claim and emit exit; the grace timeout wakes the exact blocked reader so
/// that thread still owns EOF classification, final flushes, and filter cleanup.
struct ExitCoordinator {
    inner: Mutex<ExitCoordinatorState>,
    route: SharedTerminalRoute,
    terminal_id: TerminalId,
    companion: Option<Arc<OmpCompanionLiveState>>,
    script_capability: Option<TerminalScriptCapability>,
    reader_cancel: Arc<ReaderCancellation>,
}

#[derive(Default)]
struct ExitCoordinatorState {
    child_finished: bool,
    reader_drained: bool,
    drain_forced: bool,
    emitted: bool,
    exit_code: Option<i32>,
}

const READER_DRAIN_GRACE: Duration = Duration::from_millis(500);

impl ExitCoordinator {
    fn new(
        route: SharedTerminalRoute,
        terminal_id: TerminalId,
        companion: Option<Arc<OmpCompanionLiveState>>,
        reader_cancel: Arc<ReaderCancellation>,
    ) -> Self {
        Self::with_script_capability(route, terminal_id, companion, reader_cancel, None)
    }

    fn with_script_capability(
        route: SharedTerminalRoute,
        terminal_id: TerminalId,
        companion: Option<Arc<OmpCompanionLiveState>>,
        reader_cancel: Arc<ReaderCancellation>,
        script_capability: Option<TerminalScriptCapability>,
    ) -> Self {
        Self {
            inner: Mutex::new(ExitCoordinatorState::default()),
            route,
            terminal_id,
            companion,
            reader_cancel,
            script_capability,
        }
    }

    fn child_finished(&self, exit_code: Option<i32>) {
        // New child commands end with the root process. The verifier and any
        // completed candidates remain until the reader's queued ready events
        // are delivered ahead of TerminalExited.
        if let Some(companion) = &self.companion {
            companion.revoke_commands();
        }
        if let Some(capability) = &self.script_capability {
            capability.revoke();
        }
        mark_route_exiting(&self.route);
        let should_emit = {
            let Ok(mut state) = self.inner.lock() else {
                return;
            };
            state.child_finished = true;
            state.exit_code = exit_code;
            Self::claim_emission(&mut state)
        };
        if should_emit {
            self.emit();
        }
    }

    fn reader_drained(&self) {
        mark_route_exiting(&self.route);
        let should_emit = {
            let Ok(mut state) = self.inner.lock() else {
                return;
            };
            state.reader_drained = true;
            Self::claim_emission(&mut state)
        };
        if should_emit {
            self.emit();
        }
    }

    /// A descendant can retain the PTY slave after the root child exits. Wake
    /// the reader's own blocking operation; unrelated writer/master clones are
    /// intentionally not part of this drain boundary.
    fn reader_drain_timed_out(&self) {
        let should_force = {
            let Ok(mut state) = self.inner.lock() else {
                return;
            };
            if state.child_finished && !state.reader_drained && !state.drain_forced {
                state.drain_forced = true;
                true
            } else {
                false
            }
        };
        if should_force {
            tracing::warn!(
                "PTY reader for {:?} did not drain within {:?}; cancelling read",
                self.terminal_id,
                READER_DRAIN_GRACE
            );
            self.reader_cancel.cancel();
        }
    }

    fn claim_emission(state: &mut ExitCoordinatorState) -> bool {
        if state.child_finished && state.reader_drained && !state.emitted {
            state.emitted = true;
            true
        } else {
            false
        }
    }

    fn emit(&self) {
        if let Some(companion) = &self.companion {
            companion.finalize_output_filter();
        }
        let exit_code = self.inner.lock().ok().and_then(|state| state.exit_code);
        let destination = {
            let mut route = match self.route.lock() {
                Ok(route) => route,
                Err(poisoned) => poisoned.into_inner(),
            };
            if route.state == TerminalRouteState::ExitClaimed {
                return;
            }
            route.state = TerminalRouteState::ExitClaimed;
            route
                .bridge
                .as_ref()
                .map(|bridge| (route.terminal, bridge.sender()))
        };
        let Some((terminal, sender)) = destination else {
            return;
        };
        #[allow(clippy::let_underscore_must_use)]
        let _ = sender.send(
            crate::services::async_bridge::AsyncMessage::TerminalExited {
                terminal,
                exit_code,
            },
        );
    }
}

/// Wait-thread body: poll/reap under the same fence used for signal delivery,
/// then start the bounded reader-drain grace period.
fn spawn_wait_thread(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    child_process: Arc<ChildProcess>,
    exit_coordinator: Arc<ExitCoordinator>,
) {
    thread::spawn(move || {
        let exit_code = child_process.wait_for_exit(&mut *child, exit_coordinator.terminal_id);
        exit_coordinator.child_finished(exit_code);
        thread::sleep(READER_DRAIN_GRACE);
        exit_coordinator.reader_drain_timed_out();
    });
}

/// Writer-thread body. Only an explicit shutdown or PTY write/flush failure
/// starts termination; a clean channel disconnect after reap merely releases
/// the writer/master handles.
fn run_writer_loop(
    command_rx: mpsc::Receiver<TerminalCommand>,
    mut master: Box<dyn Write + Send>,
    pty_master: Box<dyn portable_pty::MasterPty + Send>,
    child_process: &ChildProcess,
) {
    let terminate = loop {
        match command_rx.recv() {
            Ok(TerminalCommand::Write(data)) => {
                if let Err(error) = master.write_all(&data).and_then(|()| master.flush()) {
                    tracing::error!("Terminal write error: {}", error);
                    break true;
                }
            }
            Ok(TerminalCommand::Resize { cols, rows }) => {
                if let Err(error) = pty_master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    tracing::warn!("Failed to resize PTY: {}", error);
                }
            }
            Ok(TerminalCommand::Shutdown) => break true,
            Err(_) => break false,
        }
    };
    if terminate {
        child_process.request_termination();
    }
}

fn spawn_writer_thread(
    command_rx: mpsc::Receiver<TerminalCommand>,
    master: Box<dyn Write + Send>,
    pty_master: Box<dyn portable_pty::MasterPty + Send>,
    child_process: Arc<ChildProcess>,
) {
    thread::spawn(move || run_writer_loop(command_rx, master, pty_master, &child_process));
}

/// Owns everything the PTY reader thread needs. Bundled into one struct so the
/// thread body is a readable `run(self)` of small steps instead of a closure
/// capturing a dozen locals at deep nesting.
struct ReaderLoop {
    reader: Box<dyn Read + Send>,
    reader_cancel: Arc<ReaderCancellation>,
    child_process: Arc<ChildProcess>,
    state: Arc<Mutex<TerminalState>>,
    /// Sends PTY write-responses (e.g. DSR cursor reports) back to the writer.
    response_tx: mpsc::Sender<TerminalCommand>,
    backing_writer: Option<BackingWriter>,
    /// Raw byte log for session-restore replay, if a log file is set.
    log_writer: Option<std::io::BufWriter<LockedTranscriptFile>>,
    route: SharedTerminalRoute,
    terminal_id: TerminalId,
    alive: Arc<AtomicBool>,
    companion: Option<Arc<OmpCompanionLiveState>>,
    filtered_output: Vec<u8>,
    /// Scanner bookkeeping only; private-frame boundaries never synthesize
    /// bytes into the public emulator or raw transcript.
    private_boundaries: Vec<usize>,
    exit_coordinator: Arc<ExitCoordinator>,
}

impl ReaderLoop {
    /// Drain the PTY until EOF, cancellation, or a permanent error. Interrupted
    /// reads are retried without touching filter/scanner/lifecycle state.
    fn run(mut self) {
        tracing::debug!("Terminal {:?} reader thread started", self.terminal_id);
        let mut buf = [0u8; 4096];
        let mut total_bytes = 0usize;
        let reader_registered = match self.reader_cancel.register_current_thread() {
            Ok(()) => true,
            Err(error) => {
                tracing::error!("Failed to register PTY reader cancellation: {}", error);
                self.request_child_shutdown();
                false
            }
        };
        while reader_registered && !self.reader_cancel.is_cancelled() {
            match self.reader.read(&mut buf) {
                Ok(0) => {
                    tracing::info!(
                        "Terminal {:?} EOF after {} total bytes",
                        self.terminal_id,
                        total_bytes
                    );
                    break;
                }
                Ok(n) => {
                    total_bytes += n;
                    tracing::trace!(
                        "Terminal {:?} received {} bytes (total: {})",
                        self.terminal_id,
                        n,
                        total_bytes
                    );
                    self.process_read(&buf[..n], Instant::now());
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    if self.reader_cancel.is_cancelled() {
                        break;
                    }
                }
                Err(error) => {
                    if !self.reader_cancel.is_cancelled() {
                        tracing::error!("Terminal read error: {}", error);
                        // A permanent read failure is not a clean EOF. Start the
                        // bounded child termination before classifying the
                        // scanner tail as final.
                        self.request_child_shutdown();
                    }
                    break;
                }
            }
        }

        // Any exact private-frame prefix at EOF is ambiguous and stays private;
        // ordinary mismatches were already replayed while scanning.
        let trailing_visible = self
            .companion
            .as_ref()
            .map_or_else(Vec::new, |companion| companion.finish_output());
        if !trailing_visible.is_empty() {
            self.process_visible_output(&trailing_visible);
        }

        // Retry any failed rendered-history publication even when no later PTY
        // chunk arrived. Exit is not claimed until this final attempt completes.
        let state = Arc::clone(&self.state);
        if let Ok(mut state) = state.lock() {
            self.persist_backing(&mut state);
        }
        if let Some(writer) = self.log_writer.as_mut() {
            if let Err(error) = writer.flush() {
                tracing::warn!("Terminal log flush error: {}", error);
            }
        }
        // Publish exit only after transcript locks are released. Close cleanup
        // can then reserve the same paths without racing the old reader.
        drop(self.backing_writer.take());
        drop(self.log_writer.take());
        self.alive.store(false, Ordering::Release);
        self.exit_coordinator.reader_drained();
    }

    fn request_child_shutdown(&self) {
        // Wake the writer so it drops the master, and also start termination
        // directly: channel delivery alone does not order child shutdown before
        // this reader performs EOF/filter finalization.
        let _ = self.response_tx.send(TerminalCommand::Shutdown);
        self.child_process.request_termination();
    }

    fn process_read(&mut self, bytes: &[u8], now: Instant) {
        let Some(companion) = self.companion.as_ref() else {
            self.process_visible_output(bytes);
            return;
        };
        let notify_candidate = companion.filter_output_into(
            bytes,
            now,
            &mut self.filtered_output,
            &mut self.private_boundaries,
        );
        if notify_candidate {
            self.notify_companion_candidate();
        }
        // Private frames are removed, not replaced. In particular, never inject
        // CAN or any other synthetic parser byte into the emulator/raw replay.
        self.private_boundaries.clear();
        if !self.filtered_output.is_empty() {
            let mut visible = std::mem::take(&mut self.filtered_output);
            self.process_visible_output(&visible);
            visible.clear();
            self.filtered_output = visible;
        }
    }

    fn process_visible_output(&mut self, bytes: &[u8]) {
        self.process_output(bytes);
        self.append_raw_log(bytes);
        self.notify_redraw();
    }

    /// Feed `bytes` to the emulator, forward any PTY write-responses, and stream
    /// new scrollback to the backing file. Holds the state lock for the whole
    /// step so scrollback offsets stay consistent with the emulator grid.
    fn process_output(&mut self, bytes: &[u8]) {
        let shared_state = Arc::clone(&self.state);
        let Ok(mut state) = shared_state.lock() else {
            return;
        };
        state.process_output(bytes);

        // Send any PTY write responses (e.g. DSR cursor position). Critical on
        // Windows ConPTY, where PowerShell waits for this before prompting.
        for response in state.drain_pty_write_queue() {
            tracing::debug!(
                "Terminal {:?} sending PTY response: {:?}",
                self.terminal_id,
                response
            );
            let _ = self
                .response_tx
                .send(TerminalCommand::Write(response.into_bytes()));
        }

        self.persist_backing(&mut state);
    }

    fn persist_backing(&mut self, state: &mut TerminalState) {
        let Some(writer) = self.backing_writer.as_mut() else {
            return;
        };
        if let Err(error) = state.persist_new_scrollback(writer) {
            // Keep the writer and the state-owned rollback fence. A later PTY
            // chunk or final drain restores the captured boundary before retry.
            tracing::warn!("Terminal backing file write error: {}", error);
        }
    }

    /// Append raw bytes to the session log (for restore replay), if enabled.
    fn append_raw_log(&mut self, bytes: &[u8]) {
        if let Some(w) = self.log_writer.as_mut() {
            if let Err(e) = w.write_all(bytes) {
                tracing::warn!("Terminal log write error: {}", e);
                self.log_writer = None;
            } else if let Err(e) = w.flush() {
                tracing::warn!("Terminal log flush error: {}", e);
                self.log_writer = None;
            }
        }
    }

    /// Notify the main loop that this terminal produced output (redraw).
    fn notify_redraw(&self) {
        let Some((terminal, sender)) = route_destination(&self.route) else {
            return;
        };
        #[allow(clippy::let_underscore_must_use)]
        let _ =
            sender.send(crate::services::async_bridge::AsyncMessage::TerminalOutput { terminal });
    }

    fn notify_companion_candidate(&self) {
        let Some((terminal, sender)) = route_destination(&self.route) else {
            return;
        };
        #[allow(clippy::let_underscore_must_use)]
        let _ = sender.send(
            crate::services::async_bridge::AsyncMessage::OmpCompanionSnapshotReady { terminal },
        );
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

/// Convert a Windows verbatim path (`\\?\C:\…` or `\\?\UNC\server\share\…`)
/// into its non-verbatim equivalent (`C:\…` or `\\server\share\…`).
///
/// Returns the input unchanged on non-Windows platforms or for paths that
/// have no verbatim prefix.
pub(crate) fn strip_verbatim_prefix(path: &std::path::Path) -> Cow<'_, std::path::Path> {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        let mut components = path.components();
        let prefix = match components.next() {
            Some(Component::Prefix(p)) => p,
            _ => return Cow::Borrowed(path),
        };

        let mut rebuilt = std::path::PathBuf::new();
        match prefix.kind() {
            Prefix::VerbatimDisk(drive) => {
                rebuilt.push(format!("{}:\\", drive as char));
            }
            Prefix::VerbatimUNC(server, share) => {
                rebuilt.push(format!(
                    r"\\{}\{}\",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                ));
            }
            _ => return Cow::Borrowed(path),
        }
        // Skip the original RootDir (which the rebuilt prefix already includes)
        // and append the rest of the components.
        for component in components {
            if matches!(component, Component::RootDir) {
                continue;
            }
            rebuilt.push(component.as_os_str());
        }
        Cow::Owned(rebuilt)
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

/// Detect the user's shell
pub fn detect_shell() -> String {
    // Try $SHELL environment variable first
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.is_empty() {
            return shell;
        }
    }

    // Fall back to platform defaults
    #[cfg(unix)]
    {
        "/bin/sh".to_string()
    }
    #[cfg(windows)]
    {
        super::windows_shell::select_windows_shell()
    }
}

#[cfg(test)]
mod tests {
    use super::super::omp_companion::{
        omp_companion_output_tag, omp_companion_synchronizer, test_companion,
        test_output_frame as output_frame, test_secret,
    };
    use super::*;
    use base64::Engine as _;
    use std::collections::VecDeque;

    #[test]
    fn live_transcript_writer_excludes_restore_and_relocation() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("terminal.history.txt");
        let writer_lock = reserve_terminal_artifact(&artifact).unwrap();

        assert!(try_lock_terminal_artifact(&artifact).unwrap().is_none());
        drop(writer_lock);
        assert!(try_lock_terminal_artifact(&artifact).unwrap().is_some());
    }

    #[test]
    fn transcript_contention_fails_before_any_fresh_file_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("terminal.log");
        let history = dir.path().join("terminal.history.txt");
        std::fs::write(&log, b"old-log").unwrap();
        std::fs::write(&history, b"old-history").unwrap();
        let _history_owner = reserve_terminal_artifact(&history).unwrap();

        let error = match open_transcript_writers(Some(&log), Some(&history), BackingMode::Fresh) {
            Ok(_) => panic!("contended ownership must fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read(&log).unwrap(), b"old-log");
        assert_eq!(std::fs::read(&history).unwrap(), b"old-history");
    }
    fn command_target() -> OmpCompanionCommandTargetV1 {
        OmpCompanionCommandTargetV1 {
            incarnation: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            session_generation: 3,
            session_id: "018f1d74-7f7b-7d31-8d93-9a21c7b95bb1".to_string(),
            work_epoch: 9,
        }
    }

    fn admit_command_target(
        companion: &OmpCompanionLiveState,
        target: &OmpCompanionCommandTargetV1,
    ) {
        let snapshot = snapshot_for_target(target);
        assert!(companion.admit_sequence(
            &target.incarnation,
            snapshot.sequence,
            target.session_generation,
            &target.session_id,
            target.work_epoch,
        ));
        assert!(companion.commit_admitted_snapshot(&snapshot, false));
    }

    fn snapshot_for_target(
        target: &OmpCompanionCommandTargetV1,
    ) -> fresh_core::hooks::OmpCompanionSnapshotV1 {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "incarnation": target.incarnation,
            "sequence": 1,
            "sessionGeneration": target.session_generation,
            "workEpoch": target.work_epoch,
            "timestampMs": 0,
            "ompVersion": "test",
            "processId": 1,
            "sessionId": target.session_id,
            "cwd": "/tmp",
            "state": "idle",
            "runningTools": 0,
            "pendingApprovals": 0
        }))
        .unwrap()
    }

    struct ChunkReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl ChunkReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
            }
        }
    }

    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let Some(chunk) = self.chunks.front_mut() else {
                return Ok(0);
            };
            let count = chunk.len().min(buf.len());
            buf[..count].copy_from_slice(&chunk[..count]);
            chunk.drain(..count);
            if chunk.is_empty() {
                self.chunks.pop_front();
            }
            Ok(count)
        }
    }

    enum ReadStep {
        Bytes(Vec<u8>),
        Error(std::io::ErrorKind),
    }

    struct ScriptedReader {
        steps: VecDeque<ReadStep>,
    }

    impl ScriptedReader {
        fn new(steps: Vec<ReadStep>) -> Self {
            Self {
                steps: steps.into(),
            }
        }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let Some(step) = self.steps.front_mut() else {
                return Ok(0);
            };
            match step {
                ReadStep::Bytes(bytes) => {
                    let count = bytes.len().min(buf.len());
                    buf[..count].copy_from_slice(&bytes[..count]);
                    bytes.drain(..count);
                    if bytes.is_empty() {
                        self.steps.pop_front();
                    }
                    Ok(count)
                }
                ReadStep::Error(kind) => {
                    let kind = *kind;
                    self.steps.pop_front();
                    Err(std::io::Error::from(kind))
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    struct RecordingKiller {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RecordingKiller {
        fn new() -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    impl portable_pty::ChildKiller for RecordingKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    struct TestMasterPty;

    impl portable_pty::MasterPty for TestMasterPty {
        fn resize(&self, _size: PtySize) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(PtySize::default())
        }

        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
            Ok(Box::new(std::io::empty()))
        }

        fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
            Ok(Box::new(std::io::sink()))
        }

        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            None
        }

        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }

        #[cfg(unix)]
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    struct FailOnceBacking {
        bytes: Vec<u8>,
        fail_flush: bool,
    }

    impl Write for FailOnceBacking {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                self.fail_flush = false;
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "injected flush failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    impl crate::services::terminal::term::DurableScrollbackWriter for FailOnceBacking {
        fn len(&self) -> std::io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn set_len(&mut self, len: u64) -> std::io::Result<()> {
            self.bytes.truncate(len as usize);
            Ok(())
        }

        fn sync_all(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct DurabilityProbeBacking {
        bytes: Vec<u8>,
        fail_sync: std::cell::Cell<bool>,
        mismatch_after_sync: std::cell::Cell<bool>,
        mismatch_next_len: std::cell::Cell<bool>,
        fail_set_len: bool,
    }

    impl Write for DurabilityProbeBacking {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl crate::services::terminal::term::DurableScrollbackWriter for DurabilityProbeBacking {
        fn len(&self) -> std::io::Result<u64> {
            let extra = if self.mismatch_next_len.replace(false) {
                1
            } else {
                0
            };
            Ok(self.bytes.len() as u64 + extra)
        }

        fn set_len(&mut self, len: u64) -> std::io::Result<()> {
            if self.fail_set_len {
                self.fail_set_len = false;
                return Err(std::io::Error::other("injected rollback failure"));
            }
            self.bytes.truncate(len as usize);
            Ok(())
        }

        fn sync_all(&self) -> std::io::Result<()> {
            if self.fail_sync.replace(false) {
                return Err(std::io::Error::other("injected sync failure"));
            }
            if self.mismatch_after_sync.replace(false) {
                self.mismatch_next_len.set(true);
            }
            Ok(())
        }
    }

    fn test_child_process() -> Arc<ChildProcess> {
        let (killer, _) = RecordingKiller::new();
        Arc::new(ChildProcess::new(None, Box::new(killer)))
    }

    fn wait_for_exit_messages(
        bridge: &AsyncBridge,
        timeout: Duration,
    ) -> Vec<crate::services::async_bridge::AsyncMessage> {
        let deadline = Instant::now() + timeout;
        let mut messages = Vec::new();
        loop {
            messages.extend(bridge.try_recv_all());
            if messages.iter().any(|message| {
                matches!(
                    message,
                    crate::services::async_bridge::AsyncMessage::TerminalExited { .. }
                )
            }) {
                return messages;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for TerminalExited"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn wait_for_process_gone(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let result = unsafe { libc::kill(pid as i32, 0) };
            if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    struct ProcessCleanup {
        pid: Option<u32>,
        group: bool,
    }

    #[cfg(unix)]
    impl ProcessCleanup {
        fn process(pid: u32) -> Self {
            Self {
                pid: Some(pid),
                group: false,
            }
        }

        fn group(pid: u32) -> Self {
            Self {
                pid: Some(pid),
                group: true,
            }
        }

        fn disarm(&mut self) {
            self.pid = None;
        }
    }

    #[cfg(unix)]
    impl Drop for ProcessCleanup {
        fn drop(&mut self) {
            let Some(pid) = self.pid else {
                return;
            };
            let target = if self.group {
                -(pid as i32)
            } else {
                pid as i32
            };
            unsafe {
                libc::kill(target, libc::SIGKILL);
            }
        }
    }

    fn test_route(
        window: fresh_core::WindowId,
        terminal: TerminalId,
        bridge: Option<AsyncBridge>,
    ) -> SharedTerminalRoute {
        Arc::new(Mutex::new(TerminalRoute {
            terminal: fresh_core::WindowTerminalId::new(window, terminal),
            bridge,
            state: TerminalRouteState::Live,
        }))
    }

    fn test_handle(
        route: SharedTerminalRoute,
        companion: Option<Arc<OmpCompanionLiveState>>,
    ) -> (
        TerminalHandle,
        mpsc::Sender<TerminalCommand>,
        mpsc::Receiver<TerminalCommand>,
    ) {
        let (command_tx, command_rx) = mpsc::channel();
        (
            TerminalHandle {
                state: Arc::new(Mutex::new(TerminalState::new(80, 4))),
                command_tx: command_tx.clone(),
                alive: Arc::new(AtomicBool::new(true)),
                cols: 80,
                rows: 4,
                cwd: None,
                shell: "test-shell".to_string(),
                pid: None,
                child_process: test_child_process(),
                master_fd: None,
                route,
                companion,
                script_capability: None,
            },
            command_tx,
            command_rx,
        )
    }

    fn test_reader_loop(
        chunks: Vec<Vec<u8>>,
        companion: Option<Arc<OmpCompanionLiveState>>,
        bridge: AsyncBridge,
        log_path: &std::path::Path,
        backing_path: &std::path::Path,
    ) -> (
        Arc<Mutex<TerminalState>>,
        Arc<ExitCoordinator>,
        Arc<AtomicBool>,
    ) {
        test_reader(
            Box::new(ChunkReader::new(chunks)),
            companion,
            bridge,
            log_path,
            backing_path,
        )
    }

    fn test_reader(
        reader: Box<dyn Read + Send>,
        companion: Option<Arc<OmpCompanionLiveState>>,
        bridge: AsyncBridge,
        log_path: &std::path::Path,
        backing_path: &std::path::Path,
    ) -> (
        Arc<Mutex<TerminalState>>,
        Arc<ExitCoordinator>,
        Arc<AtomicBool>,
    ) {
        let terminal_id = TerminalId(7);
        let route = test_route(fresh_core::WindowId(9), terminal_id, Some(bridge.clone()));
        let (response_tx, _response_rx) = mpsc::channel();
        let reader_cancel = ReaderCancellation::detached();
        let exit_coordinator = Arc::new(ExitCoordinator::new(
            route.clone(),
            terminal_id,
            companion.clone(),
            reader_cancel.clone(),
        ));
        let state = Arc::new(Mutex::new(TerminalState::new(80, 4)));
        let alive = Arc::new(AtomicBool::new(true));
        let (log_writer, backing_writer) =
            open_transcript_writers(Some(log_path), Some(backing_path), BackingMode::Fresh)
                .unwrap();
        ReaderLoop {
            reader,
            reader_cancel,
            child_process: test_child_process(),
            state: state.clone(),
            response_tx,
            backing_writer,
            log_writer,
            route,
            terminal_id,
            alive: alive.clone(),
            companion,
            filtered_output: Vec::with_capacity(4096),
            private_boundaries: Vec::new(),
            exit_coordinator: exit_coordinator.clone(),
        }
        .run();
        (state, exit_coordinator, alive)
    }

    #[test]
    fn short_scrollback_flush_advances_the_append_only_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let backing_path = directory.path().join("history.txt");
        let durable = b"durable-history\n";
        std::fs::write(&backing_path, durable).unwrap();

        let mut state = TerminalState::new(20, 1);
        state.set_backing_file_history_end(durable.len() as u64);
        state.process_output(b"short\r\nnext");
        let (_, mut writer) =
            open_transcript_writers(None, Some(&backing_path), BackingMode::Continue).unwrap();
        let mut writer = writer.take().unwrap();
        assert!(state.persist_new_scrollback(&mut writer).unwrap() > 0);

        let durable_end = std::fs::metadata(&backing_path).unwrap().len();
        assert_eq!(state.backing_file_history_end(), durable_end);
        let persisted = String::from_utf8(std::fs::read(&backing_path).unwrap()).unwrap();
        assert!(persisted.contains("durable-history"));
        assert!(persisted.contains("short"));
    }

    #[test]
    fn backing_flush_failure_keeps_scrollback_uncommitted_and_retryable() {
        let durable = b"durable-history\n";
        let mut writer = FailOnceBacking {
            bytes: durable.to_vec(),
            fail_flush: true,
        };
        let mut state = TerminalState::new(20, 1);
        state.set_backing_file_history_end(durable.len() as u64);
        state.process_output(b"retry-me\r\nnext");

        assert!(state.persist_new_scrollback(&mut writer).is_err());
        assert_eq!(state.synced_history_lines(), 0);
        assert_eq!(state.backing_file_history_end(), durable.len() as u64);
        assert_eq!(writer.bytes, durable);

        assert!(state.persist_new_scrollback(&mut writer).unwrap() > 0);
        assert!(state.synced_history_lines() > 0);
        assert_eq!(state.backing_file_history_end(), writer.bytes.len() as u64);
        assert!(String::from_utf8(writer.bytes)
            .unwrap()
            .contains("retry-me"));
    }

    #[test]
    fn backing_sync_failure_rolls_back_without_advancing_the_cursor() {
        let durable = b"durable-history\n";
        let mut writer = DurabilityProbeBacking {
            bytes: durable.to_vec(),
            fail_sync: std::cell::Cell::new(true),
            mismatch_after_sync: std::cell::Cell::new(false),
            mismatch_next_len: std::cell::Cell::new(false),
            fail_set_len: false,
        };
        let mut state = TerminalState::new(20, 1);
        state.set_backing_file_history_end(durable.len() as u64);
        state.process_output(b"retry-sync\r\nnext");

        assert!(state.persist_new_scrollback(&mut writer).is_err());
        assert_eq!(state.synced_history_lines(), 0);
        assert_eq!(writer.bytes, durable);
        assert!(state.persist_new_scrollback(&mut writer).unwrap() > 0);
        assert_eq!(
            String::from_utf8(writer.bytes)
                .unwrap()
                .matches("retry-sync")
                .count(),
            1
        );
    }

    #[test]
    fn post_sync_length_mismatch_rolls_back_and_retries_once() {
        let durable = b"durable-history\n";
        let mut writer = DurabilityProbeBacking {
            bytes: durable.to_vec(),
            fail_sync: std::cell::Cell::new(false),
            mismatch_after_sync: std::cell::Cell::new(true),
            mismatch_next_len: std::cell::Cell::new(false),
            fail_set_len: false,
        };
        let mut state = TerminalState::new(20, 1);
        state.set_backing_file_history_end(durable.len() as u64);
        state.process_output(b"verify-length\r\nnext");

        assert!(state.persist_new_scrollback(&mut writer).is_err());
        assert_eq!(state.synced_history_lines(), 0);
        assert_eq!(writer.bytes, durable);
        assert!(state.persist_new_scrollback(&mut writer).unwrap() > 0);
        assert_eq!(
            String::from_utf8(writer.bytes)
                .unwrap()
                .matches("verify-length")
                .count(),
            1
        );
    }

    #[test]
    fn failed_rollback_fences_the_next_retry_before_reappending() {
        let durable = b"durable-history\n";
        let mut writer = DurabilityProbeBacking {
            bytes: durable.to_vec(),
            fail_sync: std::cell::Cell::new(true),
            mismatch_after_sync: std::cell::Cell::new(false),
            mismatch_next_len: std::cell::Cell::new(false),
            fail_set_len: true,
        };
        let mut state = TerminalState::new(20, 1);
        state.set_backing_file_history_end(durable.len() as u64);
        state.process_output(b"rollback-retry\r\nnext");

        assert!(state.persist_new_scrollback(&mut writer).is_err());
        assert_eq!(state.synced_history_lines(), 0);
        assert_eq!(state.backing_file_history_end(), durable.len() as u64);
        assert!(writer.bytes.len() > durable.len());

        assert!(state.persist_new_scrollback(&mut writer).unwrap() > 0);
        assert_eq!(state.backing_file_history_end(), writer.bytes.len() as u64);
        assert_eq!(
            String::from_utf8(writer.bytes)
                .unwrap()
                .matches("rollback-retry")
                .count(),
            1
        );
    }

    #[test]
    fn reader_excludes_private_only_bytes_from_emulator_files_and_output_events() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let frame = output_frame(&secret, br#"{"version":1,"type":"snapshot"}"#);
        let split_a = 13;
        let split_b = frame.len() - 5;
        let chunks = vec![
            frame[..split_a].to_vec(),
            frame[split_a..split_b].to_vec(),
            frame[split_b..].to_vec(),
        ];
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("raw.log");
        let backing_path = directory.path().join("backing.txt");
        let bridge = AsyncBridge::new();
        let (state, _, alive) = test_reader_loop(
            chunks,
            Some(companion.clone()),
            bridge.clone(),
            &log_path,
            &backing_path,
        );

        assert!(!alive.load(Ordering::Acquire));
        assert_eq!(std::fs::read(&log_path).unwrap(), b"");
        assert_eq!(std::fs::read(&backing_path).unwrap(), b"");
        let emulator_content = match state.lock() {
            Ok(state) => state.content_string(),
            Err(poisoned) => poisoned.into_inner().content_string(),
        };
        assert!(!emulator_content.contains("fresh-omp"));
        assert_eq!(companion.take_candidate(), Some(frame));
        let messages = bridge.try_recv_all();
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::OmpCompanionSnapshotReady {
                terminal
            } if *terminal == fresh_core::WindowTerminalId::new(
                fresh_core::WindowId(9),
                fresh_core::TerminalId(7),
            )
        ));
    }

    #[test]
    fn reader_forwards_only_surrounding_public_bytes() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let frame = output_frame(&secret, b"snapshot");
        let mut chunk = b"VISIBLE-BEFORE".to_vec();
        chunk.extend_from_slice(&frame);
        chunk.extend_from_slice(b"-VISIBLE-AFTER");
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("raw.log");
        let backing_path = directory.path().join("backing.txt");
        let bridge = AsyncBridge::new();
        let (state, _, _) = test_reader_loop(
            vec![chunk],
            Some(companion),
            bridge.clone(),
            &log_path,
            &backing_path,
        );

        assert_eq!(
            std::fs::read(&log_path).unwrap(),
            b"VISIBLE-BEFORE-VISIBLE-AFTER"
        );
        let content = match state.lock() {
            Ok(state) => state.content_string(),
            Err(poisoned) => poisoned.into_inner().content_string(),
        };
        assert!(content.contains("VISIBLE-BEFORE-VISIBLE-AFTER"));
        assert!(!content.contains("omp-companion"));
        let messages = bridge.try_recv_all();
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(
                    message,
                    crate::services::async_bridge::AsyncMessage::TerminalOutput { .. }
                ))
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(
                    message,
                    crate::services::async_bridge::AsyncMessage::OmpCompanionSnapshotReady { .. }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn private_frame_preserves_split_public_csi_stream() {
        let secret = test_secret();
        let frame = output_frame(&secret, b"snapshot");
        let mut stream = b"\x1b[31".to_vec();
        stream.extend_from_slice(&frame);
        stream.extend_from_slice(b"mX");
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("raw.log");
        let backing_path = directory.path().join("backing.txt");
        let bridge = AsyncBridge::new();
        let (state, _, _) = test_reader_loop(
            vec![stream],
            Some(test_companion(secret)),
            bridge,
            &log_path,
            &backing_path,
        );

        assert_eq!(std::fs::read(&log_path).unwrap(), b"\x1b[31mX");
        let line = match state.lock() {
            Ok(state) => state.get_line(0),
            Err(poisoned) => poisoned.into_inner().get_line(0),
        };
        let mut expected = TerminalState::new(80, 4);
        expected.process_output(b"\x1b[31mX");
        let expected_line = expected.get_line(0);
        assert_eq!(
            (line[0].c, line[0].fg),
            (expected_line[0].c, expected_line[0].fg)
        );
    }

    #[test]
    fn private_frame_preserves_split_public_utf8_stream() {
        let secret = test_secret();
        let frame = output_frame(&secret, b"snapshot");
        let mut stream = vec![0xe2];
        stream.extend_from_slice(&frame);
        stream.extend_from_slice(&[0x82, 0xac, b'X']);
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("raw.log");
        let backing_path = directory.path().join("backing.txt");
        let bridge = AsyncBridge::new();
        let (state, _, _) = test_reader_loop(
            vec![stream],
            Some(test_companion(secret)),
            bridge,
            &log_path,
            &backing_path,
        );

        assert_eq!(std::fs::read(&log_path).unwrap(), [0xe2, 0x82, 0xac, b'X']);
        let content = match state.lock() {
            Ok(state) => state.content_string(),
            Err(poisoned) => poisoned.into_inner().content_string(),
        };
        assert!(content.contains('€'));
        assert!(content.contains('X'));
    }

    #[test]
    fn interrupted_read_keeps_split_private_frame_filtered_and_public_tail() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let frame = output_frame(&secret, b"interrupted-private");
        let split = OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 3;
        let mut tail = frame[split..].to_vec();
        tail.extend_from_slice(b"PUBLIC-TAIL");
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("raw.log");
        let backing_path = directory.path().join("backing.txt");
        let bridge = AsyncBridge::new();
        let (state, _, _) = test_reader(
            Box::new(ScriptedReader::new(vec![
                ReadStep::Bytes(frame[..split].to_vec()),
                ReadStep::Error(std::io::ErrorKind::Interrupted),
                ReadStep::Bytes(tail),
            ])),
            Some(companion.clone()),
            bridge,
            &log_path,
            &backing_path,
        );

        assert_eq!(std::fs::read(&log_path).unwrap(), b"PUBLIC-TAIL");
        let content = match state.lock() {
            Ok(state) => state.content_string(),
            Err(poisoned) => poisoned.into_inner().content_string(),
        };
        assert!(content.contains("PUBLIC-TAIL"));
        assert!(!content.contains("fresh-omp"));
        assert_eq!(companion.take_candidate(), Some(frame));
    }

    #[test]
    fn revoked_split_candidate_stays_filtered_until_reader_drain_and_exit_barrier() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let frame = output_frame(&secret, b"split-after-manual-close");
        let split = OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 1 + 5;
        let mut visible = Vec::new();
        let mut boundaries = Vec::new();
        assert!(!companion.filter_output_into(
            &frame[..split],
            Instant::now(),
            &mut visible,
            &mut boundaries,
        ));
        assert!(visible.is_empty());
        assert_eq!(boundaries, [0]);

        companion.revoke_access();
        assert!(!companion.test_output_filter_finalized());

        let mut remaining = frame[split..].to_vec();
        remaining.extend_from_slice(b"ordinary-after-close");
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("raw.log");
        let backing_path = directory.path().join("backing.txt");
        let bridge = AsyncBridge::new();
        let (state, coordinator, alive) = test_reader_loop(
            vec![remaining],
            Some(companion.clone()),
            bridge.clone(),
            &log_path,
            &backing_path,
        );

        assert!(!alive.load(Ordering::Acquire));
        assert_eq!(std::fs::read(&log_path).unwrap(), b"ordinary-after-close");
        let content = match state.lock() {
            Ok(state) => state.content_string(),
            Err(poisoned) => poisoned.into_inner().content_string(),
        };
        assert!(content.contains("ordinary-after-close"));
        assert!(!content.contains("fresh-omp"));
        assert!(companion.take_candidate().is_none());
        assert!(!companion.test_output_filter_finalized());

        let messages = bridge.try_recv_all();
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::TerminalOutput { .. }
        ));

        coordinator.child_finished(Some(0));
        assert!(companion.test_output_filter_finalized());
        let messages = bridge.try_recv_all();
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::TerminalExited {
                exit_code: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn command_and_snapshot_ack_frames_are_private_exact_and_report_admission() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let target = command_target();
        admit_command_target(&companion, &target);
        let (command_tx, command_rx) = mpsc::channel();
        let handle = TerminalHandle {
            state: Arc::new(Mutex::new(TerminalState::new(80, 4))),
            command_tx,
            alive: Arc::new(AtomicBool::new(true)),
            cols: 80,
            rows: 4,
            cwd: None,
            shell: "omp".to_string(),
            pid: None,
            child_process: test_child_process(),
            master_fd: None,
            route: test_route(fresh_core::WindowId(1), TerminalId(2), None),
            companion: Some(companion.clone()),
            script_capability: None,
        };

        assert_eq!(handle.companion_kind(), Some(TerminalCompanion::Omp));
        assert!(handle.enqueue_omp_companion_command(OmpCompanionCommandType::Cancel, &target));
        let TerminalCommand::Write(frame) = command_rx.recv().unwrap() else {
            panic!("companion command must be admitted as a PTY write");
        };
        assert!(frame.starts_with("\u{10ffff}fresh-omp-command:v1:".as_bytes()));
        assert!(frame.ends_with("\u{10fffe}".as_bytes()));
        assert!(!frame
            .windows(secret.len())
            .any(|window| window == secret.as_slice()));
        let payload =
            &frame["\u{10ffff}fresh-omp-command:v1:".len()..frame.len() - "\u{10fffe}".len()];
        let separator = payload.iter().position(|byte| *byte == b'.').unwrap();
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&payload[..separator])
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "version": 1,
                "type": "cancel",
                "incarnation": target.incarnation,
                "sessionGeneration": target.session_generation,
                "sessionId": target.session_id,
                "workEpoch": target.work_epoch,
                "commandSequence": 1,
            })
        );
        assert_eq!(payload[separator + 1..].len(), OMP_TAG_B64_LEN);

        let mut snapshot = snapshot_for_target(&target);
        snapshot.sequence += 1;
        assert!(companion.admit_sequence(
            &snapshot.incarnation,
            snapshot.sequence,
            snapshot.session_generation,
            &snapshot.session_id,
            snapshot.work_epoch,
        ));
        assert!(handle.enqueue_omp_companion_snapshot_ack(&snapshot, true));
        let TerminalCommand::Write(frame) = command_rx.recv().unwrap() else {
            panic!("snapshot acknowledgement must be admitted as a PTY write");
        };
        let payload =
            &frame["\u{10ffff}fresh-omp-command:v1:".len()..frame.len() - "\u{10fffe}".len()];
        let separator = payload.iter().position(|byte| *byte == b'.').unwrap();
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&payload[..separator])
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            format!(
                r#"{{"version":1,"type":"snapshot_ack","incarnation":"{}","sequence":2,"sessionGeneration":3,"sessionId":"{}","workEpoch":9,"accepted":true,"commandSequence":2}}"#,
                target.incarnation, target.session_id
            )
        );
        assert!(companion.commit_admitted_snapshot(&snapshot, false));

        let mut rejected = snapshot.clone();
        rejected.sequence += 1;
        assert!(companion.admit_sequence(
            &rejected.incarnation,
            rejected.sequence,
            rejected.session_generation,
            &rejected.session_id,
            rejected.work_epoch,
        ));
        assert!(handle.enqueue_omp_companion_snapshot_ack(&rejected, false));
        let TerminalCommand::Write(frame) = command_rx.recv().unwrap() else {
            panic!("negative snapshot acknowledgement must be admitted as a PTY write");
        };
        let payload =
            &frame["\u{10ffff}fresh-omp-command:v1:".len()..frame.len() - "\u{10fffe}".len()];
        let separator = payload.iter().position(|byte| *byte == b'.').unwrap();
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&payload[..separator])
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            format!(
                r#"{{"version":1,"type":"snapshot_ack","incarnation":"{}","sequence":3,"sessionGeneration":3,"sessionId":"{}","workEpoch":9,"accepted":false,"commandSequence":3}}"#,
                target.incarnation, target.session_id
            )
        );

        let mut stale = rejected;
        stale.sequence += 1;
        assert!(!handle.enqueue_omp_companion_snapshot_ack(&stale, true));
        assert!(command_rx.try_recv().is_err());

        handle.alive.store(false, Ordering::Release);
        assert!(!handle
            .enqueue_omp_companion_command(OmpCompanionCommandType::RequestSnapshot, &target,));
        handle.alive.store(true, Ordering::Release);

        companion.revoke_access();
        assert!(!handle
            .enqueue_omp_companion_command(OmpCompanionCommandType::RequestSnapshot, &target,));
        drop(command_rx);
        assert!(!handle.enqueue_omp_companion_command(OmpCompanionCommandType::Cancel, &target));
    }

    #[test]
    fn command_admission_is_false_when_writer_channel_is_disconnected() {
        let companion = test_companion(test_secret());
        let target = command_target();
        admit_command_target(&companion, &target);
        let (command_tx, command_rx) = mpsc::channel();
        drop(command_rx);
        let handle = TerminalHandle {
            state: Arc::new(Mutex::new(TerminalState::new(80, 4))),
            command_tx,
            alive: Arc::new(AtomicBool::new(true)),
            cols: 80,
            rows: 4,
            cwd: None,
            shell: "omp".to_string(),
            pid: None,
            child_process: test_child_process(),
            master_fd: None,
            route: test_route(fresh_core::WindowId(1), TerminalId(2), None),
            companion: Some(companion),
            script_capability: None,
        };
        assert!(!handle.enqueue_omp_companion_command(OmpCompanionCommandType::Cancel, &target));
    }

    #[test]
    fn clean_writer_disconnect_after_reap_does_not_signal_child() {
        let (killer, calls) = RecordingKiller::new();
        let child_process = ChildProcess::new(None, Box::new(killer));
        child_process.mark_reaped();
        let (command_tx, command_rx) = mpsc::channel();
        drop(command_tx);

        run_writer_loop(
            command_rx,
            Box::new(std::io::sink()),
            Box::new(TestMasterPty),
            &child_process,
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn explicit_shutdown_before_reap_runs_one_bounded_termination_sequence() {
        let (killer, calls) = RecordingKiller::new();
        let child_process = Arc::new(ChildProcess::new(None, Box::new(killer)));
        let reaper = child_process.clone();
        let mark_reaped = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            reaper.mark_reaped();
        });
        let (command_tx, command_rx) = mpsc::channel();
        command_tx.send(TerminalCommand::Shutdown).unwrap();
        drop(command_tx);

        run_writer_loop(
            command_rx,
            Box::new(std::io::sink()),
            Box::new(TestMasterPty),
            &child_process,
        );
        mark_reaped.join().unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn pty_write_failure_requests_the_same_single_termination_sequence() {
        let (killer, calls) = RecordingKiller::new();
        let child_process = Arc::new(ChildProcess::new(None, Box::new(killer)));
        let reaper = child_process.clone();
        let mark_reaped = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            reaper.mark_reaped();
        });
        let (command_tx, command_rx) = mpsc::channel();
        command_tx
            .send(TerminalCommand::Write(b"fail".to_vec()))
            .unwrap();
        drop(command_tx);

        run_writer_loop(
            command_rx,
            Box::new(FailOnceBacking {
                bytes: Vec::new(),
                fail_flush: true,
            }),
            Box::new(TestMasterPty),
            &child_process,
        );
        mark_reaped.join().unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn permanent_read_error_requests_shutdown_before_eof_finalization() {
        let bridge = AsyncBridge::new();
        let terminal_id = TerminalId(12);
        let route = test_route(fresh_core::WindowId(3), terminal_id, Some(bridge.clone()));
        let (response_tx, response_rx) = mpsc::channel();
        let reader_cancel = ReaderCancellation::detached();
        let coordinator = Arc::new(ExitCoordinator::new(
            route.clone(),
            terminal_id,
            None,
            reader_cancel.clone(),
        ));
        let (killer, kill_calls) = RecordingKiller::new();
        let child_process = Arc::new(ChildProcess::new(None, Box::new(killer)));
        let reaper = child_process.clone();
        let mark_reaped = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            reaper.mark_reaped();
        });
        ReaderLoop {
            reader: Box::new(ScriptedReader::new(vec![ReadStep::Error(
                std::io::ErrorKind::BrokenPipe,
            )])),
            reader_cancel,
            child_process,
            state: Arc::new(Mutex::new(TerminalState::new(80, 4))),
            response_tx,
            backing_writer: None,
            log_writer: None,
            route,
            terminal_id,
            alive: Arc::new(AtomicBool::new(true)),
            companion: None,
            filtered_output: Vec::new(),
            private_boundaries: Vec::new(),
            exit_coordinator: coordinator.clone(),
        }
        .run();
        mark_reaped.join().unwrap();
        assert_eq!(kill_calls.load(Ordering::Acquire), 1);

        assert!(matches!(
            response_rx.recv().unwrap(),
            TerminalCommand::Shutdown
        ));
        assert!(bridge.try_recv_all().is_empty());
        coordinator.child_finished(Some(5));
        let messages = bridge.try_recv_all();
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::TerminalExited {
                exit_code: Some(5),
                ..
            }
        ));
        coordinator.child_finished(Some(99));
        coordinator.reader_drained();
        assert!(bridge.try_recv_all().is_empty());
    }

    #[test]
    fn exit_is_single_and_waits_for_reader_final_flush() {
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("raw.log");
        let backing_path = directory.path().join("backing.txt");
        let bridge = AsyncBridge::new();
        let terminal_id = TerminalId(7);
        let route = test_route(fresh_core::WindowId(9), terminal_id, Some(bridge.clone()));
        let (response_tx, _response_rx) = mpsc::channel();
        let reader_cancel = ReaderCancellation::detached();
        let coordinator = Arc::new(ExitCoordinator::new(
            route.clone(),
            terminal_id,
            None,
            reader_cancel.clone(),
        ));
        coordinator.child_finished(Some(23));
        assert!(bridge.try_recv_all().is_empty());

        let state = Arc::new(Mutex::new(TerminalState::new(80, 4)));
        let alive = Arc::new(AtomicBool::new(true));
        let (log_writer, backing_writer) =
            open_transcript_writers(Some(&log_path), Some(&backing_path), BackingMode::Fresh)
                .unwrap();
        ReaderLoop {
            reader: Box::new(ChunkReader::new(vec![
                b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5\r\n".to_vec(),
            ])),
            reader_cancel,
            child_process: test_child_process(),
            state,
            response_tx,
            backing_writer,
            log_writer,
            route,
            terminal_id,
            alive: alive.clone(),
            companion: None,
            filtered_output: Vec::new(),
            private_boundaries: Vec::new(),

            exit_coordinator: coordinator.clone(),
        }
        .run();

        assert!(!alive.load(Ordering::Acquire));
        assert_eq!(
            std::fs::read(&log_path).unwrap(),
            b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5\r\n"
        );
        let backing = String::from_utf8(std::fs::read(&backing_path).unwrap()).unwrap();
        assert!(backing.contains("line-0"));
        let messages = bridge.try_recv_all();
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::TerminalOutput { .. }
        ));
        assert!(matches!(
            &messages[1],
            crate::services::async_bridge::AsyncMessage::TerminalExited {
                exit_code: Some(23),
                ..
            }
        ));
        coordinator.reader_drained();
        coordinator.child_finished(Some(99));
        assert!(bridge.try_recv_all().is_empty());
    }
    #[test]
    fn root_child_exit_revokes_script_capability_before_reader_drain() {
        let token = crate::server::command_access::mint(crate::server::command_access::Grant::new(
            Some(4),
            true,
        ));
        let capability = TerminalScriptCapability::new(token.clone());
        let terminal_id = TerminalId(3);
        let route = test_route(fresh_core::WindowId(4), terminal_id, None);
        let coordinator = ExitCoordinator::with_script_capability(
            route,
            terminal_id,
            None,
            ReaderCancellation::detached(),
            Some(capability),
        );

        assert!(crate::server::command_access::lookup(&token).is_some());
        coordinator.child_finished(Some(0));
        assert!(crate::server::command_access::lookup(&token).is_none());
        coordinator.child_finished(Some(0));
        assert!(crate::server::command_access::lookup(&token).is_none());
    }

    #[test]
    fn exit_barrier_revokes_commands_but_preserves_ready_candidate_when_reader_finishes_first() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let target = command_target();
        admit_command_target(&companion, &target);
        assert!(companion
            .frame_command_if_active(
                OmpCompanionCommandType::RequestSnapshot,
                &target,
                &AtomicBool::new(true),
            )
            .is_some());
        let frame = output_frame(&secret, b"completed-before-exit");
        let mut visible = Vec::new();
        let mut boundaries = Vec::new();
        assert!(companion.filter_output_into(
            &frame,
            Instant::now(),
            &mut visible,
            &mut boundaries,
        ));

        let bridge = AsyncBridge::new();
        let terminal_id = TerminalId(3);
        let route = test_route(fresh_core::WindowId(4), terminal_id, Some(bridge.clone()));
        let coordinator = ExitCoordinator::new(
            route,
            terminal_id,
            Some(companion.clone()),
            ReaderCancellation::detached(),
        );
        coordinator.reader_drained();
        assert!(bridge.try_recv_all().is_empty());
        coordinator.child_finished(None);
        assert!(companion
            .frame_command_if_active(
                OmpCompanionCommandType::RequestSnapshot,
                &target,
                &AtomicBool::new(true),
            )
            .is_none());
        assert_eq!(companion.take_candidate(), Some(frame));
        assert!(companion.verify_output_auth(
            &omp_companion_synchronizer(&secret),
            b"body",
            &omp_companion_output_tag(&secret, b"body")
        ));
        let messages = bridge.try_recv_all();
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::TerminalExited {
                exit_code: None,
                ..
            }
        ));
    }

    #[test]
    fn reader_drain_timeout_forces_eof_but_late_private_bytes_stay_filtered() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let frame = output_frame(&secret, b"late-private-after-timeout");
        let split = OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 1 + 4;
        let mut visible = Vec::new();
        let mut boundaries = Vec::new();
        assert!(!companion.filter_output_into(
            &frame[..split],
            Instant::now(),
            &mut visible,
            &mut boundaries,
        ));
        assert!(visible.is_empty());
        assert_eq!(boundaries, [0]);

        let bridge = AsyncBridge::new();
        let terminal_id = TerminalId(8);
        let route = test_route(fresh_core::WindowId(5), terminal_id, Some(bridge.clone()));
        let reader_cancel = ReaderCancellation::detached();
        let coordinator = ExitCoordinator::new(
            route,
            terminal_id,
            Some(companion.clone()),
            reader_cancel.clone(),
        );

        coordinator.child_finished(Some(17));
        assert!(bridge.try_recv_all().is_empty());
        coordinator.reader_drain_timed_out();
        assert!(bridge.try_recv_all().is_empty());
        assert!(!companion.test_output_filter_finalized());
        assert!(reader_cancel.is_cancelled());

        let mut late = frame[split..].to_vec();
        late.extend_from_slice(b"ordinary-after-timeout");
        assert!(companion.filter_output_into(&late, Instant::now(), &mut visible, &mut boundaries,));
        assert_eq!(visible, b"ordinary-after-timeout");
        assert!(boundaries.is_empty());
        assert_eq!(companion.take_candidate(), Some(frame));
        assert!(!companion.test_output_filter_finalized());

        coordinator.reader_drained();
        let messages = bridge.try_recv_all();
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::TerminalExited {
                exit_code: Some(17),
                ..
            }
        ));
        assert!(companion.test_output_filter_finalized());

        coordinator.reader_drain_timed_out();
        coordinator.child_finished(Some(99));
        assert!(bridge.try_recv_all().is_empty());
    }

    #[test]
    fn adoption_routes_output_and_exit_to_target_after_source_closes() {
        let source_bridge = AsyncBridge::new();
        let target_bridge = AsyncBridge::new();
        let terminal_id = TerminalId(4);
        let route = test_route(
            fresh_core::WindowId(1),
            terminal_id,
            Some(source_bridge.clone()),
        );
        let (handle, _command_tx, _command_rx) = test_handle(route.clone(), None);
        let coordinator = Arc::new(ExitCoordinator::new(
            route.clone(),
            terminal_id,
            None,
            ReaderCancellation::detached(),
        ));
        let mut source = TerminalManager::new(fresh_core::WindowId(1));
        source.set_async_bridge(source_bridge.clone());
        source.terminals.insert(terminal_id, handle);
        let handle = source.release(terminal_id).unwrap();

        let mut target = TerminalManager::new(fresh_core::WindowId(2));
        target.set_async_bridge(target_bridge.clone());
        let adopted_id = match target.adopt(handle) {
            Ok(id) => id,
            Err(_) => panic!("live route should be adopted"),
        };
        drop(source);

        let output_route = route.clone();
        let output_coordinator = coordinator.clone();
        std::thread::spawn(move || {
            let (terminal, sender) = route_destination(&output_route).unwrap();
            sender
                .send(crate::services::async_bridge::AsyncMessage::TerminalOutput { terminal })
                .unwrap();
            output_coordinator.child_finished(Some(0));
            output_coordinator.reader_drained();
        })
        .join()
        .unwrap();

        assert!(source_bridge.try_recv_all().is_empty());
        let messages = target_bridge.try_recv_all();
        assert_eq!(messages.len(), 2);
        let target_identity =
            fresh_core::WindowTerminalId::new(fresh_core::WindowId(2), adopted_id);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::TerminalOutput { terminal }
                if *terminal == target_identity
        ));
        assert!(matches!(
            &messages[1],
            crate::services::async_bridge::AsyncMessage::TerminalExited { terminal, .. }
                if *terminal == target_identity
        ));
    }

    #[test]
    fn exit_claimed_before_adoption_stays_on_source_route() {
        let source_bridge = AsyncBridge::new();
        let target_bridge = AsyncBridge::new();
        let terminal_id = TerminalId(4);
        let route = test_route(
            fresh_core::WindowId(1),
            terminal_id,
            Some(source_bridge.clone()),
        );
        let (handle, _command_tx, _command_rx) = test_handle(route.clone(), None);
        let coordinator =
            ExitCoordinator::new(route, terminal_id, None, ReaderCancellation::detached());
        coordinator.child_finished(Some(9));
        coordinator.reader_drained();

        let mut target = TerminalManager::new(fresh_core::WindowId(2));
        target.set_async_bridge(target_bridge.clone());
        let handle = target
            .adopt(handle)
            .expect_err("claimed exit must win adoption race");
        let mut source = TerminalManager::new(fresh_core::WindowId(1));
        assert!(source.restore_released(terminal_id, handle).is_ok());

        assert!(target_bridge.try_recv_all().is_empty());
        let messages = source_bridge.try_recv_all();
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            crate::services::async_bridge::AsyncMessage::TerminalExited {
                terminal,
                exit_code: Some(9),
            } if *terminal == fresh_core::WindowTerminalId::new(
                fresh_core::WindowId(1),
                terminal_id,
            )
        ));
        assert!(source.get(terminal_id).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_shutdown_hard_kills_sighup_ignoring_root_once() {
        let bridge = AsyncBridge::new();
        let mut manager = TerminalManager::new(fresh_core::WindowId(70));
        manager.set_async_bridge(bridge.clone());
        let terminal_id = manager
            .spawn(
                80,
                4,
                None,
                None,
                None,
                BackingMode::Fresh,
                TerminalWrapper {
                    command: "/bin/sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "trap '' HUP; printf READY; while :; do sleep 1; done".to_string(),
                    ],
                    manages_cwd: false,
                },
                crate::services::env_provider::EnvDelta::default(),
                HashMap::new(),
                None,
                None,
            )
            .unwrap();
        let handle = manager.get(terminal_id).unwrap();
        let pid = handle.pid().unwrap();
        let alive = handle.alive.clone();
        let state = handle.state.clone();
        let mut cleanup = ProcessCleanup::group(pid);
        let ready_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let ready = state
                .lock()
                .map(|state| state.content_string().contains("READY"))
                .unwrap_or(false);
            if ready {
                break;
            }
            assert!(
                Instant::now() < ready_deadline,
                "PTY root never became ready"
            );
            thread::sleep(Duration::from_millis(10));
        }

        assert!(manager.close(terminal_id));
        let mut messages = wait_for_exit_messages(&bridge, Duration::from_secs(3));
        assert!(!alive.load(Ordering::Acquire));
        assert!(manager.reap(terminal_id));
        thread::sleep(Duration::from_millis(100));
        messages.extend(bridge.try_recv_all());
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(
                    message,
                    crate::services::async_bridge::AsyncMessage::TerminalExited { .. }
                ))
                .count(),
            1
        );
        assert!(wait_for_process_gone(pid, Duration::from_secs(1)));
        cleanup.disarm();
    }

    #[cfg(unix)]
    #[test]
    fn root_exit_cancels_reader_when_descendant_retains_slave_once() {
        let directory = tempfile::tempdir().unwrap();
        let child_pid_path = directory.path().join("descendant.pid");
        let bridge = AsyncBridge::new();
        let mut manager = TerminalManager::new(fresh_core::WindowId(71));
        manager.set_async_bridge(bridge.clone());
        let terminal_id = manager
            .spawn(
                80,
                4,
                None,
                None,
                None,
                BackingMode::Fresh,
                TerminalWrapper {
                    command: "/bin/sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "sleep 30 & echo $! > \"$1\"; exit 0".to_string(),
                        "fresh-test".to_string(),
                        child_pid_path.to_string_lossy().into_owned(),
                    ],
                    manages_cwd: false,
                },
                crate::services::env_provider::EnvDelta::default(),
                HashMap::new(),
                None,
                None,
            )
            .unwrap();
        let alive = manager.get(terminal_id).unwrap().alive.clone();
        let pid_deadline = Instant::now() + Duration::from_secs(2);
        let descendant_pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&child_pid_path) {
                if let Ok(pid) = pid.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                Instant::now() < pid_deadline,
                "descendant pid was not published"
            );
            thread::sleep(Duration::from_millis(10));
        };
        let mut cleanup = ProcessCleanup::process(descendant_pid);

        let mut messages = wait_for_exit_messages(&bridge, Duration::from_secs(2));
        assert!(!alive.load(Ordering::Acquire));
        assert!(manager.reap(terminal_id));
        assert!(wait_for_process_gone(
            descendant_pid,
            Duration::from_secs(1)
        ));
        cleanup.disarm();
        thread::sleep(Duration::from_millis(100));
        messages.extend(bridge.try_recv_all());
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(
                    message,
                    crate::services::async_bridge::AsyncMessage::TerminalExited { .. }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn closed_terminal_remains_tracked_until_exit_is_reaped() {
        let bridge = AsyncBridge::new();
        let terminal_id = TerminalId(6);
        let route = test_route(fresh_core::WindowId(1), terminal_id, Some(bridge.clone()));
        let (handle, _command_tx, command_rx) = test_handle(route, None);
        let mut manager = TerminalManager::new(fresh_core::WindowId(1));
        manager.set_async_bridge(bridge);
        manager.terminals.insert(terminal_id, handle);

        assert!(manager.close(terminal_id));
        assert!(manager.terminal_ids().is_empty());
        assert!(manager.tracked_terminal_ids().contains(&terminal_id));
        assert!(matches!(
            command_rx.recv().unwrap(),
            TerminalCommand::Shutdown
        ));
        assert!(manager.reap(terminal_id));
        assert!(!manager.tracked_terminal_ids().contains(&terminal_id));
    }

    #[test]
    fn test_terminal_id_display() {
        let id = TerminalId(42);
        assert_eq!(format!("{}", id), "Terminal-42");
    }

    /// Terminal ids are per-window: each manager numbers from 0, so two
    /// windows both hand out `Terminal-0`. The owning window is what
    /// disambiguates them — output messages are tagged with the
    /// `(window, terminal)` pair so a `Terminal-0` from one session can't
    /// be attributed to another session's `Terminal-0`. (Regression
    /// guard for the dock "pending output on the wrong session" bug.)
    #[test]
    fn terminal_ids_collide_across_windows_but_window_disambiguates() {
        use fresh_core::{WindowId, WindowTerminalId};

        let win_a = TerminalManager::new(WindowId(1));
        let win_b = TerminalManager::new(WindowId(2));

        // Both managers would assign the same local id to their first
        // terminal — the namespaces are independent.
        assert_eq!(win_a.next_terminal_id(), win_b.next_terminal_id());
        assert_eq!(win_a.next_terminal_id(), TerminalId(0));

        // Each manager knows its owner, so the global identity differs.
        assert_eq!(win_a.window_id(), WindowId(1));
        assert_eq!(win_b.window_id(), WindowId(2));
        let a0 = WindowTerminalId::new(win_a.window_id(), win_a.next_terminal_id());
        let b0 = WindowTerminalId::new(win_b.window_id(), win_b.next_terminal_id());
        assert_ne!(
            a0, b0,
            "same local terminal id in different windows must be distinct globally"
        );
    }

    #[test]
    fn test_detect_shell() {
        let shell = detect_shell();
        assert!(!shell.is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn strip_verbatim_prefix_is_noop_on_unix() {
        use std::path::Path;
        let p = Path::new("/home/user/project");
        assert_eq!(strip_verbatim_prefix(p).as_ref(), p);
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_removes_verbatim_disk() {
        use std::path::{Path, PathBuf};
        let verbatim = PathBuf::from(r"\\?\C:\Users\HP\OneDrive\Desktop\PY'PGMS");
        let stripped = strip_verbatim_prefix(&verbatim);
        assert_eq!(
            stripped.as_ref(),
            Path::new(r"C:\Users\HP\OneDrive\Desktop\PY'PGMS"),
            "verbatim disk prefix should be replaced with plain drive form"
        );
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_removes_verbatim_unc() {
        use std::path::{Path, PathBuf};
        let verbatim = PathBuf::from(r"\\?\UNC\server\share\dir\file");
        let stripped = strip_verbatim_prefix(&verbatim);
        assert_eq!(
            stripped.as_ref(),
            Path::new(r"\\server\share\dir\file"),
            "verbatim UNC prefix should be replaced with plain UNC form"
        );
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_passes_plain_paths_through() {
        use std::path::{Path, PathBuf};
        let plain = PathBuf::from(r"C:\Users\HP\project");
        let result = strip_verbatim_prefix(&plain);
        assert_eq!(result.as_ref(), Path::new(r"C:\Users\HP\project"));
    }
}
