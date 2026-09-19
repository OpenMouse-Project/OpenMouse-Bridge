#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Command;
use std::{
    env,
    fs::{self, File},
    io::{self, Cursor},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
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

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    archive_url: String,
    checksum_url: String,
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

pub async fn check_for_update() -> Result<Option<UpdateInfo>> {
    let url = env::var(UPDATE_URL_ENV).unwrap_or_else(|_| LATEST_RELEASE_URL.to_owned());
    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .context("could not create the update client")?;
    let release = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("could not check {url}"))?
        .error_for_status()
        .with_context(|| format!("update check failed for {url}"))?
        .json::<Release>()
        .await
        .context("could not parse the latest Bridge release")?;
    select_update(release, crate::BRIDGE_VERSION)
}

pub async fn download_and_stage(update: &UpdateInfo) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("could not create the update download client")?;
    let (archive, checksum) = tokio::try_join!(
        download(&client, &update.archive_url),
        download(&client, &update.checksum_url),
    )?;
    verify_checksum(&archive, &checksum)?;
    let version = update.version.clone();
    tokio::task::spawn_blocking(move || stage_archive(&archive, &version))
        .await
        .context("update staging task stopped unexpectedly")??;
    Ok(())
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    Ok(client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("could not download {url}"))?
        .error_for_status()
        .with_context(|| format!("update download failed for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("could not read update download from {url}"))?
        .to_vec())
}

fn select_update(release: Release, current: &str) -> Result<Option<UpdateInfo>> {
    let current = Version::parse(current).context("Bridge has an invalid current version")?;
    let version_text = release.tag_name.trim_start_matches('v');
    let available = Version::parse(version_text)
        .with_context(|| format!("release {} has an invalid version", release.tag_name))?;
    if available <= current {
        return Ok(None);
    }

    let archive_name = platform_archive_name()?;
    let checksum_name = format!("{archive_name}.sha256");
    let archive_url = asset_url(&release.assets, archive_name)?;
    let checksum_url = asset_url(&release.assets, &checksum_name)?;
    Ok(Some(UpdateInfo {
        version: available.to_string(),
        archive_url,
        checksum_url,
    }))
}

fn asset_url(assets: &[ReleaseAsset], name: &str) -> Result<String> {
    assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| anyhow!("release does not contain {name}"))
}

fn platform_archive_name() -> Result<&'static str> {
    #[cfg(target_os = "macos")]
    return Ok("openmouse-bridge-macos-universal.zip");
    #[cfg(target_os = "windows")]
    return Ok("openmouse-bridge-windows-x64.zip");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    bail!("automatic Bridge updates are available only on macOS and Windows");
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

fn stage_archive(archive: &[u8], version: &str) -> Result<()> {
    let staging = env::temp_dir().join(format!(
        "openmouse-bridge-update-{version}-{}",
        std::process::id()
    ));
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("could not clear {}", staging.display()))?;
    }
    fs::create_dir_all(&staging)
        .with_context(|| format!("could not create {}", staging.display()))?;
    extract_archive(archive, &staging)?;

    let current_executable =
        env::current_exe().context("could not locate the Bridge executable")?;
    let install_directory = current_executable
        .parent()
        .context("Bridge executable has no parent directory")?;
    verify_install_directory(install_directory)?;
    spawn_install_helper(&staging, install_directory, &current_executable)
}

fn extract_archive(archive: &[u8], staging: &Path) -> Result<()> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(archive)).context("update archive is invalid")?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .context("could not read update archive entry")?;
        let Some(relative) = entry.enclosed_name() else {
            bail!("update archive contains an unsafe path");
        };
        let destination = staging.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&destination)?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&destination)
            .with_context(|| format!("could not create {}", destination.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("could not extract {}", destination.display()))?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(mode))?;
        }
    }
    ensure!(
        staging.join(platform_binary_name()).is_file(),
        "update archive does not contain the Bridge executable"
    );
    Ok(())
}

fn verify_install_directory(directory: &Path) -> Result<()> {
    let probe = directory.join(format!(
        ".openmouse-update-write-test-{}",
        std::process::id()
    ));
    File::create(&probe)
        .with_context(|| format!("Bridge cannot update files in {}", directory.display()))?;
    fs::remove_file(probe).context("could not remove the update write test")
}

fn platform_binary_name() -> &'static str {
    #[cfg(target_os = "windows")]
    return "openmouse-bridge.exe";
    #[cfg(not(target_os = "windows"))]
    return "openmouse-bridge";
}

#[cfg(target_os = "macos")]
fn spawn_install_helper(staging: &Path, destination: &Path, executable: &Path) -> Result<()> {
    const SCRIPT: &str = r#"#!/bin/sh
pid="$1"
stage="$2"
destination="$3"
binary="$4"
while kill -0 "$pid" 2>/dev/null; do sleep 1; done
cp -f "$stage/$binary" "$destination/$binary.update" || exit 1
chmod +x "$destination/$binary.update" || exit 1
mv -f "$destination/$binary.update" "$destination/$binary" || exit 1
if [ -d "$stage/native-hid" ]; then
  rm -rf "$destination/native-hid.update"
  cp -R "$stage/native-hid" "$destination/native-hid.update" || exit 1
  rm -rf "$destination/native-hid.old"
  if [ -d "$destination/native-hid" ]; then mv "$destination/native-hid" "$destination/native-hid.old"; fi
  mv "$destination/native-hid.update" "$destination/native-hid" || exit 1
  rm -rf "$destination/native-hid.old"
fi
cd "$destination" || exit 1
nohup "$destination/$binary" >/dev/null 2>&1 &
rm -rf "$stage"
"#;
    let script = staging.join("install-update.sh");
    fs::write(&script, SCRIPT).context("could not write the update helper")?;
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
        .spawn()
        .context("could not launch the update helper")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn spawn_install_helper(staging: &Path, destination: &Path, executable: &Path) -> Result<()> {
    const SCRIPT: &str = r#"param([int]$BridgePid, [string]$Stage, [string]$Destination, [string]$Binary)
$ErrorActionPreference = "Stop"
while (Get-Process -Id $BridgePid -ErrorAction SilentlyContinue) { Start-Sleep -Seconds 1 }
Copy-Item -LiteralPath (Join-Path $Stage $Binary) -Destination (Join-Path $Destination "${Binary}.update") -Force
Move-Item -LiteralPath (Join-Path $Destination "${Binary}.update") -Destination (Join-Path $Destination $Binary) -Force
if (Test-Path -LiteralPath (Join-Path $Stage "native-hid")) {
  $Update = Join-Path $Destination "native-hid.update"
  $Old = Join-Path $Destination "native-hid.old"
  Remove-Item -LiteralPath $Update -Recurse -Force -ErrorAction SilentlyContinue
  Copy-Item -LiteralPath (Join-Path $Stage "native-hid") -Destination $Update -Recurse -Force
  Remove-Item -LiteralPath $Old -Recurse -Force -ErrorAction SilentlyContinue
  if (Test-Path -LiteralPath (Join-Path $Destination "native-hid")) { Move-Item -LiteralPath (Join-Path $Destination "native-hid") -Destination $Old -Force }
  Move-Item -LiteralPath $Update -Destination (Join-Path $Destination "native-hid") -Force
  Remove-Item -LiteralPath $Old -Recurse -Force -ErrorAction SilentlyContinue
}
Start-Process -FilePath (Join-Path $Destination $Binary) -WorkingDirectory $Destination
"#;
    let script = staging.join("install-update.ps1");
    fs::write(&script, SCRIPT).context("could not write the update helper")?;
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
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

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn spawn_install_helper(_staging: &Path, _destination: &Path, _executable: &Path) -> Result<()> {
    bail!("automatic Bridge updates are available only on macOS and Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str) -> Release {
        Release {
            tag_name: format!("v{version}"),
            assets: vec![
                ReleaseAsset {
                    name: platform_archive_name().unwrap_or("unsupported.zip").into(),
                    browser_download_url: "https://example.test/bridge.zip".into(),
                },
                ReleaseAsset {
                    name: format!(
                        "{}.sha256",
                        platform_archive_name().unwrap_or("unsupported.zip")
                    ),
                    browser_download_url: "https://example.test/bridge.zip.sha256".into(),
                },
            ],
        }
    }

    #[test]
    fn newer_stable_release_is_selected() {
        if platform_archive_name().is_err() {
            return;
        }
        let update = select_update(release("2.0.0"), "1.9.0")
            .expect("release should parse")
            .expect("new release should be selected");
        assert_eq!(update.version, "2.0.0");
    }

    #[test]
    fn current_or_older_release_is_ignored() {
        assert!(select_update(release("1.3.2"), "1.3.2").unwrap().is_none());
        assert!(select_update(release("1.2.9"), "1.3.2").unwrap().is_none());
    }

    #[test]
    fn checksum_must_match_archive() {
        let archive = b"bridge";
        let checksum = format!("{:x}  bridge.zip\n", Sha256::digest(archive));
        verify_checksum(archive, checksum.as_bytes()).expect("checksum should match");
        assert!(
            verify_checksum(
                archive,
                b"0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
    }
}
