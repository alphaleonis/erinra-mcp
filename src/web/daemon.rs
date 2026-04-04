//! Daemon state file management with file locking for coordination.
//!
//! Persists a `web.state` JSON file at `{data_dir}/web.state` tracking the
//! daemon PID, port, and registered client PIDs. All mutations use advisory
//! file locking via a separate `.web.state.lock` file.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, RefreshKind};

/// Result of ensure_daemon: either spawned a new daemon or joined existing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonAction {
    Spawned { port: u16 },
    Joined { port: u16 },
}

/// Daemon state persisted to `{data_dir}/web.state`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonState {
    pub daemon_pid: u32,
    pub port: u16,
    pub clients: Vec<u32>,
    pub auth_token: String,
}

/// Path to the state file within a data directory.
fn state_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("web.state")
}

/// Read the current daemon state **without acquiring the lock**.
/// Returns `None` if no state file exists or it is corrupt.
///
/// For consistent reads during state mutation, use [`update_state`] instead.
pub fn read_state(data_dir: &Path) -> Result<Option<DaemonState>> {
    let path = state_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => Ok(Some(state)),
            Err(e) => {
                tracing::warn!("corrupt daemon state file {}: {e}", path.display());
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write daemon state atomically (temp file + rename).
/// Callers must use [`update_state`] to ensure file locking.
fn write_state(data_dir: &Path, state: &DaemonState) -> Result<()> {
    use anyhow::Context;

    let path = state_path(data_dir);
    let tmp_path = data_dir.join(format!(".web.state.{}.tmp", std::process::id()));

    let json = serde_json::to_string_pretty(state).context("failed to serialize daemon state")?;

    std::fs::write(&tmp_path, json.as_bytes())
        .with_context(|| format!("failed to write temp state: {}", tmp_path.display()))?;

    let result = std::fs::rename(&tmp_path, &path);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result.with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

/// Remove the state file.
/// Callers must use [`update_state`] to ensure file locking.
fn remove_state(data_dir: &Path) -> Result<()> {
    let path = state_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Path to the lock file used for coordinating state access.
///
/// A separate lock file (rather than locking `web.state` directly) allows
/// non-critical readers (e.g., `erinra status`) to read the state file
/// without acquiring the lock.
fn lock_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(".web.state.lock")
}

/// Perform a locked read-modify-write on the state file.
/// The callback receives the current state (or `None`) and returns the new state
/// (or `None` to delete).
pub fn update_state<F>(data_dir: &Path, f: F) -> Result<Option<DaemonState>>
where
    F: FnOnce(Option<DaemonState>) -> Option<DaemonState>,
{
    use anyhow::Context;
    use fs2::FileExt;

    let lock_file_path = lock_path(data_dir);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_file_path)
        .with_context(|| format!("failed to open lock file: {}", lock_file_path.display()))?;

    lock_file
        .lock_exclusive()
        .context("failed to acquire exclusive lock")?;

    // Lock is released when `lock_file` is dropped (end of function or early return).
    let current = read_state(data_dir)?;
    let new_state = f(current);

    match &new_state {
        Some(state) => write_state(data_dir, state)?,
        None => remove_state(data_dir)?,
    }

    Ok(new_state)
}

/// Check if a process with the given PID is currently alive.
/// Cross-platform via sysinfo.
pub fn is_pid_alive(pid: u32) -> bool {
    let s = sysinfo::System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
    );
    s.process(sysinfo::Pid::from_u32(pid)).is_some()
}

/// Register a client PID in the daemon state (locked update).
/// If the PID is already present, this is a no-op.
/// Returns an error if no daemon state file exists.
pub fn register_client(data_dir: &Path, client_pid: u32) -> Result<()> {
    let result = update_state(data_dir, |state| {
        let mut state = state?;
        if !state.clients.contains(&client_pid) {
            state.clients.push(client_pid);
        }
        Some(state)
    })?;
    if result.is_none() {
        anyhow::bail!("cannot register client: no daemon state file exists");
    }
    Ok(())
}

/// Deregister a client PID from the daemon state (locked update).
/// If this was the last client, sends SIGTERM to the daemon for prompt shutdown
/// instead of waiting for the next sweep cycle.
pub fn deregister_client(data_dir: &Path, client_pid: u32) -> Result<()> {
    let new_state = update_state(data_dir, |state| {
        let mut state = state?;
        state.clients.retain(|&pid| pid != client_pid);
        Some(state)
    })?;

    // If we were the last client, signal the daemon to shut down promptly.
    if let Some(state) = new_state
        && state.clients.is_empty()
    {
        signal_daemon_shutdown(state.daemon_pid);
    }

    Ok(())
}

/// Send SIGTERM (Unix) or TerminateProcess (Windows) to the daemon.
fn signal_daemon_shutdown(daemon_pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: sending SIGTERM to a valid PID is safe.
        unsafe {
            libc::kill(daemon_pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        // On Windows, use GenerateConsoleCtrlEvent or TerminateProcess.
        // For now, the sweep loop handles this case.
        let _ = daemon_pid;
    }
}

/// Ensure a daemon is running. Cleans stale state, spawns if needed, registers the
/// calling process as a client. Returns what action was taken and the port.
pub fn ensure_daemon(data_dir: &Path, port: u16, bind: &str) -> Result<DaemonAction> {
    // Clean up any stale state (dead daemon / dead clients).
    cleanup_stale_state(data_dir)?;

    // Atomically check-and-claim under a single lock to prevent double-spawn.
    let (action, need_spawn) = {
        let state = update_state(data_dir, |state| {
            if let Some(state) = state {
                // Daemon exists and is alive — return unchanged.
                Some(state)
            } else {
                // No daemon — claim the slot. spawn_daemon will fill in the real PID
                // after we release the lock, but holding the lock here prevents a
                // concurrent caller from also deciding to spawn.
                Some(DaemonState {
                    daemon_pid: 0, // placeholder, updated by run_daemon on startup
                    port,
                    clients: vec![],
                    auth_token: String::new(), // placeholder, set by run_daemon on startup
                })
            }
        })?;
        let state = state.expect("update_state always returns Some here");
        if state.daemon_pid == 0 {
            (DaemonAction::Spawned { port }, true)
        } else {
            (DaemonAction::Joined { port: state.port }, false)
        }
    };

    if need_spawn {
        spawn_daemon(data_dir, port, bind)?;
        // Wait briefly for daemon to be ready, then verify it's actually alive.
        // The daemon updates the state file with its real PID on startup, so
        // a PID of 0 (placeholder) means it hasn't started yet, and a dead PID
        // means it crashed during initialization.
        let mut alive = false;
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if let Some(state) = read_state(data_dir)?
                && state.daemon_pid != 0
            {
                let sys = sysinfo::System::new_with_specifics(
                    sysinfo::RefreshKind::nothing()
                        .with_processes(sysinfo::ProcessRefreshKind::nothing()),
                );
                if sys
                    .process(sysinfo::Pid::from_u32(state.daemon_pid))
                    .is_some()
                {
                    alive = true;
                    break;
                }
            }
        }
        if !alive {
            // Clean up the placeholder state file.
            let _ = std::fs::remove_file(data_dir.join("web.state"));
            let log_path = data_dir.join("daemon.log");
            let hint = if log_path.exists() {
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                // Show the last non-empty line as a hint.
                log.lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| format!("\n  Last log line: {l}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            anyhow::bail!("daemon failed to start. Check {}{hint}", log_path.display());
        }
    }

    // Register ourselves as a client.
    register_client(data_dir, std::process::id())?;

    Ok(action)
}

/// Spawn the daemon as a detached background process by re-executing the current binary
/// with `erinra _daemon --port N --bind ADDR --data-dir PATH`.
fn spawn_daemon(data_dir: &Path, port: u16, bind: &str) -> Result<u32> {
    use anyhow::Context;

    let exe = std::env::current_exe().context("failed to get current executable path")?;
    let log_file = std::fs::File::create(data_dir.join("daemon.log"))
        .context("failed to create daemon.log")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--data-dir")
        .arg(data_dir.as_os_str())
        .arg("_daemon")
        .arg("--port")
        .arg(port.to_string())
        .arg("--bind")
        .arg(bind)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));

    // Detach from parent's process group so Ctrl-C in the terminal
    // doesn't propagate SIGINT to the daemon.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x00000008); // CREATE_NO_WINDOW
    }

    let child = cmd.spawn().context("failed to spawn daemon process")?;
    Ok(child.id())
}

/// Determine whether the daemon should shut down based on client activity.
///
/// When the client list is empty, a grace period starts. If the list remains empty
/// for the full grace duration, returns `true`. If a client appears before the grace
/// period expires, the timer resets.
///
/// This is a pure function suitable for unit testing — the caller manages the
/// `grace_start` state across sweep iterations.
pub fn should_shutdown(
    state: &DaemonState,
    grace_start: &mut Option<std::time::Instant>,
    grace_period: std::time::Duration,
) -> bool {
    if state.clients.is_empty() {
        let start = grace_start.get_or_insert_with(std::time::Instant::now);
        start.elapsed() >= grace_period
    } else {
        *grace_start = None;
        false
    }
}

/// Clean up stale daemon state caused by crashed processes.
///
/// - If no state file exists: returns `None` (no-op).
/// - If daemon PID is dead: removes state file, returns `None`.
/// - If daemon is alive: sweeps dead client PIDs from client list,
///   writes back the cleaned state, returns `Some(cleaned_state)`.
pub fn cleanup_stale_state(data_dir: &Path) -> Result<Option<DaemonState>> {
    // Single process table snapshot for all PID checks in this call.
    let sys = sysinfo::System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
    );

    update_state(data_dir, |state| {
        let mut state = state?;

        // If daemon is dead, remove everything (? propagates None).
        sys.process(sysinfo::Pid::from_u32(state.daemon_pid))?;

        // Daemon alive — sweep dead clients.
        state
            .clients
            .retain(|&pid| sys.process(sysinfo::Pid::from_u32(pid)).is_some());
        Some(state)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> DaemonState {
        DaemonState {
            daemon_pid: 1234,
            port: 9090,
            clients: vec![5678, 9012],
            auth_token: "test-token-abc123".to_string(),
        }
    }

    #[test]
    fn daemon_read_returns_none_when_no_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_state(dir.path()).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn daemon_write_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = sample_state();

        write_state(dir.path(), &state).unwrap();
        let read_back = read_state(dir.path()).unwrap();

        assert_eq!(read_back, Some(state));
    }

    #[test]
    fn daemon_write_is_atomic_no_temp_file_remains() {
        let dir = tempfile::tempdir().unwrap();
        let state = sample_state();

        write_state(dir.path(), &state).unwrap();

        // Only web.state should exist; no temp files.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        assert_eq!(entries, vec!["web.state"]);
    }

    #[test]
    fn daemon_read_returns_none_for_corrupt_state_file() {
        let dir = tempfile::tempdir().unwrap();
        // Write non-JSON garbage to the state file.
        std::fs::write(dir.path().join("web.state"), b"not json at all").unwrap();

        let result = read_state(dir.path()).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn daemon_remove_deletes_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = sample_state();

        write_state(dir.path(), &state).unwrap();
        assert!(read_state(dir.path()).unwrap().is_some());

        remove_state(dir.path()).unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn daemon_remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // No state file exists at all. Removing should not error.
        remove_state(dir.path()).unwrap();
        remove_state(dir.path()).unwrap();
    }

    #[test]
    fn daemon_update_creates_state_from_none() {
        let dir = tempfile::tempdir().unwrap();

        let result = update_state(dir.path(), |prev| {
            assert_eq!(prev, None);
            Some(sample_state())
        })
        .unwrap();

        assert_eq!(result, Some(sample_state()));
        // Verify it was persisted.
        assert_eq!(read_state(dir.path()).unwrap(), Some(sample_state()));
    }

    #[test]
    fn daemon_update_modifies_existing_state() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &sample_state()).unwrap();

        let result = update_state(dir.path(), |prev| {
            let mut s = prev.expect("should have existing state");
            s.clients.push(3333);
            Some(s)
        })
        .unwrap();

        let expected = DaemonState {
            daemon_pid: 1234,
            port: 9090,
            clients: vec![5678, 9012, 3333],
            auth_token: "test-token-abc123".to_string(),
        };
        assert_eq!(result, Some(expected.clone()));
        assert_eq!(read_state(dir.path()).unwrap(), Some(expected));
    }

    #[test]
    fn daemon_update_can_delete_state() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &sample_state()).unwrap();

        let result = update_state(dir.path(), |prev| {
            assert!(prev.is_some());
            None // delete
        })
        .unwrap();

        assert_eq!(result, None);
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn is_pid_alive_returns_true_for_current_process() {
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn is_pid_alive_returns_false_for_nonexistent_pid() {
        assert!(!is_pid_alive(u32::MAX - 1));
    }

    #[test]
    fn cleanup_stale_state_returns_none_when_no_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = cleanup_stale_state(dir.path()).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn cleanup_stale_state_removes_state_when_daemon_pid_is_dead() {
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState {
            daemon_pid: u32::MAX - 1, // non-existent PID
            port: 9090,
            clients: vec![100, 200],
            auth_token: "tok".to_string(),
        };
        write_state(dir.path(), &state).unwrap();

        let result = cleanup_stale_state(dir.path()).unwrap();
        assert_eq!(result, None);
        // State file should be gone.
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn cleanup_stale_state_sweeps_dead_client_pids() {
        let dir = tempfile::tempdir().unwrap();
        let our_pid = std::process::id();
        let dead_pid = u32::MAX - 1;
        let state = DaemonState {
            daemon_pid: our_pid, // alive (our own process)
            port: 9090,
            clients: vec![our_pid, dead_pid],
            auth_token: "tok".to_string(),
        };
        write_state(dir.path(), &state).unwrap();

        let result = cleanup_stale_state(dir.path()).unwrap();
        let expected = DaemonState {
            daemon_pid: our_pid,
            port: 9090,
            clients: vec![our_pid], // dead client removed
            auth_token: "tok".to_string(),
        };
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn cleanup_stale_state_handles_corrupt_state_file() {
        let dir = tempfile::tempdir().unwrap();
        // Write garbage to the state file.
        std::fs::write(dir.path().join("web.state"), b"not json at all").unwrap();

        let result = cleanup_stale_state(dir.path()).unwrap();
        assert_eq!(result, None);
        // Corrupt state file should be removed.
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn register_client_adds_pid_and_deregister_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        // Use a non-existent PID as daemon so signal_daemon_shutdown is harmless.
        let fake_daemon_pid = u32::MAX - 1;

        // Pre-write state with fake daemon PID, empty clients.
        write_state(
            dir.path(),
            &DaemonState {
                daemon_pid: fake_daemon_pid,
                port: 9090,
                clients: vec![],
                auth_token: "tok".to_string(),
            },
        )
        .unwrap();

        // Register a client.
        register_client(dir.path(), 1111).unwrap();
        let state = read_state(dir.path()).unwrap().expect("state should exist");
        assert_eq!(state.clients, vec![1111]);

        // Deregister the client.
        deregister_client(dir.path(), 1111).unwrap();
        let state = read_state(dir.path()).unwrap().expect("state should exist");
        assert!(state.clients.is_empty());
    }

    #[test]
    fn deregister_client_removes_pid_and_detects_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Use a non-existent PID as daemon so signal_daemon_shutdown is harmless.
        let fake_daemon_pid = u32::MAX - 1;

        // State with two clients.
        write_state(
            dir.path(),
            &DaemonState {
                daemon_pid: fake_daemon_pid,
                port: 9090,
                clients: vec![1111, 2222],
                auth_token: "tok".to_string(),
            },
        )
        .unwrap();

        // Deregister first client — list still has one.
        deregister_client(dir.path(), 1111).unwrap();
        let state = read_state(dir.path()).unwrap().unwrap();
        assert_eq!(state.clients, vec![2222]);

        // Deregister second client — list is now empty.
        // signal_daemon_shutdown targets a non-existent PID (harmless).
        deregister_client(dir.path(), 2222).unwrap();
        let state = read_state(dir.path()).unwrap().unwrap();
        assert!(state.clients.is_empty());
    }

    #[test]
    fn deregister_client_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let fake_daemon_pid = u32::MAX - 1;

        // State with one client.
        write_state(
            dir.path(),
            &DaemonState {
                daemon_pid: fake_daemon_pid,
                port: 9090,
                clients: vec![1111],
                auth_token: "tok".to_string(),
            },
        )
        .unwrap();

        // Deregister a PID that's not in the list — no error, 1111 still present.
        deregister_client(dir.path(), 9999).unwrap();
        let state = read_state(dir.path()).unwrap().unwrap();
        assert_eq!(state.clients, vec![1111]);

        // Deregister 1111 — list is now empty.
        deregister_client(dir.path(), 1111).unwrap();
        let state = read_state(dir.path()).unwrap().unwrap();
        assert!(state.clients.is_empty());

        // Deregister 1111 again (already removed) — no error.
        deregister_client(dir.path(), 1111).unwrap();
        let state = read_state(dir.path()).unwrap().unwrap();
        assert!(state.clients.is_empty());
    }

    #[test]
    fn register_client_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let our_pid = std::process::id();

        write_state(
            dir.path(),
            &DaemonState {
                daemon_pid: our_pid,
                port: 9090,
                clients: vec![],
                auth_token: "tok".to_string(),
            },
        )
        .unwrap();

        // Register same PID twice.
        register_client(dir.path(), 1111).unwrap();
        register_client(dir.path(), 1111).unwrap();

        let state = read_state(dir.path()).unwrap().expect("state should exist");
        assert_eq!(state.clients, vec![1111], "PID should appear only once");
    }

    #[test]
    fn ensure_daemon_joins_existing_alive_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let our_pid = std::process::id();

        // Pre-write state with our own PID as daemon (alive).
        write_state(
            dir.path(),
            &DaemonState {
                daemon_pid: our_pid,
                port: 9090,
                clients: vec![],
                auth_token: "tok".to_string(),
            },
        )
        .unwrap();

        let action = ensure_daemon(dir.path(), 9090, "127.0.0.1").unwrap();
        assert_eq!(action, DaemonAction::Joined { port: 9090 });

        // Verify we got registered as a client.
        let state = read_state(dir.path()).unwrap().expect("state should exist");
        assert!(
            state.clients.contains(&our_pid),
            "current process should be registered as a client"
        );
    }

    #[test]
    fn cleanup_stale_state_removes_dead_daemon_state() {
        let dir = tempfile::tempdir().unwrap();

        // Pre-write state with a dead daemon PID.
        write_state(
            dir.path(),
            &DaemonState {
                daemon_pid: u32::MAX - 1, // non-existent PID
                port: 9090,
                clients: vec![100, 200],
                auth_token: "tok".to_string(),
            },
        )
        .unwrap();

        // cleanup_stale_state should remove the dead daemon's state.
        let result = cleanup_stale_state(dir.path()).unwrap();
        assert_eq!(
            result, None,
            "stale state with dead daemon should be removed"
        );

        // State file should be gone.
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn should_shutdown_returns_false_when_clients_present() {
        let state = DaemonState {
            daemon_pid: 1,
            port: 9090,
            clients: vec![100],
            auth_token: "tok".to_string(),
        };
        let mut grace_start = None;
        let result = should_shutdown(&state, &mut grace_start, std::time::Duration::from_secs(60));
        assert!(!result);
        // Grace start should be reset to None when clients are present.
        assert!(grace_start.is_none());
    }

    #[test]
    fn should_shutdown_starts_grace_period_when_clients_empty() {
        let state = DaemonState {
            daemon_pid: 1,
            port: 9090,
            clients: vec![],
            auth_token: "tok".to_string(),
        };
        let mut grace_start = None;
        let result = should_shutdown(&state, &mut grace_start, std::time::Duration::from_secs(60));
        assert!(
            !result,
            "should not shut down immediately -- grace period just started"
        );
        assert!(grace_start.is_some(), "grace period should have started");
    }

    #[test]
    fn should_shutdown_returns_true_after_grace_period_expires() {
        let state = DaemonState {
            daemon_pid: 1,
            port: 9090,
            clients: vec![],
            auth_token: "tok".to_string(),
        };
        // Simulate grace period that started long ago.
        let mut grace_start = Some(std::time::Instant::now() - std::time::Duration::from_secs(120));
        let result = should_shutdown(&state, &mut grace_start, std::time::Duration::from_secs(60));
        assert!(result, "should shut down after grace period expires");
    }

    #[test]
    fn should_shutdown_resets_grace_when_client_rejoins() {
        let empty_state = DaemonState {
            daemon_pid: 1,
            port: 9090,
            clients: vec![],
            auth_token: "tok".to_string(),
        };
        let mut grace_start = None;

        // Start grace period.
        should_shutdown(
            &empty_state,
            &mut grace_start,
            std::time::Duration::from_secs(60),
        );
        assert!(grace_start.is_some());

        // Client rejoins.
        let active_state = DaemonState {
            daemon_pid: 1,
            port: 9090,
            clients: vec![100],
            auth_token: "tok".to_string(),
        };
        should_shutdown(
            &active_state,
            &mut grace_start,
            std::time::Duration::from_secs(60),
        );
        assert!(
            grace_start.is_none(),
            "grace period should be reset when a client rejoins"
        );
    }

    #[test]
    fn daemon_update_serializes_concurrent_access() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();

        // Start with a counter at 0.
        let initial = DaemonState {
            daemon_pid: 1,
            port: 8080,
            clients: vec![],
            auth_token: "tok".to_string(),
        };
        write_state(&data_dir, &initial).unwrap();

        let iterations = 50;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let dir = data_dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..iterations {
                        update_state(&dir, |prev| {
                            let mut s = prev.unwrap_or(DaemonState {
                                daemon_pid: 1,
                                port: 8080,
                                clients: vec![],
                                auth_token: "tok".to_string(),
                            });
                            // Use daemon_pid as a counter.
                            s.daemon_pid += 1;
                            Some(s)
                        })
                        .unwrap();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let final_state = read_state(&data_dir).unwrap().expect("state should exist");
        // 2 threads * 50 iterations = 100 increments from initial 1.
        assert_eq!(
            final_state.daemon_pid,
            1 + (2 * iterations),
            "concurrent updates should be serialized by the lock"
        );
    }
}
