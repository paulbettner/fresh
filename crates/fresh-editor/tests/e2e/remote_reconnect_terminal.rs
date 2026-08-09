//! Component test for the reconnect terminal-respawn mechanic.
//!
//! When a live remote window's carrier drops, its embedded PTY dies (a separate
//! `ssh -t` channel from the agent connection). A reconnect re-points the
//! window's authority and then calls
//! [`Window::respawn_terminals_through_authority`] to revive the dead
//! terminal(s) *in place*: a fresh PTY bound to the same buffer, spawned through
//! the (now-reconnected) authority, reusing the backing file so scrollback
//! continues.
//!
//! Whether the reborn PTY actually runs on the remote host is not observable
//! hermetically (localhost SSH shares the host; only `$SSH_CONNECTION`
//! discriminates — see the manual verification in the PR). What *is* a
//! component invariant — and the part that regressed in the first cut, where the
//! reconnect re-pointed the authority but left the terminal dead — is the
//! respawn bookkeeping: the buffer must end up bound to a *new, live* terminal,
//! every terminal-id-keyed map remapped off the stale id, and the backing file
//! preserved. This drives that directly. Requires a working PTY (`/dev/ptmx`);
//! skips when unavailable, like the other terminal e2e tests.

use crate::common::{
    harness::{EditorTestHarness, HarnessOptions},
    PathGuard,
};
use fresh::config::Config;
use fresh_core::api::TerminalCompanion;
use portable_pty::{native_pty_system, PtySize};

fn pty_available() -> bool {
    native_pty_system()
        .openpty(PtySize {
            rows: 1,
            cols: 1,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

#[test]
#[cfg_attr(target_os = "windows", ignore)] // Unix PTY shell
fn reconnect_respawns_a_dead_embedded_terminal_in_place() {
    if !pty_available() {
        eprintln!("Skipping reconnect terminal-respawn test: PTY not available");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let mut harness = EditorTestHarness::create(
        120,
        30,
        HarnessOptions::new().with_working_dir(temp.path().to_path_buf()),
    )
    .unwrap();

    let window = harness.editor_mut().active_window_mut();

    // A live embedded terminal, bound to its buffer with a backing file — the
    // pre-disconnect state.
    let (old_id, buffer_id) = window
        .open_terminal_in_window()
        .expect("terminal should spawn");
    let backing_before = window
        .terminal_backing_files
        .get(&old_id)
        .cloned()
        .expect("a live terminal records its backing file");

    // Carrier drop: the PTY died and its handle was torn down — exactly what the
    // `TerminalExited` handler does (`terminal_manager.close`) — but the
    // buffer↔terminal binding is *kept* (the remote-disconnect preserve path) so
    // the reconnect has something to revive.
    window.terminal_manager.close(old_id);
    assert!(
        window.terminal_manager.get(old_id).is_none(),
        "dead terminal's handle is torn down"
    );
    assert_eq!(
        window.get_terminal_id(buffer_id),
        Some(old_id),
        "binding is preserved across the drop, awaiting respawn"
    );

    // Reconnect's in-place respawn.
    window.respawn_terminals_through_authority();

    // The buffer is bound to a *new, live* terminal.
    let new_id = window
        .get_terminal_id(buffer_id)
        .expect("buffer is still bound to a terminal after respawn");
    assert_ne!(
        new_id, old_id,
        "respawn allocates a fresh terminal id, not the dead one"
    );
    assert!(
        window
            .terminal_manager
            .get(new_id)
            .is_some_and(|h| h.is_alive()),
        "respawned terminal is live"
    );

    // Scrollback continuity: the backing file is reused, remapped onto the new
    // id, and the stale id is fully unmapped (no leak).
    assert_eq!(
        window.terminal_backing_files.get(&new_id),
        Some(&backing_before),
        "the new terminal reuses the old backing file"
    );
    assert!(
        !window.terminal_backing_files.contains_key(&old_id),
        "the stale terminal id is unmapped from terminal_backing_files"
    );
}

/// A *still-live* terminal must be left untouched by a respawn pass — reviving a
/// healthy PTY would orphan the running one. Guards the `is_alive` skip.
#[test]
#[cfg_attr(target_os = "windows", ignore)] // Unix PTY shell
fn respawn_leaves_a_live_terminal_untouched() {
    if !pty_available() {
        eprintln!("Skipping live-terminal respawn test: PTY not available");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let mut harness = EditorTestHarness::create(
        120,
        30,
        HarnessOptions::new().with_working_dir(temp.path().to_path_buf()),
    )
    .unwrap();

    let window = harness.editor_mut().active_window_mut();
    let (id, buffer_id) = window
        .open_terminal_in_window()
        .expect("terminal should spawn");
    assert!(
        window
            .terminal_manager
            .get(id)
            .is_some_and(|h| h.is_alive()),
        "terminal is live before respawn"
    );

    window.respawn_terminals_through_authority();

    assert_eq!(
        window.get_terminal_id(buffer_id),
        Some(id),
        "a live terminal keeps its id (not respawned)"
    );
}

#[test]
#[cfg_attr(target_os = "windows", ignore)]
fn reconnect_uses_exact_omp_resume_when_generic_resume_is_disabled() {
    if !pty_available() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let resumed = temp.path().join("RESUMED");
    let relaunched = temp.path().join("RELAUNCHED");
    let omp = temp.path().join("omp");
    std::fs::write(
        &omp,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--fresh-omp-companion\" ] && [ \"$2\" = \"--version\" ]; then exit 0; fi\n\
             case \"$1\" in\n\
               --resume) touch '{}'; exec sleep 30 ;;\n\
               *) touch '{}'; exec sleep 30 ;;\n\
             esac\n",
            resumed.display(),
            relaunched.display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _path_guard = PathGuard::prepend_with_trusted_omp(temp.path(), &omp);
    let session_id = "123e4567-e89b-42d3-a456-426614174099";
    let mut config = Config::default();
    config.terminal.resume_agents = false;
    let mut harness = EditorTestHarness::create(
        120,
        30,
        HarnessOptions::new()
            .with_config(config)
            .with_working_dir(temp.path().to_path_buf()),
    )
    .unwrap();

    let window = harness.editor_mut().active_window_mut();
    let (old_id, buffer_id) = window.open_terminal_in_window().unwrap();
    window
        .terminal_commands
        .insert(old_id, vec!["omp".into(), "--initial".into()]);
    window.terminal_resume_commands.insert(
        old_id,
        vec!["omp".into(), "--resume".into(), session_id.into()],
    );
    window
        .terminal_companions
        .insert(old_id, TerminalCompanion::Omp);
    window.terminal_manager.close(old_id);

    assert_eq!(window.respawn_terminals_through_authority(), 1);
    let new_id = window.get_terminal_id(buffer_id).unwrap();
    assert_ne!(new_id, old_id);
    harness.wait_until(|_| resumed.exists()).unwrap();
    assert!(!relaunched.exists());
}
