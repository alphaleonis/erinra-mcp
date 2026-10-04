//! Daemon state file management with file locking for coordination.
//!
//! Persists a `web.state` JSON file at `{data_dir}/web.state` describing the
//! state of the shared web daemon. All mutations use advisory file locking via
//! a separate `.web.state.lock` file.
//!
//! # Contract design (unrepresentable half-initialized states)
//!
//! The on-disk `web.state` is a tagged enum ([`WebState`]) with exactly two
//! shapes:
//!
//! - [`WebState::Claiming`] — a `serve`/`dash` caller has reserved the slot and
//!   is spawning the daemon. It carries **no** token by design: a claim is not
//!   usable.
//! - [`WebState::Ready`] — the daemon is listening on `port`; requests queue in
//!   the backlog until it finishes loading and starts serving. This is the
//!   **only** variant that carries a real PID + non-empty auth token together.
//!   Both are wrapped in newtypes ([`DaemonPid`], [`AuthToken`]) whose
//!   constructors and `Deserialize` impls reject the `0` / `""` sentinels, so a
//!   half-written "ready" record can neither be constructed in code nor observed
//!   off disk.
//!
//! Liveness is injected behind the [`PidProbe`] port so coordination logic can
//! be exercised in tests without spawning real processes.
//!
//! **Reset-on-upgrade:** any `web.state` that does not parse as the current
//! [`WebState`] enum (a corrupt file, a legacy flat record from an older binary,
//! or a tampered `0`/`""` record) is treated as [`Discovery::Vacant`] and
//! recreated by the next claim. There is no migration of old files.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, RefreshKind};

/// Maximum age of a [`WebState::Claiming`] record before it is considered stale
/// and reclaimable even if the claimer PID still appears alive. This bounds the
/// window where a daemon stuck mid-startup (e.g. blocked on the ~137 MB model
/// download) blocks a new claim. Generous because model download can be slow.
const CLAIM_MAX_AGE: Duration = Duration::from_secs(120);

/// Result of [`ensure_daemon`]: either spawned a new daemon or joined existing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonAction {
    Spawned { port: u16, daemon_pid: DaemonPid },
    Joined { port: u16, daemon_pid: DaemonPid },
}

impl DaemonAction {
    /// The daemon this process registered with.
    pub fn daemon_pid(&self) -> DaemonPid {
        match self {
            Self::Spawned { daemon_pid, .. } | Self::Joined { daemon_pid, .. } => *daemon_pid,
        }
    }
}

// ---------------------------------------------------------------------------
// Non-sentinel newtypes
// ---------------------------------------------------------------------------

/// A daemon process id that is guaranteed to be non-zero.
///
/// PID `0` is never a real user process and was previously used as the
/// "placeholder, not yet started" sentinel. Forbidding it at the type level
/// means a [`WebState::Ready`] record can never carry an unstarted daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DaemonPid(u32);

impl DaemonPid {
    /// Construct a `DaemonPid`, rejecting `0`.
    pub fn new(pid: u32) -> Option<Self> {
        if pid == 0 { None } else { Some(Self(pid)) }
    }

    /// The underlying PID value (always non-zero).
    pub fn get(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for DaemonPid {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let pid = u32::deserialize(deserializer)?;
        DaemonPid::new(pid).ok_or_else(|| serde::de::Error::custom("daemon_pid must be non-zero"))
    }
}

/// A daemon bearer token that is guaranteed to be non-empty.
///
/// The empty string was previously the "placeholder, not yet set" sentinel.
/// Forbidding it at the type level means a [`WebState::Ready`] record can never
/// carry an unusable token. The [`std::fmt::Debug`] impl is hand-written to
/// **redact** the value so `?state` / `?config` debug logging cannot leak the
/// bearer token.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct AuthToken(String);

impl AuthToken {
    /// Construct an `AuthToken`, rejecting the empty string.
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        if token.is_empty() {
            None
        } else {
            Some(Self(token))
        }
    }

    /// The underlying token string (always non-empty).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner `String`.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the actual token.
        f.write_str("AuthToken(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for AuthToken {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let token = String::deserialize(deserializer)?;
        AuthToken::new(token)
            .ok_or_else(|| serde::de::Error::custom("auth_token must be non-empty"))
    }
}

// ---------------------------------------------------------------------------
// On-disk state
// ---------------------------------------------------------------------------

/// Current epoch time in seconds, used to timestamp claims.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Daemon coordination state persisted to `{data_dir}/web.state`.
///
/// See the module docs for the contract. The `status` tag selects the variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WebState {
    /// The slot is claimed and a daemon is being spawned, but it is **not yet
    /// usable**. Carries no token by design.
    Claiming {
        /// PID of the `serve`/`dash` process that took the claim.
        claimer_pid: u32,
        /// Port the daemon will bind.
        port: u16,
        /// Epoch seconds when the claim was taken (for age-out).
        since: u64,
    },
    /// The daemon is listening on `port` and usable. The **only** variant
    /// carrying a real PID + non-empty token together.
    Ready {
        daemon_pid: DaemonPid,
        port: u16,
        auth_token: AuthToken,
        clients: Vec<u32>,
    },
}

/// A proven-usable daemon: real PID + non-empty token + port together.
///
/// Constructed only by reaping a [`WebState::Ready`] whose daemon is alive, or
/// by [`bind_and_publish`]. The relay path requires a value of this type, so it
/// is structurally impossible to attempt a relay with placeholder creds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyDaemon {
    pub daemon_pid: DaemonPid,
    pub port: u16,
    pub auth_token: AuthToken,
}

impl ReadyDaemon {
    /// Base URL of the daemon's loopback HTTP endpoint.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Path to the state file within a data directory.
fn state_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("web.state")
}

/// Read and parse the current `web.state` **without acquiring the lock**.
///
/// Returns `None` if no state file exists *or* it does not parse as the current
/// [`WebState`] enum (corrupt, legacy flat shape, or tampered sentinel record).
/// This is the reset-on-upgrade behavior: an unparseable file is invisible and
/// will be recreated by the next claim.
///
/// For consistent reads during state mutation, use [`update_state`] instead.
pub fn read_state(data_dir: &Path) -> Result<Option<WebState>> {
    let path = state_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => Ok(Some(state)),
            Err(e) => {
                tracing::warn!(
                    "unparseable daemon state file {} (treating as vacant): {e}",
                    path.display()
                );
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Write daemon state atomically (temp file + rename).
/// Callers must use [`update_state`] to ensure file locking.
fn write_state(data_dir: &Path, state: &WebState) -> Result<()> {
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
///
/// This remains the single mutation primitive; `claim`/`publish`/`discover`
/// express themselves through it under the same `fs2` exclusive lock.
pub fn update_state<F>(data_dir: &Path, f: F) -> Result<Option<WebState>>
where
    F: FnOnce(Option<WebState>) -> Option<WebState>,
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

// ---------------------------------------------------------------------------
// Liveness port
// ---------------------------------------------------------------------------

/// Process-liveness probe, injected so coordination logic can be tested without
/// real processes.
pub trait PidProbe: Send + Sync {
    /// Returns `true` if a process with the given PID currently exists.
    fn is_alive(&self, pid: u32) -> bool;
}

/// Production [`PidProbe`] backed by `sysinfo`.
pub struct SysinfoProbe;

impl PidProbe for SysinfoProbe {
    fn is_alive(&self, pid: u32) -> bool {
        is_pid_alive(pid)
    }
}

/// Check if a process with the given PID is currently alive.
/// Cross-platform via sysinfo.
pub fn is_pid_alive(pid: u32) -> bool {
    let s = sysinfo::System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
    );
    s.process(sysinfo::Pid::from_u32(pid)).is_some()
}

// ---------------------------------------------------------------------------
// Discovery (typed, self-healing)
// ---------------------------------------------------------------------------

/// Typed, exhaustively-matchable result of inspecting `web.state`.
///
/// Replaces the old `Option<DaemonState>` + ad-hoc liveness check. Probing and
/// reaping of stale/dead records happen inside [`discover`], so callers never
/// re-derive liveness or parse placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// A daemon is up and usable.
    Ready(ReadyDaemon),
    /// Someone is mid-spawn; not yet usable. `age` is how long the claim has held.
    Claiming {
        claimer_pid: u32,
        port: u16,
        age: Duration,
    },
    /// No usable or in-progress daemon (no file, unparseable, or a dead record
    /// that was reaped by this call).
    Vacant,
}

/// Compute the discovery decision for an existing on-disk state plus liveness.
///
/// Pure helper (no I/O): given the current `WebState` and a probe, returns the
/// `Discovery` *and* whether the on-disk record should be reaped (deleted).
/// Reaping happens for a dead daemon (`Ready` whose PID is gone) and for an
/// aged-out or dead-claimer `Claiming`.
fn classify(state: &WebState, probe: &dyn PidProbe, now: u64) -> (Discovery, bool) {
    match state {
        WebState::Ready {
            daemon_pid,
            port,
            auth_token,
            ..
        } => {
            if probe.is_alive(daemon_pid.get()) {
                (
                    Discovery::Ready(ReadyDaemon {
                        daemon_pid: *daemon_pid,
                        port: *port,
                        auth_token: auth_token.clone(),
                    }),
                    false,
                )
            } else {
                // Dead daemon -> reap, becomes Vacant.
                (Discovery::Vacant, true)
            }
        }
        WebState::Claiming {
            claimer_pid,
            port,
            since,
        } => {
            let age = Duration::from_secs(now.saturating_sub(*since));
            let claimer_dead = !probe.is_alive(*claimer_pid);
            if claimer_dead || age >= CLAIM_MAX_AGE {
                // Stale claim (claimer gone, or stuck too long) -> reclaimable.
                (Discovery::Vacant, true)
            } else {
                (
                    Discovery::Claiming {
                        claimer_pid: *claimer_pid,
                        port: *port,
                        age,
                    },
                    false,
                )
            }
        }
    }
}

/// Inspect `web.state` and classify the daemon situation, reaping dead/stale
/// records atomically under the lock.
///
/// - `Ready` with a live daemon -> [`Discovery::Ready`].
/// - `Ready` with a dead daemon -> record removed, [`Discovery::Vacant`].
/// - `Claiming` still fresh + claimer alive -> [`Discovery::Claiming`].
/// - `Claiming` aged out or claimer dead -> record removed, [`Discovery::Vacant`].
/// - No file / unparseable -> [`Discovery::Vacant`].
pub fn discover(data_dir: &Path, probe: &dyn PidProbe) -> Result<Discovery> {
    let now = now_unix_secs();
    // Hold the lock across probe + reap so the decision and any deletion are
    // atomic with respect to concurrent claims/publishes.
    let mut decision = Discovery::Vacant;
    update_state(data_dir, |state| match state {
        None => None,
        Some(state) => {
            let (disc, reap) = classify(&state, probe, now);
            decision = disc;
            if reap { None } else { Some(state) }
        }
    })?;
    Ok(decision)
}

// ---------------------------------------------------------------------------
// Claim / publish handshake
// ---------------------------------------------------------------------------

/// Outcome of [`try_claim`].
pub enum ClaimOutcome<'a> {
    /// This caller now owns the claim and must [`DaemonClaim::abandon`] it if the
    /// spawn fails; a spawned daemon resolves it itself.
    Claimed(DaemonClaim<'a>),
    /// A usable daemon already exists; relay to it.
    AlreadyReady(ReadyDaemon),
    /// Someone else holds a fresh claim; this caller should not spawn.
    AlreadyClaiming { claimer_pid: u32, age: Duration },
}

/// An owned, in-progress claim on the daemon slot.
///
/// The spawned daemon resolves it via [`bind_and_publish`]; if the spawn fails,
/// [`abandon`](DaemonClaim::abandon) it. It does not auto-clean on drop.
pub struct DaemonClaim<'a> {
    data_dir: &'a Path,
    claimer_pid: u32,
}

impl<'a> DaemonClaim<'a> {
    /// Release a held claim (spawn failed), removing our `Claiming` record.
    /// Only removes the record if it is still ours.
    pub fn abandon(self) -> Result<()> {
        let claimer_pid = self.claimer_pid;
        update_state(self.data_dir, |state| match &state {
            Some(WebState::Claiming {
                claimer_pid: cur, ..
            }) if *cur == claimer_pid => None,
            other => other.clone(),
        })?;
        Ok(())
    }
}

/// Publish the daemon as [`WebState::Ready`] from the daemon process itself.
///
/// The daemon is a separate process re-exec'd by the claimer, so it cannot hold
/// the claimer's [`DaemonClaim`]. Instead it promotes whatever record exists to
/// `Ready`, preserving any client list:
///
/// - over a `Claiming` record (the expected case) -> `Ready`, clients empty;
/// - over an existing `Ready` (restart/race) -> `Ready` with new creds, clients
///   preserved;
/// - over no record -> `Ready`, clients empty.
///
/// Returns the proven [`ReadyDaemon`].
fn publish_ready(
    data_dir: &Path,
    daemon_pid: DaemonPid,
    port: u16,
    auth_token: AuthToken,
) -> Result<ReadyDaemon> {
    update_state(data_dir, |state| {
        let clients = match state {
            Some(WebState::Ready { clients, .. }) => clients,
            _ => vec![],
        };
        Some(WebState::Ready {
            daemon_pid,
            port,
            auth_token: auth_token.clone(),
            clients,
        })
    })?;
    Ok(ReadyDaemon {
        daemon_pid,
        port,
        auth_token,
    })
}

/// Bind the daemon listener, then publish `Ready` with the bound port.
///
/// Connections that arrive before the caller starts serving wait in the kernel
/// backlog. On bind failure the `Claiming` record for `port` is removed so the
/// spawner's wait ends at its next poll.
pub async fn bind_and_publish(
    data_dir: &Path,
    bind: &str,
    port: u16,
    daemon_pid: DaemonPid,
    auth_token: AuthToken,
) -> Result<(tokio::net::TcpListener, ReadyDaemon)> {
    let listener = match bind_listener(bind, port).await {
        Ok(listener) => listener,
        Err(e) => {
            if let Err(clear_err) = clear_claim_for_port(data_dir, port) {
                tracing::warn!("failed to clear daemon claim after bind failure: {clear_err:#}");
            }
            return Err(e);
        }
    };
    let bound_port = listener.local_addr()?.port();
    let ready = publish_ready(data_dir, daemon_pid, bound_port, auth_token)?;
    Ok((listener, ready))
}

async fn bind_listener(bind: &str, port: u16) -> Result<tokio::net::TcpListener> {
    use anyhow::Context;

    let addr: std::net::SocketAddr = format!("{bind}:{port}")
        .parse()
        .with_context(|| format!("invalid bind address: {bind}:{port}"))?;
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind daemon listener on {addr}"))
}

/// Remove a `Claiming` record for `port`; any other record belongs to a different daemon.
fn clear_claim_for_port(data_dir: &Path, port: u16) -> Result<()> {
    update_state(data_dir, |state| match state {
        Some(WebState::Claiming { port: claimed, .. }) if claimed == port => None,
        other => other,
    })?;
    Ok(())
}

/// Attempt to claim the daemon slot for `claimer_pid` on `port`.
///
/// Reaps dead/stale records first (same rules as [`discover`]), then under one
/// lock decides:
/// - `Ready` + alive -> [`ClaimOutcome::AlreadyReady`].
/// - fresh `Claiming` by someone else -> [`ClaimOutcome::AlreadyClaiming`].
/// - vacant -> writes our `Claiming` and returns [`ClaimOutcome::Claimed`].
pub fn try_claim<'a>(
    data_dir: &'a Path,
    claimer_pid: u32,
    port: u16,
    probe: &dyn PidProbe,
) -> Result<ClaimOutcome<'a>> {
    let now = now_unix_secs();
    let mut outcome_kind = ClaimDecision::Claimed;
    update_state(data_dir, |state| {
        match state {
            None => {
                outcome_kind = ClaimDecision::Claimed;
                Some(WebState::Claiming {
                    claimer_pid,
                    port,
                    since: now,
                })
            }
            Some(existing) => {
                let (disc, reap) = classify(&existing, probe, now);
                match disc {
                    Discovery::Ready(ready) => {
                        outcome_kind = ClaimDecision::AlreadyReady(ready);
                        Some(existing)
                    }
                    Discovery::Claiming {
                        claimer_pid: cur,
                        age,
                        ..
                    } => {
                        outcome_kind = ClaimDecision::AlreadyClaiming {
                            claimer_pid: cur,
                            age,
                        };
                        Some(existing)
                    }
                    Discovery::Vacant => {
                        // reap implied; take the slot ourselves.
                        debug_assert!(reap);
                        outcome_kind = ClaimDecision::Claimed;
                        Some(WebState::Claiming {
                            claimer_pid,
                            port,
                            since: now,
                        })
                    }
                }
            }
        }
    })?;

    Ok(match outcome_kind {
        ClaimDecision::Claimed => ClaimOutcome::Claimed(DaemonClaim {
            data_dir,
            claimer_pid,
        }),
        ClaimDecision::AlreadyReady(ready) => ClaimOutcome::AlreadyReady(ready),
        ClaimDecision::AlreadyClaiming { claimer_pid, age } => {
            ClaimOutcome::AlreadyClaiming { claimer_pid, age }
        }
    })
}

/// Internal: the decision reached inside the `try_claim` lock, carried out of
/// the closure (which cannot itself construct the borrowing `ClaimOutcome`).
enum ClaimDecision {
    Claimed,
    AlreadyReady(ReadyDaemon),
    AlreadyClaiming { claimer_pid: u32, age: Duration },
}

// ---------------------------------------------------------------------------
// Client registration
// ---------------------------------------------------------------------------

/// Register a client PID in a `Ready` daemon state (locked update).
/// If the PID is already present, this is a no-op.
/// Returns an error if there is no `Ready` daemon state file.
pub fn register_client(data_dir: &Path, client_pid: u32) -> Result<()> {
    if !add_client(data_dir, client_pid, None)? {
        anyhow::bail!("cannot register client: no ready daemon state file exists");
    }
    Ok(())
}

/// Register `client_pid` with the specific daemon `ready` describes. Errors if
/// the record is no longer `Ready` for that daemon_pid (vacant, claiming, or replaced).
pub fn register_client_for(data_dir: &Path, ready: &ReadyDaemon, client_pid: u32) -> Result<()> {
    if !add_client(data_dir, client_pid, Some(ready.daemon_pid))? {
        anyhow::bail!(
            "cannot register client: daemon {} is no longer the ready daemon",
            ready.daemon_pid.get()
        );
    }
    Ok(())
}

/// Add `client_pid` to a `Ready` record, restricted to `only_daemon` when given.
/// Returns whether a matching record was found.
fn add_client(data_dir: &Path, client_pid: u32, only_daemon: Option<DaemonPid>) -> Result<bool> {
    let mut registered = false;
    update_state(data_dir, |state| match state {
        Some(WebState::Ready {
            daemon_pid,
            port,
            auth_token,
            mut clients,
        }) if only_daemon.is_none_or(|pid| pid == daemon_pid) => {
            registered = true;
            if !clients.contains(&client_pid) {
                clients.push(client_pid);
            }
            Some(WebState::Ready {
                daemon_pid,
                port,
                auth_token,
                clients,
            })
        }
        other => other,
    })?;
    Ok(registered)
}

/// Deregister a client PID from a `Ready` daemon state (locked update).
/// If this was the last client, sends SIGTERM to the daemon for prompt shutdown
/// instead of waiting for the next sweep cycle.
pub fn deregister_client(data_dir: &Path, client_pid: u32) -> Result<()> {
    let new_state = remove_client(data_dir, client_pid, None)?;

    // If we were the last client, signal the daemon to shut down promptly.
    if let Some(WebState::Ready {
        daemon_pid,
        clients,
        ..
    }) = new_state
        && clients.is_empty()
    {
        signal_daemon_shutdown(daemon_pid.get());
    }

    Ok(())
}

/// Remove `client_pid` from `daemon_pid`'s `Ready` record without signaling it,
/// so the daemon's sweep and grace period decide when it shuts down. A record
/// naming another daemon is left untouched.
pub fn deregister_client_for(
    data_dir: &Path,
    daemon_pid: DaemonPid,
    client_pid: u32,
) -> Result<()> {
    remove_client(data_dir, client_pid, Some(daemon_pid))?;
    Ok(())
}

/// Remove `client_pid` from a `Ready` record, restricted to `only_daemon` when
/// given. Returns the resulting state.
fn remove_client(
    data_dir: &Path,
    client_pid: u32,
    only_daemon: Option<DaemonPid>,
) -> Result<Option<WebState>> {
    update_state(data_dir, |state| match state {
        Some(WebState::Ready {
            daemon_pid,
            port,
            auth_token,
            mut clients,
        }) if only_daemon.is_none_or(|pid| pid == daemon_pid) => {
            clients.retain(|&pid| pid != client_pid);
            Some(WebState::Ready {
                daemon_pid,
                port,
                auth_token,
                clients,
            })
        }
        other => other,
    })
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

// ---------------------------------------------------------------------------
// ensure_daemon (claim -> spawn -> publish handshake)
// ---------------------------------------------------------------------------

/// Ensure a daemon is running. Discovers/claims the slot, spawns if needed,
/// waits for the daemon to publish `Ready`, and registers the calling process
/// as a client. Returns what action was taken and the port.
///
/// Uses the production [`SysinfoProbe`].
pub fn ensure_daemon(data_dir: &Path, port: u16, bind: &str) -> Result<DaemonAction> {
    let probe = SysinfoProbe;
    let our_pid = std::process::id();

    let action = match try_claim(data_dir, our_pid, port, &probe)? {
        ClaimOutcome::AlreadyReady(ready) => DaemonAction::Joined {
            port: ready.port,
            daemon_pid: ready.daemon_pid,
        },
        ClaimOutcome::AlreadyClaiming { .. } => {
            // Someone else is spawning the daemon. Wait for it to become Ready.
            let ready = wait_for_ready(data_dir, &probe)?;
            DaemonAction::Joined {
                port: ready.port,
                daemon_pid: ready.daemon_pid,
            }
        }
        ClaimOutcome::Claimed(claim) => {
            // We own the claim — spawn and wait for the daemon to publish Ready.
            if let Err(e) = spawn_daemon(data_dir, port, bind) {
                let _ = claim.abandon();
                return Err(e);
            }
            // Spawn succeeded: the daemon process publishes `Ready` itself,
            // taking over the record. Let the claim handle fall out of scope
            // here *without* abandoning it (no Drop impl), so the daemon's
            // publish is what resolves the `Claiming` record.
            let _consumed = claim;
            match wait_for_ready(data_dir, &probe) {
                Ok(ready) => DaemonAction::Spawned {
                    port: ready.port,
                    daemon_pid: ready.daemon_pid,
                },
                Err(e) => {
                    // Daemon failed to come up. Clean up any claim/record we left
                    // behind and surface a helpful error.
                    let _ = update_state(data_dir, |state| match state {
                        Some(WebState::Claiming {
                            claimer_pid: cur, ..
                        }) if cur == our_pid => None,
                        other => other,
                    });
                    return Err(start_failure_error(data_dir).context(e));
                }
            }
        }
    };

    // Register ourselves as a client (state is Ready at this point).
    register_client(data_dir, our_pid)?;

    Ok(action)
}

/// Build a helpful "daemon failed to start" error, including the last daemon.log
/// line if available.
fn start_failure_error(data_dir: &Path) -> anyhow::Error {
    let log_path = data_dir.join("daemon.log");
    let hint = if log_path.exists() {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        log.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| format!("\n  Last log line: {l}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    anyhow::anyhow!("daemon failed to start. Check {}{hint}", log_path.display())
}

/// Poll until the daemon publishes a `Ready` record (or until timeout).
///
/// Uses the real-time poll loop only here (the coordination decisions are pure
/// and tested without sleeps). Returns the proven [`ReadyDaemon`].
fn wait_for_ready(data_dir: &Path, probe: &dyn PidProbe) -> Result<ReadyDaemon> {
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        match discover(data_dir, probe)? {
            Discovery::Ready(ready) => return Ok(ready),
            // Still spawning — keep waiting.
            Discovery::Claiming { .. } => continue,
            // Record vanished (reaped dead claimer/daemon) — give up.
            Discovery::Vacant => break,
        }
    }
    Err(start_failure_error(data_dir))
}

/// Spawn the daemon as a detached background process by re-executing the current
/// binary with `erinra _daemon --port N --bind ADDR --data-dir PATH`.
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

// ---------------------------------------------------------------------------
// Daemon-side helpers
// ---------------------------------------------------------------------------

/// Determine whether the daemon should shut down based on client activity.
///
/// When the client list is empty, a grace period starts. If the list remains
/// empty for the full grace duration, returns `true`. If a client appears before
/// the grace period expires, the timer resets.
///
/// This is a pure function suitable for unit testing — the caller manages the
/// `grace_start` state across sweep iterations.
pub fn should_shutdown(
    clients: &[u32],
    grace_start: &mut Option<std::time::Instant>,
    grace_period: std::time::Duration,
) -> bool {
    if clients.is_empty() {
        let start = grace_start.get_or_insert_with(std::time::Instant::now);
        start.elapsed() >= grace_period
    } else {
        *grace_start = None;
        false
    }
}

/// Clean up stale daemon state caused by crashed processes, and report the live
/// client list of a healthy daemon.
///
/// - No state file / unparseable: returns `None`.
/// - `Ready` with a dead daemon PID: removes state file, returns `None`.
/// - `Claiming` (any): left untouched by the sweep (publish/abandon owns it),
///   returns `None` (no client list to report).
/// - `Ready` with a live daemon: sweeps dead client PIDs, writes back the
///   cleaned state, returns `Some(clients)`.
pub fn cleanup_stale_state(data_dir: &Path, probe: &dyn PidProbe) -> Result<Option<Vec<u32>>> {
    let mut live_clients: Option<Vec<u32>> = None;
    update_state(data_dir, |state| match state {
        None => None,
        // A `Claiming` record belongs to a `serve`/`dash` mid-spawn; the daemon
        // sweep must not delete a foreign claim, so preserve it untouched.
        claiming @ Some(WebState::Claiming { .. }) => claiming,
        Some(WebState::Ready {
            daemon_pid,
            port,
            auth_token,
            mut clients,
        }) => {
            if !probe.is_alive(daemon_pid.get()) {
                // Dead daemon — remove everything.
                None
            } else {
                clients.retain(|&pid| probe.is_alive(pid));
                live_clients = Some(clients.clone());
                Some(WebState::Ready {
                    daemon_pid,
                    port,
                    auth_token,
                    clients,
                })
            }
        }
    })?;
    Ok(live_clients)
}

// ---------------------------------------------------------------------------
// Startup-mode resolver (pure) + typed relay outcome
// ---------------------------------------------------------------------------

/// The role a `serve` process resolves to at startup.
#[derive(Debug, PartialEq, Eq)]
pub enum StartupMode {
    /// A usable daemon exists; bridge stdio to it.
    Relay(ReadyDaemon),
    /// Run the MCP server in-process. `spawn_web` indicates whether the caller
    /// also asked to bring up the web dashboard daemon (`--web`).
    Standalone { spawn_web: bool },
}

/// Resolve the startup role from a [`Discovery`] and whether `--web` was passed.
///
/// PURE and TOTAL: no filesystem, no liveness probe, no sleep, no spawn. The
/// only way to reach [`StartupMode::Relay`] is a [`Discovery::Ready`]; a
/// `Claiming` row is **never** resolved to `Relay` (it is not usable yet, so the
/// caller proceeds standalone and — if it wanted web — joins/spawns later).
pub fn resolve_startup_mode(disc: Discovery, web_requested: bool) -> StartupMode {
    match disc {
        Discovery::Ready(ready) => StartupMode::Relay(ready),
        Discovery::Claiming { .. } | Discovery::Vacant => StartupMode::Standalone {
            spawn_web: web_requested,
        },
    }
}

/// Outcome of attempting the relay — the variants encode *who has spoken to the
/// MCP client*, which is what determines whether a fresh standalone fallback is
/// safe.
#[derive(Debug)]
pub enum RelayOutcome {
    /// Clean EOF on stdin; the relay finished normally.
    Completed,
    /// The relay failed before the daemon produced any output (e.g. connection
    /// refused). The client has not yet received a response from the daemon, so
    /// it is safe to fall back to a fresh standalone server.
    FailedBeforeFirstByte(anyhow::Error),
    /// The relay failed *after* at least one byte was written to stdout. The MCP
    /// client has already begun a session with the daemon and will not
    /// re-initialize with a new server, so the caller MUST fail loudly and never
    /// silently restart.
    FailedMidSession(anyhow::Error),
}

/// A writer wrapper that records whether any byte has been written through it.
///
/// Used to distinguish a pre-first-byte relay failure (safe to fall back) from a
/// mid-session failure (must fail loudly).
struct FirstByteTracker<W> {
    inner: W,
    wrote: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for FirstByteTracker<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let res = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &res
            && *n > 0
        {
            self.wrote.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        res
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Run the relay against a proven [`ReadyDaemon`], bridging process stdin/stdout
/// to the daemon's `/mcp` endpoint as a registered client, and classify the outcome.
///
/// The `FailedBeforeFirstByte` / `FailedMidSession` distinction is what makes the
/// silent-restart footgun structurally impossible: the caller can only fall back
/// to standalone in the pre-byte case.
pub async fn run_relay_mode(data_dir: &Path, ready: &ReadyDaemon) -> RelayOutcome {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    run_registered_relay(data_dir, ready, std::process::id(), stdin, stdout).await
}

/// Register → relay → deregister, so the daemon's sweep counts this session as a
/// live client. A failed registration returns `FailedBeforeFirstByte` without
/// reading stdin.
pub async fn run_registered_relay<R, W>(
    data_dir: &Path,
    ready: &ReadyDaemon,
    client_pid: u32,
    reader: R,
    writer: W,
) -> RelayOutcome
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    if let Err(e) = register_client_for(data_dir, ready, client_pid) {
        return RelayOutcome::FailedBeforeFirstByte(e);
    }
    let outcome = run_relay_mode_with(ready, reader, writer).await;
    // Never signal: a relay may leave while the daemon is still starting, or
    // just before another client joins.
    if let Err(e) = deregister_client_for(data_dir, ready.daemon_pid, client_pid) {
        tracing::warn!("failed to deregister relay client: {e:#}");
    }
    outcome
}

/// Inner form of [`run_relay_mode`] generic over reader/writer, for testing with
/// `DuplexStream`s.
pub async fn run_relay_mode_with<R, W>(ready: &ReadyDaemon, reader: R, writer: W) -> RelayOutcome
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let wrote = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tracked = FirstByteTracker {
        inner: writer,
        wrote: wrote.clone(),
    };

    match crate::relay::run_relay(reader, tracked, ready).await {
        Ok(()) => RelayOutcome::Completed,
        Err(e) => {
            if wrote.load(std::sync::atomic::Ordering::SeqCst) {
                RelayOutcome::FailedMidSession(e)
            } else {
                RelayOutcome::FailedBeforeFirstByte(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Test [`PidProbe`] with an explicit set of "alive" PIDs.
    struct FakeProbe {
        alive: HashSet<u32>,
    }

    impl FakeProbe {
        fn with(pids: &[u32]) -> Self {
            Self {
                alive: pids.iter().copied().collect(),
            }
        }

        fn none() -> Self {
            Self {
                alive: HashSet::new(),
            }
        }
    }

    impl PidProbe for FakeProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }
    }

    fn ready_state(daemon_pid: u32, port: u16, token: &str, clients: Vec<u32>) -> WebState {
        WebState::Ready {
            daemon_pid: DaemonPid::new(daemon_pid).unwrap(),
            port,
            auth_token: AuthToken::new(token).unwrap(),
            clients,
        }
    }

    fn ready_daemon(daemon_pid: u32, port: u16, token: &str) -> ReadyDaemon {
        ReadyDaemon {
            daemon_pid: DaemonPid::new(daemon_pid).unwrap(),
            port,
            auth_token: AuthToken::new(token).unwrap(),
        }
    }

    // ---- Newtype unrepresentability --------------------------------------

    #[test]
    fn daemon_pid_rejects_zero() {
        assert_eq!(DaemonPid::new(0), None);
        assert_eq!(DaemonPid::new(1).map(|p| p.get()), Some(1));
    }

    #[test]
    fn auth_token_rejects_empty() {
        assert!(AuthToken::new("").is_none());
        assert_eq!(
            AuthToken::new("tok").map(|t| t.as_str().to_string()),
            Some("tok".to_string())
        );
    }

    #[test]
    fn auth_token_debug_redacts_value() {
        let tok = AuthToken::new("super-secret-token").unwrap();
        let dbg = format!("{tok:?}");
        assert!(
            !dbg.contains("super-secret-token"),
            "Debug must not leak token: {dbg}"
        );
        assert!(
            dbg.contains("redacted"),
            "Debug should indicate redaction: {dbg}"
        );
    }

    #[test]
    fn ready_with_zero_pid_does_not_deserialize() {
        let json =
            r#"{"status":"ready","daemon_pid":0,"port":9090,"auth_token":"tok","clients":[]}"#;
        let parsed: std::result::Result<WebState, _> = serde_json::from_str(json);
        assert!(
            parsed.is_err(),
            "daemon_pid:0 must not deserialize as Ready"
        );
    }

    #[test]
    fn ready_with_empty_token_does_not_deserialize() {
        let json =
            r#"{"status":"ready","daemon_pid":1234,"port":9090,"auth_token":"","clients":[]}"#;
        let parsed: std::result::Result<WebState, _> = serde_json::from_str(json);
        assert!(
            parsed.is_err(),
            "auth_token:\"\" must not deserialize as Ready"
        );
    }

    #[test]
    fn discover_treats_zero_pid_record_as_vacant() {
        let dir = tempfile::tempdir().unwrap();
        // Write a tampered record by hand (bypassing the typed write path).
        std::fs::write(
            dir.path().join("web.state"),
            r#"{"status":"ready","daemon_pid":0,"port":9090,"auth_token":"tok","clients":[]}"#,
        )
        .unwrap();
        // PID 0 would never be alive anyway, but the point is it never reaches a probe.
        let disc = discover(dir.path(), &FakeProbe::none()).unwrap();
        assert_eq!(
            disc,
            Discovery::Vacant,
            "tampered 0-pid record must be Vacant, not fake-ready"
        );
    }

    #[test]
    fn discover_treats_empty_token_record_as_vacant() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("web.state"),
            r#"{"status":"ready","daemon_pid":1234,"port":9090,"auth_token":"","clients":[]}"#,
        )
        .unwrap();
        let disc = discover(dir.path(), &FakeProbe::with(&[1234])).unwrap();
        assert_eq!(
            disc,
            Discovery::Vacant,
            "empty-token record must be Vacant, not fake-ready"
        );
    }

    #[test]
    fn read_state_treats_legacy_flat_record_as_none() {
        let dir = tempfile::tempdir().unwrap();
        // The old flat DaemonState shape (no "status" tag).
        std::fs::write(
            dir.path().join("web.state"),
            r#"{"daemon_pid":1234,"port":9090,"clients":[],"auth_token":"tok"}"#,
        )
        .unwrap();
        assert_eq!(
            read_state(dir.path()).unwrap(),
            None,
            "legacy flat record is unparseable -> None"
        );
    }

    // ---- State round-trip / update_state ---------------------------------

    #[test]
    fn write_and_read_round_trip_ready() {
        let dir = tempfile::tempdir().unwrap();
        let state = ready_state(1234, 9090, "tok", vec![5678, 9012]);
        write_state(dir.path(), &state).unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), Some(state));
    }

    #[test]
    fn write_and_read_round_trip_claiming() {
        let dir = tempfile::tempdir().unwrap();
        let state = WebState::Claiming {
            claimer_pid: 4321,
            port: 9090,
            since: 100,
        };
        write_state(dir.path(), &state).unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), Some(state));
    }

    #[test]
    fn write_is_atomic_no_temp_file_remains() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &ready_state(1234, 9090, "tok", vec![])).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["web.state"]);
    }

    #[test]
    fn read_returns_none_when_no_state_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn read_returns_none_for_corrupt_state_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("web.state"), b"not json at all").unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn update_can_create_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let created = update_state(dir.path(), |prev| {
            assert_eq!(prev, None);
            Some(ready_state(1, 8080, "tok", vec![]))
        })
        .unwrap();
        assert!(created.is_some());
        let deleted = update_state(dir.path(), |prev| {
            assert!(prev.is_some());
            None
        })
        .unwrap();
        assert_eq!(deleted, None);
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn update_serializes_concurrent_access() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();
        // Counter encoded in the clients vec length.
        write_state(&data_dir, &ready_state(1, 8080, "tok", vec![])).unwrap();

        let iterations = 50;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|i| {
                let dir = data_dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for j in 0..iterations {
                        update_state(&dir, |prev| {
                            let mut s = prev.unwrap_or_else(|| ready_state(1, 8080, "tok", vec![]));
                            if let WebState::Ready { clients, .. } = &mut s {
                                clients.push(i * 1000 + j);
                            }
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
        let final_state = read_state(&data_dir).unwrap().unwrap();
        if let WebState::Ready { clients, .. } = final_state {
            assert_eq!(
                clients.len(),
                2 * iterations as usize,
                "concurrent updates must be serialized"
            );
        } else {
            panic!("expected Ready");
        }
    }

    // ---- Resolver matrix (pure, no I/O) ----------------------------------

    #[test]
    fn resolver_ready_resolves_to_relay_regardless_of_web_flag() {
        let ready = ready_daemon(1234, 9090, "tok");
        assert_eq!(
            resolve_startup_mode(Discovery::Ready(ready.clone()), false),
            StartupMode::Relay(ready.clone())
        );
        assert_eq!(
            resolve_startup_mode(Discovery::Ready(ready.clone()), true),
            StartupMode::Relay(ready)
        );
    }

    #[test]
    fn resolver_vacant_resolves_to_standalone_carrying_web_flag() {
        assert_eq!(
            resolve_startup_mode(Discovery::Vacant, false),
            StartupMode::Standalone { spawn_web: false }
        );
        assert_eq!(
            resolve_startup_mode(Discovery::Vacant, true),
            StartupMode::Standalone { spawn_web: true }
        );
    }

    #[test]
    fn resolver_claiming_never_resolves_to_relay() {
        for web in [false, true] {
            let mode = resolve_startup_mode(
                Discovery::Claiming {
                    claimer_pid: 4321,
                    port: 9090,
                    age: Duration::from_secs(1),
                },
                web,
            );
            assert_eq!(
                mode,
                StartupMode::Standalone { spawn_web: web },
                "Claiming must never become Relay (web={web})"
            );
        }
    }

    // ---- discover --------------------------------------------------------

    #[test]
    fn discover_vacant_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            discover(dir.path(), &FakeProbe::none()).unwrap(),
            Discovery::Vacant
        );
    }

    #[test]
    fn discover_ready_when_daemon_alive() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &ready_state(1234, 9090, "tok", vec![])).unwrap();
        let disc = discover(dir.path(), &FakeProbe::with(&[1234])).unwrap();
        assert_eq!(disc, Discovery::Ready(ready_daemon(1234, 9090, "tok")));
        // Record still present.
        assert!(read_state(dir.path()).unwrap().is_some());
    }

    #[test]
    fn discover_reaps_ready_with_dead_daemon() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &ready_state(1234, 9090, "tok", vec![])).unwrap();
        let disc = discover(dir.path(), &FakeProbe::none()).unwrap();
        assert_eq!(disc, Discovery::Vacant);
        assert_eq!(
            read_state(dir.path()).unwrap(),
            None,
            "dead Ready record must be reaped"
        );
    }

    #[test]
    fn discover_claiming_when_claimer_alive_and_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let since = now_unix_secs();
        write_state(
            dir.path(),
            &WebState::Claiming {
                claimer_pid: 4321,
                port: 9090,
                since,
            },
        )
        .unwrap();
        let disc = discover(dir.path(), &FakeProbe::with(&[4321])).unwrap();
        match disc {
            Discovery::Claiming {
                claimer_pid, port, ..
            } => {
                assert_eq!(claimer_pid, 4321);
                assert_eq!(port, 9090);
            }
            other => panic!("expected Claiming, got {other:?}"),
        }
        assert!(
            read_state(dir.path()).unwrap().is_some(),
            "fresh claim must be preserved"
        );
    }

    #[test]
    fn discover_reaps_claiming_with_dead_claimer() {
        let dir = tempfile::tempdir().unwrap();
        write_state(
            dir.path(),
            &WebState::Claiming {
                claimer_pid: 4321,
                port: 9090,
                since: now_unix_secs(),
            },
        )
        .unwrap();
        let disc = discover(dir.path(), &FakeProbe::none()).unwrap();
        assert_eq!(disc, Discovery::Vacant, "dead-claimer claim is reclaimable");
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn discover_reaps_aged_out_claiming_even_if_claimer_alive() {
        let dir = tempfile::tempdir().unwrap();
        let stale_since = now_unix_secs().saturating_sub(CLAIM_MAX_AGE.as_secs() + 10);
        write_state(
            dir.path(),
            &WebState::Claiming {
                claimer_pid: 4321,
                port: 9090,
                since: stale_since,
            },
        )
        .unwrap();
        let disc = discover(dir.path(), &FakeProbe::with(&[4321])).unwrap();
        assert_eq!(
            disc,
            Discovery::Vacant,
            "aged-out claim is reclaimable even with live claimer"
        );
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    // ---- Claim handshake -------------------------------------------------

    #[test]
    fn try_claim_on_empty_returns_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let probe = FakeProbe::with(&[100]);
        match try_claim(dir.path(), 100, 9090, &probe).unwrap() {
            ClaimOutcome::Claimed(_) => {}
            _ => panic!("empty slot should be Claimed"),
        }
        // A Claiming record should now be on disk.
        match read_state(dir.path()).unwrap() {
            Some(WebState::Claiming {
                claimer_pid, port, ..
            }) => {
                assert_eq!(claimer_pid, 100);
                assert_eq!(port, 9090);
            }
            other => panic!("expected Claiming on disk, got {other:?}"),
        }
    }

    #[test]
    fn try_claim_second_caller_sees_already_claiming() {
        let dir = tempfile::tempdir().unwrap();
        let probe = FakeProbe::with(&[100, 200]);
        let _first = match try_claim(dir.path(), 100, 9090, &probe).unwrap() {
            ClaimOutcome::Claimed(c) => c,
            _ => panic!("first caller should claim"),
        };
        match try_claim(dir.path(), 200, 9090, &probe).unwrap() {
            ClaimOutcome::AlreadyClaiming { claimer_pid, .. } => {
                assert_eq!(claimer_pid, 100, "second caller sees first claimer");
            }
            _ => panic!("second caller should see AlreadyClaiming"),
        }
    }

    #[test]
    fn try_claim_over_live_ready_returns_already_ready() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &ready_state(555, 9090, "daemon-tok", vec![])).unwrap();
        match try_claim(dir.path(), 200, 9090, &FakeProbe::with(&[200, 555])).unwrap() {
            ClaimOutcome::AlreadyReady(r) => assert_eq!(r, ready_daemon(555, 9090, "daemon-tok")),
            _ => panic!("a live Ready record should yield AlreadyReady"),
        }
    }

    #[test]
    fn abandon_removes_the_claim() {
        let dir = tempfile::tempdir().unwrap();
        let probe = FakeProbe::with(&[100]);
        let claim = match try_claim(dir.path(), 100, 9090, &probe).unwrap() {
            ClaimOutcome::Claimed(c) => c,
            _ => panic!("should claim"),
        };
        claim.abandon().unwrap();
        assert_eq!(
            read_state(dir.path()).unwrap(),
            None,
            "abandon removes the claim"
        );
        // Slot is reclaimable.
        match try_claim(dir.path(), 200, 9090, &FakeProbe::with(&[200])).unwrap() {
            ClaimOutcome::Claimed(_) => {}
            _ => panic!("after abandon the slot should be claimable again"),
        }
    }

    #[test]
    fn try_claim_reclaims_dead_claimer_slot() {
        let dir = tempfile::tempdir().unwrap();
        // Existing claim by a now-dead claimer.
        write_state(
            dir.path(),
            &WebState::Claiming {
                claimer_pid: 999,
                port: 9090,
                since: now_unix_secs(),
            },
        )
        .unwrap();
        // 999 is dead; 100 is alive.
        match try_claim(dir.path(), 100, 9090, &FakeProbe::with(&[100])).unwrap() {
            ClaimOutcome::Claimed(_) => {}
            _ => panic!("dead-claimer slot should be reclaimable"),
        }
        match read_state(dir.path()).unwrap() {
            Some(WebState::Claiming { claimer_pid, .. }) => assert_eq!(claimer_pid, 100),
            other => panic!("expected our Claiming, got {other:?}"),
        }
    }

    #[test]
    fn try_claim_over_dead_ready_reclaims_slot() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &ready_state(999, 9090, "old", vec![])).unwrap();
        // Daemon 999 is dead; claimer 100 alive.
        match try_claim(dir.path(), 100, 9090, &FakeProbe::with(&[100])).unwrap() {
            ClaimOutcome::Claimed(_) => {}
            _ => panic!("dead Ready daemon slot should be reclaimable"),
        }
    }

    // ---- register / deregister ------------------------------------------

    #[test]
    fn register_client_requires_ready_state() {
        let dir = tempfile::tempdir().unwrap();
        // No state -> error.
        assert!(register_client(dir.path(), 1111).is_err());
        // Claiming -> error (not yet usable).
        write_state(
            dir.path(),
            &WebState::Claiming {
                claimer_pid: 100,
                port: 9090,
                since: now_unix_secs(),
            },
        )
        .unwrap();
        assert!(register_client(dir.path(), 1111).is_err());
    }

    #[test]
    fn register_and_deregister_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // Dead daemon PID so deregister's shutdown signal is harmless.
        write_state(
            dir.path(),
            &ready_state(DEAD_DAEMON_PID, 9090, "tok", vec![]),
        )
        .unwrap();

        register_client(dir.path(), 1111).unwrap();
        register_client(dir.path(), 1111).unwrap(); // idempotent
        match read_state(dir.path()).unwrap().unwrap() {
            WebState::Ready { clients, .. } => assert_eq!(clients, vec![1111]),
            _ => panic!("expected Ready"),
        }

        deregister_client(dir.path(), 1111).unwrap();
        match read_state(dir.path()).unwrap().unwrap() {
            WebState::Ready { clients, .. } => assert!(clients.is_empty()),
            _ => panic!("expected Ready"),
        }
        // Idempotent deregister.
        deregister_client(dir.path(), 1111).unwrap();
    }

    // ---- cleanup_stale_state --------------------------------------------

    #[test]
    fn cleanup_returns_none_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            cleanup_stale_state(dir.path(), &FakeProbe::none()).unwrap(),
            None
        );
    }

    #[test]
    fn cleanup_removes_dead_daemon_record() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &ready_state(1234, 9090, "tok", vec![100, 200])).unwrap();
        let result = cleanup_stale_state(dir.path(), &FakeProbe::none()).unwrap();
        assert_eq!(result, None);
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }

    #[test]
    fn cleanup_sweeps_dead_clients_of_live_daemon() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &ready_state(1234, 9090, "tok", vec![100, 200])).unwrap();
        // Daemon + client 100 alive; 200 dead.
        let result = cleanup_stale_state(dir.path(), &FakeProbe::with(&[1234, 100])).unwrap();
        assert_eq!(result, Some(vec![100]));
        match read_state(dir.path()).unwrap().unwrap() {
            WebState::Ready { clients, .. } => assert_eq!(clients, vec![100]),
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn cleanup_preserves_foreign_claiming_record() {
        let dir = tempfile::tempdir().unwrap();
        let claim = WebState::Claiming {
            claimer_pid: 4321,
            port: 9090,
            since: now_unix_secs(),
        };
        write_state(dir.path(), &claim).unwrap();
        // Sweep must not delete a claim it doesn't own.
        let result = cleanup_stale_state(dir.path(), &FakeProbe::with(&[4321])).unwrap();
        assert_eq!(result, None, "Claiming yields no client list");
        assert_eq!(
            read_state(dir.path()).unwrap(),
            Some(claim),
            "claim preserved"
        );
    }

    // ---- should_shutdown -------------------------------------------------

    #[test]
    fn should_shutdown_false_with_clients_resets_grace() {
        let mut grace = Some(std::time::Instant::now());
        assert!(!should_shutdown(
            &[100],
            &mut grace,
            Duration::from_secs(60)
        ));
        assert!(grace.is_none());
    }

    #[test]
    fn should_shutdown_starts_grace_when_empty() {
        let mut grace = None;
        assert!(!should_shutdown(&[], &mut grace, Duration::from_secs(60)));
        assert!(grace.is_some());
    }

    #[test]
    fn should_shutdown_true_after_grace_expires() {
        let mut grace = Some(std::time::Instant::now() - Duration::from_secs(120));
        assert!(should_shutdown(&[], &mut grace, Duration::from_secs(60)));
    }

    // ---- ensure_daemon join path (no spawn) ------------------------------

    #[test]
    fn ensure_daemon_joins_existing_alive_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let our_pid = std::process::id();
        // Our own PID is the daemon (alive via real SysinfoProbe inside ensure_daemon).
        write_state(dir.path(), &ready_state(our_pid, 9090, "tok", vec![])).unwrap();

        let action = ensure_daemon(dir.path(), 9090, "127.0.0.1").unwrap();
        assert_eq!(
            action,
            DaemonAction::Joined {
                port: 9090,
                daemon_pid: DaemonPid::new(our_pid).unwrap(),
            }
        );
        match read_state(dir.path()).unwrap().unwrap() {
            WebState::Ready { clients, .. } => assert!(clients.contains(&our_pid)),
            _ => panic!("expected Ready"),
        }
    }

    // ---- publish_ready (daemon-side) ------------------------------------

    #[test]
    fn publish_ready_promotes_claiming_and_preserves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_state(
            dir.path(),
            &WebState::Claiming {
                claimer_pid: 100,
                port: 9090,
                since: now_unix_secs(),
            },
        )
        .unwrap();
        let ready = publish_ready(
            dir.path(),
            DaemonPid::new(555).unwrap(),
            9090,
            AuthToken::new("tok").unwrap(),
        )
        .unwrap();
        assert_eq!(ready, ready_daemon(555, 9090, "tok"));
        match read_state(dir.path()).unwrap().unwrap() {
            WebState::Ready {
                daemon_pid,
                clients,
                ..
            } => {
                assert_eq!(daemon_pid.get(), 555);
                assert!(clients.is_empty());
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn publish_ready_preserves_clients_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        write_state(dir.path(), &ready_state(111, 9090, "old", vec![7, 8])).unwrap();
        publish_ready(
            dir.path(),
            DaemonPid::new(222).unwrap(),
            9090,
            AuthToken::new("new").unwrap(),
        )
        .unwrap();
        match read_state(dir.path()).unwrap().unwrap() {
            WebState::Ready {
                daemon_pid,
                auth_token,
                clients,
                ..
            } => {
                assert_eq!(daemon_pid.get(), 222);
                assert_eq!(auth_token.as_str(), "new");
                assert_eq!(clients, vec![7, 8], "client list preserved across restart");
            }
            _ => panic!("expected Ready"),
        }
    }

    // ---- Relay footgun: pre-byte vs mid-session --------------------------

    use crate::db::{Database, DbConfig};
    use crate::embedding::MockEmbedder;
    use crate::service::{MemoryService, ServiceConfig};
    use crate::web::AppState;

    /// Start a real Axum server bound to an ephemeral port; returns a handle that
    /// can be aborted to simulate the daemon dying, plus a `ReadyDaemon`.
    async fn start_killable_server(
        token: &str,
        daemon_pid: u32,
    ) -> (tokio::task::JoinHandle<()>, ReadyDaemon) {
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let service = MemoryService::new(
            Arc::new(Mutex::new(db)),
            Arc::new(MockEmbedder::new(768)),
            None,
            ServiceConfig::default(),
        );
        let state = AppState {
            service,
            auth_token: token.to_string(),
        };
        let app = crate::web::app_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (handle, ready_daemon(daemon_pid, port, token))
    }

    #[tokio::test]
    async fn run_relay_mode_returns_failed_before_first_byte_on_connection_refused() {
        // Point at a port nobody is listening on; the very first POST fails
        // before any byte reaches stdout -> safe fallback.
        let ready = ready_daemon(std::process::id(), 1, "tok");

        // stdin carries a single request then EOF; stdout is a sink.
        let (mut stdin_w, stdin_r) = tokio::io::duplex(8192);
        let (stdout_w, _stdout_r) = tokio::io::duplex(8192);

        use tokio::io::AsyncWriteExt;
        stdin_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
            .await
            .unwrap();
        stdin_w.flush().await.unwrap();
        drop(stdin_w);

        let outcome =
            run_relay_mode_with(&ready, tokio::io::BufReader::new(stdin_r), stdout_w).await;
        match outcome {
            RelayOutcome::FailedBeforeFirstByte(_) => {}
            other => panic!("expected FailedBeforeFirstByte, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_relay_mode_returns_failed_mid_session_when_server_dies_after_first_response() {
        let (server, ready) = start_killable_server("mid-tok", DEAD_DAEMON_PID).await;

        // Manual duplex plumbing so we can drive requests and read responses.
        let (mut stdin_w, stdin_r) = tokio::io::duplex(8192);
        let (stdout_w, stdout_r) = tokio::io::duplex(8192);
        let mut stdout_r = tokio::io::BufReader::new(stdout_r);

        let ready_c = ready.clone();
        let relay = tokio::spawn(async move {
            run_relay_mode_with(&ready_c, tokio::io::BufReader::new(stdin_r), stdout_w).await
        });

        // First request succeeds -> at least one byte written to stdout.
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        stdin_w.write_all(INITIALIZE_LINE).await.unwrap();
        stdin_w.flush().await.unwrap();
        let mut first = String::new();
        stdout_r.read_line(&mut first).await.unwrap();
        assert!(!first.is_empty(), "should have received first response");

        // Now kill the server, then send a second request -> send() fails after
        // a byte was already written -> mid-session.
        server.abort();
        let _ = server.await;
        // Give the OS a moment to drop the listener.
        tokio::task::yield_now().await;
        stdin_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        stdin_w.flush().await.unwrap();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(35), relay)
            .await
            .expect("relay should finish")
            .expect("relay task should not panic");
        match outcome {
            RelayOutcome::FailedMidSession(_) => {}
            other => panic!("expected FailedMidSession, got {other:?}"),
        }
    }

    // ---- Relay sessions register as daemon clients -----------------------

    /// Positive as `i32` and above Linux `pid_max`, so a shutdown signal sent to
    /// it reaches no process (never the test runner or a process group).
    const DEAD_DAEMON_PID: u32 = 999_999_999;
    const RELAY_CLIENT_PID: u32 = 4242;

    fn clients_in_state(data_dir: &Path) -> Option<Vec<u32>> {
        match read_state(data_dir).unwrap() {
            Some(WebState::Ready { clients, .. }) => Some(clients),
            _ => None,
        }
    }

    async fn wait_for_clients(data_dir: &Path, expected: &[u32]) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while clients_in_state(data_dir).as_deref() != Some(expected) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "clients never became {expected:?}, last: {:?}",
                clients_in_state(data_dir)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Publish `ready` as the `Ready` record with no clients.
    fn write_ready_record(data_dir: &Path, ready: &ReadyDaemon) {
        let state = ready_state(
            ready.daemon_pid.get(),
            ready.port,
            ready.auth_token.as_str(),
            vec![],
        );
        write_state(data_dir, &state).unwrap();
    }

    /// Spawn `run_registered_relay` as `RELAY_CLIENT_PID` over duplex stdio;
    /// returns the stdin writer, the stdout reader, and the relay task.
    fn spawn_registered_relay(
        data_dir: &Path,
        ready: &ReadyDaemon,
    ) -> (
        tokio::io::DuplexStream,
        tokio::io::BufReader<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<RelayOutcome>,
    ) {
        let (stdin_w, stdin_r) = tokio::io::duplex(8192);
        let (stdout_w, stdout_r) = tokio::io::duplex(8192);
        let data_dir = data_dir.to_path_buf();
        let ready = ready.clone();
        let relay = tokio::spawn(async move {
            run_registered_relay(
                &data_dir,
                &ready,
                RELAY_CLIENT_PID,
                tokio::io::BufReader::new(stdin_r),
                stdout_w,
            )
            .await
        });
        (stdin_w, tokio::io::BufReader::new(stdout_r), relay)
    }

    #[tokio::test]
    async fn open_relay_session_keeps_daemon_alive_in_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ready) = start_killable_server("reg-tok", DEAD_DAEMON_PID).await;
        write_ready_record(dir.path(), &ready);
        let (stdin_w, _stdout_r, relay) = spawn_registered_relay(dir.path(), &ready);

        wait_for_clients(dir.path(), &[RELAY_CLIENT_PID]).await;
        let probe = FakeProbe::with(&[DEAD_DAEMON_PID, RELAY_CLIENT_PID]);
        let clients = cleanup_stale_state(dir.path(), &probe).unwrap().unwrap();
        assert_eq!(clients, vec![RELAY_CLIENT_PID]);
        let mut grace = None;
        assert!(!should_shutdown(
            &clients,
            &mut grace,
            Duration::from_secs(60)
        ));

        drop(stdin_w);
        relay.await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn relay_session_deregisters_on_clean_eof() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ready) = start_killable_server("eof-tok", DEAD_DAEMON_PID).await;
        write_ready_record(dir.path(), &ready);
        let (stdin_w, _stdout_r, relay) = spawn_registered_relay(dir.path(), &ready);
        wait_for_clients(dir.path(), &[RELAY_CLIENT_PID]).await;

        drop(stdin_w);
        match relay.await.unwrap() {
            RelayOutcome::Completed => {}
            other => panic!("expected Completed, got {other:?}"),
        }
        assert_eq!(clients_in_state(dir.path()), Some(vec![]));
        server.abort();
    }

    const INITIALIZE_LINE: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"0.1\"}}}\n";

    #[tokio::test]
    async fn relay_session_deregisters_when_connection_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ready = ready_daemon(DEAD_DAEMON_PID, 1, "refused-tok");
        write_ready_record(dir.path(), &ready);
        let (mut stdin_w, _stdout_r, relay) = spawn_registered_relay(dir.path(), &ready);
        wait_for_clients(dir.path(), &[RELAY_CLIENT_PID]).await;

        use tokio::io::AsyncWriteExt;
        stdin_w.write_all(INITIALIZE_LINE).await.unwrap();
        stdin_w.flush().await.unwrap();

        match relay.await.unwrap() {
            RelayOutcome::FailedBeforeFirstByte(_) => {}
            other => panic!("expected FailedBeforeFirstByte, got {other:?}"),
        }
        assert_eq!(clients_in_state(dir.path()), Some(vec![]));
    }

    #[tokio::test]
    async fn relay_session_deregisters_when_daemon_dies_mid_session() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ready) = start_killable_server("mid-reg-tok", DEAD_DAEMON_PID).await;
        write_ready_record(dir.path(), &ready);
        let (mut stdin_w, mut stdout_r, relay) = spawn_registered_relay(dir.path(), &ready);

        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        stdin_w.write_all(INITIALIZE_LINE).await.unwrap();
        stdin_w.flush().await.unwrap();
        let mut first = String::new();
        stdout_r.read_line(&mut first).await.unwrap();
        assert!(!first.is_empty(), "should have received first response");
        assert_eq!(clients_in_state(dir.path()), Some(vec![RELAY_CLIENT_PID]));

        server.abort();
        let _ = server.await;
        stdin_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        stdin_w.flush().await.unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(35), relay)
            .await
            .expect("relay should finish")
            .expect("relay task should not panic");
        match outcome {
            RelayOutcome::FailedMidSession(_) => {}
            other => panic!("expected FailedMidSession, got {other:?}"),
        }
        assert_eq!(clients_in_state(dir.path()), Some(vec![]));
    }

    /// A live process standing in for the daemon, so a shutdown signal is observable.
    #[cfg(unix)]
    struct StandInDaemon(std::process::Child);

    #[cfg(unix)]
    impl StandInDaemon {
        fn spawn() -> Self {
            Self(
                std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .unwrap(),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        /// Whether the process is still running after giving a signal time to land.
        async fn alive_after_settle(&mut self) -> bool {
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.0.try_wait().unwrap().is_none()
        }
    }

    #[cfg(unix)]
    impl Drop for StandInDaemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn last_relay_leaving_does_not_signal_daemon() {
        let mut daemon = StandInDaemon::spawn();
        let dir = tempfile::tempdir().unwrap();
        let (server, ready) = start_killable_server("quiet-tok", daemon.pid()).await;
        write_ready_record(dir.path(), &ready);
        let (stdin_w, _stdout_r, relay) = spawn_registered_relay(dir.path(), &ready);
        wait_for_clients(dir.path(), &[RELAY_CLIENT_PID]).await;

        drop(stdin_w);
        relay.await.unwrap();
        assert_eq!(clients_in_state(dir.path()), Some(vec![]));
        assert!(
            daemon.alive_after_settle().await,
            "daemon shutdown is left to its sweep"
        );
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deregister_client_signals_daemon_when_last_client_leaves() {
        let mut daemon = StandInDaemon::spawn();
        let dir = tempfile::tempdir().unwrap();
        write_state(
            dir.path(),
            &ready_state(daemon.pid(), 9090, "tok", vec![1111]),
        )
        .unwrap();

        deregister_client(dir.path(), 1111).unwrap();
        assert!(!daemon.alive_after_settle().await);
    }

    #[test]
    fn deregister_client_for_ignores_record_of_another_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let other = ready_state(DEAD_DAEMON_PID + 1, 9090, "tok", vec![RELAY_CLIENT_PID]);
        write_state(dir.path(), &other).unwrap();

        deregister_client_for(
            dir.path(),
            DaemonPid::new(DEAD_DAEMON_PID).unwrap(),
            RELAY_CLIENT_PID,
        )
        .unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), Some(other));
    }

    /// Run a registered relay against `initial` state (whose daemon is not the
    /// one `ready` names) and assert it refuses without touching state or stdin.
    async fn assert_relay_refuses_registration(initial: Option<WebState>) {
        let dir = tempfile::tempdir().unwrap();
        if let Some(state) = &initial {
            write_state(dir.path(), state).unwrap();
        }
        let ready = ready_daemon(DEAD_DAEMON_PID, 1, "bound-tok");

        let (mut stdin_w, stdin_r) = tokio::io::duplex(8192);
        let (stdout_w, _stdout_r) = tokio::io::duplex(8192);
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        stdin_w.write_all(INITIALIZE_LINE).await.unwrap();
        drop(stdin_w);
        let mut reader = tokio::io::BufReader::new(stdin_r);

        let outcome =
            run_registered_relay(dir.path(), &ready, RELAY_CLIENT_PID, &mut reader, stdout_w).await;
        match outcome {
            RelayOutcome::FailedBeforeFirstByte(_) => {}
            other => panic!("expected FailedBeforeFirstByte, got {other:?}"),
        }
        assert_eq!(read_state(dir.path()).unwrap(), initial, "state untouched");
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(
            line,
            std::str::from_utf8(INITIALIZE_LINE).unwrap(),
            "stdin not consumed"
        );
    }

    #[tokio::test]
    async fn registered_relay_refuses_when_no_state_file() {
        assert_relay_refuses_registration(None).await;
    }

    #[tokio::test]
    async fn registered_relay_refuses_when_slot_is_claiming() {
        assert_relay_refuses_registration(Some(WebState::Claiming {
            claimer_pid: 100,
            port: 1,
            since: now_unix_secs(),
        }))
        .await;
    }

    #[tokio::test]
    async fn registered_relay_refuses_when_record_names_another_daemon() {
        assert_relay_refuses_registration(Some(ready_state(
            DEAD_DAEMON_PID + 1,
            1,
            "bound-tok",
            vec![],
        )))
        .await;
    }

    // ---- Ready implies a bound listener ----------------------------------

    fn test_daemon_pid() -> DaemonPid {
        DaemonPid::new(DEAD_DAEMON_PID).unwrap()
    }

    #[tokio::test]
    async fn bind_and_publish_reports_ready_on_bound_port_before_serving() {
        let dir = tempfile::tempdir().unwrap();
        let token = AuthToken::new("bound-tok").unwrap();

        let (listener, ready) =
            bind_and_publish(dir.path(), "127.0.0.1", 0, test_daemon_pid(), token.clone())
                .await
                .unwrap();

        let bound_port = listener.local_addr().unwrap().port();
        assert_ne!(bound_port, 0);
        assert_eq!(ready.port, bound_port);
        let probe = FakeProbe::with(&[DEAD_DAEMON_PID]);
        assert_eq!(
            discover(dir.path(), &probe).unwrap(),
            Discovery::Ready(ready_daemon(DEAD_DAEMON_PID, bound_port, "bound-tok"))
        );
        // Nothing is accepting yet; the kernel backlog still completes the handshake.
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(("127.0.0.1", bound_port)),
        )
        .await
        .expect("connect should not hang")
        .expect("connect to a published Ready port should succeed");
    }

    #[tokio::test]
    async fn relay_started_before_daemon_serves_completes() {
        let dir = tempfile::tempdir().unwrap();
        let (listener, ready) = bind_and_publish(
            dir.path(),
            "127.0.0.1",
            0,
            test_daemon_pid(),
            AuthToken::new("early-tok").unwrap(),
        )
        .await
        .unwrap();

        let (mut stdin_w, stdin_r) = tokio::io::duplex(8192);
        let (stdout_w, stdout_r) = tokio::io::duplex(8192);
        let mut stdout_r = tokio::io::BufReader::new(stdout_r);
        let ready_c = ready.clone();
        let relay = tokio::spawn(async move {
            run_relay_mode_with(&ready_c, tokio::io::BufReader::new(stdin_r), stdout_w).await
        });
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        stdin_w.write_all(INITIALIZE_LINE).await.unwrap();
        stdin_w.flush().await.unwrap();

        // Stand-in for model loading: the request sits in the backlog meanwhile.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !relay.is_finished(),
            "relay must still be waiting on the unserved listener"
        );
        let db = Database::open_in_memory(&DbConfig::default()).unwrap();
        let service = MemoryService::new(
            Arc::new(Mutex::new(db)),
            Arc::new(MockEmbedder::new(768)),
            None,
            ServiceConfig::default(),
        );
        let app = crate::web::app_router(AppState {
            service,
            auth_token: "early-tok".to_string(),
        });
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut first = String::new();
        tokio::time::timeout(Duration::from_secs(10), stdout_r.read_line(&mut first))
            .await
            .expect("initialize response should arrive")
            .unwrap();
        assert!(first.contains("\"id\":1"), "unexpected response: {first}");

        drop(stdin_w);
        let outcome = tokio::time::timeout(Duration::from_secs(10), relay)
            .await
            .expect("relay should finish")
            .expect("relay task should not panic");
        match outcome {
            RelayOutcome::Completed => {}
            other => panic!("expected Completed, got {other:?}"),
        }
        server.abort();
    }

    /// Run `bind_and_publish` on `bind:port` over `prior`; returns the error flag and resulting record.
    async fn bind_and_publish_over(
        prior: Option<WebState>,
        bind: &str,
        port: u16,
    ) -> (tempfile::TempDir, bool, Option<WebState>) {
        let dir = tempfile::tempdir().unwrap();
        if let Some(state) = &prior {
            write_state(dir.path(), state).unwrap();
        }
        let result = bind_and_publish(
            dir.path(),
            bind,
            port,
            test_daemon_pid(),
            AuthToken::new("unused-tok").unwrap(),
        )
        .await;
        let after = read_state(dir.path()).unwrap();
        (dir, result.is_err(), after)
    }

    fn live_claim(claimer_pid: u32, port: u16) -> WebState {
        WebState::Claiming {
            claimer_pid,
            port,
            since: now_unix_secs(),
        }
    }

    #[tokio::test]
    async fn bind_failure_clears_claim_so_waiters_see_vacant() {
        let occupant = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken_port = occupant.local_addr().unwrap().port();

        let (dir, failed, _) =
            bind_and_publish_over(Some(live_claim(100, taken_port)), "127.0.0.1", taken_port).await;

        assert!(failed, "binding an occupied port must fail");
        // The claimer is still alive, so only removing the record lets its wait end early.
        let probe = FakeProbe::with(&[100]);
        assert_eq!(discover(dir.path(), &probe).unwrap(), Discovery::Vacant);
    }

    #[tokio::test]
    async fn unparsable_bind_address_clears_claim() {
        let (_dir, failed, after) =
            bind_and_publish_over(Some(live_claim(100, 9090)), "localhost", 9090).await;

        assert!(failed, "a hostname is not a SocketAddr");
        assert_eq!(after, None);
    }

    #[tokio::test]
    async fn bind_failure_leaves_ready_record_untouched() {
        let occupant = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken_port = occupant.local_addr().unwrap().port();
        let ready = ready_state(777, taken_port, "live-tok", vec![5]);

        let (_dir, failed, after) =
            bind_and_publish_over(Some(ready.clone()), "127.0.0.1", taken_port).await;

        assert!(failed);
        assert_eq!(after, Some(ready));
    }

    #[tokio::test]
    async fn bind_failure_leaves_claim_for_another_port_untouched() {
        let occupant = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken_port = occupant.local_addr().unwrap().port();
        let other_claim = live_claim(100, taken_port.wrapping_add(1));

        let (_dir, failed, after) =
            bind_and_publish_over(Some(other_claim.clone()), "127.0.0.1", taken_port).await;

        assert!(failed);
        assert_eq!(after, Some(other_claim));
    }
}
