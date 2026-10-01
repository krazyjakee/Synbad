//! Core-process supervisor and IPC request handler.
//!
//! Single-task state owner: all mutations to config / process state happen
//! here, driven by events on a `tokio::select!`.
//!
//! Split across this directory so the file driving the loop stays
//! navigable. Submodules each own one slice of `Supervisor`'s `impl`:
//!
//! * [`requests`] — the IPC `handle_request` dispatcher and outbound
//!   pairing kickoff.
//! * [`config_edit`] — local + remote config edits and the sync-op merge
//!   path that funnels through this struct.
//! * [`core_proc`] — Deskflow Core child lifecycle (resolve / spawn /
//!   stop / crash-restart) plus the free helpers used to build its argv.
//!
//! This file keeps the constructor, the `select!` loop, and the small
//! discovery / log / state helpers the loop calls directly.

mod config_edit;
mod core_proc;
mod requests;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{broadcast, mpsc, oneshot};

use synbad_config::{paths, Config, NodeRole};
use synbad_discovery::{Advertiser, Browser, DiscoveryEvent, Identity, TrustedPeerStore};
use synbad_ipc::log_parse;
use synbad_ipc::server::Listener;
use synbad_ipc::{DaemonState, DiscoveredPeer, Event};
use synbad_sync::VersionedConfig;

use crate::binaries::{ResolvedCore, Resolver};
use crate::pairing::{self, IncomingSession, SessionDeps};
use crate::sync::{self, SyncDeps, SyncOp};

/// Result of resolving the Core binary off the supervisor loop. `Err`
/// carries a human-readable reason surfaced to the GUI as a log line. The
/// argv is *not* part of this — it's rebuilt from the live config in
/// [`Supervisor::on_core_resolved`] so a config change during a slow
/// download can't spawn the Core with a stale role.
pub(super) type CoreResolveOutcome = Result<ResolvedCore, String>;

pub(super) const LOG_TAIL: usize = 500;
pub(super) const MIN_BACKOFF: Duration = Duration::from_millis(500);
pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Retry cap for the **client** role. A client that can't reach its
/// server keeps retrying forever; a tighter cap than the server's
/// [`MAX_BACKOFF`] means it notices the server coming back within
/// seconds rather than half a minute, while still ticking gently.
pub(super) const CLIENT_MAX_BACKOFF: Duration = Duration::from_secs(10);
/// Rapid exits retain the run intent and increase capped backoff. A child
/// that runs beyond this window resets backoff on exit, allowing a fresh
/// fast recovery after a mid-session drop.
pub(super) const FAST_FAIL_WINDOW: Duration = Duration::from_secs(2);
/// Emit an actionable server startup diagnostic after this many failures.
pub(super) const MAX_FAST_FAILS: u32 = 5;
/// How often the supervisor sweeps visible+trusted peers looking for
/// audio sessions that *should* exist but don't, and dials the missing
/// ones. The handshake/connect path is the only failure-prone step
/// (a few-second timeout) so a 5 s tick gives near-immediate recovery
/// without busy-spinning.
pub(super) const AUDIO_RECONCILE_TICK: Duration = Duration::from_secs(5);
/// First retry after a failed dial waits this long; subsequent failures
/// double up to [`AUDIO_BACKOFF_MAX`]. Reset to zero once we see a
/// `PeerStatus` for the peer (i.e. a session actually came up).
pub(super) const AUDIO_BACKOFF_MIN: Duration = Duration::from_secs(1);
pub(super) const AUDIO_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Per-peer backoff state for outbound audio dials. Stored in the
/// supervisor so the reconcile loop can skip peers that just failed.
#[derive(Debug, Clone)]
pub(super) struct AudioBackoff {
    pub next_attempt: Instant,
    pub attempts: u32,
}

impl AudioBackoff {
    fn delay(attempts: u32) -> Duration {
        // 1, 2, 4, 8, 16, 32, 60, 60, …  Saturate at AUDIO_BACKOFF_MAX
        // so a stuck peer doesn't push the next attempt out to infinity.
        let secs = AUDIO_BACKOFF_MIN
            .as_secs()
            .checked_shl(attempts.saturating_sub(1))
            .unwrap_or(AUDIO_BACKOFF_MAX.as_secs())
            .min(AUDIO_BACKOFF_MAX.as_secs());
        Duration::from_secs(secs)
    }

    fn after_failure(prev: Option<&AudioBackoff>) -> Self {
        let attempts = prev.map(|p| p.attempts.saturating_add(1)).unwrap_or(1);
        AudioBackoff {
            attempts,
            next_attempt: Instant::now() + Self::delay(attempts),
        }
    }
}

pub struct Supervisor {
    pub(super) config_path: PathBuf,
    pub(super) config: Config,
    /// Versioned mirror of `config`: same data, plus per-field Lamport
    /// stamps used for LWW merges with remote peers. `config` and
    /// `versioned.config` are kept identical — `config` is the cheap path
    /// for read-only callers (and matches the existing supervisor code),
    /// `versioned` is what we ship over the wire and what sync sessions
    /// merge into.
    pub(super) versioned: VersionedConfig,
    /// Sidecar file where `versioned`'s stamps + clock live. The config
    /// itself stays in TOML at `config_path`.
    pub(super) versions_path: PathBuf,
    pub(super) state: DaemonState,
    pub(super) connected_peers: std::collections::BTreeSet<String>,
    pub(super) active_screen: Option<String>,
    pub(super) log_tail: VecDeque<String>,
    pub(super) events: broadcast::Sender<Event>,
    /// `true` after a Start, and at launch unless the user explicitly
    /// stopped (see [`paths::user_stopped_marker`]). Drives auto-restart
    /// when the Core exits unexpectedly, and gates
    /// the audio subsystem: audio is online only while this is true *and*
    /// `config.audio.enabled` (see [`Supervisor::reconcile_audio_subsystem`])
    /// so input sharing and audio go up and down together.
    pub(super) desired_running: bool,
    /// Send `()` to terminate the supervised child. `None` when not running.
    pub(super) child_kill: Option<oneshot::Sender<()>>,
    pub(super) backoff: Duration,
    /// When the next automatic Core restart is due. Set by the crash /
    /// reconnect path instead of sleeping inline, so the `select!` loop
    /// keeps serving IPC (a Stop lands immediately) while we wait. Cleared
    /// by any explicit stop or start.
    pub(super) restart_at: Option<tokio::time::Instant>,
    pub(super) log_rx: mpsc::Receiver<String>,
    pub(super) log_tx: mpsc::Sender<String>,
    /// Core exits, tagged with the pid that exited so a late exit from a
    /// superseded child can't be mistaken for the current one's.
    pub(super) exit_rx: mpsc::Receiver<(u32, std::process::ExitStatus)>,
    pub(super) exit_tx: mpsc::Sender<(u32, std::process::ExitStatus)>,
    /// Pid of the Core child this supervisor currently owns, if any.
    pub(super) child_pid: Option<u32>,
    pub(super) fs_rx: mpsc::Receiver<()>,
    pub(super) _fs_watcher: RecommendedWatcher,
    pub(super) resolver: Resolver,
    /// Carries the Core program+argv (or a failure reason) back from the
    /// background resolution task into the `select!` loop. Resolving can
    /// hit the network (GitHub API + a multi-MB asset download + archive
    /// extraction); doing it inline would freeze the daemon — IPC,
    /// pairing, discovery and sync would all stall until it finished. See
    /// [`core_proc`] for the spawn / handoff.
    pub(super) core_resolve_tx: mpsc::Sender<CoreResolveOutcome>,
    pub(super) core_resolve_rx: mpsc::Receiver<CoreResolveOutcome>,
    /// `true` while a resolution task is in flight. Guards against firing
    /// a second one (e.g. repeated Start clicks) before the first lands.
    pub(super) core_resolving: bool,
    /// Set by `Request::Shutdown`; the run loop stops the Core and returns
    /// after the response has been flushed to the client.
    pub(super) shutdown: bool,
    /// When the currently-spawned child started — used to classify exits
    /// as "instant fail" vs "ran for a while then died".
    pub(super) started_at: Option<Instant>,
    /// Consecutive instant-fails. Reset when the child runs longer than
    /// [`FAST_FAIL_WINDOW`] or when the user explicitly stops/starts.
    pub(super) fast_fail_count: u32,

    /// Stable per-machine identity (UUID + ed25519 keypair). Persists
    /// across restarts via the user's config dir.
    pub(super) identity: Arc<Identity>,
    /// mDNS service advertisement. Dropped on shutdown to send a goodbye.
    /// `None` if discovery failed to start — daemon keeps running.
    /// Live-updated by `ensure_audio_subsystem` / `teardown_audio_subsystem`
    /// so peers see our `audio_port` flip without a daemon restart.
    pub(super) advertiser: Option<Advertiser>,
    /// mDNS browser; lives alongside the advertiser.
    pub(super) _browser: Option<Browser>,
    /// Incoming Found/Lost events from the browser thread.
    pub(super) discovery_rx: Option<mpsc::Receiver<DiscoveryEvent>>,
    /// Currently-visible peers, keyed by machine_id.
    pub(super) peers: HashMap<String, DiscoveredPeer>,
    /// User-paired peers, mutex-guarded so pairing sessions can persist
    /// without funneling through the supervisor task.
    pub(super) trust: Arc<tokio::sync::Mutex<TrustedPeerStore>>,
    /// `oneshot::Sender` half for each in-flight pairing session, keyed
    /// by session_id. `ConfirmPairing` looks up here.
    pub(super) pairing_confirm: HashMap<String, oneshot::Sender<bool>>,
    /// Receiver for new inbound sessions accepted by the pairing listener.
    pub(super) incoming_pairings: Option<mpsc::Receiver<IncomingSession>>,
    /// Dependencies handed to every pairing session task.
    pub(super) pairing_deps: Arc<SessionDeps>,
    /// Owned pairing tasks; aborted when the supervisor is dropped.
    pub(super) _pairing_listener: Option<tokio::task::JoinHandle<()>>,
    pub(super) pairing_tasks: Vec<tokio::task::JoinHandle<()>>,

    /// Shared deps for sync sessions (identity, trust, event bus, the
    /// channel sessions use to ask the supervisor to merge).
    pub(super) sync_deps: Arc<SyncDeps>,
    /// Listener accepting inbound sync sessions. `None` if bind failed
    /// at startup — outbound sync still works.
    pub(super) _sync_listener: Option<tokio::task::JoinHandle<()>>,
    /// Receiver for the SyncOp channel — sessions ask us to merge / read
    /// state through this.
    pub(super) sync_ops: mpsc::Receiver<SyncOp>,
    /// One outbound sync per peer, tagged with the head at dial time.
    /// Reconcile consumes results, tracks success, and retries failures.
    pub(super) sync_tasks: HashMap<String, (String, tokio::task::JoinHandle<bool>)>,
    pub(super) sync_confirmed: HashMap<String, String>,
    pub(super) sync_backoff: HashMap<String, AudioBackoff>,
    listener_ports: (u16, u16, u16),

    /// Audio bridge handle (commands + events). `None` whenever the
    /// audio subsystem is offline — i.e. Synbad isn't started or
    /// `config.audio.enabled` is false. Brought up / torn down live by
    /// [`Supervisor::reconcile_audio_subsystem`]; no daemon restart needed.
    pub(super) audio: Option<synbad_audio::AudioBridgeHandle>,
    /// Run-loop task driving the bridge. Held so the bridge isn't dropped.
    pub(super) _audio_task: Option<tokio::task::JoinHandle<()>>,
    /// Listener accepting inbound audio signaling sessions.
    pub(super) _audio_listener: Option<tokio::task::JoinHandle<()>>,
    /// Shared deps reused by every outbound audio dial fired from
    /// `dial_audio_one`. `None` when audio is disabled — outbound dial
    /// is skipped in that case.
    pub(super) audio_dial_deps: Option<Arc<crate::audio::AudioListenerDeps>>,
    /// Outbound audio handshake tasks kept alive while running. GCed
    /// alongside `sync_tasks` / `pairing_tasks`.
    pub(super) audio_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Peers currently being dialed. Prevents the reconcile loop from
    /// spawning a second dial for a peer whose first dial is still
    /// negotiating the handshake.
    pub(super) audio_inflight: std::collections::HashSet<String>,
    /// Peers we believe have a live audio session in the bridge. Updated
    /// from `AudioEvent::PeerStatus` (entry) and `AudioEvent::SessionClosed`
    /// or `PeerStatus.last_error.is_some()` (eviction). The reconcile
    /// loop dials any peer that *should* be live but isn't.
    pub(super) audio_live: std::collections::HashSet<String>,
    /// Per-peer dial backoff. A peer is skipped during reconcile until
    /// `next_attempt` passes; cleared on successful session establishment.
    pub(super) audio_backoff: HashMap<String, AudioBackoff>,
    /// Outbound dial tasks send their result here. The supervisor reads
    /// this in `select!` to clear in-flight tracking and bump backoff.
    pub(super) audio_dial_done_tx: mpsc::Sender<crate::audio::AudioDialOutcome>,
    pub(super) audio_dial_done_rx: mpsc::Receiver<crate::audio::AudioDialOutcome>,
    /// Periodic kick for the reconcile loop. See [`AUDIO_RECONCILE_TICK`].
    pub(super) audio_reconcile: tokio::time::Interval,
}

impl Supervisor {
    /// `log_tx` / `log_rx` are owned by the caller because `main.rs` also
    /// attaches a tracing subscriber layer to the same sender, so synbad's
    /// own info/warn lines appear in the GUI's in-app log alongside Core's
    /// stdout/stderr.
    pub async fn new(
        config_path: PathBuf,
        events: broadcast::Sender<Event>,
        log_tx: mpsc::Sender<String>,
        log_rx: mpsc::Receiver<String>,
    ) -> Result<Self> {
        let config = Config::load(&config_path)?.unwrap_or_default();
        let versions_path = paths::config_versions_file();

        let (exit_tx, exit_rx) = mpsc::channel::<(u32, std::process::ExitStatus)>(16);
        let (fs_tx, fs_rx) = mpsc::channel::<()>(16);

        // notify invokes the callback off-tokio; bridge via try_send.
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                use notify::EventKind;
                if matches!(
                    ev.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                ) {
                    let _ = fs_tx.try_send(());
                }
            }
        })
        .context("creating file watcher")?;
        // Watch the parent dir — atomic-rename saves don't trigger Modify on the file itself.
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).ok();
            watcher
                .watch(parent, RecursiveMode::NonRecursive)
                .with_context(|| format!("watching {:?}", parent))?;
        }

        let resolver = Resolver::new(paths::state_dir().join("bin"))
            .context("initializing binary resolver")?;
        let (core_resolve_tx, core_resolve_rx) = mpsc::channel::<CoreResolveOutcome>(4);

        let identity = Identity::load_or_create(&paths::config_dir().join("identity"))
            .context("loading machine identity")?;
        tracing::info!(
            machine_id = %identity.machine_id,
            fingerprint = %identity.fingerprint,
            "local identity ready"
        );
        let identity = Arc::new(identity);

        // Build the versioned config. If the sidecar already exists,
        // restore its stamps; otherwise bootstrap with everything stamped
        // by the local machine_id at counter=1.
        let versioned = match VersionedConfig::load_sidecar(&versions_path) {
            Ok(Some(sidecar)) => {
                VersionedConfig::from_parts(config.clone(), sidecar.stamps, sidecar.clock)
            }
            Ok(None) => {
                let v = VersionedConfig::initial(config.clone(), &identity.machine_id.to_string());
                // Persist the bootstrap stamps so a peer that asks for
                // our state during the very first session sees a stable
                // identity rather than counter=0/empty-origin defaults.
                if let Err(e) = v.save_sidecar(&versions_path) {
                    tracing::warn!(
                        ?e,
                        ?versions_path,
                        "could not write initial versions sidecar"
                    );
                }
                v
            }
            Err(e) => {
                tracing::warn!(?e, ?versions_path, "ignoring malformed versions sidecar");
                VersionedConfig::initial(config.clone(), &identity.machine_id.to_string())
            }
        };

        // mDNS startup is best-effort: a no-network dev box or a locked-down
        // VM shouldn't keep the rest of the daemon from coming up. We log
        // and continue without discovery if either side fails.
        let (advertiser, browser, discovery_rx) =
            match core_proc::start_discovery(&identity, &config, &versioned.head_hash()) {
                Ok((a, b, rx)) => (Some(a), Some(b), Some(rx)),
                Err(e) => {
                    tracing::warn!(?e, "discovery disabled");
                    (None, None, None)
                }
            };

        let trust_path = paths::config_dir().join("trusted-peers.json");
        let trust = TrustedPeerStore::load(&trust_path)
            .with_context(|| format!("loading trusted-peers at {:?}", trust_path))?;
        let trust = Arc::new(tokio::sync::Mutex::new(trust));

        let pairing_deps = Arc::new(SessionDeps {
            identity: identity.clone(),
            trust: trust.clone(),
            events: events.clone(),
            display_name: config.server_name.clone(),
        });

        // The pairing listener is also best-effort; if the port is taken
        // or routing is broken we just lose pairing. Outbound dialing
        // would still work, but with no listener inbound, the symmetric
        // protocol can't complete — the supervisor still serves the GUI.
        let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingSession>(8);
        let pairing_listener =
            match pairing::spawn_listener(config.service_port, pairing_deps.clone(), incoming_tx)
                .await
            {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(?e, "pairing listener disabled");
                    None
                }
            };

        // Sync listener: best-effort same as pairing. Multiple syncs can
        // be in flight at once, but the supervisor merges them serially
        // through the ops channel — bound is small because each op is
        // tiny and fast.
        let (sync_ops_tx, sync_ops_rx) = mpsc::channel::<SyncOp>(32);
        let sync_deps = Arc::new(SyncDeps {
            identity: identity.clone(),
            trust: trust.clone(),
            events: events.clone(),
            ops: sync_ops_tx,
        });
        let sync_listener = match sync::spawn_listener(config.sync_port, sync_deps.clone()).await {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!(?e, "sync listener disabled");
                None
            }
        };

        // Audio subsystem is built on demand by `ensure_audio_subsystem`
        // — both at startup (if `config.audio.enabled` is true) and at
        // runtime if the user flips the toggle. We construct the
        // Supervisor with empty audio fields and bring the subsystem up
        // immediately after if needed; the same helper handles the live
        // reconfigure path.
        //
        // Channel for outbound audio dial tasks to report back. Bound is
        // small — the supervisor drains it on every loop iteration and
        // a handful of in-flight dials is the worst case.
        let (audio_dial_done_tx, audio_dial_done_rx) =
            mpsc::channel::<crate::audio::AudioDialOutcome>(16);

        let listener_ports = (
            config.service_port,
            config.sync_port,
            config.audio.signal_port,
        );
        let mut supervisor = Supervisor {
            config_path,
            config,
            versioned,
            versions_path,
            state: DaemonState::Stopped,
            connected_peers: Default::default(),
            active_screen: None,
            log_tail: VecDeque::with_capacity(LOG_TAIL),
            events,
            desired_running: false,
            child_kill: None,
            backoff: MIN_BACKOFF,
            restart_at: None,
            log_rx,
            log_tx,
            exit_rx,
            exit_tx,
            child_pid: None,
            fs_rx,
            _fs_watcher: watcher,
            resolver,
            core_resolve_tx,
            core_resolve_rx,
            core_resolving: false,
            shutdown: false,
            started_at: None,
            fast_fail_count: 0,
            identity,
            advertiser,
            _browser: browser,
            discovery_rx,
            peers: HashMap::new(),
            trust,
            pairing_confirm: HashMap::new(),
            incoming_pairings: Some(incoming_rx),
            pairing_deps,
            _pairing_listener: pairing_listener,
            pairing_tasks: Vec::new(),
            sync_deps,
            _sync_listener: sync_listener,
            sync_ops: sync_ops_rx,
            sync_tasks: HashMap::new(),
            sync_confirmed: HashMap::new(),
            sync_backoff: HashMap::new(),
            listener_ports,
            audio: None,
            _audio_task: None,
            _audio_listener: None,
            audio_dial_deps: None,
            audio_tasks: Vec::new(),
            audio_inflight: std::collections::HashSet::new(),
            audio_live: std::collections::HashSet::new(),
            audio_backoff: HashMap::new(),
            audio_dial_done_tx,
            audio_dial_done_rx,
            audio_reconcile: {
                let mut i = tokio::time::interval(AUDIO_RECONCILE_TICK);
                // Skip the immediate first tick — we don't want to fire a
                // reconcile inside the constructor before `run` is even
                // up. The first deliberate kick happens from
                // `ensure_audio_subsystem` after it brings the subsystem
                // online.
                i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                i.reset();
                i
            },
        };

        // Sharing is on by default: bring the Core up at launch unless the
        // user explicitly pressed Stop last time. A client with no server
        // yet simply enters its reconnect loop.
        if !paths::user_stopped_marker().exists() {
            supervisor.desired_running = true;
            supervisor.start_core().await;
        }

        // Audio is coupled to the run state: it comes up only while
        // Synbad is started. Routing through the same reconcile the
        // Start/Stop handlers use keeps startup and runtime coherent.
        let _ = supervisor.reconcile_audio_subsystem().await;

        Ok(supervisor)
    }

    pub async fn run(&mut self, mut listener: Listener) -> Result<()> {
        let mut shutdown_signals = ShutdownSignals::new();
        loop {
            // `discovery_rx` and `incoming_pairings` may be absent if the
            // corresponding subsystem failed to start. We pin a fresh
            // future each iteration so the `select!` doesn't have to be
            // conditional on their presence.
            let discovery_recv = async {
                match self.discovery_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<DiscoveryEvent>>().await,
                }
            };
            let pairing_accept = async {
                match self.incoming_pairings.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<IncomingSession>>().await,
                }
            };
            let audio_event = async {
                match self.audio.as_mut() {
                    Some(h) => h.events_rx.recv().await,
                    None => std::future::pending::<Option<synbad_audio::AudioEvent>>().await,
                }
            };
            let restart_at = self.restart_at;
            let restart_due = async move {
                match restart_at {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                _ = restart_due => {
                    self.restart_at = None;
                    if self.desired_running {
                        self.start_core().await;
                    }
                }
                _ = self.audio_reconcile.tick() => {
                    // Periodic safety net: re-attempt any audio session that
                    // *should* be live but isn't. Cheap no-op when the
                    // subsystem is disabled or every peer is already up.
                    self.reconcile_network_services().await;
                    self.reconcile_sync_sessions().await;
                    self.gc_pairing_tasks();
                    self.gc_audio_tasks();
                    self.reconcile_audio_sessions();
                }
                Some(outcome) = self.audio_dial_done_rx.recv() => {
                    self.handle_audio_dial_outcome(outcome);
                }
                Some(req) = listener.next_request() => {
                    self.handle_request(req).await;
                    if self.shutdown {
                        tracing::info!("shutdown requested by client, stopping");
                        self.stop_core().await;
                        self.teardown_audio_subsystem().await;
                        // Give the IPC connection task a beat to flush the
                        // `Response::Ok` we just queued before the process
                        // exits out from under it.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        return Ok(());
                    }
                }
                Some(outcome) = self.core_resolve_rx.recv() => {
                    self.on_core_resolved(outcome).await;
                }
                Some(line) = self.log_rx.recv() => {
                    self.record_log(line);
                }
                Some((pid, status)) = self.exit_rx.recv() => {
                    self.handle_child_exit(pid, status).await;
                }
                Some(()) = self.fs_rx.recv() => {
                    self.handle_config_changed().await;
                }
                Some(ev) = discovery_recv => {
                    self.handle_discovery(ev);
                }
                Some(s) = pairing_accept => {
                    self.handle_incoming_pairing(s);
                }
                Some(op) = self.sync_ops.recv() => {
                    self.handle_sync_op(op).await;
                }
                Some(ev) = audio_event => {
                    self.handle_audio_event(ev);
                }
                signal = shutdown_signals.recv() => {
                    tracing::info!("{signal}, shutting down");
                    self.stop_core().await;
                    self.teardown_audio_subsystem().await;
                    return Ok(());
                }
            }
        }
    }

    fn handle_incoming_pairing(&mut self, session: IncomingSession) {
        tracing::info!(session_id = %session.session_id, "inbound pairing session opened");
        self.pairing_confirm
            .insert(session.session_id.clone(), session.confirm_tx);
        self.pairing_tasks.push(session._task);
        let _ = session.registered.send(());
        self.gc_pairing_tasks();
    }

    /// Forward an event from the audio bridge onto the supervisor's
    /// event bus so the GUI sees it, and update internal liveness state
    /// so the reconcile loop knows which peers still need a dial.
    fn handle_audio_event(&mut self, ev: synbad_audio::AudioEvent) {
        use synbad_audio::AudioEvent as A;
        match ev {
            A::PeerStatus(status) => {
                // A status with `last_error: Some(_)` means the bridge
                // still holds the session but the underlying WebRTC PC
                // is broken — treat it as not-live so reconcile retries
                // (the bridge's glare path will replace the dead session
                // when our new dial reaches it).
                if status.last_error.is_some() {
                    self.audio_live.remove(&status.machine_id);
                } else {
                    self.audio_live.insert(status.machine_id.clone());
                    self.audio_backoff.remove(&status.machine_id);
                }
                let _ = self.events.send(Event::AudioPeerStatus { status });
            }
            A::Error { peer, message } => {
                if let Some(id) = &peer {
                    self.audio_live.remove(id);
                    self.audio_inflight.remove(id);
                    let retry = AudioBackoff::after_failure(self.audio_backoff.get(id));
                    self.audio_backoff.insert(id.clone(), retry);
                }
                let _ = self.events.send(Event::AudioError { peer, message });
            }
            A::DevicesChanged => {
                let _ = self.events.send(Event::AudioDevicesChanged);
            }
            A::SessionClosed { peer } => {
                self.audio_live.remove(&peer);
                tracing::debug!(peer = %peer, "audio session closed; reconcile will retry");
                // Tell the GUI to drop the row. The GUI's per-peer
                // status table is push-driven (the lazy `GetAudioStatus`
                // only fires on tab open), so without this signal a
                // stale "connected" entry would stick around until the
                // user restarts the app. The reconcile loop will redial
                // shortly if the peer is still trusted and visible —
                // when that session comes up a fresh `PeerStatus` will
                // repopulate the row.
                let _ = self
                    .events
                    .send(Event::AudioPeerRemoved { machine_id: peer });
            }
        }
    }

    pub(super) fn gc_pairing_tasks(&mut self) {
        self.pairing_tasks.retain(|t| !t.is_finished());
        self.pairing_confirm.retain(|_, tx| !tx.is_closed());
    }

    fn handle_discovery(&mut self, ev: DiscoveryEvent) {
        match ev {
            DiscoveryEvent::Found(peer) => {
                // mdns-sd fires `ServiceResolved` once per resolved address,
                // so a peer with several IPs (loopback + LAN + docker) shows
                // up multiple times in quick succession. Skip the
                // re-broadcast if nothing the user cares about changed —
                // only the `host` flips between resolutions and any one
                // value is fine to keep. `audio_port` is in the check so
                // a peer flipping audio on (re-registers their TXT with
                // a fresh audio_port) kicks our reconcile loop right
                // away instead of waiting up to 5 s for the next tick.
                let unchanged = self
                    .peers
                    .get(&peer.machine_id)
                    .map(|p| {
                        p.machine_id == peer.machine_id
                            && p.fingerprint == peer.fingerprint
                            && p.config_head == peer.config_head
                            && p.audio_port == peer.audio_port
                            && p.host == peer.host
                            && p.addresses == peer.addresses
                            && p.service_port == peer.service_port
                            && p.sync_port == peer.sync_port
                            && p.core_port == peer.core_port
                            && p.display_name == peer.display_name
                    })
                    .unwrap_or(false);
                let was_present = self.peers.contains_key(&peer.machine_id);
                self.peers.insert(peer.machine_id.clone(), peer.clone());
                if !unchanged || !was_present {
                    tracing::info!(
                        machine_id = %peer.machine_id,
                        display = %peer.display_name,
                        host = %peer.host,
                        "peer discovered"
                    );
                    let _ = self
                        .events
                        .send(Event::PeerDiscovered { peer: peer.clone() });
                    // If this peer is trusted and advertised a head that
                    // differs from ours, open a pull-sync so we converge
                    // even if we missed their previous push (e.g. we
                    // weren't on the LAN at the time).
                    self.sync_confirmed.remove(&peer.machine_id);
                    self.sync_backoff.remove(&peer.machine_id);
                    self.maybe_pull_from(peer);
                    // Independently consider opening an audio session
                    // for any peer that should be live but isn't. Driven
                    // off the same trigger (peer became visible) so a
                    // fresh LAN connection brings audio up without user
                    // action; the reconcile loop is also the safety net
                    // for any peer whose previous dial failed.
                    self.reconcile_audio_sessions();
                }
            }
            DiscoveryEvent::Lost { machine_id } => {
                if self.peers.remove(&machine_id).is_some() {
                    tracing::info!(%machine_id, "peer lost");
                    let _ = self.events.send(Event::PeerLost {
                        machine_id: machine_id.clone(),
                    });
                    // Drop liveness/backoff so a re-find dials cleanly.
                    self.audio_live.remove(&machine_id);
                    self.audio_backoff.remove(&machine_id);
                    self.sync_confirmed.remove(&machine_id);
                    self.sync_backoff.remove(&machine_id);
                    if let Some((_, task)) = self.sync_tasks.remove(&machine_id) {
                        task.abort();
                    }
                    if let Some(audio) = &self.audio {
                        let _ = audio
                            .commands_tx
                            .try_send(synbad_audio::AudioCommand::ClosePeer {
                                peer_machine_id: machine_id,
                            });
                    }
                }
            }
        }
    }

    /// Bring the audio subsystem online if it isn't already. Idempotent
    /// — safe to call repeatedly. Used both at startup and when the user
    /// flips `audio.enabled` from false to true at runtime, so the
    /// checkbox doesn't require a daemon restart anymore.
    ///
    /// On failure the daemon keeps running without audio; the GUI will
    /// surface the listener bind error via the standard `AudioError`
    /// path used by the live-reconfigure caller.
    pub(super) async fn ensure_audio_subsystem(&mut self) -> anyhow::Result<()> {
        if self.audio.is_some() {
            return Ok(());
        }
        if !self.config.audio.enabled {
            return Ok(());
        }
        let bridge = synbad_audio::AudioBridge::new(
            self.config.audio.clone(),
            self.identity.clone(),
            self.trust.clone(),
        );
        let (handle, task) = bridge.spawn();
        let dial_deps = Arc::new(crate::audio::AudioListenerDeps {
            identity: self.identity.clone(),
            trust: self.trust.clone(),
            bridge_commands: handle.commands_tx.clone(),
        });
        let listener =
            match crate::audio::spawn_listener(self.config.audio.signal_port, dial_deps.clone())
                .await
            {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(?e, "audio signal listener disabled");
                    None
                }
            };
        let listener_up = listener.is_some();
        self.audio = Some(handle);
        self._audio_task = Some(task);
        self._audio_listener = listener;
        self.audio_dial_deps = Some(dial_deps);
        // Refresh the mDNS TXT so peers learn our audio_port without a
        // daemon restart. Only advertise it when the listener actually
        // bound — peers seeing the key try to dial it. Failure here is
        // non-fatal: outbound dial still works, peers just won't redial
        // us until the next time something kicks the advertisement.
        if listener_up {
            if let Some(adv) = self.advertiser.as_mut() {
                if let Err(e) = adv.set_audio_port(self.config.audio.signal_port) {
                    tracing::warn!(?e, "failed to refresh mDNS TXT with audio_port");
                }
            }
        }
        tracing::info!("audio subsystem online");
        // Kick the reconcile loop right away so visible+trusted peers
        // get a session without waiting for the periodic tick.
        self.reconcile_audio_sessions();
        Ok(())
    }

    /// Tear the audio subsystem down — used when the user flips
    /// `audio.enabled` from true to false. Aborts the listener task,
    /// asks the bridge to drain, and drops dial deps so the reconcile
    /// loop becomes a no-op.
    pub(super) async fn teardown_audio_subsystem(&mut self) {
        for task in self.audio_tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
        if let Some(listener) = self._audio_listener.take() {
            listener.abort();
            let _ = listener.await;
        }
        if let Some(handle) = self.audio.take() {
            let _ = handle
                .commands_tx
                .try_send(synbad_audio::AudioCommand::Shutdown);
        }
        if let Some(mut task) = self._audio_task.take() {
            if tokio::time::timeout(Duration::from_millis(500), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
        while self.audio_dial_done_rx.try_recv().is_ok() {}
        self.audio_dial_deps = None;
        self.audio_live.clear();
        self.audio_inflight.clear();
        self.audio_backoff.clear();
        // Drop the audio_port TXT key so peers stop dialing us. Goodbye
        // is best-effort — a peer that misses the unregister will hit a
        // refused connection on its next reconcile tick and back off.
        if let Some(adv) = self.advertiser.as_mut() {
            if let Err(e) = adv.set_audio_port(0) {
                tracing::warn!(?e, "failed to drop audio_port from mDNS TXT");
            }
        }
        tracing::info!("audio subsystem offline");
    }

    /// Drive the audio subsystem to the state implied by the run state
    /// and config: it should be online iff Synbad is started
    /// (`desired_running`) *and* audio is enabled in config. This is the
    /// single level-triggered entry point — called from startup, the
    /// Start/Stop/Restart handlers, and the audio-config toggle — so
    /// input sharing and audio activate and deactivate together and
    /// never diverge.
    ///
    /// Idempotent: brings the bridge up or tears it down as needed. On a
    /// failed bring-up it surfaces an `AudioError` to the GUI and returns
    /// the error so the caller can react further; teardown can't fail.
    pub(super) async fn reconcile_audio_subsystem(&mut self) -> anyhow::Result<()> {
        let want = self.desired_running && self.config.audio.enabled;
        if !want {
            if self.audio.is_some() {
                self.teardown_audio_subsystem().await;
            }
            return Ok(());
        }
        if let Err(e) = self.ensure_audio_subsystem().await {
            tracing::warn!(?e, "audio subsystem failed to start");
            let _ = self.events.send(Event::AudioError {
                peer: None,
                message: format!("Audio could not start: {e}"),
            });
            return Err(e);
        }
        Ok(())
    }

    /// Open an outbound audio session to a single peer iff every gate
    /// passes:
    /// - audio subsystem is up (`audio_dial_deps` is `Some`),
    /// - the peer advertised an `audio_port`,
    /// - the peer's routing under the current config would actually do
    ///   work (skipping this avoids dead empty sessions for peers the
    ///   user has explicitly disabled),
    /// - our `machine_id` sorts lower than theirs (glare rule — only
    ///   one side dials so we don't end up with two sessions),
    /// - the peer is in the trust store,
    /// - we don't already think a session is live for that peer,
    /// - no other dial to that peer is in flight, and
    /// - per-peer backoff has elapsed.
    ///
    /// Called from [`Self::reconcile_audio_sessions`]; not directly anywhere
    /// else. The reconcile path is the only place this should fire —
    /// keeping it single-entry means the inflight/backoff bookkeeping is
    /// guaranteed consistent.
    fn dial_audio_one(&mut self, peer: DiscoveredPeer) {
        let Some(dial_deps) = self.audio_dial_deps.as_ref() else {
            return;
        };
        if peer.audio_port == 0 {
            // Common after a hot toggle: our bridge is up but the peer's
            // mDNS advertisement is still the startup snapshot without
            // an `audio_port` key. `trace!` because this fires once per
            // visible peer per reconcile tick — too noisy for `debug!`,
            // but you need _some_ breadcrumb when "audio is on but
            // nothing's dialing" is the symptom.
            tracing::trace!(
                peer = %peer.machine_id,
                "skipping audio dial: peer's mDNS TXT has no audio_port"
            );
            return;
        }
        if !synbad_audio::peer_audio_active(&self.config.audio, &peer.machine_id) {
            return;
        }
        if self.identity.machine_id.to_string() >= peer.machine_id {
            // The other side dials. We accept on the listener; their
            // reconcile loop will redial us if their session drops.
            return;
        }
        if self.audio_live.contains(&peer.machine_id) {
            return;
        }
        if self.audio_inflight.contains(&peer.machine_id) {
            return;
        }
        if let Some(b) = self.audio_backoff.get(&peer.machine_id) {
            if b.next_attempt > Instant::now() {
                return;
            }
        }
        let is_trusted = match self.trust.try_lock() {
            Ok(g) => g.contains(&peer.machine_id),
            Err(_) => {
                tracing::debug!("trust mutex busy; skipping audio dial");
                return;
            }
        };
        if !is_trusted {
            return;
        }
        tracing::debug!(peer = %peer.machine_id, "dialing audio session");
        self.audio_inflight.insert(peer.machine_id.clone());
        let handle =
            crate::audio::spawn_outbound(peer, dial_deps.clone(), self.audio_dial_done_tx.clone());
        self.audio_tasks.push(handle);
        self.gc_audio_tasks();
    }

    /// Walk every visible peer and dial the ones that should have a
    /// session but don't. The single-entry helper [`Self::dial_audio_one`]
    /// enforces all the per-peer gates; this function just iterates.
    ///
    /// Triggered from three places:
    /// 1. The 5 s `audio_reconcile` interval (safety net for failed
    ///    dials, dropped sessions, and config changes the bridge missed).
    /// 2. Right after [`Self::ensure_audio_subsystem`] brings the subsystem up
    ///    so the user doesn't wait a tick for the first dial.
    /// 3. On each `DiscoveryEvent::Found` so newly-arrived peers are
    ///    snappy.
    pub(super) fn reconcile_audio_sessions(&mut self) {
        if self.audio_dial_deps.is_none() {
            return;
        }
        // Clone the peer list out so the loop body can mutably borrow
        // `self` to update inflight/backoff state.
        let candidates: Vec<DiscoveredPeer> = self.peers.values().cloned().collect();
        for peer in candidates {
            self.dial_audio_one(peer);
        }
    }

    /// Resolve an outbound dial. The bridge publishes an initial status
    /// for accepted sessions; failure clears inflight and arms backoff.
    pub(super) fn handle_audio_dial_outcome(&mut self, outcome: crate::audio::AudioDialOutcome) {
        use crate::audio::AudioDialOutcome as O;
        match outcome {
            O::Ok { peer_machine_id } => {
                // The handshake reached the bridge; clear backoff so a
                // later transient failure doesn't inherit stale attempts.
                self.audio_inflight.remove(&peer_machine_id);
                self.audio_backoff.remove(&peer_machine_id);
                tracing::debug!(peer = %peer_machine_id, "outbound audio dial handed off to bridge");
            }
            O::Err {
                peer_machine_id,
                error,
            } => {
                self.audio_inflight.remove(&peer_machine_id);
                let next = AudioBackoff::after_failure(self.audio_backoff.get(&peer_machine_id));
                tracing::debug!(
                    peer = %peer_machine_id,
                    attempts = next.attempts,
                    %error,
                    "outbound audio dial failed; scheduling retry"
                );
                self.audio_backoff.insert(peer_machine_id, next);
            }
        }
    }

    pub(super) fn gc_audio_tasks(&mut self) {
        self.audio_tasks.retain(|t| !t.is_finished());
    }

    /// Sync a trusted visible peer when the current local head has not
    /// been confirmed, respecting one in-flight task and capped backoff.
    fn maybe_pull_from(&mut self, peer: DiscoveredPeer) {
        if peer.sync_port == 0 {
            return;
        }
        let head = self.versioned.head_hash();
        if self.sync_tasks.contains_key(&peer.machine_id)
            || self.sync_confirmed.get(&peer.machine_id) == Some(&head)
            || self
                .sync_backoff
                .get(&peer.machine_id)
                .is_some_and(|b| b.next_attempt > Instant::now())
        {
            return;
        }
        let is_trusted = match self.trust.try_lock() {
            Ok(g) => g.contains(&peer.machine_id),
            Err(_) => {
                tracing::debug!("trust mutex busy; skipping pull");
                return;
            }
        };
        if !is_trusted {
            return;
        }
        let peer_id = peer.machine_id.clone();
        let handle = sync::spawn_outbound(peer, self.sync_deps.clone());
        self.sync_tasks.insert(peer_id, (head, handle));
    }

    async fn reconcile_sync_sessions(&mut self) {
        let finished: Vec<String> = self
            .sync_tasks
            .iter()
            .filter(|(_, (_, task))| task.is_finished())
            .map(|(peer, _)| peer.clone())
            .collect();
        for peer in finished {
            let (head, task) = self.sync_tasks.remove(&peer).expect("finished sync exists");
            if matches!(task.await, Ok(true)) {
                self.sync_confirmed.insert(peer.clone(), head);
                self.sync_backoff.remove(&peer);
            } else {
                let retry = AudioBackoff::after_failure(self.sync_backoff.get(&peer));
                self.sync_backoff.insert(peer, retry);
            }
        }
        for peer in self.peers.values().cloned().collect::<Vec<_>>() {
            self.maybe_pull_from(peer);
        }
    }

    /// Failed startup binds and dead listeners are retried while IPC keeps
    /// serving. Port edits replace listeners without restarting the daemon.
    async fn reconcile_network_services(&mut self) {
        if self.pairing_deps.display_name != self.config.server_name {
            self.pairing_deps = Arc::new(SessionDeps {
                identity: self.identity.clone(),
                trust: self.trust.clone(),
                events: self.events.clone(),
                display_name: self.config.server_name.clone(),
            });
            if let Some(task) = self._pairing_listener.take() {
                task.abort();
                let _ = task.await;
            }
        }
        if self
            .discovery_rx
            .as_ref()
            .is_some_and(mpsc::Receiver::is_closed)
        {
            self.discovery_rx = None;
            self._browser = None;
            self.advertiser = None;
        }
        if self._browser.is_none() {
            match core_proc::start_discovery(
                &self.identity,
                &self.config,
                &self.versioned.head_hash(),
            ) {
                Ok((a, b, rx)) => {
                    self.advertiser = Some(a);
                    self._browser = Some(b);
                    self.discovery_rx = Some(rx);
                }
                Err(e) => tracing::debug!(?e, "discovery retry failed"),
            }
        }
        if self.listener_ports.0 != self.config.service_port
            || self
                ._pairing_listener
                .as_ref()
                .is_some_and(|t| t.is_finished())
        {
            if let Some(task) = self._pairing_listener.take() {
                task.abort();
                let _ = task.await;
            }
            self.incoming_pairings = None;
        }
        if self._pairing_listener.is_none() {
            let (tx, rx) = mpsc::channel(8);
            match pairing::spawn_listener(self.config.service_port, self.pairing_deps.clone(), tx)
                .await
            {
                Ok(task) => {
                    self._pairing_listener = Some(task);
                    self.incoming_pairings = Some(rx);
                    self.listener_ports.0 = self.config.service_port;
                }
                Err(e) => tracing::debug!(?e, "pairing listener retry failed"),
            }
        }
        if self.listener_ports.1 != self.config.sync_port
            || self
                ._sync_listener
                .as_ref()
                .is_some_and(|t| t.is_finished())
        {
            if let Some(task) = self._sync_listener.take() {
                task.abort();
                let _ = task.await;
            }
        }
        if self._sync_listener.is_none() {
            match sync::spawn_listener(self.config.sync_port, self.sync_deps.clone()).await {
                Ok(task) => {
                    self._sync_listener = Some(task);
                    self.listener_ports.1 = self.config.sync_port;
                }
                Err(e) => tracing::debug!(?e, "sync listener retry failed"),
            }
        }
        if self.listener_ports.2 != self.config.audio.signal_port
            || self._audio_task.as_ref().is_some_and(|t| t.is_finished())
        {
            self.teardown_audio_subsystem().await;
        }
        let _ = self.reconcile_audio_subsystem().await;
        if let Some(audio) = &self.audio {
            // Retry a live config handoff if a previous edit hit a full queue.
            let _ = audio
                .commands_tx
                .try_send(synbad_audio::AudioCommand::Reconfigure(
                    self.config.audio.clone(),
                ));
        }
        self.listener_ports.2 = self.config.audio.signal_port;
        if self
            ._audio_listener
            .as_ref()
            .is_some_and(|t| t.is_finished())
        {
            self._audio_listener = None;
        }
        if self._audio_listener.is_none() {
            if let Some(deps) = self.audio_dial_deps.clone() {
                match crate::audio::spawn_listener(self.config.audio.signal_port, deps).await {
                    Ok(task) => self._audio_listener = Some(task),
                    Err(e) => tracing::debug!(?e, "audio listener retry failed"),
                }
            }
        }
        let audio_port = if self._audio_listener.is_some() {
            self.config.audio.signal_port
        } else {
            0
        };
        if let Some(advertiser) = &mut self.advertiser {
            if let Err(e) = advertiser.refresh(
                self.config.service_port,
                self.config.sync_port,
                self.config.port,
                audio_port,
                &self.versioned.head_hash(),
            ) {
                tracing::debug!(?e, "advertisement refresh failed");
            }
        }
    }

    pub(super) fn record_log(&mut self, line: String) {
        if self.log_tail.len() >= LOG_TAIL {
            self.log_tail.pop_front();
        }
        self.log_tail.push_back(line.clone());
        // Surface any structured signal embedded in the raw line (peer
        // connect/disconnect, screen switch). Subscribers that only watch
        // the raw log still see it via `Event::Log` below.
        if let Some(structured) = log_parse::parse(&line) {
            match &structured {
                Event::PeerConnected { name } => {
                    self.connected_peers.insert(name.clone());
                }
                Event::PeerDisconnected { name } => {
                    self.connected_peers.remove(name);
                }
                Event::ActiveScreen { name } => {
                    self.active_screen = Some(name.clone());
                }
                _ => {}
            }
            let _ = self.events.send(structured);
        }
        self.maybe_force_reconnect(&line);
        let _ = self.events.send(Event::Log { line });
    }

    /// Some Deskflow Core builds log "disconnected from server" but keep
    /// the process alive without reconnecting. The supervisor's exit-driven
    /// retry loop never engages in that case, so we kill the child on the
    /// signal — `handle_child_exit` then runs the normal client-reconnect
    /// path (gentle capped-backoff retry that never gives up).
    fn maybe_force_reconnect(&mut self, line: &str) {
        // Don't recurse on our own "[synbad] can't reach server …" status
        // line emitted by `handle_child_exit`.
        if line.starts_with("[synbad]") {
            return;
        }
        if !matches!(self.config.role, NodeRole::Client) {
            return;
        }
        if !log_parse::is_client_server_disconnect(line) {
            return;
        }
        let Some(tx) = self.child_kill.take() else {
            return;
        };
        tracing::warn!("core reported disconnect without exit; forcing restart");
        let _ = tx.send(());
    }

    pub(super) fn set_state(&mut self, new_state: DaemonState) {
        if self.state != new_state {
            if !new_state.is_running() {
                self.connected_peers.clear();
                self.active_screen = None;
            }
            tracing::debug!(?new_state, "state change");
            self.state = new_state.clone();
            let _ = self.events.send(Event::State { state: new_state });
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        for task in self
            .pairing_tasks
            .drain(..)
            .chain(self.audio_tasks.drain(..))
        {
            task.abort();
        }
        for (_, (_, task)) in self.sync_tasks.drain() {
            task.abort();
        }
        for task in [
            &mut self._pairing_listener,
            &mut self._sync_listener,
            &mut self._audio_listener,
            &mut self._audio_task,
        ] {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
    }
}

/// The signals that should shut the daemon down cleanly: Ctrl-C everywhere,
/// plus SIGTERM (systemd stop, logout, plain `kill`) and SIGHUP (controlling
/// terminal closed) on Unix. Handling them — rather than letting the default
/// action kill us — is what makes `stop_core` run, so the Deskflow child
/// isn't orphaned still holding the Core port.
///
/// The Unix handlers are installed once and live for the whole loop, so a
/// signal that arrives while another `select!` arm is running is queued,
/// not lost (a per-iteration `ctrl_c()` future has that gap).
struct ShutdownSignals {
    #[cfg(unix)]
    int: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hup: Option<tokio::signal::unix::Signal>,
}

impl ShutdownSignals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let install = |kind: SignalKind, name: &str| match signal(kind) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(?e, "could not install {name} handler");
                    None
                }
            };
            Self {
                int: install(SignalKind::interrupt(), "SIGINT"),
                term: install(SignalKind::terminate(), "SIGTERM"),
                hup: install(SignalKind::hangup(), "SIGHUP"),
            }
        }
        #[cfg(not(unix))]
        Self {}
    }

    /// Resolves with the signal's name once one arrives. A handler that
    /// failed to install simply never fires.
    async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            async fn next(sig: &mut Option<tokio::signal::unix::Signal>) {
                match sig {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending().await,
                }
            }
            tokio::select! {
                _ = next(&mut self.int) => "ctrl-c",
                _ = next(&mut self.term) => "SIGTERM",
                _ = next(&mut self.hup) => "SIGHUP",
            }
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
            "ctrl-c"
        }
    }
}
