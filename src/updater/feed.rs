//! `latest.json`: what each release publishes for the in-app updater.
//!
//! ```json
//! {
//!   "version": "1.0.1",
//!   "notes": "- Fixed …",
//!   "pub_date": "2026-11-20T12:00:00Z",
//!   "platforms": {
//!     "windows-x86_64":   { "url": "…/OpenMouse-Bridge-windows-x64-setup.exe", "sha256": "…", "size": 123 },
//!     "darwin-universal": { "url": "…/openmouse-bridge-macos-app.zip",       "sha256": "…", "size": 123 },
//!     "linux-x86_64":     { "url": "…/OpenMouse-Bridge-linux-x86_64.AppImage", "sha256": "…", "size": 123 }
//!   }
//! }
//! ```
//!
//! The same shape OpenMouse Desktop publishes. It is a release asset, so
//! `releases/latest/download/latest.json` always names the newest release.

use std::{collections::HashMap, path::PathBuf};

use anyhow::{Context, Result, ensure};
use semver::Version;
use serde::Deserialize;

/// The newest release's `latest.json`.
pub(super) const LATEST_JSON_URL: &str =
    "https://github.com/OpenMouse-Project/OpenMouse-Bridge/releases/latest/download/latest.json";
/// Overrides [`LATEST_JSON_URL`], to test against a local fake release.
pub(super) const FEED_URL_ENV: &str = "OPENMOUSE_BRIDGE_UPDATE_FEED_URL";
/// Where to send someone whose install Bridge cannot replace by itself.
pub(super) const RELEASES_PAGE: &str =
    "https://github.com/OpenMouse-Project/OpenMouse-Bridge/releases/latest";
/// The largest asset Bridge will download (the AppImage is the biggest).
pub(super) const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub(super) struct Feed {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub platforms: HashMap<String, FeedAsset>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(super) struct FeedAsset {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// How this copy of Bridge was installed, which decides the asset it updates
/// from and how the update is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum InstallKind {
    /// Installed by `OpenMouse-Bridge-windows-x64-setup.exe` (or a zip that
    /// the installer can take over in place): run the new installer silently.
    #[cfg_attr(not(windows), allow(dead_code))]
    WindowsInstaller,
    /// A signed `.app` bundle: swap in the new bundle from the app zip.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    MacBundle,
    /// An AppImage: replace the file at `$APPIMAGE`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    LinuxAppImage { path: PathBuf },
}

impl InstallKind {
    /// The key of this install's asset in [`Feed::platforms`].
    pub fn platform_key(&self) -> &'static str {
        match self {
            Self::WindowsInstaller => "windows-x86_64",
            Self::MacBundle => "darwin-universal",
            Self::LinuxAppImage { .. } => "linux-x86_64",
        }
    }
}

/// How this install updates, or `None` when it cannot update itself (a bare
/// binary on macOS, an extracted tarball on Linux): those get a link to the
/// release page instead.
pub(super) fn current_install_kind() -> Option<InstallKind> {
    #[cfg(target_os = "windows")]
    return Some(InstallKind::WindowsInstaller);
    #[cfg(target_os = "macos")]
    return crate::platform::bundle_root().map(|_| InstallKind::MacBundle);
    #[cfg(target_os = "linux")]
    return appimage_path(std::env::var_os("APPIMAGE"))
        .map(|path| InstallKind::LinuxAppImage { path });
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    None
}

/// The AppImage this process runs from: the runtime sets `$APPIMAGE` to the
/// image file's absolute path. Anything else (unset, relative, not a file) is
/// not an AppImage install.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn appimage_path(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    (path.is_absolute() && path.is_file()).then_some(path)
}

/// What the feed offers this install.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Offer {
    /// Nothing newer than `current`.
    None,
    /// A newer version with an asset for this install.
    Install { version: Version, asset: FeedAsset },
    /// A newer version this install cannot apply by itself.
    Manual { version: Version },
}

/// Compares the feed with the running `current` version for an install of
/// `kind` (`None`: one that cannot update itself).
pub(super) fn offer(feed: &Feed, current: &str, kind: Option<&InstallKind>) -> Result<Offer> {
    let current = Version::parse(current).context("Bridge has an invalid current version")?;
    let available = Version::parse(feed.version.trim_start_matches('v'))
        .with_context(|| format!("latest.json has an invalid version {:?}", feed.version))?;
    if available <= current {
        return Ok(Offer::None);
    }
    let Some(kind) = kind else {
        return Ok(Offer::Manual { version: available });
    };
    let Some(asset) = feed.platforms.get(kind.platform_key()) else {
        // A release without this platform's asset: still worth knowing about.
        return Ok(Offer::Manual { version: available });
    };
    validate_asset(asset)?;
    Ok(Offer::Install {
        version: available,
        asset: asset.clone(),
    })
}

/// An asset must come over HTTPS (plain HTTP only from this machine, for
/// testing against a local fake release), with a SHA-256 and a sane size.
fn validate_asset(asset: &FeedAsset) -> Result<()> {
    let url = reqwest::Url::parse(&asset.url)
        .with_context(|| format!("latest.json has an invalid URL {:?}", asset.url))?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && local),
        "latest.json asset URL must use HTTPS: {}",
        asset.url
    );
    ensure!(
        asset.sha256.len() == 64 && asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "latest.json asset has an invalid SHA-256"
    );
    ensure!(
        asset.size > 0 && asset.size <= MAX_ASSET_BYTES,
        "latest.json asset has an invalid size {}",
        asset.size
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn feed(version: &str, platforms: &[(&str, &str)]) -> Feed {
        Feed {
            version: version.into(),
            notes: "- Fixes".into(),
            platforms: platforms
                .iter()
                .map(|(key, url)| {
                    (
                        (*key).to_owned(),
                        FeedAsset {
                            url: (*url).to_owned(),
                            sha256: SHA.into(),
                            size: 42,
                        },
                    )
                })
                .collect(),
        }
    }

    fn appimage() -> InstallKind {
        InstallKind::LinuxAppImage {
            path: "/opt/OpenMouse-Bridge.AppImage".into(),
        }
    }

    #[test]
    fn parses_the_published_shape() {
        let feed: Feed = serde_json::from_str(&format!(
            r#"{{"version":"1.0.1","notes":"- a","pub_date":"2026-11-20T12:00:00Z",
                "platforms":{{"linux-x86_64":{{"url":"https://example.com/a.AppImage","sha256":"{SHA}","size":9}}}}}}"#
        ))
        .unwrap();
        assert_eq!(feed.version, "1.0.1");
        assert_eq!(feed.platforms["linux-x86_64"].size, 9);
    }

    #[test]
    fn offers_only_newer_versions_with_this_installs_asset() {
        let url = "https://github.com/x/a.AppImage";
        let newer = feed("1.0.1", &[("linux-x86_64", url)]);
        assert!(matches!(
            offer(&newer, "1.0.0", Some(&appimage())).unwrap(),
            Offer::Install { ref version, ref asset } if version.to_string() == "1.0.1" && asset.url == url
        ));
        assert_eq!(
            offer(&newer, "1.0.1", Some(&appimage())).unwrap(),
            Offer::None
        );
        assert_eq!(
            offer(&newer, "1.1.0", Some(&appimage())).unwrap(),
            Offer::None
        );
        // A "v" prefix is tolerated, a pre-release is older than its release.
        let tagged = feed("v1.0.0", &[("linux-x86_64", url)]);
        assert!(matches!(
            offer(&tagged, "1.0.0-beta.21", Some(&appimage())).unwrap(),
            Offer::Install { .. }
        ));
    }

    #[test]
    fn installs_that_cannot_update_themselves_get_a_manual_offer() {
        let newer = feed(
            "1.0.1",
            &[("windows-x86_64", "https://github.com/x/setup.exe")],
        );
        let manual = Offer::Manual {
            version: Version::new(1, 0, 1),
        };
        assert_eq!(offer(&newer, "1.0.0", None).unwrap(), manual);
        // No asset for this platform in the release.
        assert_eq!(offer(&newer, "1.0.0", Some(&appimage())).unwrap(), manual);
    }

    #[test]
    fn unsafe_assets_are_refused() {
        let plain = feed(
            "1.0.1",
            &[("linux-x86_64", "http://example.com/a.AppImage")],
        );
        assert!(offer(&plain, "1.0.0", Some(&appimage())).is_err());
        let local = feed(
            "1.0.1",
            &[("linux-x86_64", "http://127.0.0.1:8000/a.AppImage")],
        );
        assert!(offer(&local, "1.0.0", Some(&appimage())).is_ok());

        let mut bad_hash = feed("1.0.1", &[("linux-x86_64", "https://example.com/a")]);
        bad_hash.platforms.get_mut("linux-x86_64").unwrap().sha256 = "abc".into();
        assert!(offer(&bad_hash, "1.0.0", Some(&appimage())).is_err());

        let mut empty = feed("1.0.1", &[("linux-x86_64", "https://example.com/a")]);
        empty.platforms.get_mut("linux-x86_64").unwrap().size = 0;
        assert!(offer(&empty, "1.0.0", Some(&appimage())).is_err());

        assert!(offer(&feed("next", &[]), "1.0.0", Some(&appimage())).is_err());
    }

    #[test]
    fn appimage_installs_need_an_absolute_existing_file() {
        assert_eq!(appimage_path(None), None);
        assert_eq!(appimage_path(Some("relative.AppImage".into())), None);
        assert_eq!(
            appimage_path(Some("/definitely/not/here.AppImage".into())),
            None
        );
        let file = std::env::current_exe().unwrap();
        assert_eq!(appimage_path(Some(file.clone().into())), Some(file));
    }

    #[test]
    fn platform_keys_match_the_release_workflow() {
        assert_eq!(
            InstallKind::WindowsInstaller.platform_key(),
            "windows-x86_64"
        );
        assert_eq!(InstallKind::MacBundle.platform_key(), "darwin-universal");
        assert_eq!(appimage().platform_key(), "linux-x86_64");
    }
}
