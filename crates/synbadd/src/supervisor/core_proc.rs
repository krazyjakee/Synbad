//! Deskflow Core child-process lifecycle and the off-loop helpers it
//! depends on.
//!
//! Owns the start → resolve → spawn → exit → restart pipeline plus the
//! pure-ish helpers that don't touch `Supervisor` state: argv
//! construction, log-pipe glue, binary fetching, mDNS startup. The
//! free functions live next to the methods that call them so the whole
//! "bring up / tear down a Core" story sits in one file.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot};

use synbad_config::{paths, Config, NodeRole};
use synbad_discovery::{Advertiser, Browser, DiscoveryEvent, Identity};
use synbad_ipc::{DaemonState, Event};

use crate::binaries::{CoreLayout, ResolvedCore, Resolver};

use super::{
    CoreResolveOutcome, Supervisor, CLIENT_MAX_BACKOFF, FAST_FAIL_WINDOW, MAX_BACKOFF,
    MAX_FAST_FAILS, MIN_BACKOFF,
};

impl Supervisor {
    /// Begin starting the Core. Writes the generated artefacts, then kicks
    /// binary resolution onto a background task and returns immediately —
    /// the child is actually spawned later in [`Self::on_core_resolved`]
    /// when the result lands on `core_resolve_rx`.
    ///
    /// This indirection is the fix for the daemon freezing while a Core
    /// download is in flight: `ensure_core` can take many seconds (GitHub
    /// API, a ~27 MB asset, archive extraction), and the supervisor's
    /// `select!` loop also services IPC, pairing, discovery and sync.
    /// Awaiting the download here used to block all of them — pairing in
    /// particular looked like "clicking Pair does nothing".
    ///
    /// Infallible by design: every failure past this point (resolution,
    /// writing the generated config, spawning) is handled in
    /// [`Self::on_core_resolved`], which schedules a retry, so no caller can
    /// strand the supervisor in `Starting` with nothing pending.
    pub(super) async fn start_core(&mut self) {
        // Whoever is starting us now supersedes any pending auto-restart.
        self.restart_at = None;
        if matches!(self.state, DaemonState::Running { .. }) || self.child_kill.is_some() {
            return;
        }
        self.set_state(DaemonState::Starting);
        if self.core_resolving {
            // A resolution (possibly a first-run download) is already in
            // flight; it spawns from the live config when it lands.
            return;
        }

        self.core_resolving = true;
        let resolver = self.resolver.clone();
        let config = self.config.clone();
        let events = self.events.clone();
        let tx = self.core_resolve_tx.clone();
        tokio::spawn(async move {
            let outcome = resolve_core(&resolver, &config, &events)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(outcome).await;
        });
    }

    /// Write the generated screen layout + Deskflow settings the Core reads.
    /// Done right before spawning (not when the start was requested) so a
    /// config change that landed during a slow download is honoured.
    fn write_core_artefacts(&self) -> Result<()> {
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
        Ok(())
    }

    /// A start attempt failed before a child was running. Surface it and
    /// arm a retry: these failures (network, a transient spawn error, a
    /// briefly unwritable state dir) usually clear on their own.
    fn start_failed(&mut self, what: String) {
        let delay = self.schedule_restart();
        let msg = format!("[synbad] {what}; retrying in {delay:?}");
        tracing::error!("{}", msg);
        self.record_log(msg);
        self.set_state(DaemonState::Crashed { exit_code: None });
    }

    /// Spawn the Core child from a resolved binary, wiring up log readers,
    /// the kill channel, and the exit watcher. Synchronous and fast — all
    /// the slow work happened in the resolution task.
    fn spawn_child(&mut self, program: PathBuf, args: Vec<String>) -> Result<()> {
        tracing::info!(program = %program.display(), ?args, "starting core");

        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Backstop for the cases our shutdown path never runs (SIGKILL, OOM
        // kill, abort): have the kernel SIGTERM the Core when we die, so an
        // orphaned deskflow-server can't keep holding the Core port and make
        // the next daemon's server fast-fail on bind. The signal fires when
        // the forking *thread* exits; tokio worker threads live as long as
        // the runtime, i.e. as long as the daemon.
        #[cfg(target_os = "linux")]
        // SAFETY: the closure runs in the forked child before exec and only
        // makes an async-signal-safe syscall.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn {}: {}. Is the binary on PATH?",
                program.display(),
                e
            )
        })?;

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
            let _ = exit_tx.send((pid, status)).await;
        });

        self.child_pid = Some(pid);
        self.set_state(DaemonState::Running { pid });
        // NB: backoff is intentionally *not* reset here. Spawning is not proof
        // the link came up — a client dialing an unreachable server spawns,
        // instantly fails, and respawns. Resetting on spawn would pin the
        // backoff at MIN and hammer the server every ~500ms. It's reset only
        // once a child clears FAST_FAIL_WINDOW (see `handle_child_exit`) or on
        // an explicit user Start/Restart.
        self.started_at = Some(Instant::now());
        Ok(())
    }

    /// Handle the result of an off-loop Core resolution. Spawns the child
    /// if we still want one. A failed resolution (usually GitHub being
    /// unreachable on a first-run download) is retried with backoff —
    /// it's transient, and giving up would leave sharing off until the
    /// user noticed and clicked Start.
    pub(super) async fn on_core_resolved(&mut self, outcome: CoreResolveOutcome) {
        self.core_resolving = false;

        // The user may have hit Stop, or a Restart may have superseded
        // this resolution while it was downloading. Don't spawn a child
        // nobody asked for.
        if !self.desired_running {
            return;
        }
        if matches!(self.state, DaemonState::Running { .. }) || self.child_kill.is_some() {
            return;
        }

        let resolved = match outcome {
            Ok(r) => r,
            Err(reason) => {
                self.start_failed(format!("could not obtain Deskflow Core: {reason}"));
                return;
            }
        };

        if let Err(e) = self.write_core_artefacts() {
            self.start_failed(format!("{e:#}"));
            return;
        }

        // Rebuild argv from the *current* config so a role/address change
        // that landed while the download ran is honoured.
        let conf_path = paths::generated_conf();
        let settings_path = paths::generated_settings();
        let (program, args) =
            match build_command(&resolved, &self.config, &conf_path, &settings_path) {
                Ok(pa) => pa,
                Err(e) => {
                    // A config problem (e.g. client with no server address):
                    // retrying can't fix it, and the config edit that does
                    // will restart the Core itself.
                    let msg = format!("[synbad] bad Core command line: {e:#}");
                    tracing::error!("{}", msg);
                    self.record_log(msg);
                    self.set_state(DaemonState::Crashed { exit_code: None });
                    return;
                }
            };

        if let Err(e) = self.spawn_child(program, args) {
            self.start_failed(format!("{e:#}"));
        }
    }

    /// Bounce the Core with a fresh retry budget. Used for explicit
    /// Restarts and whenever the Core's inputs change (server address,
    /// role, layout): new settings deserve an immediate attempt rather than
    /// inheriting a reconnect loop's backed-off delay.
    pub(super) async fn restart_core(&mut self) {
        self.fast_fail_count = 0;
        self.backoff = MIN_BACKOFF;
        self.stop_core().await;
        self.start_core().await;
    }

    pub(super) async fn stop_core(&mut self) {
        self.restart_at = None;
        // Whatever child we had is being retired; if its exit lands after
        // the timeout below, `handle_child_exit` must ignore it.
        let pid = self.child_pid.take();
        if let Some(tx) = self.child_kill.take() {
            let _ = tx.send(());
            // Wait for *this* child's exit so state reflects reality before
            // we return (and a server's replacement doesn't race it for the
            // port). Skip stale exits from children an earlier stop gave up
            // on — returning on one of those would be returning early.
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                while let Some((exited, _)) = self.exit_rx.recv().await {
                    if Some(exited) == pid {
                        break;
                    }
                }
            })
            .await;
        }
        self.set_state(DaemonState::Stopped);
    }

    pub(super) async fn handle_child_exit(&mut self, pid: u32, status: std::process::ExitStatus) {
        let code = status.code();
        if self.child_pid != Some(pid) {
            // A child we already stopped, exiting after `stop_core` gave up
            // waiting on it. Acting on it would clobber the state of (and
            // drop the kill handle for) the child that replaced it.
            tracing::debug!(pid, ?code, "ignoring exit of superseded core");
            return;
        }
        tracing::info!(?code, "core exited");
        self.child_pid = None;
        self.child_kill = None;

        // Classify: did the child run long enough to be considered "alive"?
        // A sub-second exit usually means missing libs (exit 127), bad CLI,
        // or permission denial — restarting won't help.
        let ran_for = self.started_at.take().map(|t| t.elapsed());
        let instant_fail = ran_for.map(|d| d < FAST_FAIL_WINDOW).unwrap_or(false);
        if instant_fail {
            self.fast_fail_count += 1;
        } else {
            // The child proved it could stay up past the fast-fail window —
            // this is a mid-session drop, not a startup/reachability failure.
            // Reset both the fast-fail budget and the reconnect backoff so
            // recovery starts fast again.
            self.fast_fail_count = 0;
            self.backoff = MIN_BACKOFF;
        }

        if !self.desired_running {
            self.set_state(DaemonState::Stopped);
            return;
        }

        // The two roles fail for different reasons and so are handled
        // differently. Server-role instant-fails usually mean a startup
        // problem (port in use, missing libs) that retrying won't fix, so we
        // give up after MAX_FAST_FAILS. Client-role means the server is
        // unreachable — a paired, enabled link is level-triggered, so we keep
        // gently retrying with capped exponential backoff and recover on our
        // own once the server comes back (reboot, network blip, or a server
        // that simply started after us). The shared reset path (a child that
        // clears FAST_FAIL_WINDOW) gives a mid-session drop a fresh, fast
        // budget for either role.
        let is_client = matches!(self.config.role, NodeRole::Client);

        if !is_client && self.fast_fail_count >= MAX_FAST_FAILS {
            // Server role gives up. The exit code stays on the chip so the GUI
            // surfaces what happened, plus a log line explaining the stop.
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
            // Giving up clears the run state, so audio must follow it down —
            // keep the "audio online iff started" invariant that the Start/Stop
            // handlers uphold, rather than leaving a bridge up under a stopped
            // Synbad. No-op if audio was never online.
            let _ = self.reconcile_audio_subsystem().await;
            return;
        }

        let delay = self.schedule_restart();
        if is_client {
            // Level-triggered reconnect: surface a non-terminal `Reconnecting`
            // status (not `Crashed`) so the UI shows we're still trying, and
            // keep retrying indefinitely until the server is reachable or the
            // user stops. `attempt` is the consecutive fast-fail count; a
            // long-lived run that just got dropped resets it to "attempt 1".
            let attempt = self.fast_fail_count.saturating_add(1);
            self.set_state(DaemonState::Reconnecting {
                attempt,
                next_retry_secs: delay.as_secs(),
            });
            self.record_log(format!(
                "[synbad] can't reach server (exit {:?}); retrying in {:?} (attempt {})",
                code, delay, attempt
            ));
        } else {
            self.set_state(DaemonState::Crashed { exit_code: code });
        }
        tracing::warn!(
            ?delay,
            attempt = self.fast_fail_count,
            "core exited, will restart"
        );
    }

    /// Arm the restart timer the `select!` loop waits on, using the
    /// current backoff, and double the backoff for next time. Returns the
    /// delay armed. Never sleeps inline — blocking the loop here used to
    /// freeze IPC (including Stop) for up to [`MAX_BACKOFF`] per retry.
    fn schedule_restart(&mut self) -> Duration {
        let (delay, next) = restart_backoff(self.backoff, self.config.role);
        self.backoff = next;
        self.restart_at = Some(tokio::time::Instant::now() + delay);
        delay
    }
}

/// `(delay to use now, backoff for next time)` given the current backoff:
/// capped exponential, with a tighter cap for clients so they notice a
/// returning server quickly.
fn restart_backoff(backoff: Duration, role: NodeRole) -> (Duration, Duration) {
    let cap = match role {
        NodeRole::Client => CLIENT_MAX_BACKOFF,
        NodeRole::Server => MAX_BACKOFF,
    };
    let delay = backoff.min(cap);
    (delay, (delay * 2).min(cap))
}

/// Resolve the Deskflow Core binary, fetching from upstream on first use.
///
/// Runs on a detached task (see [`Supervisor::start_core`]) so the network
/// work never blocks the supervisor loop. A user-set `binaries.core`
/// override short-circuits the fetch and is always treated as a unified
/// `deskflow-core`; argv for both paths is built later by [`build_command`]
/// from the live config.
async fn resolve_core(
    resolver: &Resolver,
    config: &Config,
    events: &broadcast::Sender<Event>,
) -> Result<ResolvedCore> {
    if let Some(path) = config.binaries.core.clone() {
        return Ok(ResolvedCore {
            layout: CoreLayout::Unified { path },
        });
    }
    fetch_binary(resolver, events).await
}

/// Fetch (or cache-hit) the Deskflow Core release, forwarding upstream
/// resolver events to the IPC bus as human-readable log lines.
async fn fetch_binary(
    resolver: &Resolver,
    events: &broadcast::Sender<Event>,
) -> Result<ResolvedCore> {
    let (tx, mut rx) = mpsc::channel::<crate::binaries::Event>(64);
    let events = events.clone();
    let forwarder = tokio::spawn(async move {
        use crate::binaries::Event as BE;
        while let Some(ev) = rx.recv().await {
            let line = match ev {
                BE::CheckingLatest => "[synbad] checking deskflow releases/latest".to_string(),
                BE::Downloading { tag, asset, url } => {
                    format!("[synbad] downloading {} ({}) from {}", asset, tag, url)
                }
                BE::Progress {
                    asset,
                    bytes,
                    total,
                } => match total {
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
    let result = resolver.ensure_core(tx).await;
    forwarder.abort();
    result
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
            let host = config
                .server_address
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("client role requires server_address"))?;
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
/// [`synbad_sync::VersionedConfig`]. We advertise it under the `cfg` TXT
/// key so peers detect divergence at discovery time. The advertisement is
/// a startup snapshot — updates require restarting the advertiser, which
/// mdns-sd doesn't make cheap. In practice the push-on-edit path keeps
/// trusted peers in sync without depending on the TXT freshness; the TXT
/// is useful for the discovery-driven pull on first contact.
pub(super) fn start_discovery(
    identity: &Identity,
    config: &Config,
    config_head: &str,
) -> Result<(Advertiser, Browser, mpsc::Receiver<DiscoveryEvent>)> {
    let display = sanitize_display_name(&config.server_name);
    // Only advertise the audio port when the user has actually opted
    // into the audio bridge — a peer that sees an `audio_port` of zero
    // would attempt to dial and fail. Keeping the key absent matches
    // the "no audio here" reading on the consumer side.
    let advertised_audio_port = if config.audio.enabled {
        config.audio.signal_port
    } else {
        0
    };
    let advertiser = Advertiser::start(
        identity,
        &display,
        config.service_port,
        config.sync_port,
        config.port,
        advertised_audio_port,
        config_head,
    )
    .context("starting mDNS advertiser")?;
    let (browser, rx) =
        Browser::start(&identity.machine_id.to_string()).context("starting mDNS browser")?;
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
        Config {
            role,
            server_name: "alpha".into(),
            screens: vec![Screen {
                name: "alpha".into(),
                aliases: vec![],
                position: Default::default(),
                monitors: vec![],
            }],
            port: 24800,
            server_address: matches!(role, NodeRole::Client).then(|| "peer.local".into()),
            ..Config::default()
        }
    }

    fn backoff_sequence(role: NodeRole, n: usize) -> Vec<u64> {
        let mut b = MIN_BACKOFF;
        (0..n)
            .map(|_| {
                let (delay, next) = restart_backoff(b, role);
                b = next;
                delay.as_millis() as u64
            })
            .collect()
    }

    #[test]
    fn client_backoff_caps_low_and_never_stops() {
        assert_eq!(
            backoff_sequence(NodeRole::Client, 9),
            vec![500, 1000, 2000, 4000, 8000, 10_000, 10_000, 10_000, 10_000]
        );
    }

    #[test]
    fn server_backoff_caps_at_max() {
        assert_eq!(
            backoff_sequence(NodeRole::Server, 8),
            vec![500, 1000, 2000, 4000, 8000, 16_000, 30_000, 30_000]
        );
    }

    #[test]
    fn role_switch_clamps_a_larger_server_backoff() {
        // A server that backed off to 30 s then flipped to client must not
        // wait longer than the client cap.
        let (delay, next) = restart_backoff(MAX_BACKOFF, NodeRole::Client);
        assert_eq!(delay, CLIENT_MAX_BACKOFF);
        assert_eq!(next, CLIENT_MAX_BACKOFF);
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
        assert_eq!(args, vec!["-f", "-1", "-n", "alpha", "peer.local:24800"]);
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
