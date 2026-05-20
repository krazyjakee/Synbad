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
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot};

use synbad_config::{paths, Config, NodeRole};
use synbad_discovery::{Advertiser, Browser, DiscoveryEvent, Identity};
use synbad_ipc::{DaemonState, Event};

use crate::binaries::{CoreLayout, ResolvedCore, Resolver};

use super::{
    CoreResolveOutcome, Supervisor, FAST_FAIL_WINDOW, MAX_BACKOFF, MAX_FAST_FAILS, MIN_BACKOFF,
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
    pub(super) async fn start_core(&mut self) -> Result<()> {
        if matches!(self.state, DaemonState::Running { .. })
            || self.child_kill.is_some()
            || self.core_resolving
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
        Ok(())
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
            let _ = exit_tx.send(status).await;
        });

        self.set_state(DaemonState::Running { pid });
        self.backoff = MIN_BACKOFF;
        self.started_at = Some(Instant::now());
        Ok(())
    }

    /// Handle the result of an off-loop Core resolution. Spawns the child
    /// if we still want one; otherwise surfaces the failure as a log line
    /// and `Crashed` state without retrying (matching the old inline
    /// behaviour where a failed Start didn't auto-retry).
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
                let msg = format!("[synbad] could not obtain Deskflow Core: {reason}");
                tracing::error!("{}", msg);
                self.record_log(msg);
                self.set_state(DaemonState::Crashed { exit_code: None });
                return;
            }
        };

        // Rebuild argv from the *current* config so a role/address change
        // that landed while the download ran is honoured.
        let conf_path = paths::generated_conf();
        let settings_path = paths::generated_settings();
        let (program, args) =
            match build_command(&resolved, &self.config, &conf_path, &settings_path) {
                Ok(pa) => pa,
                Err(e) => {
                    let msg = format!("[synbad] bad Core command line: {e:#}");
                    tracing::error!("{}", msg);
                    self.record_log(msg);
                    self.set_state(DaemonState::Crashed { exit_code: None });
                    return;
                }
            };

        if let Err(e) = self.spawn_child(program, args) {
            let msg = format!("[synbad] {e:#}");
            tracing::error!("{}", msg);
            self.record_log(msg);
            self.set_state(DaemonState::Crashed { exit_code: None });
        }
    }

    pub(super) async fn stop_core(&mut self) {
        if let Some(tx) = self.child_kill.take() {
            let _ = tx.send(());
            // Wait for the exit event so state reflects reality before we return.
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), self.exit_rx.recv()).await;
        }
        self.set_state(DaemonState::Stopped);
    }

    pub(super) async fn handle_child_exit(&mut self, status: std::process::ExitStatus) {
        let code = status.code();
        tracing::info!(?code, "core exited");
        self.child_kill = None;

        // Classify: did the child run long enough to be considered "alive"?
        // A sub-second exit usually means missing libs (exit 127), bad CLI,
        // or permission denial — restarting won't help.
        let ran_for = self.started_at.take().map(|t| t.elapsed());
        let instant_fail = ran_for.map(|d| d < FAST_FAIL_WINDOW).unwrap_or(false);
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
        tracing::warn!(
            ?delay,
            attempt = self.fast_fail_count,
            "core crashed, will restart"
        );
        tokio::time::sleep(delay).await;
        if self.desired_running {
            if let Err(e) = self.start_core().await {
                tracing::error!(?e, "restart failed");
            }
        }
    }
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
    let advertiser = Advertiser::start(
        identity,
        &display,
        config.service_port,
        config.sync_port,
        config.port,
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
