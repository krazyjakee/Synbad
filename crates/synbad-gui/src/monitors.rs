//! Local display enumeration.
//!
//! Reports the desktops attached to *this* machine so the layout editor
//! can size the local screen by real monitor dimensions. Other machines'
//! monitors arrive through config sync — see [`synbad_config::Screen::monitors`].
//!
//! On X11 we query RandR ourselves instead of going through `display-info`.
//! That crate issues `RRGetScreenResources`, which makes Xorg force a full
//! hardware re-probe of every output (DDC/EDID reads) while the server is
//! blocked — ~100 ms on NVIDIA. We enumerate on a timer, so that turned
//! into a frame hitch in every GL/Vulkan app on the desktop every few
//! seconds (#65). `RRGetMonitors` returns the server's cached layout and
//! is all we need.

use synbad_config::MonitorInfo;

/// Snapshot every monitor reported by the OS. Returns an empty vec if the
/// platform layer fails (e.g. no display server attached) so the rest of
/// the app degrades gracefully to the legacy fixed-size screen box.
pub fn enumerate() -> Vec<MonitorInfo> {
    #[cfg(target_os = "linux")]
    if !x11::is_wayland() {
        return x11::enumerate().unwrap_or_else(|e| {
            tracing::warn!("monitor enumeration failed: {e}");
            Vec::new()
        });
    }

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

#[cfg(target_os = "linux")]
mod x11 {
    use synbad_config::MonitorInfo;
    use xcb::randr::GetMonitors;
    use xcb::x::{GetProperty, ATOM_RESOURCE_MANAGER, ATOM_STRING};

    /// Same session detection `display-info` uses, so the Wayland path
    /// keeps going through it unchanged.
    pub(super) fn is_wayland() -> bool {
        std::env::var_os("WAYLAND_DISPLAY")
            .or_else(|| std::env::var_os("XDG_SESSION_TYPE"))
            .is_some_and(|v| v.to_string_lossy().to_lowercase().contains("wayland"))
    }

    pub(super) fn enumerate() -> Result<Vec<MonitorInfo>, Box<dyn std::error::Error>> {
        let (conn, index) =
            xcb::Connection::connect_with_extensions(None, &[xcb::Extension::RandR], &[])?;
        let screen = conn
            .get_setup()
            .roots()
            .nth(index as usize)
            .ok_or("X screen not found")?;

        // Xft.dpi lives in the RESOURCE_MANAGER string on the root window.
        // `long_length` is in 4-byte units; 64 KiB covers any sane database.
        let resources = conn.wait_for_reply(conn.send_request(&GetProperty {
            delete: false,
            window: screen.root(),
            property: ATOM_RESOURCE_MANAGER,
            r#type: ATOM_STRING,
            long_offset: 0,
            long_length: 16 * 1024,
        }))?;
        let scale = xft_scale(&String::from_utf8_lossy(resources.value())).unwrap_or(1.0);

        let reply = conn.wait_for_reply(conn.send_request(&GetMonitors {
            window: screen.root(),
            get_active: true,
        }))?;

        Ok(reply
            .monitors()
            .map(|m| MonitorInfo {
                // Logical pixels, matching what display-info reported.
                x: (m.x() as f32 / scale) as i32,
                y: (m.y() as f32 / scale) as i32,
                w: (m.width() as f32 / scale) as u32,
                h: (m.height() as f32 / scale) as u32,
                primary: m.primary(),
            })
            .collect())
    }

    /// `Xft.dpi` from an X resource database string, as a scale factor
    /// relative to 96 DPI.
    fn xft_scale(resources: &str) -> Option<f32> {
        let dpi: f32 = resources
            .lines()
            .find_map(|l| l.strip_prefix("Xft.dpi:"))?
            .trim()
            .parse()
            .ok()?;
        (dpi > 0.0).then_some(dpi / 96.0)
    }

    #[cfg(test)]
    mod tests {
        use super::xft_scale;

        #[test]
        fn xft_scale_parses_dpi() {
            let db = "Xcursor.size:\t24\nXft.dpi:\t144\nXft.antialias:\t1\n";
            assert_eq!(xft_scale(db), Some(1.5));
        }

        #[test]
        fn xft_scale_missing_or_bad() {
            assert_eq!(xft_scale("Xcursor.size:\t24\n"), None);
            assert_eq!(xft_scale("Xft.dpi:\tnope\n"), None);
            assert_eq!(xft_scale("Xft.dpi:\t0\n"), None);
        }
    }
}
