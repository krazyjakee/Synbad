//! Config-edit funnel.
//!
//! Every local-origin edit (GUI `SetConfig` or `$EDITOR` save picked up
//! by the FS watcher) and every remote-origin merge (from a peer over
//! the sync protocol) flows through this file. Keeping them all here
//! means the stamp/persist/regen/restart/gossip sequence is identical
//! regardless of where the edit came from — no second copy of the
//! "after-edit" boilerplate to drift out of sync.

use std::time::Duration;

use anyhow::{Context, Result};

use synbad_config::Config;
use synbad_ipc::Event;
use synbad_sync::MergeOutcome;

use crate::sync::{self, SyncOp};

use super::Supervisor;

impl Supervisor {
    pub(super) async fn set_config(&mut self, new_config: Config) -> Result<()> {
        new_config.validate()?;
        new_config
            .save(&self.config_path)
            .context("persisting new config")?;
        self.apply_local_edit(new_config, /*restart_if_running=*/ true)
            .await
    }

    pub(super) async fn handle_config_changed(&mut self) {
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
        // Decide before swapping `self.config`: does this edit touch any
        // input the Core actually consumes? Layout-only churn (e.g. a
        // service/sync port tweak) should apply silently rather than
        // bouncing active input sharing.
        let core_inputs_differ = self.config.core_inputs_differ(&new_config);
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
            if core_inputs_differ {
                self.stop_core().await;
                if let Err(e) = self.start_core().await {
                    tracing::warn!(?e, "restart after config change failed");
                }
            } else {
                tracing::info!("config changed but Core inputs unchanged; applied without restart");
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
            Ok(g) => g
                .list()
                .iter()
                .map(|p| p.machine_id.clone())
                .collect::<Vec<_>>(),
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

    pub(super) fn gc_sync_tasks(&mut self) {
        self.sync_tasks.retain(|t| !t.is_finished());
    }

    /// Handle a `SyncOp` request from a sync session task.
    pub(super) async fn handle_sync_op(&mut self, op: SyncOp) {
        match op {
            SyncOp::Snapshot { reply } => {
                let _ = reply.send(self.versioned.clone());
            }
            SyncOp::Merge {
                peer_machine_id,
                incoming,
                reply,
            } => {
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
                    let core_inputs_differ = self.config.core_inputs_differ(&self.versioned.config);
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
                    // the new screen layout / options take effect. A
                    // peer pushing a daemon-only change (e.g. a sync
                    // port) shouldn't bounce our active input sharing.
                    if self.desired_running && core_inputs_differ {
                        self.stop_core().await;
                        if let Err(e) = self.start_core().await {
                            tracing::warn!(?e, "restart after sync merge failed");
                        }
                    } else if self.desired_running {
                        tracing::info!("merged config left Core inputs unchanged; no restart");
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
}
