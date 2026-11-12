#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use std::process::Command;
use std::{
    env, fs,
    io::{self, Cursor},
    path::Path,
    time::Duration,
};

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, ensure};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/OpenMouse-Project/OpenMouse-Bridge/releases/latest";
const UPDATE_URL_ENV: &str = "OPENMOUSE_BRIDGE_UPDATE_API_URL";
const USER_AGENT: &str = "OpenMouse-Bridge-Updater";
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The bundle a macOS app-update archive carries at its root.
#[cfg(any(target_os = "macos", test))]
const MACOS_APP_NAME: &str = "OpenMouse Bridge.app";

mod feed;

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    /// The release notes from `latest.json`; empty for a stub completion.
    pub notes: String,
    source: UpdateSource,
}

#[derive(Clone, Debug)]
enum UpdateSource {
    /// A `-full` zip from the GitHub release listing, which a stub install
    /// (see [`is_stub_install`]) fetches to finish itself.
    Legacy {
        archive_url: String,
        checksum_url: String,
    },
    /// This install's asset in `latest.json`.
    Feed {
        kind: feed::InstallKind,
        asset: feed::FeedAsset,
    },
    /// A newer release this install cannot apply by itself.
    Manual,
}

impl UpdateInfo {
    /// Where to download the update by hand, for an install that cannot
    /// replace itself (a bare binary, or a release missing this platform).
    pub fn manual_download_url(&self) -> Option<&'static str> {
        matches!(self.source, UpdateSource::Manual).then_some(feed::RELEASES_PAGE)
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// Reads `latest.json` and returns the update it offers this install, if the
/// release is newer than the running Bridge.
pub async fn check_for_update() -> Result<Option<UpdateInfo>> {
    let url = env::var(feed::FEED_URL_ENV).unwrap_or_else(|_| feed::LATEST_JSON_URL.to_owned());
    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .context("could not create the update client")?;
    let response = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("could not check {url}"))?
        .error_for_status()
        .with_context(|| format!("update check failed for {url}"))?;
    let bytes = read_bounded_response(response, MAX_RELEASE_BYTES).await?;
    let latest: feed::Feed =
        serde_json::from_slice(&bytes).context("could not parse latest.json")?;
    let kind = feed::current_install_kind();
    Ok(
        match feed::offer(&latest, crate::BRIDGE_VERSION, kind.as_ref())? {
            feed::Offer::None => None,
            feed::Offer::Install { version, asset } => Some(UpdateInfo {
                version: version.to_string(),
                notes: latest.notes,
                source: UpdateSource::Feed {
                    kind: kind.context("an install offer needs an install kind")?,
                    asset,
                },
            }),
            feed::Offer::Manual { version } => Some(UpdateInfo {
                version: version.to_string(),
                notes: latest.notes,
                source: UpdateSource::Manual,
            }),
        },
    )
}

/// The release to finish a stub install with: the latest one, even when it is
/// the version already running, because the install only lacks its payload.
pub async fn check_for_stub_completion() -> Result<UpdateInfo> {
    let release = fetch_latest_release().await?;
    select_completion(release, crate::BRIDGE_VERSION)
}

async fn fetch_latest_release() -> Result<Release> {
    let url = env::var(UPDATE_URL_ENV).unwrap_or_else(|_| LATEST_RELEASE_URL.to_owned());
    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .context("could not create the update client")?;
    let response = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("could not check {url}"))?
        .error_for_status()
        .with_context(|| format!("update check failed for {url}"))?;
    let bytes = read_bounded_response(response, MAX_RELEASE_BYTES).await?;
    serde_json::from_slice::<Release>(&bytes).context("could not parse the latest Bridge release")
}

/// Set once an install is being prepared or has been handed to the update
/// helper. Two helpers racing for the install lock would leave the loser
/// relaunching the old binary, so this process stages at most one install.
static INSTALL_CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn claim_install() -> Result<()> {
    ensure!(
        !INSTALL_CLAIMED.swap(true, std::sync::atomic::Ordering::AcqRel),
        "another Bridge update is already being installed"
    );
    Ok(())
}

fn release_install_claim() {
    INSTALL_CLAIMED.store(false, std::sync::atomic::Ordering::Release);
}

/// Bytes of the update archive received so far, for a shell to draw a progress
/// bar. Only the archive is counted, never the checksum or a release listing.
static DOWNLOADED: AtomicU64 = AtomicU64::new(0);
/// The archive's size as the server announced it, or 0 while it has not.
static DOWNLOAD_TOTAL: AtomicU64 = AtomicU64::new(0);

/// How far the update download has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded: u64,
    /// `None` when the server did not say how big the archive is.
    pub total: Option<u64>,
}

impl DownloadProgress {
    /// 0.0 to 1.0, or `None` while the total is unknown.
    pub fn fraction(self) -> Option<f32> {
        let total = self.total?;
        Some((self.downloaded as f32 / total as f32).clamp(0.0, 1.0))
    }

    /// Every byte has arrived; checking and unpacking it come next.
    pub fn is_complete(self) -> bool {
        self.total.is_some_and(|total| self.downloaded >= total)
    }
}

pub fn download_progress() -> DownloadProgress {
    let total = DOWNLOAD_TOTAL.load(Ordering::Relaxed);
    DownloadProgress {
        downloaded: DOWNLOADED.load(Ordering::Relaxed),
        total: (total > 0).then_some(total),
    }
}

/// An update that has been downloaded, verified and staged, waiting for
/// Bridge to restart into it.
struct Prepared {
    version: String,
    plan: ApplyPlan,
}

/// How a prepared update is applied once Bridge has exited.
enum ApplyPlan {
    /// The shell or PowerShell helpers swap files in: a `-full` zip for a
    /// stub install, or a macOS bundle from the app zip.
    Package {
        staging: StagingDirectory,
        target: PackageTarget,
    },
    /// The new installer runs silently over this install (Windows).
    #[cfg_attr(not(windows), allow(dead_code))]
    Installer {
        staging: StagingDirectory,
        installer: std::path::PathBuf,
        destination: std::path::PathBuf,
        executable: std::path::PathBuf,
    },
    /// The new AppImage replaces the file this one runs from (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    AppImage {
        staged: StagedFile,
        target: std::path::PathBuf,
    },
}

enum PackageTarget {
    /// The executable and its files, flat in `install_directory`.
    Flat {
        install_directory: std::path::PathBuf,
        executable: std::path::PathBuf,
    },
    /// A `.app` replaced as a whole, so its code signature stays intact.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Bundle {
        install_directory: std::path::PathBuf,
        bundle_name: std::ffi::OsString,
    },
}

/// A staged file next to its target, removed again unless a helper took it.
struct StagedFile {
    path: std::path::PathBuf,
    handed_off: bool,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.handed_off {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The update waiting for a restart, if any. Holding one keeps the install
/// claim, so a second download cannot race it.
static PREPARED: std::sync::Mutex<Option<Prepared>> = std::sync::Mutex::new(None);

fn prepared() -> std::sync::MutexGuard<'static, Option<Prepared>> {
    PREPARED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The version staged and waiting for "Restart to update", if any.
pub fn prepared_version() -> Option<String> {
    prepared().as_ref().map(|prepared| prepared.version.clone())
}

/// Downloads `update`, verifies it and stages it, without applying it: that
/// waits for [`apply_prepared`]. Preparing the version already staged does
/// nothing; a different one replaces it.
pub async fn prepare(update: &UpdateInfo) -> Result<()> {
    {
        let mut staged = prepared();
        if staged
            .as_ref()
            .is_some_and(|prepared| prepared.version == update.version)
        {
            return Ok(());
        }
        if staged.take().is_some() {
            release_install_claim();
        }
    }
    claim_install()?;
    match prepare_claimed(update).await {
        Ok(plan) => {
            *prepared() = Some(Prepared {
                version: update.version.clone(),
                plan,
            });
            Ok(())
        }
        Err(error) => {
            release_install_claim();
            Err(error)
        }
    }
}

/// Hands the staged update to its helper, which waits for this process to
/// exit, applies it and starts Bridge again. Returns the version; the caller
/// must then exit.
pub fn apply_prepared() -> Result<String> {
    let Prepared { version, plan } = prepared().take().context("no update is ready to install")?;
    if let Err(error) = plan.apply() {
        release_install_claim();
        return Err(error);
    }
    Ok(version)
}

async fn prepare_claimed(update: &UpdateInfo) -> Result<ApplyPlan> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("could not create the update download client")?;
    DOWNLOADED.store(0, Ordering::Relaxed);
    DOWNLOAD_TOTAL.store(0, Ordering::Relaxed);
    let version = update.version.clone();
    match &update.source {
        UpdateSource::Legacy {
            archive_url,
            checksum_url,
        } => {
            let (archive, checksum) = tokio::try_join!(
                download(&client, archive_url, MAX_ARCHIVE_BYTES, true),
                download(&client, checksum_url, MAX_CHECKSUM_BYTES, false),
            )?;
            verify_checksum(&archive, &checksum)?;
            spawn_staging(move || prepare_archive(&archive, &version)).await
        }
        UpdateSource::Feed { kind, asset } => {
            let limit = usize::try_from(asset.size).context("update is too large")?;
            let bytes = download(&client, &asset.url, limit, true).await?;
            ensure!(
                bytes.len() as u64 == asset.size,
                "update download is {} bytes, latest.json says {}",
                bytes.len(),
                asset.size
            );
            verify_digest(&bytes, &asset.sha256)?;
            match kind.clone() {
                feed::InstallKind::MacBundle => {
                    spawn_staging(move || prepare_archive(&bytes, &version)).await
                }
                feed::InstallKind::WindowsInstaller => {
                    spawn_staging(move || prepare_installer(&bytes)).await
                }
                feed::InstallKind::LinuxAppImage { path } => {
                    spawn_staging(move || prepare_appimage(&bytes, &path)).await
                }
            }
        }
        UpdateSource::Manual => {
            anyhow::bail!("this install cannot update itself; download the new version instead")
        }
    }
}

async fn spawn_staging(
    stage: impl FnOnce() -> Result<ApplyPlan> + Send + 'static,
) -> Result<ApplyPlan> {
    tokio::task::spawn_blocking(stage)
        .await
        .context("update staging task stopped unexpectedly")?
}

async fn download(
    client: &reqwest::Client,
    url: &str,
    limit: usize,
    track: bool,
) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("could not download {url}"))?
        .error_for_status()
        .with_context(|| format!("update download failed for {url}"))?;
    read_bounded(response, limit, track).await
}

pub(crate) async fn read_bounded_response(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>> {
    read_bounded(response, limit, false).await
}

/// Reads a response up to `limit` bytes; with `track`, records its progress in
/// [`download_progress`].
async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
    track: bool,
) -> Result<Vec<u8>> {
    if let Some(length) = response.headers().get(reqwest::header::CONTENT_LENGTH) {
        let length = length
            .to_str()?
            .parse::<u64>()
            .context("invalid download content length")?;
        ensure!(length <= limit as u64, "download exceeds byte limit");
        if track {
            DOWNLOAD_TOTAL.store(length, Ordering::Relaxed);
        }
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("could not read bounded download")?
    {
        ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            "download exceeds byte limit"
        );
        if bytes.capacity() - bytes.len() < chunk.len() {
            let capacity =
                limit.min((bytes.len() + chunk.len()).max(bytes.capacity().saturating_mul(2)));
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .context("could not allocate bounded download buffer")?;
        }
        bytes.extend_from_slice(&chunk);
        if track {
            DOWNLOADED.store(bytes.len() as u64, Ordering::Relaxed);
        }
    }
    Ok(bytes)
}

fn release_version(release: &Release) -> Result<Version> {
    let version_text = release.tag_name.trim_start_matches('v');
    Version::parse(version_text)
        .with_context(|| format!("release {} has an invalid version", release.tag_name))
}

fn select_completion(release: Release, current: &str) -> Result<UpdateInfo> {
    let current = Version::parse(current).context("Bridge has an invalid current version")?;
    let available = release_version(&release)?;
    ensure!(
        available >= current,
        "the latest release {available} is older than this Bridge"
    );
    package_update(&release, available)
}

fn package_update(release: &Release, available: Version) -> Result<UpdateInfo> {
    let archive_name = platform_archive_name()?;
    let checksum_name = format!("{archive_name}.sha256");
    let archive_url = asset_url(&release.assets, archive_name)?;
    let checksum_url = asset_url(&release.assets, &checksum_name)?;
    Ok(UpdateInfo {
        version: available.to_string(),
        notes: String::new(),
        source: UpdateSource::Legacy {
            archive_url,
            checksum_url,
        },
    })
}

fn asset_url(assets: &[ReleaseAsset], name: &str) -> Result<String> {
    assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| anyhow!("release does not contain {name}"))
}

fn platform_archive_name() -> Result<&'static str> {
    // Installs from the DMG run as a signed .app and are replaced as a whole
    // bundle. Everything else follows the `-full` zips: the unsuffixed zips of
    // the same platforms are small bridge packages (see `is_stub_install`) for
    // the older updaters, which cannot open an archive over their 16 MiB
    // metadata budget.
    #[cfg(target_os = "macos")]
    return Ok(if crate::platform::bundle_root().is_some() {
        "openmouse-bridge-macos-app.zip"
    } else {
        "openmouse-bridge-macos-universal-full.zip"
    });
    #[cfg(target_os = "windows")]
    return Ok("openmouse-bridge-windows-x64-full.zip");
    #[cfg(target_os = "linux")]
    return Ok("openmouse-bridge-linux-x64-full.zip");
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    anyhow::bail!("automatic Bridge updates are available only on macOS, Windows, and Linux");
}

/// A bridge package keeps the Node.js runtime as a placeholder this small; the
/// real one is tens of megabytes.
const PLACEHOLDER_RUNTIME_MAX_BYTES: u64 = 4096;

/// Whether this install came from a bridge package: the small archive that
/// older updaters can open, which carries this binary but only a placeholder
/// for the bundled Node.js runtime. It has to fetch the full package before
/// the native HID helper can run. Bundled macOS installs are always complete.
pub fn is_stub_install() -> bool {
    #[cfg(target_os = "macos")]
    if crate::platform::bundle_root().is_some() {
        return false;
    }
    env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|parent| parent.join("native-hid")))
        .is_some_and(|native_hid| runtime_is_placeholder(&native_hid))
}

fn runtime_is_placeholder(native_hid: &Path) -> bool {
    let runtime = if cfg!(windows) {
        native_hid.join("node/node.exe")
    } else {
        native_hid.join("node/bin/node")
    };
    fs::metadata(runtime)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() < PLACEHOLDER_RUNTIME_MAX_BYTES)
}

fn verify_checksum(archive: &[u8], checksum_file: &[u8]) -> Result<()> {
    let checksum = std::str::from_utf8(checksum_file)
        .context("update checksum is not UTF-8")?
        .split_whitespace()
        .next()
        .context("update checksum is empty")?;
    ensure!(
        checksum.len() == 64,
        "update checksum has an invalid length"
    );
    let actual = format!("{:x}", Sha256::digest(archive));
    ensure!(
        actual.eq_ignore_ascii_case(checksum),
        "update checksum does not match"
    );
    Ok(())
}

/// Checks `bytes` against the SHA-256 `latest.json` gave for them.
fn verify_digest(bytes: &[u8], expected: &str) -> Result<()> {
    ensure!(
        format!("{:x}", Sha256::digest(bytes)).eq_ignore_ascii_case(expected),
        "update checksum does not match"
    );
    Ok(())
}

struct StagingDirectory {
    path: std::path::PathBuf,
    handed_off: bool,
}

impl StagingDirectory {
    fn new_in(parent: &Path) -> Result<Self> {
        use std::hash::BuildHasher;
        verify_directory_chain(parent)?;
        for _ in 0..32 {
            let nonce = std::collections::hash_map::RandomState::new()
                .hash_one((std::process::id(), std::time::SystemTime::now()));
            let path = parent.join(format!("openmouse-update-{nonce:016x}"));
            match private_directory_builder().create(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        handed_off: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("could not create private update staging"),
            }
        }
        anyhow::bail!("could not allocate unique update staging")
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if !self.handed_off {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Where update staging directories go: the private user cache on Windows,
/// the temporary directory elsewhere.
fn staging_parent() -> Result<std::path::PathBuf> {
    #[cfg(windows)]
    let parent = directories::BaseDirs::new()
        .context("could not locate private user cache")?
        .cache_dir()
        .to_path_buf();
    #[cfg(not(windows))]
    let parent = env::temp_dir();
    let parent = fs::canonicalize(parent).context("could not resolve update staging parent")?;
    Ok(without_verbatim_prefix(parent))
}

/// Unpacks and checks an update archive and works out where it installs.
fn prepare_archive(archive: &[u8], version: &str) -> Result<ApplyPlan> {
    Version::parse(version).context("invalid update version")?;
    let staging = StagingDirectory::new_in(&staging_parent()?)?;
    extract_archive(archive, &staging.path)?;
    verify_package(&staging.path)?;
    let current_executable =
        env::current_exe().context("could not locate the Bridge executable")?;
    ensure!(
        current_executable.file_name() == Some(std::ffi::OsStr::new(platform_binary_name())),
        "Bridge executable has an unexpected name"
    );
    // A bundled install replaces the whole .app so its code signature stays
    // intact; the directory that must be writable is the one holding it.
    #[cfg(target_os = "macos")]
    if let Some(bundle) = crate::platform::bundle_root() {
        let install_directory = bundle
            .parent()
            .context("Bridge bundle has no parent directory")?;
        let bundle_name = bundle
            .file_name()
            .context("Bridge bundle has no file name")?;
        verify_install_targets(install_directory, &[bundle_name])?;
        return Ok(ApplyPlan::Package {
            target: PackageTarget::Bundle {
                install_directory: install_directory.to_path_buf(),
                bundle_name: bundle_name.to_owned(),
            },
            staging,
        });
    }
    let install_directory = current_executable
        .parent()
        .context("Bridge executable has no parent directory")?;
    verify_install_directory(install_directory)?;
    Ok(ApplyPlan::Package {
        target: PackageTarget::Flat {
            install_directory: install_directory.to_path_buf(),
            executable: current_executable.clone(),
        },
        staging,
    })
}

/// Stages the Windows installer in a private directory. It installs over the
/// directory this Bridge runs from, so a portable copy is updated in place.
#[cfg_attr(not(windows), allow(dead_code))]
fn prepare_installer(installer: &[u8]) -> Result<ApplyPlan> {
    let staging = StagingDirectory::new_in(&staging_parent()?)?;
    let path = staging.path.join("OpenMouse-Bridge-setup.exe");
    write_new_file(&path, installer, false)?;
    let executable = env::current_exe().context("could not locate the Bridge executable")?;
    let executable = without_verbatim_prefix(executable);
    let destination = executable
        .parent()
        .context("Bridge executable has no parent directory")?
        .to_path_buf();
    Ok(ApplyPlan::Installer {
        staging,
        installer: path,
        destination,
        executable,
    })
}

/// Stages the new AppImage beside `target`, so the helper's rename stays on
/// one filesystem and is atomic. Fails early when that directory is not
/// writable, rather than after Bridge has exited.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn prepare_appimage(image: &[u8], target: &Path) -> Result<ApplyPlan> {
    use std::hash::BuildHasher;
    let directory = target
        .parent()
        .context("the AppImage has no parent directory")?;
    verify_directory_chain(directory)?;
    let nonce = std::collections::hash_map::RandomState::new()
        .hash_one((std::process::id(), std::time::SystemTime::now()));
    let staged = StagedFile {
        path: directory.join(format!("{APPIMAGE_STAGING_PREFIX}{nonce:016x}.AppImage")),
        handed_off: false,
    };
    write_new_file(&staged.path, image, true).with_context(|| {
        format!(
            "could not save the update next to {}; is that folder writable?",
            target.display()
        )
    })?;
    Ok(ApplyPlan::AppImage {
        staged,
        target: target.to_path_buf(),
    })
}

/// Writes a file that must not exist yet, executable when asked (Unix).
fn write_new_file(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o755 } else { 0o600 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    let mut file = options
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("could not write {}", path.display()))
}

impl ApplyPlan {
    /// Starts the helper that applies this plan once Bridge has exited.
    fn apply(self) -> Result<()> {
        let log = UpdateLog::new()?;
        match self {
            Self::Package {
                mut staging,
                target,
            } => {
                match target {
                    PackageTarget::Flat {
                        install_directory,
                        executable,
                    } => {
                        spawn_install_helper(&staging.path, &install_directory, &executable, &log)?
                    }
                    #[cfg(target_os = "macos")]
                    PackageTarget::Bundle {
                        install_directory,
                        bundle_name,
                    } => spawn_bundle_install_helper(
                        &staging.path,
                        &install_directory,
                        &bundle_name,
                        &log,
                    )?,
                    #[cfg(not(target_os = "macos"))]
                    PackageTarget::Bundle { .. } => {
                        anyhow::bail!("app bundles are updated only on macOS")
                    }
                }
                staging.handed_off = true;
            }
            #[cfg(windows)]
            Self::Installer {
                mut staging,
                installer,
                destination,
                executable,
            } => {
                spawn_installer_helper(&staging.path, &installer, &destination, &executable, &log)?;
                staging.handed_off = true;
            }
            #[cfg(target_os = "linux")]
            Self::AppImage { mut staged, target } => {
                spawn_appimage_helper(&staged.path, &target, &log)?;
                staged.handed_off = true;
            }
            #[allow(unreachable_patterns)]
            _ => anyhow::bail!("this kind of update cannot be applied on this platform"),
        }
        Ok(())
    }
}

/// Where the helpers write: an append-only log in Bridge's log directory, and
/// a marker the next launch reads to say the update failed.
struct UpdateLog {
    log: std::path::PathBuf,
    failure: std::path::PathBuf,
}

/// The helpers' log, `update.log` in the Bridge log directory.
const UPDATE_LOG_NAME: &str = "update.log";
/// Written by a helper whose update failed; read (and removed) on next launch.
const UPDATE_FAILURE_NAME: &str = "update-failed.txt";

impl UpdateLog {
    fn new() -> Result<Self> {
        let directory = crate::config::log_dir()?;
        fs::create_dir_all(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        Ok(Self {
            log: directory.join(UPDATE_LOG_NAME),
            failure: directory.join(UPDATE_FAILURE_NAME),
        })
    }
}

/// Prefix of the AppImage staged beside the installed one.
const APPIMAGE_STAGING_PREFIX: &str = ".openmouse-bridge-update-";
/// Staging directories older than this are left over from an update that was
/// never applied; younger ones may still belong to a helper finishing up.
const STALE_STAGING_AGE: Duration = Duration::from_secs(60 * 60);

/// Removes downloads a previous run staged but never applied (Bridge quit
/// instead of restarting into them). Call only once this process is known to
/// be the only Bridge running.
pub fn remove_stale_downloads() {
    if let Some(feed::InstallKind::LinuxAppImage { path }) = feed::current_install_kind()
        && let Some(directory) = path.parent()
    {
        remove_matching(directory, |name, _| {
            name.starts_with(APPIMAGE_STAGING_PREFIX) && name.ends_with(".AppImage")
        });
    }
    if let Ok(parent) = staging_parent() {
        remove_matching(&parent, |name, metadata| {
            name.starts_with("openmouse-update-")
                && metadata
                    .modified()
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > STALE_STAGING_AGE)
        });
    }
}

/// Deletes the entries of `directory` that `stale` picks, never following
/// links.
fn remove_matching(directory: &Path, stale: impl Fn(&str, &fs::Metadata) -> bool) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let (Some(name), Ok(metadata)) = (
            entry.file_name().to_str().map(str::to_owned),
            entry.metadata(),
        ) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !stale(&name, &metadata) {
            continue;
        }
        let path = entry.path();
        let removed = if metadata.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match removed {
            Ok(()) => tracing::info!(path = %path.display(), "removed a stale update download"),
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "could not remove a stale update download");
            }
        }
    }
}

/// Why the last update did not install, as its helper recorded it, if it
/// failed. Reading it clears it, so it is reported once.
pub fn take_last_failure() -> Option<String> {
    let path = crate::config::log_dir().ok()?.join(UPDATE_FAILURE_NAME);
    let text = fs::read(&path).ok()?;
    let _ = fs::remove_file(&path);
    let text = String::from_utf8_lossy(&text[..text.len().min(1024)])
        .trim()
        .to_owned();
    Some(if text.is_empty() {
        "the last update could not be installed".to_owned()
    } else {
        text
    })
}

/// `fs::canonicalize` returns `\\?\C:\...` on Windows. Windows PowerShell 5.1
/// cannot walk the parents of such a path, so the install helper threw before
/// installing or relaunching anything; hand it the plain drive path instead.
/// UNC and other verbatim forms are kept as they are.
fn without_verbatim_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    match text.strip_prefix(r"\\?\") {
        Some(rest)
            if rest.as_bytes().get(1) == Some(&b':')
                && rest.as_bytes()[0].is_ascii_alphabetic() =>
        {
            rest.into()
        }
        _ => path,
    }
}

/// Where an update package keeps the files Bridge needs, relative to the
/// extracted archive: flat next to the executable, or inside a macOS bundle.
struct PackageLayout {
    binary: std::path::PathBuf,
    license: std::path::PathBuf,
    native_hid: std::path::PathBuf,
    extra: Vec<std::path::PathBuf>,
}

impl PackageLayout {
    fn flat() -> Self {
        Self {
            binary: platform_binary_name().into(),
            license: "DM-Sans-OFL.txt".into(),
            native_hid: "native-hid".into(),
            extra: Vec::new(),
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn macos_bundle() -> Self {
        let app = Path::new(MACOS_APP_NAME);
        Self {
            binary: app.join("Contents/MacOS/openmouse-bridge"),
            license: app.join("Contents/Resources/DM-Sans-OFL.txt"),
            native_hid: app.join("Contents/Resources/native-hid"),
            extra: vec![app.join("Contents/Info.plist")],
        }
    }

    fn current() -> Self {
        #[cfg(target_os = "macos")]
        if crate::platform::bundle_root().is_some() {
            return Self::macos_bundle();
        }
        Self::flat()
    }
}

fn verify_package(staging: &Path) -> Result<()> {
    verify_package_with(staging, &PackageLayout::current())
}

fn verify_package_with(staging: &Path, layout: &PackageLayout) -> Result<()> {
    verify_tree(staging)?;
    let license = staging.join(&layout.license);
    let expected = include_bytes!("../assets/fonts/DM-Sans-OFL.txt");
    ensure!(
        reject_link(&license)?.is_file() && fs::metadata(&license)?.len() == expected.len() as u64,
        "update package requires the complete DM Sans license"
    );
    ensure!(
        fs::read(&license)? == expected,
        "update package contains an incomplete DM Sans license"
    );
    let node = if cfg!(windows) {
        layout.native_hid.join("node/node.exe")
    } else {
        layout.native_hid.join("node/bin/node")
    };
    let hid = &layout.native_hid;
    let mut required = vec![
        layout.binary.clone(),
        hid.join("src/apply.mjs"),
        hid.join("src/brands.mjs"),
        hid.join("src/hid-device-adapter.mjs"),
        hid.join("src/attack-shark-x11.mjs"),
        hid.join("package.json"),
        node.clone(),
        hid.join("node_modules/node-hid/package.json"),
        hid.join("node_modules/@openmouse/protocol/package.json"),
        hid.join("node_modules/@openmouse/keyboard-protocol/package.json"),
    ];
    required.extend(layout.extra.iter().cloned());
    for name in &required {
        let path = staging.join(name);
        verify_directory_chain(path.parent().context("missing package parent")?)?;
        let metadata = reject_link(&path)
            .with_context(|| format!("update package is missing {}", name.display()))?;
        ensure!(
            metadata.is_file() && metadata.len() > 0,
            "update package file is empty or invalid: {}",
            name.display()
        );
        #[cfg(unix)]
        if *name == layout.binary || *name == node {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o500 == 0o500,
                "update executable requires owner read and execute access: {}",
                name.display()
            );
        }
    }
    Ok(())
}

const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 4096;
const MAX_RELEASE_BYTES: usize = 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 8192;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024 * 1024;

fn extract_archive(archive: &[u8], staging: &Path) -> Result<()> {
    extract_archive_with_limits(
        archive,
        staging,
        MAX_ARCHIVE_ENTRIES,
        MAX_FILE_BYTES,
        MAX_OUTPUT_BYTES,
    )
}

fn safe_entry_path(name: &str) -> Result<&Path> {
    ensure!(
        !name.is_empty() && name.len() <= 1024 && !name.starts_with('/'),
        "unsafe archive path"
    );
    ensure!(!name.contains(['\\', ':', '\0']), "unsafe archive path");
    let name = name.strip_suffix('/').unwrap_or(name);
    let mut count = 0;
    for part in name.split('/') {
        count += 1;
        ensure!(
            !part.is_empty() && part != "." && part != ".." && !part.ends_with(['.', ' ']),
            "unsafe archive path component"
        );
        ensure!(
            !part
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')),
            "unsafe archive path component"
        );
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(
            !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()),
            "reserved archive path component"
        );
    }
    ensure!(count <= 64, "archive path is too deep");
    Ok(Path::new(name))
}

fn reject_link(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "update path contains a symbolic link"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "update path contains a reparse point"
        );
    }
    Ok(metadata)
}

fn verify_directory_chain(directory: &Path) -> Result<()> {
    ensure!(directory.is_absolute(), "update directory must be absolute");
    for ancestor in directory.ancestors() {
        ensure!(
            reject_link(ancestor)?.is_dir(),
            "update ancestor is not a directory"
        );
    }
    Ok(())
}

fn private_directory_builder() -> fs::DirBuilder {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder
}

fn create_entry_directories(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(_) => ensure!(
                reject_link(&path)?.is_dir(),
                "archive parent is not a directory"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let builder = private_directory_builder();
                builder.create(&path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        fs::canonicalize(&path)?.starts_with(fs::canonicalize(root)?),
        "archive path escapes staging"
    );
    Ok(())
}

const MAX_ZIP_METADATA_READ_BYTES: usize = 16 * 1024 * 1024;

struct ArchiveReader<'a> {
    inner: Cursor<&'a [u8]>,
    metadata_budget: &'a std::cell::Cell<Option<usize>>,
}

impl io::Read for ArchiveReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.metadata_budget.get();
        let limit = remaining.unwrap_or(buffer.len()).min(buffer.len());
        if !buffer.is_empty() && remaining == Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP metadata read budget exceeded",
            ));
        }
        let read = self.inner.read(&mut buffer[..limit])?;
        if let Some(remaining) = remaining {
            self.metadata_budget.set(Some(remaining - read));
        }
        Ok(read)
    }
}

impl io::Seek for ArchiveReader<'_> {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

fn extract_archive_with_limits(
    archive: &[u8],
    staging: &Path,
    entries: usize,
    per_file: u64,
    total: u64,
) -> Result<()> {
    use std::io::Read;
    ensure!(
        archive.len() <= MAX_ARCHIVE_BYTES,
        "update archive exceeds download limit"
    );
    verify_directory_chain(staging)?;
    let metadata_budget = std::cell::Cell::new(Some(MAX_ZIP_METADATA_READ_BYTES));
    let reader = ArchiveReader {
        inner: Cursor::new(archive),
        metadata_budget: &metadata_budget,
    };
    // Update archives never carry prepended data. Without a known offset the
    // zip crate scans the whole file forward looking for the central
    // directory, which spends the entire metadata budget on any archive
    // larger than it - that is, every real release.
    let mut zip = zip::ZipArchive::with_config(
        zip::read::Config {
            archive_offset: zip::read::ArchiveOffset::Known(0),
        },
        reader,
    )
    .context("update archive is invalid or exceeds metadata budget")?;
    metadata_budget.set(None);
    ensure!(zip.len() <= entries, "update archive has too many entries");
    let mut declared_total = 0u64;
    let mut names = std::collections::BTreeSet::new();
    let mut output_paths = std::collections::BTreeSet::new();
    for index in 0..zip.len() {
        let entry = zip
            .by_index(index)
            .context("could not read update archive entry")?;
        let name = entry.name()?;
        let relative = safe_entry_path(&name)?;
        let enclosed = entry
            .enclosed_name()
            .context("update archive contains an unsafe path")?;
        ensure!(
            relative == enclosed,
            "archive path normalization is not allowed"
        );
        ensure!(
            names.insert(relative.to_string_lossy().to_lowercase()),
            "duplicate archive path"
        );
        for path in relative
            .ancestors()
            .filter(|path| !path.as_os_str().is_empty())
        {
            output_paths.insert(path.to_string_lossy().to_lowercase());
            ensure!(
                output_paths.len() <= entries,
                "archive creates too many filesystem entries"
            );
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            ensure!(
                kind == 0 || kind == if entry.is_dir() { 0o040000 } else { 0o100000 },
                "unsupported archive entry type"
            );
        }
        ensure!(entry.size() <= per_file, "archive entry exceeds size limit");
        ensure!(
            !entry.is_dir() || entry.size() == 0,
            "archive directory has file data"
        );
        declared_total = declared_total
            .checked_add(entry.size())
            .context("archive size overflow")?;
        ensure!(declared_total <= total, "archive exceeds output limit");
        let destination = staging.join(relative);
        ensure!(
            destination.starts_with(staging),
            "archive path escapes staging"
        );
        let mut cursor = destination.as_path();
        while cursor != staging {
            match fs::symlink_metadata(cursor) {
                Ok(_) => {
                    let metadata = reject_link(cursor)?;
                    ensure!(
                        metadata.is_dir() && (cursor != destination || entry.is_dir()),
                        "archive would overwrite an existing file"
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            cursor = cursor.parent().context("archive path escapes staging")?;
        }
    }
    let mut written_total = 0;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let relative = safe_entry_path(&entry.name()?)?.to_path_buf();
        let destination = staging.join(&relative);
        if entry.is_dir() {
            create_entry_directories(staging, &relative)?;
            continue;
        }
        if let Some(parent) = relative.parent() {
            create_entry_directories(staging, parent)?;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&destination)
            .with_context(|| format!("could not create {}", destination.display()))?;
        let limit = per_file.min(total - written_total);
        let copied = io::copy(&mut (&mut entry).take(limit), &mut output)?;
        let mut extra = [0u8; 1];
        ensure!(
            entry.read(&mut extra)? == 0 && copied == entry.size(),
            "archive entry exceeds output limit or declared size"
        );
        written_total += copied;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            output.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    ensure!(
        reject_link(&staging.join(&PackageLayout::current().binary))?.is_file(),
        "update archive does not contain the Bridge executable"
    );
    Ok(())
}

fn verify_install_directory(directory: &Path) -> Result<()> {
    verify_install_targets(
        directory,
        &[
            std::ffi::OsStr::new(platform_binary_name()),
            std::ffi::OsStr::new("native-hid"),
            std::ffi::OsStr::new("openmouse-app-icon.png"),
            std::ffi::OsStr::new("DM-Sans-OFL.txt"),
        ],
    )
}

/// Checks that `directory` is a plain, unlocked, writable directory and that
/// every existing entry named in `names` is a link-free tree.
fn verify_install_targets(directory: &Path, names: &[&std::ffi::OsStr]) -> Result<()> {
    verify_directory_chain(directory)?;
    match fs::symlink_metadata(directory.join(".openmouse-update.lock")) {
        Ok(_) => anyhow::bail!("Bridge update lock already exists"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("could not inspect Bridge update lock"),
    }
    for name in names {
        let path = directory.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => verify_tree(&path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let _probe =
        StagingDirectory::new_in(directory).context("Bridge install directory is not writable")?;
    Ok(())
}

fn verify_tree(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(path) = pending.pop() {
        count += 1;
        ensure!(
            count <= MAX_ARCHIVE_ENTRIES,
            "update tree has too many entries"
        );
        let metadata = reject_link(&path)?;
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "unsupported update file type"
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                pending.push(entry?.path());
                ensure!(
                    count + pending.len() <= MAX_ARCHIVE_ENTRIES,
                    "update tree has too many entries"
                );
            }
        }
    }
    Ok(())
}

fn write_helper(path: &Path, script: &str) -> Result<()> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(script.as_bytes())
        .context("could not write the update helper")
}

fn platform_binary_name() -> &'static str {
    #[cfg(target_os = "windows")]
    return "openmouse-bridge.exe";
    #[cfg(not(target_os = "windows"))]
    return "openmouse-bridge";
}

#[cfg(any(unix, test))]
const UNIX_TRANSACTION: &str = r#"install_package() (
set -eu
stage="$1"
destination="$2"
binary="$3"
[ -f "$stage/$binary" ] && [ -d "$stage/native-hid" ] || exit 1
for tree_root in "$stage" "$destination"; do
  parent="$tree_root"
  while [ "$parent" != / ]; do
    [ -d "$parent" ] && [ ! -L "$parent" ] || exit 1
    parent=$(dirname "$parent") || exit 1
  done
done
special=$(find "$stage" ! -type f ! -type d -print) || exit 1
[ -z "$special" ] || exit 1
names="native-hid $binary DM-Sans-OFL.txt"
if [ -f "$stage/openmouse-app-icon.png" ]; then names="openmouse-app-icon.png $names"; fi
lock="$destination/.openmouse-update.lock"
(umask 077; mkdir "$lock") || exit 2
transaction=$(umask 077; mktemp -d "$destination/.openmouse-update.XXXXXXXX") || { rmdir "$lock" || :; exit 2; }
committed=no
rollback() {
  status=$?
  trap - 0 1 2 15
  failed=no
  if [ "$committed" != yes ]; then
    for name in $names; do
      if [ -e "$transaction/old/$name" ]; then
        rm -rf "$destination/$name" && mv "$transaction/old/$name" "$destination/$name" || failed=yes
      elif [ -f "$transaction/installed/$name" ]; then
        rm -rf "$destination/$name" || failed=yes
      fi
    done
  fi
  if [ "$failed" = yes ]; then
    printf '%s\n' "Update rollback failed; backups retained at $transaction" >&2
    exit 1
  fi
  rm -rf "$transaction"
  rmdir "$lock"
  if [ "$committed" != yes ]; then exit 2; fi
  exit "$status"
}
trap rollback 0
trap 'exit 1' 1 2 15
mkdir "$transaction/new" "$transaction/old" "$transaction/installed" || exit 1
for name in $names; do
  if [ -e "$destination/$name" ] || [ -L "$destination/$name" ]; then
    special=$(find "$destination/$name" ! -type f ! -type d -print) || exit 1
    [ -z "$special" ] || exit 1
  fi
  cp -R "$stage/$name" "$transaction/new/$name" || exit 1
 done
chmod u+x "$transaction/new/$binary" || exit 1
for name in $names; do
  if [ -e "$destination/$name" ]; then mv "$destination/$name" "$transaction/old/$name" || exit 1; fi
 done
for name in $names; do
  : > "$transaction/installed/$name" || exit 1
  mv "$transaction/new/$name" "$destination/$name" || exit 1
 done
committed=yes
)
"#;

#[cfg(any(unix, test))]
const UNIX_COMPLETION: &str = r#"complete_update() {
  if install_package "$1" "$2" "$3"; then
    :
  else
    status=$?
    [ "$status" -eq 2 ] || return 1
  fi
  "$4" "$2" "$3"
}
"#;

/// Shell lines every Unix helper starts with: its output goes to the update
/// log, `note` timestamps a line, `fail` also leaves the failure marker for
/// the next launch. Expects `$log` and `$failure`.
#[cfg(any(target_os = "macos", target_os = "linux", all(unix, test)))]
const UNIX_LOGGING: &str = r#"exec >>"$log" 2>&1
note() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }
fail() { note "$1"; printf '%s\n' "$1" > "$failure"; }
"#;

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn spawn_install_helper(
    staging: &Path,
    destination: &Path,
    executable: &Path,
    log: &UpdateLog,
) -> Result<()> {
    let script = staging.join("install-update.sh");
    let contents = format!(
        "#!/bin/sh\n{UNIX_TRANSACTION}\n{UNIX_COMPLETION}\npid=\"$1\"\nstage=\"$2\"\ndestination=\"$3\"\nbinary=\"$4\"\nlog=\"$5\"\nfailure=\"$6\"\n{UNIX_LOGGING}trap 'rm -rf \"$stage\"' 0\nwhile kill -0 \"$pid\" 2>/dev/null; do sleep 1; done\nnote \"Installing the update into $destination\"\nlaunch_bridge() {{ cd \"$1\" || return 1; nohup \"$1/$2\" >/dev/null 2>&1 & }}\ncomplete_update \"$stage\" \"$destination\" \"$binary\" launch_bridge && note 'Done' || fail 'The update could not be installed; Bridge kept the current version'\n"
    );
    write_helper(&script, &contents)?;
    Command::new("/bin/sh")
        .arg(&script)
        .arg(std::process::id().to_string())
        .arg(staging)
        .arg(destination)
        .arg(
            executable
                .file_name()
                .context("Bridge executable has no file name")?,
        )
        .arg(&log.log)
        .arg(&log.failure)
        .spawn()
        .context("could not launch the update helper")?;
    Ok(())
}

/// Replaces the AppImage at `$target` with the staged one: the old image is
/// moved aside first and put back if the new one cannot be moved in, and
/// whichever is in place is started again either way.
#[cfg(any(target_os = "linux", all(unix, test)))]
const APPIMAGE_SWAP: &str = r#"swap_appimage() {
  staged="$1"
  target="$2"
  previous="$target.previous"
  if [ ! -f "$staged" ]; then
    fail 'The downloaded update was missing; Bridge kept the current version'
    return 1
  fi
  if ! mv -f "$target" "$previous"; then
    rm -f "$staged"
    fail "Could not move $target aside; Bridge kept the current version"
    return 1
  fi
  if ! mv -f "$staged" "$target"; then
    mv -f "$previous" "$target"
    rm -f "$staged"
    fail 'Could not install the new AppImage; Bridge kept the current version'
    return 1
  fi
  rm -f "$previous"
  note "Installed the new AppImage at $target"
}
"#;

#[cfg(target_os = "linux")]
fn spawn_appimage_helper(staged: &Path, target: &Path, log: &UpdateLog) -> Result<()> {
    // The helper outlives Bridge and the AppImage mount it runs from, so its
    // script lives in the temporary directory, not inside the image.
    let script = tempfile_path("openmouse-update", "sh")?;
    let contents = format!(
        "#!/bin/sh\npid=\"$1\"\nstaged=\"$2\"\ntarget=\"$3\"\nlog=\"$4\"\nfailure=\"$5\"\n{UNIX_LOGGING}{APPIMAGE_SWAP}trap 'rm -f \"$0\"' 0\nwhile kill -0 \"$pid\" 2>/dev/null; do sleep 1; done\nnote \"Updating $target\"\nswap_appimage \"$staged\" \"$target\"\n(cd \"$(dirname \"$target\")\" && exec nohup \"$target\" </dev/null >/dev/null 2>&1) &\n"
    );
    write_helper(&script, &contents)?;
    Command::new("/bin/sh")
        .arg(&script)
        .arg(std::process::id().to_string())
        .arg(staged)
        .arg(target)
        .arg(&log.log)
        .arg(&log.failure)
        // The AppImage runtime sets these for this image; the new image must
        // set its own when it starts.
        .env_remove("APPIMAGE")
        .env_remove("APPDIR")
        .env_remove("ARGV0")
        .env_remove("OWD")
        .spawn()
        .context("could not launch the update helper")?;
    Ok(())
}

/// A fresh, unused path in the temporary directory.
#[cfg(target_os = "linux")]
fn tempfile_path(prefix: &str, extension: &str) -> Result<std::path::PathBuf> {
    use std::hash::BuildHasher;
    let directory = staging_parent()?;
    for _ in 0..32 {
        let nonce = std::collections::hash_map::RandomState::new()
            .hash_one((std::process::id(), std::time::SystemTime::now()));
        let path = directory.join(format!("{prefix}-{nonce:016x}.{extension}"));
        if !path.exists() {
            return Ok(path);
        }
    }
    anyhow::bail!("could not allocate a temporary helper path")
}

/// Replaces one directory (a macOS `.app`) as a unit. Same lock, backup and
/// rollback discipline as `UNIX_TRANSACTION`: status 2 means nothing changed.
#[cfg(any(target_os = "macos", all(unix, test)))]
const BUNDLE_TRANSACTION: &str = r#"install_bundle() (
set -eu
stage="$1"
destination="$2"
staged="$3"
installed="$4"
[ -d "$stage/$staged/Contents/MacOS" ] || exit 1
for tree_root in "$stage" "$destination"; do
  parent="$tree_root"
  while [ "$parent" != / ]; do
    [ -d "$parent" ] && [ ! -L "$parent" ] || exit 1
    parent=$(dirname "$parent") || exit 1
  done
done
special=$(find "$stage" ! -type f ! -type d -print) || exit 1
[ -z "$special" ] || exit 1
lock="$destination/.openmouse-update.lock"
(umask 077; mkdir "$lock") || exit 2
transaction=$(umask 077; mktemp -d "$destination/.openmouse-update.XXXXXXXX") || { rmdir "$lock" || :; exit 2; }
committed=no
rollback() {
  status=$?
  trap - 0 1 2 15
  failed=no
  if [ "$committed" != yes ]; then
    if [ -e "$transaction/old/$installed" ]; then
      rm -rf "$destination/$installed" && mv "$transaction/old/$installed" "$destination/$installed" || failed=yes
    elif [ -f "$transaction/installed/marker" ]; then
      rm -rf "$destination/$installed" || failed=yes
    fi
  fi
  if [ "$failed" = yes ]; then
    printf '%s\n' "Update rollback failed; backups retained at $transaction" >&2
    exit 1
  fi
  rm -rf "$transaction"
  rmdir "$lock"
  if [ "$committed" != yes ]; then exit 2; fi
  exit "$status"
}
trap rollback 0
trap 'exit 1' 1 2 15
mkdir "$transaction/new" "$transaction/old" "$transaction/installed" || exit 1
if [ -e "$destination/$installed" ] || [ -L "$destination/$installed" ]; then
  special=$(find "$destination/$installed" ! -type f ! -type d -print) || exit 1
  [ -z "$special" ] || exit 1
fi
cp -R "$stage/$staged" "$transaction/new/$installed" || exit 1
chmod u+x "$transaction/new/$installed/Contents/MacOS/openmouse-bridge" || exit 1
if [ -e "$destination/$installed" ]; then mv "$destination/$installed" "$transaction/old/$installed" || exit 1; fi
: > "$transaction/installed/marker" || exit 1
mv "$transaction/new/$installed" "$destination/$installed" || exit 1
committed=yes
)
"#;

#[cfg(any(target_os = "macos", all(unix, test)))]
const BUNDLE_COMPLETION: &str = r#"complete_bundle_update() {
  if install_bundle "$1" "$2" "$3" "$4"; then
    :
  else
    status=$?
    [ "$status" -eq 2 ] || return 1
  fi
  "$5" "$2" "$4"
}
"#;

#[cfg(target_os = "macos")]
fn spawn_bundle_install_helper(
    staging: &Path,
    destination: &Path,
    installed: &std::ffi::OsStr,
    log: &UpdateLog,
) -> Result<()> {
    let script = staging.join("install-update.sh");
    let contents = format!(
        "#!/bin/sh\n{BUNDLE_TRANSACTION}\n{BUNDLE_COMPLETION}\npid=\"$1\"\nstage=\"$2\"\ndestination=\"$3\"\nstaged=\"$4\"\ninstalled=\"$5\"\nlog=\"$6\"\nfailure=\"$7\"\n{UNIX_LOGGING}trap 'rm -rf \"$stage\"' 0\nwhile kill -0 \"$pid\" 2>/dev/null; do sleep 1; done\nnote \"Installing the update into $destination/$installed\"\nlaunch_bundle() {{ open \"$1/$2\"; }}\ncomplete_bundle_update \"$stage\" \"$destination\" \"$staged\" \"$installed\" launch_bundle && note 'Done' || fail 'The update could not be installed; Bridge kept the current version'\n"
    );
    write_helper(&script, &contents)?;
    Command::new("/bin/sh")
        .arg(&script)
        .arg(std::process::id().to_string())
        .arg(staging)
        .arg(destination)
        .arg(MACOS_APP_NAME)
        .arg(installed)
        .arg(&log.log)
        .arg(&log.failure)
        .spawn()
        .context("could not launch the update helper")?;
    Ok(())
}

#[cfg(windows)]
const WINDOWS_TRANSACTION: &str = r#"function Assert-CleanTree([string]$Path) {
  $Pending = [System.Collections.Generic.Stack[string]]::new()
  $Pending.Push($Path)
  $Count = 0
  while ($Pending.Count -gt 0) {
    $Item = Get-Item -LiteralPath $Pending.Pop() -Force -ErrorAction Stop
    if (($Item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Update path contains a reparse point' }
    $Count++
    if ($Count -gt 8192) { throw 'Update tree has too many entries' }
    if ($Item.PSIsContainer) {
      foreach ($Child in Get-ChildItem -LiteralPath $Item.FullName -Force -ErrorAction Stop) {
        $Pending.Push($Child.FullName)
        if ($Count + $Pending.Count -gt 8192) { throw 'Update tree has too many entries' }
      }
    }
  }
}
function Install-BridgePackage([string]$Stage, [string]$Destination, [string]$Binary) {
  foreach ($Root in @($Stage, $Destination)) {
    $Parent = Get-Item -LiteralPath $Root -Force
    while ($null -ne $Parent) {
      if (($Parent.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Update parent contains a reparse point' }
      $Parent = $Parent.Parent
    }
  }
  Assert-CleanTree $Stage
  $Names = @('native-hid', $Binary, 'DM-Sans-OFL.txt')
  if (Test-Path -LiteralPath (Join-Path $Stage 'openmouse-app-icon.png')) { $Names = @('openmouse-app-icon.png') + $Names }
  foreach ($Name in $Names) {
    $Source = Get-Item -LiteralPath (Join-Path $Stage $Name) -Force
    if (($Name -eq 'native-hid') -ne $Source.PSIsContainer) { throw 'Invalid update package type' }
    if (Test-Path -LiteralPath (Join-Path $Destination $Name)) { Assert-CleanTree (Join-Path $Destination $Name) }
  }
  $Lock = Join-Path $Destination '.openmouse-update.lock'
  New-Item -ItemType Directory -Path $Lock -ErrorAction Stop | Out-Null
  $Transaction = Join-Path $Destination ('.openmouse-update.' + [guid]::NewGuid().ToString('N'))
  $Backed = @()
  $Installed = @()
  $RollbackFailed = $false
  try {
    New-Item -ItemType Directory -Path $Transaction -ErrorAction Stop | Out-Null
    $New = Join-Path $Transaction 'new'
    $Old = Join-Path $Transaction 'old'
    New-Item -ItemType Directory -Path $New, $Old -ErrorAction Stop | Out-Null
    foreach ($Name in $Names) {
      Copy-Item -LiteralPath (Join-Path $Stage $Name) -Destination (Join-Path $New $Name) -Recurse -ErrorAction Stop
    }
    foreach ($Name in $Names) {
      if (Test-Path -LiteralPath (Join-Path $Destination $Name)) {
        Move-Item -LiteralPath (Join-Path $Destination $Name) -Destination (Join-Path $Old $Name) -ErrorAction Stop
        $Backed += $Name
      }
    }
    foreach ($Name in $Names) {
      $Installed += $Name
      Move-Item -LiteralPath (Join-Path $New $Name) -Destination (Join-Path $Destination $Name) -ErrorAction Stop
    }
  } catch {
    $Failure = $_
    foreach ($Name in $Installed) {
      try {
        $Target = Join-Path $Destination $Name
        if (Test-Path -LiteralPath $Target) { Remove-Item -LiteralPath $Target -Recurse -Force -ErrorAction Stop }
      } catch { $RollbackFailed = $true }
    }
    foreach ($Name in $Backed) {
      try {
        $Target = Join-Path $Destination $Name
        if (Test-Path -LiteralPath $Target) { throw 'Cannot safely restore update backup over an existing target' }
        Move-Item -LiteralPath (Join-Path $Old $Name) -Destination $Target -ErrorAction Stop
      }
      catch { $RollbackFailed = $true }
    }
    if ($RollbackFailed) { throw "Update rollback failed; backups retained at $Transaction" }
    Write-Warning $Failure
    return $false
  } finally {
    if (-not $RollbackFailed) {
      Remove-Item -LiteralPath $Transaction -Recurse -Force -ErrorAction SilentlyContinue
      Remove-Item -LiteralPath $Lock -Force -ErrorAction SilentlyContinue
    }
  }
  return $true
}
"#;

#[cfg(windows)]
const WINDOWS_COMPLETION: &str = r#"function Complete-BridgeUpdate([string]$Stage, [string]$Destination, [string]$Binary, [scriptblock]$Launch) {
  $Updated = Install-BridgePackage $Stage $Destination $Binary
  & $Launch $Destination $Binary
  return $Updated
}
"#;

#[cfg(target_os = "windows")]
fn spawn_install_helper(
    staging: &Path,
    destination: &Path,
    executable: &Path,
    _log: &UpdateLog,
) -> Result<()> {
    let script = staging.join("install-update.ps1");
    let contents = format!(
        "param([int]$BridgePid, [string]$Stage, [string]$Destination, [string]$Binary)\n$ErrorActionPreference = 'Stop'\n{WINDOWS_TRANSACTION}\n{WINDOWS_COMPLETION}\ntry {{\nwhile (Get-Process -Id $BridgePid -ErrorAction SilentlyContinue) {{ Start-Sleep -Seconds 1 }}\nComplete-BridgeUpdate $Stage $Destination $Binary {{ param($Directory, $Name) Start-Process -FilePath (Join-Path $Directory $Name) -WorkingDirectory $Directory }}\n}} finally {{ Remove-Item -LiteralPath $Stage -Recurse -Force -ErrorAction SilentlyContinue }}\n"
    );
    write_helper(&script, &contents)?;
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-BridgePid")
        .arg(std::process::id().to_string())
        .arg("-Stage")
        .arg(staging)
        .arg("-Destination")
        .arg(destination)
        .arg("-Binary")
        .arg(
            executable
                .file_name()
                .context("Bridge executable has no file name")?,
        );
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .spawn()
        .context("could not launch the update helper")?;
    Ok(())
}

/// Runs the new installer silently over this install once Bridge has exited,
/// then starts Bridge again. Inno Setup rolls its own changes back when it
/// fails, so a non-zero exit leaves the current version in place.
#[cfg(windows)]
const WINDOWS_INSTALLER_HELPER: &str = r#"param([int]$BridgePid, [string]$Stage, [string]$Installer, [string]$Destination, [string]$Executable, [string]$Log, [string]$Failure)
$ErrorActionPreference = 'Stop'
function Note([string]$Text) {
  $Stamp = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
  Add-Content -LiteralPath $Log -Value "$Stamp $Text" -ErrorAction SilentlyContinue
}
function Fail([string]$Text) {
  Note $Text
  Set-Content -LiteralPath $Failure -Value $Text -ErrorAction SilentlyContinue
}
try {
  while (Get-Process -Id $BridgePid -ErrorAction SilentlyContinue) { Start-Sleep -Seconds 1 }
  Note "Running the installer into $Destination"
  $InstallerLog = Join-Path (Split-Path -Parent $Log) 'update-installer.log'
  $Arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CLOSEAPPLICATIONS', "/DIR=`"$Destination`"", "/LOG=`"$InstallerLog`"")
  $Process = Start-Process -FilePath $Installer -ArgumentList $Arguments -Wait -PassThru
  if ($Process.ExitCode -eq 0) {
    Note 'Done'
  } else {
    Fail "The installer stopped with code $($Process.ExitCode); Bridge kept the current version"
  }
} catch {
  Fail "The update could not be installed: $($_.Exception.Message)"
} finally {
  Start-Process -FilePath $Executable -WorkingDirectory $Destination -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath $Stage -Recurse -Force -ErrorAction SilentlyContinue
}
"#;

#[cfg(windows)]
fn spawn_installer_helper(
    staging: &Path,
    installer: &Path,
    destination: &Path,
    executable: &Path,
    log: &UpdateLog,
) -> Result<()> {
    let script = staging.join("install-update.ps1");
    write_helper(&script, WINDOWS_INSTALLER_HELPER)?;
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-BridgePid")
        .arg(std::process::id().to_string())
        .arg("-Stage")
        .arg(staging)
        .arg("-Installer")
        .arg(installer)
        .arg("-Destination")
        .arg(destination)
        .arg("-Executable")
        .arg(executable)
        .arg("-Log")
        .arg(&log.log)
        .arg("-Failure")
        .arg(&log.failure);
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .spawn()
        .context("could not launch the update helper")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn spawn_install_helper(
    _staging: &Path,
    _destination: &Path,
    _executable: &Path,
    _log: &UpdateLog,
) -> Result<()> {
    anyhow::bail!("automatic Bridge updates are available only on macOS, Windows, and Linux")
}

#[cfg(test)]
mod tests;
