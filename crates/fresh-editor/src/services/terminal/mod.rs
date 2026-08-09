//! Terminal emulation service for Fresh
//!
//! This module provides built-in terminal support using:
//! - `alacritty_terminal` for terminal emulation (VT100/ANSI parsing, grid management)
//! - `portable-pty` for cross-platform PTY management
//!
//! # Incremental Streaming Architecture
//!
//! The terminal uses an incremental streaming design that avoids O(n) work on mode
//! switches and session restore. The key insight is that scrollback history is append-only.
//!
//! ## Data Flow
//!
//! 1. **PTY Read Loop** (manager.rs): As PTY output arrives, `process_output()` updates
//!    the terminal grid, then `flush_new_scrollback()` appends complete scrollback lines
//!    to the rendered history file.
//!
//! 2. **Terminal → Scrollback** (terminal.rs: `sync_terminal_to_buffer`): Flushes history,
//!    atomically replaces a separate history + visible-screen checkpoint, then loads that
//!    checkpoint as the read-only buffer.
//!
//! 3. **Scrollback → Terminal** (terminal.rs: `enter_terminal_mode`): Resumes the live grid.
//!    Neither the append-only history nor its checkpoint needs truncation.
//!
//! 4. **Workspace Save** (app/workspace.rs): Atomically refreshes each terminal checkpoint
//!    before serializing its distinct history and checkpoint paths.
//!
//! 5. **Workspace Restore** (app/workspace.rs): Loads the checkpoint directly (skipping log
//!    replay), while a replacement PTY continues only the append-only history file.
//!
//! ## Terminal Artifact Structure
//!
//! Under the workspace's terminal artifact directory each terminal owns:
//! - `*.history.txt`: append-only rendered scrollback
//! - `*.txt`: atomically replaced read-only history + visible-screen checkpoint
//! - `*.log`: unfiltered PTY byte stream
//!
//! ## Module Responsibilities
//!
//! - `term.rs`: Terminal state and incremental streaming methods
//! - `manager.rs`: PTY lifecycle and read loop with streaming
//! - `../app/terminal.rs`: Mode switching logic
//! - `../app/session.rs`: Session save/restore integration

pub mod manager;
mod omp_companion;
pub mod path_link;
pub mod pty;
pub mod term;
#[cfg(windows)]
pub mod windows_shell;

pub use manager::{detect_shell, BackingMode, TerminalId, TerminalManager};
pub use term::{PrependedHead, TerminalCell, TerminalState};
#[cfg(windows)]
pub use windows_shell::set_skip_app_execution_alias;
