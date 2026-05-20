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
    /// `maybe_dial_audio`. `None` when audio is disabled — outbound dial
    /// is skipped in that case.
    pub(super) audio_dial_deps: Option<Arc<crate::audio::AudioListenerDeps>>,
    /// Outbound audio handshake tasks kept alive while running. GCed
    /// alongside `sync_tasks` / `pairing_tasks`.
    pub(super) audio_tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Supervisor {
    pub async fn new(config_path: PathBuf, events: broadcast::Sender<Event>) -> Result<Self> {
        let config = Config::load(&config_path)?.unwrap_or_default();
        let versions_path = paths::config_versions_file();

        let (log_tx, log_rx) = mpsc::channel::<String>(1024);
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

        // Audio bridge: only spawned if explicitly enabled in config.
        // Toggling `audio.enabled` requires a daemon restart in v1; the
        // GUI surfaces this to the user. `dial_deps` is the same struct
        // the listener uses, kept around so `maybe_dial_audio` can fire
        // outbound handshakes without rebuilding it each time.
        let (audio_handle, audio_task, audio_listener, audio_dial_deps) = if config.audio.enabled {
            let bridge = synbad_audio::AudioBridge::new(
                config.audio.clone(),
                identity.clone(),
                trust.clone(),
            );
            let (handle, task) = bridge.spawn();
            let listener_deps = Arc::new(crate::audio::AudioListenerDeps {
                identity: identity.clone(),
                trust: trust.clone(),
                bridge_commands: handle.commands_tx.clone(),
            });
            let listener =
                match crate::audio::spawn_listener(config.audio.signal_port, listener_deps.clone())
                    .await
                {
                    Ok(h) => Some(h),
                    Err(e) => {
                        tracing::warn!(?e, "audio signal listener disabled");
                        None
                    }
                };
            (Some(handle), Some(task), listener, Some(listener_deps))
        } else {
            (None, None, None, None)
        };

        Ok(Supervisor {
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
            audio: audio_handle,
            _audio_task: audio_task,
            _audio_listener: audio_listener,
            audio_dial_deps,
            audio_tasks: Vec::new(),
        })
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

    /// Forward an event from the audio bridge onto the supervisor's event
    /// bus so the GUI sees it.
    fn handle_audio_event(&self, ev: synbad_audio::AudioEvent) {
        use synbad_audio::AudioEvent as A;
        let ipc_event = match ev {
            A::PeerStatus(status) => Event::AudioPeerStatus { status },
            A::Error { peer, message } => Event::AudioError { peer, message },
            A::DevicesChanged => Event::AudioDevicesChanged,
        };
        let _ = self.events.send(ipc_event);
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
                    self.maybe_pull_from(peer.clone());
                    // Independently consider opening an audio session.
                    // Driven off the same trigger (peer became visible)
                    // so a fresh LAN connection brings audio up without
                    // user action.
                    self.maybe_dial_audio(peer);
                }
            }
            DiscoveryEvent::Lost { machine_id } => {
                if self.peers.remove(&machine_id).is_some() {
                    tracing::info!(%machine_id, "peer lost");
                    let _ = self.events.send(Event::PeerLost { machine_id });
                }
            }
        }
    }

    /// Open an outbound audio session to a freshly-visible peer iff:
    /// - audio is enabled on this daemon,
    /// - the peer's routing for us would actually do work (skipping this
    ///   check would leave dead empty sessions for peers the user has
    ///   explicitly disabled),
    /// - the peer advertised an `audio_port`,
    /// - the peer is in the trust store, and
    /// - our `machine_id` sorts lower than theirs (glare rule — only
    ///   one side dials so we don't end up with two sessions).
    ///
    /// Reconnect path: after a network blip long enough for mDNS records
    /// to expire (~120 s default TTL), the peer fires `DiscoveryEvent::Lost`
    /// then `Found` when it reappears. `was_present` flips false, which
    /// re-enters this function. Short blips below the mDNS TTL keep the
    /// peer "visible" and currently don't auto-redial — the bridge's
    /// glare rule still picks up cleanly if the peer dials us first.
    fn maybe_dial_audio(&mut self, peer: DiscoveredPeer) {
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
            // The other side dials. We accept on the listener.
            return;
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
        let handle = crate::audio::spawn_outbound(peer, dial_deps.clone());
        self.audio_tasks.push(handle);
        self.gc_audio_tasks();
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
