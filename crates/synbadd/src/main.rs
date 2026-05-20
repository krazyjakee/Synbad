//! `synbadd` — Synbad background daemon.
//!
//! Owns the Synbad config on disk, supervises the Synergy Core child
//! process, and serves GUI requests over a Unix socket.

use anyhow::{Context, Result};
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

use synbad_config::{paths, Config};
use synbad_ipc::server::Listener;
use synbad_ipc::Event;

mod audio;
mod binaries;
mod pairing;
mod supervisor;
mod sync;

use supervisor::Supervisor;

fn main() -> Result<()> {
    // Privileged-helper short-circuit. When the auto-updater can't write the
    // install directory as the calling user (typical for `/usr/bin/synbadd`),
    // it re-launches this binary under `pkexec` / `sudo` / `osascript` / UAC
    // with `__apply-update --plan <path>`. We answer that here, *before* we
    // touch tokio / the ipc socket / config, so the helper does nothing but
    // perform the file moves the plan describes and exit.
    let mut args = std::env::args_os().skip(1);
    if let Some(plan_path) = synbad_update::parse_apply_update_args(&mut args) {
        return apply_update_helper(&plan_path);
    }

    run_daemon()
}

fn apply_update_helper(plan_path: &std::path::Path) -> Result<()> {
    // Plain stderr — the helper has no logger configured yet and may be
    // running under pkexec where tracing-subscriber's defaults aren't useful.
    match synbad_update::apply_plan(plan_path) {
        Ok(plan) => {
            eprintln!("synbadd: installed {}", plan.tag);
            Ok(())
        }
        Err(e) => {
            eprintln!("synbadd: apply-update failed: {e:#}");
            Err(e)
        }
    }
}

#[tokio::main]
async fn run_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = paths::config_file();
    let socket_path = paths::ipc_socket();

    // Seed a default config if none exists yet — first-run experience.
    if Config::load(&config_path)?.is_none() {
        let default = Config::default();
        default
            .save(&config_path)
            .with_context(|| format!("seeding default config at {:?}", config_path))?;
        tracing::info!(?config_path, "wrote default config");
    }

    let (event_tx, _) = broadcast::channel::<Event>(1024);
    let listener = Listener::bind(&socket_path, event_tx.clone())
        .await
        .with_context(|| format!("binding ipc socket at {:?}", socket_path))?;
    tracing::info!(?socket_path, "ipc listening");

    let mut supervisor = Supervisor::new(config_path.clone(), event_tx.clone()).await?;
    supervisor.run(listener).await
}
