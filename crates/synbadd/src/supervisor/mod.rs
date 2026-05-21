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

use synbad_config::{paths, Config};
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
/// A child that exits within this window of being spawned is treated as an
/// "instant fail" — usually a missing shared library, bad CLI, or refused
/// permission. We count consecutive instant-fails and stop the restart loop
/// after [`MAX_FAST_FAILS`] of them.
pub(super) const FAST_FAIL_WINDOW: Duration = Duration::from_secs(2);
pub(super) const MAX_FAST_FAILS: u32 = 5;
/// Client role: cap consecutive failed reconnects. A child that ran long
/// enough to clear [`FAST_FAIL_WINDOW`] resets the counter, so a transient
/// mid-session disconnect still gets a fresh budget — only an unreachable
/// server (3 fast failures in a row) makes us give up.
pub(super) const MAX_CLIENT_RECONNECTS: u32 = 3;

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
        let attempts = prev.map(|p| p.attempts + 1).unwrap_or(1);
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
    pub(super) log_tail: VecDeque<String>,
    pub(super) events: broadcast::Sender<Event>,
    /// `true` after a Start (or after a SetConfig while already running).
    /// Drives auto-restart when the Core exits unexpectedly.
    pub(super) desired_running: bool,
    /// Send `()` to terminate the supervised child. `None` when not running.
    pub(super) child_kill: Option<oneshot::Sender<()>>,
    pub(super) backoff: Duration,
    pub(super) log_rx: mpsc::Receiver<String>,
    pub(super) log_tx: mpsc::Sender<String>,
    pub(super) exit_rx: mpsc::Receiver<std::process::ExitStatus>,
    pub(super) exit_tx: mpsc::Sender<std::process::ExitStatus>,
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
    pub(super) _advertiser: Option<Advertiser>,
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
    /// Tasks kept alive so spawned pairing sessions aren't dropped.
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
    /// Outbound sync sessions kept alive while running. We GC finished
    /// handles like we do with pairing tasks.
    pub(super) sync_tasks: Vec<tokio::task::JoinHandle<()>>,

    /// Audio bridge handle (commands + events). `None` if audio is
    /// disabled in config at startup. Toggling `audio.enabled` requires a
    /// daemon restart in v1.
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

        let (exit_tx, exit_rx) = mpsc::channel::<std::process::ExitStatus>(16);
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

        let mut supervisor = Supervisor {
            config_path,
            config,
            versioned,
            versions_path,
            state: DaemonState::Stopped,
            log_tail: VecDeque::with_capacity(LOG_TAIL),
            events,
            desired_running: false,
            child_kill: None,
            backoff: MIN_BACKOFF,
            log_rx,
            log_tx,
            exit_rx,
            exit_tx,
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
            _advertiser: advertiser,
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
            sync_tasks: Vec::new(),
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

        // Bring the audio subsystem up if it was enabled in the saved
        // config. Failure here is logged but non-fatal — the daemon
        // still serves IPC and other peers continue to work.
        if supervisor.config.audio.enabled {
            if let Err(e) = supervisor.ensure_audio_subsystem().await {
                tracing::warn!(?e, "audio subsystem failed to start");
            }
        }

        Ok(supervisor)
    }

    pub async fn run(&mut self, mut listener: Listener) -> Result<()> {
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

            tokio::select! {
                _ = self.audio_reconcile.tick() => {
                    // Periodic safety net: re-attempt any audio session that
                    // *should* be live but isn't. Cheap no-op when the
                    // subsystem is disabled or every peer is already up.
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
                Some(status) = self.exit_rx.recv() => {
                    self.handle_child_exit(status).await;
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
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("ctrl-c, shutting down");
                    self.stop_core().await;
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
                let _ = self
                    .events
                    .send(Event::AudioPeerStatus { status });
            }
            A::Error { peer, message } => {
                let _ = self.events.send(Event::AudioError { peer, message });
            }
            A::DevicesChanged => {
                let _ = self.events.send(Event::AudioDevicesChanged);
            }
            A::SessionClosed { peer } => {
                self.audio_live.remove(&peer);
                tracing::debug!(peer = %peer, "audio session closed; reconcile will retry");
                // Don't surface as an IPC event — the GUI already sees
                // the session disappear from the next `AudioStatus`
                // snapshot, and a transient close that immediately
                // reconnects shouldn't generate a flash of error noise.
            }
        }
    }

    pub(super) fn gc_pairing_tasks(&mut self) {
        self.pairing_tasks.retain(|t| !t.is_finished());
    }

    fn handle_discovery(&mut self, ev: DiscoveryEvent) {
        match ev {
            DiscoveryEvent::Found(peer) => {
                // mdns-sd fires `ServiceResolved` once per resolved address,
                // so a peer with several IPs (loopback + LAN + docker) shows
                // up multiple times in quick succession. Skip the
                // re-broadcast if nothing the user cares about changed —
                // only the `host` flips between resolutions and any one
                // value is fine to keep.
                let unchanged = self
                    .peers
                    .get(&peer.machine_id)
                    .map(|p| {
                        p.machine_id == peer.machine_id
                            && p.fingerprint == peer.fingerprint
                            && p.config_head == peer.config_head
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
                    let _ = self.events.send(Event::PeerLost { machine_id: machine_id.clone() });
                    // Drop liveness/backoff so a re-find dials cleanly.
                    self.audio_live.remove(&machine_id);
                    self.audio_backoff.remove(&machine_id);
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
        self.audio = Some(handle);
        self._audio_task = Some(task);
        self._audio_listener = listener;
        self.audio_dial_deps = Some(dial_deps);
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
        if let Some(handle) = &self.audio {
            let _ = handle
                .commands_tx
                .send(synbad_audio::AudioCommand::Shutdown)
                .await;
        }
        if let Some(listener) = self._audio_listener.take() {
            listener.abort();
        }
        self.audio = None;
        self._audio_task = None;
        self.audio_dial_deps = None;
        self.audio_live.clear();
        self.audio_inflight.clear();
        self.audio_backoff.clear();
        tracing::info!("audio subsystem offline");
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
    /// Called from [`reconcile_audio_sessions`]; not directly anywhere
    /// else. The reconcile path is the only place this should fire —
    /// keeping it single-entry means the inflight/backoff bookkeeping is
    /// guaranteed consistent.
    fn dial_audio_one(&mut self, peer: DiscoveredPeer) {
        let Some(dial_deps) = self.audio_dial_deps.as_ref() else {
            return;
        };
        if peer.audio_port == 0 {
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
        let handle = crate::audio::spawn_outbound(
            peer,
            dial_deps.clone(),
            self.audio_dial_done_tx.clone(),
        );
        self.audio_tasks.push(handle);
        self.gc_audio_tasks();
    }

    /// Walk every visible peer and dial the ones that should have a
    /// session but don't. The single-entry helper [`dial_audio_one`]
    /// enforces all the per-peer gates; this function just iterates.
    ///
    /// Triggered from three places:
    /// 1. The 5 s `audio_reconcile` interval (safety net for failed
    ///    dials, dropped sessions, and config changes the bridge missed).
    /// 2. Right after [`ensure_audio_subsystem`] brings the subsystem up
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

    /// Resolve an outbound dial. On success we leave inflight set — the
    /// bridge will emit a `PeerStatus` event shortly that flips the peer
    /// into `audio_live`. On failure we evict inflight and arm backoff.
    pub(super) fn handle_audio_dial_outcome(
        &mut self,
        outcome: crate::audio::AudioDialOutcome,
    ) {
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
                let next =
                    AudioBackoff::after_failure(self.audio_backoff.get(&peer_machine_id));
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

    /// Open an outbound sync to a freshly-visible peer iff: it's trusted,
    /// it advertised a `sync_port`, and its `cfg` head looks different
    /// from ours. The session itself is harmless if heads actually match
    /// (the merge is a no-op), but skipping the trivial case avoids
    /// connection churn every time a peer's mDNS record refreshes.
    fn maybe_pull_from(&mut self, peer: DiscoveredPeer) {
        if peer.sync_port == 0 {
            return;
        }
        let head_match =
            !peer.config_head.is_empty() && peer.config_head == self.versioned.head_hash();
        if head_match {
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
        let handle = sync::spawn_outbound(peer, self.sync_deps.clone());
        self.sync_tasks.push(handle);
        self.gc_sync_tasks();
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
            let _ = self.events.send(structured);
        }
        let _ = self.events.send(Event::Log { line });
    }

    pub(super) fn set_state(&mut self, new_state: DaemonState) {
        if self.state != new_state {
            tracing::debug!(?new_state, "state change");
            self.state = new_state.clone();
            let _ = self.events.send(Event::State { state: new_state });
        }
    }
}
