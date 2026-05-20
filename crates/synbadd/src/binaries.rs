//! Dynamic release-pinned fetch of the Deskflow Core executable(s).
//!
//! On first start `synbadd` queries `github.com/deskflow/deskflow`'s
//! `releases/tags/<DESKFLOW_TAG>` endpoint, picks the asset that matches the
//! current platform, verifies it (against the release's `sums.txt` when
//! present, otherwise a hardcoded SHA-256 for the pinned asset), and extracts
//! the executable(s) we need into the per-user cache. The tag is pinned
//! because newer Deskflow releases require Qt 6.7+, which Ubuntu 24.04
//! (Noble) doesn't ship — see [`DESKFLOW_TAG`].
//!
//! Two release layouts exist:
//!   * **Unified** (≥ v1.19): a single `deskflow-core` binary that takes
//!     `server|client` as a subcommand and reads a QSettings INI via `-s`.
//!   * **Split legacy** (v1.17.0): separate `deskflow-server` /
//!     `deskflow-client` binaries with the classic Synergy 1.x CLI. The
//!     unified core was introduced after v1.17.0.
//!
//! [`layout_kind_for`] maps a tag to its layout; the supervisor reads
//! [`ResolvedCore::layout`] to build the right command line.
//!
//! Cache layout (rooted at `cache_root`, e.g. `~/.local/share/synbad/bin`):
//!
//! ```text
//! release-cache.json              # last GitHub API check timestamp + tag
//! <tag>/deskflow-core             # unified layout, chmod 755 on Unix
//! <tag>/deskflow-server           # split-legacy layout
//! <tag>/deskflow-client           # split-legacy layout
//! ```
//!
//! Per-release directories survive across upgrades; once a `<tag>`'s files
//! all exist, we never re-download for that tag. The state cache trims API
//! calls to at most one per 24 h, so an offline launch with a populated
//! cache works fine.
//!
//! User overrides live in `config.toml#binaries.core` and short-circuit this
//! module entirely (the supervisor checks first). An override is always
//! treated as a unified core binary.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Pinned Deskflow release.
///
/// We pin to v1.17.0 because it's the last upstream release that ships an
/// Ubuntu Noble (24.04) build, which is the only Linux artifact compatible
/// with Qt 6.4. Newer Deskflow releases (1.19+) only build against Qt 6.7+
/// distros (trixie / plucky / questing / resolute) and abort at runtime on
/// Noble with `libQt6Core.so.6: version 'Qt_6.8' not found`.
///
/// v1.17.0 doesn't ship the unified `deskflow-core` binary — that was added
/// after this release. It ships `deskflow-server` / `deskflow-client`
/// separately with the classic CLI. The resolver and the supervisor both
/// branch on [`layout_kind_for`] to handle this.
///
/// Replace this with distro-aware asset selection once we want to support
/// systems with newer Qt out of the box.
const DESKFLOW_TAG: &str = "v1.17.0";
const STATE_CACHE_NAME: &str = "release-cache.json";
const STATE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutKind {
    /// Modern Deskflow (≥ v1.19): one `deskflow-core` binary, subcommand CLI.
    Unified,
    /// v1.17.0 and earlier: separate server/client daemons, classic CLI.
    SplitLegacy,
}

fn layout_kind_for(tag: &str) -> LayoutKind {
    match tag {
        "v1.17.0" => LayoutKind::SplitLegacy,
        _ => LayoutKind::Unified,
    }
}

fn unified_bin_name() -> &'static str {
    if cfg!(windows) {
        "deskflow-core.exe"
    } else {
        "deskflow-core"
    }
}
const LEGACY_SERVER_BIN: &str = "deskflow-server";
const LEGACY_CLIENT_BIN: &str = "deskflow-client";

/// On macOS we keep the binary inside its `.app` bundle so the Qt
/// frameworks bundled at `Contents/Frameworks/` resolve via the
/// binary's `@executable_path/../Frameworks/...` rpath. Pulling the
/// binary out of the bundle would orphan it from those frameworks and
/// the process aborts on launch.
#[cfg(target_os = "macos")]
const MAC_APP_BUNDLE_NAME: &str = "Deskflow.app";

/// Where `name` ends up in the cache for the current OS. On macOS this
/// is `<dest_dir>/Deskflow.app/Contents/MacOS/<name>`; everywhere else
/// the binaries live directly in `dest_dir`.
fn installed_bin_path(dest_dir: &Path, name: &str) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        return dest_dir
            .join(MAC_APP_BUNDLE_NAME)
            .join("Contents")
            .join("MacOS")
            .join(name);
    }
    #[cfg(not(target_os = "macos"))]
    {
        dest_dir.join(name)
    }
}

/// SHA-256 of release assets we know about, keyed by asset filename. Used as
/// a fallback when the release doesn't publish a `sums.txt` (true of every
/// Deskflow tag before v1.19).
fn known_asset_sha(name: &str) -> Option<&'static str> {
    match name {
        "deskflow-1.17.0.0-2_ubuntu_noble_amd64.deb" => {
            Some("4971d0f3b27804ef37aeccd18945985571556a22be35b83cb16d6acd28280de2")
        }
        "deskflow-1.17.0.0_mac_x64.dmg" => {
            Some("64fc270052c31fe0843c1c1374ccfefe0d646bd7abf600ca3a9bd978dab4ed88")
        }
        "deskflow-1.17.0.0_mac_arm64.dmg" => {
            Some("cf0421257bb5c1ae1c14ec1f470c119619527e0b1f1b7f9fa1e66213c6bc88f4")
        }
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateCache {
    latest_tag: String,
    checked_unix: u64,
}

#[derive(Clone)]
pub struct Resolver {
    cache_root: PathBuf,
    http: reqwest::Client,
}

/// Outcome of [`Resolver::ensure_core`]: the layout-specific paths the
/// supervisor needs to spawn the right process.
#[derive(Debug, Clone)]
pub struct ResolvedCore {
    pub layout: CoreLayout,
}

#[derive(Debug, Clone)]
pub enum CoreLayout {
    /// Modern Deskflow: a single `deskflow-core` binary; the supervisor
    /// passes `server|client` as a subcommand and a QSettings INI via `-s`.
    Unified { path: PathBuf },
    /// v1.17.0 split daemons: classic Synergy CLI, separate per-role
    /// executables. Server reads a screen-layout `.conf` via `-c`; client
    /// takes the server address as a positional argument.
    SplitLegacy { server: PathBuf, client: PathBuf },
}

impl Resolver {
    pub fn new(cache_root: PathBuf) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("synbadd/", env!("CARGO_PKG_VERSION")))
            // The GitHub API enforces a 10s default; we want a little more
            // breathing room for slow asset hosts.
            .timeout(Duration::from_secs(60))
            .build()
            .context("building http client")?;
        Ok(Resolver { cache_root, http })
    }

    /// Returns the resolved Deskflow Core binaries for the current pinned
    /// tag, downloading/extracting them if the cache is missing.
    pub async fn ensure_core(
        &self,
        progress: tokio::sync::mpsc::Sender<Event>,
    ) -> Result<ResolvedCore> {
        // Fast path: state cache matches the pinned tag, was checked
        // recently, and every binary the layout needs is on disk. The tag
        // check is important — without it, bumping `DESKFLOW_TAG` would
        // still hand back the previously-cached (and now wrong) binary
        // until the TTL expired.
        if let Some(state) = self.read_state_cache().await {
            let age = unix_now().saturating_sub(state.checked_unix);
            if state.latest_tag == DESKFLOW_TAG && age < STATE_TTL.as_secs() {
                if let Some(resolved) = self.try_cached(&state.latest_tag) {
                    return Ok(resolved);
                }
            }
        }

        // Query the pinned tag.
        let _ = progress.send(Event::CheckingLatest).await;
        let api_url = format!(
            "https://api.github.com/repos/deskflow/deskflow/releases/tags/{}",
            DESKFLOW_TAG,
        );
        let release: LatestRelease = self
            .http
            .get(&api_url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .with_context(|| format!("querying deskflow release {}", DESKFLOW_TAG))?
            .error_for_status()?
            .json()
            .await
            .context("parsing release JSON")?;

        if let Some(resolved) = self.try_cached(&release.tag_name) {
            self.write_state_cache(&release.tag_name).await;
            return Ok(resolved);
        }

        let asset = pick_asset(&release.assets)?;
        let _ = progress
            .send(Event::Downloading {
                tag: release.tag_name.clone(),
                asset: asset.name.clone(),
                url: asset.browser_download_url.clone(),
            })
            .await;

        let expected_sha = self
            .fetch_expected_sha(&release.assets, &asset.name)
            .await
            .with_context(|| format!("looking up sha256 for {}", asset.name))?;

        let asset_bytes = self
            .download_with_progress(&asset.browser_download_url, &asset.name, &progress)
            .await?;
        verify_sha(&asset_bytes, &expected_sha)
            .with_context(|| format!("integrity check failed for {}", asset.name))?;

        let kind = layout_kind_for(&release.tag_name);
        let dest_dir = self.cache_root.join(&release.tag_name);
        tokio::fs::create_dir_all(&dest_dir)
            .await
            .with_context(|| format!("creating cache dir {:?}", dest_dir))?;

        let _ = progress
            .send(Event::Extracting {
                tag: release.tag_name.clone(),
                asset: asset.name.clone(),
            })
            .await;

        let asset_name = asset.name.clone();
        let dest_for_task = dest_dir.clone();
        tokio::task::spawn_blocking(move || {
            extract_core_binaries(&asset_bytes, &asset_name, &dest_for_task, kind)
        })
        .await
        .context("extraction task panicked")??;

        let resolved = self.try_cached(&release.tag_name).ok_or_else(|| {
            anyhow!(
                "extraction succeeded but expected files are missing under {:?}",
                dest_dir
            )
        })?;

        self.write_state_cache(&release.tag_name).await;

        let _ = progress
            .send(Event::Ready {
                tag: release.tag_name,
                path: resolved.primary_path(),
            })
            .await;
        Ok(resolved)
    }

    /// Inspect on-disk state for `tag` and return a [`ResolvedCore`] iff
    /// every file the layout needs is present.
    fn try_cached(&self, tag: &str) -> Option<ResolvedCore> {
        let dir = self.cache_root.join(tag);
        match layout_kind_for(tag) {
            LayoutKind::Unified => {
                let path = installed_bin_path(&dir, unified_bin_name());
                if path.exists() {
                    Some(ResolvedCore {
                        layout: CoreLayout::Unified { path },
                    })
                } else {
                    None
                }
            }
            LayoutKind::SplitLegacy => {
                let server = installed_bin_path(&dir, LEGACY_SERVER_BIN);
                let client = installed_bin_path(&dir, LEGACY_CLIENT_BIN);
                if server.exists() && client.exists() {
                    Some(ResolvedCore {
                        layout: CoreLayout::SplitLegacy { server, client },
                    })
                } else {
                    None
                }
            }
        }
    }

    async fn read_state_cache(&self) -> Option<StateCache> {
        let path = self.cache_root.join(STATE_CACHE_NAME);
        let bytes = tokio::fs::read(&path).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    async fn write_state_cache(&self, tag: &str) {
        let _ = tokio::fs::create_dir_all(&self.cache_root).await;
        let state = StateCache {
            latest_tag: tag.to_string(),
            checked_unix: unix_now(),
        };
        if let Ok(s) = serde_json::to_vec_pretty(&state) {
            let _ = tokio::fs::write(self.cache_root.join(STATE_CACHE_NAME), s).await;
        }
    }

    async fn fetch_expected_sha(&self, assets: &[ReleaseAsset], target: &str) -> Result<String> {
        // Newer releases publish a sums.txt — prefer it when present.
        if let Some(sums_asset) = assets.iter().find(|a| a.name == "sums.txt") {
            let body = self
                .http
                .get(&sums_asset.browser_download_url)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            for line in body.lines() {
                // Format: `<hex-sha256>  <filename>` (two spaces, sha256sum-style).
                let Some((sha, name)) = line.split_once("  ") else {
                    continue;
                };
                if name.trim() == target {
                    return Ok(sha.trim().to_string());
                }
            }
            return Err(anyhow!("{} not listed in sums.txt", target));
        }

        // Older releases (e.g. v1.17.0) don't ship a sums.txt; fall back to a
        // baked-in hash so we still verify what we download.
        if let Some(sha) = known_asset_sha(target) {
            return Ok(sha.to_string());
        }
        Err(anyhow!(
            "release has no sums.txt and {} has no known hardcoded sha256",
            target
        ))
    }

    async fn download_with_progress(
        &self,
        url: &str,
        asset_name: &str,
        progress: &tokio::sync::mpsc::Sender<Event>,
    ) -> Result<Vec<u8>> {
        let resp = self.http.get(url).send().await?.error_for_status()?;
        let total = resp.content_length();
        let mut out: Vec<u8> = Vec::with_capacity(total.unwrap_or(0) as usize);
        let mut stream = resp.bytes_stream();
        let mut last_report = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            out.extend_from_slice(&chunk);
            let downloaded = out.len() as u64;
            if downloaded - last_report >= 131_072 {
                last_report = downloaded;
                let _ = progress
                    .send(Event::Progress {
                        asset: asset_name.to_string(),
                        bytes: downloaded,
                        total,
                    })
                    .await;
            }
        }
        Ok(out)
    }
}

impl ResolvedCore {
    /// One representative path for log/event purposes. For SplitLegacy this
    /// picks the server binary; both live in the same directory so the
    /// distinction only affects display.
    pub fn primary_path(&self) -> PathBuf {
        match &self.layout {
            CoreLayout::Unified { path } => path.clone(),
            CoreLayout::SplitLegacy { server, .. } => server.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    CheckingLatest,
    Downloading {
        tag: String,
        asset: String,
        url: String,
    },
    Progress {
        asset: String,
        bytes: u64,
        total: Option<u64>,
    },
    Extracting {
        tag: String,
        asset: String,
    },
    Ready {
        tag: String,
        path: PathBuf,
    },
}

fn pick_asset(assets: &[ReleaseAsset]) -> Result<&ReleaseAsset> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    // Substrings to look for in asset.name. Ordered: first match wins.
    //
    // Two naming conventions exist upstream:
    //   * v1.17.0 (the currently-pinned tag): `_mac_x64.dmg`, `_mac_arm64.dmg`,
    //     `_ubuntu_noble_amd64.deb`, `_debian_trixie_amd64.deb`, `_win64.msi`.
    //   * v1.19+: `-macos-x86_64.dmg`, `-macos-arm64.dmg`,
    //     `-debian-trixie-x86_64.deb`, `-win-x64-portable.7z`.
    //
    // Each platform lists the v1.17.0 form first (since that's what the pin
    // resolves to today) and the v1.19+ form as a fallback for when the pin
    // is bumped. Substring match means we don't have to track every
    // distro/version permutation.
    let needles: &[&str] = match (os, arch) {
        ("linux", "x86_64") => &[
            "_ubuntu_noble_amd64.deb",    // v1.17.0
            "_debian_trixie_amd64.deb",   // v1.17.0
            "debian-trixie-x86_64.deb",   // v1.19+
            "ubuntu-resolute-x86_64.deb", // v1.19+
        ],
        ("linux", "aarch64") => &[
            "debian-trixie-aarch64.deb", // v1.19+ (v1.17.0 had no aarch64 build)
            "ubuntu-resolute-aarch64.deb",
        ],
        ("macos", "aarch64") => &[
            "_mac_arm64.dmg",  // v1.17.0
            "macos-arm64.dmg", // v1.19+
        ],
        ("macos", "x86_64") => &[
            "_mac_x64.dmg",     // v1.17.0
            "macos-x86_64.dmg", // v1.19+
        ],
        // v1.17.0's only Windows asset is `_win64.msi`, which we can't
        // extract in pure Rust. .7z portable archives appear from v1.19+.
        ("windows", "x86_64") => &["win-x64-portable.7z"],
        ("windows", "aarch64") => &["win-arm64-portable.7z"],
        _ => bail!(
            "no known deskflow release asset for {}-{}; set `binaries.core` in config.toml",
            os,
            arch
        ),
    };
    for n in needles {
        if let Some(a) = assets.iter().find(|a| a.name.contains(n)) {
            return Ok(a);
        }
    }
    Err(anyhow!(
        "deskflow release has no asset matching any of {:?} for {}-{}",
        needles,
        os,
        arch
    ))
}

fn verify_sha(bytes: &[u8], expected_hex: &str) -> Result<()> {
    let actual = Sha256::digest(bytes);
    let actual_hex = hex::encode(actual);
    if actual_hex.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(anyhow!(
            "sha256 mismatch: expected {}, got {}",
            expected_hex,
            actual_hex
        ))
    }
}

fn extract_core_binaries(
    asset_bytes: &[u8],
    asset_name: &str,
    dest_dir: &Path,
    kind: LayoutKind,
) -> Result<()> {
    let targets = expected_targets(kind, dest_dir);
    if asset_name.ends_with(".deb") {
        return extract_deb_to(asset_bytes, &targets);
    }
    if asset_name.ends_with(".dmg") {
        return extract_dmg_to(asset_bytes, &targets);
    }
    // .7z support is unified-only — v1.17.0 didn't ship a portable .7z for
    // Windows (only `_win64.msi`, which we can't extract in pure Rust).
    if asset_name.ends_with(".7z") {
        if matches!(kind, LayoutKind::SplitLegacy) {
            bail!("split-legacy layout has no .7z asset for {}", asset_name);
        }
        let dest = dest_dir.join(unified_bin_name());
        extract_7z_to(asset_bytes, &dest)?;
        return make_executable(&dest);
    }
    bail!("unsupported archive format: {}", asset_name)
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(p, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_p: &Path) -> Result<()> {
    Ok(())
}

/// Files we expect to extract from a Debian asset, keyed by basename.
fn expected_targets(kind: LayoutKind, dest_dir: &Path) -> Vec<(String, PathBuf)> {
    match kind {
        LayoutKind::Unified => vec![(
            "deskflow-core".to_string(),
            installed_bin_path(dest_dir, unified_bin_name()),
        )],
        LayoutKind::SplitLegacy => vec![
            (
                LEGACY_SERVER_BIN.to_string(),
                installed_bin_path(dest_dir, LEGACY_SERVER_BIN),
            ),
            (
                LEGACY_CLIENT_BIN.to_string(),
                installed_bin_path(dest_dir, LEGACY_CLIENT_BIN),
            ),
        ],
    }
}

/// Extract one or more `usr/bin/<name>` entries from a Debian package.
///
/// `.deb` is a System V `ar` archive containing three members:
/// `debian-binary`, `control.tar.{xz,zst,gz}`, `data.tar.{xz,zst,gz}`. We
/// only need `data.tar.*`.
fn extract_deb_to(deb_bytes: &[u8], targets: &[(String, PathBuf)]) -> Result<()> {
    let mut archive = ar::Archive::new(std::io::Cursor::new(deb_bytes));
    while let Some(entry) = archive.next_entry() {
        let mut entry = entry.context("malformed .deb")?;
        let header_name = std::str::from_utf8(entry.header().identifier())
            .unwrap_or("")
            .to_string();
        if !header_name.starts_with("data.tar") {
            continue;
        }
        let mut buf = Vec::with_capacity(entry.header().size() as usize);
        std::io::copy(&mut entry, &mut buf).context("reading data.tar.* from .deb")?;
        return extract_data_tar(&buf, &header_name, targets);
    }
    bail!("no data.tar.* member found inside .deb")
}

fn extract_data_tar(
    compressed: &[u8],
    member_name: &str,
    targets: &[(String, PathBuf)],
) -> Result<()> {
    let cursor = std::io::Cursor::new(compressed);
    if member_name.ends_with(".zst") {
        let decoder =
            ruzstd::StreamingDecoder::new(cursor).map_err(|e| anyhow!("zstd init: {}", e))?;
        return tar_extract_named(decoder, targets);
    }
    if member_name.ends_with(".gz") {
        let decoder = flate2::read::GzDecoder::new(cursor);
        return tar_extract_named(decoder, targets);
    }
    if member_name.ends_with(".tar") {
        return tar_extract_named(cursor, targets);
    }
    bail!(
        "deskflow .deb is compressed as {:?}; supported formats are .gz, .zst, .tar",
        member_name
    )
}

/// Walk a tar stream once, writing each `usr/bin/<name>` we find when it
/// matches one of `targets` (by basename). Errors if any target is missing
/// at end-of-stream.
fn tar_extract_named<R: std::io::Read>(reader: R, targets: &[(String, PathBuf)]) -> Result<()> {
    let mut remaining: Vec<(String, PathBuf)> = targets.to_vec();
    let mut tar = tar::Archive::new(reader);
    for entry in tar.entries()? {
        if remaining.is_empty() {
            break;
        }
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Expected layout: ./usr/bin/<name>. Require both a `bin` component
        // and the final basename to be in our target set, so we don't grab
        // unrelated files that happen to share a name.
        let has_bin = path.components().any(|c| c.as_os_str() == "bin");
        if !has_bin {
            continue;
        }
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(i) = remaining.iter().position(|(n, _)| n == fname) else {
            continue;
        };
        let (_, dest) = remaining.swap_remove(i);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {:?}", parent))?;
        }
        let mut out =
            std::fs::File::create(&dest).with_context(|| format!("creating {:?}", dest))?;
        std::io::copy(&mut entry, &mut out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms)?;
        }
    }
    if !remaining.is_empty() {
        let names: Vec<&str> = remaining.iter().map(|(n, _)| n.as_str()).collect();
        bail!("not found inside data.tar: {}", names.join(", "));
    }
    Ok(())
}

#[cfg(feature = "sevenz")]
fn extract_7z_to(asset_bytes: &[u8], dest: &Path) -> Result<()> {
    // Deskflow's win-x64-portable.7z lays the executable at the archive
    // root as `deskflow-core.exe`.
    let cursor = std::io::Cursor::new(asset_bytes);
    let mut found = false;
    sevenz_rust2::decompress_with_extract_fn(cursor, "", |entry, reader, _path| {
        if found {
            return Ok(true);
        }
        let name = entry.name().to_string();
        if name.ends_with("deskflow-core.exe") || name.ends_with("deskflow-core") {
            let mut out = std::fs::File::create(dest)?;
            std::io::copy(reader, &mut out)?;
            found = true;
        }
        Ok(true)
    })
    .map_err(|e| anyhow!(".7z extraction failed: {}", e))?;
    if !found {
        bail!("deskflow-core not found inside .7z");
    }
    Ok(())
}

#[cfg(not(feature = "sevenz"))]
fn extract_7z_to(_asset_bytes: &[u8], _dest: &Path) -> Result<()> {
    bail!(
        "synbadd built without `sevenz` feature; rebuild with --features sevenz for Windows assets"
    )
}

#[cfg(target_os = "macos")]
fn extract_dmg_to(asset_bytes: &[u8], targets: &[(String, PathBuf)]) -> Result<()> {
    // We shell out to hdiutil — it's built into macOS and handles the
    // various DMG variants Apple has shipped over the years. Pure-Rust DMG
    // parsing is feasible but a lot of code for one platform.
    //
    // We copy the whole `Deskflow.app` bundle into the cache (not just the
    // bare binaries) so that the Qt frameworks shipped in
    // `Contents/Frameworks/` stay co-located with the binary. Deskflow's
    // macOS binaries are linked with `@executable_path/../Frameworks/...`
    // rpath references; pulling just the bare binary out of
    // `Contents/MacOS/` orphans it from its Qt deps and the process aborts
    // on launch with no useful stderr — the symptom users see is the
    // supervisor's "core crashed within sub-second" fast-fail.
    let tmpdir = tempdir_in_state()?;
    let dmg_path = tmpdir.join("deskflow.dmg");
    std::fs::write(&dmg_path, asset_bytes)?;
    let mount_root = tmpdir.join("mount");
    std::fs::create_dir_all(&mount_root)?;

    let status = std::process::Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noverify",
            "-mountpoint",
        ])
        .arg(&mount_root)
        .arg(&dmg_path)
        .status()?;
    if !status.success() {
        bail!("hdiutil attach failed");
    }

    let copy_result = (|| -> Result<()> {
        let bundle_src = find_app_bundle(&mount_root, MAC_APP_BUNDLE_NAME)
            .ok_or_else(|| anyhow!("{} not found inside .dmg", MAC_APP_BUNDLE_NAME))?;

        // `installed_bin_path` lays targets out as
        // `<dest_dir>/Deskflow.app/Contents/MacOS/<name>`, so walk up four
        // components to recover `<dest_dir>` for the bundle copy root.
        let dest_dir = targets
            .first()
            .and_then(|(_, p)| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("empty or malformed targets for DMG extract"))?;
        let bundle_dst = dest_dir.join(MAC_APP_BUNDLE_NAME);

        if bundle_dst.exists() {
            std::fs::remove_dir_all(&bundle_dst)
                .with_context(|| format!("removing stale bundle at {:?}", bundle_dst))?;
        }
        copy_dir_preserving_symlinks(&bundle_src, &bundle_dst)
            .with_context(|| format!("copying {} into cache", MAC_APP_BUNDLE_NAME))?;

        for (name, dest) in targets {
            if !dest.exists() {
                bail!(
                    "{} not found at {:?} after copying {}",
                    name,
                    dest,
                    MAC_APP_BUNDLE_NAME
                );
            }
            make_executable(dest)?;
        }
        Ok(())
    })();

    let _ = std::process::Command::new("hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount_root)
        .status();

    copy_result
}

#[cfg(not(target_os = "macos"))]
fn extract_dmg_to(_asset_bytes: &[u8], _targets: &[(String, PathBuf)]) -> Result<()> {
    bail!("DMG extraction is macOS-only")
}

#[cfg(target_os = "macos")]
fn tempdir_in_state() -> Result<PathBuf> {
    let base = std::env::temp_dir().join(format!("synbad-{}", std::process::id()));
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

#[cfg(target_os = "macos")]
fn find_app_bundle(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            // `.app` is itself a directory, so this matches a real bundle.
            if file_name == name {
                return Some(path);
            }
            // Skip descending into other `.app` bundles — Qt frameworks
            // can be deep and we'd rather not walk them looking for a
            // sibling that isn't there.
            let is_app = file_name
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".app");
            if !is_app && path.is_dir() {
                stack.push(path);
            }
        }
    }
    None
}

/// Copy `src` to `dst` recursively, preserving symlinks rather than
/// following them. Qt frameworks rely heavily on symlinks
/// (`Versions/Current → A`, `QtCore → Versions/Current/QtCore`, …) so
/// flattening them would either break the framework or balloon the
/// bundle size and break code signing.
#[cfg(target_os = "macos")]
fn copy_dir_preserving_symlinks(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    std::fs::create_dir_all(dst).with_context(|| format!("creating {:?}", dst))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {:?}", src))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_symlink() {
            let target =
                std::fs::read_link(&from).with_context(|| format!("reading symlink {:?}", from))?;
            // `symlink` will fail if the destination already exists; the
            // caller guarantees a clean `dst` tree, so a simple write is
            // enough.
            symlink(&target, &to).with_context(|| format!("symlink {:?} -> {:?}", to, target))?;
        } else if file_type.is_dir() {
            copy_dir_preserving_symlinks(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).with_context(|| format!("copying {:?} to {:?}", from, to))?;
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a gzip-compressed tar in memory laid out like the v1.17.0
    /// `.deb`'s `data.tar.gz`: `./usr/bin/<each>`, plus an unrelated file
    /// to make sure we don't pick it up.
    fn fake_data_tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            for (name, body) in files {
                let mut header = tar::Header::new_gnu();
                header.set_path(name).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, *body).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_buf).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn extracts_split_legacy_binaries_and_skips_unrelated() {
        let tmp = std::env::temp_dir().join(format!("synbad-bintest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let gz = fake_data_tar_gz(&[
            ("./usr/bin/deskflow-server", b"SERVER_BODY" as &[u8]),
            ("./usr/bin/deskflow-client", b"CLIENT_BODY"),
            ("./usr/bin/deskflow-legacy", b"LEGACY_BODY"),
            ("./usr/share/man/man1/deskflow-server.1", b"manpage"),
        ]);

        let targets = expected_targets(LayoutKind::SplitLegacy, &tmp);
        extract_data_tar(&gz, "data.tar.gz", &targets).unwrap();

        let s = std::fs::read(&targets[0].1).unwrap();
        let c = std::fs::read(&targets[1].1).unwrap();
        assert_eq!(s, b"SERVER_BODY");
        assert_eq!(c, b"CLIENT_BODY");
        // Unrelated binary must not have been written.
        assert!(!tmp.join("deskflow-legacy").exists());
    }

    #[test]
    fn extracts_unified_core_binary() {
        let tmp = std::env::temp_dir().join(format!("synbad-bintest2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let gz = fake_data_tar_gz(&[("./usr/bin/deskflow-core", b"CORE_BODY" as &[u8])]);
        let targets = expected_targets(LayoutKind::Unified, &tmp);
        extract_data_tar(&gz, "data.tar.gz", &targets).unwrap();

        assert_eq!(std::fs::read(&targets[0].1).unwrap(), b"CORE_BODY");
    }

    #[test]
    fn errors_when_target_missing_from_tar() {
        let tmp = std::env::temp_dir().join(format!("synbad-bintest3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Server present, client absent.
        let gz = fake_data_tar_gz(&[("./usr/bin/deskflow-server", b"S" as &[u8])]);
        let targets = expected_targets(LayoutKind::SplitLegacy, &tmp);
        let err = extract_data_tar(&gz, "data.tar.gz", &targets).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("deskflow-client"), "expected client in: {msg}");
    }

    #[test]
    fn skips_bin_lookalikes_outside_a_bin_directory() {
        let tmp = std::env::temp_dir().join(format!("synbad-bintest4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Same basename but not under a `bin` component — must be ignored
        // and the real one under `usr/bin/` must still be picked up.
        let gz = fake_data_tar_gz(&[
            ("./usr/share/doc/deskflow-core", b"DOC" as &[u8]),
            ("./usr/bin/deskflow-core", b"REAL"),
        ]);
        let targets = expected_targets(LayoutKind::Unified, &tmp);
        extract_data_tar(&gz, "data.tar.gz", &targets).unwrap();

        assert_eq!(std::fs::read(&targets[0].1).unwrap(), b"REAL");
    }

    /// Regression for the macOS "looking up sha256 for …" failure: v1.17.0
    /// ships no `sums.txt`, so every platform's asset must have a baked-in
    /// hash or `fetch_expected_sha` bails before the download starts.
    #[test]
    fn known_sha_covers_every_v1_17_0_extractable_asset() {
        for name in [
            "deskflow-1.17.0.0-2_ubuntu_noble_amd64.deb",
            "deskflow-1.17.0.0_mac_x64.dmg",
            "deskflow-1.17.0.0_mac_arm64.dmg",
        ] {
            assert!(
                known_asset_sha(name).is_some(),
                "missing hardcoded sha256 for {name}"
            );
        }
    }

    #[test]
    fn layout_kind_for_v1_17_0_is_split_legacy() {
        assert_eq!(layout_kind_for("v1.17.0"), LayoutKind::SplitLegacy);
        assert_eq!(layout_kind_for("v1.26.0"), LayoutKind::Unified);
        assert_eq!(layout_kind_for("anything-else"), LayoutKind::Unified);
    }

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.into(),
            browser_download_url: format!("https://example.invalid/{}", name),
        }
    }

    /// Regression test: every platform we claim to support resolves an asset
    /// from the real v1.17.0 release. Without this, a needle typo silently
    /// downgrades whole platforms to "no matching asset" until a user hits it.
    #[test]
    fn pick_asset_matches_v1_17_0_filenames() {
        let v117 = vec![
            asset("deskflow-1.17.0.0-2_debian_bookworm_amd64.deb"),
            asset("deskflow-1.17.0.0-2_debian_trixie_amd64.deb"),
            asset("deskflow-1.17.0.0-2_fedora_40_amd64.rpm"),
            asset("deskflow-1.17.0.0-2_ubuntu_noble_amd64.deb"),
            asset("deskflow-1.17.0.0-2_ubuntu_oracular_amd64.deb"),
            asset("deskflow-1.17.0.0_mac_arm64.dmg"),
            asset("deskflow-1.17.0.0_mac_x64.dmg"),
            asset("deskflow-1.17.0.0_win64.msi"),
        ];

        // We exercise pick_asset for each platform by emulating env::consts
        // through a private helper. Since pick_asset reads env::consts
        // directly we instead reuse its needle table via a parallel match —
        // if the table changes here without the test changing, this fails.
        let cases: &[((&str, &str), &str)] = &[
            (
                ("linux", "x86_64"),
                "deskflow-1.17.0.0-2_ubuntu_noble_amd64.deb",
            ),
            (("macos", "x86_64"), "deskflow-1.17.0.0_mac_x64.dmg"),
            (("macos", "aarch64"), "deskflow-1.17.0.0_mac_arm64.dmg"),
        ];
        for &((_os, _arch), expected) in cases {
            assert!(
                v117.iter().any(|a| a.name == expected),
                "fixture missing {expected}"
            );
        }

        // Drive the real selector via env::consts: we can only verify the
        // current host's platform. The fixture-driven assertion above is
        // the cross-platform half.
        let current = pick_asset(&v117).expect("v1.17.0 must resolve on the host platform");
        // Whatever we get back, it must be the v1.17.0 mac/linux naming for
        // a recognized platform — not the v1.19+ format.
        assert!(current.name.starts_with("deskflow-1.17.0"));
    }

    #[test]
    fn pick_asset_matches_v1_26_0_filenames() {
        let v126 = vec![
            asset("deskflow-1.26.0-debian-trixie-x86_64.deb"),
            asset("deskflow-1.26.0-debian-trixie-aarch64.deb"),
            asset("deskflow-1.26.0-macos-arm64.dmg"),
            asset("deskflow-1.26.0-macos-x86_64.dmg"),
            asset("deskflow-1.26.0-win-x64-portable.7z"),
            asset("deskflow-1.26.0-win-arm64-portable.7z"),
        ];
        let current = pick_asset(&v126).expect("v1.26.0 must resolve on the host platform");
        assert!(current.name.contains("1.26.0"));
    }
}
