//! Editor methods for window lifecycle (create, switch, close).
//!
//! Windows are introduced in
//! `docs/internal/orchestrator-sessions-design.md`. After Step 0b each
//! window owns its file tree, file mod-times, LSP set, panel-id
//! map, and split layout outright. `set_active_window` is therefore
//! a pointer write (plus seed-buffer allocation when diving into a
//! never-activated window) — there are no warm-swap stashes left to
//! shuffle. Plugins that listen for `active_window_changed` see the
//! same hook sequence as before.

use crate::app::window::Window;
use crate::app::window_resources::WindowResources;
use crate::services::plugins::hooks::HookArgs;
use crate::view::split::{SplitManager, SplitViewState};
use fresh_core::WindowId;
use std::collections::HashMap;
use std::path::PathBuf;

/// Seed state for a freshly-built window layout: the base buffer plus the
/// editor/metadata/event-log/split scaffolding returned by
/// [`Editor::build_fresh_layout_if_needed`].
type FreshLayoutSeed = (
    fresh_core::BufferId,
    crate::state::EditorState,
    crate::app::types::BufferMetadata,
    crate::model::event::EventLog,
    SplitManager,
    HashMap<crate::model::event::LeafId, SplitViewState>,
);

fn rollback_terminal_artifacts(moved: &[crate::workspace::TerminalArtifactRelocation]) -> bool {
    for move_ in moved.iter().rev() {
        match (move_.source.exists(), move_.destination.exists()) {
            (true, false) => continue,
            (false, true) => {
                if let Err(error) =
                    crate::workspace::durable_rename(&move_.destination, &move_.source)
                {
                    tracing::error!(
                        "failed to roll terminal artifact {} back to {}: {error}",
                        move_.destination.display(),
                        move_.source.display()
                    );
                    return false;
                }
            }
            (true, true) => {
                tracing::error!(
                    "cannot roll terminal artifact back because both {} and {} exist",
                    move_.source.display(),
                    move_.destination.display()
                );
                return false;
            }
            (false, false) => {
                tracing::error!(
                    "cannot roll terminal artifact back because both {} and {} are missing",
                    move_.source.display(),
                    move_.destination.display()
                );
                return false;
            }
        }
    }
    true
}

fn finish_terminal_artifacts<'a>(
    moved: impl IntoIterator<Item = &'a crate::workspace::TerminalArtifactRelocation>,
) -> bool {
    for move_ in moved {
        match (move_.source.exists(), move_.destination.exists()) {
            (false, true) => continue,
            (true, false) => {
                if let Err(error) =
                    crate::workspace::durable_rename(&move_.source, &move_.destination)
                {
                    tracing::error!(
                        "failed to move terminal artifact {} to {}: {error}",
                        move_.source.display(),
                        move_.destination.display()
                    );
                    return false;
                }
            }
            (true, true) => {
                tracing::error!(
                    "cannot move terminal artifact because both {} and {} exist",
                    move_.source.display(),
                    move_.destination.display()
                );
                return false;
            }
            (false, false) => {
                tracing::error!(
                    "cannot move terminal artifact because both {} and {} are missing",
                    move_.source.display(),
                    move_.destination.display()
                );
                return false;
            }
        }
    }
    true
}

impl crate::app::Editor {
    /// Snapshot the editor-global resources every new `Window` needs.
    /// All fields are cheap clones (`Arc` increments or `Clone`-by-value
    /// where the inner type already holds `Arc`s, like `Authority`).
    /// Called by `create_window_at` and by the first-dive seed path in
    /// `set_active_window`; also by `editor_init` for the base window.
    pub(crate) fn window_resources(&self) -> WindowResources {
        WindowResources {
            config: std::sync::Arc::clone(&self.config),
            grammar_registry: std::sync::Arc::clone(&self.grammar_registry),
            theme_registry: std::sync::Arc::clone(&self.theme_registry),
            theme_cache: std::sync::Arc::clone(&self.theme_cache),
            keybindings: std::sync::Arc::clone(&self.keybindings),
            command_registry: std::sync::Arc::clone(&self.command_registry),
            // Derive the window's fs_manager from the *same* authority we hand
            // it below, so directory listings (the file explorer) ride the
            // window's filesystem — local or remote — instead of a stale,
            // boot-time local one. A born-attached SSH/k8s window otherwise
            // showed the local machine in the explorer while its terminal ran
            // remote, because the cached fs_manager never tracked the authority.
            // Default to a host fs_manager; callers that build a window with a
            // non-local authority re-derive it from that authority's
            // filesystem. The window's `authority` is set on the `Window`
            // itself (not here in the `Clone`-fanned resources).
            fs_manager: std::sync::Arc::new(crate::services::fs::FsManager::new(
                std::sync::Arc::new(crate::model::filesystem::StdFileSystem),
            )),
            local_filesystem: std::sync::Arc::clone(&self.local_filesystem),
            buffer_id_alloc: self.buffer_id_alloc.clone(),
            time_source: std::sync::Arc::clone(&self.time_source),
            dir_context: self.dir_context.clone(),
            tokio_runtime: self.tokio_runtime.clone(),
            async_bridge: self.async_bridge.clone(),
            plugin_manager: std::rc::Rc::clone(&self.plugin_manager),
            theme: std::sync::Arc::clone(&self.theme),
            event_broadcaster: self.event_broadcaster.clone(),
            recovery_service: std::sync::Arc::clone(&self.recovery_service),
            mouse_capture: std::sync::Arc::clone(&self.mouse_capture),
        }
    }

    /// Allocate a session id, insert a new `Session`, fire
    /// `session_created`. Does not switch active.
    ///
    /// Caller is responsible for ensuring `root` is absolute. The
    /// `PluginCommand::CreateWindow` dispatcher rejects relative
    /// paths before reaching here.
    ///
    /// Find an existing window whose root resolves to the same
    /// canonical directory, if any. Backs the one-session-per-dir
    /// invariant: opening a directory that already has a window
    /// reuses it rather than creating a duplicate.
    pub(crate) fn find_window_by_root(&self, root: &std::path::Path) -> Option<WindowId> {
        let key = crate::app::orchestrator_persistence::canonical_key(root);
        self.windows
            .iter()
            .find(|(_, w)| crate::app::orchestrator_persistence::canonical_key(&w.root) == key)
            .map(|(id, _)| *id)
    }

    /// Open the window for `root`, creating it if absent. Enforces
    /// one-session-per-directory: if a window already exists at the
    /// same canonical root it is returned as-is and `label` is
    /// ignored (the existing window keeps its label) — no duplicate
    /// is created.
    ///
    /// Seeds a freshly created window with an empty scratch buffer +
    /// a minimal split layout up front (same shape as the first-dive
    /// seed path), so the window is renderable immediately. Without
    /// this, never-dived windows have `splits == None` and any
    /// cross-window render (e.g. the Orchestrator preview pane's
    /// `WindowEmbed`) draws blank.
    pub fn create_window_at(&mut self, root: PathBuf, label: String) -> WindowId {
        // One session per directory: reuse an existing window at this
        // root instead of spawning a colliding duplicate.
        if let Some(existing) = self.find_window_by_root(&root) {
            return existing;
        }
        // A new window for `root` is its own local session with its **own**
        // per-session trust scoped to that root — not a clone of the active
        // session's authority/trust (which would leak a trust decision across
        // projects). Its `fs_manager` rides the same (host) filesystem.
        let local_authority = self.local_session_authority(&root);
        self.create_window_with_authority(root, label, local_authority)
    }

    /// Number of live windows whose canonical root matches `root` — the size
    /// of a root's co-tenant session group.
    pub(crate) fn windows_at_root_count(&self, root: &std::path::Path) -> usize {
        let key = crate::app::orchestrator_persistence::canonical_key(root);
        self.windows
            .values()
            .filter(|w| crate::app::orchestrator_persistence::canonical_key(&w.root) == key)
            .count()
    }

    /// Create a new **local** co-tenant window over `root`. Extraction is
    /// guarded before this point, so no remote authority configuration or
    /// connection can leak into the new window.
    fn create_co_tenant_window(&mut self, root: PathBuf) -> WindowId {
        let base = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        let n = self.windows_at_root_count(&root);
        let label = if n == 0 {
            base
        } else {
            format!("{base} ({})", n + 1)
        };
        let authority = self.local_session_authority(&root);
        self.create_window_with_authority(root, label, authority)
    }

    /// Drop the throwaway `[No Name]` seed a freshly created window is born
    /// with (via [`Self::build_fresh_layout_if_needed`], so it is renderable
    /// the instant `window_created` fires). Extraction is about to move the
    /// real tab in as the window's sole content, so clearing the seed first
    /// lets [`Self::move_buffer_membership_to_window`] re-seed the split
    /// rooted at the extracted buffer — otherwise the co-tenant opens showing
    /// the extracted tab *and* a stray empty `[No Name]`.
    ///
    /// Guarded to only ever touch that birth seed: exactly one buffer, unnamed
    /// and unmodified. Anything else is real content and is left untouched.
    /// Safe to call only before any render of `target` (the extract flow runs
    /// synchronously, so no render intervenes).
    fn discard_fresh_window_seed(&mut self, target: WindowId) {
        let Some(w) = self.windows.get_mut(&target) else {
            return;
        };
        let is_birth_seed = w.buffers.len() == 1
            && w.buffers
                .iter()
                .next()
                .is_some_and(|(_, s)| s.buffer.file_path().is_none() && !s.buffer.is_modified());
        if !is_birth_seed {
            return;
        }
        for id in w.buffers.ids() {
            w.buffers.remove(&id);
            w.buffer_metadata.remove(&id);
            w.event_logs.remove(&id);
        }
        w.buffers.clear_splits();
    }

    /// Drop only the throwaway window after a failed terminal extraction.
    /// Artifact cleanup is intentionally non-recursive: a prepared journal may
    /// still need files whose reverse rename failed, so recovery owns them.
    fn discard_failed_extraction_window(&mut self, target: WindowId) {
        let identity = self.windows.get(&target).map(|window| {
            (
                window.root.clone(),
                window.stable_id.clone(),
                window.terminal_artifacts_dir(),
            )
        });
        if let Some((root, stable_id, artifacts)) = identity {
            if let Err(error) = std::fs::remove_dir(&artifacts) {
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) {
                    tracing::error!(
                        "failed to remove empty aborted extraction directory {}: {error}",
                        artifacts.display()
                    );
                }
            }
            if let Err(error) =
                crate::workspace::Workspace::delete_by_id_in(&self.dir_context, &root, &stable_id)
            {
                tracing::warn!("failed to retire aborted extraction workspace: {error}");
            }
        }
        let invocation = self.plugin_invocation(target);
        if self.windows.remove(&target).is_some() {
            self.plugin_manager
                .read()
                .unwrap()
                .run_hook_with_invocation(
                    "window_closed",
                    HookArgs::WindowClosed { id: target.0 },
                    invocation,
                );
        }
    }

    /// Create a new window rooted at `root` under an explicit `authority`,
    /// seeded with an empty scratch buffer + minimal split layout (so it is
    /// renderable immediately) and announced via `window_created`.
    ///
    /// Unlike [`Self::create_window_at`] this does **not** dedup by root or
    /// mint a fresh local authority — the caller supplies the backend. That
    /// lets Switch Project (`change_working_dir`) carry a remote window's
    /// already-connected authority onto the new project so the new root opens
    /// on the same container / SSH host, while local callers pass a fresh
    /// [`Self::local_session_authority`].
    pub(crate) fn create_window_with_authority(
        &mut self,
        root: PathBuf,
        label: String,
        authority: crate::services::authority::Authority,
    ) -> WindowId {
        self.create_window_with_authority_and_stable_id(root, label, authority, None)
    }

    /// Variant used by lifecycle rollback. `Some(id)` installs the exact
    /// durable identity before workspace lookup; `Some("")` deliberately
    /// selects the sole legacy root-keyed workspace. Ordinary creates pass
    /// `None` and keep the freshly minted identity from [`Window::new`].
    pub(crate) fn create_window_with_authority_and_stable_id(
        &mut self,
        root: PathBuf,
        label: String,
        authority: crate::services::authority::Authority,
        stable_id_override: Option<String>,
    ) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;

        let mut resources = self.window_resources();
        resources.fs_manager = std::sync::Arc::new(crate::services::fs::FsManager::new(
            std::sync::Arc::clone(&authority.filesystem),
        ));
        let mut session = Window::new(id, label, root.clone(), authority, resources);
        if let Some(stable_id) = stable_id_override {
            session.stable_id = stable_id;
        }
        session.terminal_width = self.terminal_width;
        session.terminal_height = self.terminal_height;
        let resolved_label = session.label.clone();
        self.windows.insert(id, session);

        if let Some((buf, state, metadata, event_log, mgr, vs)) =
            self.build_fresh_layout_if_needed(id)
        {
            if let Some(s) = self.windows.get_mut(&id) {
                s.buffers.set_splits((mgr, vs));
                s.buffers.insert(buf, state);
                s.buffer_metadata.insert(buf, metadata);
                s.event_logs.insert(buf, event_log);
            }
        }

        self.run_plugin_hook_for_window(
            id,
            "window_created",
            HookArgs::WindowCreated {
                id: id.0,
                label: resolved_label,
                root: root.to_string_lossy().into_owned(),
            },
        );
        id
    }

    /// A fresh per-session execution scope (trust + env) for `root`. Local
    /// sessions key trust on the repository owner and env state on the exact
    /// worktree; remote sessions use the verified-agent identity factory in
    /// `services::authority` instead.
    pub(crate) fn session_scope_for(
        &self,
        root: &std::path::Path,
    ) -> crate::services::authority::SessionScope {
        let trust_owner = crate::services::workspace_trust::trust_owner_root(
            self.local_filesystem.as_ref(),
            root,
        );
        crate::services::authority::SessionScope::for_root(
            root,
            &self.dir_context.project_state_dir(root),
            &self.dir_context.project_state_dir(&trust_owner),
        )
    }

    /// A fresh local authority for a brand-new session rooted at `root`, with
    /// its **own** per-session trust + env (not clones of the active
    /// session's) and a host filesystem. The canonical backend for the
    /// Orchestrator's "New Session (Local)" flow: callers pass this to
    /// [`Self::create_window_with_terminal`] so a new session for a different
    /// project is born under its own local backend, trust, and env.
    pub fn local_session_authority(
        &self,
        root: &std::path::Path,
    ) -> crate::services::authority::Authority {
        crate::services::authority::Authority::local_scoped(self.session_scope_for(root))
    }

    /// Atomic "create a new window seeded with an agent terminal"
    /// entry point. Used by Orchestrator's new-session flow.
    ///
    /// Unlike `create_window_at`, this path deliberately does NOT
    /// seed an empty `[No Name]` buffer up front — the terminal
    /// becomes the window's seed via `create_plugin_terminal`'s
    /// no-active-split branch, so the new window is born with a
    /// single tab (the terminal) instead of `[No Name] | <agent>`.
    ///
    /// The eager-seed invariant `create_window_at` upholds
    /// ("window is renderable immediately after returning") still
    /// holds here: the call to `create_plugin_terminal` runs
    /// synchronously on the same thread before this function
    /// yields, installing the terminal-rooted split layout before
    /// any other code can observe the window. The `window_created`
    /// hook is intentionally fired *after* the terminal is wired
    /// up so plugin handlers see the new window in its final
    /// shape, not the half-built intermediate state.
    ///
    /// `root` must be absolute; the plugin-command dispatcher
    /// validates this before reaching here.
    ///
    /// `authority` is the backend the new session is born under — passed
    /// explicitly so this primitive never guesses. The Orchestrator's "New
    /// Session (Local)" flow hands it [`Self::local_session_authority`] (a
    /// fresh local backend sharing the editor's trust + env handles), so a
    /// new session for a *different* project does not inherit the active
    /// window's container/SSH/k8s backend just because that window was
    /// focused when "+ New" was clicked. The born-attached remote-session
    /// path (`create_remote_session_window`) passes its already-connected
    /// backend so the new window's filesystem, LSP spawner, and terminal all
    /// act remotely from birth.
    ///
    /// The editor-wide authority cache is re-pointed at the new active
    /// window via [`Self::adopt_active_window_authority`] before returning,
    /// so the status bar, quick-open, and the 100+ `self.authority` call
    /// sites reflect the session the user just landed on rather than the one
    /// they left.
    ///
    /// `resume` is the exact agent-resume argv; `relaunch` is the clean argv
    /// used when ordinary agent resume is disabled. The initial `command` may
    /// contain one-shot provisioning ids and prompts and is never persisted.
    #[allow(clippy::too_many_arguments)]
    pub fn create_window_with_terminal(
        &mut self,
        root: PathBuf,
        label: String,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        relaunch: Option<Vec<String>>,
        title: Option<String>,
        window_authority: crate::services::authority::Authority,
        resume: Option<Vec<String>>,
        env: Option<HashMap<String, String>>,
        allow_script: bool,
        companion: Option<fresh_core::api::TerminalCompanion>,
        selected_agent: bool,
        activate: bool,
        initial_state: Option<(String, HashMap<String, serde_json::Value>)>,
    ) -> Result<(WindowId, fresh_core::TerminalId, fresh_core::BufferId), String> {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;

        // The backend the editor was acting through before this new
        // session — captured so `adopt_active_window_authority` can tell
        // whether the active authority actually changed and skip the
        // hook/snapshot churn when it didn't.
        let previous_authority_label = self.authority().display_label.clone();

        let mut resources = self.window_resources();
        // Re-derive the window's `fs_manager` from *its* backend's filesystem
        // so the file explorer rides this session's backend, then build the
        // window owning `window_authority` outright.
        resources.fs_manager = std::sync::Arc::new(crate::services::fs::FsManager::new(
            std::sync::Arc::clone(&window_authority.filesystem),
        ));
        let mut session = Window::new(id, label, root.clone(), window_authority, resources);
        session.terminal_width = self.terminal_width;
        session.terminal_height = self.terminal_height;
        if let Some((plugin_name, state)) = initial_state {
            if !state.is_empty() {
                session.plugin_state.insert(plugin_name, state);
            }
        }
        let resolved_label = session.label.clone();
        self.windows.insert(id, session);

        // Dive into the new window before spawning the terminal
        // so `Window::create_plugin_terminal` operates on a window
        // with `splits.is_none()` — that's the "no active_split"
        // branch which seeds the layout rooted at the terminal
        // buffer. We bypass `set_active_window`'s
        // `build_fresh_layout_if_needed` call (which would install
        // a `[No Name]` seed) by writing the active-window pointer
        // directly.
        let previous_id = self.active_window;
        if activate {
            self.checkpoint_window_workspace(previous_id);
        }
        self.switch_active_window_pointer(id);

        // Persist the clean relaunch argv, not the initial provisioning
        // command. `None` (plain shell) is recorded as an empty vec: a present
        // entry — even empty — marks a restorable session terminal.
        let restore_command = relaunch.or_else(|| command.clone()).unwrap_or_default();

        // Assemble the extra env injected into the seeded terminal's child:
        // `FRESH_BIN` always, plus (when `allow_script` is set) a capability
        // token bound to this new window, so a client in the terminal can drive
        // the editor but no other window. Minting happens here — after the
        // window id is known, before the PTY spawns — so the token is live by
        // the time the child reads its env. See `terminal::agent_command_env`.
        let terminal_env = match crate::app::terminal::agent_command_env(id, env, allow_script) {
            Ok(env) => env,
            Err(error) => {
                let rollback = self.rollback_unpublished_terminal_window(id, previous_id, &root);
                return Err(if rollback.is_empty() {
                    error
                } else {
                    format!("{error}; rollback failures: {}", rollback.join("; "))
                });
            }
        };

        let spawn_result = {
            let target = self
                .windows
                .get_mut(&id)
                .expect("just-inserted window must be present");
            target.create_plugin_terminal(crate::app::terminal::PluginTerminalSpec {
                cwd: cwd.or_else(|| Some(root.clone())),
                direction: None, // no split direction — let the no-layout branch seed
                ratio: None,
                focus: true,       // newly spawned terminal is the seed
                persistent: false, // ephemeral by default; orchestrator owns persistence
                command,
                title: title.filter(|t| !t.is_empty()),
                env: terminal_env.vars.clone(),
                companion,
                script_capability: terminal_env.script_capability(),
            })
        };

        let (terminal_id, buffer_id, _split_id) = match spawn_result {
            Ok(triple) => triple,
            Err(error) => {
                terminal_env.revoke();
                let rollback = self.rollback_unpublished_terminal_window(id, previous_id, &root);
                return Err(if rollback.is_empty() {
                    error
                } else {
                    format!("{error}; rollback failures: {}", rollback.join("; "))
                });
            }
        };

        // Mark the freshly-spawned agent terminal restorable so workspace
        // capture persists it (with its command) and a later launch
        // re-runs it, instead of the session coming back as a blank pane.
        // An explicit `resume` argv (agent-resume) supersedes the launch
        // command on restore — see `restore_terminal_from_workspace`.
        if let Some(target) = self.windows.get_mut(&id) {
            target.mark_terminal_restorable(terminal_id, Some(restore_command), resume);
            // File the token this terminal's child was handed, so workspace
            // capture persists the grant and a restore re-mints it — without
            // it a restored agent keeps its conversation but loses the ability
            // to drive the editor.
            target.record_terminal_script_token(terminal_id, terminal_env.script_token.as_deref());
            if selected_agent {
                target.tracked_agent_terminal = Some(terminal_id);
            }
        }

        // Background creation returns focus before publication without firing
        // any active-window side effects. Activated creation keeps the exact
        // new window selected until its durable snapshot succeeds.
        if !activate {
            self.switch_active_window_pointer(previous_id);
        }

        // Register the leader pid with the new window's
        // process_groups so window-level signal operations reach
        // the spawned group. Mirrors `create_plugin_terminal`'s
        // registration in the active-target path of
        // `handle_create_terminal`, but kept here because we
        // bypass that dispatcher.
        if let Some(pid) = self
            .windows
            .get(&id)
            .and_then(|w| w.terminal_manager.get(terminal_id))
            .and_then(|h| h.pid())
        {
            let pg_label = format!("terminal #{}", terminal_id.0);
            if let Some(win) = self.windows.get_mut(&id) {
                win.process_groups.register(pid, pg_label);
            }
        }
        // Creation is acknowledged only after the terminal checkpoint and
        // workspace registry entry are both durable. No lifecycle hook or
        // trust prompt may observe a half-published session.
        if let Err(error) = self.save_workspace_for(id) {
            terminal_env.revoke();
            let rollback = self.rollback_unpublished_terminal_window(id, previous_id, &root);
            return Err(if rollback.is_empty() {
                format!("failed to publish new workspace: {error}")
            } else {
                format!(
                    "failed to publish new workspace: {error}; rollback failures: {}",
                    rollback.join("; ")
                )
            });
        }

        if activate {
            self.clear_panel_scoped_mode_on_switch_away(previous_id);
            self.adopt_active_window_authority(&previous_authority_label);
        }

        // A trust decision belongs only to a successfully-published local
        // session that is still the exact active window. Background or failed
        // creation must never steal focus with a prompt.
        if activate
            && self.active_window == id
            && self
                .authority()
                .filesystem
                .remote_connection_info()
                .is_none()
        {
            self.maybe_prompt_workspace_trust(true);
        }

        // Size the newly-created window's PTYs (mirrors
        // `set_active_window`'s post-dive resize so the seeded terminal
        // renders into the right cell rect on its first frame). Route
        // through the funnel rather than `win.resize_visible_terminals()`
        // directly: a brand-new window's `dock_cols` cache is still 0, and
        // `relayout` pushes the current editor-global dock width into every
        // window before sizing, so the seeded terminal accounts for a dock
        // that's already showing.
        self.relayout();

        // Plugin lifecycle: fire `window_created` first, then
        // `active_window_changed`. Order mirrors the
        // `create_window_at` + `set_active_window` sequence the
        // orchestrator previously chained — plugin handlers that
        // care about either event see the same payload order.
        self.run_plugin_hook_for_window(
            id,
            "window_created",
            HookArgs::WindowCreated {
                id: id.0,
                label: resolved_label,
                root: root.to_string_lossy().into_owned(),
            },
        );
        if activate && previous_id != id {
            self.run_plugin_hook_for_window(
                id,
                "active_window_changed",
                HookArgs::ActiveWindowChanged {
                    previous_id: Some(previous_id.0),
                    active_id: id.0,
                },
            );
        }
        #[cfg(feature = "plugins")]
        self.update_plugin_state_snapshot();
        if activate {
            #[cfg(feature = "plugins")]
            self.run_plugin_hook_for_window(
                id,
                "buffer_activated",
                crate::services::plugins::hooks::HookArgs::BufferActivated { buffer_id },
            );
        }

        Ok((id, terminal_id, buffer_id))
    }

    /// Reverse every side effect of a create attempt that never reached its
    /// durable publication point. The unpublished window is invisible to
    /// lifecycle hooks, so cleanup is direct and emits no close events.
    fn rollback_unpublished_terminal_window(
        &mut self,
        id: WindowId,
        previous_id: WindowId,
        root: &std::path::Path,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        let stable_id = self.windows.get(&id).map(|window| window.stable_id.clone());
        let terminal_ids = self
            .windows
            .get(&id)
            .map(|window| window.terminal_manager.tracked_terminal_ids())
            .unwrap_or_default();

        for terminal_id in &terminal_ids {
            self.purge_omp_companion_terminal(fresh_core::WindowTerminalId::new(id, *terminal_id));
        }
        if let Some(window) = self.windows.get_mut(&id) {
            window.revoke_all_terminal_script_tokens();
            for (entry, result) in window.process_groups.signal_all("SIGKILL") {
                if let Err(error) = result {
                    failures.push(format!(
                        "failed to stop process group {}: {error}",
                        entry.leader_pid
                    ));
                }
            }
            for terminal_id in terminal_ids {
                window.terminal_manager.close(terminal_id);
            }
        }
        if self.active_window == id {
            self.switch_active_window_pointer(previous_id);
        }
        let removed = self.windows.remove(&id);
        drop(removed);

        self.session_keepalives.remove(&id);
        self.pending_remote_reattach.remove(&id);
        self.remote_connected_cache.remove(&id);
        self.materialize_pending.remove(&id);

        if let Some(stable_id) = stable_id {
            if let Err(error) =
                crate::workspace::Workspace::delete_by_id_in(&self.dir_context, root, &stable_id)
            {
                failures.push(format!("failed to remove workspace snapshot: {error}"));
            }
            if let Err(error) = crate::workspace::delete_terminal_artifacts_by_id(
                &self.dir_context,
                root,
                &stable_id,
            ) {
                failures.push(format!("failed to remove terminal artifacts: {error}"));
            }
        }
        failures
    }

    /// Clear a floating-panel-scoped editor mode on the window we are
    /// switching *away* from.
    ///
    /// A plugin-defined editor mode (`editor.setEditorMode`) tied to a mounted
    /// floating widget panel — the Orchestrator picker (`orchestrator-open`) or
    /// new-session form (`orchestrator-new-form`) — is transient UI state that
    /// belongs to the *panel*, not to the window it was opened over.
    /// `setEditorMode` writes to whatever window is active when the plugin
    /// calls it, so a plugin that switches the active window while its panel is
    /// still mounted (the orchestrator "dive": `setActiveWindow(target)` first,
    /// then `closeOpenDialog()` / `closeForm()` which runs
    /// `setEditorMode(null)`) lands the clear on the *incoming* window and
    /// leaves the *outgoing* one stuck in the panel's mode. That stuck mode
    /// stays masked while the window sits in terminal mode, then silently
    /// swallows every printable key the moment the user leaves terminal mode
    /// (e.g. opens a file via quick-open) — the buffer ignores all keyboard
    /// input until the user switches sessions.
    ///
    /// Both window-switch paths must call this before moving the active
    /// pointer: the ordinary `set_active_window` dive *and* the born-attached
    /// remote session creation (`create_window_with_terminal`), which writes
    /// the active pointer directly and so never reaches `set_active_window`'s
    /// own guard. See #2237 / #2234 item 4.
    ///
    /// vi-mode and other persistent per-window modes are unaffected: they never
    /// have a floating panel mounted during a window switch.
    fn clear_panel_scoped_mode_on_switch_away(&mut self, previous_id: WindowId) {
        if self.floating_widget_panel.is_some() {
            if let Some(win) = self.windows.get_mut(&previous_id) {
                win.editor_mode = None;
            }
        }
    }

    /// Change only the active-window pointer, retiring every mouse capture or
    /// drag before its owning window changes.
    pub(crate) fn switch_active_window_pointer(&mut self, id: WindowId) {
        if self.active_window == id {
            return;
        }
        self.cancel_active_mouse_gesture();
        self.active_window = id;
    }

    /// Switch the active window to `id`.
    /// Pointer write: every per-window field
    /// (panel_ids / file_mod_times / file_explorer / lsp / splits)
    /// already lives on `Window`, so flipping `active_window` is the
    /// whole switch. Diving into a never-activated window seeds it
    /// with a fresh empty buffer + SplitManager so the renderer
    /// finds a populated `splits` field.
    ///
    /// No-op when `id` is already active. Logs and returns when
    /// `id` is unknown — the design treats unknown ids as a plugin
    /// bug (caller verifies with `listWindows`), not a recoverable
    /// error worth surfacing through the channel.
    pub fn set_active_window(&mut self, id: WindowId) {
        if self.active_window == id {
            return;
        }
        if !self.windows.contains_key(&id) {
            tracing::warn!("set_active_window: unknown window id {id}; active window unchanged");
            return;
        }

        let previous_id = self.active_window;

        // Capture the outgoing window's immutable persistence generation before
        // leaving it. Publication is serialized/coalesced on a blocking worker,
        // so terminal and workspace fsyncs never delay the focus change. (No-op
        // for an unmaterialized seed or a window with no splits — see
        // `checkpoint_window_workspace`.)
        self.checkpoint_window_workspace(previous_id);

        // Capture the outgoing backend label so we can tell, after the
        // switch, whether the active *authority* actually changed (most
        // window switches are between same-authority local sessions, where
        // it doesn't). Only then do we re-point editor-wide caches + fire
        // the `authority_changed` hook.
        let previous_authority_label = self.authority().display_label.clone();

        // Clear any panel-scoped editor mode on the window we're leaving so
        // it can never outlive the switch (see
        // `clear_panel_scoped_mode_on_switch_away`).
        self.clear_panel_scoped_mode_on_switch_away(previous_id);

        // Lazy materialization: if this window's saved workspace hasn't
        // been restored yet, restore it now (before seeding) so the
        // dive lands on real content rather than an empty buffer.
        self.materialize_window(id);

        // For a never-activated incoming window, allocate a fresh
        // seed buffer + SplitManager rooted at it. The state is
        // installed into the incoming window's `buffers` map after
        // the active pointer moves. After a successful materialize the
        // window already has splits, so this is a no-op.
        let fresh_layout = self.build_fresh_layout_if_needed(id);

        // Pointer write — that's the whole switch. `working_dir()` derives
        // from the active window's root. The pointer helper retires every
        // outgoing drag/capture before the owner changes.
        self.switch_active_window_pointer(id);
        // A multi-click sequence cannot cross a workspace switch. The
        // incoming window may still remember a click at this same screen cell
        // from its previous activation; treating the first click after a
        // round-trip as a double-click bypasses chrome routing (notably the
        // Orchestrator dock) and applies text selection to the buffer instead.
        if let Some(window) = self.windows.get_mut(&id) {
            window.previous_click_time = None;
            window.previous_click_position = None;
            window.previous_click_target = None;
            window.click_count = 0;
        }

        // For a never-activated incoming window, install the freshly
        // built layout into the window's `splits` field and attach
        // the seed buffer.
        if let Some((buf, state, metadata, event_log, mgr, vs)) = fresh_layout {
            if let Some(s) = self.windows.get_mut(&id) {
                s.buffers.set_splits((mgr, vs));
                s.buffers.insert(buf, state);
                s.buffer_metadata.insert(buf, metadata);
                s.event_logs.insert(buf, event_log);
            }
        }

        // Authority follows the active window. Each `Window` owns its
        // `resources.authority`; the editor-wide `self.authority` cache (read
        // by the 100+ filesystem/spawn/terminal call sites) must now reflect
        // the window we just switched to, or a per-session remote/cloud
        // backend would silently keep acting through the previous window's
        // authority. This is the switch-time counterpart to
        // `set_session_authority` (which mirrors on swap of the *active*
        // window) — see `AUTHORITY_DESIGN.md` §"Evolution: per-session
        // authority". Cheap for the common case: same-authority local windows
        // share `Arc`s and the label is unchanged, so the hook below is
        // skipped.
        self.adopt_active_window_authority(&previous_authority_label);

        // If we just switched to a remote session that came back from disk
        // dormant (backend spec known, live authority still the local
        // placeholder), start reconnecting its backend now — the per-window
        // activation the per-session design calls for. SSH/k8s reconnect from
        // core; the agent terminals re-run in the live backend once it lands.
        #[cfg(feature = "plugins")]
        self.reconnect_dormant_session_if_needed(id);

        // Refresh the plugin state snapshot so `getCwd()` (and every
        // other snapshot field) reflects the window we just switched
        // to *before* the `active_window_changed` hook runs. Without
        // this, plugins that read `editor.getCwd()` — Live Grep, file
        // finders, etc. — keep targeting the previous window's project
        // after a dive, surfacing the wrong project's files.
        #[cfg(feature = "plugins")]
        self.update_plugin_state_snapshot();

        self.run_plugin_hook_for_window(
            id,
            "active_window_changed",
            HookArgs::ActiveWindowChanged {
                previous_id: Some(previous_id.0),
                active_id: id.0,
            },
        );

        // Bring `terminal_mode` in line with the incoming window's active
        // buffer, exactly as the tab-switch path (`set_active_buffer`) does.
        // A window whose active buffer is a *restored* terminal comes back
        // with that buffer marked `Live` (see
        // `restore_terminals_from_workspace`) but its window-level
        // `terminal_mode` flag defaulted to `false` and the buffer left
        // read-only — the window switch never touched either. Without this
        // sync the first dive into such a session after an editor restart
        // lands on the read-only scrollback view instead of the live
        // terminal, and the user has to type (or wait for new output) to
        // wake it. Diving is a focus change just like a tab switch, so it
        // must route through the same single mode authority. A terminal the
        // user had explicitly dropped to Scrollback stays read-only (its
        // remembered mode isn't `Live`), so this only revives genuinely-live
        // terminals.
        self.sync_terminal_mode_to_active_buffer();

        // Reflow the newly-active window's visible terminal PTYs to
        // match their dive-view split rects. Without this, a session
        // that was just previewed in the orchestrator picker
        // (`render_session_preview_into_rect` resizes PTYs to the
        // embed rect — typically ~half the terminal's height) keeps
        // drawing at that smaller size after the dive, leaving the
        // bottom of the dive view blank until something else triggers
        // a resize. Same applies for the inverse: dive away while a
        // session has a small split, dive back when the window is
        // bigger — the terminal needs the new dimensions. Route through
        // the funnel so the dive-target window also picks up the current
        // editor-global dock width (its `dock_cols` cache may be stale).
        self.relayout();
    }

    /// Switch the active window and play a directional wipe over the
    /// editor content as the incoming window appears. The editor
    /// content geometry is layout-driven (identical for any session),
    /// so the outgoing window's last content rect is the right area to
    /// animate: `SlideIn` pushes the previous frame out as the new
    /// content slides in over it.
    ///
    /// The "before" comes from `last_rendered_frame`, the editor-wide
    /// clone of the last painted frame, so it is the workspace the user
    /// is actually looking at. It cannot come from the incoming window's
    /// own animation runner: runners are per-window and only the active
    /// window paints, so that one still holds whatever this window drew
    /// the last time it was on screen.
    ///
    /// Starting the effect here is only sound because every path that
    /// reaches this function runs between frames — the render path
    /// dispatches plugin commands only in its pre-layout drain, before
    /// anything has been painted for the outgoing window.
    pub fn set_active_window_animated(&mut self, id: WindowId, from_edge: &str) {
        let animate = self.active_window != id
            && self.windows.contains_key(&id)
            && self.config().editor.animations;
        // Wipe the ENTIRE window — menu bar, explorer, tabs, splits, and
        // status bar — i.e. everything to the right of the dock. That's
        // the chrome area from the dock split, not just the buffer's
        // content rect. The dock column itself stays put.
        let full = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: self.terminal_width,
            height: self.terminal_height,
        };
        let (_dock, area) = self.compute_dock_split(full);
        self.set_active_window(id);
        if !animate {
            return;
        }
        if area.width == 0 || area.height == 0 {
            return;
        }
        use crate::view::animation::{AnimationKind, Edge};
        let from = match from_edge {
            "top" => Edge::Top,
            "bottom" => Edge::Bottom,
            "left" => Edge::Left,
            "right" => Edge::Right,
            _ => Edge::Bottom,
        };
        self.active_window_mut().animations.start(
            area,
            AnimationKind::SlideIn {
                from,
                duration: std::time::Duration::from_millis(180),
                delay: std::time::Duration::ZERO,
            },
        );
    }

    /// Cycle to the next open window in the workspace.
    ///
    /// Windows are ordered by their numeric `WindowId` (which is
    /// monotonically assigned by `create_window_at`), so "next"
    /// reads in creation order with wrap-around. No-op when only
    /// one window is open (issue #2031).
    pub fn next_window(&mut self) {
        self.cycle_active_window(1);
    }

    /// Cycle to the previous open window. See [`Self::next_window`]
    /// for ordering.
    pub fn prev_window(&mut self) {
        self.cycle_active_window(-1);
    }

    /// Step `delta` positions through the open windows (positive =
    /// forward, negative = backward), wrapping around at the ends.
    /// Centralises the cycle logic shared by `next_window` and
    /// `prev_window` so both directions stay in sync if the
    /// underlying ordering changes (e.g. user-controlled reorder).
    fn cycle_active_window(&mut self, delta: isize) {
        // A plugin (the orchestrator dock) may constrain cycling to a
        // specific ordered subset — the windows currently visible in its
        // session list — so Next/Prev Window walks exactly that list rather
        // than every open window. Ids no longer open are dropped, preserving
        // the given order. An empty result (or no override) falls back to the
        // default: every window, ordered by id.
        let override_ids: Option<Vec<WindowId>> = self
            .window_cycle_order
            .as_ref()
            .map(|order| {
                order
                    .iter()
                    .copied()
                    .filter(|id| self.windows.contains_key(id))
                    .collect::<Vec<_>>()
            })
            .filter(|kept| !kept.is_empty());
        let ids: Vec<WindowId> = match override_ids {
            Some(kept) => kept,
            None => {
                let mut all: Vec<WindowId> = self.windows.keys().copied().collect();
                all.sort_by_key(|id| id.0);
                all
            }
        };
        if ids.len() <= 1 {
            return;
        }
        let current_pos = match ids.iter().position(|id| *id == self.active_window) {
            Some(pos) => pos as isize,
            None => 0,
        };
        let len = ids.len() as isize;
        let next_pos = (((current_pos + delta) % len) + len) % len;
        let next_id = ids[next_pos as usize];
        self.set_active_window(next_id);
    }

    /// Build a fresh seed buffer + split layout for `id` if that
    /// window is missing either a split tree or any buffer to back
    /// it. Returns `None` when the window is unknown or already
    /// populated. The caller is responsible for installing the
    /// returned tuple into the window's fields.
    ///
    /// Both branches (no splits, or splits but empty buffer map)
    /// are pathological: render walks the active buffer and would
    /// panic at `expect("active buffer must be present")` when the
    /// split manager points at a buffer id that isn't in
    /// `window.buffers`.
    ///
    /// Factored out of `set_active_window` so other call sites that
    /// need to populate an inert window shell can share the same
    /// seed-construction logic.
    pub(crate) fn build_fresh_layout_if_needed(&mut self, id: WindowId) -> Option<FreshLayoutSeed> {
        if !self
            .windows
            .get(&id)
            .is_some_and(|s| s.buffers.splits().is_none() || s.buffers.is_empty())
        {
            return None;
        }
        let buf = self.alloc_buffer_id();
        let mut state = crate::state::EditorState::new(
            self.terminal_width,
            self.terminal_height,
            self.config.editor.large_file_threshold_bytes as usize,
            std::sync::Arc::clone(&self.authority().filesystem),
        );
        state
            .margins
            .configure_for_line_numbers(self.config.editor.line_numbers);
        state
            .buffer
            .set_default_line_ending(self.config.editor.default_line_ending.to_line_ending());
        let metadata = crate::app::types::BufferMetadata::new();
        let event_log = crate::model::event::EventLog::new();
        let manager = SplitManager::new(buf);
        let active_leaf = manager.active_split();
        let mut view_states = HashMap::new();
        view_states.insert(
            active_leaf,
            SplitViewState::with_buffer(self.terminal_width, self.terminal_height, buf),
        );
        Some((buf, state, metadata, event_log, manager, view_states))
    }

    /// Eagerly initialise an inactive session's per-session
    /// state without diving. Useful for plugins (Orchestrator) that
    /// want to pay the warm-up cost (file-tree walk, ignore
    /// matcher, etc.) ahead of the user's first dive.
    ///
    /// In the current build this is a placeholder — file
    /// explorer rebuilds and LSP boot still happen on first dive.
    /// The API exists so callers don't have to be rewritten when
    /// eager warm-up wires up later.
    pub fn prewarm_window(&mut self, id: WindowId) {
        if id == self.active_window {
            return;
        }
        if !self.windows.contains_key(&id) {
            tracing::warn!("prewarm_window: unknown session id {id}");
        }
        // Placeholder for eager warm-up of file_explorer / LSP.
    }

    /// Remove a buffer from whichever window holds it. Returns the
    /// removed `EditorState` if the buffer was found. Step 0c: each
    /// buffer lives in exactly one window, so this is at most one
    /// successful removal.
    pub(crate) fn detach_buffer_from_all_windows(
        &mut self,
        buffer_id: fresh_core::BufferId,
    ) -> Option<crate::state::EditorState> {
        for w in self.windows.values_mut() {
            if let Some(state) = w.buffers.remove(&buffer_id) {
                return Some(state);
            }
        }
        None
    }

    /// Move a tab into a **new local workspace co-tenanting the same project root**.
    ///
    /// The source authority is checked before creating a window or changing a
    /// split. A remote session cannot safely transfer its window-local backend
    /// and must stay intact. Local file tabs only mutate the source after the
    /// target window exists; terminal tabs additionally finish every durable
    /// artifact transfer before their source split or buffer membership changes.
    pub fn extract_tab_to_new_workspace(&mut self, buffer_id: fresh_core::BufferId) {
        use rust_i18n::t;

        let source = self.active_window;
        let Some((
            is_local,
            selected_terminal,
            terminal_has_script_access,
            companion_terminal,
            is_terminal,
            path,
            root,
            stable_id,
        )) = self.windows.get(&source).map(|window| {
            let selected_terminal = window
                .terminal_buffers
                .get(&buffer_id)
                .map(|binding| binding.terminal_id);
            (
                !window.authority_spec.is_remote()
                    && window
                        .authority()
                        .filesystem
                        .remote_connection_info()
                        .is_none(),
                selected_terminal,
                selected_terminal.is_some_and(|id| window.terminal_has_script_access(id)),
                selected_terminal.is_some_and(|id| window.terminal_companions.contains_key(&id)),
                window.is_terminal_buffer(buffer_id),
                window
                    .buffers
                    .get(&buffer_id)
                    .and_then(|state| state.buffer.file_path().map(|path| path.to_path_buf())),
                window.root.clone(),
                window.stable_id.clone(),
            )
        })
        else {
            return;
        };
        if !is_local {
            self.set_status_message("Cannot extract: source workspace is not local".to_string());
            return;
        }
        if selected_terminal.is_some_and(|terminal_id| {
            self.self_update_terminal
                == Some(fresh_core::WindowTerminalId::new(source, terminal_id))
        }) {
            self.set_status_message(t!("status.update_running").to_string());
            return;
        }
        if terminal_has_script_access {
            self.set_status_message(t!("workspace.extract_terminal_script_capable").to_string());
            return;
        }
        if companion_terminal {
            self.set_status_message(t!("workspace.extract_terminal_companion").to_string());
            return;
        }

        if is_terminal {
            self.extract_terminal_tab_to_new_workspace(source, buffer_id);
            return;
        }

        let Some(path) = path else {
            self.set_status_message(t!("workspace.extract_no_file_path").to_string());
            return;
        };

        let _root_lock = match crate::workspace::lock_workspace_root(&self.dir_context, &root) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!("failed to lock workspace for tab extraction: {error}");
                return;
            }
        };
        if let Err(error) = self.save_workspace_for(source) {
            tracing::warn!("failed to checkpoint source before tab extraction: {error}");
            return;
        }
        let source_before = match crate::workspace::Workspace::load_by_id_in(
            &self.dir_context,
            &root,
            &stable_id,
        ) {
            Ok(Some(workspace)) => workspace,
            Ok(None) => {
                tracing::warn!("tab extraction source checkpoint disappeared");
                return;
            }
            Err(error) => {
                tracing::warn!("failed to reload tab extraction checkpoint: {error}");
                return;
            }
        };
        let (source_splits, source_buffer_ids) = self
            .windows
            .get(&source)
            .map(|window| {
                (
                    window
                        .buffers
                        .splits()
                        .map(|(manager, states)| (manager.clone(), states.clone())),
                    window
                        .buffers
                        .ids()
                        .into_iter()
                        .collect::<std::collections::HashSet<_>>(),
                )
            })
            .expect("source exists through synchronous tab extraction");

        let target = self.create_co_tenant_window(root);
        self.discard_fresh_window_seed(target);
        // These in-memory operations cannot fail after their source/target
        // membership preconditions above have been established.
        self.retarget_leaves_off_buffer(source, buffer_id);
        self.move_buffer_membership_between_windows(buffer_id, source, target);
        let publication_error = if let Err(error) = self.save_workspace_for(target) {
            Some(format!("target publication failed: {error}"))
        } else if let Err(error) = self.save_workspace_for(source) {
            Some(format!("source publication failed: {error}"))
        } else {
            None
        };
        if let Some(error) = publication_error {
            self.move_buffer_membership_between_windows(buffer_id, target, source);
            if let Some(window) = self.windows.get_mut(&source) {
                for extra in window
                    .buffers
                    .ids()
                    .into_iter()
                    .filter(|id| !source_buffer_ids.contains(id))
                    .collect::<Vec<_>>()
                {
                    window.buffers.remove(&extra);
                    window.buffer_metadata.remove(&extra);
                    window.event_logs.remove(&extra);
                }
                if let Some(splits) = source_splits {
                    window.buffers.set_splits(splits);
                }
            }
            if let Err(restore_error) = source_before.save_in(&self.dir_context) {
                tracing::error!(
                    "failed to restore source snapshot after tab extraction error: {restore_error}"
                );
            }
            self.discard_failed_extraction_window(target);
            tracing::error!("tab extraction rolled back: {error}");
            return;
        }

        let target_label = self
            .windows
            .get(&target)
            .map(|w| w.label.clone())
            .unwrap_or_default();
        self.set_active_window(target);
        self.set_active_buffer(buffer_id);

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        self.set_status_message(
            t!("workspace.extracted_tab", name = name, label = target_label).to_string(),
        );
    }

    /// Terminal-tab body of [`Self::extract_tab_to_new_workspace`]: move the
    /// live terminal — PTY handle, backing/log files, launch/resume argv, and
    /// process-group registration — into a new workspace co-tenanting the same
    /// project root. The running process is untouched; its output threads are
    /// retagged so the stream follows it (`TerminalManager::adopt`). The
    /// shell's own cwd is irrelevant to the workspace root now — the co-tenant
    /// is rooted at the source project, same as the file path.
    fn extract_terminal_tab_to_new_workspace(
        &mut self,
        source: WindowId,
        buffer_id: fresh_core::BufferId,
    ) {
        use rust_i18n::t;

        let Some((terminal_id, terminal_is_live, root, stable_id, name)) =
            self.windows.get(&source).and_then(|window| {
                let terminal_id = window.terminal_buffers.get(&buffer_id)?.terminal_id;
                let name = window
                    .buffer_metadata
                    .get(&buffer_id)
                    .map(|metadata| metadata.display_name.clone())
                    .or_else(|| {
                        window
                            .buffers
                            .get(&buffer_id)
                            .and_then(|state| state.buffer.file_path())
                            .and_then(|path| path.file_name())
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| "[No Name]".to_string());
                Some((
                    terminal_id,
                    window.terminal_manager.get(terminal_id).is_some(),
                    window.root.clone(),
                    window.stable_id.clone(),
                    name,
                ))
            })
        else {
            return;
        };
        if !terminal_is_live {
            self.set_status_message(t!("workspace.extract_terminal_dormant").to_string());
            return;
        }

        let _root_lock = match crate::workspace::lock_workspace_root(&self.dir_context, &root) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!("failed to lock workspace for terminal extraction: {error}");
                return;
            }
        };

        if let Err(error) = self.save_workspace_for(source) {
            tracing::warn!("failed to checkpoint source before terminal extraction: {error}");
            return;
        }
        let source_before = match crate::workspace::Workspace::load_by_id_in(
            &self.dir_context,
            &root,
            &stable_id,
        ) {
            Ok(Some(workspace)) => workspace,
            Ok(None) => {
                tracing::warn!("terminal extraction source checkpoint disappeared");
                return;
            }
            Err(error) => {
                tracing::warn!("failed to reload terminal extraction checkpoint: {error}");
                return;
            }
        };
        let (source_splits, source_buffer_ids) = self
            .windows
            .get(&source)
            .map(|window| {
                (
                    window
                        .buffers
                        .splits()
                        .map(|(manager, states)| (manager.clone(), states.clone())),
                    window
                        .buffers
                        .ids()
                        .into_iter()
                        .collect::<std::collections::HashSet<_>>(),
                )
            })
            .expect("source exists through synchronous terminal extraction");

        let target = self.create_co_tenant_window(root.clone());
        self.discard_fresh_window_seed(target);
        let Some((intent_path, mut intent, target_terminal_id)) = self
            .move_terminal_machinery_to_window(
                source,
                buffer_id,
                terminal_id,
                target,
                source_before.clone(),
            )
        else {
            self.discard_failed_extraction_window(target);
            tracing::warn!("terminal exited or its artifacts could not be moved during extraction");
            return;
        };

        self.retarget_leaves_off_buffer(source, buffer_id);
        self.move_buffer_membership_between_windows(buffer_id, source, target);

        let source_after = self
            .windows
            .get(&source)
            .expect("source exists after terminal extraction")
            .capture_workspace();
        let mut target_after = self
            .windows
            .get(&target)
            .expect("target exists after terminal extraction")
            .capture_workspace();
        for target_terminal in &mut target_after.terminals {
            let Some(source_terminal) = source_before.terminals.iter().find(|source_terminal| {
                source_terminal.history_path.as_ref().and_then(|path| {
                    intent
                        .artifacts
                        .iter()
                        .find(|move_| &move_.source == path)
                        .map(|move_| &move_.destination)
                }) == target_terminal.history_path.as_ref()
            }) else {
                continue;
            };
            target_terminal.backing_history_end = source_terminal.backing_history_end;
            target_terminal.checkpoint_generation = source_terminal.checkpoint_generation.clone();
            if source_terminal.checkpoint_generation.is_some() {
                target_terminal.backing_path = intent
                    .artifacts
                    .iter()
                    .find(|move_| move_.source == source_terminal.backing_path)
                    .expect("serialized terminal checkpoint is part of the extraction journal")
                    .destination
                    .clone();
            }
        }

        // The durable source snapshot must stop advertising the old transcript
        // paths before their lock pathnames and immutable checkpoint move.
        // Until this save lands, a second process still finds an exact source
        // workspace and remains fenced by the live writer's source locks.
        let mut commit_error = source_after
            .save_in(&self.dir_context)
            .err()
            .map(|error| format!("source cutover failed: {error}"));
        if commit_error.is_none()
            && !finish_terminal_artifacts(
                intent
                    .artifacts
                    .iter()
                    .filter(|move_| move_.after_source_cutover),
            )
        {
            commit_error = Some("terminal artifact cutover failed".to_string());
        }
        if commit_error.is_none() {
            intent.phase = crate::workspace::TerminalExtractionPhase::Committed {
                source_after: source_after.clone(),
                target_after: target_after.clone(),
            };
            commit_error =
                crate::workspace::persist_terminal_extraction_intent(&self.dir_context, &intent)
                    .err()
                    .map(|error| format!("journal commit failed: {error}"));
        }
        if let Some(error) = commit_error.as_ref() {
            tracing::error!("failed to commit terminal extraction: {error}");
            if self.rollback_terminal_extraction_live(
                source,
                target,
                buffer_id,
                target_terminal_id,
                &intent,
                source_splits,
                source_buffer_ids,
            ) {
                let restored = source_before.save_in(&self.dir_context);
                self.discard_failed_extraction_window(target);
                let cleared = crate::workspace::clear_terminal_extraction_intent(&intent_path);
                if let Err(restore_error) = restored {
                    tracing::error!(
                        "failed to republish source after terminal extraction rollback: {restore_error}"
                    );
                }
                if let Err(clear_error) = cleared {
                    tracing::error!(
                        "failed to clear reversed terminal extraction journal: {clear_error}"
                    );
                }
                self.set_status_message(
                    "Terminal extraction could not be committed and was rolled back".to_string(),
                );
                return;
            }
        }

        let transaction_complete = if commit_error.is_some() {
            false
        } else {
            match target_after.save_in(&self.dir_context) {
                Err(error) => {
                    tracing::error!("terminal extraction target snapshot awaits recovery: {error}");
                    false
                }
                Ok(()) => {
                    if let Err(error) =
                        crate::workspace::clear_terminal_extraction_intent(&intent_path)
                    {
                        tracing::warn!("terminal extraction journal cleanup deferred: {error}");
                    }
                    true
                }
            }
        };

        let target_label = self
            .windows
            .get(&target)
            .map(|w| w.label.clone())
            .unwrap_or_default();
        self.set_active_window(target);
        self.set_active_buffer(buffer_id);
        self.sync_terminal_mode_to_active_buffer();
        self.active_window_mut().resize_visible_terminals();

        if transaction_complete {
            self.set_status_message(
                t!("workspace.extracted_tab", name = name, label = target_label).to_string(),
            );
        } else {
            self.set_status_message(
                "Terminal extraction is pending durable recovery; do not close either session"
                    .to_string(),
            );
        }
    }

    /// Move every piece of per-terminal state from the named source window to
    /// `target`: the adopted PTY, checkpoint, append-only history, raw log,
    /// launch/resume argv, ephemeral flag, title/fg-name caches, and process
    /// group registration.
    fn move_terminal_machinery_to_window(
        &mut self,
        source: WindowId,
        buffer_id: fresh_core::BufferId,
        terminal_id: crate::services::terminal::TerminalId,
        target: WindowId,
        source_before: crate::workspace::Workspace,
    ) -> Option<(
        PathBuf,
        crate::workspace::TerminalExtractionIntent,
        crate::services::terminal::TerminalId,
    )> {
        if source == target {
            return None;
        }

        let target_window = self.windows.get(&target)?;
        let target_artifacts = target_window.terminal_artifacts_dir();
        let target_bridge = target_window.bridge.clone();
        let target_root = target_window.root.clone();
        let target_stable_id = target_window.stable_id.clone();
        let src = self.windows.get(&source)?;
        if src
            .terminal_buffers
            .get(&buffer_id)
            .is_none_or(|binding| binding.terminal_id != terminal_id)
        {
            return None;
        }
        let backing = src.terminal_backing_files.get(&terminal_id).cloned();
        let history = src.terminal_history_files.get(&terminal_id).cloned();
        let log = src.terminal_log_files.get(&terminal_id).cloned();
        let serialized_terminal = source_before
            .terminals
            .iter()
            .find(|terminal| terminal.history_path.as_ref() == history.as_ref())?;
        let checkpoint = serialized_terminal
            .checkpoint_generation
            .as_ref()
            .map(|_| serialized_terminal.backing_path.clone())
            .filter(|path| backing.as_ref() != Some(path));

        let mut artifacts = Vec::with_capacity(8);
        for source_path in [
            backing.as_ref(),
            history.as_ref(),
            log.as_ref(),
            checkpoint.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !source_path.exists() {
                tracing::warn!(
                    "terminal artifact disappeared before extraction: {}",
                    source_path.display()
                );
                return None;
            }
            let original_name = source_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("terminal");
            let stem = source_path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("terminal");
            let extension = source_path.extension().and_then(|value| value.to_str());
            let destination = (0u32..)
                .map(|suffix| {
                    if suffix == 0 {
                        target_artifacts.join(original_name)
                    } else {
                        let name = match extension {
                            Some(extension) => format!("{stem}-moved-{suffix}.{extension}"),
                            None => format!("{stem}-moved-{suffix}"),
                        };
                        target_artifacts.join(name)
                    }
                })
                .find(|candidate| {
                    !candidate.exists()
                        && !crate::services::terminal::manager::terminal_artifact_lock_path(
                            candidate,
                        )
                        .exists()
                })
                .expect("terminal artifact suffix space is non-empty");
            let source_lock =
                crate::services::terminal::manager::terminal_artifact_lock_path(source_path);
            if source_lock.exists() {
                artifacts.push(crate::workspace::TerminalArtifactRelocation {
                    source: source_lock,
                    destination: crate::services::terminal::manager::terminal_artifact_lock_path(
                        &destination,
                    ),
                    after_source_cutover: true,
                });
            }
            let after_source_cutover = serialized_terminal.log_path == *source_path
                || serialized_terminal.history_path.as_ref() == Some(source_path)
                || (serialized_terminal.checkpoint_generation.is_some()
                    && serialized_terminal.backing_path == *source_path);
            artifacts.push(crate::workspace::TerminalArtifactRelocation {
                source: source_path.clone(),
                destination,
                after_source_cutover,
            });
        }

        let intent = crate::workspace::TerminalExtractionIntent::prepared(
            source_before,
            target_root,
            target_stable_id,
            artifacts,
        );
        let intent_path = match crate::workspace::persist_terminal_extraction_intent(
            &self.dir_context,
            &intent,
        ) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!("failed to prepare terminal extraction journal: {error}");
                return None;
            }
        };
        if let Err(error) = std::fs::create_dir_all(&target_artifacts) {
            tracing::warn!("failed to create target terminal artifact directory: {error}");
            #[allow(clippy::let_underscore_must_use)]
            let _ = crate::workspace::clear_terminal_extraction_intent(&intent_path);
            return None;
        }
        let Some(handle) = self
            .windows
            .get_mut(&source)
            .and_then(|src| src.terminal_manager.release(terminal_id))
        else {
            #[allow(clippy::let_underscore_must_use)]
            let _ = crate::workspace::clear_terminal_extraction_intent(&intent_path);
            return None;
        };
        let pid = handle.pid();
        if !finish_terminal_artifacts(
            intent
                .artifacts
                .iter()
                .filter(|move_| !move_.after_source_cutover),
        ) {
            let rollback_complete = rollback_terminal_artifacts(&intent.artifacts);
            if rollback_complete {
                #[allow(clippy::let_underscore_must_use)]
                let _ = crate::workspace::clear_terminal_extraction_intent(&intent_path);
            }
            if let Some(src) = self.windows.get_mut(&source) {
                if src
                    .terminal_manager
                    .restore_released(terminal_id, handle)
                    .is_err()
                {
                    tracing::error!("failed to restore terminal after artifact move failure");
                }
            }
            return None;
        }

        let adoption = {
            let tgt = self
                .windows
                .get_mut(&target)
                .expect("target window exists through synchronous terminal move");
            tgt.terminal_manager.set_async_bridge(target_bridge);
            tgt.terminal_manager.adopt(handle)
        };
        let new_id = match adoption {
            Ok(new_id) => new_id,
            Err(handle) => {
                let rollback_complete = rollback_terminal_artifacts(&intent.artifacts);
                if rollback_complete {
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = crate::workspace::clear_terminal_extraction_intent(&intent_path);
                }
                if let Some(src) = self.windows.get_mut(&source) {
                    if src
                        .terminal_manager
                        .restore_released(terminal_id, handle)
                        .is_err()
                    {
                        tracing::error!("failed to restore terminal after adoption lost exit race");
                    }
                }
                return None;
            }
        };

        let relocated = |path: Option<std::path::PathBuf>| {
            path.map(|path| {
                intent
                    .artifacts
                    .iter()
                    .find_map(|move_| (move_.source == path).then(|| move_.destination.clone()))
                    .expect("every terminal artifact was moved")
            })
        };
        let backing = relocated(backing);
        let history = relocated(history);
        let log = relocated(log);

        let src = self
            .windows
            .get_mut(&source)
            .expect("source window exists through synchronous terminal move");
        src.terminal_buffers.remove(&buffer_id);
        src.terminal_backing_files.remove(&terminal_id);
        src.terminal_history_files.remove(&terminal_id);
        src.terminal_log_files.remove(&terminal_id);
        let command = src.terminal_commands.remove(&terminal_id);
        let resume = src.terminal_resume_commands.remove(&terminal_id);
        let ephemeral = src.ephemeral_terminals.remove(&terminal_id);
        let tracked_agent = src.tracked_agent_terminal == Some(terminal_id);
        if tracked_agent {
            src.tracked_agent_terminal = None;
        }
        let explicit_title = src.terminal_explicit_titles.remove(&buffer_id);
        let fg_name = src.terminal_fg_cache.remove(&buffer_id);
        if let Some(pid) = pid {
            src.process_groups.forget(pid);
        }

        let tgt = self
            .windows
            .get_mut(&target)
            .expect("target window exists through synchronous terminal move");
        tgt.terminal_buffers.insert(
            buffer_id,
            crate::app::window::TerminalBuffer::new_live(new_id),
        );
        if let Some(path) = backing {
            tgt.terminal_backing_files.insert(new_id, path);
        }
        if let Some(path) = history {
            tgt.terminal_history_files.insert(new_id, path);
        }
        if let Some(path) = log {
            tgt.terminal_log_files.insert(new_id, path);
        }
        if let Some(command) = command {
            tgt.terminal_commands.insert(new_id, command);
        }
        if let Some(resume) = resume {
            tgt.terminal_resume_commands.insert(new_id, resume);
        }
        if ephemeral {
            tgt.ephemeral_terminals.insert(new_id);
        }
        if tracked_agent {
            tgt.tracked_agent_terminal = Some(new_id);
        }
        if explicit_title {
            tgt.terminal_explicit_titles.insert(buffer_id);
        }
        if let Some(name) = fg_name {
            tgt.terminal_fg_cache.insert(buffer_id, name);
        }
        if let Some(pid) = pid {
            tgt.process_groups
                .register(pid, format!("terminal #{}", new_id.0));
        }
        Some((intent_path, intent, new_id))
    }

    /// Reverse the in-memory half of a terminal extraction while its prepared
    /// journal still names the authoritative source. Artifact renames happen
    /// first; only a complete reversal retargets the live PTY and buffer home.
    fn rollback_terminal_extraction_live(
        &mut self,
        source: WindowId,
        target: WindowId,
        buffer_id: fresh_core::BufferId,
        target_terminal_id: crate::services::terminal::TerminalId,
        intent: &crate::workspace::TerminalExtractionIntent,
        source_splits: Option<(
            SplitManager,
            HashMap<crate::model::event::LeafId, SplitViewState>,
        )>,
        source_buffer_ids: std::collections::HashSet<fresh_core::BufferId>,
    ) -> bool {
        if !rollback_terminal_artifacts(&intent.artifacts) {
            return false;
        }
        let Some(handle) = self
            .windows
            .get_mut(&target)
            .and_then(|window| window.terminal_manager.release(target_terminal_id))
        else {
            let _ = finish_terminal_artifacts(&intent.artifacts);
            return false;
        };
        let pid = handle.pid();
        let source_terminal_id = match self
            .windows
            .get_mut(&source)
            .expect("source exists through synchronous terminal rollback")
            .terminal_manager
            .adopt(handle)
        {
            Ok(terminal_id) => terminal_id,
            Err(handle) => {
                let _ = finish_terminal_artifacts(&intent.artifacts);
                if self
                    .windows
                    .get_mut(&target)
                    .expect("target exists through synchronous terminal rollback")
                    .terminal_manager
                    .restore_released(target_terminal_id, handle)
                    .is_err()
                {
                    tracing::error!("failed to retain terminal after extraction rollback race");
                }
                return false;
            }
        };

        let original_path = |path: Option<PathBuf>| {
            path.map(|path| {
                intent
                    .artifacts
                    .iter()
                    .find_map(|move_| (move_.destination == path).then(|| move_.source.clone()))
                    .expect("every terminal artifact rollback is journaled")
            })
        };
        let (
            backing,
            history,
            log,
            command,
            resume,
            ephemeral,
            explicit_title,
            foreground_name,
            tracked_agent,
        ) = {
            let target_window = self
                .windows
                .get_mut(&target)
                .expect("target exists through synchronous terminal rollback");
            target_window.terminal_buffers.remove(&buffer_id);
            let backing = original_path(
                target_window
                    .terminal_backing_files
                    .remove(&target_terminal_id),
            );
            let history = original_path(
                target_window
                    .terminal_history_files
                    .remove(&target_terminal_id),
            );
            let log = original_path(target_window.terminal_log_files.remove(&target_terminal_id));
            let command = target_window.terminal_commands.remove(&target_terminal_id);
            let resume = target_window
                .terminal_resume_commands
                .remove(&target_terminal_id);
            let ephemeral = target_window
                .ephemeral_terminals
                .remove(&target_terminal_id);
            let explicit_title = target_window.terminal_explicit_titles.remove(&buffer_id);
            let foreground_name = target_window.terminal_fg_cache.remove(&buffer_id);
            let tracked_agent = target_window.tracked_agent_terminal == Some(target_terminal_id);
            if tracked_agent {
                target_window.tracked_agent_terminal = None;
            }
            if let Some(pid) = pid {
                target_window.process_groups.forget(pid);
            }
            (
                backing,
                history,
                log,
                command,
                resume,
                ephemeral,
                explicit_title,
                foreground_name,
                tracked_agent,
            )
        };

        let source_window = self
            .windows
            .get_mut(&source)
            .expect("source exists through synchronous terminal rollback");
        source_window.terminal_buffers.insert(
            buffer_id,
            crate::app::window::TerminalBuffer::new_live(source_terminal_id),
        );
        if let Some(path) = backing {
            source_window
                .terminal_backing_files
                .insert(source_terminal_id, path);
        }
        if let Some(path) = history {
            source_window
                .terminal_history_files
                .insert(source_terminal_id, path);
        }
        if let Some(path) = log {
            source_window
                .terminal_log_files
                .insert(source_terminal_id, path);
        }
        if let Some(command) = command {
            source_window
                .terminal_commands
                .insert(source_terminal_id, command);
        }
        if let Some(resume) = resume {
            source_window
                .terminal_resume_commands
                .insert(source_terminal_id, resume);
        }
        if ephemeral {
            source_window.ephemeral_terminals.insert(source_terminal_id);
        }
        if explicit_title {
            source_window.terminal_explicit_titles.insert(buffer_id);
        }
        if let Some(name) = foreground_name {
            source_window.terminal_fg_cache.insert(buffer_id, name);
        }
        if tracked_agent {
            source_window.tracked_agent_terminal = Some(source_terminal_id);
        }
        if let Some(pid) = pid {
            source_window
                .process_groups
                .register(pid, format!("terminal #{}", source_terminal_id.0));
        }

        self.move_buffer_membership_between_windows(buffer_id, target, source);
        if let Some(source_window) = self.windows.get_mut(&source) {
            for extra in source_window
                .buffers
                .ids()
                .into_iter()
                .filter(|id| !source_buffer_ids.contains(id))
                .collect::<Vec<_>>()
            {
                source_window.buffers.remove(&extra);
                source_window.buffer_metadata.remove(&extra);
                source_window.event_logs.remove(&extra);
            }
            if let Some(splits) = source_splits {
                source_window.buffers.set_splits(splits);
            }
        }
        true
    }

    /// Re-home a buffer from the active window to `target`.
    pub(crate) fn move_buffer_membership_to_window(
        &mut self,
        buffer_id: fresh_core::BufferId,
        target: WindowId,
    ) {
        self.move_buffer_membership_between_windows(buffer_id, self.active_window, target);
    }

    /// Re-home `buffer_id` between the two named windows. Extraction captures
    /// `source` before it creates or activates a co-tenant, so it can never
    /// accidentally drain whichever window became active later.
    fn move_buffer_membership_between_windows(
        &mut self,
        buffer_id: fresh_core::BufferId,
        source: WindowId,
        target: WindowId,
    ) {
        if source == target {
            return;
        }
        if !self.windows.contains_key(&target) {
            return;
        }
        let Some(source_window) = self.windows.get_mut(&source) else {
            return;
        };
        let Some(state) = source_window.buffers.remove(&buffer_id) else {
            return;
        };
        let metadata = source_window.buffer_metadata.remove(&buffer_id);
        let event_log = source_window.event_logs.remove(&buffer_id);

        let leaf_ids: Vec<_> = source_window
            .buffers
            .splits()
            .map(|(_, view_states)| view_states.keys().copied().collect())
            .unwrap_or_default();
        for leaf_id in leaf_ids {
            if let Some(view_state) = source_window
                .split_view_states_mut()
                .and_then(|view_states| view_states.get_mut(&leaf_id))
            {
                view_state.remove_buffer(buffer_id);
            }
        }

        let target_window = self
            .windows
            .get_mut(&target)
            .expect("target was checked before synchronous buffer transfer");
        target_window.buffers.insert(buffer_id, state);
        if let Some(metadata) = metadata {
            target_window.buffer_metadata.insert(buffer_id, metadata);
        }
        if let Some(event_log) = event_log {
            target_window.event_logs.insert(buffer_id, event_log);
        }
        if let Some((manager, view_states)) = target_window.buffers.splits_mut() {
            let active_leaf = manager.active_split();
            if let Some(view_state) = view_states.get_mut(&active_leaf) {
                view_state.add_buffer(buffer_id);
            }
        } else {
            let manager = crate::view::split::SplitManager::new(buffer_id);
            let active_leaf = manager.active_split();
            let mut view_states = std::collections::HashMap::new();
            view_states.insert(
                active_leaf,
                crate::view::split::SplitViewState::with_buffer(
                    self.terminal_width,
                    self.terminal_height,
                    buffer_id,
                ),
            );
            target_window.buffers.set_splits((manager, view_states));
        }
    }

    /// Switch every leaf of `source` that displays `buffer_id` to another tab
    /// before the buffer's membership moves to a co-tenant.
    fn retarget_leaves_off_buffer(&mut self, source: WindowId, buffer_id: fresh_core::BufferId) {
        use crate::view::split::TabTarget;
        debug_assert_eq!(self.active_window, source);

        let Some((mgr, view_states)) = self.windows.get(&source).and_then(|w| w.buffers.splits())
        else {
            return;
        };

        // For every leaf that displays the extracted buffer, snapshot its
        // replacement candidates in focus-history (LRU) order — most-recently
        // focused first, then any other open tab — mirroring the real
        // close-tab path (`resolve_close_replacement`) rather than raw tab
        // order. The rect is a probe; only ids matter here. Owned Vecs so the
        // `self.windows` borrow is released before the mutating loop.
        let probe = ratatui::layout::Rect::new(0, 0, 1, 1);
        let showing: Vec<(crate::model::event::LeafId, Vec<fresh_core::BufferId>)> = mgr
            .root()
            .get_leaves_with_rects(probe)
            .into_iter()
            .filter(|(_, displayed, _)| *displayed == buffer_id)
            .map(|(leaf_id, _, _)| {
                let candidates = view_states
                    .get(&leaf_id)
                    .map(|vs| {
                        vs.focus_history
                            .iter()
                            .rev()
                            .chain(vs.open_buffers.iter())
                            .filter_map(|t| match t {
                                TabTarget::Buffer(id) if *id != buffer_id => Some(*id),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                (leaf_id, candidates)
            })
            .collect();

        for (leaf_id, candidates) in showing {
            // First candidate that still exists and isn't a hidden helper
            // buffer — the close path excludes `hidden_from_tabs` on both its
            // LRU and fallback branches, so a leaf is never re-pointed onto a
            // panel/helper buffer.
            let replacement = candidates.into_iter().find(|bid| {
                self.windows.get(&source).is_some_and(|window| {
                    window.buffers.contains_key(bid)
                        && !window
                            .buffer_metadata
                            .get(bid)
                            .map(|metadata| metadata.hidden_from_tabs)
                            .unwrap_or(false)
                })
            });
            if let Some(replacement) = replacement {
                if let Some(window) = self.windows.get_mut(&source) {
                    window.set_pane_buffer(leaf_id, replacement);
                }
                continue;
            }
            let leaf_count = self
                .windows
                .get(&source)
                .and_then(|w| w.buffers.splits())
                .map(|(mgr, _)| mgr.root().count_leaves())
                .unwrap_or(1);
            if leaf_count > 1 {
                self.handle_close_split(leaf_id.into());
            } else {
                // Last leaf with no other tab: seed a fresh scratch buffer so
                // the source window keeps a renderable tab after the move —
                // but honor the same opt-out the close path does. When the
                // user disabled auto-creating an empty buffer on last close,
                // mark the seed a hidden synthetic placeholder so the emptied
                // source window genuinely looks blank instead of forcing a
                // visible `[No Name]`.
                let new_id = self.new_buffer();
                if !self
                    .config
                    .editor
                    .auto_create_empty_buffer_on_last_buffer_close
                {
                    if let Some(meta) = self.active_window_mut().buffer_metadata.get_mut(&new_id) {
                        meta.hidden_from_tabs = true;
                        meta.synthetic_placeholder = true;
                    }
                }
            }
        }
    }

    /// Close a session and drop its `Session` entry. Refuses to
    /// close the currently active session — the caller must switch
    /// to a different session first. Refuses to close the *last*
    /// remaining window — the editor must always host at least one.
    ///
    /// There is no special "base" window any more: id 1 is just the
    /// window the editor launched into, closable like any other once
    /// another window exists. The real invariant is "≥1 window", not
    /// "id 1 lives forever".
    ///
    /// Returns `true` on success, `false` on rejection.
    pub fn close_window(&mut self, id: WindowId) -> bool {
        // A dormant remote session usually has no `Window` and no live
        // connection — closing it just drops its descriptor so it leaves the
        // dock. (Without a window it can never be active, and isn't the
        // "last window".) A dormant session that DOES have a window — the
        // disconnected shell a failed reconnect leaves behind — falls through
        // to the normal close path below (which honours the active/last-window
        // guards) and drops its descriptor together with the window.
        if self.dormant_remote.contains_key(&id) && !self.windows.contains_key(&id) {
            self.cancel_remote_reconnect(id);
            self.dormant_remote.remove(&id);
            self.plugin_manager
                .read()
                .unwrap()
                .run_hook_with_invocation(
                    "window_closed",
                    HookArgs::WindowClosed { id: id.0 },
                    Some(Self::closed_plugin_invocation(id)),
                );
            return true;
        }
        if self.windows.len() <= 1 {
            tracing::warn!("close_window: refusing to close the last remaining window (id {id})");
            return false;
        }
        if id == self.active_window {
            tracing::warn!(
                "close_window: refusing to close the active session (id {id}); \
                 switch first via setActiveWindow"
            );
            return false;
        }
        let closing_invocation = self
            .plugin_invocation(id)
            .unwrap_or_else(|| Self::closed_plugin_invocation(id));
        self.cancel_remote_reconnect(id);
        self.cancel_remote_attaches_for_window(id);
        #[cfg(feature = "plugins")]
        self.file_watcher_manager.unwatch_window(id);
        let Some(window) = self.windows.get(&id) else {
            tracing::warn!("close_window: unknown session id {id}");
            return false;
        };
        let remote_connection_id = window.authority().filesystem.remote_channel_id();
        let tracked_terminal_ids: std::collections::HashSet<_> = window
            .terminal_manager
            .tracked_terminal_ids()
            .into_iter()
            .collect();
        let closing_bridge = window.bridge.clone();
        let closing_root = window.root.clone();
        let closing_stable_id = window.stable_id.clone();
        let purge_terminal_ids: std::collections::HashSet<_> = tracked_terminal_ids
            .iter()
            .copied()
            .chain(window.terminal_companions.keys().copied())
            .collect();
        for terminal_id in purge_terminal_ids {
            self.purge_omp_companion_terminal(fresh_core::WindowTerminalId::new(id, terminal_id));
        }
        if let Some(window) = self.windows.get_mut(&id) {
            window.revoke_all_terminal_script_tokens();
        }
        // Remove the window immediately from user-visible state, but retain the
        // identities until their reaped exit events arrive. The exit barrier
        // bounds reader drain, so this cannot wait forever on a PTY descendant.
        let waits_for_terminal_exits = !tracked_terminal_ids.is_empty();
        if waits_for_terminal_exits {
            self.closing_windows.insert(
                id,
                crate::app::ClosingWindowExitBarrier {
                    terminal_ids: tracked_terminal_ids,
                    bridge: closing_bridge,
                    root: closing_root,
                    stable_id: closing_stable_id,
                    invocation: closing_invocation.clone(),
                },
            );
        }
        if self
            .self_update_terminal
            .is_some_and(|terminal| terminal.window == id)
        {
            self.finish_self_update(None);
            self.self_update_terminal = None;
        }
        self.windows.remove(&id);
        // Closing a dormant session's disconnected shell drops the whole
        // session: the descriptor must leave the dock with the window.
        self.dormant_remote.remove(&id);
        self.pending_remote_reattach.remove(&id);
        self.remote_connected_cache.remove(&id);
        if let Some(connection_id) = remote_connection_id {
            self.stop_remote_reconnect_forwarder(connection_id);
        }
        // Tear down a born-attached remote session's connection (carrier +
        // reconnect/heartbeat + runtime) when its window closes. No-op for
        // local windows, which never have an entry.
        if self.session_keepalives.remove(&id).is_some() {
            tracing::info!("close_window: dropped remote session keepalive for window {id}");
        }
        if !waits_for_terminal_exits {
            self.plugin_manager
                .read()
                .unwrap()
                .run_hook_with_invocation(
                    "window_closed",
                    HookArgs::WindowClosed { id: id.0 },
                    Some(closing_invocation),
                );
        }

        true
    }

    /// Born-attached remote session: create a **new window** whose authority is
    /// the already-connected remote backend (Kubernetes / SSH / …), seed its
    /// terminal *inside* that backend, and park the connection `keepalive`
    /// keyed by the window so it outlives editor rebuilds and is torn down on
    /// close.
    ///
    /// Unlike the global `install_authority_with_keepalive` restart, existing
    /// windows are left untouched — the remote session coexists with them, and
    /// `set_active_window` (Gap A) retargets the active authority when the user
    /// switches. The new window is born under `authority` because it is passed
    /// straight to `create_window_with_terminal` as that window's backend, so
    /// its filesystem, LSP spawner, and terminal wrapper all act in the backend
    /// from birth (there are no stale local handles to invalidate; the caveat
    /// for hot-swapping an existing window's authority does not apply here).
    /// Editor-wide authority caches adopt the new backend only when
    /// `activate` is true; background creation leaves the previously active
    /// window untouched.
    pub(crate) fn create_remote_session_window(
        &mut self,
        authority: crate::services::authority::Authority,
        keepalive: Box<dyn std::any::Any + Send>,
        root: PathBuf,
        label: String,
        command: Option<Vec<String>>,
        activate: bool,
        spec: crate::services::authority::SessionAuthoritySpec,
        initial_state: Option<(String, HashMap<String, serde_json::Value>)>,
    ) -> Result<WindowId, String> {
        match self.create_window_with_terminal(
            root.clone(),
            label,
            Some(root),
            command,
            None,
            None,
            authority,
            None,
            None,
            false,
            None,
            false,
            activate,
            initial_state,
        ) {
            Ok((window_id, _terminal, _buffer)) => {
                self.session_keepalives.insert(window_id, keepalive);
                // Persist how to reconnect this backend on the new session so
                // a restart / relaunch can bring it back rather than degrade
                // it to local.
                if let Some(w) = self.windows.get_mut(&window_id) {
                    w.authority_spec = spec;
                }
                Ok(window_id)
            }
            Err(e) => {
                // The connect succeeded but the window couldn't be seeded
                // (e.g. the backend has no python3 / the pod died):
                // `create_window_with_terminal` already rolled the active
                // pointer back to the previous window and left the
                // editor-wide authority untouched (it never installed the
                // remote one), so just drop the keepalive (tears down the
                // carrier).
                drop(keepalive);
                Err(e)
            }
        }
    }

    /// Begin bringing a **dormant remote** session online: connect its SSH/kube
    /// backend, then — on success — promote it to a real `Window`
    /// ([`Self::promote_dormant_remote`]). Used when the user dives into a
    /// session that boot discovered but never connected: it has no `Window` yet,
    /// only a `dormant_remote` descriptor (no authority). The active window is
    /// left unchanged until the connection lands, so the editor never shows a
    /// window without its real backend.
    pub(crate) fn bring_dormant_remote_online(&mut self, id: WindowId) {
        let Some(descriptor) = self.dormant_remote.get(&id) else {
            return;
        };
        // Only remote-agent sessions are ever placed in `dormant_remote`.
        let spec = match &descriptor.authority_spec {
            crate::services::authority::SessionAuthoritySpec::RemoteAgent(s) => s.clone(),
            _ => return,
        };
        if self.remote_reconnect_inflight(id) {
            return;
        }
        // A prior failed connect may have left a disconnected shell window
        // for this session — clear its recorded failure so the indicator
        // shows "Connecting" (not a stale error) while this retry runs.
        if let Some(window) = self.windows.get_mut(&id) {
            window.remote_reconnect_error = None;
        }
        // `start_remote_connect` emits `RemoteAttachReady` owned by this exact
        // reconnecting window; promotion cannot be confused with any plugin's
        // same-numbered callback request.
        #[cfg(feature = "plugins")]
        self.start_remote_connect(
            spec,
            crate::app::RemoteAttachOwner::Reconnect { window_id: id },
            true,
            None,
        );
        #[cfg(not(feature = "plugins"))]
        let _ = spec;
    }

    /// Promote a dormant remote session to a live `Window`, **born with the
    /// freshly-connected `authority`**. Its persisted workspace is restored
    /// through that authority, so its terminals spawn on the remote backend —
    /// never the local host. This is the *only* path that turns a
    /// `dormant_remote` descriptor into a `Window`; there is deliberately no way
    /// to build that window without the connected backend in hand, which is what
    /// makes "a restored remote terminal running locally" unrepresentable.
    pub(crate) fn promote_dormant_remote(
        &mut self,
        id: WindowId,
        authority: crate::services::authority::Authority,
        keepalive: Box<dyn std::any::Any + Send>,
        root: PathBuf,
        spec: crate::services::authority::SessionAuthoritySpec,
        restore_allowed: bool,
    ) {
        let Some(descriptor) = self.dormant_remote.get(&id).cloned() else {
            drop(authority);
            drop(keepalive);
            return;
        };
        let workspace = if restore_allowed {
            match descriptor
                .stable_id
                .as_deref()
                .filter(|stable_id| !stable_id.is_empty())
            {
                Some(stable_id) => crate::workspace::Workspace::load_by_id_in(
                    &self.dir_context,
                    &descriptor.root,
                    stable_id,
                )
                .map_err(|error| error.to_string())
                .and_then(|workspace| {
                    workspace
                        .map(Some)
                        .ok_or_else(|| format!("workspace {stable_id} was not found"))
                }),
                None => crate::workspace::Workspace::load_in(&self.dir_context, &descriptor.root)
                    .map_err(|error| error.to_string()),
            }
        } else {
            Ok(None)
        };
        let workspace = match workspace {
            Ok(workspace) => workspace,
            Err(reason) => {
                tracing::warn!(?id, "remote workspace restore failed: {reason}");
                if let Some(shell) = self.windows.get_mut(&id) {
                    shell.remote_reconnect_error = Some(reason.clone());
                    shell.set_status_message(format!("Connection failed: {reason}"));
                }
                drop(authority);
                drop(keepalive);
                return;
            }
        };

        // The descriptor remains authoritative until the exact snapshot has
        // loaded. A close or newer completion can therefore invalidate this
        // promotion without losing the durable reconnect information.
        if self.dormant_remote.remove(&id).is_none() {
            drop(authority);
            drop(keepalive);
            return;
        }

        let mut resources = self.window_resources();
        resources.fs_manager = std::sync::Arc::new(crate::services::fs::FsManager::new(
            std::sync::Arc::clone(&authority.filesystem),
        ));
        let mut window = match workspace {
            Some(workspace) => crate::app::window::Window::from_workspace(
                id,
                descriptor.label.clone(),
                root.clone(),
                authority,
                resources,
                &workspace,
            ),
            None => {
                let mut window = crate::app::window::Window::new(
                    id,
                    descriptor.label.clone(),
                    root,
                    authority,
                    resources,
                );
                window.seed_initial_layout();
                window
            }
        };
        if let Some(stable_id) = descriptor
            .stable_id
            .as_ref()
            .filter(|stable_id| !stable_id.is_empty())
        {
            window.stable_id.clone_from(stable_id);
        }
        window.terminal_width = self.terminal_width;
        window.terminal_height = self.terminal_height;
        if restore_allowed {
            window.plugin_state = descriptor.plugin_state;
        }
        window.authority_spec = spec;
        window.set_status_message(format!("Connected: {}", descriptor.label));

        let already_active = self.active_window == id;
        let previous_authority_label = already_active
            .then(|| self.authority().display_label.clone())
            .unwrap_or_default();
        self.windows.insert(id, window);
        self.session_keepalives.insert(id, keepalive);

        if already_active {
            self.adopt_active_window_authority(&previous_authority_label);
            #[cfg(feature = "plugins")]
            self.update_plugin_state_snapshot();
            self.sync_terminal_mode_to_active_buffer();
            self.relayout();
        } else {
            #[cfg(feature = "plugins")]
            self.update_plugin_state_snapshot();
        }
    }

    /// Replace a live remote session without carrying any state from the old
    /// authority. Used for an explicit remote project switch and whenever a
    /// reconnect proves a different tenant identity at the same locator.
    pub(crate) fn replace_remote_window_without_restore(
        &mut self,
        id: WindowId,
        authority: crate::services::authority::Authority,
        keepalive: Box<dyn std::any::Any + Send>,
        root: PathBuf,
        label: Option<String>,
        spec: crate::services::authority::SessionAuthoritySpec,
    ) {
        let Some(existing) = self.windows.get(&id) else {
            drop(authority);
            drop(keepalive);
            return;
        };
        let terminal_ids: std::collections::HashSet<_> = existing
            .terminal_manager
            .tracked_terminal_ids()
            .into_iter()
            .chain(
                existing
                    .terminal_buffers
                    .values()
                    .map(|binding| binding.terminal_id),
            )
            .chain(existing.terminal_companions.keys().copied())
            .chain(existing.terminal_commands.keys().copied())
            .chain(existing.terminal_script_tokens.keys().copied())
            .collect();
        let old_connection_id = existing.authority().filesystem.remote_channel_id();
        let previous_authority_label = (self.active_window == id)
            .then(|| existing.authority().display_label.clone())
            .unwrap_or_default();
        for terminal_id in terminal_ids {
            let terminal = fresh_core::WindowTerminalId::new(id, terminal_id);
            self.purge_omp_companion_terminal(terminal);
            self.terminal_stop_tombstones.remove(&terminal);
        }
        #[cfg(feature = "plugins")]
        self.file_watcher_manager.unwatch_window(id);
        if let Some(existing) = self.windows.get_mut(&id) {
            existing.revoke_all_terminal_script_tokens();
            for terminal_id in existing.terminal_manager.tracked_terminal_ids() {
                existing.terminal_manager.close(terminal_id);
            }
            for (entry, result) in existing.process_groups.signal_all("SIGKILL") {
                if let Err(error) = result {
                    tracing::warn!(
                        ?id,
                        pid = entry.leader_pid,
                        "failed to stop replaced remote process group: {error}"
                    );
                }
            }
        }
        let background_ids: Vec<_> = self
            .background_process_handles
            .iter()
            .filter_map(|(process_id, (owner, _))| (*owner == id).then_some(*process_id))
            .collect();
        for process_id in background_ids {
            if let Some((_, handle)) = self.background_process_handles.remove(&process_id) {
                handle.abort();
            }
        }
        let host_ids: Vec<_> = self
            .host_process_handles
            .iter()
            .filter_map(|(process_id, (owner, _))| (*owner == id).then_some(*process_id))
            .collect();
        for process_id in host_ids {
            if let Some((_, cancel)) = self.host_process_handles.remove(&process_id) {
                let _ = cancel.send(());
            }
        }
        let old = self.windows.remove(&id).expect("window checked above");
        let stable_id = old.stable_id.clone();
        let label = label.unwrap_or_else(|| old.label.clone());
        drop(old);
        self.session_keepalives.remove(&id);
        self.pending_remote_reattach.remove(&id);
        self.remote_connected_cache.remove(&id);
        if let Some(connection_id) = old_connection_id {
            self.stop_remote_reconnect_forwarder(connection_id);
        }

        let mut resources = self.window_resources();
        resources.fs_manager = std::sync::Arc::new(crate::services::fs::FsManager::new(
            std::sync::Arc::clone(&authority.filesystem),
        ));
        let mut window = crate::app::window::Window::new(id, label, root, authority, resources);
        window.stable_id = stable_id;
        window.authority_spec = spec;
        window.terminal_width = self.terminal_width;
        window.terminal_height = self.terminal_height;
        window.seed_initial_layout();
        window.set_status_message("Connected".to_string());
        self.windows.insert(id, window);
        self.session_keepalives.insert(id, keepalive);

        if self.active_window == id {
            self.adopt_active_window_authority(&previous_authority_label);
            self.sync_terminal_mode_to_active_buffer();
            self.relayout();
        }
        #[cfg(feature = "plugins")]
        self.update_plugin_state_snapshot();
    }

    /// Ensure a dormant remote session has its **empty shell** `Window`, so a
    /// dive can commit the switch immediately — before (and regardless of
    /// whether) its backend connect resolves (issue #2570: the dock must
    /// never select a workspace the editor didn't actually enter, and a dead
    /// host can keep the connect in flight for minutes).
    ///
    /// The shell is a real `Window` on a local placeholder authority with
    /// nothing restored into it: its persisted workspace can only be restored
    /// through the connected backend, so it stays on disk, authoritative
    /// (`save_workspace_for` skips descriptor-backed ids). The descriptor is
    /// deliberately **kept** in `dormant_remote`, so diving again retries the
    /// connect and a success still lands in `promote_dormant_remote` — which
    /// replaces this shell with the fully-restored window. While the shell is
    /// active, the status bar presents the in-flight connect as `Connecting`
    /// and a recorded failure as `Disconnected` (with the Retry popup).
    pub(crate) fn ensure_dormant_shell(&mut self, id: WindowId) {
        if self.windows.contains_key(&id) {
            return;
        }
        let Some(descriptor) = self.dormant_remote.get(&id) else {
            return;
        };
        let root = descriptor.root.clone();
        // Same per-session local scope a boot-discovered local shell gets:
        // its own trust + env handles, never a clone of the previous
        // window's. Routed through the blessed factory so this shell inherits
        // the worktree→repo trust keying too.
        let authority = self.local_session_authority(&root);
        let mut window = Window::new(
            id,
            descriptor.label.clone(),
            root,
            authority,
            self.window_resources(),
        );
        window.terminal_width = self.terminal_width;
        window.terminal_height = self.terminal_height;
        window.plugin_state = descriptor.plugin_state.clone();
        // Keep the backend identity so the status bar / dock present the
        // session as its real (not-yet-connected) backend and a retry knows
        // what to reconnect to — never downgraded to local.
        window.authority_spec = descriptor.authority_spec.clone();
        // The shell renders as a placeholder page (see
        // `render_dormant_shell_page`), not as an editable buffer — nothing
        // can be meaningfully edited before the backend connects. Seed the
        // layout here (so the renderer has a populated `splits`) and lock
        // its scratch buffer.
        window.seed_initial_layout();
        let seed_buffer = window.active_buffer();
        window.mark_buffer_read_only(seed_buffer, true);
        self.windows.insert(id, window);
    }
}

#[cfg(test)]
mod tests {
    use super::finish_terminal_artifacts;
    use std::fs::OpenOptions;
    use std::sync::Arc;

    fn test_editor(root: &std::path::Path) -> crate::app::Editor {
        let dir_context = crate::config_io::DirectoryContext::for_testing(root);
        crate::app::Editor::for_test(
            crate::config::Config::default(),
            80,
            24,
            Some(root.to_path_buf()),
            dir_context,
            crate::view::color_support::ColorCapability::TrueColor,
            Arc::new(crate::model::filesystem::StdFileSystem),
            None,
            None,
            false,
            false,
        )
        .unwrap()
    }

    #[test]
    fn extraction_keeps_source_lock_path_fenced_until_cutover() {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("source");
        let target_dir = temp.path().join("target");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();
        let source = source_dir.join("terminal.txt");
        let destination = target_dir.join("terminal.txt");
        let source_lock = crate::services::terminal::manager::terminal_artifact_lock_path(&source);
        let destination_lock =
            crate::services::terminal::manager::terminal_artifact_lock_path(&destination);
        std::fs::write(&source, b"history").unwrap();
        let writer_lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&source_lock)
            .unwrap();
        writer_lock.lock().unwrap();
        let relocations = [
            crate::workspace::TerminalArtifactRelocation {
                source: source_lock.clone(),
                destination: destination_lock.clone(),
                after_source_cutover: true,
            },
            crate::workspace::TerminalArtifactRelocation {
                source: source.clone(),
                destination: destination.clone(),
                after_source_cutover: false,
            },
        ];

        assert!(finish_terminal_artifacts(
            relocations
                .iter()
                .filter(|move_| !move_.after_source_cutover)
        ));
        assert!(!source.exists());
        assert!(destination.exists());
        assert!(source_lock.exists());
        assert!(!destination_lock.exists());
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&source_lock)
            .unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));

        assert!(finish_terminal_artifacts(
            relocations
                .iter()
                .filter(|move_| move_.after_source_cutover)
        ));
        assert!(!source_lock.exists());
        assert!(destination_lock.exists());
    }

    #[test]
    fn pointer_switch_cancels_outgoing_mouse_state() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("source");
        let target_root = temp.path().join("target");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&target_root).unwrap();
        let mut editor = test_editor(&source_root);
        let source = editor.active_window;
        let target = editor.create_window_at(target_root, "target".into());
        {
            let mouse = &mut editor.windows.get_mut(&source).unwrap().mouse_state;
            mouse.start_mouse_gesture(crossterm::event::MouseButton::Left);
            mouse.dragging_text_selection = true;
        }

        editor.switch_active_window_pointer(target);

        assert_eq!(editor.active_window, target);
        let mouse = &editor.windows.get(&source).unwrap().mouse_state;
        assert!(mouse.gesture_captures.is_empty());
        assert!(!mouse.dragging_text_selection);
    }

    #[test]
    fn dormant_promotion_restores_exact_id_without_stealing_focus() {
        let temp = tempfile::tempdir().unwrap();
        let base_root = temp.path().join("base");
        let remote_root = temp.path().join("remote");
        std::fs::create_dir_all(&base_root).unwrap();
        std::fs::create_dir_all(&remote_root).unwrap();
        let alpha = remote_root.join("alpha.txt");
        let beta = remote_root.join("beta.txt");
        std::fs::write(&alpha, "alpha").unwrap();
        std::fs::write(&beta, "beta").unwrap();
        let mut editor = test_editor(&base_root);
        let foreground = editor.active_window;

        let target = editor.create_window_at(remote_root.clone(), "target".into());
        editor.set_active_window(target);
        editor.open_file(&alpha).unwrap();
        editor.save_workspace_for(target).unwrap();
        let target_stable_id = editor.windows.get(&target).unwrap().stable_id.clone();

        let sibling = editor.create_co_tenant_window(remote_root.clone());
        editor.set_active_window(sibling);
        editor.open_file(&beta).unwrap();
        editor.save_workspace_for(sibling).unwrap();
        editor.set_active_window(foreground);

        editor.windows.remove(&target).unwrap();
        let spec = crate::services::authority::SessionAuthoritySpec::RemoteAgent(
            crate::services::authority::RemoteAgentSpec {
                transport: crate::services::authority::RemoteTransportSpec::Ssh {
                    user: None,
                    host: "example.invalid".to_string(),
                    port: None,
                    identity_file: None,
                    remote_path: Some(remote_root.to_string_lossy().into_owned()),
                    extra_args: Vec::new(),
                },
                verified_anchor: None,
                canonical_root: None,
                base_env: Vec::new(),
                window: true,
                label: Some("target".to_string()),
                command: None,
            },
        );
        editor.dormant_remote.insert(
            target,
            crate::app::orchestrator_persistence::PersistedWindow {
                id: target.0,
                label: "target".to_string(),
                root: remote_root.clone(),
                project_path: Some(remote_root.clone()),
                shared_worktree: true,
                plugin_state: std::collections::HashMap::new(),
                authority_spec: spec.clone(),
                stable_id: Some(target_stable_id.clone()),
            },
        );
        let authority = editor.local_session_authority(&remote_root);

        editor.promote_dormant_remote(
            target,
            authority,
            Box::new(()),
            remote_root.clone(),
            spec,
            true,
        );

        assert_eq!(editor.active_window, foreground);
        let promoted = editor.windows.get(&target).unwrap();
        assert_eq!(promoted.stable_id, target_stable_id);
        assert!(promoted.buffer_metadata.values().any(|metadata| {
            metadata
                .file_path()
                .is_some_and(|path| path.ends_with("alpha.txt"))
        }));
        assert!(!promoted.buffer_metadata.values().any(|metadata| {
            metadata
                .file_path()
                .is_some_and(|path| path.ends_with("beta.txt"))
        }));
    }

    #[test]
    fn remote_identity_replacement_drops_old_terminal_and_plugin_state() {
        let temp = tempfile::tempdir().unwrap();
        let old_root = temp.path().join("old");
        let new_root = temp.path().join("new");
        std::fs::create_dir_all(&old_root).unwrap();
        std::fs::create_dir_all(&new_root).unwrap();
        let mut editor = test_editor(&old_root);
        let window_id = editor.active_window;
        let old_stable_id = editor.windows[&window_id].stable_id.clone();
        let terminal_id = fresh_core::TerminalId(23);
        {
            let window = editor.windows.get_mut(&window_id).unwrap();
            window.create_terminal_buffer_detached(terminal_id);
            window
                .terminal_commands
                .insert(terminal_id, vec!["old-agent".to_string()]);
            window.plugin_state.insert(
                "old-plugin".to_string(),
                std::collections::HashMap::from([("tenant".to_string(), serde_json::json!("old"))]),
            );
            window.remember_terminal_script_access(terminal_id);
        }

        #[cfg(feature = "plugins")]
        let watch_handle = {
            let owner = crate::services::file_watcher::WatchOwner {
                window_id,
                authority: editor.windows[&window_id].authority().stamp(),
                plugin_instance_id: None,
            };
            editor
                .file_watcher_manager
                .watch(
                    &crate::services::async_bridge::AsyncBridge::new(),
                    &old_root,
                    true,
                    owner,
                )
                .unwrap()
        };

        let authority = editor.local_session_authority(&new_root);
        editor.replace_remote_window_without_restore(
            window_id,
            authority,
            Box::new(()),
            new_root.clone(),
            Some("new".to_string()),
            crate::services::authority::SessionAuthoritySpec::Local,
        );

        let replacement = editor.windows.get(&window_id).unwrap();
        assert_eq!(replacement.root, new_root);
        assert_eq!(replacement.stable_id, old_stable_id);
        assert!(replacement.terminal_buffers.is_empty());
        assert!(replacement.terminal_commands.is_empty());
        assert!(replacement.terminal_script_tokens.is_empty());
        assert!(replacement.plugin_state.is_empty());
        #[cfg(feature = "plugins")]
        assert!(editor.file_watcher_manager.owner(watch_handle).is_none());
    }
    #[test]
    fn closing_window_aborts_only_its_hanging_reconnect() {
        let temp = tempfile::tempdir().unwrap();

        let base_root = temp.path().join("base");
        let target_root = temp.path().join("target");
        std::fs::create_dir_all(&base_root).unwrap();
        std::fs::create_dir_all(&target_root).unwrap();
        let mut editor = test_editor(&base_root);
        let foreground = editor.active_window;
        let target = editor.create_window_at(target_root, "target".into());
        editor.set_active_window(foreground);
        let foreground_attempt = editor
            .begin_remote_attach_attempt(crate::app::RemoteAttachOwner::Reconnect {
                window_id: foreground,
            })
            .unwrap();
        let target_attempt = editor
            .begin_remote_attach_attempt(crate::app::RemoteAttachOwner::Reconnect {
                window_id: target,
            })
            .unwrap();
        let (foreground_cancel, mut foreground_cancelled) = tokio::sync::oneshot::channel();
        let (target_cancel, mut target_cancelled) = tokio::sync::oneshot::channel();
        editor
            .remote_attach_cancels
            .insert(foreground_attempt, foreground_cancel);
        editor
            .remote_attach_cancels
            .insert(target_attempt, target_cancel);

        assert!(editor.close_window(target));

        assert_eq!(target_cancelled.try_recv(), Ok(()));
        assert!(matches!(
            foreground_cancelled.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            editor.remote_reconnect_attempts.get(&foreground),
            Some(&foreground_attempt)
        );
        assert!(!editor.remote_reconnect_attempts.contains_key(&target));
    }
    #[cfg(feature = "plugins")]
    #[test]
    fn remote_scope_is_keyed_by_verified_anchor_and_canonical_root() {
        use crate::services::authority::{
            remote_session_scope_for_identity, RemoteTenantAnchor, RemoteTenantIdentity,
        };
        use crate::services::workspace_trust::TrustLevel;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("local");
        std::fs::create_dir_all(&root).unwrap();
        let editor = test_editor(&root);
        editor
            .active_window()
            .authority()
            .workspace_trust
            .set_level_transient(TrustLevel::Trusted);
        let identity = RemoteTenantIdentity {
            anchor: RemoteTenantAnchor {
                digest: "a".repeat(64),
            },
            canonical_root: std::path::PathBuf::from("/workspace"),
        };

        let first = remote_session_scope_for_identity(&editor.dir_context, &identity);
        assert_eq!(first.trust.level(), TrustLevel::Restricted);
        first.trust.set_level(TrustLevel::Trusted);

        let first_reopened = remote_session_scope_for_identity(&editor.dir_context, &identity);
        let other_tenant = remote_session_scope_for_identity(
            &editor.dir_context,
            &RemoteTenantIdentity {
                anchor: RemoteTenantAnchor {
                    digest: "b".repeat(64),
                },
                canonical_root: std::path::PathBuf::from("/workspace"),
            },
        );
        let other_root = remote_session_scope_for_identity(
            &editor.dir_context,
            &RemoteTenantIdentity {
                anchor: identity.anchor,
                canonical_root: std::path::PathBuf::from("/other-workspace"),
            },
        );
        assert_eq!(first_reopened.trust.level(), TrustLevel::Trusted);
        assert_eq!(other_tenant.trust.level(), TrustLevel::Restricted);
        assert_eq!(other_root.trust.level(), TrustLevel::Restricted);
    }
}
