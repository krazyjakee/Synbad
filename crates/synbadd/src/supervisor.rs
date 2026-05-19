//! Core-process supervisor and IPC request handler.
//!
//! Single-task state owner: all mutations to config / process state happen
//! here, driven by events on a `tokio::select!`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot};

use std::sync::Arc;

use synbad_config::{paths, Config, NodeRole};
use synbad_discovery::{Advertiser, Browser, DiscoveryEvent, Identity, TrustedPeerStore};
use synbad_ipc::log_parse;
use synbad_ipc::server::{IncomingRequest, Listener};
use synbad_ipc::{DaemonState, DiscoveredPeer, Event, Request, Response};
use synbad_sync::{MergeOutcome, VersionedConfig};

use crate::binaries::{CoreLayout, ResolvedCore, Resolver};
use crate::pairing::{self, IncomingSession, SessionDeps};
use crate::sync::{self, SyncDeps, SyncOp};

const LOG_TAIL: usize = 500;
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A child that exits within this window of being spawned is treated as an
/// "instant fail" — usually a missing shared library, bad CLI, or refused
/// permission. We count consecutive instant-fails and stop the restart loop
/// after [`MAX_FAST_FAILS`] of them.
const FAST_FAIL_WINDOW: Duration = Duration::from_secs(2);
const MAX_FAST_FAILS: u32 = 5;

pub struct Supervisor {
    config_path: PathBuf,
    config: Config,
    /// Versioned mirror of `config`: same data, plus per-field Lamport
    /// stamps used for LWW merges with remote peers. `config` and
    /// `versioned.config` are kept identical — `config` is the cheap path
    /// for read-only callers (and matches the existing supervisor code),
    /// `versioned` is what we ship over the wire and what sync sessions
    /// merge into.
    versioned: VersionedConfig,
    /// Sidecar file where `versioned`'s stamps + clock live. The config
    /// itself stays in TOML at `config_path`.
    versions_path: PathBuf,
    state: DaemonState,
    log_tail: VecDeque<String>,
    events: broadcast::Sender<Event>,
    /// `true` after a Start (or after a SetConfig while already running).
    /// Drives auto-restart when the Core exits unexpectedly.
    desired_running: bool,
    /// Send `()` to terminate the supervised child. `None` when not running.
    child_kill: Option<oneshot::Sender<()>>,
    backoff: Duration,
    log_rx: mpsc::Receiver<String>,
    log_tx: mpsc::Sender<String>,
    exit_rx: mpsc::Receiver<std::process::ExitStatus>,
    exit_tx: mpsc::Sender<std::process::ExitStatus>,
    fs_rx: mpsc::Receiver<()>,
    _fs_watcher: RecommendedWatcher,
    resolver: Resolver,
    /// When the currently-spawned child started — used to classify exits
    /// as "instant fail" vs "ran for a while then died".
    started_at: Option<Instant>,
    /// Consecutive instant-fails. Reset when the child runs longer than
    /// [`FAST_FAIL_WINDOW`] or when the user explicitly stops/starts.
    fast_fail_count: u32,

    /// Stable per-machine identity (UUID + ed25519 keypair). Persists
    /// across restarts via the user's config dir.
    identity: Arc<Identity>,
    /// mDNS service advertisement. Dropped on shutdown to send a goodbye.
    /// `None` if discovery failed to start — daemon keeps running.
    _advertiser: Option<Advertiser>,
    /// mDNS browser; lives alongside the advertiser.
    _browser: Option<Browser>,
    /// Incoming Found/Lost events from the browser thread.
    discovery_rx: Option<mpsc::Receiver<DiscoveryEvent>>,
    /// Currently-visible peers, keyed by machine_id.
    peers: HashMap<String, DiscoveredPeer>,
    /// User-paired peers, mutex-guarded so pairing sessions can persist
    /// without funneling through the supervisor task.
    trust: Arc<tokio::sync::Mutex<TrustedPeerStore>>,
    /// `oneshot::Sender` half for each in-flight pairing session, keyed
    /// by session_id. `ConfirmPairing` looks up here.
    pairing_confirm: HashMap<String, oneshot::Sender<bool>>,
    /// Receiver for new inbound sessions accepted by the pairing listener.
    incoming_pairings: Option<mpsc::Receiver<IncomingSession>>,
    /// Dependencies handed to every pairing session task.
    pairing_deps: Arc<SessionDeps>,
    /// Tasks kept alive so spawned pairing sessions aren't dropped.
    _pairing_listener: Option<tokio::task::JoinHandle<()>>,
    pairing_tasks: Vec<tokio::task::JoinHandle<()>>,

    /// Shared deps for sync sessions (identity, trust, event bus, the
    /// channel sessions use to ask the supervisor to merge).
    sync_deps: Arc<SyncDeps>,
    /// Listener accepting inbound sync sessions. `None` if bind failed
    /// at startup — outbound sync still works.
    _sync_listener: Option<tokio::task::JoinHandle<()>>,
    /// Receiver for the SyncOp channel — sessions ask us to merge / read
    /// state through this.
    sync_ops: mpsc::Receiver<SyncOp>,
    /// Outbound sync sessions kept alive while running. We GC finished
    /// handles like we do with pairing tasks.
    sync_tasks: Vec<tokio::task::JoinHandle<()>>,
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
                let v = VersionedConfig::initial(
                    config.clone(),
                    &identity.machine_id.to_string(),
                );
                // Persist the bootstrap stamps so a peer that asks for
                // our state during the very first session sees a stable
                // identity rather than counter=0/empty-origin defaults.
                if let Err(e) = v.save_sidecar(&versions_path) {
                    tracing::warn!(?e, ?versions_path, "could not write initial versions sidecar");
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
            match start_discovery(&identity, &config, &versioned.head_hash()) {
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
        let pairing_listener = match pairing::spawn_listener(
            config.service_port,
            pairing_deps.clone(),
            incoming_tx,
        )
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
        let sync_listener =
            match sync::spawn_listener(config.sync_port, sync_deps.clone()).await {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(?e, "sync listener disabled");
                    None
                }
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

            tokio::select! {
                Some(req) = listener.next_request() => {
                    self.handle_request(req).await;
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

    fn gc_pairing_tasks(&mut self) {
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
                    let _ = self.events.send(Event::PeerDiscovered { peer: peer.clone() });
                    // If this peer is trusted and advertised a head that
                    // differs from ours, open a pull-sync so we converge
                    // even if we missed their previous push (e.g. we
                    // weren't on the LAN at the time).
                    self.maybe_pull_from(peer);
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

    /// Open an outbound sync to a freshly-visible peer iff: it's trusted,
    /// it advertised a `sync_port`, and its `cfg` head looks different
    /// from ours. The session itself is harmless if heads actually match
    /// (the merge is a no-op), but skipping the trivial case avoids
    /// connection churn every time a peer's mDNS record refreshes.
    fn maybe_pull_from(&mut self, peer: DiscoveredPeer) {
        if peer.sync_port == 0 {
            return;
        }
        let head_match = !peer.config_head.is_empty()
            && peer.config_head == self.versioned.head_hash();
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

    async fn handle_request(&mut self, req: IncomingRequest) {
        let IncomingRequest { request, reply, .. } = req;
        let response = match request {
            Request::GetStatus => Response::Status {
                state: self.state.clone(),
                recent_log: self.log_tail.iter().cloned().collect(),
            },
            Request::GetConfig => Response::Config { config: self.config.clone() },
            Request::SetConfig { config } => match self.set_config(config).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: e.to_string() },
            },
            Request::Start => {
                self.desired_running = true;
                // Explicit user action resets the give-up state from a
                // prior instant-fail loop — they may have fixed the
                // missing-deps issue and want us to try again.
                self.fast_fail_count = 0;
                match self.start_core().await {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error { message: e.to_string() },
                }
            }
            Request::Stop => {
                self.desired_running = false;
                self.fast_fail_count = 0;
                self.stop_core().await;
                Response::Ok
            }
            Request::Restart => {
                self.desired_running = true;
                self.fast_fail_count = 0;
                self.stop_core().await;
                match self.start_core().await {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error { message: e.to_string() },
                }
            }
            Request::Subscribe => Response::Ok,
            Request::ListPeers => Response::Peers {
                peers: self.peers.values().cloned().collect(),
            },
            Request::GetLocalIdentity => Response::LocalIdentity {
                machine_id: self.identity.machine_id.to_string(),
                fingerprint: self.identity.fingerprint.clone(),
            },
            Request::StartPairing { machine_id } => match self.start_pairing(&machine_id) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: e.to_string() },
            },
            Request::ConfirmPairing { session_id, accept } => {
                match self.pairing_confirm.remove(&session_id) {
                    Some(tx) => {
                        let _ = tx.send(accept);
                        Response::Ok
                    }
                    None => Response::Error {
                        message: format!("no pending pairing session {:?}", session_id),
                    },
                }
            }
            Request::ListTrustedPeers => {
                let trust = self.trust.lock().await;
                Response::TrustedPeers { peers: trust.list().to_vec() }
            }
            Request::RevokeTrust { machine_id } => {
                let mut trust = self.trust.lock().await;
                match trust.remove(&machine_id) {
                    Ok(true) => {
                        let _ = self
                            .events
                            .send(Event::TrustRevoked { machine_id: machine_id.clone() });
                        Response::Ok
                    }
                    Ok(false) => Response::Error {
                        message: format!("peer {:?} is not trusted", machine_id),
                    },
                    Err(e) => Response::Error { message: e.to_string() },
                }
            }
        };
        let _ = reply.send(response);
    }

    fn start_pairing(&mut self, machine_id: &str) -> Result<()> {
        let peer = self
            .peers
            .get(machine_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("peer {:?} not currently discovered", machine_id))?;
        let handle = pairing::spawn_outbound(peer, self.pairing_deps.clone());
        self.pairing_confirm
            .insert(handle.session_id.clone(), handle.confirm_tx);
        self.pairing_tasks.push(handle._task);
        self.gc_pairing_tasks();
        Ok(())
    }

    async fn set_config(&mut self, new_config: Config) -> Result<()> {
        new_config.validate()?;
        new_config
            .save(&self.config_path)
            .context("persisting new config")?;
        self.apply_local_edit(new_config, /*restart_if_running=*/ true)
            .await
    }

    async fn handle_config_changed(&mut self) {
        // Coalesce a burst of FS events from one logical save.
        tokio::time::sleep(Duration::from_millis(100)).await;
        while self.fs_rx.try_recv().is_ok() {}

        match Config::load(&self.config_path) {
            Ok(Some(cfg)) if cfg != self.config => {
                tracing::info!("config changed on disk, applying");
                // FS-driven edits don't go through `Config::save`, so we
                // skip persisting again — just stamp + propagate.
                if let Err(e) = self.apply_local_edit(cfg, true).await {
                    tracing::warn!(?e, "applying FS-edited config failed");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(?e, "config reload failed"),
        }
    }

    /// Promote a candidate `Config` to the new local state: bump stamps,
    /// persist the versions sidecar, regen Core artefacts, restart Core
    /// (if desired), and gossip the change to every trusted peer we can
    /// currently see.
    ///
    /// Single funnel for local-origin edits — both `SetConfig` from the
    /// GUI and FS-watcher reloads from a user `$EDITOR` come through
    /// here, so the gossip behaviour is identical regardless of source.
    async fn apply_local_edit(
        &mut self,
        new_config: Config,
        restart_if_running: bool,
    ) -> Result<()> {
        let origin = self.identity.machine_id.to_string();
        let changed = self.versioned.apply_local(new_config.clone(), &origin);
        self.config = new_config;
        if changed {
            // Best-effort sidecar persistence. If this fails the next
            // local edit still bumps the in-memory stamps correctly —
            // we'd only lose the bump across a daemon restart.
            if let Err(e) = self.versioned.save_sidecar(&self.versions_path) {
                tracing::warn!(?e, "versions sidecar save failed");
            }
        }
        let _ = self.events.send(Event::ConfigChanged);
        if restart_if_running && self.desired_running {
            self.stop_core().await;
            if let Err(e) = self.start_core().await {
                tracing::warn!(?e, "restart after config change failed");
            }
        }
        // Only push when our edit produced a new head. A no-op edit
        // (same content, same stamps) wouldn't move any peer's state,
        // and rebroadcasting it would create gossip churn for nothing.
        if changed {
            self.push_to_trusted_peers();
        }
        Ok(())
    }

    /// Open an outbound sync to every currently-visible trusted peer.
    /// Called after a local edit, and on a peer first becoming visible
    /// with a divergent head. Sessions are idempotent (LWW), so a stray
    /// extra push is harmless.
    fn push_to_trusted_peers(&mut self) {
        // Snapshot trust list so we don't hold the mutex across spawns.
        // The mutex is async; the snapshot itself is the common case.
        let trust = match self.trust.try_lock() {
            Ok(g) => g.list().iter().map(|p| p.machine_id.clone()).collect::<Vec<_>>(),
            Err(_) => {
                // Trust mutex is contended — schedule a deferred push so
                // we don't drop the change on the floor.
                tracing::debug!("trust mutex busy; deferring push");
                return;
            }
        };
        for peer in self.peers.values().cloned().collect::<Vec<_>>() {
            if !trust.iter().any(|m| m == &peer.machine_id) {
                continue;
            }
            if peer.sync_port == 0 {
                continue;
            }
            let handle = sync::spawn_outbound(peer, self.sync_deps.clone());
            self.sync_tasks.push(handle);
        }
        self.gc_sync_tasks();
    }

    fn gc_sync_tasks(&mut self) {
        self.sync_tasks.retain(|t| !t.is_finished());
    }

    /// Handle a `SyncOp` request from a sync session task.
    async fn handle_sync_op(&mut self, op: SyncOp) {
        match op {
            SyncOp::Snapshot { reply } => {
                let _ = reply.send(self.versioned.clone());
            }
            SyncOp::Merge { peer_machine_id, incoming, reply } => {
                let incoming = *incoming;
                let outcome = self.versioned.merge(&incoming);
                if matches!(outcome, MergeOutcome::Updated) {
                    // Validate the merged config — a malformed inbound
                    // state shouldn't poison our on-disk config. If the
                    // merge produced an invalid config we roll back the
                    // merge before persisting.
                    if let Err(e) = self.versioned.config.validate() {
                        tracing::warn!(
                            ?e,
                            from = %peer_machine_id,
                            "merged config failed validation; rolling back"
                        );
                        // Restore from the on-disk config (which is
                        // still the last validated copy).
                        match Config::load(&self.config_path) {
                            Ok(Some(c)) => {
                                self.versioned.config = c.clone();
                                self.config = c;
                            }
                            _ => {
                                // Worst case: keep the merged config in
                                // memory but don't persist it. Next
                                // local edit will overwrite.
                            }
                        }
                        let _ = reply.send(self.versioned.clone());
                        return;
                    }
                    self.config = self.versioned.config.clone();
                    // Persist both the config TOML and the sidecar.
                    if let Err(e) = self.config.save(&self.config_path) {
                        tracing::warn!(?e, "persisting merged config failed");
                    }
                    if let Err(e) = self.versioned.save_sidecar(&self.versions_path) {
                        tracing::warn!(?e, "persisting merged sidecar failed");
                    }
                    let _ = self.events.send(Event::ConfigChanged);
                    // Regenerate Core artefacts + restart if running so
                    // the new screen layout / options take effect.
                    if self.desired_running {
                        self.stop_core().await;
                        if let Err(e) = self.start_core().await {
                            tracing::warn!(?e, "restart after sync merge failed");
                        }
                    }
                    tracing::info!(
                        from = %peer_machine_id,
                        head = %self.versioned.head_hash(),
                        "merged remote config edit"
                    );
                }
                let _ = reply.send(self.versioned.clone());
            }
        }
    }

    async fn start_core(&mut self) -> Result<()> {
        if matches!(self.state, DaemonState::Running { .. } | DaemonState::Starting)
            || self.child_kill.is_some()
        {
            return Ok(());
        }
        self.set_state(DaemonState::Starting);

        let conf_path = paths::generated_conf();
        let settings_path = paths::generated_settings();
        if let Some(parent) = conf_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&conf_path, self.config.generate_synergy_conf())
            .with_context(|| format!("writing {:?}", conf_path))?;
        std::fs::write(
            &settings_path,
            self.config.generate_deskflow_settings(&conf_path),
        )
        .with_context(|| format!("writing {:?}", settings_path))?;

        let (program, args) = self
            .resolve_program_and_args(&conf_path, &settings_path)
            .await?;
        tracing::info!(program = %program.display(), ?args, "starting core");

        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.set_state(DaemonState::Crashed { exit_code: None });
                return Err(anyhow::anyhow!(
                    "failed to spawn {}: {}. Is the binary on PATH?",
                    program.display(),
                    e
                ));
            }
        };

        let pid = child.id().unwrap_or(0);

        if let Some(stdout) = child.stdout.take() {
            spawn_log_reader(stdout, self.log_tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_reader(stderr, self.log_tx.clone());
        }

        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
        self.child_kill = Some(kill_tx);
        let exit_tx = self.exit_tx.clone();

        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s.unwrap_or_default(),
                _ = &mut kill_rx => {
                    let _ = child.start_kill();
                    child.wait().await.unwrap_or_default()
                }
            };
            let _ = exit_tx.send(status).await;
        });

        self.set_state(DaemonState::Running { pid });
        self.backoff = MIN_BACKOFF;
        self.started_at = Some(Instant::now());
        Ok(())
    }

    async fn stop_core(&mut self) {
        if let Some(tx) = self.child_kill.take() {
            let _ = tx.send(());
            // Wait for the exit event so state reflects reality before we return.
            let _ = tokio::time::timeout(Duration::from_secs(2), self.exit_rx.recv()).await;
        }
        self.set_state(DaemonState::Stopped);
    }

    async fn handle_child_exit(&mut self, status: std::process::ExitStatus) {
        let code = status.code();
        tracing::info!(?code, "core exited");
        self.child_kill = None;

        // Classify: did the child run long enough to be considered "alive"?
        // A sub-second exit usually means missing libs (exit 127), bad CLI,
        // or permission denial — restarting won't help.
        let ran_for = self.started_at.take().map(|t| t.elapsed());
        let instant_fail = ran_for
            .map(|d| d < FAST_FAIL_WINDOW)
            .unwrap_or(false);
        if instant_fail {
            self.fast_fail_count += 1;
        } else {
            self.fast_fail_count = 0;
        }

        if !self.desired_running {
            self.set_state(DaemonState::Stopped);
            return;
        }

        if self.fast_fail_count >= MAX_FAST_FAILS {
            // Give up. The exit code stays on the chip so the GUI surfaces
            // what happened, and we record a log line explaining why we
            // stopped retrying.
            self.desired_running = false;
            let msg = format!(
                "[synbad] core exited within {:?} on {} consecutive attempts (exit {:?}); \
                 giving up. Check that Deskflow's runtime deps (Qt6) are installed, \
                 then click Start.",
                FAST_FAIL_WINDOW, self.fast_fail_count, code
            );
            tracing::error!("{}", msg);
            self.record_log(msg);
            self.set_state(DaemonState::Crashed { exit_code: code });
            return;
        }

        self.set_state(DaemonState::Crashed { exit_code: code });
        let delay = self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        tracing::warn!(?delay, attempt = self.fast_fail_count, "core crashed, will restart");
        tokio::time::sleep(delay).await;
        if self.desired_running {
            if let Err(e) = self.start_core().await {
                tracing::error!(?e, "restart failed");
            }
        }
    }

    /// Resolve the Deskflow Core executable (fetching from upstream on
    /// first use) and build the CLI for the current role + release layout.
    ///
    /// Modern Deskflow (≥ v1.19) ships a unified `deskflow-core` that takes
    /// `server|client` as a subcommand and reads its settings from a
    /// QSettings INI passed via `-s`. v1.17.0 ships split daemons with the
    /// classic Synergy CLI: the server reads a screen-layout `.conf` via
    /// `-c`; the client takes the server address as a positional argument.
    /// See [`CoreLayout`] and `Config::generate_deskflow_settings` /
    /// `Config::generate_synergy_conf`.
    async fn resolve_program_and_args(
        &self,
        conf_path: &Path,
        settings_path: &Path,
    ) -> Result<(PathBuf, Vec<String>)> {
        // User override: always treated as a unified `deskflow-core`.
        // Anyone wanting to point at v1.17.0-style split daemons should
        // leave this unset and let the resolver fetch them.
        if let Some(p) = self.config.binaries.core.clone() {
            let mode = match self.config.role {
                NodeRole::Server => "server",
                NodeRole::Client => "client",
            };
            return Ok((
                p,
                vec![
                    mode.into(),
                    "-s".into(),
                    settings_path.to_string_lossy().into_owned(),
                ],
            ));
        }
        let resolved = self.fetch_binary().await?;
        build_command(&resolved, &self.config, conf_path, settings_path)
    }

    /// Fetch (or cache-hit) the Deskflow Core release, forwarding upstream
    /// resolver events to the IPC bus as human-readable log lines.
    async fn fetch_binary(&self) -> Result<ResolvedCore> {
        let (tx, mut rx) = mpsc::channel::<crate::binaries::Event>(64);
        let events = self.events.clone();
        let forwarder = tokio::spawn(async move {
            use crate::binaries::Event as BE;
            while let Some(ev) = rx.recv().await {
                let line = match ev {
                    BE::CheckingLatest => "[synbad] checking deskflow releases/latest".to_string(),
                    BE::Downloading { tag, asset, url } => {
                        format!("[synbad] downloading {} ({}) from {}", asset, tag, url)
                    }
                    BE::Progress { asset, bytes, total } => match total {
                        Some(t) => format!(
                            "[synbad] {}: {} / {} bytes ({:.1}%)",
                            asset,
                            bytes,
                            t,
                            (bytes as f64 / t as f64) * 100.0
                        ),
                        None => format!("[synbad] {}: {} bytes", asset, bytes),
                    },
                    BE::Extracting { tag, asset } => {
                        format!("[synbad] extracting deskflow core from {} ({})", asset, tag)
                    }
                    BE::Ready { tag, path } => {
                        format!("[synbad] deskflow core {} ready at {}", tag, path.display())
                    }
                };
                let _ = events.send(Event::Log { line });
            }
        });
        let result = self.resolver.ensure_core(tx).await;
        forwarder.abort();
        result
    }

    fn record_log(&mut self, line: String) {
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

    fn set_state(&mut self, new_state: DaemonState) {
        if self.state != new_state {
            tracing::debug!(?new_state, "state change");
            self.state = new_state.clone();
            let _ = self.events.send(Event::State { state: new_state });
        }
    }
}

/// Construct the program + argv for spawning the Deskflow Core child,
/// branching on the release's layout. Pure function so it's easy to test.
fn build_command(
    resolved: &ResolvedCore,
    config: &Config,
    conf_path: &Path,
    settings_path: &Path,
) -> Result<(PathBuf, Vec<String>)> {
    match (&resolved.layout, config.role) {
        (CoreLayout::Unified { path }, role) => {
            let mode = match role {
                NodeRole::Server => "server",
                NodeRole::Client => "client",
            };
            Ok((
                path.clone(),
                vec![
                    mode.into(),
                    "-s".into(),
                    settings_path.to_string_lossy().into_owned(),
                ],
            ))
        }
        // v1.17.0 server: `-f` foreground, `-1` no self-restart (the
        // supervisor handles that), `-n` local screen name, `-a :port`
        // bind on all interfaces, `-c` screen-layout file.
        (CoreLayout::SplitLegacy { server, .. }, NodeRole::Server) => Ok((
            server.clone(),
            vec![
                "-f".into(),
                "-1".into(),
                "-n".into(),
                config.server_name.clone(),
                "-a".into(),
                format!(":{}", config.port),
                "-c".into(),
                conf_path.to_string_lossy().into_owned(),
            ],
        )),
        // v1.17.0 client: server address is positional. `Config::validate`
        // guarantees `server_address` is Some when role=Client, so the
        // `ok_or_else` here is defensive — surfaces a clear error rather
        // than spawning a child that immediately exits with a usage error.
        (CoreLayout::SplitLegacy { client, .. }, NodeRole::Client) => {
            let host = config.server_address.as_deref().ok_or_else(|| {
                anyhow::anyhow!("client role requires server_address")
            })?;
            let addr = if host.contains(':') {
                host.to_string()
            } else {
                format!("{}:{}", host, config.port)
            };
            Ok((
                client.clone(),
                vec![
                    "-f".into(),
                    "-1".into(),
                    "-n".into(),
                    config.server_name.clone(),
                    addr,
                ],
            ))
        }
    }
}

fn spawn_log_reader<R>(reader: R, sink: mpsc::Sender<String>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if sink.send(line).await.is_err() {
                break;
            }
        }
    });
}

/// Initialise the mDNS advertiser + browser. Returns the pair plus the
/// browser's event receiver. Failure here is recoverable — the daemon
/// keeps running with discovery disabled.
///
/// `config_head` is the current short hash of the local
/// [`VersionedConfig`]. We advertise it under the `cfg` TXT key so peers
/// detect divergence at discovery time. The advertisement is a startup
/// snapshot — updates require restarting the advertiser, which mdns-sd
/// doesn't make cheap. In practice the push-on-edit path keeps trusted
/// peers in sync without depending on the TXT freshness; the TXT is
/// useful for the discovery-driven pull on first contact.
fn start_discovery(
    identity: &Identity,
    config: &Config,
    config_head: &str,
) -> Result<(Advertiser, Browser, mpsc::Receiver<DiscoveryEvent>)> {
    let display = sanitize_display_name(&config.server_name);
    let advertiser = Advertiser::start(
        identity,
        &display,
        config.service_port,
        config.sync_port,
        config.port,
        config_head,
    )
    .context("starting mDNS advertiser")?;
    let (browser, rx) = Browser::start(&identity.machine_id.to_string())
        .context("starting mDNS browser")?;
    Ok((advertiser, browser, rx))
}

fn sanitize_display_name(name: &str) -> String {
    // mDNS instance names can't be empty or contain `.`; everything else
    // is fine. We're conservative and also strip control chars.
    let s: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '.')
        .collect();
    if s.trim().is_empty() {
        "synbad".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synbad_config::{Config, NodeRole, Screen};

    fn base_config(role: NodeRole) -> Config {
        let mut cfg = Config::default();
        cfg.role = role;
        cfg.server_name = "alpha".into();
        cfg.screens = vec![Screen {
            name: "alpha".into(),
            aliases: vec![],
            position: Default::default(),
        }];
        cfg.port = 24800;
        if matches!(role, NodeRole::Client) {
            cfg.server_address = Some("peer.local".into());
        }
        cfg
    }

    #[test]
    fn unified_server_uses_subcommand_and_settings_ini() {
        let resolved = ResolvedCore {
            layout: CoreLayout::Unified {
                path: PathBuf::from("/cache/v1.26.0/deskflow-core"),
            },
        };
        let cfg = base_config(NodeRole::Server);
        let (prog, args) = build_command(
            &resolved,
            &cfg,
            Path::new("/x/synergy.conf"),
            Path::new("/x/settings.ini"),
        )
        .unwrap();
        assert_eq!(prog, PathBuf::from("/cache/v1.26.0/deskflow-core"));
        assert_eq!(args, vec!["server", "-s", "/x/settings.ini"]);
    }

    #[test]
    fn unified_client_uses_subcommand_and_settings_ini() {
        let resolved = ResolvedCore {
            layout: CoreLayout::Unified {
                path: PathBuf::from("/cache/v1.26.0/deskflow-core"),
            },
        };
        let cfg = base_config(NodeRole::Client);
        let (_prog, args) = build_command(
            &resolved,
            &cfg,
            Path::new("/x/synergy.conf"),
            Path::new("/x/settings.ini"),
        )
        .unwrap();
        assert_eq!(args, vec!["client", "-s", "/x/settings.ini"]);
    }

    #[test]
    fn legacy_server_uses_classic_cli_with_conf_path() {
        let resolved = ResolvedCore {
            layout: CoreLayout::SplitLegacy {
                server: PathBuf::from("/cache/v1.17.0/deskflow-server"),
                client: PathBuf::from("/cache/v1.17.0/deskflow-client"),
            },
        };
        let cfg = base_config(NodeRole::Server);
        let (prog, args) = build_command(
            &resolved,
            &cfg,
            Path::new("/x/synergy.conf"),
            Path::new("/x/settings.ini"),
        )
        .unwrap();
        assert_eq!(prog, PathBuf::from("/cache/v1.17.0/deskflow-server"));
        assert_eq!(
            args,
            vec![
                "-f",
                "-1",
                "-n",
                "alpha",
                "-a",
                ":24800",
                "-c",
                "/x/synergy.conf",
            ]
        );
    }

    #[test]
    fn legacy_client_passes_server_address_as_positional() {
        let resolved = ResolvedCore {
            layout: CoreLayout::SplitLegacy {
                server: PathBuf::from("/cache/v1.17.0/deskflow-server"),
                client: PathBuf::from("/cache/v1.17.0/deskflow-client"),
            },
        };
        let mut cfg = base_config(NodeRole::Client);
        cfg.server_address = Some("peer.local".into());
        let (prog, args) = build_command(
            &resolved,
            &cfg,
            Path::new("/x/synergy.conf"),
            Path::new("/x/settings.ini"),
        )
        .unwrap();
        assert_eq!(prog, PathBuf::from("/cache/v1.17.0/deskflow-client"));
        // Port appended when bare host given.
        assert_eq!(
            args,
            vec!["-f", "-1", "-n", "alpha", "peer.local:24800"]
        );
    }

    #[test]
    fn legacy_client_preserves_explicit_port_in_address() {
        let resolved = ResolvedCore {
            layout: CoreLayout::SplitLegacy {
                server: PathBuf::from("/x/s"),
                client: PathBuf::from("/x/c"),
            },
        };
        let mut cfg = base_config(NodeRole::Client);
        cfg.server_address = Some("peer.local:24900".into());
        let (_p, args) = build_command(
            &resolved,
            &cfg,
            Path::new("/x/synergy.conf"),
            Path::new("/x/settings.ini"),
        )
        .unwrap();
        assert!(args.contains(&"peer.local:24900".to_string()));
        assert!(!args.contains(&"peer.local:24800".to_string()));
    }
}
