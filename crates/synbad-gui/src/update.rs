//! Auto-update worker plumbing for Synbad GUI.
//!
//! Two background phases live behind one "Check for updates…" modal:
//!
//! 1. **Check.** Hit the GitHub Releases API and report whether a newer tag
//!    is available. Cheap (one HTTPS call); always runs on a worker thread
//!    so the UI never stalls.
//! 2. **Install.** Stream the matching archive to disk, extract it, and swap
//!    the running `synbad-gui` (and sibling `synbadd` daemon) in place.
//!    Reports download / install progress through an `mpsc` so the dialog
//!    can show a live status line and bar.
//!
//! Both stages talk through channels: the worker sends events on a sender
//! and the UI thread polls a receiver each frame in
//! `SynbadApp::poll_update`. The state machine is intentionally tiny —
//! one optional state struct on the app, transitioned by user clicks.

use std::sync::mpsc::{channel, Receiver};
use std::thread;

use eframe::egui;

use synbad_update::{check, download_and_apply, CheckResult, Progress, UpdateInfo};

/// Where the update dialog is in its lifecycle. Drives both the modal's
/// content and which buttons are enabled.
pub enum UpdateState {
    /// Background check running. The receiver carries the eventual result.
    Checking { rx: Receiver<CheckResultMsg> },
    /// Check returned successfully; we know the latest version. The user
    /// decides whether to download.
    Ready(CheckResult),
    /// Check failed. Carries a human-readable error message; user can retry.
    CheckFailed(String),
    /// Download / install in flight. Status text and bar fed by `rx`.
    Installing {
        info: UpdateInfo,
        rx: Receiver<InstallMsg>,
        stage: String,
        downloaded: u64,
        total: u64,
    },
    /// Install finished successfully — user just needs to restart.
    Installed { tag: String, elevated: bool },
    /// Install failed mid-stream. Carries the error message; user can retry
    /// (which falls back to a fresh check).
    InstallFailed(String),
}

/// Messages from the check worker. Single-shot — sent once and the channel
/// hangs up.
pub enum CheckResultMsg {
    Ok(CheckResult),
    Err(String),
}

/// Streamed messages from the install worker. The worker emits zero or more
/// `Progress` followed by exactly one `Done`.
pub enum InstallMsg {
    Progress(Progress),
    Done(Result<DoneInfo, String>),
}

/// Carries enough about the successful install for the UI to show the right
/// follow-up copy (the elevated bit changes the wording).
pub struct DoneInfo {
    pub tag: String,
    pub elevated: bool,
}

/// Spawn the GitHub release lookup on a worker. Cheap call — one HTTPS
/// request — but threaded so the UI never blocks on DNS or a slow
/// connection.
pub fn spawn_check(ctx: &egui::Context) -> UpdateState {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    let current = env!("CARGO_PKG_VERSION").to_string();
    thread::spawn(move || {
        let msg = match check(&current) {
            Ok(r) => CheckResultMsg::Ok(r),
            Err(e) => CheckResultMsg::Err(format!("{e:#}")),
        };
        let _ = tx.send(msg);
        ctx.request_repaint();
    });
    UpdateState::Checking { rx }
}

/// Spawn the download + install worker for the given release. The
/// `Progress` events are forwarded onto an `mpsc` so the UI thread can
/// paint them without holding any locks.
pub fn spawn_install(ctx: &egui::Context, info: UpdateInfo) -> UpdateState {
    let (tx, rx) = channel();
    let total = info.asset_size;
    let info_for_thread = info.clone();
    let ctx_for_thread = ctx.clone();
    thread::spawn(move || {
        let tx_progress = tx.clone();
        let ctx_for_progress = ctx_for_thread.clone();
        let outcome = download_and_apply(&info_for_thread, move |evt| {
            let _ = tx_progress.send(InstallMsg::Progress(evt));
            ctx_for_progress.request_repaint();
        });
        let done = match outcome {
            Ok(applied) => InstallMsg::Done(Ok(DoneInfo {
                tag: applied.tag,
                elevated: applied.elevated,
            })),
            Err(e) => InstallMsg::Done(Err(format!("{e:#}"))),
        };
        let _ = tx.send(done);
        ctx_for_thread.request_repaint();
    });
    UpdateState::Installing {
        info,
        rx,
        stage: "starting".into(),
        downloaded: 0,
        total,
    }
}

/// True when a check or install worker is producing events. The app's
/// repaint heartbeat uses this so the progress bar keeps moving without
/// the user having to nudge the window.
pub fn in_flight(state: &Option<UpdateState>) -> bool {
    matches!(
        state,
        Some(UpdateState::Checking { .. }) | Some(UpdateState::Installing { .. })
    )
}

/// Drain whatever the in-flight worker has produced this frame and
/// transition the state machine. Idempotent — a no-op when nothing is in
/// flight.
pub fn poll(state: &mut Option<UpdateState>) {
    // Step 1: figure out the next state by inspecting the current one. Done
    // separately from the assignment so we don't hold a borrow while we
    // mutate the field.
    let next = match state.as_mut() {
        Some(UpdateState::Checking { rx }) => match rx.try_recv() {
            Ok(CheckResultMsg::Ok(res)) => Some(UpdateState::Ready(res)),
            Ok(CheckResultMsg::Err(e)) => Some(UpdateState::CheckFailed(e)),
            Err(_) => None,
        },
        Some(UpdateState::Installing {
            rx,
            stage,
            downloaded,
            total,
            info: _,
        }) => {
            // Drain every queued progress message in one frame so the bar
            // catches up instead of moving one tick per repaint.
            let mut done_with: Option<Result<DoneInfo, String>> = None;
            loop {
                match rx.try_recv() {
                    Ok(InstallMsg::Progress(Progress::Stage(s))) => {
                        *stage = s;
                    }
                    Ok(InstallMsg::Progress(Progress::Download {
                        downloaded: d,
                        total: t,
                    })) => {
                        *downloaded = d;
                        if t > 0 {
                            *total = t;
                        }
                    }
                    Ok(InstallMsg::Done(r)) => {
                        done_with = Some(r);
                        break;
                    }
                    Err(_) => break,
                }
            }
            done_with.map(|r| match r {
                Ok(d) => UpdateState::Installed {
                    tag: d.tag,
                    elevated: d.elevated,
                },
                Err(e) => UpdateState::InstallFailed(e),
            })
        }
        _ => None,
    };
    if let Some(s) = next {
        *state = Some(s);
    }
}

/// Render the update modal. Returns the user's choice for the parent to act
/// on — `Action::None` for "no change", or a specific transition that the
/// caller wires back into the state machine and worker spawners.
pub enum Action {
    None,
    /// User clicked Close — drop the modal entirely.
    Close,
    /// User clicked Check — kick a fresh GitHub lookup.
    Check,
    /// User clicked Install — kick the download/apply worker with this info.
    Install(UpdateInfo),
}

/// Pretty-format a byte count for the progress label. Stays in MB up to a
/// few hundred megabytes; release archives are well under that ceiling.
fn fmt_bytes(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if b == 0 {
        "0 MB".into()
    } else {
        format!("{:.1} MB", b as f64 / MB)
    }
}

/// Draw the modal window. Caller owns the `bool` controlling whether the
/// modal is open at all; this function never closes it directly (the
/// returned [`Action::Close`] is the signal to flip it false).
pub fn draw_modal(
    ctx: &egui::Context,
    open: &mut bool,
    state: &Option<UpdateState>,
    current_version: &str,
) -> Action {
    let mut action = Action::None;
    let mut keep_open = *open;
    egui::Window::new("Updates")
        .open(&mut keep_open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(420.0)
        .show(ctx, |ui| {
            ui.label(format!("Current version: {current_version}"));
            ui.separator();
            match state {
                None => {
                    ui.label("Check GitHub for a newer release.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Check now").clicked() {
                            action = Action::Check;
                        }
                        if ui.button("Close").clicked() {
                            action = Action::Close;
                        }
                    });
                }
                Some(UpdateState::Checking { .. }) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Checking for updates…");
                    });
                }
                Some(UpdateState::Ready(res)) => {
                    ui.label(format!("Latest version: {}", res.info.version));
                    if let Some(url) = (!res.info.html_url.is_empty()).then_some(&res.info.html_url)
                    {
                        ui.hyperlink_to("Release notes", url);
                    }
                    if !res.info.body.is_empty() {
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical()
                            .id_source("update-notes")
                            .max_height(180.0)
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new(&res.info.body).monospace());
                            });
                    }
                    ui.add_space(8.0);
                    if res.newer {
                        ui.label(format!("Download size: {}", fmt_bytes(res.info.asset_size)));
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.add(egui::Button::new("Download and install")).clicked() {
                                action = Action::Install(res.info.clone());
                            }
                            if ui.button("Close").clicked() {
                                action = Action::Close;
                            }
                        });
                    } else {
                        ui.colored_label(
                            egui::Color32::LIGHT_GREEN,
                            "You're on the latest version.",
                        );
                        ui.add_space(4.0);
                        if ui.button("Close").clicked() {
                            action = Action::Close;
                        }
                    }
                }
                Some(UpdateState::CheckFailed(e)) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, "Check failed:");
                    ui.label(egui::RichText::new(e).monospace());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Retry").clicked() {
                            action = Action::Check;
                        }
                        if ui.button("Close").clicked() {
                            action = Action::Close;
                        }
                    });
                }
                Some(UpdateState::Installing {
                    info,
                    stage,
                    downloaded,
                    total,
                    ..
                }) => {
                    ui.label(format!("Installing {}…", info.version));
                    ui.label(stage);
                    let frac = if *total > 0 {
                        (*downloaded as f32 / *total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    ui.add(egui::ProgressBar::new(frac).show_percentage());
                    ui.label(format!(
                        "{} / {}",
                        fmt_bytes(*downloaded),
                        fmt_bytes(*total)
                    ));
                }
                Some(UpdateState::Installed { tag, elevated }) => {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, format!("Installed {}.", tag));
                    if *elevated {
                        ui.label("Files were swapped under elevated privileges.");
                    }
                    ui.label(
                        "Restart Synbad (and the synbadd daemon) for the new \
                         version to take effect.",
                    );
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        action = Action::Close;
                    }
                }
                Some(UpdateState::InstallFailed(e)) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, "Install failed:");
                    ui.label(egui::RichText::new(e).monospace());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Retry").clicked() {
                            action = Action::Check;
                        }
                        if ui.button("Close").clicked() {
                            action = Action::Close;
                        }
                    });
                }
            }
        });
    // The window's own X button feeds the same close path so the caller
    // doesn't have to special-case it.
    if !keep_open {
        action = Action::Close;
    }
    *open = keep_open;
    action
}
