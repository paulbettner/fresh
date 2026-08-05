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
use fresh_core::api::{OmpCompanionCommandType, TerminalCompanion};
pub use fresh_core::TerminalId;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

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
    /// The same terminal continuing (workspace restore, respawn after exit):
    /// append to its transcripts and treat what's there as its own history.
    Continue,
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

/// The `(window, terminal)` identity stamped on this terminal's async
/// messages, shared with the reader and wait threads. A `Mutex` (rather
/// than a plain captured copy) so the tag can be rewritten when another
/// window's manager adopts a live terminal — see
/// [`TerminalManager::adopt`]; the threads read it at each send.
type SharedWtId = Arc<Mutex<fresh_core::WindowTerminalId>>;

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
    /// PTY master file descriptor, captured at spawn. Used to read the
    /// terminal's foreground process group via `tcgetpgrp` for tmux-style
    /// tab auto-naming. `None` on Windows or when the platform doesn't
    /// expose it. Only read on Linux (the only `/proc`-backed target).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    master_fd: Option<i32>,
    /// The identity tag shared with this terminal's reader/wait threads,
    /// rewritten when the handle moves to another window's manager.
    wt_id: SharedWtId,
    /// Live capability shared with the reader. The editor bridge may clone the
    /// Arc to drain and authenticate the latest candidate without exposing the
    /// secret itself.
    pub(crate) companion: Option<Arc<OmpCompanionLiveState>>,
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
    pub fn enqueue_omp_companion_command(&self, command: OmpCompanionCommandType) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        let Some(companion) = &self.companion else {
            return false;
        };
        let Some(frame) = companion.frame_command_if_active(command, &self.alive) else {
            return false;
        };
        self.command_tx.send(TerminalCommand::Write(frame)).is_ok()
    }

    /// Revoke commands, authentication, and candidate delivery immediately.
    /// The reader-owned output filter remains live until its final drain.
    pub fn revoke_omp_companion(&self) {
        if let Some(companion) = &self.companion {
            companion.revoke_access();
        }
    }

    /// Shutdown the terminal
    pub fn shutdown(&self) {
        self.revoke_omp_companion();
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

    /// Send `signal` to the terminal's process group. Returns
    /// `Ok(false)` when the terminal has no recorded pid
    /// (Windows, or platforms where portable_pty didn't report
    /// one) — caller can fall back to `shutdown()` (SIGKILL via
    /// child_killer). The shell is always its own session
    /// leader inside a pty, so `kill(-pid, …)` reaches the
    /// shell *and* any subprocesses it forked.
    ///
    /// Recognised signal names: `"SIGTERM"`, `"SIGKILL"`,
    /// `"SIGINT"`, `"SIGHUP"`. Unknown names return an Err
    /// instead of dropping silently.
    #[cfg(unix)]
    pub fn signal(&self, signal_name: &str) -> Result<bool, String> {
        let Some(pid) = self.pid else {
            return Ok(false);
        };
        let sig = match signal_name {
            "SIGTERM" => libc::SIGTERM,
            "SIGKILL" => libc::SIGKILL,
            "SIGINT" => libc::SIGINT,
            "SIGHUP" => libc::SIGHUP,
            other => return Err(format!("unsupported signal: {}", other)),
        };
        // `kill(-pid, sig)` targets the process group whose
        // leader is `pid`. The pty puts the spawned shell at
        // the head of its own session, so this catches
        // sub-processes the shell or agent forked.
        let rc = unsafe { libc::kill(-(pid as i32), sig) };
        if rc == 0 {
            Ok(true)
        } else {
            let err = std::io::Error::last_os_error();
            // ESRCH = no such process group. Treat as
            // "nothing to signal" rather than an error so the
            // caller's stop flow stays idempotent.
            if err.raw_os_error() == Some(libc::ESRCH) {
                Ok(false)
            } else {
                Err(format!("kill(-{}, {}): {}", pid, signal_name, err))
            }
        }
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
    /// Map from terminal ID to handle
    terminals: HashMap<TerminalId, TerminalHandle>,
    /// Next terminal ID
    next_id: usize,
    /// Async bridge for sending notifications to main loop
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
            next_id: 0,
            async_bridge: None,
        }
    }

    /// The window that owns this manager.
    pub fn window_id(&self) -> fresh_core::WindowId {
        self.window_id
    }

    /// Set the async bridge for communication with main loop
    pub fn set_async_bridge(&mut self, bridge: AsyncBridge) {
        self.async_bridge = Some(bridge);
    }

    /// Peek at the next terminal ID that would be assigned.
    pub fn next_terminal_id(&self) -> TerminalId {
        TerminalId(self.next_id)
    }

    /// Spawn a new terminal session
    ///
    /// # Arguments
    /// * `cols` - Initial terminal width in columns
    /// * `rows` - Initial terminal height in rows
    /// * `cwd` - Optional working directory (defaults to current directory)
    /// * `log_path` - Optional path for raw PTY log (for session restore)
    /// * `backing_path` - Optional path for rendered scrollback (incremental streaming)
    /// * `backing_mode` - Whether this terminal *continues* the transcript
    ///   already in those files (restore / respawn) or starts a new one
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
    ) -> Result<TerminalId, String> {
        let id = TerminalId(self.next_id);
        self.next_id += 1;

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
    ) -> Result<TerminalHandle, String> {
        let pty_pair = open_pty(cols, rows)?;

        // The active authority's terminal wrapper drives the shell command
        // unconditionally — local wraps `detect_shell()` with no args;
        // container/remote authorities re-parent into `docker exec -w …`,
        // `ssh …`, etc.
        let (cmd, shell) =
            build_shell_command(terminal_wrapper, cwd.as_deref(), &env_delta, &extra_env);

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

        // A *continuing* terminal (workspace restore, respawn after exit) picks
        // up the transcript already in its backing file: seed the history end
        // so entering terminal mode doesn't truncate it to 0. A `Fresh`
        // terminal never does — whatever is on that path belongs to some
        // earlier terminal, and inheriting it would show one terminal's
        // scrollback in another (fresh#2836).
        if let (BackingMode::Continue, Some(p)) = (backing_mode, backing_path.as_ref()) {
            if let Ok(metadata) = std::fs::metadata(p) {
                if metadata.len() > 0 {
                    if let Ok(mut s) = state.lock() {
                        s.set_backing_file_history_end(metadata.len());
                    }
                }
            }
        }

        let (command_tx, command_rx) = mpsc::channel::<TerminalCommand>();
        let alive = Arc::new(AtomicBool::new(true));

        let master_writer = pty_pair
            .master
            .take_writer()
            .map_err(|e| format!("Failed to get PTY writer: {}", e))?;
        let reader = pty_pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("Failed to get PTY reader: {}", e))?;

        let log_writer = open_log_writer(log_path.as_deref(), backing_mode);
        let backing_writer = open_backing_writer(backing_path.as_deref(), backing_mode);

        // Tag output/exit with the owning window so the main loop never has to
        // guess which session a `Terminal-N` belongs to (ids collide across
        // windows). See `fresh_core::WindowTerminalId`. Shared (not copied)
        // with the reader/wait threads so `adopt` can retag a live terminal
        // when it moves to another window's manager.
        let companion = companion.map(|spawn| Arc::new(OmpCompanionLiveState::new(spawn)));
        let wt_id: SharedWtId = Arc::new(Mutex::new(fresh_core::WindowTerminalId::new(
            self.window_id,
            id,
        )));
        let exit_coordinator = Arc::new(ExitCoordinator::new(
            self.async_bridge.clone(),
            wt_id.clone(),
            id,
            companion.clone(),
        ));

        // Reader thread: drains PTY output, feeds the emulator, streams
        // scrollback / raw log to disk, and pings the main loop to redraw.
        let reader_loop = ReaderLoop {
            reader,
            state: state.clone(),
            response_tx: command_tx.clone(),
            backing_writer,
            log_writer,
            async_bridge: self.async_bridge.clone(),
            wt_id: wt_id.clone(),
            terminal_id: id,
            alive: alive.clone(),
            filtered_output: if companion.is_some() {
                Vec::with_capacity(4096)
            } else {
                Vec::new()
            },
            companion: companion.clone(),
            exit_coordinator: exit_coordinator.clone(),
        };
        thread::spawn(move || reader_loop.run());

        // Wait thread: records status; the shared barrier emits only after the
        // reader has reached EOF and completed its final flush.
        spawn_wait_thread(child, exit_coordinator);

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
        spawn_writer_thread(command_rx, master_writer, pty_pair.master, child_killer);

        Ok(TerminalHandle {
            state,
            command_tx,
            alive,
            cols,
            rows,
            cwd,
            shell,
            pid: child_pid,
            master_fd,
            wt_id,
            companion,
        })
    }

    /// Remove a live terminal from this manager *without* shutting it down,
    /// so another window's manager can [`Self::adopt`] it. The PTY, its
    /// reader/writer/wait threads, and the running process are untouched.
    pub fn release(&mut self, id: TerminalId) -> Option<TerminalHandle> {
        self.terminals.remove(&id)
    }

    /// Adopt a live terminal released from another window's manager.
    ///
    /// Assigns the handle a fresh id in this manager's namespace (ids are
    /// per-window and would otherwise collide) and rewrites the shared
    /// `(window, terminal)` tag, so output/exit messages the terminal's
    /// threads send from now on are attributed to this window. Returns the
    /// new id.
    pub fn adopt(&mut self, handle: TerminalHandle) -> TerminalId {
        let id = TerminalId(self.next_id);
        self.next_id += 1;
        if let Ok(mut wt_id) = handle.wt_id.lock() {
            *wt_id = fresh_core::WindowTerminalId::new(self.window_id, id);
        }
        self.terminals.insert(id, handle);
        id
    }

    /// Get a terminal handle by ID
    pub fn get(&self, id: TerminalId) -> Option<&TerminalHandle> {
        self.terminals.get(&id)
    }

    /// Get a mutable terminal handle by ID
    pub fn get_mut(&mut self, id: TerminalId) -> Option<&mut TerminalHandle> {
        self.terminals.get_mut(&id)
    }

    /// Close a terminal, revoking its companion before removing the handle.
    pub fn close(&mut self, id: TerminalId) -> bool {
        let Some(handle) = self.terminals.get(&id) else {
            return false;
        };
        handle.shutdown();
        self.terminals.remove(&id);
        true
    }

    /// Get all terminal IDs
    pub fn terminal_ids(&self) -> Vec<TerminalId> {
        self.terminals.keys().copied().collect()
    }

    /// Get count of open terminals
    pub fn count(&self) -> usize {
        self.terminals.len()
    }

    /// Shutdown all terminals, revoking capabilities before handles disappear.
    pub fn shutdown_all(&mut self) {
        for handle in self.terminals.values() {
            handle.shutdown();
        }
        self.terminals.clear();
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
            if let Some(handle) = self.terminals.get(id) {
                handle.revoke_omp_companion();
            }
            self.terminals.remove(id);
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
fn build_shell_command(
    terminal_wrapper: TerminalWrapper,
    cwd: Option<&std::path::Path>,
    env_delta: &crate::services::env_provider::EnvDelta,
    extra_env: &HashMap<String, String>,
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

    // Apply the activated-environment delta (venv/direnv/mise) before the
    // control vars below, so TERM/FRESH_SESSION win over any same-named key
    // (issue #2355).
    for (k, v) in &env_delta.set {
        cmd.env(k, v);
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
        cmd.env(k, v);
    }

    (cmd, shell)
}

/// Open the optional raw-PTY log file for full-session capture. `Continue`
/// appends to the existing capture; `Fresh` truncates (see [`BackingMode`]).
fn open_log_writer(
    log_path: Option<&std::path::Path>,
    mode: BackingMode,
) -> Option<std::io::BufWriter<std::fs::File>> {
    log_path.and_then(|p| open_transcript_file(p, mode))
}

/// Open the optional scrollback backing file. `Continue` (workspace restore,
/// respawn after exit) appends so the transcript keeps streaming where it left
/// off; `Fresh` truncates so a new terminal never starts on top of another
/// terminal's scrollback.
fn open_backing_writer(
    backing_path: Option<&std::path::Path>,
    mode: BackingMode,
) -> Option<std::io::BufWriter<std::fs::File>> {
    backing_path.and_then(|p| open_transcript_file(p, mode))
}

/// Shared open for the two on-disk transcripts (raw log, rendered scrollback):
/// append when continuing an existing terminal's story, truncate when starting
/// a new one.
fn open_transcript_file(
    path: &std::path::Path,
    mode: BackingMode,
) -> Option<std::io::BufWriter<std::fs::File>> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true);
    match mode {
        BackingMode::Continue => options.append(true),
        BackingMode::Fresh => options.write(true).truncate(true),
    };
    options.open(path).ok().map(std::io::BufWriter::new)
}

/// Shared child-status/reader-drained barrier. The second prerequisite clears
/// the drained companion output filter and emits the sole `TerminalExited` notification.
struct ExitCoordinator {
    inner: Mutex<ExitCoordinatorState>,
    async_bridge: Option<AsyncBridge>,
    wt_id: SharedWtId,
    terminal_id: TerminalId,
    companion: Option<Arc<OmpCompanionLiveState>>,
}

#[derive(Default)]
struct ExitCoordinatorState {
    child_finished: bool,
    reader_drained: bool,
    emitted: bool,
    exit_code: Option<i32>,
}

impl ExitCoordinator {
    fn new(
        async_bridge: Option<AsyncBridge>,
        wt_id: SharedWtId,
        terminal_id: TerminalId,
        companion: Option<Arc<OmpCompanionLiveState>>,
    ) -> Self {
        Self {
            inner: Mutex::new(ExitCoordinatorState::default()),
            async_bridge,
            wt_id,
            terminal_id,
            companion,
        }
    }

    fn child_finished(&self, exit_code: Option<i32>) {
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
        let Some(bridge) = &self.async_bridge else {
            return;
        };
        // Read the tag only after both prerequisites; adoption may have changed
        // the owning window while either background thread was still active.
        let Ok(terminal) = self.wt_id.lock().map(|id| *id) else {
            return;
        };
        #[allow(clippy::let_underscore_must_use)]
        let _ = bridge.sender().send(
            crate::services::async_bridge::AsyncMessage::TerminalExited {
                terminal,
                exit_code,
            },
        );
    }
}

/// Wait-thread body: record the child's status without racing reader flushes.
fn spawn_wait_thread(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    exit_coordinator: Arc<ExitCoordinator>,
) {
    thread::spawn(move || {
        let exit_code = match child.wait() {
            Ok(status) => Some(status.exit_code() as i32),
            Err(error) => {
                tracing::warn!(
                    "child.wait() failed for {:?}: {}",
                    exit_coordinator.terminal_id,
                    error
                );
                None
            }
        };
        exit_coordinator.child_finished(exit_code);
    });
}

/// Writer-thread body: own the master, apply queued writes/resizes, and kill
/// the child on shutdown. The wait-thread reaps the exit status, so this thread
/// intentionally doesn't call `wait` (which would race it).
fn spawn_writer_thread(
    command_rx: mpsc::Receiver<TerminalCommand>,
    mut master: Box<dyn Write + Send>,
    pty_master: Box<dyn portable_pty::MasterPty + Send>,
    mut child_killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
) {
    thread::spawn(move || {
        loop {
            match command_rx.recv() {
                Ok(TerminalCommand::Write(data)) => {
                    if let Err(e) = master.write_all(&data) {
                        tracing::error!("Terminal write error: {}", e);
                        break;
                    }
                    // Best-effort flush — PTY write errors are handled above.
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = master.flush();
                }
                Ok(TerminalCommand::Resize { cols, rows }) => {
                    if let Err(e) = pty_master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    }) {
                        tracing::warn!("Failed to resize PTY: {}", e);
                    }
                }
                Ok(TerminalCommand::Shutdown) | Err(_) => {
                    break;
                }
            }
        }
        // User-initiated shutdown: ask the OS to terminate the child via the
        // cloned killer. The wait-thread owns `child` and reaps the status.
        #[allow(clippy::let_underscore_must_use)]
        let _ = child_killer.kill();
    });
}

/// Owns everything the PTY reader thread needs. Bundled into one struct so the
/// thread body is a readable `run(self)` of small steps instead of a closure
/// capturing a dozen locals at deep nesting.
struct ReaderLoop {
    reader: Box<dyn Read + Send>,
    state: Arc<Mutex<TerminalState>>,
    /// Sends PTY write-responses (e.g. DSR cursor reports) back to the writer.
    response_tx: mpsc::Sender<TerminalCommand>,
    /// Incremental scrollback stream (rendered lines), if a backing file is set.
    backing_writer: Option<std::io::BufWriter<std::fs::File>>,
    /// Raw byte log for session-restore replay, if a log file is set.
    log_writer: Option<std::io::BufWriter<std::fs::File>>,
    async_bridge: Option<AsyncBridge>,
    wt_id: SharedWtId,
    terminal_id: TerminalId,
    alive: Arc<AtomicBool>,
    companion: Option<Arc<OmpCompanionLiveState>>,
    filtered_output: Vec<u8>,
    exit_coordinator: Arc<ExitCoordinator>,
}

impl ReaderLoop {
    /// Drain the PTY until EOF or error, then mark the terminal dead and flush.
    fn run(mut self) {
        tracing::debug!("Terminal {:?} reader thread started", self.terminal_id);
        let mut buf = [0u8; 4096];
        let mut total_bytes = 0usize;
        loop {
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
                Err(error) => {
                    tracing::error!("Terminal read error: {}", error);
                    break;
                }
            }
        }

        // An unverified prefix/synchronizer fragment is ordinary output at EOF;
        // a verified, unterminated candidate is private and is discarded.
        let trailing_visible = self
            .companion
            .as_ref()
            .map_or_else(Vec::new, |companion| companion.finish_output());
        if !trailing_visible.is_empty() {
            self.process_visible_output(&trailing_visible);
        }

        // The reader side of the exit barrier is recorded only after every
        // final emulator/backing/log action and writer flush has completed.
        if let Some(writer) = self.backing_writer.as_mut() {
            #[allow(clippy::let_underscore_must_use)]
            let _ = writer.flush();
        }
        if let Some(writer) = self.log_writer.as_mut() {
            #[allow(clippy::let_underscore_must_use)]
            let _ = writer.flush();
        }
        self.alive.store(false, Ordering::Release);
        self.exit_coordinator.reader_drained();
    }

    fn process_read(&mut self, bytes: &[u8], now: Instant) {
        let Some(companion) = self.companion.as_ref() else {
            self.process_visible_output(bytes);
            return;
        };
        let notify_candidate = companion.filter_output_into(bytes, now, &mut self.filtered_output);
        if notify_candidate {
            self.notify_companion_candidate();
        }
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
        let Ok(mut state) = self.state.lock() else {
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
            // Receiver may be dropped if the writer thread exited.
            #[allow(clippy::let_underscore_must_use)]
            let _ = self
                .response_tx
                .send(TerminalCommand::Write(response.into_bytes()));
        }

        // Incrementally stream new scrollback lines to the backing file.
        if let Some(writer) = self.backing_writer.as_mut() {
            match state.flush_new_scrollback(writer) {
                Ok(lines_written) => {
                    if lines_written > 0 {
                        if let Ok(pos) = writer.get_ref().metadata() {
                            state.set_backing_file_history_end(pos.len());
                        }
                        #[allow(clippy::let_underscore_must_use)]
                        let _ = writer.flush();
                    }
                }
                Err(e) => {
                    tracing::warn!("Terminal backing file write error: {}", e);
                    self.backing_writer = None;
                }
            }
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
        if let Some(bridge) = &self.async_bridge {
            // Read the tag per send — the terminal may have been adopted by
            // another window since it was spawned.
            let Ok(terminal) = self.wt_id.lock().map(|id| *id) else {
                return;
            };
            #[allow(clippy::let_underscore_must_use)]
            let _ = bridge
                .sender()
                .send(crate::services::async_bridge::AsyncMessage::TerminalOutput { terminal });
        }
    }

    fn notify_companion_candidate(&self) {
        let Some(bridge) = &self.async_bridge else {
            return;
        };
        let Ok(terminal) = self.wt_id.lock().map(|id| *id) else {
            return;
        };
        #[allow(clippy::let_underscore_must_use)]
        let _ = bridge.sender().send(
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
    use std::collections::VecDeque;
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
        let terminal_id = TerminalId(7);
        let wt_id = Arc::new(Mutex::new(fresh_core::WindowTerminalId::new(
            fresh_core::WindowId(9),
            terminal_id,
        )));
        let exit_coordinator = Arc::new(ExitCoordinator::new(
            Some(bridge.clone()),
            wt_id.clone(),
            terminal_id,
            companion.clone(),
        ));
        let state = Arc::new(Mutex::new(TerminalState::new(80, 4)));
        let alive = Arc::new(AtomicBool::new(true));
        let (response_tx, _response_rx) = mpsc::channel();
        ReaderLoop {
            reader: Box::new(ChunkReader::new(chunks)),
            state: state.clone(),
            response_tx,
            backing_writer: open_backing_writer(Some(backing_path), BackingMode::Fresh),
            log_writer: open_log_writer(Some(log_path), BackingMode::Fresh),
            async_bridge: Some(bridge),
            wt_id,
            terminal_id,
            alive: alive.clone(),
            companion,
            filtered_output: Vec::with_capacity(4096),
            exit_coordinator: exit_coordinator.clone(),
        }
        .run();
        (state, exit_coordinator, alive)
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
            crate::services::async_bridge::AsyncMessage::OmpCompanionSnapshotReady { .. }
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
    fn revoked_split_candidate_stays_filtered_until_reader_drain_and_exit_barrier() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let frame = output_frame(&secret, b"split-after-manual-close");
        let split = OMP_OUTPUT_PREFIX.len() + OMP_SYNC_B64_LEN + 1 + 5;
        let mut visible = Vec::new();
        assert!(!companion.filter_output_into(&frame[..split], Instant::now(), &mut visible,));
        assert!(visible.is_empty());

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
    fn command_frame_is_private_direction_separated_and_reports_admission() {
        let secret = test_secret();
        let companion = test_companion(secret);
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
            master_fd: None,
            wt_id: Arc::new(Mutex::new(fresh_core::WindowTerminalId::new(
                fresh_core::WindowId(1),
                TerminalId(2),
            ))),
            companion: Some(companion.clone()),
        };

        assert_eq!(handle.companion_kind(), Some(TerminalCompanion::Omp));
        assert!(handle.enqueue_omp_companion_command(OmpCompanionCommandType::Cancel));
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
        assert_eq!(
            &payload[..separator],
            b"eyJ2ZXJzaW9uIjoxLCJ0eXBlIjoiY2FuY2VsIn0"
        );
        assert_eq!(
            &payload[separator + 1..],
            b"pnoZtZh1IvlXyDP3ukIHnEDdSb4vjqePd89L2dcBIWQ"
        );

        handle.alive.store(false, Ordering::Release);
        assert!(!handle.enqueue_omp_companion_command(OmpCompanionCommandType::RequestSnapshot));
        handle.alive.store(true, Ordering::Release);

        companion.revoke_access();
        assert!(!handle.enqueue_omp_companion_command(OmpCompanionCommandType::RequestSnapshot));
        drop(command_rx);
        assert!(!handle.enqueue_omp_companion_command(OmpCompanionCommandType::Cancel));
    }

    #[test]
    fn command_admission_is_false_when_writer_channel_is_disconnected() {
        let companion = test_companion(test_secret());
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
            master_fd: None,
            wt_id: Arc::new(Mutex::new(fresh_core::WindowTerminalId::new(
                fresh_core::WindowId(1),
                TerminalId(2),
            ))),
            companion: Some(companion),
        };
        assert!(!handle.enqueue_omp_companion_command(OmpCompanionCommandType::Cancel));
    }

    #[test]
    fn exit_is_single_and_waits_for_reader_final_flush() {
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("raw.log");
        let backing_path = directory.path().join("backing.txt");
        let bridge = AsyncBridge::new();
        let terminal_id = TerminalId(7);
        let wt_id = Arc::new(Mutex::new(fresh_core::WindowTerminalId::new(
            fresh_core::WindowId(9),
            terminal_id,
        )));
        let coordinator = Arc::new(ExitCoordinator::new(
            Some(bridge.clone()),
            wt_id.clone(),
            terminal_id,
            None,
        ));
        coordinator.child_finished(Some(23));
        assert!(bridge.try_recv_all().is_empty());

        let state = Arc::new(Mutex::new(TerminalState::new(80, 4)));
        let alive = Arc::new(AtomicBool::new(true));
        let (response_tx, _response_rx) = mpsc::channel();
        ReaderLoop {
            reader: Box::new(ChunkReader::new(vec![
                b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4\r\nline-5\r\n".to_vec(),
            ])),
            state,
            response_tx,
            backing_writer: open_backing_writer(Some(&backing_path), BackingMode::Fresh),
            log_writer: open_log_writer(Some(&log_path), BackingMode::Fresh),
            async_bridge: Some(bridge.clone()),
            wt_id,
            terminal_id,
            alive: alive.clone(),
            companion: None,
            filtered_output: Vec::new(),
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
    fn exit_barrier_also_waits_when_reader_finishes_first_and_revokes_before_exit() {
        let secret = test_secret();
        let companion = test_companion(secret);
        let bridge = AsyncBridge::new();
        let terminal_id = TerminalId(3);
        let coordinator = ExitCoordinator::new(
            Some(bridge.clone()),
            Arc::new(Mutex::new(fresh_core::WindowTerminalId::new(
                fresh_core::WindowId(4),
                terminal_id,
            ))),
            terminal_id,
            Some(companion.clone()),
        );
        coordinator.reader_drained();
        assert!(bridge.try_recv_all().is_empty());
        assert!(companion.verify_output_auth(
            &omp_companion_synchronizer(&secret),
            b"body",
            &omp_companion_output_tag(&secret, b"body")
        ));
        coordinator.child_finished(None);
        assert!(companion.take_candidate().is_none());
        assert!(!companion.verify_output_auth(
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
