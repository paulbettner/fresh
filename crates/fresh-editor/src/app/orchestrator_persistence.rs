//! Cross-restart persistence for Orchestrator sessions and
//! plugin global state.
//!
//! ## The session registry is the directory set
//!
//! There is no central session-list file. A session *is* a
//! directory (one session per dir), and the registry is the
//! per-dir workspace cache:
//!
//!   - `<data_dir>/workspaces/<encoded-root>.json` — one file per
//!     directory ever opened. Each carries that window's identity
//!     (`label`, `session_plugin_state`) plus its buffer/split
//!     layout. [`discover_sessions`] scans this directory at boot,
//!     garbage-collects dead local entries, migrates legacy filenames to
//!     exact stable-id paths, and returns one window per durable identity.
//!
//!   - `<data_dir>/orchestrator/state/<plugin>.json` — editor-wide
//!     plugin global state, one file per plugin (not per-project).
//!
//! `PersistedWindow` / `PersistedWindows` are now in-memory shapes
//! produced by discovery (and still the parse target of a legacy
//! `windows.json` during migration), not an on-disk schema.
//!
//! ## Migration
//!
//! Older builds kept a central `<data_dir>/orchestrator/windows.json`
//! (and, before that, per-cwd `<data>/orchestrator/<encoded_cwd>/
//! windows.json`). On first read, [`migrate_legacy_windows`] folds any
//! per-cwd files into a single windows.json, then
//! [`migrate_windows_json_into_workspaces`] backfills its
//! `label` / per-session plugin state into the matching per-dir
//! workspace files and retires the file to `windows.json.retired.bak`.
//! After that the workspace cache is the sole registry.
//!
//! State lives under the platform data dir (`$XDG_DATA_HOME/fresh/`),
//! never the working tree (issue #1991).
//!
//! ## Startup
//!
//! [`read_persisted_windows_env`] + [`read_persisted_plugin_state`]
//! run from `editor_init` before the editor struct exists. The
//! foreground window is the one whose `root` matches the launch cwd
//! ([`pick_active_window_for_cwd`]) — authoritatively, regardless of
//! which session was last used; if none matches, a clean window is
//! booted at the cwd. Every other discovered session comes back as an
//! inert shell (no splits/LSP) restored lazily on first dive/preview.
//! The "warm" layout is intentionally not persisted across restarts —
//! re-warming on first dive is fast enough.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::Editor;

/// One session as it appears on disk.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct PersistedWindow {
    pub(crate) id: u64,
    pub(crate) label: String,
    pub(crate) root: PathBuf,
    /// Project this session belongs to — the canonical repo
    /// root (or arbitrary directory for non-git sessions) the
    /// user pointed the new-session form at. `None` for legacy
    /// v1-migrated entries where the project_path wasn't
    /// recorded; the migration synthesises it from the
    /// per-cwd directory name. The Open dialog filters by this
    /// field so sessions for the current project surface first
    /// without an explicit toggle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_path: Option<PathBuf>,
    /// `true` when the session shares its working tree with
    /// other sessions (or runs in-place inside a non-git
    /// directory); `false` when it has its own dedicated
    /// `git worktree add`. Defaults to `false` for v1-migrated
    /// entries (the v1 flow always created a fresh worktree).
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) shared_worktree: bool,
    /// Per-session plugin state (the same map kept in
    /// `Session.plugin_state`). Empty plugins / empty keys are
    /// stripped on save.
    #[serde(default)]
    pub(crate) plugin_state: HashMap<String, HashMap<String, serde_json::Value>>,
    /// How to rebuild/reconnect this session's backend on restore (read
    /// from the workspace file's `authority_spec`). `Local` for an ordinary
    /// host session. Threaded into the window at construction so an
    /// unmaterialized background session still knows its backend (and a
    /// later save doesn't clobber it back to local).
    #[serde(default, skip_serializing_if = "is_local_authority_spec")]
    pub(crate) authority_spec: crate::services::authority::SessionAuthoritySpec,
    /// Durable workspace identity carried in the workspace file
    /// (`Workspace::stable_id`). Discovery publishes legacy files under an
    /// exact stable-id path before returning them, so restored windows always
    /// carry `Some` even though the optional shape remains for old envelopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stable_id: Option<String>,
}

fn is_local_authority_spec(spec: &crate::services::authority::SessionAuthoritySpec) -> bool {
    matches!(
        spec,
        crate::services::authority::SessionAuthoritySpec::Local
    )
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Top-level shape of `windows.json`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct PersistedWindows {
    /// Schema version. `1` (or missing) = legacy per-cwd file
    /// without `project_path` / `shared_worktree`. `2` = global
    /// store with both fields populated. The loader handles
    /// either shape; the writer always emits `2`.
    #[serde(default = "default_version")]
    pub(crate) version: u32,
    /// Last active session id at quit time. The loader makes
    /// this session the active one again. If missing or
    /// dangling, falls back to the base session.
    pub(crate) active: u64,
    /// `next_window_id` at quit time — preserved so newly
    /// created sessions after restart don't collide with ids
    /// the user might still see in plugin state.
    pub(crate) next_id: u64,
    pub(crate) windows: Vec<PersistedWindow>,
}

fn default_version() -> u32 {
    1
}

const CURRENT_VERSION: u32 = 2;

/// Read the global `windows.json` and return the parsed
/// envelope. Returns `None` when the file doesn't exist or
/// fails to parse — those are not error cases at the editor
/// level (a missing or corrupted file just means "no persisted
/// state").
///
/// Migrates v1 (per-cwd) files into the global store on first
/// load and renames each to `.migrated.bak`. The `working_dir`
/// argument is no longer used for the file location (it's
/// global now); it's kept in the signature so the factory can
/// later pass it to the orchestrator plugin as the
/// "default project filter" hint without a second IO pass.
///
/// Pure file IO + JSON parse. Used by the editor factory to
/// decide how to build the initial windows map before any
/// `Editor` instance exists.
pub(crate) fn read_persisted_windows_env(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
    _working_dir: &Path,
) -> Option<PersistedWindows> {
    // Legacy v1 (per-cwd) → windows.json, if any survive. windows.json
    // is itself legacy now; the next step folds it into the per-dir
    // workspace files and retires it.
    let global_p = global_windows_path(data_dir);
    if !filesystem.exists(&global_p) {
        migrate_legacy_windows(filesystem, data_dir);
    }
    migrate_windows_json_into_workspaces(filesystem, data_dir);
    // Daemon-scoped workspaces predate the one-set model; fold them in before
    // discovery so they show up in the dock like any other workspace.
    migrate_session_workspaces_into_store(filesystem, data_dir);

    // The workspace cache is the session registry now: sessions discovered
    // from disk, keyed by durable identity. A directory may host several
    // co-tenant workspaces (a tab extracted into its own window over the same
    // project), so discovery yields one window per `stable_id`, not one per
    // directory. GC dead entries and build a window per survivor.
    let windows = discover_sessions(filesystem, data_dir);
    if windows.is_empty() {
        return None;
    }
    let next_id = windows.iter().map(|w| w.id).max().unwrap_or(0) + 1;
    // `active` is decided downstream by the launch cwd
    // (`pick_active_window_for_cwd`); 0 means "no stored hint", so the
    // cwd-match branch governs which session is foregrounded (the first
    // co-tenant at the cwd root when several share it).
    Some(PersistedWindows {
        version: CURRENT_VERSION,
        active: 0,
        next_id,
        windows,
    })
}

fn workspaces_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("workspaces")
}

/// Per-dir workspace file path for `root` under `data_dir` — mirrors
/// `crate::workspace::get_workspace_path` but honours the passed data
/// dir rather than the process-global one.
fn workspace_file_for(data_dir: &Path, root: &Path) -> PathBuf {
    let filename = format!(
        "{}.json",
        crate::workspace::encode_path_for_filename(&canonical_key(root))
    );
    workspaces_dir(data_dir).join(filename)
}

fn workspace_file_for_id(data_dir: &Path, root: &Path, stable_id: &str) -> PathBuf {
    let safe_id: String = stable_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    workspaces_dir(data_dir).join(format!(
        "{}.{}.json",
        crate::workspace::encode_path_for_filename(&canonical_key(root)),
        safe_id
    ))
}

fn authority_identity_key(spec: &crate::services::authority::SessionAuthoritySpec) -> String {
    use crate::services::authority::SessionAuthoritySpec;

    match spec {
        SessionAuthoritySpec::Local => "local".to_string(),
        SessionAuthoritySpec::RemoteAgent(agent) => match agent.verified_identity() {
            Some(identity) => format!(
                "remote-tenant:{}:{}",
                identity.anchor.digest,
                identity.canonical_root.to_string_lossy()
            ),
            None => format!(
                "remote-transport:{}",
                serde_json::to_string(&agent.transport)
                    .expect("remote transport persistence is serializable")
            ),
        },
        SessionAuthoritySpec::Plugin(payload) => format!(
            "plugin:{}",
            serde_json::to_string(payload).expect("plugin authority persistence is serializable")
        ),
    }
}

fn deterministic_legacy_stable_id(
    source: &Path,
    root: &Path,
    authority_spec: &crate::services::authority::SessionAuthoritySpec,
) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(b"fresh-legacy-workspace-v1\0");
    hasher.update(source.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(canonical_key(root).to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(authority_identity_key(authority_spec).as_bytes());
    let digest = hasher.finalize();
    let mut id = String::with_capacity(10 + digest.len() * 2);
    id.push_str("ws-legacy-");
    for byte in digest {
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    id
}
fn deterministic_conflict_stable_id(
    stable_id: &str,
    root: &Path,
    authority_key: &str,
    nonce: u32,
) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(b"fresh-conflicting-workspace-id-v1\0");
    hasher.update(stable_id.as_bytes());
    hasher.update([0]);
    hasher.update(canonical_key(root).to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(authority_key.as_bytes());
    hasher.update(nonce.to_le_bytes());
    let digest = hasher.finalize();
    let mut id = String::with_capacity(12 + digest.len() * 2);
    id.push_str("ws-conflict-");
    for byte in digest {
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    id
}

fn persisted_workspace_matches(
    value: &serde_json::Value,
    stable_id: &str,
    root: &Path,
    authority_spec: &crate::services::authority::SessionAuthoritySpec,
) -> bool {
    if value.get("stable_id").and_then(|id| id.as_str()) != Some(stable_id) {
        return false;
    }
    let Some(saved_root) = value
        .get("working_dir")
        .and_then(|saved| saved.as_str())
        .map(PathBuf::from)
    else {
        return false;
    };
    let saved_authority = value
        .get("authority_spec")
        .and_then(|saved| serde_json::from_value(saved.clone()).ok())
        .unwrap_or_default();
    canonical_key(&saved_root) == canonical_key(root)
        && authority_identity_key(&saved_authority) == authority_identity_key(authority_spec)
}

fn adopt_workspace_file(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
    source: &Path,
    value: &mut serde_json::Value,
    root: &Path,
    authority_spec: &crate::services::authority::SessionAuthoritySpec,
    stable_id: String,
) -> Option<String> {
    let destination = workspace_file_for_id(data_dir, root, &stable_id);
    if source == destination {
        return persisted_workspace_matches(value, &stable_id, root, authority_spec)
            .then_some(stable_id);
    }
    let source_saved_at = value.get("saved_at").and_then(|v| v.as_u64()).unwrap_or(0);

    let destination_value = filesystem
        .read_file(&destination)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    if filesystem.exists(&destination) {
        let Some(existing) = destination_value.as_ref() else {
            return None;
        };
        if !persisted_workspace_matches(existing, &stable_id, root, authority_spec) {
            return None;
        }
        let destination_saved_at = existing
            .get("saved_at")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if destination_saved_at >= source_saved_at {
            let _ = filesystem.remove_file(source).ok();
            return Some(stable_id);
        }
    }

    value.as_object_mut()?.insert(
        "stable_id".into(),
        serde_json::Value::String(stable_id.clone()),
    );
    let output = serde_json::to_vec_pretty(value).ok()?;
    filesystem.write_file(&destination, &output).ok()?;
    let published = filesystem
        .read_file(&destination)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())?;
    if !persisted_workspace_matches(&published, &stable_id, root, authority_spec) {
        return None;
    }
    let _ = filesystem.remove_file(source).ok();
    Some(stable_id)
}

fn basename_label(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Scan the workspace cache, garbage-collect definitively dead local roots,
/// and return one session per durable workspace identity. Legacy files are
/// first published under an exact stable-id path so restore never has to guess
/// among co-tenant local and remote sessions that share a textual root.
fn discover_sessions(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
) -> Vec<PersistedWindow> {
    type SessionState = HashMap<String, HashMap<String, serde_json::Value>>;
    let dir = workspaces_dir(data_dir);
    tracing::debug!(dir = %dir.display(), "discover_sessions: read_dir");
    let mut entries = match filesystem.read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    tracing::debug!(
        count = entries.len(),
        "discover_sessions: read_dir returned"
    );
    struct PendingCandidate {
        source: PathBuf,
        value: serde_json::Value,
        root: PathBuf,
        root_key: PathBuf,
        label: String,
        plugin_state: SessionState,
        authority_spec: crate::services::authority::SessionAuthoritySpec,
        authority_key: String,
        desired_stable_id: String,
        exact_identity_path: bool,
        saved_at: u64,
    }
    struct Candidate {
        root: PathBuf,
        root_key: PathBuf,
        label: String,
        plugin_state: SessionState,
        authority_spec: crate::services::authority::SessionAuthoritySpec,
        authority_key: String,
        stable_id: String,
        saved_at: u64,
    }
    let mut pending: Vec<PendingCandidate> = Vec::new();
    for entry in entries {
        let p = &entry.path;
        // Only real workspace files. A torn `*.json.tmp` write or a
        // `*.retired.bak` already fails the `.json` suffix test.
        if !entry.name.ends_with(".json") {
            continue;
        }
        tracing::debug!(path = %p.display(), "discover_sessions: read_file");
        let Ok(bytes) = filesystem.read_file(p) else {
            continue;
        };
        let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let Some(root) = val.get("working_dir").and_then(|v| v.as_str()) else {
            continue;
        };
        let root = PathBuf::from(root);
        // The session's backend spec (how to reconnect on restore). Absent /
        // unparseable → `Local`, so a malformed entry degrades safely. Read
        // *before* the GC check: a remote session's `root` lives on the
        // remote host, so it can't be validated against the local filesystem.
        let authority_spec: crate::services::authority::SessionAuthoritySpec = val
            .get("authority_spec")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        // GC only local sessions, and only on a *definitive* answer that the
        // root is unusable: `NotFound` (the directory is gone) or `Ok(false)`
        // (the path was replaced by a non-dir). Drop the stale cache file then
        // — best-effort, a failed delete just leaves a harmless file to retry
        // next boot. Any *other* `Err` (permission, IO, an unreachable
        // remote/unmounted FS) is ambiguous but recoverable, so keep the file
        // rather than irreversibly losing the session.
        //
        // Remote sessions (SSH / kube) are *never* GC'd against the local
        // filesystem: their `root` is a path on the remote host that the local
        // `filesystem` here can't see, so `is_dir` would answer `Ok(false)`
        // and silently delete every remote session's workspace file on the
        // next boot — the session would vanish from the Orchestrator dock
        // after a restart. Whether the remote dir still exists is only knowable
        // after reconnecting, so we keep the entry and let restore decide.
        if !authority_spec.is_remote() {
            match filesystem.is_dir(&root) {
                Ok(true) => {}
                Ok(false) => {
                    let _ = filesystem.remove_file(p).ok();
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let _ = filesystem.remove_file(p).ok();
                    continue;
                }
                Err(_) => continue,
            }
        }
        let desired_stable_id = val
            .get("stable_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| deterministic_legacy_stable_id(p, &root, &authority_spec));
        let authority_key = authority_identity_key(&authority_spec);
        let label = val
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| basename_label(&root));
        let plugin_state: SessionState = val
            .get("session_plugin_state")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let exact_identity_path = p == &workspace_file_for_id(data_dir, &root, &desired_stable_id);
        pending.push(PendingCandidate {
            source: p.clone(),
            root_key: canonical_key(&root),
            root,
            label,
            plugin_state,
            authority_spec,
            authority_key,
            desired_stable_id,
            exact_identity_path,
            saved_at: val.get("saved_at").and_then(|v| v.as_u64()).unwrap_or(0),
            value: val,
        });
    }
    // A workspace id is the public durable identity consumed throughout the
    // editor, so it must remain globally unique even when two authorities
    // arrive with the same persisted id. Sort first so an already-canonical
    // path keeps its id; otherwise authority/root/path order deterministically
    // chooses the keeper. Every other authority claim receives a deterministic
    // scalar replacement and is republished under that identity.
    pending.sort_by(|a, b| {
        a.desired_stable_id
            .cmp(&b.desired_stable_id)
            .then_with(|| b.exact_identity_path.cmp(&a.exact_identity_path))
            .then_with(|| a.authority_key.cmp(&b.authority_key))
            .then_with(|| a.root_key.cmp(&b.root_key))
            .then_with(|| a.source.cmp(&b.source))
    });
    let mut reserved_ids: HashSet<String> = pending
        .iter()
        .map(|candidate| candidate.desired_stable_id.clone())
        .collect();
    let mut original_claimed: HashSet<String> = HashSet::new();
    let mut assigned_claims: std::collections::BTreeMap<(String, String), String> =
        std::collections::BTreeMap::new();
    let mut found: Vec<Candidate> = Vec::new();
    for mut candidate in pending {
        let claim = (
            candidate.desired_stable_id.clone(),
            candidate.authority_key.clone(),
        );
        let assigned_stable_id = if let Some(existing) = assigned_claims.get(&claim) {
            existing.clone()
        } else if original_claimed.insert(candidate.desired_stable_id.clone()) {
            assigned_claims.insert(claim, candidate.desired_stable_id.clone());
            candidate.desired_stable_id.clone()
        } else {
            let mut nonce = 0;
            let minted = loop {
                let id = deterministic_conflict_stable_id(
                    &candidate.desired_stable_id,
                    &candidate.root,
                    &candidate.authority_key,
                    nonce,
                );
                if reserved_ids.insert(id.clone()) {
                    break id;
                }
                nonce = nonce
                    .checked_add(1)
                    .expect("workspace identity collision nonce exhausted");
            };
            tracing::warn!(
                stable_id = %candidate.desired_stable_id,
                replacement = %minted,
                authority = %candidate.authority_key,
                "discover_sessions: replacing cross-authority duplicate workspace identity"
            );
            assigned_claims.insert(claim, minted.clone());
            minted
        };
        let Some(stable_id) = adopt_workspace_file(
            filesystem,
            data_dir,
            &candidate.source,
            &mut candidate.value,
            &candidate.root,
            &candidate.authority_spec,
            assigned_stable_id,
        ) else {
            tracing::warn!(
                path = %candidate.source.display(),
                "discover_sessions: refusing workspace whose exact stable identity could not be published"
            );
            continue;
        };
        found.push(Candidate {
            root: candidate.root,
            root_key: candidate.root_key,
            label: candidate.label,
            plugin_state: candidate.plugin_state,
            authority_spec: candidate.authority_spec,
            authority_key: candidate.authority_key,
            stable_id,
            saved_at: candidate.saved_at,
        });
    }

    // Resolve duplicate exact identities to the freshest snapshot. The public
    // scalar id is now globally unique across authorities, so no composite key
    // leaks into the rest of the editor.
    let mut by_identity: std::collections::BTreeMap<String, Candidate> =
        std::collections::BTreeMap::new();
    for candidate in found {
        let key = candidate.stable_id.clone();
        match by_identity.get(&key) {
            Some(current) if current.saved_at >= candidate.saved_at => {
                tracing::info!(
                    root = %candidate.root.display(),
                    "discover_sessions: skipping stale same-identity duplicate"
                );
            }
            _ => {
                by_identity.insert(key, candidate);
            }
        }
    }
    let mut sessions: Vec<Candidate> = by_identity.into_values().collect();
    sessions.sort_by(|a, b| {
        a.root_key
            .cmp(&b.root_key)
            .then_with(|| a.authority_key.cmp(&b.authority_key))
            .then_with(|| a.stable_id.cmp(&b.stable_id))
    });
    sessions
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let (project_path, shared_worktree) = read_orch_session_meta(&c.plugin_state);
            PersistedWindow {
                id: (i as u64) + 1,
                label: c.label,
                root: c.root,
                project_path,
                shared_worktree,
                authority_spec: c.authority_spec,
                plugin_state: c.plugin_state,
                stable_id: Some(c.stable_id),
            }
        })
        .collect()
}

/// Fold legacy `windows.json` session metadata (label + per-session
/// plugin state) into the per-dir workspace files, then retire the
/// file. After this the workspace cache is the sole registry. Only
/// existing workspace files are backfilled; entries with no workspace
/// file are dropped (they carried no buffer content to restore). No-op
/// once `windows.json` is gone.
fn migrate_windows_json_into_workspaces(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
) {
    let global_p = global_windows_path(data_dir);
    if !filesystem.exists(&global_p) {
        return;
    }
    let Ok(bytes) = filesystem.read_file(&global_p) else {
        return;
    };
    let Ok(env) = serde_json::from_slice::<PersistedWindows>(&bytes) else {
        return; // leave an unparseable file in place rather than lose it
    };
    for w in &env.windows {
        let ws_path = workspace_file_for(data_dir, &w.root);
        if !filesystem.exists(&ws_path) {
            continue;
        }
        let Ok(wbytes) = filesystem.read_file(&ws_path) else {
            continue;
        };
        let Ok(mut val) = serde_json::from_slice::<serde_json::Value>(&wbytes) else {
            continue;
        };
        if let Some(obj) = val.as_object_mut() {
            obj.entry("label")
                .or_insert_with(|| serde_json::Value::String(w.label.clone()));
            if !obj.contains_key("session_plugin_state") && !w.plugin_state.is_empty() {
                if let Ok(ps) = serde_json::to_value(&w.plugin_state) {
                    obj.insert("session_plugin_state".into(), ps);
                }
            }
        }
        if let Ok(out) = serde_json::to_vec_pretty(&val) {
            // Best-effort backfill: on failure the workspace keeps its pre-migration content.
            let _ = filesystem.write_file(&ws_path, &out).ok();
        }
    }
    // Retire windows.json (keep a .bak so a downgrade isn't one-way).
    let bak = global_p.with_extension("json.retired.bak");
    if filesystem.rename(&global_p, &bak).is_err() {
        // Best-effort: if delete also fails the file stays and migration reruns (idempotent).
        let _ = filesystem.remove_file(&global_p).ok();
    }
}

/// Fold pre-migration daemon-scoped workspaces into the one workspace store.
///
/// A named daemon used to persist *all* of its windows into a single
/// `session-workspaces/<daemon>.json`, keyed on the daemon's name. That store
/// could not represent more than one window (each overwrote the last) and
/// nothing here ever scanned it, so those layouts were invisible to the dock
/// and unreachable from any other invocation. Workspaces are now one set,
/// shared by direct mode and every daemon, so each surviving legacy snapshot is
/// re-keyed as an ordinary workspace: its recorded `working_dir` plus a minted
/// `stable_id` if it predates durable identities.
///
/// Two named daemons that each held a layout over the same root migrate into
/// two co-tenant workspaces over that root — which the per-window store already
/// supports — so neither user's layout is dropped.
///
/// Idempotent and best-effort: the legacy directory is renamed aside once every
/// file in it has been converted, and anything that fails to convert is left
/// alone for the next boot to retry rather than deleted.
fn migrate_session_workspaces_into_store(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
) {
    let legacy_dir = data_dir.join("session-workspaces");
    let Ok(entries) = filesystem.read_dir(&legacy_dir) else {
        return;
    };
    let mut all_converted = true;
    for entry in entries {
        if !entry.name.ends_with(".json") {
            continue;
        }
        let Ok(bytes) = filesystem.read_file(&entry.path) else {
            all_converted = false;
            continue;
        };
        let Ok(mut val) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            // Unparseable: leave it rather than silently lose a layout.
            all_converted = false;
            continue;
        };
        let Some(root) = val
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
        else {
            all_converted = false;
            continue;
        };
        // Mint an identity for snapshots written before durable ids, so the
        // file lands under the id-keyed name the per-window store uses.
        let stable_id = match val.get("stable_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                let id = crate::workspace::generate_stable_id();
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("stable_id".into(), serde_json::Value::String(id.clone()));
                }
                id
            }
        };
        let dest = workspaces_dir(data_dir).join(format!(
            "{}.{}.json",
            crate::workspace::encode_path_for_filename(&canonical_key(&root)),
            stable_id
        ));
        // Never clobber a live workspace: the per-window store is authoritative
        // wherever both describe the same identity.
        if filesystem.exists(&dest) {
            continue;
        }
        let Ok(out) = serde_json::to_vec_pretty(&val) else {
            all_converted = false;
            continue;
        };
        if filesystem.write_file(&dest, &out).is_err() {
            all_converted = false;
            continue;
        }
        tracing::info!(
            "Migrated daemon-scoped workspace {:?} into the shared store as {:?}",
            entry.path,
            dest
        );
    }
    if !all_converted {
        return;
    }
    // Retire the legacy directory (keep it as a .bak so a downgrade isn't
    // one-way). A failure just leaves it for the next boot — the conversion
    // above skips anything already present, so rerunning is harmless.
    let bak = legacy_dir.with_extension("retired.bak");
    if filesystem.exists(&bak) {
        return;
    }
    let _ = filesystem.rename(&legacy_dir, &bak).ok();
}

/// Pick which persisted session to bring up at boot, scoped to the
/// editor's launch cwd.
///
/// The rule the user expects: re-opening the editor in a project
/// should reopen the session they last used **in that project** —
/// but never a session from a *different* project (that cross-project
/// bleed is what made one day's work leak into the next). So we only
/// ever consider windows that belong to `cwd`:
///
///   1. If `env.active` (the globally last-used session at quit)
///      belongs to `cwd`, that's the last-used session for this
///      project — bring it up.
///   2. Else pick the most-recently-*created* window belonging to
///      `cwd` (highest id — orchestrator ids are monotonic). This is
///      the fallback for "your last-used session was in another
///      project, but this one has sessions of its own."
///   3. Else `None` — the caller boots a clean base window at `cwd`.
///
/// A window "belongs to" `cwd` when its **`root`** — the directory the
/// window actually opens in — equals `cwd` after canonicalization. We
/// match on `root`, NOT `project_path`: an orchestrator worktree session
/// carries `project_path == <parent project>` but `root == <worktree>`,
/// so matching on `project_path` would resurrect a worktree-rooted window
/// when the user launched in the project dir (issue #2056). `project_path`
/// stays purely as orchestrator-dialog grouping metadata. The previous
/// base (id 1) is eligible too — if it was the user's last-used window in
/// this cwd, reopening it is just a clean editor at the cwd.
pub(crate) fn pick_active_window_for_cwd<'a>(
    env: Option<&'a PersistedWindows>,
    cwd: &Path,
) -> Option<&'a PersistedWindow> {
    let env = env?;
    if let Some(w) = env
        .windows
        .iter()
        .find(|w| w.id == env.active && window_matches_cwd(w, cwd))
    {
        return Some(w);
    }
    env.windows
        .iter()
        .filter(|w| window_matches_cwd(w, cwd))
        .max_by_key(|w| w.id)
}

fn window_matches_cwd(w: &PersistedWindow, cwd: &Path) -> bool {
    paths_equal(&w.root, cwd)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    canonical_key(a) == canonical_key(b)
}

/// Canonicalized identity for a session root. Sessions are
/// identified by directory (one session per dir), so every root
/// comparison and dedup goes through this: it resolves symlinks
/// and normalizes trailing slashes so `/repos/inty` and
/// `/repos/inty/` (and a symlinked tmpdir vs its real path) map to
/// the same session.
pub(crate) fn canonical_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Scan `<data>/orchestrator/*/windows.json` for legacy v1
/// per-cwd files. Fold every session into one v2 envelope, with
/// `project_path` derived by reverse-decoding the slug
/// directory name back into the original cwd path. Write the
/// global file, then rename each legacy file to
/// `windows.json.migrated.bak` so a downgrade isn't a one-way
/// trip.
///
/// Conflicts: two cwd-keyed files with the same session id
/// collide rarely (sessions are interactively created and ids
/// monotonic per-store), but if they do the file with the more
/// recent mtime wins; the loser's id is re-numbered to
/// `next_id` of the winning envelope.
fn migrate_legacy_windows(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
) {
    let orch_root = data_dir.join("orchestrator");
    if !filesystem.exists(&orch_root) {
        return;
    }
    let entries = match filesystem.read_dir(&orch_root) {
        Ok(es) => es,
        Err(_) => return,
    };
    let mut merged_windows: Vec<PersistedWindow> = Vec::new();
    let mut merged_active: u64 = 1;
    let mut merged_next_id: u64 = 2;
    let mut used_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut legacy_to_rename: Vec<PathBuf> = Vec::new();

    for entry in entries {
        let dir = entry.path;
        if !filesystem.is_dir(&dir).unwrap_or(false) {
            continue;
        }
        // Only look at directories that look like slug-encoded
        // paths (i.e. not the `state/` plugin dir, which lives
        // alongside but isn't a per-cwd bucket).
        let dir_name = match dir.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if dir_name == "state" {
            continue;
        }
        let legacy_p = dir.join("windows.json");
        if !filesystem.exists(&legacy_p) {
            continue;
        }
        let bytes = match filesystem.read_file(&legacy_p) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let env = match serde_json::from_slice::<PersistedWindows>(&bytes) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let project_path = crate::workspace::decode_filename_to_path(&dir_name)
            .unwrap_or_else(|| PathBuf::from(dir_name.clone()));

        let mut local_renum: HashMap<u64, u64> = HashMap::new();
        for mut w in env.windows.into_iter() {
            // Default project_path to the decoded cwd unless
            // the entry already carries one (a partial migration
            // re-running on the same data).
            if w.project_path.is_none() {
                w.project_path = Some(project_path.clone());
            }
            if used_ids.contains(&w.id) {
                let new_id = merged_next_id;
                local_renum.insert(w.id, new_id);
                merged_next_id = merged_next_id.saturating_add(1);
                used_ids.insert(new_id);
                w.id = new_id;
            } else {
                used_ids.insert(w.id);
                merged_next_id = merged_next_id.max(w.id.saturating_add(1));
            }
            merged_windows.push(w);
        }
        // Most-recently-modified per-cwd file decides which
        // session id becomes "active" in the merged store.
        // Stat the file; if we can't, the last file scanned
        // wins by virtue of being last.
        let active_id = local_renum.get(&env.active).copied().unwrap_or(env.active);
        merged_active = active_id;
        legacy_to_rename.push(legacy_p);
    }

    if merged_windows.is_empty() {
        return;
    }
    merged_windows.sort_by_key(|w| w.id);
    let envelope = PersistedWindows {
        version: CURRENT_VERSION,
        active: merged_active,
        next_id: merged_next_id,
        windows: merged_windows,
    };
    let global_p = global_windows_path(data_dir);
    if let Err(e) = filesystem.create_dir_all(&orch_root) {
        tracing::warn!("orchestrator migration: failed to create {orch_root:?}: {e}");
        return;
    }
    let bytes = match serde_json::to_vec_pretty(&envelope) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("orchestrator migration: failed to serialise envelope: {e}");
            return;
        }
    };
    if let Err(e) = filesystem.write_file(&global_p, &bytes) {
        tracing::warn!("orchestrator migration: failed to write {global_p:?}: {e}");
        return;
    }
    for legacy_p in legacy_to_rename {
        let backup = legacy_p.with_extension("json.migrated.bak");
        if let Err(e) = filesystem.rename(&legacy_p, &backup) {
            tracing::warn!(
                "orchestrator migration: failed to rename {legacy_p:?} → {backup:?}: {e}"
            );
        }
    }
    tracing::info!(
        "orchestrator persistence: migrated {} sessions from legacy per-cwd layout into {:?}",
        envelope.windows.len(),
        global_p
    );
}

/// Read every `state/<plugin>.json` into a flat
/// `plugin → key → value` map. Skips files with unsafe names,
/// non-JSON extensions, parse errors, and empty maps. Same
/// motivations as [`read_persisted_windows_env`] — used by the
/// editor factory pre-construction.
///
/// Reads from the global `<data>/orchestrator/state/` directory.
/// The legacy per-cwd plugin state files (under
/// `<data>/orchestrator/<encoded_cwd>/state/`) are folded into
/// the global directory the first time we encounter no global
/// state and at least one legacy file — see
/// `migrate_legacy_plugin_state`.
pub(crate) fn read_persisted_plugin_state(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
    _working_dir: &Path,
) -> HashMap<String, HashMap<String, serde_json::Value>> {
    let mut out: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();
    let state_dir = global_state_dir(data_dir);
    if !filesystem.exists(&state_dir) {
        migrate_legacy_plugin_state(filesystem, data_dir);
    }
    if !filesystem.exists(&state_dir) {
        return out;
    }
    let entries = match filesystem.read_dir(&state_dir) {
        Ok(es) => es,
        Err(e) => {
            tracing::warn!("orchestrator persistence: failed to read {state_dir:?}: {e}");
            return out;
        }
    };
    for entry in entries {
        let path = entry.path;
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !plugin_name_is_safe(stem) {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match filesystem.read_file(&path) {
            Ok(bytes) => {
                match serde_json::from_slice::<HashMap<String, serde_json::Value>>(&bytes) {
                    Ok(map) if !map.is_empty() => {
                        out.insert(stem.to_owned(), map);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("orchestrator persistence: failed to parse {path:?}: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("orchestrator persistence: failed to read {path:?}: {e}");
            }
        }
    }
    out
}

/// Global orchestrator state location under the platform data
/// dir. v2 stores everything in one tree regardless of the
/// editor's cwd; see issue #1991 for why this is no longer
/// rooted at `<working_dir>/.fresh`.
fn orchestrator_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("orchestrator")
}

fn global_windows_path(data_dir: &Path) -> PathBuf {
    orchestrator_dir(data_dir).join("windows.json")
}

fn global_state_dir(data_dir: &Path) -> PathBuf {
    orchestrator_dir(data_dir).join("state")
}

fn global_plugin_state_path(data_dir: &Path, plugin: &str) -> PathBuf {
    // Plugin names are short identifiers (`orchestrator`,
    // `live_grep`, …) so no escaping is needed for typical
    // input. Reject anything that would escape the state dir to
    // avoid `../`-style traversal in case a plugin picks a
    // pathological name.
    global_state_dir(data_dir).join(format!("{plugin}.json"))
}

fn plugin_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && !name.starts_with('.')
}

/// Fold legacy per-cwd plugin state into the global
/// `<data>/orchestrator/state/` directory. Per-plugin files
/// with the same name are merged key-by-key; the most recently
/// modified cwd's file wins on conflict. Legacy files are
/// renamed to `<plugin>.json.migrated.bak`. Best-effort: any
/// filesystem error logs WARN and continues.
fn migrate_legacy_plugin_state(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
) {
    let orch_root = data_dir.join("orchestrator");
    if !filesystem.exists(&orch_root) {
        return;
    }
    let cwd_entries = match filesystem.read_dir(&orch_root) {
        Ok(es) => es,
        Err(_) => return,
    };
    let mut merged: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();
    let mut legacy_to_rename: Vec<PathBuf> = Vec::new();
    for cwd_entry in cwd_entries {
        let dir = cwd_entry.path;
        if !filesystem.is_dir(&dir).unwrap_or(false) {
            continue;
        }
        let dir_name = match dir.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if dir_name == "state" {
            continue;
        }
        let state_dir = dir.join("state");
        if !filesystem.exists(&state_dir) {
            continue;
        }
        let plugin_entries = match filesystem.read_dir(&state_dir) {
            Ok(es) => es,
            Err(_) => continue,
        };
        for pe in plugin_entries {
            let p = pe.path;
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !plugin_name_is_safe(stem) {
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match filesystem.read_file(&p) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let map: HashMap<String, serde_json::Value> = match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let slot = merged.entry(stem.to_owned()).or_default();
            for (k, v) in map {
                slot.insert(k, v);
            }
            legacy_to_rename.push(p);
        }
    }
    if merged.is_empty() {
        return;
    }
    let target_state_dir = global_state_dir(data_dir);
    if let Err(e) = filesystem.create_dir_all(&target_state_dir) {
        tracing::warn!("orchestrator migration: failed to create {target_state_dir:?}: {e}");
        return;
    }
    for (plugin, map) in &merged {
        let path = global_plugin_state_path(data_dir, plugin);
        let bytes = match serde_json::to_vec_pretty(map) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("orchestrator migration: failed to serialise plugin {plugin}: {e}");
                continue;
            }
        };
        if let Err(e) = filesystem.write_file(&path, &bytes) {
            tracing::warn!("orchestrator migration: failed to write {path:?}: {e}");
        }
    }
    for legacy_p in legacy_to_rename {
        let backup = legacy_p.with_extension("json.migrated.bak");
        if let Err(e) = filesystem.rename(&legacy_p, &backup) {
            tracing::warn!(
                "orchestrator migration: failed to rename {legacy_p:?} → {backup:?}: {e}"
            );
        }
    }
    tracing::info!(
        "orchestrator persistence: migrated plugin state for {} plugins",
        merged.len()
    );
}

fn unique_plugin_state_temp(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("plugin-state.json");
    path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()))
}

fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("persistence path has no parent directory"))?;
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    {
        // `replace_plugin_state_temp` uses MOVEFILE_WRITE_THROUGH, Windows'
        // durable-publication boundary for a renamed directory entry.
        let _ = path;
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_plugin_state_temp(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    temp: &Path,
    target: &Path,
) -> io::Result<()> {
    filesystem.rename(temp, target)
}

#[cfg(windows)]
fn replace_plugin_state_temp(
    _filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    temp: &Path,
    target: &Path,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
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

fn persist_plugin_global_state_transaction(
    filesystem: &(dyn crate::model::filesystem::FileSystem + Send + Sync),
    data_dir: &Path,
    plugin: &str,
    dirty: &HashSet<String>,
    memory: Option<&HashMap<String, serde_json::Value>>,
    after_read: impl FnOnce(),
) -> io::Result<()> {
    let state_dir = global_state_dir(data_dir);
    filesystem.create_dir_all(&state_dir)?;

    let path = global_plugin_state_path(data_dir, plugin);
    let lock_path = path.with_extension("json.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock_file.lock()?;

    let mut merged: HashMap<String, serde_json::Value> = match filesystem.read_file(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => HashMap::new(),
        Err(error) => return Err(error),
    };
    after_read();

    for key in dirty {
        match memory.and_then(|values| values.get(key)) {
            Some(value) => {
                merged.insert(key.clone(), value.clone());
            }
            None => {
                merged.remove(key);
            }
        }
    }

    let bytes = serde_json::to_vec_pretty(&merged).map_err(io::Error::other)?;
    let temp = unique_plugin_state_temp(&path);
    let publication = (|| -> io::Result<()> {
        let mut writer = filesystem.create_file(&temp)?;
        writer.write_all(&bytes)?;
        writer.sync_all()?;
        drop(writer);
        replace_plugin_state_temp(filesystem, &temp, &path)?;
        sync_parent_directory(&path)
    })();
    if publication.is_err() {
        #[allow(clippy::let_underscore_must_use)]
        let _ = filesystem.remove_file(&temp);
    }
    publication
}
impl Editor {
    /// Persist `sessions` + `plugin_global_state` to disk. Best-
    /// effort: filesystem errors are logged at WARN and swallowed
    /// so a transient permission glitch doesn't block quit.
    pub fn save_orchestrator_state(&mut self) {
        // Sessions are no longer written to a central windows.json:
        // each window's identity (label, per-session plugin_state) is
        // persisted in its own per-dir workspace file by
        // `save_all_windows_workspaces` (called just before this on
        // quit), and the session list is rediscovered from those files
        // at boot. Only editor-global plugin state is written here.
        //
        // Every change is already flushed eagerly by `handle_set_global_state`,
        // so this quit-time pass is a backstop for keys whose eager flush
        // failed (they stay dirty). Plugins with no locally-changed keys are
        // deliberately NOT written: their in-memory map is a boot-time
        // snapshot, and rewriting it would revert anything a concurrently
        // running editor process saved since this one started.
        let plugins: Vec<String> = self.plugin_global_dirty.keys().cloned().collect();
        for plugin in plugins {
            self.persist_plugin_global_state(&plugin);
        }
    }

    /// Flush one plugin's locally-changed global-state keys through the host
    /// filesystem under a per-plugin interprocess lock. Publication uses a
    /// unique same-directory temp, file sync, atomic rename, and parent sync.
    /// Called eagerly from `handle_set_global_state` on every
    /// mutation — not just at clean quit — so a killed or crashed editor
    /// doesn't forget editor-global plugin state (e.g. the Orchestrator
    /// dock's folders and session→folder assignments; issue #2703). Mirrors
    /// the eager per-session workspace checkpointing
    /// (`checkpoint_window_workspace`). Best-effort: errors are logged at
    /// WARN and swallowed (failed keys stay dirty for the next attempt).
    ///
    /// The state file is shared with any concurrently running editor
    /// process, and each process only loads it once at boot — so this MERGES
    /// rather than snapshots: it re-reads the file and overlays exactly the
    /// keys this instance changed (`plugin_global_dirty`), leaving keys other
    /// instances wrote in the meantime intact. Writing the whole in-memory
    /// map here used to let a stale instance's fold-toggle (or quit) silently
    /// revert another instance's entire dock organisation.
    ///
    /// Same-key writes remain last-writer-wins, and a deletion still writes
    /// the file even when the result is empty (`{}`) so clearing a plugin's
    /// last key survives a crash too.
    pub(crate) fn persist_plugin_global_state(&mut self, plugin: &str) {
        if !plugin_name_is_safe(plugin) {
            tracing::warn!(
                "orchestrator persistence: skipping plugin with unsafe name: {plugin:?}"
            );
            return;
        }
        let Some(dirty) = self.plugin_global_dirty.get(plugin).cloned() else {
            return;
        };
        if dirty.is_empty() {
            return;
        }

        let result = persist_plugin_global_state_transaction(
            self.local_filesystem.as_ref(),
            &self.dir_context.data_dir,
            plugin,
            &dirty,
            self.plugin_global_state.get(plugin),
            || {},
        );
        if let Err(error) = result {
            tracing::warn!(
                "orchestrator persistence: failed to persist plugin {plugin:?}: {error}"
            );
            return;
        }

        let remove_slot = if let Some(current) = self.plugin_global_dirty.get_mut(plugin) {
            for key in &dirty {
                current.remove(key);
            }
            current.is_empty()
        } else {
            false
        };
        if remove_slot {
            self.plugin_global_dirty.remove(plugin);
        }
    }
}

/// Pull `project_path` (PathBuf) and `shared_worktree` (bool)
/// out of a session's per-plugin state, if the orchestrator
/// plugin has set them via `setWindowState`. Both keys live
/// under the `"orchestrator"` plugin slot; the keys are
/// `"project_path"` and `"shared_worktree"`.
fn read_orch_session_meta(
    plugin_state: &HashMap<String, HashMap<String, serde_json::Value>>,
) -> (Option<PathBuf>, bool) {
    let slot = plugin_state.get("orchestrator");
    let project_path = slot
        .and_then(|m| m.get("project_path"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from);
    let shared_worktree = slot
        .and_then(|m| m.get("shared_worktree"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (project_path, shared_worktree)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    use crate::config_io::DirectoryContext;

    #[test]
    fn paths_live_under_data_dir_not_working_dir() {
        // Regression test for issue #1991: orchestrator persistence
        // must never write inside the user's working tree.
        let temp = tempfile::tempdir().unwrap();
        let context = DirectoryContext::for_testing(temp.path());
        let data_dir = context.data_dir.as_path();
        let working_dir = temp.path().join("project");

        let wp = global_windows_path(data_dir);
        let sd = global_state_dir(data_dir);
        let psp = global_plugin_state_path(data_dir, "orchestrator");

        assert!(
            wp.starts_with(data_dir),
            "windows_path must live under data_dir, got {wp:?}"
        );
        assert!(
            sd.starts_with(data_dir),
            "state_dir must live under data_dir, got {sd:?}"
        );
        assert!(
            psp.starts_with(data_dir),
            "plugin_state_path must live under data_dir, got {psp:?}"
        );

        for p in [&wp, &sd, &psp] {
            assert!(
                !p.starts_with(&working_dir),
                "orchestrator path must not be inside the working tree: {p:?}"
            );
            for component in p.components() {
                if let std::path::Component::Normal(c) = component {
                    assert_ne!(
                        c, ".fresh",
                        "orchestrator path must not contain a `.fresh` component: {p:?}"
                    );
                }
            }
        }
    }

    fn make_window(id: u64, root: &str, project_path: Option<&str>) -> PersistedWindow {
        PersistedWindow {
            id,
            label: String::new(),
            root: PathBuf::from(root),
            project_path: project_path.map(PathBuf::from),
            shared_worktree: false,
            authority_spec: Default::default(),
            plugin_state: HashMap::new(),
            stable_id: None,
        }
    }

    fn env_with(active: u64, windows: Vec<PersistedWindow>) -> PersistedWindows {
        PersistedWindows {
            version: CURRENT_VERSION,
            active,
            next_id: windows.iter().map(|w| w.id).max().unwrap_or(0) + 1,
            windows,
        }
    }

    #[test]
    fn pick_active_never_crosses_projects() {
        // Regression for the orchestration bug: launching in /repoB
        // must never bring up a session rooted in /repoA, even when
        // /repoA holds the globally last-used session (env.active).
        let env = env_with(
            2,
            vec![
                make_window(1, "/repoA", Some("/repoA")),
                make_window(2, "/repoA", Some("/repoA")),
                make_window(3, "/repoB", Some("/repoB")),
            ],
        );
        let picked = pick_active_window_for_cwd(Some(&env), Path::new("/repoB"))
            .expect("a /repoB session exists");
        assert_eq!(
            picked.id, 3,
            "must pick the /repoB session, not env.active=2"
        );
    }

    #[test]
    fn pick_active_reopens_last_used_for_cwd() {
        // env.active points at this project's last-used session — it
        // wins even though it isn't the highest id.
        let env = env_with(
            2,
            vec![
                make_window(2, "/repoA", Some("/repoA")),
                make_window(5, "/repoA", Some("/repoA")),
            ],
        );
        let picked =
            pick_active_window_for_cwd(Some(&env), Path::new("/repoA")).expect("matching window");
        assert_eq!(
            picked.id, 2,
            "env.active is the last-used session for the cwd"
        );
    }

    #[test]
    fn pick_active_falls_back_to_most_recent_session_for_cwd() {
        // The globally last-used session (env.active=9) is in another
        // project, so for /repoA we fall back to the most-recently-
        // created /repoA session (highest id), not the first.
        let env = env_with(
            9,
            vec![
                make_window(2, "/repoA", Some("/repoA")),
                make_window(7, "/repoA", Some("/repoA")),
                make_window(9, "/repoB", Some("/repoB")),
            ],
        );
        let picked =
            pick_active_window_for_cwd(Some(&env), Path::new("/repoA")).expect("matching window");
        assert_eq!(picked.id, 7, "fall back to the most recent /repoA session");
    }

    #[test]
    fn pick_active_returns_none_when_no_window_matches_cwd() {
        // No session for this cwd → caller boots a clean base window.
        let env = env_with(
            1,
            vec![
                make_window(1, "/repoA", Some("/repoA")),
                make_window(2, "/repoB", Some("/repoB")),
            ],
        );
        assert!(pick_active_window_for_cwd(Some(&env), Path::new("/repoC")).is_none());
    }

    #[test]
    fn pick_active_falls_back_to_root_when_project_path_missing() {
        // Legacy v1-migrated entries may lack project_path; match on root.
        let env = env_with(
            2,
            vec![
                make_window(1, "/repoA", None),
                make_window(2, "/repoB", None),
            ],
        );
        let picked =
            pick_active_window_for_cwd(Some(&env), Path::new("/repoA")).expect("matching window");
        assert_eq!(picked.id, 1);
    }

    #[test]
    fn global_paths_are_independent_of_working_dir() {
        // v2: persistence is global, not per-cwd. Two different
        // cwds resolve to the same file path so the user sees
        // their full session history regardless of where the
        // editor was launched from.
        let temp = tempfile::tempdir().unwrap();
        let context = DirectoryContext::for_testing(temp.path());
        let data_dir = context.data_dir.as_path();
        let a = global_windows_path(data_dir);
        let b = global_windows_path(data_dir);
        assert_eq!(a, b);
        assert_eq!(a, data_dir.join("orchestrator").join("windows.json"));
    }

    #[test]
    fn discover_gcs_missing_dirs_and_yields_one_session_per_existing_dir() {
        use crate::model::filesystem::StdFileSystem;
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path();
        let ws_dir = workspaces_dir(data_dir);
        std::fs::create_dir_all(&ws_dir).unwrap();

        // A workspace file for an existing dir...
        let live = tempfile::tempdir().unwrap();
        let live_root = live.path().canonicalize().unwrap();
        let live_file = ws_dir.join("live.json");
        std::fs::write(
            &live_file,
            serde_json::to_vec(&serde_json::json!({
                "working_dir": live_root, "label": "live-session",
            }))
            .unwrap(),
        )
        .unwrap();

        // ...and one for a directory that does not exist.
        let dead_file = ws_dir.join("dead.json");
        std::fs::write(
            &dead_file,
            serde_json::to_vec(&serde_json::json!({
                "working_dir": "/no/such/dir/anywhere", "label": "dead",
            }))
            .unwrap(),
        )
        .unwrap();

        let fs = StdFileSystem;
        let sessions = discover_sessions(&fs, data_dir);

        assert_eq!(sessions.len(), 1, "only the existing dir yields a session");
        assert_eq!(sessions[0].root, live_root);
        assert_eq!(sessions[0].label, "live-session");
        assert!(!dead_file.exists(), "the dead dir's cache file was GC'd");
        let stable_id = sessions[0]
            .stable_id
            .as_deref()
            .expect("legacy workspace receives a stable identity");
        assert!(!live_file.exists(), "the legacy filename is retired");
        assert!(
            workspace_file_for_id(data_dir, &live_root, stable_id).exists(),
            "the live workspace is published under its exact identity"
        );
    }

    #[test]
    fn discover_reads_authority_spec_so_remote_sessions_arent_lost() {
        // A session that was running on a remote backend persists an
        // `authority_spec` in its workspace file; discovery must surface it
        // (so restore can reconnect rather than degrade to local). A file
        // without the field reads back as `Local` — back-compat for sessions
        // written before per-session backends existed.
        use crate::model::filesystem::StdFileSystem;
        use crate::services::authority::{
            AuthorityPayload, FilesystemSpec, SessionAuthoritySpec, SpawnerSpec,
            TerminalWrapperSpec,
        };
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path();
        let ws_dir = workspaces_dir(data_dir);
        std::fs::create_dir_all(&ws_dir).unwrap();

        let remote_root = tempfile::tempdir().unwrap();
        let remote_root = remote_root.path().canonicalize().unwrap();
        let spec = SessionAuthoritySpec::Plugin(AuthorityPayload {
            filesystem: FilesystemSpec::Local,
            spawner: SpawnerSpec::DockerExec {
                container_id: "abc123".into(),
                user: Some("vscode".into()),
                workspace: Some("/workspaces/proj".into()),
                env: Vec::new(),
            },
            terminal_wrapper: TerminalWrapperSpec::HostShell,
            display_label: "Container:abc123".into(),
            path_translation: None,
        });
        std::fs::write(
            ws_dir.join("remote.json"),
            serde_json::to_vec(&serde_json::json!({
                "working_dir": remote_root,
                "label": "remote-session",
                "authority_spec": spec,
            }))
            .unwrap(),
        )
        .unwrap();

        // A plain local session with no `authority_spec` field at all.
        let local_root = tempfile::tempdir().unwrap();
        let local_root = local_root.path().canonicalize().unwrap();
        std::fs::write(
            ws_dir.join("local.json"),
            serde_json::to_vec(&serde_json::json!({
                "working_dir": local_root, "label": "local-session",
            }))
            .unwrap(),
        )
        .unwrap();

        let fs = StdFileSystem;
        let sessions = discover_sessions(&fs, data_dir);

        let remote = sessions
            .iter()
            .find(|s| s.label == "remote-session")
            .expect("remote session discovered");
        assert_eq!(
            remote.authority_spec, spec,
            "the remote backend spec round-trips through discovery"
        );
        let local = sessions
            .iter()
            .find(|s| s.label == "local-session")
            .expect("local session discovered");
        assert_eq!(
            local.authority_spec,
            SessionAuthoritySpec::Local,
            "a session with no persisted spec reads back as Local"
        );
    }

    #[test]
    fn discover_keeps_remote_session_whose_root_is_absent_locally() {
        // Regression: a running SSH session persists a `working_dir` that is a
        // path on the *remote* host — it does not (and need not) exist on the
        // local filesystem. Discovery runs the GC check against the local
        // filesystem, so before the fix `is_dir` answered `Ok(false)` and the
        // remote session's workspace file was deleted on the next boot,
        // dropping it from the Orchestrator dock. A remote session must survive
        // discovery even though its root is absent locally.
        use crate::model::filesystem::StdFileSystem;
        use crate::services::authority::{
            RemoteAgentSpec, RemoteTransportSpec, SessionAuthoritySpec,
        };
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path();
        let ws_dir = workspaces_dir(data_dir);
        std::fs::create_dir_all(&ws_dir).unwrap();

        // A path that does not exist on the local filesystem — it lives on the
        // remote host the SSH session is rooted at.
        let remote_only_root = "/home/remote-user/project-on-remote-host";
        assert!(
            !Path::new(remote_only_root).exists(),
            "test precondition: the remote root must not exist locally"
        );
        let spec = SessionAuthoritySpec::RemoteAgent(RemoteAgentSpec {
            transport: RemoteTransportSpec::Ssh {
                user: Some("remote-user".into()),
                host: "example.com".into(),
                port: None,
                identity_file: None,
                remote_path: Some(remote_only_root.into()),
                extra_args: Vec::new(),
            },
            verified_anchor: None,
            canonical_root: None,
            base_env: Vec::new(),
            window: true,
            label: Some("ssh-session".into()),
            command: None,
        });
        std::fs::write(
            ws_dir.join("ssh.json"),
            serde_json::to_vec(&serde_json::json!({
                "working_dir": remote_only_root,
                "label": "ssh-session",
                "authority_spec": spec,
            }))
            .unwrap(),
        )
        .unwrap();

        let fs = StdFileSystem;
        let sessions = discover_sessions(&fs, data_dir);

        let ssh = sessions
            .iter()
            .find(|s| s.label == "ssh-session")
            .expect("the SSH session survives discovery despite a remote-only root");
        assert_eq!(ssh.authority_spec, spec);
        let stable_id = ssh
            .stable_id
            .as_deref()
            .expect("remote legacy workspace receives a stable identity");
        assert!(
            !ws_dir.join("ssh.json").exists(),
            "legacy filename is retired"
        );
        assert!(
            workspace_file_for_id(data_dir, Path::new(remote_only_root), stable_id).exists(),
            "the remote session is retained under its exact stable identity"
        );
    }

    #[test]
    fn legacy_remote_sessions_at_the_same_root_keep_authority_and_exact_identity() {
        use crate::model::filesystem::StdFileSystem;
        use crate::services::authority::{
            RemoteAgentSpec, RemoteTransportSpec, SessionAuthoritySpec,
        };

        fn ssh_spec(host: &str, remote_root: &str) -> SessionAuthoritySpec {
            SessionAuthoritySpec::RemoteAgent(RemoteAgentSpec {
                transport: RemoteTransportSpec::Ssh {
                    user: Some("builder".into()),
                    host: host.into(),
                    port: None,
                    identity_file: None,
                    remote_path: Some(remote_root.into()),
                    extra_args: Vec::new(),
                },
                verified_anchor: None,
                canonical_root: None,
                base_env: Vec::new(),
                window: true,
                label: None,
                command: None,
            })
        }

        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path();
        let ws_dir = workspaces_dir(data_dir);
        std::fs::create_dir_all(&ws_dir).unwrap();
        let root = "/srv/shared/project";
        let host_a = ssh_spec("alpha.example", root);
        let host_b = ssh_spec("beta.example", root);
        let legacy_a = ws_dir.join("host-a.json");
        let legacy_b = ws_dir.join("host-b.json");
        std::fs::write(
            &legacy_a,
            serde_json::to_vec(&serde_json::json!({
                "working_dir": root,
                "label": "host-a",
                "stable_id": "ws-existing-host-a",
                "authority_spec": host_a,
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            &legacy_b,
            serde_json::to_vec(&serde_json::json!({
                "working_dir": root,
                "label": "host-b",
                "authority_spec": host_b,
            }))
            .unwrap(),
        )
        .unwrap();

        let fs = StdFileSystem;
        let first = discover_sessions(&fs, data_dir);
        assert_eq!(
            first.len(),
            2,
            "different SSH authorities must not suppress each other"
        );
        let a = first
            .iter()
            .find(|window| window.label == "host-a")
            .unwrap();
        let b = first
            .iter()
            .find(|window| window.label == "host-b")
            .unwrap();
        assert_eq!(a.stable_id.as_deref(), Some("ws-existing-host-a"));
        let b_id = b
            .stable_id
            .as_deref()
            .expect("id-less legacy file is migrated");
        assert_ne!(b_id, "ws-existing-host-a");
        assert_eq!(a.authority_spec, host_a);
        assert_eq!(b.authority_spec, host_b);
        assert!(!legacy_a.exists() && !legacy_b.exists());
        assert!(workspace_file_for_id(data_dir, Path::new(root), "ws-existing-host-a").exists());
        assert!(workspace_file_for_id(data_dir, Path::new(root), b_id).exists());

        let second = discover_sessions(&fs, data_dir);
        let second_b = second
            .iter()
            .find(|window| window.label == "host-b")
            .unwrap();
        assert_eq!(second_b.stable_id.as_deref(), Some(b_id));
    }

    #[test]
    fn discover_rekeys_cross_authority_duplicate_workspace_ids() {
        use crate::model::filesystem::StdFileSystem;
        use crate::services::authority::{
            RemoteAgentSpec, RemoteTransportSpec, SessionAuthoritySpec,
        };

        fn ssh_spec(host: &str, remote_root: &str) -> SessionAuthoritySpec {
            SessionAuthoritySpec::RemoteAgent(RemoteAgentSpec {
                transport: RemoteTransportSpec::Ssh {
                    user: Some("builder".into()),
                    host: host.into(),
                    port: None,
                    identity_file: None,
                    remote_path: Some(remote_root.into()),
                    extra_args: Vec::new(),
                },
                verified_anchor: None,
                canonical_root: None,
                base_env: Vec::new(),
                window: true,
                label: None,
                command: None,
            })
        }

        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path();
        let ws_dir = workspaces_dir(data_dir);
        std::fs::create_dir_all(&ws_dir).unwrap();
        let root = "/srv/shared/project";
        let authority_a = ssh_spec("alpha.example", root);
        let authority_b = ssh_spec("beta.example", root);
        for (path, label, authority) in [
            (ws_dir.join("alpha.json"), "alpha", &authority_a),
            (
                workspace_file_for_id(data_dir, Path::new(root), "ws-shared-across-authorities"),
                "beta",
                &authority_b,
            ),
        ] {
            std::fs::write(
                path,
                serde_json::to_vec(&serde_json::json!({
                    "working_dir": root,
                    "label": label,
                    "stable_id": "ws-shared-across-authorities",
                    "authority_spec": authority,
                }))
                .unwrap(),
            )
            .unwrap();
        }

        let fs = StdFileSystem;
        let first = discover_sessions(&fs, data_dir);
        assert_eq!(
            first.len(),
            2,
            "both authority candidates remain discoverable"
        );
        let ids: HashSet<&str> = first
            .iter()
            .map(|window| window.stable_id.as_deref().unwrap())
            .collect();
        assert_eq!(ids.len(), 2, "public workspace ids must be globally unique");
        assert!(ids.contains("ws-shared-across-authorities"));
        for window in &first {
            assert!(workspace_file_for_id(
                data_dir,
                &window.root,
                window.stable_id.as_deref().unwrap(),
            )
            .exists());
        }
        let first_by_label: HashMap<String, String> = first
            .iter()
            .map(|window| {
                (
                    window.label.clone(),
                    window.stable_id.clone().expect("discovered stable id"),
                )
            })
            .collect();

        let second_by_label: HashMap<String, String> = discover_sessions(&fs, data_dir)
            .into_iter()
            .map(|window| {
                (
                    window.label,
                    window.stable_id.expect("rediscovered stable id"),
                )
            })
            .collect();
        assert_eq!(
            second_by_label, first_by_label,
            "the collision replacement must be durable and deterministic"
        );
    }

    #[test]
    fn migrate_folds_windows_json_into_workspace_files_and_retires_it() {
        use crate::model::filesystem::StdFileSystem;
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path();
        let proj = tempfile::tempdir().unwrap();
        let proj_root = proj.path().canonicalize().unwrap();

        // An existing per-dir workspace file with no label yet.
        let ws_path = workspace_file_for(data_dir, &proj_root);
        std::fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::fs::write(
            &ws_path,
            serde_json::to_vec(&serde_json::json!({ "working_dir": proj_root })).unwrap(),
        )
        .unwrap();

        // A legacy windows.json naming that session with a label.
        let global_p = global_windows_path(data_dir);
        std::fs::create_dir_all(global_p.parent().unwrap()).unwrap();
        std::fs::write(
            &global_p,
            serde_json::to_vec(&serde_json::json!({
                "version": 2, "active": 1, "next_id": 2,
                "windows": [ { "id": 1, "label": "from-windows-json", "root": proj_root } ],
            }))
            .unwrap(),
        )
        .unwrap();

        let fs = StdFileSystem;
        migrate_windows_json_into_workspaces(&fs, data_dir);

        assert!(!global_p.exists(), "windows.json is retired");
        assert!(
            global_p.with_extension("json.retired.bak").exists(),
            "a .retired.bak is kept"
        );
        let val: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ws_path).unwrap()).unwrap();
        assert_eq!(
            val.get("label").and_then(|v| v.as_str()),
            Some("from-windows-json"),
            "the label was folded into the per-dir workspace file"
        );
    }

    #[test]
    fn concurrent_global_state_transactions_preserve_disjoint_keys() {
        use crate::model::filesystem::StdFileSystem;
        use std::sync::{mpsc, Arc, Barrier};
        use std::time::Duration;

        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path().to_path_buf();
        let filesystem = Arc::new(StdFileSystem);
        let (first_read_tx, first_read_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let first_fs = Arc::clone(&filesystem);
        let first_data = data_dir.clone();
        let first = std::thread::spawn(move || {
            let dirty = HashSet::from(["first".to_string()]);
            let memory = HashMap::from([("first".to_string(), serde_json::json!(1))]);
            persist_plugin_global_state_transaction(
                first_fs.as_ref(),
                &first_data,
                "orchestrator",
                &dirty,
                Some(&memory),
                || {
                    first_read_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            )
        });

        // Hold the first writer after its read. The second writer starts at a
        // deterministic barrier: without the per-plugin lock it publishes its
        // stale merge before the first resumes, and one disjoint key is lost.
        first_read_rx.recv().unwrap();
        let start = Arc::new(Barrier::new(2));
        let second_start = Arc::clone(&start);
        let second_fs = Arc::clone(&filesystem);
        let second_data = data_dir.clone();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = std::thread::spawn(move || {
            let dirty = HashSet::from(["second".to_string()]);
            let memory = HashMap::from([("second".to_string(), serde_json::json!(2))]);
            second_start.wait();
            let result = persist_plugin_global_state_transaction(
                second_fs.as_ref(),
                &second_data,
                "orchestrator",
                &dirty,
                Some(&memory),
                || {},
            );
            second_done_tx.send(result).unwrap();
        });
        start.wait();
        let premature = second_done_rx.recv_timeout(Duration::from_millis(100));
        let second_was_blocked = matches!(&premature, Err(mpsc::RecvTimeoutError::Timeout));
        release_tx.send(()).unwrap();

        let second_result = match premature {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => second_done_rx.recv().unwrap(),
            Err(error) => panic!("second writer completion channel failed: {error}"),
        };
        first.join().unwrap().unwrap();
        second_result.unwrap();
        second.join().unwrap();
        assert!(
            second_was_blocked,
            "the second writer published while the first held the plugin lock"
        );

        let path = global_plugin_state_path(&data_dir, "orchestrator");
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(state["first"], 1);
        assert_eq!(state["second"], 2);
        assert!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp")),
            "successful publishers must remove only their own unique temps"
        );
    }
}
