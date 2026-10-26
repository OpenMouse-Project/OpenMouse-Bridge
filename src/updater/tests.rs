use super::*;

fn fixture() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = env::temp_dir();
    #[cfg(unix)]
    let parent = fs::canonicalize(parent).unwrap();
    let root = parent.join(format!(
        "bridge-archive-test-{}-{}-literal $ O'Connor [x]",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    root
}

fn directory_link(link: &Path, target: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let output = Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn remove_directory_link(link: &Path) {
    #[cfg(unix)]
    fs::remove_file(link).unwrap();
    #[cfg(windows)]
    fs::remove_dir(link).unwrap();
}

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn install_preflight_rejects_existing_lock_without_mutation() {
    for kind in ["file", "directory", "link"] {
        let root = fixture();
        let lock = root.join(".openmouse-update.lock");
        fs::write(root.join("sentinel"), b"untouched").unwrap();
        match kind {
            "file" => fs::write(&lock, b"other-transaction").unwrap(),
            "directory" => fs::create_dir(&lock).unwrap(),
            _ => directory_link(&lock, &root.join("missing")),
        }
        let result = verify_install_directory(&root);
        assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        if kind == "link" {
            remove_directory_link(&lock);
        }
        fs::remove_dir_all(root).unwrap();
        assert!(result.is_err(), "accepted existing {kind} lock");
    }
    let root = fixture();
    verify_install_directory(&root).unwrap();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn install_preflight_rejects_fifo_lock() {
    let root = fixture();
    let lock = root.join(".openmouse-update.lock");
    assert!(
        Command::new("mkfifo")
            .arg(&lock)
            .status()
            .unwrap()
            .success()
    );
    let result = verify_install_directory(&root);
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    fs::remove_dir_all(root).unwrap();
    assert!(result.is_err());
}

#[test]
fn extraction_never_overwrites_existing_files() {
    let root = fixture();
    let executable = root.join(platform_binary_name());
    fs::write(&executable, b"original fixture").unwrap();
    let bytes = archive(&[(platform_binary_name(), b"replacement")]);
    assert!(extract_archive(&bytes, &root).is_err());
    assert_eq!(fs::read(executable).unwrap(), b"original fixture");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unsafe_archive_types_and_portable_paths_are_rejected() {
    for name in [
        "../escape",
        "/absolute",
        "C:/escape",
        "dir\\escape",
        "file:stream",
        "CON",
        "trailing.",
    ] {
        let root = fixture();
        let bytes = archive(&[(name, b"bad"), (platform_binary_name(), b"bridge")]);
        assert!(extract_archive(&bytes, &root).is_err(), "accepted {name}");
        fs::remove_dir_all(root).unwrap();
    }
    for kind in [0o120777u32, 0o010644, 0o020644, 0o060644, 0o140644] {
        let root = fixture();
        let mut bytes = archive(&[(platform_binary_name(), b"bridge")]);
        let central = bytes.windows(4).position(|p| p == b"PK\x01\x02").unwrap();
        bytes[central + 5] = 3;
        bytes[central + 38..central + 42].copy_from_slice(&(kind << 16).to_le_bytes());
        assert!(
            extract_archive(&bytes, &root).is_err(),
            "accepted type {kind:o}"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn extraction_does_not_follow_existing_directory_links() {
    let root = fixture();
    let outside = fixture();
    fs::write(outside.join("sentinel"), b"untouched").unwrap();
    let link = root.join("linked");
    directory_link(&link, &outside);
    let bytes = archive(&[
        ("linked/escape", b"bad"),
        (platform_binary_name(), b"bridge"),
    ]);
    assert!(extract_archive(&bytes, &root).is_err());
    assert!(!outside.join("escape").exists());
    assert!(extract_archive(&bytes, &link).is_err());
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
    remove_directory_link(&link);
    let executable = root.join(platform_binary_name());
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("sentinel"), &executable).unwrap();
    #[cfg(windows)]
    fs::hard_link(outside.join("sentinel"), &executable).unwrap();
    let bytes = archive(&[(platform_binary_name(), b"replacement")]);
    assert!(extract_archive(&bytes, &root).is_err());
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[cfg(unix)]
#[test]
fn extraction_masks_all_special_permission_bits() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture();
    let mut bytes = archive(&[(platform_binary_name(), b"bridge")]);
    let central = bytes.windows(4).position(|p| p == b"PK\x01\x02").unwrap();
    bytes[central + 5] = 3;
    bytes[central + 38..central + 42].copy_from_slice(&(0o107755u32 << 16).to_le_bytes());
    extract_archive(&bytes, &root).unwrap();
    assert_eq!(
        fs::metadata(root.join(platform_binary_name()))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn downloads_are_bounded_without_network_or_content_length() {
    let response = reqwest::Response::from(axum::http::Response::new(vec![b'x'; 17]));
    assert!(read_bounded_response(response, 16).await.is_err());
    let response = reqwest::Response::from(
        axum::http::Response::builder()
            .header("content-length", "1")
            .body(vec![b'x'; 17])
            .unwrap(),
    );
    assert!(read_bounded_response(response, 16).await.is_err());
    let response = reqwest::Response::from(axum::http::Response::new(vec![b'x'; 16]));
    assert_eq!(
        read_bounded_response(response, 16).await.unwrap(),
        vec![b'x'; 16]
    );
    let response = reqwest::Response::from(
        axum::http::Response::builder()
            .header("content-length", "9999999999")
            .body(Vec::<u8>::new())
            .unwrap(),
    );
    assert!(
        read_bounded_response(response, MAX_CHECKSUM_BYTES)
            .await
            .is_err()
    );
}

#[test]
fn archive_entry_and_decompression_limits_preserve_existing_fixtures() {
    let bytes = archive(&[(platform_binary_name(), b"12345"), ("other", b"12345")]);
    for (entries, file, total) in [(1, 5, 10), (2, 4, 10), (2, 5, 9)] {
        let root = fixture();
        fs::write(root.join("sentinel"), b"untouched").unwrap();
        assert!(extract_archive_with_limits(&bytes, &root, entries, file, total).is_err());
        assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
    let root = fixture();
    extract_archive_with_limits(&bytes, &root, 2, 5, 10).unwrap();
    assert_eq!(fs::read(root.join("other")).unwrap(), b"12345");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn private_staging_is_unique_and_cleans_failed_preparation() {
    let parent = fixture();
    let path;
    {
        let first = StagingDirectory::new_in(&parent).unwrap();
        let second = StagingDirectory::new_in(&parent).unwrap();
        assert_ne!(first.path, second.path);
        path = first.path.clone();
        let bytes = archive(&[(platform_binary_name(), b"bridge")]);
        extract_archive(&bytes, &first.path).unwrap();
        assert!(verify_package(&first.path).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
    assert!(!path.exists());
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
    fs::remove_dir_all(parent).unwrap();
}

#[test]
fn zip_metadata_reader_stops_before_unbounded_directory_work() {
    let bytes = archive(&[(platform_binary_name(), b"bridge")]);
    let budget = std::cell::Cell::new(Some(8));
    let reader = ArchiveReader {
        inner: Cursor::new(bytes.as_slice()),
        metadata_budget: &budget,
    };
    assert!(zip::ZipArchive::new(reader).is_err());
    assert_eq!(budget.get(), Some(0));
    let budget = std::cell::Cell::new(Some(MAX_ZIP_METADATA_READ_BYTES));
    let reader = ArchiveReader {
        inner: Cursor::new(bytes.as_slice()),
        metadata_budget: &budget,
    };
    assert_eq!(zip::ZipArchive::new(reader).unwrap().len(), 1);
}

#[test]
fn decompression_limit_does_not_trust_declared_entry_size() {
    let root = fixture();
    fs::write(root.join("sentinel"), b"untouched").unwrap();
    let mut bytes = archive(&[(platform_binary_name(), &[b'A'; 1024])]);
    let central = bytes.windows(4).position(|p| p == b"PK\x01\x02").unwrap();
    bytes[central + 24..central + 28].copy_from_slice(&1u32.to_le_bytes());
    bytes[22..26].copy_from_slice(&1u32.to_le_bytes());
    assert!(extract_archive_with_limits(&bytes, &root, 1, 3, 3).is_err());
    assert!(
        fs::metadata(root.join(platform_binary_name())).is_ok_and(|metadata| metadata.len() <= 3)
    );
    assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"untouched");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn generated_unix_transaction_commits_or_restores_fixture_package() {
    for case in [
        "success",
        "lock-creation-failure",
        "transaction-creation-failure",
        "existing-directory-lock",
        "existing-file-lock",
        "existing-link-lock",
        "copy-failure",
        "commit-failure",
        "rollback-failure",
    ] {
        let root = fixture();
        let stage = root.join("stage");
        let destination = root.join("install");
        for directory in [&stage, &destination] {
            fs::create_dir(directory).unwrap();
            fs::create_dir(directory.join("native-hid")).unwrap();
        }
        fs::write(root.join("sandbox-marker"), b"bridge-update-fixture-only").unwrap();
        fs::write(stage.join("openmouse-bridge"), b"new-binary").unwrap();
        fs::write(stage.join("native-hid/helper"), b"new-helper").unwrap();
        fs::write(destination.join("openmouse-bridge"), b"old-binary").unwrap();
        fs::write(destination.join("native-hid/helper"), b"old-helper").unwrap();
        fs::write(destination.join("sentinel"), b"untouched").unwrap();
        fs::write(
            stage.join("DM-Sans-OFL.txt"),
            include_bytes!("../../assets/fonts/DM-Sans-OFL.txt"),
        )
        .unwrap();
        fs::write(destination.join("DM-Sans-OFL.txt"), b"old-license").unwrap();
        let lock = destination.join(".openmouse-update.lock");
        match case {
            "existing-directory-lock" => fs::create_dir(&lock).unwrap(),
            "existing-file-lock" => fs::write(&lock, b"other-transaction").unwrap(),
            "existing-link-lock" => directory_link(&lock, &root.join("missing")),
            _ => {}
        }
        let script = root.join("sandbox.sh");
        let contents = format!(
            "{UNIX_TRANSACTION}\n{UNIX_COMPLETION}\n{}",
            r#"
root="$1"
fixture_case="$2"
[ "$(cat "$root/sandbox-marker")" = bridge-update-fixture-only ] || exit 90
mkdir() {
  if [ "$fixture_case" = lock-creation-failure ] && [ "$1" = "$root/install/.openmouse-update.lock" ]; then return 1; fi
  command mkdir "$@"
}
mktemp() {
  if [ "$fixture_case" = transaction-creation-failure ]; then return 1; fi
  command mktemp "$@"
}
cp() {
  if [ "$fixture_case" = copy-failure ] && [ "$2" = "$root/stage/native-hid" ]; then return 1; fi
  command cp "$@"
}
mv() {
  case "$1" in */new/openmouse-bridge) if [ "$fixture_case" = commit-failure ] || [ "$fixture_case" = rollback-failure ]; then return 1; fi ;; esac
  case "$1" in */old/openmouse-bridge) if [ "$fixture_case" = rollback-failure ]; then return 1; fi ;; esac
  command mv "$@"
}
record_launch() { cat "$1/$2" > "$root/launch-record"; }
case "$fixture_case" in
  lock-creation-failure|transaction-creation-failure|existing-*-lock)
    if install_package "$root/stage" "$root/install" openmouse-bridge; then exit 91; else status=$?; fi
    [ "$status" -eq 2 ] || { printf 'Expected safe failure status 2, got %s\n' "$status" >&2; exit 92; }
    ;;
esac
complete_update "$root/stage" "$root/install" openmouse-bridge record_launch
"#
        );
        assert!(!contents.contains("nohup") && !contents.contains("kill -0"));
        write_helper(&script, &contents).unwrap();
        let root_argument = root.to_string_lossy().replace('\\', "/");
        #[cfg(windows)]
        let root_argument = format!(
            "/{}{}",
            root_argument[..1].to_ascii_lowercase(),
            &root_argument[2..]
        );
        let output = Command::new(if cfg!(windows) { "sh" } else { "/bin/sh" })
            .arg(&script)
            .arg(&root_argument)
            .arg(case)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            case != "rollback-failure",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            root.join("launch-record").exists(),
            case != "rollback-failure",
            "{case}: relaunch eligibility"
        );
        if case == "rollback-failure" {
            assert!(destination.join(".openmouse-update.lock").exists());
            fs::remove_dir_all(root).unwrap();
            continue;
        }
        let new = case == "success";
        assert_eq!(
            fs::read(root.join("launch-record")).unwrap(),
            if new { b"new-binary" } else { b"old-binary" }
        );
        assert_eq!(
            fs::read(destination.join("openmouse-bridge")).unwrap(),
            if new { b"new-binary" } else { b"old-binary" }
        );
        assert_eq!(
            fs::read(destination.join("DM-Sans-OFL.txt"))
                .unwrap()
                .as_slice(),
            if new {
                include_bytes!("../../assets/fonts/DM-Sans-OFL.txt").as_slice()
            } else {
                b"old-license"
            }
        );
        assert_eq!(
            fs::read(destination.join("native-hid/helper")).unwrap(),
            if new { b"new-helper" } else { b"old-helper" }
        );
        assert_eq!(
            fs::read(destination.join("sentinel")).unwrap(),
            b"untouched"
        );
        assert_eq!(
            fs::read_dir(&destination).unwrap().count(),
            if case.starts_with("existing-") { 5 } else { 4 }
        );
        if case == "existing-file-lock" {
            assert_eq!(fs::read(&lock).unwrap(), b"other-transaction");
        }
        if case == "existing-link-lock" {
            remove_directory_link(&lock);
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn generated_windows_transaction_commits_or_restores_fixture_package() {
    use std::os::windows::fs::OpenOptionsExt;
    for case in [
        "success",
        "copy-failure",
        "locked-binary",
        "commit-failure",
        "existing-lock",
        "rollback-failure",
    ] {
        let root = fixture();
        let stage = root.join("stage");
        let destination = root.join("install");
        for directory in [&stage, &destination] {
            fs::create_dir(directory).unwrap();
            fs::create_dir(directory.join("native-hid")).unwrap();
        }
        fs::write(root.join("sandbox-marker"), b"bridge-update-fixture-only").unwrap();
        fs::write(stage.join(platform_binary_name()), b"new-binary").unwrap();
        fs::write(stage.join("native-hid/helper"), b"new-helper").unwrap();
        fs::write(stage.join("openmouse-app-icon.png"), b"new-icon").unwrap();
        fs::write(destination.join(platform_binary_name()), b"old-binary").unwrap();
        fs::write(destination.join("native-hid/helper"), b"old-helper").unwrap();
        fs::write(destination.join("openmouse-app-icon.png"), b"old-icon").unwrap();
        fs::write(destination.join("sentinel"), b"untouched").unwrap();
        fs::write(
            stage.join("DM-Sans-OFL.txt"),
            include_bytes!("../../assets/fonts/DM-Sans-OFL.txt"),
        )
        .unwrap();
        fs::write(destination.join("DM-Sans-OFL.txt"), b"old-license").unwrap();
        if case == "existing-lock" {
            fs::create_dir(destination.join(".openmouse-update.lock")).unwrap();
            fs::write(
                destination.join(".openmouse-update.lock/owner"),
                b"other-transaction",
            )
            .unwrap();
        }
        let lock = if case == "locked-binary" {
            Some(
                fs::OpenOptions::new()
                    .read(true)
                    .share_mode(1)
                    .open(destination.join(platform_binary_name()))
                    .unwrap(),
            )
        } else {
            None
        };
        let script = root.join("sandbox.ps1");
        let contents = format!("param([string]$Root, [string]$Case)\n$ErrorActionPreference = 'Stop'\n{WINDOWS_TRANSACTION}\n{WINDOWS_COMPLETION}\n{}
", r#"
if (-not [IO.Path]::IsPathRooted($Root) -or [IO.File]::ReadAllText((Join-Path $Root 'sandbox-marker')) -ne 'bridge-update-fixture-only') { throw 'Not a fixture' }
$Stage = Join-Path $Root 'stage'
$Destination = Join-Path $Root 'install'
$Binary = 'openmouse-bridge.exe'
function Copy-Item([string]$LiteralPath, [string]$Destination, [switch]$Recurse, [string]$ErrorAction) {
  if ($Case -eq 'copy-failure' -and $LiteralPath.EndsWith('native-hid')) { throw 'Injected fixture copy failure' }
  Microsoft.PowerShell.Management\Copy-Item @PSBoundParameters
}
function Move-Item([string]$LiteralPath, [string]$Destination, [string]$ErrorAction) {
  if (($Case -eq 'commit-failure' -or $Case -eq 'rollback-failure') -and $LiteralPath.EndsWith(('new\' + $Binary))) { throw 'Injected fixture commit failure' }
  if ($Case -eq 'rollback-failure' -and $LiteralPath.EndsWith(('old\' + $Binary))) { throw 'Injected fixture rollback failure' }
  Microsoft.PowerShell.Management\Move-Item @PSBoundParameters
}
$Failed = $false
try { $Updated = Complete-BridgeUpdate $Stage $Destination $Binary { param($Directory, $Name) [IO.File]::WriteAllText((Join-Path $Root 'launch-record'), [IO.File]::ReadAllText((Join-Path $Directory $Name))) }; $Failed = -not $Updated } catch { $Failed = $true }
if (($Case -eq 'success') -eq $Failed) { throw 'Unexpected fixture transaction result' }
"#);
        assert!(!contents.contains("Start-Process") && !contents.contains("Get-Process"));
        write_helper(&script, &contents).unwrap();
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .arg(&root)
            .arg(case)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        drop(lock);
        let new = case == "success";
        assert_eq!(
            root.join("launch-record").exists(),
            !matches!(case, "existing-lock" | "rollback-failure"),
            "{case}: relaunch eligibility"
        );
        if case == "rollback-failure" {
            assert!(fs::read_dir(&destination).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".openmouse-update.")
            }));
            fs::remove_dir_all(root).unwrap();
            continue;
        }
        if case != "existing-lock" {
            assert_eq!(
                fs::read(root.join("launch-record")).unwrap(),
                if new { b"new-binary" } else { b"old-binary" }
            );
        }
        assert_eq!(
            fs::read(destination.join(platform_binary_name())).unwrap(),
            if new { b"new-binary" } else { b"old-binary" }
        );
        assert_eq!(
            fs::read(destination.join("DM-Sans-OFL.txt"))
                .unwrap()
                .as_slice(),
            if new {
                include_bytes!("../../assets/fonts/DM-Sans-OFL.txt").as_slice()
            } else {
                b"old-license"
            }
        );
        assert_eq!(
            fs::read(destination.join("native-hid/helper")).unwrap(),
            if new { b"new-helper" } else { b"old-helper" }
        );
        assert_eq!(
            fs::read(destination.join("openmouse-app-icon.png")).unwrap(),
            if new { b"new-icon" } else { b"old-icon" }
        );
        assert_eq!(
            fs::read(destination.join("sentinel")).unwrap(),
            b"untouched"
        );
        assert_eq!(
            fs::read_dir(&destination).unwrap().count(),
            if case == "existing-lock" { 6 } else { 5 }
        );
        if case == "existing-lock" {
            assert_eq!(
                fs::read(destination.join(".openmouse-update.lock/owner")).unwrap(),
                b"other-transaction"
            );
        }
        assert_eq!(
            fs::read(stage.join(platform_binary_name())).unwrap(),
            b"new-binary"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn helper_write_is_exclusive_and_package_preflight_is_complete() {
    let root = fixture();
    let script = root.join("install-update.ps1");
    fs::write(&script, b"fixture").unwrap();
    assert!(write_helper(&script, "replacement").is_err());
    assert_eq!(fs::read(&script).unwrap(), b"fixture");
    let node = if cfg!(windows) {
        "native-hid/node/node.exe"
    } else {
        "native-hid/node/bin/node"
    };
    let names = [
        platform_binary_name(),
        "native-hid/src/apply.mjs",
        "native-hid/src/brands.mjs",
        "native-hid/src/hid-device-adapter.mjs",
        "native-hid/src/attack-shark-x11.mjs",
        "native-hid/package.json",
        node,
        "native-hid/node_modules/node-hid/package.json",
        "native-hid/node_modules/@openmouse/protocol/package.json",
        "native-hid/node_modules/@openmouse/keyboard-protocol/package.json",
    ];
    for name in names {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"inert-fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    assert!(verify_package(&root).is_err(), "font license is mandatory");
    fs::write(
        root.join("DM-Sans-OFL.txt"),
        include_bytes!("../../assets/fonts/DM-Sans-OFL.txt"),
    )
    .unwrap();
    verify_package(&root).unwrap();
    fs::write(root.join("DM-Sans-OFL.txt"), b"truncated notice").unwrap();
    assert!(
        verify_package(&root).is_err(),
        "font license must be complete"
    );
    fs::write(
        root.join("DM-Sans-OFL.txt"),
        include_bytes!("../../assets/fonts/DM-Sans-OFL.txt"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in [platform_binary_name(), node] {
            let path = root.join(name);
            for mode in [0o641, 0o111, 0o400, 0o600, 0o050, 0o005] {
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                let result = verify_package(&root);
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                assert!(result.is_err(), "accepted {name} mode {mode:o}");
            }
        }
    }
    for name in names {
        fs::write(root.join(name), []).unwrap();
        assert!(verify_package(&root).is_err(), "accepted empty {name}");
        fs::write(root.join(name), b"inert-fixture").unwrap();
    }
    fs::remove_dir_all(root).unwrap();
}

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
