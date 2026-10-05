//! Real daemon regressions with isolated XDG directories and a fake Core.
#![cfg(target_os = "linux")]

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use synbad_config::Config;
use synbad_ipc::client::Connection;
use synbad_ipc::{DaemonState, Request, Response};

struct Daemon {
    child: Child,
    root: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    fn launch(mut config: Config, fail_core: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "synbad-life-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let previous_name = config.server_name.clone();
        config.server_name = format!(
            "synbad-test-{}",
            root.file_name().unwrap().to_string_lossy()
        );
        for screen in &mut config.screens {
            if screen.name == previous_name {
                screen.name = config.server_name.clone();
            }
        }
        let core = root.join("core");
        fs::write(
            &core,
            r##"#!/bin/sh
count=$(cat "$SYNBAD_TEST_ROOT/attempts" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" > "$SYNBAD_TEST_ROOT/attempts"
if [ -f "$SYNBAD_TEST_ROOT/unavailable" ]; then exit 1; fi
if [ -f "$SYNBAD_TEST_ROOT/connected" ]; then echo 'NOTE: connected to server'; fi
if [ "$1" = server ]; then echo 'NOTE: started server, waiting for clients'; fi
exec sleep 600
"##,
        )
        .unwrap();
        fs::set_permissions(&core, fs::Permissions::from_mode(0o700)).unwrap();
        if fail_core {
            fs::write(root.join("unavailable"), "").unwrap();
        }
        config.binaries.core = Some(core);
        config
            .save(&root.join("config/synbad/config.toml"))
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_synbadd"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("UID", "lifecycle-test")
            .env("SYNBAD_TEST_ROOT", &root)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let socket = root.join("data/synbad/synbadd-lifecycle-test.sock");
        let mut daemon = Self {
            child,
            root,
            socket,
        };
        daemon.wait_until(Duration::from_secs(5), |d| {
            d.request(Request::GetStatus).is_some()
        });
        daemon
    }

    fn request(&self, request: Request) -> Option<Response> {
        Connection::connect(&self.socket)
            .ok()?
            .request(request)
            .ok()
    }

    fn wait_until(&mut self, budget: Duration, check: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        loop {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "daemon exited unexpectedly"
            );
            if check(self) {
                return;
            }
            assert!(
                start.elapsed() < budget,
                "daemon condition not reached in {budget:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Kill the isolated daemon; Linux PDEATHSIG also cleans up its Core.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn reserve_port() -> TcpListener {
    TcpListener::bind("0.0.0.0:0").unwrap()
}

#[test]
fn listeners_recover_after_startup_port_contention_and_apply_port_edits() {
    let pairing = reserve_port();
    let sync = reserve_port();
    let audio = reserve_port();
    let mut config = Config {
        service_port: pairing.local_addr().unwrap().port(),
        sync_port: sync.local_addr().unwrap().port(),
        ..Config::default()
    };
    config.audio.enabled = true;
    config.audio.signal_port = audio.local_addr().unwrap().port();
    let ports = [
        config.service_port,
        config.sync_port,
        config.audio.signal_port,
    ];
    let mut daemon = Daemon::launch(config.clone(), false);
    drop((pairing, sync, audio));
    daemon.wait_until(Duration::from_secs(8), |_| {
        ports
            .iter()
            .all(|port| TcpStream::connect(("127.0.0.1", *port)).is_ok())
    });
    let new_pairing = reserve_port();
    let new_sync = reserve_port();
    let new_audio = reserve_port();
    config.service_port = new_pairing.local_addr().unwrap().port();
    config.sync_port = new_sync.local_addr().unwrap().port();
    config.audio.signal_port = new_audio.local_addr().unwrap().port();
    let new_ports = [
        config.service_port,
        config.sync_port,
        config.audio.signal_port,
    ];
    drop((new_pairing, new_sync, new_audio));
    assert!(matches!(
        daemon.request(Request::SetConfig {
            config: Box::new(config)
        }),
        Some(Response::Ok)
    ));
    daemon.wait_until(Duration::from_secs(8), |_| {
        new_ports
            .iter()
            .all(|port| TcpStream::connect(("127.0.0.1", *port)).is_ok())
    });
    assert!(ports
        .iter()
        .all(|port| TcpStream::connect(("127.0.0.1", *port)).is_err()));
    assert!(matches!(daemon.request(Request::Stop), Some(Response::Ok)));
    daemon.wait_until(Duration::from_secs(2), |_| {
        TcpStream::connect(("127.0.0.1", new_ports[2])).is_err()
    });
}

#[test]
fn server_recovers_after_five_fast_failures_and_stop_cancels_retries() {
    let pairing = reserve_port();
    let sync = reserve_port();
    let config = Config {
        service_port: pairing.local_addr().unwrap().port(),
        sync_port: sync.local_addr().unwrap().port(),
        ..Config::default()
    };
    drop((pairing, sync));
    let mut daemon = Daemon::launch(config, true);
    daemon.wait_until(Duration::from_secs(22), |d| {
        fs::read_to_string(d.root.join("attempts"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|n| n >= 5)
            && matches!(
                d.request(Request::GetStatus),
                Some(Response::Status {
                    state: DaemonState::Crashed { .. },
                    ..
                })
            )
    });
    fs::remove_file(daemon.root.join("unavailable")).unwrap();
    daemon.wait_until(Duration::from_secs(75), |d| {
        let recovered = fs::read_to_string(d.root.join("attempts"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|n| n >= 6);
        recovered
            && matches!(
                d.request(Request::GetStatus),
                Some(Response::Status {
                    state: DaemonState::Running { .. },
                    ..
                })
            )
    });
    assert!(matches!(daemon.request(Request::Stop), Some(Response::Ok)));
    fs::write(daemon.root.join("unavailable"), "").unwrap();
    assert!(matches!(daemon.request(Request::Start), Some(Response::Ok)));
    daemon.wait_until(Duration::from_secs(2), |d| {
        matches!(
            d.request(Request::GetStatus),
            Some(Response::Status {
                state: DaemonState::Crashed { .. },
                ..
            })
        )
    });
    assert!(matches!(daemon.request(Request::Stop), Some(Response::Ok)));
    let attempts = fs::read_to_string(daemon.root.join("attempts")).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        fs::read_to_string(daemon.root.join("attempts")).unwrap(),
        attempts
    );
    assert!(matches!(
        daemon.request(Request::GetStatus),
        Some(Response::Status {
            state: DaemonState::Stopped,
            ..
        })
    ));
}

#[test]
fn silent_client_is_restarted_but_a_connected_client_survives_the_watchdog() {
    let pairing = reserve_port();
    let sync = reserve_port();
    let config = Config {
        role: synbad_config::NodeRole::Client,
        server_address: Some("127.0.0.1:9".into()),
        service_port: pairing.local_addr().unwrap().port(),
        sync_port: sync.local_addr().unwrap().port(),
        ..Config::default()
    };
    drop((pairing, sync));
    let mut daemon = Daemon::launch(config, false);
    daemon.wait_until(Duration::from_secs(35), |d| {
        fs::read_to_string(d.root.join("attempts"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|n| n >= 2)
    });
    let log = fs::read_to_string(daemon.root.join("data/synbad/daemon.log")).unwrap();
    assert!(log.contains("no input-sharing connection within 30s"));
    fs::write(daemon.root.join("connected"), "").unwrap();
    assert!(matches!(
        daemon.request(Request::Restart),
        Some(Response::Ok)
    ));
    daemon.wait_until(Duration::from_secs(3), |d| {
        fs::read_to_string(d.root.join("data/synbad/core.log"))
            .is_ok_and(|s| s.contains("connected to server"))
    });
    let attempts = fs::read_to_string(daemon.root.join("attempts")).unwrap();
    std::thread::sleep(Duration::from_secs(32));
    assert_eq!(
        fs::read_to_string(daemon.root.join("attempts")).unwrap(),
        attempts
    );
    assert!(matches!(
        daemon.request(Request::GetStatus),
        Some(Response::Status {
            state: DaemonState::Running { .. },
            ..
        })
    ));
}
