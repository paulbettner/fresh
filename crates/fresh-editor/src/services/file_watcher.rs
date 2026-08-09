//! Plugin-driven filesystem watching.
//!
//! Backs the `watchPath` / `unwatchPath` plugin API and the
//! `path_changed` plugin hook. One editor-local `notify::Watcher` is shared
//! across that editor's plugin watches; each `watchPath` call registers a path
//! and stores a per-call handle in [`FileWatcherManager`] so unwatching is a
//! removal lookup rather than tearing down unrelated registrations.
//!
//! Events flow notify-thread → AsyncBridge → main loop →
//! `path_changed` hook. The path is passed verbatim from
//! `notify::Event::paths` (no canonicalisation, no debouncing —
//! plugins decide their dedup policy).
//!
//! **Why not per-plugin watchers?** notify's backends (inotify on Linux,
//! kqueue on BSD/macOS, ReadDirectoryChangesW on Windows) all have
//! per-process file-descriptor / handle limits. Sharing within one editor
//! avoids a watcher thread per plugin, while the manager-local callback map
//! prevents events from crossing into another editor's AsyncBridge.

use crate::services::async_bridge::{AsyncBridge, AsyncMessage, PathChangeKind};
use notify::{
    event::{CreateKind, EventKind, ModifyKind, RemoveKind},
    RecommendedWatcher, RecursiveMode, Watcher,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Allocate a process-globally-unique opaque handle. Handles are exposed to
/// plugins and may be compared across editor instances in tests, but callback
/// routing remains private to the manager that registered the watch.
fn alloc_global_handle() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Immutable authority that registered a filesystem watch. Events and
/// unwatch requests must return through this exact plugin/window generation;
/// a remote project replacement must not inherit a host watcher from the
/// authority it replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WatchOwner {
    pub window_id: fresh_core::WindowId,
    pub authority: fresh_core::api::AuthorityStamp,
    pub plugin_instance_id: Option<fresh_core::api::PluginInstanceId>,
}

struct WatchRegistration {
    path: PathBuf,
    mode: RecursiveMode,
    owner: WatchOwner,
}

/// Manages plugin-registered file watchers. Created on demand the
/// first time a `WatchPath` arrives — the `notify::Watcher` is
/// non-zero-cost (spawns a backend thread on macOS / Windows) and
/// many editor instances never need it at all.
pub struct FileWatcherManager {
    /// The single shared notify `Watcher`. `None` until the first
    /// successful `watch` call wires up the AsyncBridge route.
    watcher: Option<RecommendedWatcher>,
    /// `handle → registration`. Ownership is immutable so queued events and
    /// guessed handle ids cannot escape the plugin/window/authority that
    /// created the watch.
    handles: HashMap<u64, WatchRegistration>,
    /// Callback-visible registrations for this manager only. A process-global
    /// map would route one editor's filesystem event into every other editor's
    /// AsyncBridge.
    callback_handles: Arc<Mutex<HandleMap>>,
}

impl FileWatcherManager {
    pub fn new() -> Self {
        Self {
            watcher: None,
            handles: HashMap::new(),
            callback_handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a watch. `bridge` is needed only on the first call
    /// to construct the `Watcher`; subsequent calls reuse the
    /// existing Watcher and ignore the parameter.
    ///
    /// Returns the allocated handle on success, or an error string
    /// on `notify` failures (path missing, permission, kernel
    /// limit). Errors are surfaced to the plugin via
    /// `WatchPathRegistered::result`.
    pub fn watch(
        &mut self,
        bridge: &AsyncBridge,
        path: &Path,
        recursive: bool,
        owner: WatchOwner,
    ) -> Result<u64, String> {
        if self.watcher.is_none() {
            self.watcher = Some(build_watcher(
                bridge.clone(),
                Arc::clone(&self.callback_handles),
            )?);
        }
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let effective_mode = if mode == RecursiveMode::Recursive
            || self.handles.values().any(|registration| {
                registration.path == path && registration.mode == RecursiveMode::Recursive
            }) {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let watcher = self
            .watcher
            .as_mut()
            .expect("just constructed above if missing");
        watcher
            .watch(path, effective_mode)
            .map_err(|e| format!("watchPath({}): {}", path.display(), e))?;
        let handle = alloc_global_handle();
        self.handles.insert(
            handle,
            WatchRegistration {
                path: path.to_path_buf(),
                mode,
                owner,
            },
        );
        if let Ok(mut handles) = self.callback_handles.lock() {
            handles.insert(handle, (path.to_path_buf(), mode));
        }
        Ok(handle)
    }

    /// Return the immutable owner for a live handle.
    pub fn owner(&self, handle: u64) -> Option<WatchOwner> {
        self.handles
            .get(&handle)
            .map(|registration| registration.owner)
    }

    /// Drop a watcher only when the caller owns its exact registration.
    pub fn unwatch_owned(&mut self, handle: u64, owner: WatchOwner) -> bool {
        if self.owner(handle) != Some(owner) {
            return false;
        }
        self.unwatch(handle);
        true
    }

    /// Drop every watcher owned by a closing or authority-replaced window.
    pub fn unwatch_window(&mut self, window_id: fresh_core::WindowId) {
        let handles: Vec<_> = self
            .handles
            .iter()
            .filter_map(|(handle, registration)| {
                (registration.owner.window_id == window_id).then_some(*handle)
            })
            .collect();
        for handle in handles {
            self.unwatch(handle);
        }
    }

    /// Drop a registered watcher. Unknown handles are ignored.
    pub fn unwatch(&mut self, handle: u64) {
        if let Some(registration) = self.handles.remove(&handle) {
            if let Ok(mut handles) = self.callback_handles.lock() {
                handles.remove(&handle);
            }
            let remaining_mode = if self.handles.values().any(|candidate| {
                candidate.path == registration.path && candidate.mode == RecursiveMode::Recursive
            }) {
                Some(RecursiveMode::Recursive)
            } else if self
                .handles
                .values()
                .any(|candidate| candidate.path == registration.path)
            {
                Some(RecursiveMode::NonRecursive)
            } else {
                None
            };
            if let Some(w) = self.watcher.as_mut() {
                if let Err(e) = w.unwatch(&registration.path) {
                    tracing::debug!(
                        "unwatchPath({}): notify returned {}; continuing — the editor's view is now consistent",
                        registration.path.display(),
                        e
                    );
                }
                if let Some(mode) = remaining_mode {
                    if let Err(error) = w.watch(&registration.path, mode) {
                        tracing::warn!(
                            "failed to preserve remaining watchPath({}): {}",
                            registration.path.display(),
                            error
                        );
                    }
                }
            }
        }
    }
}

impl Default for FileWatcherManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Notify event → AsyncMessage routing
//
// notify delivers events on its backend thread; we translate them
// into `AsyncMessage::PathChanged` and post via the AsyncBridge.
// The mapping is many-to-many: one event may carry multiple paths
// (rename old / new), and one path may match multiple registered
// watchers (a path watched directly + a parent watched
// recursively). Strategy:
//
// - For each `Event::paths`, find every registered handle whose
//   watch path is an ancestor (recursive) or equal (non-recursive)
//   to the event path.
// - Emit one `PathChanged` per (handle, path) pair.
//
// The callback gets this manager's own `Arc<Mutex<HandleMap>>`: notify needs
// `'static` ownership, but callback routing must remain editor-local.
// ---------------------------------------------------------------

/// Type alias kept short for readability. Stores `(path, recursive)`
/// keyed by handle — the source of truth for the notify callback's
/// path-prefix lookups.
type HandleMap = HashMap<u64, (PathBuf, RecursiveMode)>;

fn matches_handle(watch_path: &Path, recursive: RecursiveMode, event_path: &Path) -> bool {
    match recursive {
        RecursiveMode::Recursive => event_path.starts_with(watch_path),
        RecursiveMode::NonRecursive => {
            // notify reports the changed path verbatim. For
            // non-recursive watches we accept the watch path
            // itself OR its direct children — the user's mental
            // model of "watch this directory" includes its
            // immediate contents. Sub-children fall through.
            event_path == watch_path
                || event_path
                    .parent()
                    .map(|p| p == watch_path)
                    .unwrap_or(false)
        }
    }
}

fn classify_kind(kind: &EventKind) -> PathChangeKind {
    match kind {
        EventKind::Create(CreateKind::File)
        | EventKind::Create(CreateKind::Folder)
        | EventKind::Create(CreateKind::Any)
        | EventKind::Create(CreateKind::Other) => PathChangeKind::Create,
        EventKind::Remove(RemoveKind::File)
        | EventKind::Remove(RemoveKind::Folder)
        | EventKind::Remove(RemoveKind::Any)
        | EventKind::Remove(RemoveKind::Other) => PathChangeKind::Delete,
        EventKind::Modify(ModifyKind::Name(_)) => PathChangeKind::Rename,
        EventKind::Modify(_) => PathChangeKind::Modify,
        _ => PathChangeKind::Other,
    }
}

fn build_watcher(
    bridge: AsyncBridge,
    handles: Arc<Mutex<HandleMap>>,
) -> Result<RecommendedWatcher, String> {
    let bridge = Arc::new(bridge);
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("notify error event: {}", e);
                return;
            }
        };
        let kind = classify_kind(&event.kind);
        let map = match handles.lock() {
            Ok(m) => m,
            Err(_) => return,
        };
        for path in event.paths.iter() {
            for (handle, (watch_path, mode)) in map.iter() {
                if matches_handle(watch_path, *mode, path) {
                    #[allow(clippy::let_underscore_must_use)]
                    let _ = bridge.sender().send(AsyncMessage::PathChanged {
                        handle: *handle,
                        path: path.clone(),
                        kind,
                    });
                }
            }
        }
    })
    .map_err(|e| format!("notify::recommended_watcher: {}", e))?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recursive matches: any descendant of the watch path counts;
    /// non-recursive matches only the watch path itself or its
    /// direct children.
    #[test]
    fn matches_handle_respects_recursive_mode() {
        let root = Path::new("/repo");
        assert!(matches_handle(
            root,
            RecursiveMode::Recursive,
            Path::new("/repo/src/lib.rs")
        ));
        assert!(matches_handle(
            root,
            RecursiveMode::NonRecursive,
            Path::new("/repo/lib.rs")
        ));
        assert!(!matches_handle(
            root,
            RecursiveMode::NonRecursive,
            Path::new("/repo/src/lib.rs")
        ));
        assert!(!matches_handle(
            root,
            RecursiveMode::Recursive,
            Path::new("/other/file.rs")
        ));
    }

    /// Kind classification buckets every notify-supplied variant
    /// into one of the five exposed strings.
    #[test]
    fn kind_classification_covers_main_variants() {
        use notify::event::*;
        assert!(matches!(
            classify_kind(&EventKind::Create(CreateKind::File)),
            PathChangeKind::Create
        ));
        assert!(matches!(
            classify_kind(&EventKind::Remove(RemoveKind::File)),
            PathChangeKind::Delete
        ));
        assert!(matches!(
            classify_kind(&EventKind::Modify(ModifyKind::Data(DataChange::Content))),
            PathChangeKind::Modify
        ));
        assert!(matches!(
            classify_kind(&EventKind::Modify(ModifyKind::Name(RenameMode::Both))),
            PathChangeKind::Rename
        ));
    }

    #[test]
    fn watcher_registration_is_manager_local_and_exactly_owned() {
        let handle = alloc_global_handle();
        let owner = WatchOwner {
            window_id: fresh_core::WindowId(7),
            authority: fresh_core::api::AuthorityStamp {
                id: 11,
                generation: 3,
            },
            plugin_instance_id: Some(fresh_core::api::PluginInstanceId::fresh()),
        };
        let mut manager = FileWatcherManager::new();
        manager.callback_handles.lock().unwrap().insert(
            handle,
            (PathBuf::from("/repo"), RecursiveMode::NonRecursive),
        );
        manager.handles.insert(
            handle,
            WatchRegistration {
                path: PathBuf::from("/repo"),
                mode: RecursiveMode::NonRecursive,
                owner,
            },
        );

        let other_manager = FileWatcherManager::new();
        assert!(!other_manager
            .callback_handles
            .lock()
            .unwrap()
            .contains_key(&handle));
        assert!(matches!(
            manager.callback_handles.lock().unwrap().get(&handle),
            Some((path, RecursiveMode::NonRecursive)) if path == Path::new("/repo")
        ));

        let wrong_owner = WatchOwner {
            authority: fresh_core::api::AuthorityStamp {
                id: 11,
                generation: 4,
            },
            ..owner
        };
        assert!(!manager.unwatch_owned(handle, wrong_owner));
        assert_eq!(manager.owner(handle), Some(owner));
        manager.unwatch_window(owner.window_id);
        assert!(manager.owner(handle).is_none());
        assert!(!manager
            .callback_handles
            .lock()
            .unwrap()
            .contains_key(&handle));
    }
}
