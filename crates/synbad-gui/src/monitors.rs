//! Local display enumeration.
//!
//! Reports the desktops attached to *this* machine so the layout editor
//! can size the local screen by real monitor dimensions. Other machines'
//! monitors arrive through config sync — see [`synbad_config::Screen::monitors`].

use synbad_config::MonitorInfo;

/// Snapshot every monitor reported by the OS. Returns an empty vec if the
/// platform layer fails (e.g. no display server attached) so the rest of
/// the app degrades gracefully to the legacy fixed-size screen box.
pub fn enumerate() -> Vec<MonitorInfo> {
    match display_info::DisplayInfo::all() {
        Ok(list) => list
            .into_iter()
            .map(|d| MonitorInfo {
                x: d.x,
                y: d.y,
                // display-info reports physical pixels divided by scale_factor
                // — i.e. logical pixels, which is what we want.
                w: d.width,
                h: d.height,
                primary: d.is_primary,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("monitor enumeration failed: {e}");
            Vec::new()
        }
    }
}
