//! Workspace persistence for per-project editor state
//!
//! Saves and restores:
//! - Split layout and open files
//! - Cursor and scroll positions per split per file
//! - File explorer state
//! - Search/replace history and options
//! - Bookmarks
//!
//! ## Storage
//!
//! Workspaces are stored in `$XDG_DATA_HOME/fresh/workspaces/{encoded_path}.json`
//! where `{encoded_path}` is the working directory path with:
//! - Path separators (`/`) replaced with underscores (`_`)
//! - Special characters percent-encoded as `%XX`
//!
//! Example: `/home/user/my project` becomes `home_user_my%20project.json`
//!
//! The encoding is fully reversible using `decode_filename_to_path()`.
//!
//! ## Crash Resistance
//!
//! Uses atomic writes: write to temp file, then rename.
//! This ensures the workspace file is never left in a corrupted state.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::input::input_history::get_data_dir;

/// Current workspace file format version
pub const WORKSPACE_VERSION: u32 = 1;

/// Current per-file workspace version
pub const FILE_WORKSPACE_VERSION: u32 = 1;

/// Persisted workspace state for a working directory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Schema version for future migrations
    pub version: u32,

    /// Working directory this workspace belongs to (for validation)
    pub working_dir: PathBuf,

    /// Split layout tree
    pub split_layout: SerializedSplitNode,

    /// Active split ID
    pub active_split_id: usize,

    /// Per-split view states (keyed by split_id)
    pub split_states: HashMap<usize, SerializedSplitViewState>,

    /// Editor config overrides (toggles that differ from defaults)
    #[serde(default)]
    pub config_overrides: WorkspaceConfigOverrides,

    /// File explorer state
    pub file_explorer: FileExplorerState,

    /// Input histories (search, replace, command palette, etc.)
    #[serde(default)]
    pub histories: WorkspaceHistories,

    /// Search options (persist across searches within workspace)
    #[serde(default)]
    pub search_options: SearchOptions,

    /// Bookmarks (character key -> file position)
    #[serde(default)]
    pub bookmarks: HashMap<char, SerializedBookmark>,

    /// Open terminal workspaces (for restoration)
    #[serde(default)]
    pub terminals: Vec<SerializedTerminalWorkspace>,
    /// Terminal index of the OMP companion currently owned by the orchestrator
    /// session. The index, rather than the process-local terminal id, survives
    /// terminal-id remapping during workspace restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracked_agent_terminal: Option<usize>,

    /// External files open in the workspace (files outside working_dir)
    /// These are stored as absolute paths since they can't be made relative
    #[serde(default)]
    pub external_files: Vec<PathBuf>,

    /// Files that were read-only at save time; re-applied on restore.
    /// Relative to `working_dir` when possible, otherwise absolute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only_files: Vec<PathBuf>,

    /// Unnamed buffers that should be restored from recovery files
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unnamed_buffers: Vec<UnnamedBufferRef>,

    /// Plugin-managed global state, isolated per plugin name.
    /// Persisted across sessions so plugins can store non-buffer-specific state.
    /// TODO: Need to think about plugin isolation / namespacing strategy for these APIs.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub plugin_global_state: HashMap<String, HashMap<String, serde_json::Value>>,

    /// Timestamp when workspace was saved (Unix epoch seconds)
    pub saved_at: u64,

    /// Display label for this session (orchestrator). Defaults to the
    /// root basename when absent. Since windows.json was dropped, the
    /// per-dir workspace file is the sole session record, so the label
    /// lives here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Per-session plugin state (the window's own `plugin_state`,
    /// carrying e.g. the orchestrator's `project_path` /
    /// `shared_worktree`). Distinct from `plugin_global_state` (which
    /// is editor-wide and lives in the global store). Persisted here so
    /// session identity survives across restarts without windows.json.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub session_plugin_state: HashMap<String, HashMap<String, serde_json::Value>>,

    /// How to rebuild / reconnect this session's backend on restore. `Local`
    /// (the default, skipped when serialized) for an ordinary host session;
    /// a `Plugin` (devcontainer/docker) or `RemoteAgent` (SSH/Kubernetes)
    /// spec for a session that was running remotely, so a restart or
    /// relaunch can bring it back disconnected-but-reconnectable rather than
    /// silently local. See `docs/internal/PER_SESSION_BACKENDS_DESIGN.md`.
    #[serde(default, skip_serializing_if = "is_local_authority_spec")]
    pub authority_spec: crate::services::authority::SessionAuthoritySpec,

    /// Durable identity of the workspace this snapshot belongs to, minted
    /// once when the window is created and stable across restarts and
    /// relabels. The workspace file is named
    /// `workspaces/<encoded-root>.<stable_id>.json` — the encoded root is
    /// only a filename-level locator for cheap lookup; this id is what
    /// distinguishes the workspace, and the `working_dir` recorded inside
    /// the file stays authoritative wherever names collide. `None` only for
    /// legacy files written before stable ids existed — the owning window
    /// mints an id at construction, keeps it through the id-less load, and
    /// the next save re-keys the file (then retires the root-keyed
    /// duplicate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
}

/// Mint a process-reserved, store-checked durable workspace identity.
///
/// UUID entropy makes cross-process collisions vanishingly unlikely; the
/// reservation set closes deterministic/injected same-process collisions and
/// the workspace scan prevents adopting an identity already published by a
/// different Fresh process.
pub fn generate_stable_id() -> String {
    generate_stable_id_with(|| format!("ws-{}", uuid::Uuid::new_v4().simple()))
}

fn generate_stable_id_with(mut candidate: impl FnMut() -> String) -> String {
    static RESERVED: LazyLock<Mutex<HashSet<String>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    let reserved = &*RESERVED;
    loop {
        let id = candidate();
        if id.is_empty() {
            continue;
        }
        let mut ids = reserved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ids.contains(&id) || stable_id_published(&id) {
            continue;
        }
        ids.insert(id.clone());
        return id;
    }
}

fn stable_id_published(stable_id: &str) -> bool {
    let Ok(dir) = get_workspaces_dir() else {
        return false;
    };
    stable_id_published_in(&dir, stable_id)
}

fn stable_id_published_in(workspaces_dir: &Path, stable_id: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(workspaces_dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|value| {
                value
                    .get("stable_id")
                    .and_then(|id| id.as_str())
                    .map(str::to_owned)
            })
            .as_deref()
            == Some(stable_id)
    })
}

fn stable_id_claimed_by_other_in(
    workspaces_dir: &Path,
    stable_id: &str,
    working_dir: &Path,
) -> bool {
    let expected = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let Ok(entries) = std::fs::read_dir(workspaces_dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            return false;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
            return false;
        };
        if value.get("stable_id").and_then(|id| id.as_str()) != Some(stable_id) {
            return false;
        }
        let Some(root) = value
            .get("working_dir")
            .and_then(|root| root.as_str())
            .map(PathBuf::from)
        else {
            return true;
        };
        root.canonicalize().unwrap_or(root) != expected
    })
}

/// Skip-serialize predicate so workspace files for ordinary local sessions
/// don't carry a redundant `authority_spec: Local`.
fn is_local_authority_spec(spec: &crate::services::authority::SessionAuthoritySpec) -> bool {
    matches!(
        spec,
        crate::services::authority::SessionAuthoritySpec::Local
    )
}

/// Reference to a persisted unnamed buffer (content stored in recovery files)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnnamedBufferRef {
    /// Stable recovery ID used to locate the recovery file
    pub recovery_id: String,
    /// Display name shown in tabs (e.g., "Untitled-1")
    pub display_name: String,
}

/// Serializable split layout (mirrors SplitNode but with file paths instead of buffer IDs)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SerializedSplitNode {
    Leaf {
        /// File path relative to working_dir (None for scratch buffers)
        file_path: Option<PathBuf>,
        split_id: usize,
        /// Optional label set by plugins (e.g., "claude-sidebar")
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Recovery ID for unnamed buffers (when file_path is None)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unnamed_recovery_id: Option<String>,
        /// Role tag (e.g. UtilityDock). Mirrors `SplitNode::Leaf::role`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<crate::view::split::SplitRole>,
    },
    Terminal {
        terminal_index: usize,
        split_id: usize,
        /// Optional label set by plugins
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Role tag — terminals can also be the dock occupant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<crate::view::split::SplitRole>,
    },
    Split {
        direction: SerializedSplitDirection,
        first: Box<Self>,
        second: Box<Self>,
        ratio: f32,
        split_id: usize,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SerializedSplitDirection {
    Horizontal,
    Vertical,
}

/// Per-split view state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedSplitViewState {
    /// Open tabs in tab order (files or terminals)
    #[serde(default)]
    pub open_tabs: Vec<SerializedTabRef>,

    /// Active tab index in open_tabs (if present)
    #[serde(default)]
    pub active_tab_index: Option<usize>,

    /// Open files in tab order (paths relative to working_dir)
    /// Deprecated; retained for backward compatibility.
    #[serde(default)]
    pub open_files: Vec<PathBuf>,

    /// Active file index in open_files
    #[serde(default)]
    pub active_file_index: usize,

    /// Per-file cursor and scroll state
    #[serde(default)]
    pub file_states: HashMap<PathBuf, SerializedFileState>,

    /// Tab scroll offset
    #[serde(default)]
    pub tab_scroll_offset: usize,

    /// View mode
    #[serde(default)]
    pub view_mode: SerializedViewMode,

    /// Compose width if in compose mode
    #[serde(default)]
    pub compose_width: Option<u16>,
}

/// Per-file state within a split
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedFileState {
    /// Primary cursor position (byte offset)
    pub cursor: SerializedCursor,

    /// Additional cursors for multi-cursor
    #[serde(default)]
    pub additional_cursors: Vec<SerializedCursor>,

    /// Scroll position (byte offset)
    pub scroll: SerializedScroll,

    /// View mode for this buffer in this split
    #[serde(default)]
    pub view_mode: SerializedViewMode,

    /// Compose width for this buffer in this split
    #[serde(default)]
    pub compose_width: Option<u16>,

    /// Explicit per-buffer line-number override (`None` = follow global default).
    /// Persists the "Toggle Line Numbers (Current Buffer)" choice across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_numbers: Option<bool>,

    /// Explicit per-buffer line-wrap override (`None` = follow global default).
    /// Persists the "Toggle Line Wrap (Current Buffer)" choice across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_wrap: Option<bool>,

    /// Explicit per-buffer virtual-space override (`None` = follow global
    /// default). Persists the "Toggle Virtual Space (Current Buffer)" choice
    /// across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_space: Option<crate::config::VirtualSpaceMode>,

    /// Explicit per-buffer indentation-guide override (`None` = follow the
    /// global `editor.indentation_guide` mode). Persists the "Toggle
    /// Indentation Guides (Current Buffer)" choice across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indentation_guide: Option<bool>,

    /// Explicit per-buffer folding-indicator override (`None` = show them).
    /// Persists the "Toggle Folding Indicators (Current Buffer)" choice across
    /// restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fold_indicators: Option<bool>,

    /// Explicit per-buffer indentation-style override (`None` = follow the
    /// language default). Persists the "Toggle Indentation: Spaces ↔ Tabs
    /// (Current Buffer)" choice across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_tabs: Option<bool>,

    /// Explicit per-buffer whitespace-indicator master override (`None` =
    /// follow config). Persists the "Toggle Whitespace Indicators (Current
    /// Buffer)" / "Toggle Tab Indicators (Current Buffer)" choice across
    /// restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whitespace_indicators: Option<bool>,

    /// Explicit per-buffer tab-indicator override, layered on top of
    /// `whitespace_indicators` (`None` = follow the master/config resolution).
    /// Persists the "Toggle Tab Indicators (Current Buffer)" choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_indicators: Option<bool>,

    /// Explicit per-buffer current-line-highlight override (`None` = follow the
    /// global default). Persists the "Toggle Current Line Highlight (Current
    /// Buffer)" choice across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight_current_line: Option<bool>,

    /// Explicit per-buffer occurrence-highlight override (`None` = follow the
    /// global default). Persists the "Toggle Occurrence Highlight (Current
    /// Buffer)" choice across restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight_occurrences: Option<bool>,

    /// Plugin-managed state (arbitrary key-value pairs, persisted across sessions)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub plugin_state: HashMap<String, serde_json::Value>,

    /// Collapsed folding ranges for this buffer/view
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folds: Vec<SerializedFoldRange>,
}

/// Line-based folded range for persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedFoldRange {
    /// Header line number (visible line that owns the fold)
    pub header_line: usize,
    /// Last hidden line number (inclusive)
    pub end_line: usize,
    /// Optional placeholder text for the fold
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Text of the header line at save time. Used on restore to detect
    /// whether the file was edited externally between sessions (issue #1568):
    /// if the text at `header_line` no longer matches, we search nearby
    /// lines for it and fall back to dropping the fold rather than
    /// re-attaching it to unrelated content.
    ///
    /// `Option` for backward compatibility with older session files that
    /// didn't record the text.
    #[serde(default)]
    pub header_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedCursor {
    /// Cursor position as byte offset from start of file
    pub position: usize,
    /// Selection anchor as byte offset (if selection active)
    #[serde(default)]
    pub anchor: Option<usize>,
    /// Sticky column for vertical movement (character column)
    #[serde(default)]
    pub sticky_column: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedScroll {
    /// Top visible position as byte offset
    pub top_byte: usize,
    /// Virtual line offset within the top line (for wrapped lines)
    #[serde(default)]
    pub top_view_line_offset: usize,
    /// Left column offset (for horizontal scroll)
    #[serde(default)]
    pub left_column: usize,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum SerializedViewMode {
    #[default]
    Source,
    /// Page view (document-style layout with centering and concealment).
    /// Accepts "Compose" for backward compatibility with saved workspaces.
    #[serde(alias = "Compose")]
    PageView,
}

/// Config overrides that differ from base config
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfigOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_numbers: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_line_numbers: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_wrap: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syntax_highlighting: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_inlay_hints: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse_enabled: Option<bool>,
    /// Legacy: menu bar visibility was once stored as a per-workspace
    /// override here. It is now a global preference (`editor.show_menu_bar`),
    /// so this field is no longer written and is ignored on restore. Kept
    /// only for serde compatibility with workspaces saved by older builds.
    /// See issue #1156.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_bar_hidden: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileExplorerState {
    pub visible: bool,
    /// File explorer width. See [`crate::config::ExplorerWidth`] for
    /// the accepted wire formats (percent string, column string, legacy
    /// numeric forms). The `width_percent` alias preserves read
    /// compatibility with workspace files written by earlier versions.
    #[serde(
        alias = "width_percent",
        default = "crate::config::default_explorer_width_value"
    )]
    pub width: crate::config::ExplorerWidth,
    /// File explorer side placement
    #[serde(default)]
    pub side: crate::config::FileExplorerSide,
    /// Expanded directories (relative paths)
    #[serde(default)]
    pub expanded_dirs: Vec<PathBuf>,
    /// Scroll offset
    #[serde(default)]
    pub scroll_offset: usize,
    /// Show hidden files (fixes #569)
    #[serde(default)]
    pub show_hidden: bool,
    /// Show gitignored files (fixes #569)
    #[serde(default)]
    pub show_gitignored: bool,
}

impl Default for FileExplorerState {
    fn default() -> Self {
        Self {
            visible: false,
            width: crate::config::default_explorer_width_value(),
            side: crate::config::FileExplorerSide::Left,
            expanded_dirs: Vec::new(),
            scroll_offset: 0,
            show_hidden: false,
            show_gitignored: false,
        }
    }
}

/// Per-workspace input histories
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceHistories {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replace: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_palette: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goto_line: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_file: Vec<String>,
}

/// Search options that persist across searches within a workspace
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchOptions {
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub whole_word: bool,
    #[serde(default)]
    pub use_regex: bool,
    #[serde(default)]
    pub confirm_each: bool,
}

/// Serialized bookmark (file path + byte offset)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedBookmark {
    /// File path (relative to working_dir)
    pub file_path: PathBuf,
    /// Byte offset position in the file
    pub position: usize,
}

/// Reference to an open tab (file path, terminal index, or unnamed buffer)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SerializedTabRef {
    File(PathBuf),
    Terminal(usize),
    /// An unnamed buffer identified by its recovery ID
    Unnamed(String),
}

/// Persisted metadata for a terminal workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedTerminalWorkspace {
    pub terminal_index: usize,
    pub cwd: Option<PathBuf>,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub log_path: PathBuf,
    pub backing_path: PathBuf,
    /// Append-only rendered scrollback. `backing_path` is a separately replaced
    /// visible checkpoint; older workspaces omit this and are migrated once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_path: Option<PathBuf>,
    /// Exact append-only history length used to build `backing_path`. Restore
    /// promotes that checkpoint only until later append-only history overtakes
    /// it; newer history remains authoritative. Absent in older workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing_history_end: Option<u64>,
    /// Immutable checkpoint publication identity. Each successful full save
    /// writes a fresh checkpoint path and records the same generation here;
    /// failed saves leave the previously published pair authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_generation: Option<String>,
    /// Clean argv for a fresh relaunch of this terminal, or an empty vector for
    /// the durable plain-shell marker. Initial launch-only provisioning ids and
    /// prompts are deliberately excluded. The historical schema field remains
    /// named `command`. Absent in older workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Agent-resume spec: how to *rejoin* this terminal's agent session. OMP's
    /// exact authenticated resume wins regardless of the generic preference;
    /// ordinary agents use it only when resume is enabled and otherwise fall
    /// back to the clean `command` relaunch argv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_resume: Option<AgentResume>,
    /// Set when this terminal's process had already quit at save time.
    ///
    /// Restore then brings the buffer back as read-only scrollback with the
    /// restart offer re-armed, rather than respawning: the user closed the
    /// editor on a *finished* process, and silently re-running it — spending
    /// agent tokens on a conversation they were done with — is not what
    /// "restore my workspace" should mean. The restart is one click away
    /// either way. Absent for live terminals and in older workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited: Option<ExitedTerminalState>,
    /// The tab's explicit title (`claude`, `npm`, a plugin-supplied name),
    /// when it had one. Without this a restored agent tab falls back to
    /// foreground-process auto-naming and reads `node` / `bash` — or plain
    /// `*Terminal N*` once the process has exited and there is no foreground
    /// to read. Absent for tabs that were auto-named to begin with, which
    /// re-derive their name the same way after restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether this terminal's child was granted editor control — the
    /// Orchestrator's `allowScript`, which stamps a `FRESH_CMD_TOKEN`
    /// capability token into the agent's environment.
    ///
    /// Only the *grant* is persisted, never the token: the token table is
    /// in-memory and process-global, so the string this terminal carried in a
    /// previous run means nothing to the run that restores it. Restore mints a
    /// fresh token bound to the restored window (see
    /// `Window::remint_terminal_script_env`); without this flag a restored
    /// agent came back unable to drive the editor at all. Absent for plain
    /// terminals and in workspaces written before this field existed — which
    /// read back as `false`, i.e. no grant, the safe direction.
    #[serde(default, skip_serializing_if = "is_false")]
    pub script_access: bool,
    /// Descriptive companion marker. Capability secrets and live state are
    /// deliberately excluded from workspace persistence. A markerless legacy
    /// exact OMP resume may be promoted only by local unambiguous restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub companion: Option<fresh_core::api::TerminalCompanion>,
}

/// `skip_serializing_if` helper: keeps the default-`false` capability flag out
/// of the JSON for the overwhelming majority of terminals that never had it.
fn is_false(v: &bool) -> bool {
    !*v
}

/// The saved state of a terminal whose process had quit before the editor did.
/// A struct rather than a bare bool so it can carry more of the dead process's
/// story later (signal, duration, …) without a breaking schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitedTerminalState {
    /// Wait-status exit code, when the platform reported one. Drives the
    /// `(exit N)` suffix on the restored restart indicator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// How to rejoin a terminal's agent conversation on restore. A struct (not a
/// bare argv) so it can grow — e.g. an env overlay for per-session config
/// isolation, or a capture-provenance / policy field — without a breaking
/// schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResume {
    /// Resolved resume argv, with any session id already substituted into
    /// its own array slot (never a shell string). Run through the active
    /// authority's terminal wrapper, exactly like a launch command.
    pub argv: Vec<String>,
}

// ============================================================================
// Global file state persistence (per-file, not per-project)
// ============================================================================

/// Individual file state stored in its own file
///
/// Each source file's scroll/cursor state is stored in a separate JSON file
/// at `$XDG_DATA_HOME/fresh/file_states/{encoded_path}.json`.
/// This allows concurrent editors to safely update different files without
/// conflicts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedFileState {
    /// Schema version for future migrations
    pub version: u32,

    /// The file state (cursor, scroll, etc.)
    pub state: SerializedFileState,

    /// Timestamp when last saved (Unix epoch seconds)
    pub saved_at: u64,
}

impl PersistedFileState {
    fn new(state: SerializedFileState) -> Self {
        Self {
            version: FILE_WORKSPACE_VERSION,
            state,
            saved_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

/// Per-file workspace storage for scroll/cursor positions
///
/// Unlike project workspaces which store file states relative to a working directory,
/// this stores file states by absolute path so they persist across projects.
/// This means opening the same file from different projects (or without a project)
/// will restore the same scroll/cursor position.
///
/// Each file's state is stored in a separate JSON file at
/// `$XDG_DATA_HOME/fresh/file_states/{encoded_path}.json` to avoid conflicts
/// between concurrent editors. States are loaded lazily when opening files
/// and saved immediately when closing files or saving the workspace.
pub struct PersistedFileWorkspace;

impl PersistedFileWorkspace {
    /// Get the directory for file state files
    fn states_dir() -> io::Result<PathBuf> {
        Ok(get_data_dir()?.join("file_states"))
    }

    /// Get the state file path for a source file
    fn state_file_path(source_path: &Path) -> io::Result<PathBuf> {
        let canonical = source_path
            .canonicalize()
            .unwrap_or_else(|_| source_path.to_path_buf());
        let filename = format!("{}.json", encode_path_for_filename(&canonical));
        Ok(Self::states_dir()?.join(filename))
    }

    /// Load the state for a file by its absolute path (from disk)
    pub fn load(path: &Path) -> Option<SerializedFileState> {
        let state_path = match Self::state_file_path(path) {
            Ok(p) => p,
            Err(_) => return None,
        };

        if !state_path.exists() {
            return None;
        }

        let content = match std::fs::read_to_string(&state_path) {
            Ok(c) => c,
            Err(_) => return None,
        };

        let persisted: PersistedFileState = match serde_json::from_str(&content) {
            Ok(p) => p,
            Err(_) => return None,
        };

        // Check version compatibility
        if persisted.version > FILE_WORKSPACE_VERSION {
            return None;
        }

        Some(persisted.state)
    }

    /// Save the state for a file by its absolute path (to disk, atomic write)
    pub fn save(path: &Path, state: SerializedFileState) {
        let state_path = match Self::state_file_path(path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to get state path for {:?}: {}", path, e);
                return;
            }
        };

        // Ensure directory exists
        if let Some(parent) = state_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("Failed to create state dir: {}", e);
                return;
            }
        }

        let persisted = PersistedFileState::new(state);
        let content = match serde_json::to_string_pretty(&persisted) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to serialize file state: {}", e);
                return;
            }
        };

        // Write atomically: temp file + rename
        let temp_path = state_path.with_extension("json.tmp");

        let write_result = (|| -> io::Result<()> {
            let mut file = std::fs::File::create(&temp_path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temp_path, &state_path)?;
            Ok(())
        })();

        if let Err(e) = write_result {
            tracing::warn!("Failed to save file state for {:?}: {}", path, e);
        } else {
            tracing::trace!("File state saved for {:?}", path);
        }
    }
}

// ============================================================================
// Workspace file management
// ============================================================================

/// Get the workspaces directory
pub fn get_workspaces_dir() -> io::Result<PathBuf> {
    Ok(get_data_dir()?.join("workspaces"))
}

/// Encode a path into a filesystem-safe filename using percent encoding
///
/// Keeps alphanumeric chars, `-`, `.`, `_` as-is.
/// Replaces `/` with `_` for readability.
/// Percent-encodes other special characters as %XX.
///
/// Example: `/home/user/my project` -> `home_user_my%20project`
pub fn encode_path_for_filename(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    let mut result = String::with_capacity(path_str.len() * 2);

    for c in path_str.chars() {
        match c {
            // Path separators become underscores for readability
            '/' | '\\' => result.push('_'),
            // Safe chars pass through
            c if c.is_ascii_alphanumeric() => result.push(c),
            '-' | '.' => result.push(c),
            // Underscore needs special handling to avoid collision with /
            '_' => result.push_str("%5F"),
            // Everything else gets percent-encoded
            c => {
                for byte in c.to_string().as_bytes() {
                    result.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }

    // Remove leading underscores (from leading /)
    let result = result.trim_start_matches('_').to_string();

    // Collapse multiple underscores
    let mut final_result = String::with_capacity(result.len());
    let mut last_was_underscore = false;
    for c in result.chars() {
        if c == '_' {
            if !last_was_underscore {
                final_result.push(c);
            }
            last_was_underscore = true;
        } else {
            final_result.push(c);
            last_was_underscore = false;
        }
    }

    if final_result.is_empty() {
        final_result = "root".to_string();
    }

    final_result
}

/// Decode a filename back to the original path (for debugging/tooling)
#[allow(dead_code)]
pub fn decode_filename_to_path(encoded: &str) -> Option<PathBuf> {
    if encoded == "root" {
        return Some(PathBuf::from("/"));
    }

    let mut result = String::with_capacity(encoded.len() + 1);
    // Re-add leading slash that was stripped during encoding
    result.push('/');

    let mut chars = encoded.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            // Read two hex digits
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    result.push(byte as char);
                }
            }
        } else if c == '_' {
            result.push('/');
        } else {
            result.push(c);
        }
    }

    Some(PathBuf::from(result))
}

/// Legacy (pre-stable-id) workspace file path for a working directory:
/// the filename is the encoded canonical root. Still used as the write
/// fallback for snapshots without a `stable_id`, and by the one-time
/// `windows.json` migration. Reads must NOT assume this name — see
/// [`find_workspace_file_by_root`].
pub fn get_workspace_path(working_dir: &Path) -> io::Result<PathBuf> {
    Ok(get_workspace_path_in_dir(
        &get_workspaces_dir()?,
        working_dir,
    ))
}

fn get_workspace_path_in_dir(workspaces_dir: &Path, working_dir: &Path) -> PathBuf {
    let canonical = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let filename = format!("{}.json", encode_path_for_filename(&canonical));
    workspaces_dir.join(filename)
}

/// Make a stable id filename-safe. The minted alphabet (`ws-<hex>-<hex>`)
/// already is; this is a defensive net so a hand-edited file can never
/// smuggle path separators or dots into a workspace filename (dots are the
/// root/id delimiter — see [`workspace_path_for`]).
fn sanitize_stable_id(stable_id: &str) -> String {
    stable_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug)]
struct WorkspaceRootLockInner {
    _file: std::fs::File,
}

/// Process-reentrant, host-wide lock for every persistence mutation at one
/// canonical workspace root. A retained lifecycle owner keeps the same
/// underlying file lock alive while ordinary saves/deletes in this process
/// re-enter through the shared `Arc`; another Fresh process cannot publish at
/// that root until the final owner releases it.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceRootLock {
    _inner: Arc<WorkspaceRootLockInner>,
}

static WORKSPACE_ROOT_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<WorkspaceRootLockInner>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static WORKSPACE_ROOT_OWNERS: LazyLock<Mutex<HashMap<String, (PathBuf, WorkspaceRootLock)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn canonical_workspace_root(root: &Path) -> PathBuf {
    if let Ok(canonical) = root.canonicalize() {
        return canonical;
    }

    // Lifecycle operations may rename the owned root before replay. Resolve the
    // nearest surviving ancestor so the same path keeps the same lock identity
    // even after its final component no longer exists (notably /var -> /private/var
    // on macOS).
    let mut suffix = Vec::new();
    let mut ancestor = root;
    while let Some(name) = ancestor.file_name() {
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        if let Ok(mut canonical) = parent.canonicalize() {
            for component in suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        ancestor = parent;
    }
    root.to_path_buf()
}

fn workspace_root_lock_path(workspaces_dir: &Path, root: &Path) -> PathBuf {
    workspaces_dir.join(format!(
        ".root-{}.lock",
        encode_path_for_filename(&canonical_workspace_root(root))
    ))
}

fn workspace_root_lock_in(
    workspaces_dir: &Path,
    root: &Path,
    wait: bool,
) -> io::Result<WorkspaceRootLock> {
    std::fs::create_dir_all(workspaces_dir)?;
    let root = canonical_workspace_root(root);
    let mut locks = WORKSPACE_ROOT_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = locks.get(&root).and_then(Weak::upgrade) {
        return Ok(WorkspaceRootLock { _inner: existing });
    }
    locks.remove(&root);

    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(workspace_root_lock_path(workspaces_dir, &root))?;
    if wait {
        file.lock()?;
    } else {
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "workspace root is owned by another publisher: {}",
                        root.display()
                    ),
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
    let inner = Arc::new(WorkspaceRootLockInner { _file: file });
    locks.insert(root, Arc::downgrade(&inner));
    Ok(WorkspaceRootLock { _inner: inner })
}

pub(crate) fn lock_workspace_root(
    dir_context: &crate::config_io::DirectoryContext,
    root: &Path,
) -> io::Result<WorkspaceRootLock> {
    workspace_root_lock_in(&dir_context.workspaces_dir(), root, true)
}

fn validate_workspace_owner_id(owner_id: &str) -> io::Result<()> {
    if owner_id.is_empty()
        || !owner_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace owner id must contain only ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

pub(crate) fn acquire_workspace_root_ownership(
    dir_context: &crate::config_io::DirectoryContext,
    root: &Path,
    owner_id: &str,
) -> io::Result<()> {
    validate_workspace_owner_id(owner_id)?;
    let root = canonical_workspace_root(root);
    {
        let owners = WORKSPACE_ROOT_OWNERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((owned_root, _)) = owners.get(owner_id) {
            return if *owned_root == root {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "workspace owner id is already bound to another root",
                ))
            };
        }
        if owners.values().any(|(owned_root, _)| *owned_root == root) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workspace root is already owned by another lifecycle",
            ));
        }
    }

    let guard = workspace_root_lock_in(&dir_context.workspaces_dir(), &root, false)?;
    let mut owners = WORKSPACE_ROOT_OWNERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if owners.values().any(|(owned_root, _)| *owned_root == root) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "workspace root is already owned by another lifecycle",
        ));
    }
    owners.insert(owner_id.to_string(), (root, guard));
    Ok(())
}

pub(crate) fn release_workspace_root_ownership(owner_id: &str) -> io::Result<()> {
    validate_workspace_owner_id(owner_id)?;
    WORKSPACE_ROOT_OWNERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(owner_id);
    Ok(())
}

fn workspace_root_ownership(owner_id: &str) -> io::Result<(PathBuf, WorkspaceRootLock)> {
    validate_workspace_owner_id(owner_id)?;
    WORKSPACE_ROOT_OWNERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(owner_id)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace lifecycle owner is not active",
            )
        })
}

fn require_workspace_root_ownership(owner_id: &str, root: &Path) -> io::Result<WorkspaceRootLock> {
    let (owned_root, guard) = workspace_root_ownership(owner_id)?;
    if owned_root == canonical_workspace_root(root) {
        Ok(guard)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace lifecycle does not own the requested root",
        ))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceArtifactQuarantine {
    stable_id: Option<String>,
    #[serde(default)]
    source_root: Option<PathBuf>,
    #[serde(default)]
    restored_to: Option<PathBuf>,
    had_artifacts: bool,
}

fn workspace_artifact_quarantine_dir(
    dir_context: &crate::config_io::DirectoryContext,
    owner_id: &str,
) -> PathBuf {
    dir_context
        .data_dir
        .join("workspace-artifact-quarantine")
        .join(owner_id)
}

fn workspace_artifact_source(
    dir_context: &crate::config_io::DirectoryContext,
    root: &Path,
    stable_id: Option<&str>,
) -> PathBuf {
    stable_id.map_or_else(
        || dir_context.terminal_dir_for(root),
        |stable_id| terminal_artifacts_dir(dir_context, root, stable_id),
    )
}

/// Move an exact terminal-artifact namespace into a durable lifecycle stage.
/// The prepared manifest is published before the rename, so retry after a crash
/// deterministically completes the same move. The caller must retain root
/// ownership until it chooses restore, retained archive, or delete-only purge.
pub(crate) fn quarantine_workspace_artifacts(
    dir_context: &crate::config_io::DirectoryContext,
    root: &Path,
    stable_id: Option<&str>,
    owner_id: &str,
) -> io::Result<()> {
    let _ownership = require_workspace_root_ownership(owner_id, root)?;
    let root_identity = canonical_workspace_root(root);
    if stable_id == Some("") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace stable id must not be empty",
        ));
    }
    let stage = workspace_artifact_quarantine_dir(dir_context, owner_id);
    let manifest_path = stage.join("manifest.json");
    let payload = stage.join("artifacts");
    let source = workspace_artifact_source(dir_context, root, stable_id);

    if manifest_path.exists() {
        let manifest: WorkspaceArtifactQuarantine =
            serde_json::from_slice(&std::fs::read(&manifest_path)?)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if manifest
            .source_root
            .as_ref()
            .is_some_and(|source_root| canonical_workspace_root(source_root) != root_identity)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace artifact owner id is already bound to another root",
            ));
        }
        if manifest.stable_id.as_deref() != stable_id {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace artifact owner id is already bound to another identity",
            ));
        }
        if manifest.restored_to.is_some() {
            return Ok(());
        }
        return match (manifest.had_artifacts, source.exists(), payload.exists()) {
            (false, false, false) | (true, false, true) => Ok(()),
            (true, true, false) => durable_rename(&source, &payload),
            _ => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace artifact quarantine is ambiguous",
            )),
        };
    }

    std::fs::create_dir_all(&stage)?;
    let manifest = WorkspaceArtifactQuarantine {
        stable_id: stable_id.map(str::to_owned),
        source_root: Some(root.to_path_buf()),
        restored_to: None,
        had_artifacts: source.exists(),
    };
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    atomic_write_workspace_with(&manifest_path, &bytes, |_| Ok(()), sync_workspace_parent)?;
    if manifest.had_artifacts {
        durable_rename(&source, &payload)?;
    }
    Ok(())
}

/// Restore a staged artifact namespace, optionally at a new root after a
/// collision-safe archive move. Success consumes the payload but retains a
/// small durable completion receipt so replay after a crash is idempotent.
pub(crate) fn restore_workspace_artifacts(
    dir_context: &crate::config_io::DirectoryContext,
    target_root: &Path,
    stable_id: Option<&str>,
    owner_id: &str,
) -> io::Result<()> {
    let (owned_root, _ownership) = workspace_root_ownership(owner_id)?;
    let stage = workspace_artifact_quarantine_dir(dir_context, owner_id);
    let manifest_path = stage.join("manifest.json");
    let mut manifest: WorkspaceArtifactQuarantine =
        serde_json::from_slice(&std::fs::read(&manifest_path)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if manifest
        .source_root
        .as_ref()
        .is_some_and(|source_root| canonical_workspace_root(source_root) != owned_root)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace artifact quarantine belongs to another root ownership",
        ));
    }
    if manifest.stable_id.as_deref() != stable_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace artifact identity does not match its quarantine",
        ));
    }
    let target_identity = canonical_workspace_root(target_root);
    let _target_lock = if target_identity == owned_root {
        None
    } else {
        let target_owned_elsewhere = WORKSPACE_ROOT_OWNERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|(candidate_owner, (candidate_root, _))| {
                candidate_owner != owner_id && *candidate_root == target_identity
            });
        if target_owned_elsewhere {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workspace artifact restore target is owned by another lifecycle",
            ));
        }
        Some(workspace_root_lock_in(
            &dir_context.workspaces_dir(),
            &target_identity,
            false,
        )?)
    };
    if let Some(restored_to) = manifest.restored_to.as_ref() {
        if canonical_workspace_root(restored_to) != target_identity {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace artifact quarantine was already restored to another root",
            ));
        }
        if !manifest.had_artifacts {
            return Ok(());
        }
        let payload = stage.join("artifacts");
        let destination = workspace_artifact_source(dir_context, restored_to, stable_id);
        return match (payload.exists(), destination.exists()) {
            (false, true) => Ok(()),
            (true, false) => {
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                durable_rename(&payload, &destination)
            }
            (true, true) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "completed workspace artifact restore is ambiguous",
            )),
            (false, false) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "completed workspace artifact restore is missing its payload and destination",
            )),
        };
    }
    let payload = stage.join("artifacts");
    let destination = workspace_artifact_source(dir_context, target_root, stable_id);
    match (
        manifest.had_artifacts,
        payload.exists(),
        destination.exists(),
    ) {
        (false, false, false) | (true, false, true) => {}
        (true, true, false) => {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            durable_rename(&payload, &destination)?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace artifact restore is ambiguous",
            ));
        }
    }
    manifest.restored_to = Some(target_root.to_path_buf());
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    atomic_write_workspace_with(&manifest_path, &bytes, |_| Ok(()), sync_workspace_parent)
}

/// Permanently discard a staged namespace. Lifecycle callers use this only
/// after committed Delete; Archive deliberately retains the stage.
pub(crate) fn purge_workspace_artifact_quarantine(
    dir_context: &crate::config_io::DirectoryContext,
    owner_id: &str,
) -> io::Result<()> {
    let (owned_root, _ownership) = workspace_root_ownership(owner_id)?;
    let stage = workspace_artifact_quarantine_dir(dir_context, owner_id);
    if stage.exists() {
        let manifest: WorkspaceArtifactQuarantine =
            serde_json::from_slice(&std::fs::read(stage.join("manifest.json"))?)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if manifest
            .source_root
            .as_ref()
            .is_some_and(|source_root| canonical_workspace_root(source_root) != owned_root)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace artifact quarantine belongs to another root ownership",
            ));
        }
    }
    match std::fs::remove_dir_all(&stage) {
        Ok(()) => sync_workspace_parent(&stage),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
/// Local transcript directory for one durable workspace identity. The project
/// root remains a readable locator, while `stable_id` prevents co-tenant
/// windows at that root from opening/truncating each other's terminal files.
pub fn terminal_artifacts_dir(
    dir_context: &crate::config_io::DirectoryContext,
    working_dir: &Path,
    stable_id: &str,
) -> PathBuf {
    let root = dir_context.terminal_dir_for(working_dir);
    if stable_id.is_empty() {
        root
    } else {
        root.join(sanitize_stable_id(stable_id))
    }
}

/// Remove terminal artifacts for one durable workspace identity, then durably
/// publish the directory removal. Missing artifacts are already forgotten.
pub(crate) fn delete_terminal_artifacts_by_id(
    dir_context: &crate::config_io::DirectoryContext,
    working_dir: &Path,
    stable_id: &str,
) -> io::Result<()> {
    let _root_lock = lock_workspace_root(dir_context, working_dir)?;
    delete_terminal_artifact_directory(&terminal_artifacts_dir(dir_context, working_dir, stable_id))
}

/// Remove every terminal-artifact co-tenant at one root. The caller must first
/// prove that no live or draining window still owns the root.
pub(crate) fn delete_terminal_artifacts_for_root(
    dir_context: &crate::config_io::DirectoryContext,
    working_dir: &Path,
) -> io::Result<()> {
    let _root_lock = lock_workspace_root(dir_context, working_dir)?;
    delete_terminal_artifact_directory(&dir_context.terminal_dir_for(working_dir))
}

fn delete_terminal_artifact_directory(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => sync_workspace_parent(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// One local terminal artifact ownership move performed by workspace
/// extraction. The pair is journaled before the first rename so startup can
/// deterministically roll a prepared move back or finish a committed one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TerminalArtifactRelocation {
    pub source: PathBuf,
    pub destination: PathBuf,
    /// Source-authoritative files remain in place until source metadata no
    /// longer advertises them. This includes lock pathnames and the immutable
    /// serialized checkpoint. Older journals moved everything in one phase,
    /// so absent means an ordinary pre-cutover artifact.
    #[serde(default, skip_serializing_if = "is_false")]
    pub after_source_cutover: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(crate) enum TerminalExtractionPhase {
    Prepared,
    Committed {
        source_after: Workspace,
        target_after: Workspace,
    },
}

/// Durable extraction intent. `source_before` is the authoritative rollback
/// snapshot until `phase` atomically changes to `Committed`; after that the two
/// post-cutover snapshots are authoritative and recovery only finishes forward.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TerminalExtractionIntent {
    pub id: String,
    pub source_before: Workspace,
    pub target_root: PathBuf,
    pub target_stable_id: String,
    pub artifacts: Vec<TerminalArtifactRelocation>,
    pub phase: TerminalExtractionPhase,
}

impl TerminalExtractionIntent {
    pub(crate) fn prepared(
        source_before: Workspace,
        target_root: PathBuf,
        target_stable_id: String,
        artifacts: Vec<TerminalArtifactRelocation>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            source_before,
            target_root,
            target_stable_id,
            artifacts,
            phase: TerminalExtractionPhase::Prepared,
        }
    }
}

fn terminal_extraction_dir(dir_context: &crate::config_io::DirectoryContext) -> PathBuf {
    dir_context.data_dir.join("terminal-extractions")
}

pub(crate) fn terminal_extraction_intent_path(
    dir_context: &crate::config_io::DirectoryContext,
    id: &str,
) -> PathBuf {
    terminal_extraction_dir(dir_context).join(format!("{id}.json"))
}

pub(crate) fn persist_terminal_extraction_intent(
    dir_context: &crate::config_io::DirectoryContext,
    intent: &TerminalExtractionIntent,
) -> io::Result<PathBuf> {
    let path = terminal_extraction_intent_path(dir_context, &intent.id);
    std::fs::create_dir_all(path.parent().expect("extraction intent has parent"))?;
    let bytes = serde_json::to_vec_pretty(intent).map_err(io::Error::other)?;
    atomic_write_workspace_with(&path, &bytes, |_| Ok(()), sync_workspace_parent)?;
    Ok(path)
}

pub(crate) fn clear_terminal_extraction_intent(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_workspace_parent(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Recover every extraction journal before workspace discovery. Prepared
/// transactions restore the exact source snapshot; committed transactions
/// finish publishing both post-cutover snapshots. A failed rename leaves the
/// journal and every surviving artifact in place for the next startup.
pub(crate) fn recover_terminal_extractions(
    dir_context: &crate::config_io::DirectoryContext,
) -> io::Result<()> {
    let dir = terminal_extraction_dir(dir_context);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut first_error = None;
    for entry in entries {
        let result =
            entry.and_then(|entry| recover_terminal_extraction(dir_context, &entry.path()));
        if let Err(error) = result {
            tracing::error!("terminal extraction recovery deferred: {error}");
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn recover_terminal_extraction(
    dir_context: &crate::config_io::DirectoryContext,
    path: &Path,
) -> io::Result<()> {
    let intent: TerminalExtractionIntent = serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut roots = vec![
        intent.source_before.working_dir.clone(),
        intent.target_root.clone(),
    ];
    roots.sort();
    roots.dedup();
    let _root_locks = roots
        .iter()
        .map(|root| lock_workspace_root(dir_context, root))
        .collect::<io::Result<Vec<_>>>()?;
    match &intent.phase {
        TerminalExtractionPhase::Prepared => {
            for move_ in intent.artifacts.iter().rev() {
                restore_relocated_artifact(move_)?;
            }
            intent
                .source_before
                .save_in(dir_context)
                .map_err(|error| io::Error::other(error.to_string()))?;
            Workspace::delete_by_id_in(dir_context, &intent.target_root, &intent.target_stable_id)
                .map_err(|error| io::Error::other(error.to_string()))?;
            let target_dir =
                terminal_artifacts_dir(dir_context, &intent.target_root, &intent.target_stable_id);
            match std::fs::remove_dir(&target_dir) {
                Ok(()) => sync_workspace_parent(&target_dir)?,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        TerminalExtractionPhase::Committed {
            source_after,
            target_after,
        } => {
            for move_ in &intent.artifacts {
                finish_relocated_artifact(move_)?;
            }
            target_after
                .save_in(dir_context)
                .map_err(|error| io::Error::other(error.to_string()))?;
            source_after
                .save_in(dir_context)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
    }
    clear_terminal_extraction_intent(path)
}

fn restore_relocated_artifact(move_: &TerminalArtifactRelocation) -> io::Result<()> {
    match (move_.source.exists(), move_.destination.exists()) {
        (true, false) => Ok(()),
        (false, true) => durable_rename(&move_.destination, &move_.source),
        (true, true) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "both extraction artifact locations exist: {} and {}",
                move_.source.display(),
                move_.destination.display()
            ),
        )),
        (false, false) => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "extraction artifact missing from both {} and {}",
                move_.source.display(),
                move_.destination.display()
            ),
        )),
    }
}

fn finish_relocated_artifact(move_: &TerminalArtifactRelocation) -> io::Result<()> {
    match (move_.source.exists(), move_.destination.exists()) {
        (false, true) => Ok(()),
        (true, false) => durable_rename(&move_.source, &move_.destination),
        (true, true) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "both committed extraction artifact locations exist: {} and {}",
                move_.source.display(),
                move_.destination.display()
            ),
        )),
        (false, false) => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "committed extraction artifact missing from both {} and {}",
                move_.source.display(),
                move_.destination.display()
            ),
        )),
    }
}

/// Workspace file path for a workspace with a durable id:
/// `workspaces/<encoded-root>.<stable_id>.json`.
///
/// The encoded root is a *locator index*, not the identity — lookup
/// prefilters on it without reading file contents, while `stable_id`
/// is what distinguishes the workspace (and, in the future, same-root
/// siblings). Content (`working_dir` inside the file) stays authoritative
/// wherever names collide.
pub fn workspace_path_for(working_dir: &Path, stable_id: &str) -> io::Result<PathBuf> {
    Ok(workspace_path_for_in_dir(
        &get_workspaces_dir()?,
        working_dir,
        stable_id,
    ))
}

fn workspace_path_for_in_dir(
    workspaces_dir: &Path,
    working_dir: &Path,
    stable_id: &str,
) -> PathBuf {
    let canonical = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let filename = format!(
        "{}.{}.json",
        encode_path_for_filename(&canonical),
        sanitize_stable_id(stable_id)
    );
    workspaces_dir.join(filename)
}

/// Identity fields of a candidate workspace file for one directory.
struct WorkspaceFileIdentity {
    path: PathBuf,
    saved_at: u64,
    stable_id: Option<String>,
}

/// All workspace files claiming `working_dir`: the legacy root-keyed name
/// plus every `<encoded-root>.<id>.json` sibling. Filenames are only a
/// prefilter — an encoded root can be a prefix of another's (e.g. `/a`
/// vs `/a.b`), so each candidate's recorded `working_dir` is verified
/// before it counts. Unparseable files are skipped (a torn write just
/// means "not a workspace").

fn candidate_files_for_root_in_dir(
    workspaces_dir: &Path,
    working_dir: &Path,
) -> io::Result<Vec<WorkspaceFileIdentity>> {
    let target = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let encoded = encode_path_for_filename(&target);
    let legacy_name = format!("{encoded}.json");
    let id_prefix = format!("{encoded}.");

    let entries = match std::fs::read_dir(workspaces_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != legacy_name && !(name.starts_with(&id_prefix) && name.ends_with(".json")) {
            continue;
        }
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
            tracing::warn!(
                "Ignoring unparseable workspace file {:?} while resolving {:?}",
                path,
                working_dir
            );
            continue;
        };
        let claimed = val
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let Some(claimed) = claimed else {
            continue;
        };
        let claimed_canonical = claimed.canonicalize().unwrap_or_else(|_| claimed.clone());
        if claimed_canonical != target && claimed != working_dir {
            continue;
        }
        found.push(WorkspaceFileIdentity {
            path,
            saved_at: val.get("saved_at").and_then(|v| v.as_u64()).unwrap_or(0),
            stable_id: val
                .get("stable_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        });
    }
    Ok(found)
}

/// Strict host-side persistence inventory for lifecycle transactions. A file
/// whose name may belong to `working_dir` is never silently skipped when its
/// contents are unreadable or ambiguous; the caller must preserve everything
/// rather than proceed from an incomplete deletion guard.
pub fn inspect_workspace_persistence(
    working_dir: &Path,
) -> Result<Vec<fresh_core::api::WorkspacePersistenceFile>, WorkspaceError> {
    inspect_workspace_persistence_in_dir(&get_workspaces_dir()?, working_dir)
}

pub fn inspect_workspace_persistence_in(
    dir_context: &crate::config_io::DirectoryContext,
    working_dir: &Path,
) -> Result<Vec<fresh_core::api::WorkspacePersistenceFile>, WorkspaceError> {
    inspect_workspace_persistence_in_dir(&dir_context.workspaces_dir(), working_dir)
}

fn inspect_workspace_persistence_in_dir(
    workspaces_dir: &Path,
    working_dir: &Path,
) -> Result<Vec<fresh_core::api::WorkspacePersistenceFile>, WorkspaceError> {
    let target = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let encoded = encode_path_for_filename(&target);
    let legacy_name = format!("{encoded}.json");
    let id_prefix = format!("{encoded}.");
    let entries = match std::fs::read_dir(workspaces_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != legacy_name && !(name.starts_with(&id_prefix) && name.ends_with(".json")) {
            continue;
        }
        let path = entry.path();
        let content = std::fs::read_to_string(&path)?;
        let value: serde_json::Value = serde_json::from_str(&content)?;
        let claimed = value
            .get("working_dir")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("workspace file {} has no working_dir", path.display()),
                )
            })?;
        let claimed_canonical = claimed.canonicalize().unwrap_or_else(|_| claimed.clone());
        if claimed_canonical != target && claimed != working_dir {
            continue;
        }
        let workspace: Workspace = serde_json::from_value(value)?;
        if workspace.version > WORKSPACE_VERSION {
            return Err(WorkspaceError::VersionTooNew {
                version: workspace.version,
                max_supported: WORKSPACE_VERSION,
            });
        }
        match &workspace.stable_id {
            None if name != legacy_name => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "id-keyed workspace file {} has no stable id",
                        path.display()
                    ),
                )
                .into());
            }
            Some(_) if name == legacy_name => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "legacy workspace file {} carries a stable id",
                        path.display()
                    ),
                )
                .into());
            }
            Some(stable_id) => {
                let expected = workspace_path_for_in_dir(workspaces_dir, working_dir, stable_id);
                if expected.file_name() != path.file_name() {
                    return Err(WorkspaceError::IdentityMismatch {
                        expected: stable_id.clone(),
                        found: workspace.stable_id.clone(),
                    });
                }
            }
            None => {}
        }
        files.push(fresh_core::api::WorkspacePersistenceFile {
            path: path.to_string_lossy().into_owned(),
            content,
            stable_id: workspace.stable_id,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn create_attempt_marker(workspace: &Workspace) -> Option<&str> {
    workspace
        .session_plugin_state
        .get("orchestrator")?
        .get("create_attempt")?
        .as_str()
}

fn created_workspace_identity(workspace: &Workspace) -> Result<(String, String), WorkspaceError> {
    let workspace_id = workspace.stable_id.clone().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "a created workspace marker has no durable stable id",
        )
    })?;
    Ok((
        workspace.working_dir.to_string_lossy().into_owned(),
        workspace_id,
    ))
}

fn select_created_workspace(
    marked: Vec<Workspace>,
    hinted: Vec<Workspace>,
) -> Result<Option<(String, String)>, WorkspaceError> {
    let candidates = if marked.is_empty() { hinted } else { marked };
    match candidates.len() {
        0 => Ok(None),
        1 => created_workspace_identity(&candidates[0]).map(Some),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace create-attempt inventory is ambiguous",
        )
        .into()),
    }
}

fn inspect_workspace_create_attempt_strict_in_dir(
    workspaces_dir: &Path,
    attempt_id: &str,
    root_hint: Option<&Path>,
    workspace_id_hint: Option<&str>,
) -> Result<Option<(String, String)>, WorkspaceError> {
    if let Some(root) = root_hint {
        let files = inspect_workspace_persistence_in_dir(workspaces_dir, root)?;
        let mut marked = Vec::new();
        let mut hinted = Vec::new();
        for file in files {
            let workspace: Workspace = serde_json::from_str(&file.content)?;
            if create_attempt_marker(&workspace) == Some(attempt_id) {
                marked.push(workspace);
            } else if workspace_id_hint.is_some_and(|id| workspace.stable_id.as_deref() == Some(id))
            {
                hinted.push(workspace);
            }
        }
        return select_created_workspace(marked, hinted);
    }

    let entries = match std::fs::read_dir(workspaces_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut marked = Vec::new();
    let mut hinted = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let content = std::fs::read_to_string(&path)?;
        let workspace: Workspace = serde_json::from_str(&content)?;
        if workspace.version > WORKSPACE_VERSION {
            return Err(WorkspaceError::VersionTooNew {
                version: workspace.version,
                max_supported: WORKSPACE_VERSION,
            });
        }
        let expected = match workspace.stable_id.as_deref() {
            Some(stable_id) => {
                workspace_path_for_in_dir(workspaces_dir, &workspace.working_dir, stable_id)
            }
            None => get_workspace_path_in_dir(workspaces_dir, &workspace.working_dir),
        };
        if expected.file_name() != path.file_name() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("workspace file {} has ambiguous identity", path.display()),
            )
            .into());
        }
        if create_attempt_marker(&workspace) == Some(attempt_id) {
            marked.push(workspace);
        } else if workspace_id_hint.is_some_and(|id| workspace.stable_id.as_deref() == Some(id)) {
            hinted.push(workspace);
        }
    }
    select_created_workspace(marked, hinted)
}

/// Locate one create attempt without ever collapsing an incomplete persistence
/// scan into "not found". The tagged result is consumed directly by the bundled
/// Orchestrator's Retry/Dismiss recovery paths.
pub fn inspect_workspace_create_attempt_in(
    dir_context: &crate::config_io::DirectoryContext,
    attempt_id: &str,
    root_hint: Option<&Path>,
    workspace_id_hint: Option<&str>,
) -> fresh_core::api::WorkspaceCreateAttemptInventory {
    match inspect_workspace_create_attempt_strict_in_dir(
        &dir_context.workspaces_dir(),
        attempt_id,
        root_hint,
        workspace_id_hint,
    ) {
        Ok(Some((root, workspace_id))) => {
            fresh_core::api::WorkspaceCreateAttemptInventory::Found { root, workspace_id }
        }
        Ok(None) => fresh_core::api::WorkspaceCreateAttemptInventory::NotFound,
        Err(error) => fresh_core::api::WorkspaceCreateAttemptInventory::Error {
            message: error.to_string(),
        },
    }
}

/// Ranking key used to arbitrate between multiple workspace files that
/// claim the same canonical root (a legacy root-keyed file plus its
/// re-keyed successor, mid-migration): the freshest snapshot wins —
/// highest `saved_at`, with a stable-id-bearing file breaking a
/// same-timestamp tie so a same-second migration can't resurrect the
/// pre-migration id-less copy.
///
/// This is the single source of truth for that rule. Boot-time session
/// discovery (`orchestrator_persistence::discover_sessions`) and the
/// per-root read chokepoint ([`find_workspace_file_by_root`]) feed the two
/// separate `stable_id` adoption sites, and MUST rank identically — if they
/// disagreed on which file is authoritative for a root, a window could adopt
/// one identity at boot and a different one at materialize. Keep both callers
/// routed through here.
pub fn workspace_freshness_rank(saved_at: u64, has_stable_id: bool) -> (u64, bool) {
    (saved_at, has_stable_id)
}

/// Find the workspace file for `working_dir`. When more than one file
/// claims the directory the freshest snapshot wins — see
/// [`workspace_freshness_rank`].
pub fn find_workspace_file_by_root(working_dir: &Path) -> io::Result<Option<PathBuf>> {
    find_workspace_file_by_root_in_dir(&get_workspaces_dir()?, working_dir)
}

pub fn find_workspace_file_by_root_in(
    dir_context: &crate::config_io::DirectoryContext,
    working_dir: &Path,
) -> io::Result<Option<PathBuf>> {
    find_workspace_file_by_root_in_dir(&dir_context.workspaces_dir(), working_dir)
}

fn find_workspace_file_by_root_in_dir(
    workspaces_dir: &Path,
    working_dir: &Path,
) -> io::Result<Option<PathBuf>> {
    let mut best: Option<WorkspaceFileIdentity> = None;
    for ident in candidate_files_for_root_in_dir(workspaces_dir, working_dir)? {
        let newer = match &best {
            None => true,
            Some(b) => {
                workspace_freshness_rank(ident.saved_at, ident.stable_id.is_some())
                    > workspace_freshness_rank(b.saved_at, b.stable_id.is_some())
            }
        };
        if newer {
            best = Some(ident);
        }
    }
    Ok(best.map(|b| b.path))
}

/// The retired daemon-scoped workspace directory.
///
/// Workspaces are one set now, shared by direct mode and every daemon, so
/// nothing writes here any more. The path survives only so boot migration can
/// find pre-existing snapshots and fold them into the real store — see
/// `orchestrator_persistence::migrate_session_workspaces_into_store`.
pub fn get_session_workspaces_dir() -> io::Result<PathBuf> {
    Ok(get_data_dir()?.join("session-workspaces"))
}

/// Workspace error types
#[derive(Debug)]
pub enum WorkspaceError {
    Io(anyhow::Error),
    Json(serde_json::Error),
    WorkdirMismatch {
        expected: PathBuf,
        found: PathBuf,
    },
    IdentityMismatch {
        expected: String,
        found: Option<String>,
    },
    VersionTooNew {
        version: u32,
        max_supported: u32,
    },
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "Workspace error: {}", e),
            Self::Json(e) => write!(f, "JSON error: {}", e),
            Self::WorkdirMismatch { expected, found } => {
                write!(
                    f,
                    "Working directory mismatch: expected {:?}, found {:?}",
                    expected, found
                )
            }
            Self::IdentityMismatch { expected, found } => {
                write!(
                    f,
                    "Workspace identity mismatch: expected {:?}, found {:?}",
                    expected, found
                )
            }
            WorkspaceError::VersionTooNew {
                version,
                max_supported,
            } => {
                write!(
                    f,
                    "Workspace version {} is newer than supported (max: {})",
                    version, max_supported
                )
            }
        }
    }
}

impl std::error::Error for WorkspaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => e.source(),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for WorkspaceError {
    fn from(e: io::Error) -> Self {
        WorkspaceError::Io(e.into())
    }
}

impl From<anyhow::Error> for WorkspaceError {
    fn from(e: anyhow::Error) -> Self {
        WorkspaceError::Io(e)
    }
}

impl From<serde_json::Error> for WorkspaceError {
    fn from(e: serde_json::Error) -> Self {
        WorkspaceError::Json(e)
    }
}

fn unique_workspace_temp(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace.json");
    path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()))
}

fn sync_workspace_parent(path: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("workspace path has no parent directory"))?;
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    {
        // `replace_workspace_temp` uses MOVEFILE_WRITE_THROUGH, Windows'
        // durable-publication boundary for the renamed directory entry.
        let _ = path;
        Ok(())
    }
}

/// Atomically rename a file or directory and make the directory-entry change
/// durable before callers publish metadata that names the destination.
#[cfg(not(windows))]
pub(crate) fn durable_rename(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)?;
    let source_parent = source
        .parent()
        .ok_or_else(|| io::Error::other("rename source has no parent directory"))?;
    let destination_parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("rename destination has no parent directory"))?;
    std::fs::File::open(source_parent)?.sync_all()?;
    if destination_parent != source_parent {
        std::fs::File::open(destination_parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn durable_rename(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn replace_workspace_temp(temp: &Path, target: &Path) -> io::Result<()> {
    durable_rename(temp, target)
}

fn atomic_write_workspace_with(
    path: &Path,
    content: &[u8],
    after_temp_sync: impl FnOnce(&Path) -> io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let temp = unique_workspace_temp(path);
    let publication = (|| -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        after_temp_sync(&temp)?;
        replace_workspace_temp(&temp, path)?;
        sync_parent(path)
    })();
    if publication.is_err() {
        #[allow(clippy::let_underscore_must_use)]
        let _ = std::fs::remove_file(&temp);
    }
    publication
}
impl Workspace {
    /// Load workspace for a working directory (if exists).
    ///
    /// Resolution goes by the `working_dir` recorded inside each workspace
    /// file (filenames are stable-id-keyed; legacy files are root-keyed) —
    /// see [`find_workspace_file_by_root`].
    pub fn load(working_dir: &Path) -> Result<Option<Workspace>, WorkspaceError> {
        Self::load_in_dir(&get_workspaces_dir()?, working_dir)
    }

    pub fn load_in(
        dir_context: &crate::config_io::DirectoryContext,
        working_dir: &Path,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        Self::load_in_dir(&dir_context.workspaces_dir(), working_dir)
    }

    fn load_in_dir(
        workspaces_dir: &Path,
        working_dir: &Path,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        let Some(path) = find_workspace_file_by_root_in_dir(workspaces_dir, working_dir)? else {
            tracing::debug!("No workspace file found for {:?}", working_dir);
            return Ok(None);
        };
        Self::load_from_path(&path, working_dir)
    }

    /// Load one exact durable co-tenant identity at `working_dir`.
    /// A missing exact file may adopt only the legacy root-keyed, id-less
    /// snapshot; an id-bearing sibling is never a fallback.
    pub fn load_by_id(
        working_dir: &Path,
        stable_id: &str,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        Self::load_by_id_in_dir(&get_workspaces_dir()?, working_dir, stable_id)
    }

    pub fn load_by_id_in(
        dir_context: &crate::config_io::DirectoryContext,
        working_dir: &Path,
        stable_id: &str,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        Self::load_by_id_in_dir(&dir_context.workspaces_dir(), working_dir, stable_id)
    }

    fn load_by_id_in_dir(
        workspaces_dir: &Path,
        working_dir: &Path,
        stable_id: &str,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        let path = workspace_path_for_in_dir(workspaces_dir, working_dir, stable_id);
        if path.exists() {
            let workspace = Self::load_from_path(&path, working_dir)?;
            if let Some(workspace) = &workspace {
                if workspace.stable_id.as_deref() != Some(stable_id) {
                    return Err(WorkspaceError::IdentityMismatch {
                        expected: stable_id.to_string(),
                        found: workspace.stable_id.clone(),
                    });
                }
            }
            return Ok(workspace);
        }

        let legacy = get_workspace_path_in_dir(workspaces_dir, working_dir);
        let workspace = Self::load_from_path(&legacy, working_dir)?;
        match workspace {
            Some(workspace) if workspace.stable_id.is_none() => Ok(Some(workspace)),
            Some(workspace) => Err(WorkspaceError::IdentityMismatch {
                expected: stable_id.to_string(),
                found: workspace.stable_id,
            }),
            None => Ok(None),
        }
    }

    /// Read, parse, and validate a workspace file at `path`, checking it
    /// claims `expected_working_dir` and isn't from a newer schema.
    fn load_from_path(
        path: &Path,
        expected_working_dir: &Path,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        if !path.exists() {
            tracing::debug!("Workspace file does not exist: {:?}", path);
            return Ok(None);
        }

        tracing::debug!("Loading workspace from {:?}", path);
        let content = std::fs::read_to_string(path)?;
        let workspace: Workspace = serde_json::from_str(&content)?;

        tracing::debug!(
            "Loaded workspace: version={}, split_states={}, active_split={}",
            workspace.version,
            workspace.split_states.len(),
            workspace.active_split_id
        );

        // Validate working_dir matches (canonicalize both for comparison)
        let expected = expected_working_dir
            .canonicalize()
            .unwrap_or_else(|_| expected_working_dir.to_path_buf());
        let found = workspace
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| workspace.working_dir.clone());

        if expected != found {
            tracing::warn!(
                "Workspace working_dir mismatch: expected {:?}, found {:?}",
                expected,
                found
            );
            return Err(WorkspaceError::WorkdirMismatch { expected, found });
        }

        // Check version compatibility
        if workspace.version > WORKSPACE_VERSION {
            tracing::warn!(
                "Workspace version {} is newer than supported {}",
                workspace.version,
                WORKSPACE_VERSION
            );
            return Err(WorkspaceError::VersionTooNew {
                version: workspace.version,
                max_supported: WORKSPACE_VERSION,
            });
        }

        Ok(Some(workspace))
    }

    /// `true` when this workspace snapshot doesn't reference any
    /// real buffer content — every split's open_tabs is empty, and
    /// there are no terminals, no unnamed buffers, and no external
    /// files. Virtual buffers (Dashboard, plugin scratch buffers)
    /// are stripped during serialisation, so a Dashboard-only quit
    /// produces a snapshot that looks identical to a truly empty
    /// one. Used by `save_workspace` to refuse to clobber a real
    /// on-disk workspace with such a snapshot.
    pub fn has_no_real_content(&self) -> bool {
        self.terminals.is_empty()
            && self.external_files.is_empty()
            && self.unnamed_buffers.is_empty()
            && self.split_states.values().all(|s| s.open_tabs.is_empty())
    }

    /// `true` when this snapshot has no file/unnamed content that a
    /// Dashboard-only quit should preserve. Unlike [`Self::has_no_real_content`],
    /// terminals do NOT count as preservable: a terminal is live runtime
    /// state, so once the user closes it the on-disk entry is stale and must
    /// not block `save_workspace` from writing the now-empty snapshot (which
    /// would otherwise resurrect the closed terminal on the next restart).
    pub fn has_no_preservable_content(&self) -> bool {
        self.external_files.is_empty()
            && self.unnamed_buffers.is_empty()
            && self.split_states.values().all(|s| {
                s.open_tabs
                    .iter()
                    .all(|t| matches!(t, SerializedTabRef::Terminal(_)))
            })
    }

    /// Save workspace with a durable atomic replacement.
    ///
    /// 1. Create a caller-unique temp in the same directory with `create_new`.
    /// 2. Write and sync the complete snapshot.
    /// 3. Atomically rename it to the final path.
    /// 4. Sync the parent directory before reporting success.
    pub fn save(&self) -> Result<(), WorkspaceError> {
        self.save_in_dir(&get_workspaces_dir()?)
    }

    pub fn save_in(
        &self,
        dir_context: &crate::config_io::DirectoryContext,
    ) -> Result<(), WorkspaceError> {
        self.save_in_dir(&dir_context.workspaces_dir())
    }

    fn save_in_dir(&self, workspaces_dir: &Path) -> Result<(), WorkspaceError> {
        let _root_lock = workspace_root_lock_in(workspaces_dir, &self.working_dir, true)?;
        let path = match &self.stable_id {
            Some(id) => workspace_path_for_in_dir(workspaces_dir, &self.working_dir, id),
            None => get_workspace_path_in_dir(workspaces_dir, &self.working_dir),
        };
        tracing::debug!("Saving workspace to {:?}", path);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let _identity_lock = if let Some(id) = &self.stable_id {
            let lock_path =
                workspaces_dir.join(format!(".stable-id-{}.lock", sanitize_stable_id(id)));
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(lock_path)?;
            lock.lock()?;
            match Self::load_from_path(&path, &self.working_dir)? {
                Some(existing) if existing.stable_id.as_deref() != Some(id.as_str()) => {
                    return Err(WorkspaceError::IdentityMismatch {
                        expected: id.clone(),
                        found: existing.stable_id,
                    });
                }
                Some(_) => {}
                None if stable_id_claimed_by_other_in(workspaces_dir, id, &self.working_dir) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("workspace stable id {id:?} is already claimed by another root"),
                    )
                    .into());
                }
                None => {}
            }
            Some(lock)
        } else {
            None
        };

        let content = serde_json::to_string_pretty(self)?;
        tracing::trace!("Workspace JSON size: {} bytes", content.len());

        atomic_write_workspace_with(&path, content.as_bytes(), |_| Ok(()), sync_workspace_parent)?;
        tracing::info!("Workspace saved to {:?}", path);

        if self.stable_id.is_some() {
            let legacy = get_workspace_path_in_dir(workspaces_dir, &self.working_dir);
            if legacy != path && legacy.exists() {
                tracing::info!(
                    "Retiring legacy workspace file {:?} (re-keyed to {:?})",
                    legacy,
                    path
                );
                match std::fs::remove_file(&legacy) {
                    Ok(()) => {
                        if let Err(error) = sync_workspace_parent(&legacy) {
                            tracing::debug!("Could not sync retired legacy workspace: {error}");
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::debug!("Could not retire legacy workspace file: {error}");
                    }
                }
            }
        }

        Ok(())
    }

    /// Delete *every* workspace file claiming a directory — every co-tenant
    /// identity plus any legacy root-keyed file. This is the whole-root
    /// teardown (the project directory itself is going away); to close a
    /// single session that shares its root with others, use
    /// [`Self::delete_by_id`] instead so its peers survive.
    ///
    /// Inventory is strict: unreadable or malformed name-matching files abort
    /// before the first unlink, so a lifecycle transaction never proceeds from
    /// an incomplete view of what must be forgotten. Once admitted, every file
    /// is attempted and the first hard error is returned after the sweep.
    pub fn delete(working_dir: &Path) -> Result<(), WorkspaceError> {
        Self::delete_in_dir(&get_workspaces_dir()?, working_dir)
    }

    pub fn delete_in(
        dir_context: &crate::config_io::DirectoryContext,
        working_dir: &Path,
    ) -> Result<(), WorkspaceError> {
        Self::delete_in_dir(&dir_context.workspaces_dir(), working_dir)
    }

    fn delete_in_dir(workspaces_dir: &Path, working_dir: &Path) -> Result<(), WorkspaceError> {
        let _root_lock = workspace_root_lock_in(workspaces_dir, working_dir, true)?;
        let admitted = inspect_workspace_persistence_in_dir(workspaces_dir, working_dir)?;
        let mut first_err: Option<io::Error> = None;
        let mut removed = false;
        for file in admitted {
            let path = PathBuf::from(file.path);
            match std::fs::remove_file(&path) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!("Failed to delete workspace file {:?}: {e}", path);
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if removed {
            let path = get_workspace_path_in_dir(workspaces_dir, working_dir);
            if let Err(error) = sync_workspace_parent(&path) {
                first_err.get_or_insert(error);
            }
        }
        match first_err {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    /// Delete a single workspace identity's file
    /// (`workspaces/<encoded-root>.<stable_id>.json`), leaving any co-tenant
    /// workspaces on the same root untouched. The content identity is validated
    /// before unlinking, so a corrupt or misnamed file is preserved for explicit
    /// recovery rather than deleted under the caller's requested identity.
    /// `NotFound` is success (already gone).
    pub fn delete_by_id(working_dir: &Path, stable_id: &str) -> Result<(), WorkspaceError> {
        Self::delete_by_id_in_dir(&get_workspaces_dir()?, working_dir, stable_id)
    }

    pub fn delete_by_id_in(
        dir_context: &crate::config_io::DirectoryContext,
        working_dir: &Path,
        stable_id: &str,
    ) -> Result<(), WorkspaceError> {
        Self::delete_by_id_in_dir(&dir_context.workspaces_dir(), working_dir, stable_id)
    }

    fn delete_by_id_in_dir(
        workspaces_dir: &Path,
        working_dir: &Path,
        stable_id: &str,
    ) -> Result<(), WorkspaceError> {
        let _root_lock = workspace_root_lock_in(workspaces_dir, working_dir, true)?;
        let path = workspace_path_for_in_dir(workspaces_dir, working_dir, stable_id);
        let Some(workspace) = Self::load_from_path(&path, working_dir)? else {
            return Ok(());
        };
        if workspace.stable_id.as_deref() != Some(stable_id) {
            return Err(WorkspaceError::IdentityMismatch {
                expected: stable_id.to_string(),
                found: workspace.stable_id,
            });
        }
        std::fs::remove_file(&path)?;
        sync_workspace_parent(&path)?;
        Ok(())
    }

    /// Create a new workspace with current timestamp
    pub fn new(working_dir: PathBuf) -> Self {
        Self {
            version: WORKSPACE_VERSION,
            working_dir,
            split_layout: SerializedSplitNode::Leaf {
                file_path: None,
                split_id: 0,
                label: None,
                unnamed_recovery_id: None,
                role: None,
            },
            active_split_id: 0,
            split_states: HashMap::new(),
            config_overrides: WorkspaceConfigOverrides::default(),
            file_explorer: FileExplorerState::default(),
            histories: WorkspaceHistories::default(),
            search_options: SearchOptions::default(),
            bookmarks: HashMap::new(),
            terminals: Vec::new(),
            tracked_agent_terminal: None,
            external_files: Vec::new(),
            read_only_files: Vec::new(),
            unnamed_buffers: Vec::new(),
            plugin_global_state: HashMap::new(),
            saved_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            label: None,
            session_plugin_state: HashMap::new(),
            authority_spec: crate::services::authority::SessionAuthoritySpec::Local,
            stable_id: None,
        }
    }

    /// Update the saved_at timestamp to now
    pub fn touch(&mut self) {
        self.saved_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_path_percent_encoding() {
        // Test basic path encoding - readable with underscores for separators
        let encoded = encode_path_for_filename(Path::new("/home/user/project"));
        assert_eq!(encoded, "home_user_project");
        assert!(!encoded.contains('/')); // No slashes in encoded output

        // Round-trip: encode then decode should give original path
        let decoded = decode_filename_to_path(&encoded).unwrap();
        assert_eq!(decoded, PathBuf::from("/home/user/project"));

        // Different paths should give different encodings
        let path1 = get_workspace_path(Path::new("/home/user/project")).unwrap();
        let path2 = get_workspace_path(Path::new("/home/user/other")).unwrap();
        assert_ne!(path1, path2);

        // Same path should give same encoding
        let path1_again = get_workspace_path(Path::new("/home/user/project")).unwrap();
        assert_eq!(path1, path1_again);

        // Filename should end with .json and be readable
        let filename = path1.file_name().unwrap().to_str().unwrap();
        assert!(filename.ends_with(".json"));
        assert!(filename.starts_with("home_user_project"));
    }

    #[test]
    fn stable_id_generation_retries_a_reserved_candidate() {
        let first = format!("test-ws-{}", uuid::Uuid::new_v4().simple());
        let second = format!("test-ws-{}", uuid::Uuid::new_v4().simple());
        assert_eq!(generate_stable_id_with(|| first.clone()), first);

        let mut attempts = 0;
        let generated = generate_stable_id_with(|| {
            attempts += 1;
            if attempts == 1 {
                first.clone()
            } else {
                second.clone()
            }
        });

        assert_eq!(generated, second);
        assert_eq!(attempts, 2);
    }

    #[test]
    fn terminal_artifacts_are_namespaced_by_stable_id() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_context = crate::config_io::DirectoryContext::for_testing(temp.path());
        let root = temp.path().join("shared-root");

        let first = terminal_artifacts_dir(&dir_context, &root, "ws-a");
        let second = terminal_artifacts_dir(&dir_context, &root, "ws-b");

        assert_ne!(first, second);
        assert_eq!(first.parent(), second.parent());
        assert_eq!(first.file_name().unwrap(), "ws-a");
        assert_eq!(second.file_name().unwrap(), "ws-b");
    }

    #[test]
    fn deleting_one_terminal_artifact_namespace_preserves_its_sibling() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_context = crate::config_io::DirectoryContext::for_testing(temp.path());
        let root = temp.path().join("shared-root");
        let first = terminal_artifacts_dir(&dir_context, &root, "ws-a");
        let second = terminal_artifacts_dir(&dir_context, &root, "ws-b");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("terminal.txt"), b"first").unwrap();
        std::fs::write(second.join("terminal.txt"), b"second").unwrap();

        delete_terminal_artifacts_by_id(&dir_context, &root, "ws-a").unwrap();

        assert!(!first.exists());
        assert_eq!(
            std::fs::read(second.join("terminal.txt")).unwrap(),
            b"second"
        );
    }

    #[test]
    fn artifact_quarantine_retargets_after_owned_root_move() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_context = crate::config_io::DirectoryContext::for_testing(temp.path());
        let root = temp.path().join("project");
        let archived_root = temp.path().join("project-archived");
        std::fs::create_dir_all(&root).unwrap();
        let stable_id = "ws-retarget";
        let sibling_id = "ws-sibling";
        let artifacts = terminal_artifacts_dir(&dir_context, &root, stable_id);
        let sibling = terminal_artifacts_dir(&dir_context, &root, sibling_id);
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(artifacts.join("terminal.txt"), b"retargeted").unwrap();
        std::fs::write(sibling.join("terminal.txt"), b"sibling").unwrap();
        let owner = format!("test-{}", uuid::Uuid::new_v4().simple());

        acquire_workspace_root_ownership(&dir_context, &root, &owner).unwrap();
        quarantine_workspace_artifacts(&dir_context, &root, Some(stable_id), &owner).unwrap();
        std::fs::rename(&root, &archived_root).unwrap();
        restore_workspace_artifacts(&dir_context, &archived_root, Some(stable_id), &owner).unwrap();
        // Crash replay may restart the lifecycle from quarantine; the retained
        // completion receipt makes both calls exact no-ops.
        quarantine_workspace_artifacts(&dir_context, &root, Some(stable_id), &owner).unwrap();
        restore_workspace_artifacts(&dir_context, &archived_root, Some(stable_id), &owner).unwrap();
        release_workspace_root_ownership(&owner).unwrap();

        assert_eq!(
            std::fs::read(
                terminal_artifacts_dir(&dir_context, &archived_root, stable_id)
                    .join("terminal.txt")
            )
            .unwrap(),
            b"retargeted"
        );
        assert_eq!(
            std::fs::read(sibling.join("terminal.txt")).unwrap(),
            b"sibling"
        );
    }

    #[test]
    fn completed_artifact_restore_replays_a_surviving_payload() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_context = crate::config_io::DirectoryContext::for_testing(temp.path());
        let source_root = temp.path().join("source");
        let target_root = temp.path().join("target");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&target_root).unwrap();
        let stable_id = "ws-replay";
        let source = terminal_artifacts_dir(&dir_context, &source_root, stable_id);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("terminal.txt"), b"recoverable").unwrap();
        let owner = format!("test-{}", uuid::Uuid::new_v4().simple());

        acquire_workspace_root_ownership(&dir_context, &source_root, &owner).unwrap();
        quarantine_workspace_artifacts(&dir_context, &source_root, Some(stable_id), &owner)
            .unwrap();
        restore_workspace_artifacts(&dir_context, &target_root, Some(stable_id), &owner).unwrap();

        let stage = workspace_artifact_quarantine_dir(&dir_context, &owner);
        let payload = stage.join("artifacts");
        let destination = terminal_artifacts_dir(&dir_context, &target_root, stable_id);
        durable_rename(&destination, &payload).unwrap();
        assert!(!destination.exists());
        assert!(payload.exists());

        restore_workspace_artifacts(&dir_context, &target_root, Some(stable_id), &owner).unwrap();
        release_workspace_root_ownership(&owner).unwrap();

        assert_eq!(
            std::fs::read(destination.join("terminal.txt")).unwrap(),
            b"recoverable"
        );
        assert!(!payload.exists());
    }

    #[test]
    fn root_ownership_rejects_a_second_lifecycle() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_context = crate::config_io::DirectoryContext::for_testing(temp.path());
        let root = temp.path().join("shared-root");
        std::fs::create_dir_all(&root).unwrap();
        let first = format!("first-{}", uuid::Uuid::new_v4().simple());
        let second = format!("second-{}", uuid::Uuid::new_v4().simple());

        acquire_workspace_root_ownership(&dir_context, &root, &first).unwrap();
        let error = acquire_workspace_root_ownership(&dir_context, &root, &second).unwrap_err();
        release_workspace_root_ownership(&first).unwrap();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn quarantine_purge_rejects_rebound_owner_id() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_context = crate::config_io::DirectoryContext::for_testing(temp.path());
        let original_root = temp.path().join("original");
        let unrelated_root = temp.path().join("unrelated");
        std::fs::create_dir_all(&original_root).unwrap();
        std::fs::create_dir_all(&unrelated_root).unwrap();
        let owner = format!("owner-{}", uuid::Uuid::new_v4().simple());

        acquire_workspace_root_ownership(&dir_context, &original_root, &owner).unwrap();
        quarantine_workspace_artifacts(&dir_context, &original_root, Some("ws-a"), &owner).unwrap();
        release_workspace_root_ownership(&owner).unwrap();
        acquire_workspace_root_ownership(&dir_context, &unrelated_root, &owner).unwrap();
        let error = purge_workspace_artifact_quarantine(&dir_context, &owner).unwrap_err();
        release_workspace_root_ownership(&owner).unwrap();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn prepared_extraction_keeps_its_journal_until_ambiguous_moves_are_reversible() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_context = crate::config_io::DirectoryContext::for_testing(temp.path());
        let source_root = temp.path().join("source-root");
        let target_root = temp.path().join("target-root");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&target_root).unwrap();
        let source_id = format!("source-{}", uuid::Uuid::new_v4().simple());
        let target_id = format!("target-{}", uuid::Uuid::new_v4().simple());
        let source = terminal_artifacts_dir(&dir_context, &source_root, &source_id)
            .join("terminal.history.txt");
        let destination = terminal_artifacts_dir(&dir_context, &target_root, &target_id)
            .join("terminal.history.txt");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(&source, b"source-copy").unwrap();
        std::fs::write(&destination, b"moved-copy").unwrap();

        let mut source_before = Workspace::new(source_root.clone());
        source_before.stable_id = Some(source_id.clone());
        let intent = TerminalExtractionIntent::prepared(
            source_before,
            target_root.clone(),
            target_id,
            vec![TerminalArtifactRelocation {
                source: source.clone(),
                destination: destination.clone(),
                after_source_cutover: false,
            }],
        );
        let journal = persist_terminal_extraction_intent(&dir_context, &intent).unwrap();

        let error = recover_terminal_extractions(&dir_context).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(journal.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"source-copy");
        assert_eq!(std::fs::read(&destination).unwrap(), b"moved-copy");

        std::fs::remove_file(&source).unwrap();
        recover_terminal_extractions(&dir_context).unwrap();
        assert!(!journal.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"moved-copy");
        assert!(!destination.exists());
        assert!(
            Workspace::load_by_id_in(&dir_context, &source_root, &source_id)
                .unwrap()
                .is_some()
        );

        Workspace::delete(&source_root).unwrap();
    }

    #[test]
    fn prepared_recovery_reverses_a_crash_after_source_cutover() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir_context = crate::config_io::DirectoryContext::for_testing(temp.path());
        let source_root = temp.path().join("source-root");
        let target_root = temp.path().join("target-root");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&target_root).unwrap();
        let source_id = format!("source-{}", uuid::Uuid::new_v4().simple());
        let target_id = format!("target-{}", uuid::Uuid::new_v4().simple());
        let source_dir = terminal_artifacts_dir(&dir_context, &source_root, &source_id);
        let target_dir = terminal_artifacts_dir(&dir_context, &target_root, &target_id);
        let moved_source = source_dir.join("terminal.txt");
        let moved_destination = target_dir.join("terminal.txt");
        let deferred_source = source_dir.join("terminal.history.txt");
        let deferred_destination = target_dir.join("terminal.history.txt");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(&moved_destination, b"moved-before-cutover").unwrap();
        std::fs::write(&deferred_source, b"source-authoritative").unwrap();

        let mut source_before = Workspace::new(source_root.clone());
        source_before.stable_id = Some(source_id.clone());
        source_before.label = Some("before".to_string());
        let mut source_after = source_before.clone();
        source_after.label = Some("after".to_string());
        source_after.save_in(&dir_context).unwrap();
        let intent = TerminalExtractionIntent::prepared(
            source_before,
            target_root,
            target_id,
            vec![
                TerminalArtifactRelocation {
                    source: moved_source.clone(),
                    destination: moved_destination.clone(),
                    after_source_cutover: false,
                },
                TerminalArtifactRelocation {
                    source: deferred_source.clone(),
                    destination: deferred_destination.clone(),
                    after_source_cutover: true,
                },
            ],
        );
        persist_terminal_extraction_intent(&dir_context, &intent).unwrap();

        recover_terminal_extractions(&dir_context).unwrap();

        assert_eq!(
            std::fs::read(&moved_source).unwrap(),
            b"moved-before-cutover"
        );
        assert!(!moved_destination.exists());
        assert_eq!(
            std::fs::read(&deferred_source).unwrap(),
            b"source-authoritative"
        );
        assert!(!deferred_destination.exists());
        assert_eq!(
            Workspace::load_by_id_in(&dir_context, &source_root, &source_id)
                .unwrap()
                .unwrap()
                .label
                .as_deref(),
            Some("before")
        );
    }
    #[test]
    fn committed_extraction_preserves_both_locations_when_forward_state_is_ambiguous() {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        std::fs::write(&source, b"new-source").unwrap();
        std::fs::write(&destination, b"committed-destination").unwrap();
        let relocation = TerminalArtifactRelocation {
            source: source.clone(),
            destination: destination.clone(),
            after_source_cutover: false,
        };

        let error = finish_relocated_artifact(&relocation).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&source).unwrap(), b"new-source");
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"committed-destination"
        );

        std::fs::remove_file(&destination).unwrap();
        finish_relocated_artifact(&relocation).unwrap();
        assert!(!source.exists());
        assert_eq!(std::fs::read(destination).unwrap(), b"new-source");
    }

    #[test]
    fn delete_by_id_keeps_co_tenant_siblings() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("shared-root");
        std::fs::create_dir(&root).unwrap();
        let mut first = Workspace::new(root.clone());
        first.stable_id = Some("ws-delete-a".into());
        let mut second = Workspace::new(root.clone());
        second.stable_id = Some("ws-delete-b".into());
        first.save().unwrap();
        second.save().unwrap();

        Workspace::delete_by_id(&root, "ws-delete-a").unwrap();

        assert!(!workspace_path_for(&root, "ws-delete-a").unwrap().exists());
        assert!(workspace_path_for(&root, "ws-delete-b").unwrap().exists());
        Workspace::delete(&root).unwrap();
    }

    #[test]
    fn test_percent_encoding_edge_cases() {
        // Path with dashes (should pass through)
        let encoded = encode_path_for_filename(Path::new("/home/user/my-project"));
        assert_eq!(encoded, "home_user_my-project");

        // Path with spaces (percent-encoded)
        let encoded = encode_path_for_filename(Path::new("/home/user/my project"));
        assert_eq!(encoded, "home_user_my%20project");
        let decoded = decode_filename_to_path(&encoded).unwrap();
        assert_eq!(decoded, PathBuf::from("/home/user/my project"));

        // Path with underscores (percent-encoded to avoid collision with /)
        let encoded = encode_path_for_filename(Path::new("/home/user/my_project"));
        assert_eq!(encoded, "home_user_my%5Fproject");
        let decoded = decode_filename_to_path(&encoded).unwrap();
        assert_eq!(decoded, PathBuf::from("/home/user/my_project"));

        // Root path
        let encoded = encode_path_for_filename(Path::new("/"));
        assert_eq!(encoded, "root");
    }

    #[test]
    fn test_workspace_serialization() {
        let workspace = Workspace::new(PathBuf::from("/home/user/test"));
        let json = serde_json::to_string(&workspace).unwrap();
        let restored: Workspace = serde_json::from_str(&json).unwrap();

        assert_eq!(workspace.version, restored.version);
        assert_eq!(workspace.working_dir, restored.working_dir);
    }

    #[test]
    fn test_workspace_config_overrides_skip_none() {
        let overrides = WorkspaceConfigOverrides::default();
        let json = serde_json::to_string(&overrides).unwrap();

        // Empty overrides should serialize to empty object
        assert_eq!(json, "{}");
    }

    #[test]
    fn test_workspace_config_overrides_with_values() {
        let overrides = WorkspaceConfigOverrides {
            line_wrap: Some(false),
            ..Default::default()
        };
        let json = serde_json::to_string(&overrides).unwrap();

        assert!(json.contains("line_wrap"));
        assert!(!json.contains("line_numbers")); // None values skipped
    }

    #[test]
    fn test_split_layout_serialization() {
        // Create a nested split layout
        let layout = SerializedSplitNode::Split {
            direction: SerializedSplitDirection::Vertical,
            first: Box::new(SerializedSplitNode::Leaf {
                file_path: Some(PathBuf::from("src/main.rs")),
                split_id: 1,
                label: None,
                unnamed_recovery_id: None,
                role: None,
            }),
            second: Box::new(SerializedSplitNode::Leaf {
                file_path: Some(PathBuf::from("src/lib.rs")),
                split_id: 2,
                label: None,
                unnamed_recovery_id: None,
                role: None,
            }),
            ratio: 0.5,
            split_id: 0,
        };

        let json = serde_json::to_string(&layout).unwrap();
        let restored: SerializedSplitNode = serde_json::from_str(&json).unwrap();

        // Verify the restored layout matches
        match restored {
            SerializedSplitNode::Split {
                direction,
                ratio,
                split_id,
                ..
            } => {
                assert!(matches!(direction, SerializedSplitDirection::Vertical));
                assert_eq!(ratio, 0.5);
                assert_eq!(split_id, 0);
            }
            _ => panic!("Expected Split node"),
        }
    }

    #[test]
    fn test_file_state_serialization() {
        let file_state = SerializedFileState {
            cursor: SerializedCursor {
                position: 1234,
                anchor: Some(1000),
                sticky_column: 15,
            },
            additional_cursors: vec![SerializedCursor {
                position: 5000,
                anchor: None,
                sticky_column: 0,
            }],
            scroll: SerializedScroll {
                top_byte: 500,
                top_view_line_offset: 2,
                left_column: 10,
            },
            view_mode: SerializedViewMode::Source,
            compose_width: None,
            line_numbers: None,
            line_wrap: None,
            virtual_space: None,
            indentation_guide: None,
            fold_indicators: None,
            use_tabs: None,
            whitespace_indicators: None,
            tab_indicators: None,
            highlight_current_line: None,
            highlight_occurrences: None,
            plugin_state: HashMap::new(),
            folds: Vec::new(),
        };

        let json = serde_json::to_string(&file_state).unwrap();
        let restored: SerializedFileState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.cursor.position, 1234);
        assert_eq!(restored.cursor.anchor, Some(1000));
        assert_eq!(restored.cursor.sticky_column, 15);
        assert_eq!(restored.additional_cursors.len(), 1);
        assert_eq!(restored.scroll.top_byte, 500);
        assert_eq!(restored.scroll.left_column, 10);
    }

    #[test]
    fn test_bookmark_serialization() {
        let mut bookmarks = HashMap::new();
        bookmarks.insert(
            'a',
            SerializedBookmark {
                file_path: PathBuf::from("src/main.rs"),
                position: 1234,
            },
        );
        bookmarks.insert(
            'b',
            SerializedBookmark {
                file_path: PathBuf::from("src/lib.rs"),
                position: 5678,
            },
        );

        let json = serde_json::to_string(&bookmarks).unwrap();
        let restored: HashMap<char, SerializedBookmark> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 2);
        assert_eq!(restored.get(&'a').unwrap().position, 1234);
        assert_eq!(
            restored.get(&'b').unwrap().file_path,
            PathBuf::from("src/lib.rs")
        );
    }

    #[test]
    fn test_search_options_serialization() {
        let options = SearchOptions {
            case_sensitive: true,
            whole_word: true,
            use_regex: false,
            confirm_each: true,
        };

        let json = serde_json::to_string(&options).unwrap();
        let restored: SearchOptions = serde_json::from_str(&json).unwrap();

        assert!(restored.case_sensitive);
        assert!(restored.whole_word);
        assert!(!restored.use_regex);
        assert!(restored.confirm_each);
    }

    #[test]
    fn test_full_workspace_round_trip() {
        let mut workspace = Workspace::new(PathBuf::from("/home/user/myproject"));

        // Configure split layout
        workspace.split_layout = SerializedSplitNode::Split {
            direction: SerializedSplitDirection::Horizontal,
            first: Box::new(SerializedSplitNode::Leaf {
                file_path: Some(PathBuf::from("README.md")),
                split_id: 1,
                label: None,
                unnamed_recovery_id: None,
                role: None,
            }),
            second: Box::new(SerializedSplitNode::Leaf {
                file_path: Some(PathBuf::from("Cargo.toml")),
                split_id: 2,
                label: None,
                unnamed_recovery_id: None,
                role: None,
            }),
            ratio: 0.6,
            split_id: 0,
        };
        workspace.active_split_id = 1;

        // Add split state
        workspace.split_states.insert(
            1,
            SerializedSplitViewState {
                open_tabs: vec![
                    SerializedTabRef::File(PathBuf::from("README.md")),
                    SerializedTabRef::File(PathBuf::from("src/lib.rs")),
                ],
                active_tab_index: Some(0),
                open_files: vec![PathBuf::from("README.md"), PathBuf::from("src/lib.rs")],
                active_file_index: 0,
                file_states: HashMap::new(),
                tab_scroll_offset: 0,
                view_mode: SerializedViewMode::Source,
                compose_width: None,
            },
        );

        // Add bookmarks
        workspace.bookmarks.insert(
            'm',
            SerializedBookmark {
                file_path: PathBuf::from("src/main.rs"),
                position: 100,
            },
        );

        // Set search options
        workspace.search_options.case_sensitive = true;
        workspace.search_options.use_regex = true;

        // Serialize and deserialize
        let json = serde_json::to_string_pretty(&workspace).unwrap();
        let restored: Workspace = serde_json::from_str(&json).unwrap();

        // Verify everything matches
        assert_eq!(restored.version, WORKSPACE_VERSION);
        assert_eq!(restored.working_dir, PathBuf::from("/home/user/myproject"));
        assert_eq!(restored.active_split_id, 1);
        assert!(restored.bookmarks.contains_key(&'m'));
        assert!(restored.search_options.case_sensitive);
        assert!(restored.search_options.use_regex);

        // Verify split state
        let split_state = restored.split_states.get(&1).unwrap();
        assert_eq!(split_state.open_files.len(), 2);
        assert_eq!(split_state.open_files[0], PathBuf::from("README.md"));
    }

    #[test]
    fn test_workspace_file_save_load() {
        use std::fs;

        // Create a temporary directory for testing
        let temp_dir = std::env::temp_dir().join("fresh_workspace_test");
        drop(fs::remove_dir_all(&temp_dir)); // Clean up from previous runs
        fs::create_dir_all(&temp_dir).unwrap();

        let workspace_path = temp_dir.join("test_workspace.json");

        // Create a workspace
        let mut workspace = Workspace::new(temp_dir.clone());
        workspace.search_options.case_sensitive = true;
        workspace.bookmarks.insert(
            'x',
            SerializedBookmark {
                file_path: PathBuf::from("test.txt"),
                position: 42,
            },
        );

        // Save it directly to test path
        let content = serde_json::to_string_pretty(&workspace).unwrap();
        let temp_path = workspace_path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temp_path).unwrap();
        std::io::Write::write_all(&mut file, content.as_bytes()).unwrap();
        file.sync_all().unwrap();
        std::fs::rename(&temp_path, &workspace_path).unwrap();

        // Load it back
        let loaded_content = fs::read_to_string(&workspace_path).unwrap();
        let loaded: Workspace = serde_json::from_str(&loaded_content).unwrap();

        // Verify
        assert_eq!(loaded.working_dir, temp_dir);
        assert!(loaded.search_options.case_sensitive);
        assert_eq!(loaded.bookmarks.get(&'x').unwrap().position, 42);

        // Cleanup
        drop(fs::remove_dir_all(&temp_dir));
    }

    #[test]
    fn test_workspace_version_check() {
        let workspace = Workspace::new(PathBuf::from("/test"));
        assert_eq!(workspace.version, WORKSPACE_VERSION);

        // Serialize with a future version number
        let mut json_value: serde_json::Value = serde_json::to_value(&workspace).unwrap();
        json_value["version"] = serde_json::json!(999);

        let json = serde_json::to_string(&json_value).unwrap();
        let restored: Workspace = serde_json::from_str(&json).unwrap();

        // Should still deserialize, but version is 999
        assert_eq!(restored.version, 999);
    }

    #[test]
    fn test_empty_workspace_histories() {
        let histories = WorkspaceHistories::default();
        let json = serde_json::to_string(&histories).unwrap();

        // Empty histories should serialize to empty object (due to skip_serializing_if)
        assert_eq!(json, "{}");

        // But should deserialize back correctly
        let restored: WorkspaceHistories = serde_json::from_str(&json).unwrap();
        assert!(restored.search.is_empty());
        assert!(restored.replace.is_empty());
    }

    #[test]
    fn test_file_explorer_state_percent_round_trip() {
        let state = FileExplorerState {
            visible: true,
            width: crate::config::ExplorerWidth::Percent(25),
            side: crate::config::FileExplorerSide::Left,
            expanded_dirs: vec![
                PathBuf::from("src"),
                PathBuf::from("src/app"),
                PathBuf::from("tests"),
            ],
            scroll_offset: 5,
            show_hidden: true,
            show_gitignored: false,
        };

        let json = serde_json::to_string(&state).unwrap();
        let restored: FileExplorerState = serde_json::from_str(&json).unwrap();

        assert!(restored.visible);
        assert_eq!(restored.width, crate::config::ExplorerWidth::Percent(25));
        assert_eq!(restored.expanded_dirs.len(), 3);
        assert_eq!(restored.scroll_offset, 5);
        assert!(restored.show_hidden);
        assert!(!restored.show_gitignored);
    }

    #[test]
    fn test_file_explorer_state_columns_round_trip() {
        let state = FileExplorerState {
            visible: true,
            width: crate::config::ExplorerWidth::Columns(42),
            side: crate::config::FileExplorerSide::Left,
            expanded_dirs: vec![],
            scroll_offset: 0,
            show_hidden: false,
            show_gitignored: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: FileExplorerState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.width, crate::config::ExplorerWidth::Columns(42));
    }

    /// Legacy workspace files named the field `width_percent` and
    /// stored the value as a float fraction in `0.0..=1.0`. Both must
    /// still load (via serde `alias` and the `ExplorerWidth`
    /// deserializer).
    #[test]
    fn test_file_explorer_state_legacy_width_percent_alias() {
        let json = r#"{
            "visible": true,
            "width_percent": 0.3,
            "expanded_dirs": [],
            "scroll_offset": 0,
            "show_hidden": false,
            "show_gitignored": false
        }"#;
        let restored: FileExplorerState = serde_json::from_str(json).unwrap();
        assert_eq!(restored.width, crate::config::ExplorerWidth::Percent(30));
    }

    #[test]
    fn terminal_companion_marker_is_additive_and_secret_free() {
        let legacy = r#"{
            "terminal_index":0,
            "cwd":null,
            "shell":"omp",
            "cols":80,
            "rows":24,
            "log_path":"terminal.log",
            "backing_path":"terminal.txt",
            "command":["omp","launch"],
            "agent_resume":{"argv":["omp","--resume","00000000-0000-0000-0000-000000000000"]}
        }"#;
        let legacy_terminal: SerializedTerminalWorkspace =
            serde_json::from_str(legacy).expect("legacy terminal workspace must decode");
        assert!(legacy_terminal.companion.is_none());

        let mut marked = legacy_terminal;
        marked.companion = Some(fresh_core::api::TerminalCompanion::Omp);
        let json = serde_json::to_string(&marked).expect("serialize companion marker");
        assert!(json.contains("\"companion\":\"omp\""));
        assert!(!json.contains("secret"));
        assert!(!json.contains("token"));
        assert!(!json.contains("snapshot"));
    }

    #[test]
    fn concurrent_workspace_publishers_use_unique_temps_and_sync_parent() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{mpsc, Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("workspace.json");
        let barrier = Arc::new(Barrier::new(3));
        let (temp_tx, temp_rx) = mpsc::channel();
        let parent_syncs = Arc::new(AtomicUsize::new(0));

        let spawn = |content: &'static [u8]| {
            let target = target.clone();
            let barrier = Arc::clone(&barrier);
            let temp_tx = temp_tx.clone();
            let parent_syncs = Arc::clone(&parent_syncs);
            std::thread::spawn(move || {
                atomic_write_workspace_with(
                    &target,
                    content,
                    |temp| {
                        temp_tx.send(temp.to_path_buf()).unwrap();
                        barrier.wait();
                        Ok(())
                    },
                    |published| {
                        parent_syncs.fetch_add(1, Ordering::SeqCst);
                        sync_workspace_parent(published)
                    },
                )
            })
        };

        let first = spawn(br#"{"writer":"first"}"#);
        let second = spawn(br#"{"writer":"second"}"#);
        barrier.wait();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();

        let temps = [temp_rx.recv().unwrap(), temp_rx.recv().unwrap()];
        assert_eq!(temps.len(), 2);
        assert_ne!(
            temps[0], temps[1],
            "publishers must never share a temp alias"
        );
        assert!(temps.iter().all(|temp| !temp.exists()));
        assert_eq!(parent_syncs.load(Ordering::SeqCst), 2);
        let final_bytes = std::fs::read(&target).unwrap();
        assert!(
            final_bytes == br#"{"writer":"first"}"# || final_bytes == br#"{"writer":"second"}"#,
            "the final must be one complete publication, got {final_bytes:?}"
        );
    }

    #[test]
    fn workspace_publication_fault_keeps_previous_final_and_cleans_own_temp() {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("workspace.json");
        std::fs::write(&target, b"previous").unwrap();
        let (attempted_temp_tx, attempted_temp_rx) = mpsc::channel();

        let error = atomic_write_workspace_with(
            &target,
            b"replacement",
            move |temp| {
                attempted_temp_tx.send(temp.to_path_buf()).unwrap();
                Err(io::Error::other("injected failure before publication"))
            },
            |_| panic!("parent sync must not run before a successful rename"),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(std::fs::read(&target).unwrap(), b"previous");
        let temp = attempted_temp_rx.recv().unwrap();
        assert!(
            !temp.exists(),
            "a failed publisher cleans only its own temp"
        );
    }
}
