use std::collections::HashSet;

use anyhow::Result;

use crate::config::LocalApplication;

#[cfg(any(target_os = "macos", test))]
#[path = "native_image.rs"]
mod native_image;

#[derive(Debug)]
pub struct InvalidInput(pub String);

impl std::fmt::Display for InvalidInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for InvalidInput {}

pub fn canonicalize(applications: &[LocalApplication]) -> Result<Vec<LocalApplication>> {
    canonicalize_against(applications, &[])
}

pub fn canonicalize_against(
    applications: &[LocalApplication],
    previous: &[LocalApplication],
) -> Result<Vec<LocalApplication>> {
    canonicalize_with_previous(applications, previous, resolve_bundle)
}

#[cfg(test)]
fn canonicalize_with(
    applications: &[LocalApplication],
    resolve: impl Fn(&str) -> Result<(String, Option<String>)>,
) -> Result<Vec<LocalApplication>> {
    canonicalize_with_previous(applications, &[], resolve)
}

fn canonicalize_with_previous(
    applications: &[LocalApplication],
    previous: &[LocalApplication],
    resolve: impl Fn(&str) -> Result<(String, Option<String>)>,
) -> Result<Vec<LocalApplication>> {
    if applications.len() > 256 {
        return invalid("at most 256 local applications are allowed");
    }
    let mut canonical = applications.to_vec();
    for application in &mut canonical {
        if application.path.ends_with(".app") {
            if !valid_leaf(&application.executable) {
                return invalid("invalid bundle executable filename");
            }
            application.executable = application
                .path
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .into();
        }
    }
    validate(&canonical)?;
    for application in &mut canonical {
        if application.path.ends_with(".app") {
            let (executable, identifier) = resolve(&application.path).map_err(|error| {
                InvalidInput(format!("could not resolve application bundle: {error}"))
            })?;
            if !executable.starts_with(&format!("{}/", application.path)) {
                return invalid("bundle executable must remain inside the selected bundle");
            }
            application.bundle_path = Some(application.path.clone());
            application.bundle_identifier = identifier;
            application.executable = executable.rsplit('/').next().unwrap_or_default().into();
            application.path = executable;
        } else if let Some(bundle) = &application.bundle_path {
            let unchanged = previous.iter().any(|old| {
                old.id == application.id
                    && old.path == application.path
                    && old.executable == application.executable
                    && old.bundle_path == application.bundle_path
                    && old.bundle_identifier == application.bundle_identifier
            });
            if !unchanged {
                let (executable, identifier) = resolve(bundle).map_err(|error| {
                    InvalidInput(format!("could not verify application bundle: {error}"))
                })?;
                if executable != application.path
                    || application
                        .bundle_identifier
                        .as_ref()
                        .is_some_and(|claimed| Some(claimed) != identifier.as_ref())
                {
                    return invalid("bundle metadata does not identify the selected executable");
                }
                application.bundle_identifier = identifier;
            }
        }
    }
    validate(&canonical)?;
    Ok(canonical)
}

#[cfg(not(target_os = "macos"))]
fn resolve_bundle(_path: &str) -> Result<(String, Option<String>)> {
    invalid(".app bundles can only be resolved on macOS; select the actual executable")
}

#[cfg(any(target_os = "macos", test))]
fn same_metadata(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    let same = left.is_file()
        && right.is_file()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        same && left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.mode() == right.mode()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        same
    }
}

#[cfg(any(target_os = "macos", test))]
fn check_unchanged(
    path: &std::path::Path,
    file: &std::fs::File,
    original: &std::fs::Metadata,
) -> Result<()> {
    anyhow::ensure!(
        same_metadata(original, &file.metadata()?)
            && same_metadata(original, &std::fs::symlink_metadata(path)?),
        "application file changed during validation"
    );
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn checked_open(path: &std::path::Path, limit: u64) -> Result<(std::fs::File, std::fs::Metadata)> {
    checked_open_with(path, limit, || Ok(()))
}

#[cfg(any(target_os = "macos", test))]
fn checked_open_with(
    path: &std::path::Path,
    limit: u64,
    after_metadata: impl FnOnce() -> Result<()>,
) -> Result<(std::fs::File, std::fs::Metadata)> {
    let before = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        before.is_file() && before.len() <= limit,
        "application file is not regular or exceeds its size limit"
    );
    after_metadata()?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x4 | 0x100);
    }
    let file = options.open(path)?;
    check_unchanged(path, &file, &before)?;
    Ok((file, before))
}

#[cfg(any(target_os = "macos", test))]
fn read_bundle_metadata(path: &std::path::Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let (mut file, metadata) = checked_open(path, 65536)?;
    let mut bytes = Vec::new();
    (&mut file).take(65537).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 65536 && bytes.len() as u64 == metadata.len(),
        "bundle metadata exceeded its size limit or changed"
    );
    check_unchanged(path, &file, &metadata)?;
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn resolve_bundle(path: &str) -> Result<(String, Option<String>)> {
    use anyhow::{Context, ensure};
    use objc2::{class, msg_send, rc::Retained, runtime::AnyObject};
    use objc2_foundation::{NSData, NSString};
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};
    objc2::rc::autoreleasepool(|_| {
        let root = fs::canonicalize(path).context("bundle is missing or inaccessible")?;
        ensure!(
            root == Path::new(path),
            "select the canonical bundle path, not a link"
        );
        ensure!(root.is_dir(), "selected bundle is not a directory");
        let info = root.join("Contents/Info.plist");
        ensure!(
            fs::canonicalize(&info)? == info,
            "bundle metadata must not be linked"
        );
        let bytes = read_bundle_metadata(&info)?;
        let data = NSData::with_bytes(&bytes);
        let dictionary: Option<Retained<AnyObject>> = unsafe {
            msg_send![class!(NSPropertyListSerialization),
                propertyListWithData: &*data,
                options: 0_usize,
                format: std::ptr::null_mut::<usize>(),
                error: std::ptr::null_mut::<*mut AnyObject>()]
        };
        let dictionary = dictionary.context("invalid application property list")?;
        let is_dictionary: bool =
            unsafe { msg_send![&*dictionary, isKindOfClass: class!(NSDictionary)] };
        ensure!(is_dictionary, "application metadata must be a dictionary");
        let string = |key: &str| -> Result<Option<String>> {
            let value: Option<Retained<AnyObject>> =
                unsafe { msg_send![&*dictionary, objectForKey: &*NSString::from_str(key)] };
            value
                .map(|value| {
                    value
                        .downcast::<NSString>()
                        .map(|value| value.to_string())
                        .map_err(|_| anyhow::anyhow!("bundle metadata {key} must be a string"))
                })
                .transpose()
        };
        ensure!(
            string("CFBundlePackageType")?.as_deref() == Some("APPL"),
            "bundle is not a native application"
        );
        let leaf = string("CFBundleExecutable")?.context("bundle has no executable")?;
        ensure!(valid_leaf(&leaf), "invalid bundle executable filename");
        let executable_selection = root.join("Contents/MacOS").join(leaf);
        let executable = fs::canonicalize(&executable_selection)?;
        ensure!(
            executable.starts_with(&root),
            "bundle executable escapes the bundle"
        );
        let (mut image, metadata) = checked_open(&executable, u64::MAX)?;
        ensure!(
            metadata.permissions().mode() & 0o500 == 0o500,
            "bundle executable requires owner read and execute access"
        );
        ensure!(
            native_image::validate(&mut image, metadata.len()),
            "bundle executable is not a supported native binary"
        );
        check_unchanged(&executable, &image, &metadata)?;
        ensure!(
            read_bundle_metadata(&info)? == bytes,
            "bundle metadata changed during resolution"
        );
        ensure!(
            fs::canonicalize(path)? == root
                && fs::canonicalize(&info)? == info
                && fs::canonicalize(&executable_selection)? == executable,
            "application bundle changed during resolution"
        );
        let identifier = string("CFBundleIdentifier")?;
        Ok((
            executable
                .to_str()
                .context("executable path is not Unicode")?
                .into(),
            identifier,
        ))
    })
}

pub fn validate(applications: &[LocalApplication]) -> Result<()> {
    if applications.len() > 256 {
        return invalid("at most 256 local applications are allowed");
    }
    let mut ids = HashSet::new();
    for application in applications {
        let suffix = application.id.strip_prefix("local:").unwrap_or_default();
        if suffix.is_empty()
            || suffix.len() > 128
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return invalid(
                "local application ID must have a local: prefix and a 1–128 character ASCII suffix",
            );
        }
        if !ids.insert(&application.id) {
            return invalid("local application IDs must be unique");
        }
        if application.name.trim().is_empty()
            || application.name.chars().count() > 80
            || application.name.chars().any(char::is_control)
        {
            return invalid("local application name must contain 1–80 characters without controls");
        }
        if application.bundle_path.as_ref().is_some_and(|value| {
            !value.starts_with('/')
                || !value.ends_with(".app")
                || value.chars().count() > 1024
                || value.chars().any(forbidden_character)
                || value.split('/').any(|part| matches!(part, "." | ".."))
        }) || application.bundle_identifier.as_ref().is_some_and(|value| {
            value.is_empty() || value.len() > 255 || value.chars().any(char::is_control)
        }) {
            return invalid("invalid bundle metadata");
        }
        if application.bundle_identifier.is_some() && application.bundle_path.is_none() {
            return invalid("bundle identifier requires a bundle path");
        }
        if let Some(bundle) = &application.bundle_path
            && application.path != *bundle
            && !application.path.starts_with(&format!("{bundle}/"))
        {
            return invalid("bundle executable must remain inside the selected bundle");
        }
        if !valid_leaf(&application.executable) {
            return invalid(
                "executable must be a leaf filename of at most 255 characters, not a command",
            );
        }
        if !application.path.is_empty() {
            let path = &application.path;
            let windows = crate::games::is_windows_path(path);
            let parts: Vec<_> = path
                .split(['/', '\\'])
                .filter(|part| !part.is_empty())
                .collect();
            let leaf = parts.last().copied().unwrap_or_default();
            if path.chars().count() > 1024
                || path.chars().any(forbidden_character)
                || if windows && path.as_bytes().get(1) == Some(&b':') {
                    path[2..].contains(':')
                } else {
                    path.contains(':')
                }
                || (!windows && !path.starts_with('/'))
                || path.ends_with(['/', '\\'])
                || parts.iter().any(|part| matches!(*part, "." | ".."))
                || ((path.starts_with("\\\\") || path.starts_with("//")) && parts.len() < 3)
                || if windows {
                    !leaf.eq_ignore_ascii_case(&application.executable)
                } else {
                    leaf != application.executable
                }
            {
                return invalid(
                    "path must be a full executable path matching the executable filename, at most 1024 characters",
                );
            }
        }
    }
    Ok(())
}

pub fn canonicalize_profile(
    profile: &mut crate::config::ApplicationProfile,
    applications: &[LocalApplication],
) -> Result<()> {
    if let Some(id) = &profile.application.local_id {
        let target = applications
            .iter()
            .find(|target| &target.id == id)
            .ok_or_else(|| InvalidInput(format!("unknown local application ID: {id}")))?;
        profile.application.name = target.name.clone();
        profile.application.executable = target.executable.clone();
        profile.application.path = target.path.clone();
        profile.application.activation = target.activation;
        profile.application.bundle_path = target.bundle_path.clone();
        profile.application.bundle_identifier = target.bundle_identifier.clone();
    }
    Ok(())
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(InvalidInput(message.into()).into())
}

fn forbidden_character(character: char) -> bool {
    character.is_control() || matches!(character, '*' | '?' | '"' | ';' | '|' | '<' | '>' | '`')
}

fn valid_leaf(value: &str) -> bool {
    !value.trim().is_empty()
        && value == value.trim()
        && value.chars().count() <= 255
        && !matches!(value, "." | "..")
        && !value.chars().any(|character| {
            forbidden_character(character) || matches!(character, '/' | '\\' | ':')
        })
        && !value.contains(" -")
        && !value.to_ascii_lowercase().contains(".exe ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LocalApplicationKind;

    fn target() -> LocalApplication {
        LocalApplication {
            id: "local:editor".into(),
            name: "Editor".into(),
            executable: "editor.exe".into(),
            path: String::new(),
            kind: LocalApplicationKind::App,
            activation: None,
            bundle_path: None,
            bundle_identifier: None,
        }
    }

    #[test]
    fn bundle_ingress_resolves_all_before_accepting_batch() {
        let mut input = target();
        input.path = "/Applications/Editor.app".into();
        let original = input.clone();
        let prepared = canonicalize_with(std::slice::from_ref(&input), |_| {
            Ok((
                "/Applications/Editor.app/Contents/MacOS/editor".into(),
                Some("io.editor".into()),
            ))
        })
        .unwrap();
        assert_eq!(prepared[0].executable, "editor");
        assert_eq!(
            prepared[0].bundle_path.as_deref(),
            Some(input.path.as_str())
        );
        assert_eq!(prepared[0].bundle_identifier.as_deref(), Some("io.editor"));
        assert_eq!(input, original);
        let mut missing = input.clone();
        missing.id = "local:missing".into();
        missing.path = "/Applications/Missing.app".into();
        assert!(
            canonicalize_with(&[input.clone(), missing], |path| {
                if path.contains("Missing") {
                    return invalid("missing bundle");
                }
                Ok((
                    "/Applications/Editor.app/Contents/MacOS/editor".into(),
                    None,
                ))
            })
            .is_err()
        );
        assert_eq!(input, original);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_bundle_resolution_is_bounded_and_rejects_moved_executables() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = parent.join(format!("bridge-native-bundle-{}", std::process::id()));
        let bundle = root.join("Editor.app");
        let executable = bundle.join("Contents/MacOS/editor");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        let info = bundle.join("Contents/Info.plist");
        fs::write(&info, r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleExecutable</key><string>editor</string><key>CFBundleIdentifier</key><string>io.editor</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>"#).unwrap();
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut input = target();
        input.path = bundle.to_str().unwrap().into();
        input.executable = "Editor.app".into();
        let result = canonicalize(std::slice::from_ref(&input)).unwrap();
        assert_eq!(result[0].path, executable.to_str().unwrap());
        assert_eq!(result[0].bundle_identifier.as_deref(), Some("io.editor"));
        let original_image = fs::read(&executable).unwrap();
        fs::write(&executable, [0xcf, 0xfa, 0xed, 0xfe]).unwrap();
        assert!(canonicalize(std::slice::from_ref(&input)).is_err());
        fs::write(&executable, original_image).unwrap();
        fs::rename(&executable, executable.with_extension("moved")).unwrap();
        assert!(canonicalize(std::slice::from_ref(&input)).is_err());
        fs::write(&info, vec![b'x'; 65537]).unwrap();
        assert!(canonicalize(std::slice::from_ref(&input)).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_metadata_and_image_open_reject_fifo_and_symlinks_without_blocking() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::symlink},
        };
        unsafe extern "C" {
            fn mkfifo(path: *const std::ffi::c_char, mode: u16) -> std::ffi::c_int;
        }
        let root = std::env::temp_dir().join(format!("bridge-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let fifo = root.join("Info.plist");
        let cpath = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(read_bundle_metadata(&fifo).is_err());
        assert!(checked_open(&fifo, u64::MAX).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        let file = root.join("regular");
        std::fs::write(&file, b"data").unwrap();
        let link = root.join("link");
        symlink(&file, &link).unwrap();
        assert!(read_bundle_metadata(&link).is_err());
        let raced = root.join("raced");
        std::fs::write(&raced, b"regular").unwrap();
        let raced_cpath = CString::new(raced.as_os_str().as_bytes()).unwrap();
        let start = std::time::Instant::now();
        assert!(
            checked_open_with(&raced, 65536, || {
                std::fs::remove_file(&raced)?;
                anyhow::ensure!(
                    unsafe { mkfifo(raced_cpath.as_ptr(), 0o600) } == 0,
                    "mkfifo failed"
                );
                Ok(())
            })
            .is_err()
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_bundle_metadata_requires_native_consistency() {
        let mut input = target();
        input.executable = "editor".into();
        input.path = "/Applications/Editor.app/Contents/MacOS/editor".into();
        input.bundle_path = Some("/Applications/Editor.app".into());
        input.bundle_identifier = Some("io.editor".into());
        let resolve = |_: &str| Ok((input.path.clone(), input.bundle_identifier.clone()));
        assert_eq!(
            canonicalize_with(std::slice::from_ref(&input), resolve).unwrap(),
            vec![input.clone()]
        );
        for (path, identifier) in [
            (
                "/Applications/Editor.app/Contents/MacOS/other",
                Some("io.editor"),
            ),
            (input.path.as_str(), Some("io.other")),
            (input.path.as_str(), None),
        ] {
            assert!(
                canonicalize_with(std::slice::from_ref(&input), |_| Ok((
                    path.into(),
                    identifier.map(str::to_owned)
                )))
                .is_err()
            );
        }
        let mut identifier_only = target();
        identifier_only.bundle_identifier = Some("io.editor".into());
        assert!(canonicalize_with(&[identifier_only], |_| unreachable!()).is_err());
    }

    #[test]
    fn unchanged_missing_bundle_allows_unrelated_crud_but_retarget_requires_resolution() {
        let mut old = target();
        old.executable = "editor".into();
        old.path = "/Applications/Editor.app/Contents/MacOS/editor".into();
        old.bundle_path = Some("/Applications/Editor.app".into());
        old.bundle_identifier = Some("io.editor".into());
        let mut renamed = old.clone();
        renamed.name = "Renamed".into();
        let mut other = target();
        other.id = "local:other".into();
        let result = canonicalize_with_previous(
            &[renamed.clone(), other.clone()],
            std::slice::from_ref(&old),
            |_| invalid("missing bundle"),
        )
        .unwrap();
        assert_eq!(result, vec![renamed, other.clone()]);
        assert_eq!(
            canonicalize_with_previous(
                std::slice::from_ref(&other),
                std::slice::from_ref(&old),
                |_| unreachable!()
            )
            .unwrap(),
            vec![other]
        );
        let mut retargeted = old.clone();
        retargeted.path.push('2');
        retargeted.executable.push('2');
        let mut identifier_changed = old.clone();
        identifier_changed.bundle_identifier = Some("io.other".into());
        let mut new_id = old.clone();
        new_id.id = "local:new".into();
        for changed in [retargeted, identifier_changed, new_id] {
            assert!(
                canonicalize_with_previous(&[changed], std::slice::from_ref(&old), |_| invalid(
                    "missing bundle"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn bounded_bundle_metadata_rejects_oversize_nonregular_and_changed_files() {
        let root = std::env::temp_dir().join(format!("bridge-metadata-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Info.plist");
        std::fs::write(&path, b"plist").unwrap();
        assert_eq!(read_bundle_metadata(&path).unwrap(), b"plist");
        let (file, metadata) = checked_open(&path, 65536).unwrap();
        std::fs::write(&path, b"changed length").unwrap();
        assert!(check_unchanged(&path, &file, &metadata).is_err());
        std::fs::write(&path, vec![0; 65537]).unwrap();
        assert!(read_bundle_metadata(&path).is_err());
        assert!(read_bundle_metadata(&root).is_err());
        drop(file);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_validation_accepts_literal_filename_punctuation_without_mutation() {
        for (executable, path) in [
            (
                "editor.exe",
                "C:/Users/O'Connor/Apps & Tools/$Apps/editor.exe",
            ),
            ("Tom Clancy's Game.exe", "C:/Games/Tom Clancy's Game.exe"),
            ("Tools & Games.exe", r"C:\Apps & Tools\Tools & Games.exe"),
            ("$editor.exe", r"\\server\share$\$editor.exe"),
            ("editor.exe", "/opt/O'Connor/Apps & Tools/$Apps/editor.exe"),
            ("Tom Clancy's Game.exe", ""),
            ("Tools & Games.exe", ""),
            ("$editor.exe", ""),
        ] {
            let mut entry = target();
            entry.executable = executable.into();
            entry.path = path.into();
            let original = entry.clone();
            assert!(validate(std::slice::from_ref(&entry)).is_ok(), "{entry:?}");
            assert_eq!(entry, original);
        }
    }

    #[test]
    fn local_validation_rejects_nonliteral_targets_without_mutation() {
        for (executable, path) in [
            (".", ""),
            ("..", ""),
            ("editor?.exe", ""),
            ("editor\u{7f}.exe", ""),
            ("editor.exe --flag", "C:/Tools/editor.exe --flag"),
            ("editor.exe;calc", "C:/Tools/editor.exe;calc"),
            ("editor.exe", "C:/Tools/./editor.exe"),
            ("editor.exe", "C:/Tools/../editor.exe"),
            ("editor.exe", "C:/Tools/ed\nitor/editor.exe"),
            ("editor.exe", "C:/Tools/*/editor.exe"),
            ("editor.exe", "C:/Tools/?/editor.exe"),
            ("editor.exe", "file:///Tools/editor.exe"),
            ("editor.exe", "C:/Users/O'Connor/Apps & Tools/other.exe"),
        ] {
            let mut entry = target();
            entry.executable = executable.into();
            entry.path = path.into();
            let original = entry.clone();
            assert!(validate(std::slice::from_ref(&entry)).is_err(), "{entry:?}");
            assert_eq!(entry, original);
        }
    }

    #[test]
    fn local_validation_accepts_boundaries_and_rejects_extra_path_colons() {
        let mut entry = target();
        entry.id = format!("local:{}", "a".repeat(128));
        entry.name = "字".repeat(80);
        entry.executable = "a".repeat(255);
        entry.path = format!("C:/{}/{}", "d".repeat(765), entry.executable);
        assert_eq!(entry.path.chars().count(), 1024);
        assert!(validate(&[entry]).is_ok());
        for path in [
            r"C:\Tools:stream\editor.exe",
            "/usr:bin/editor.exe",
            r"\\server:port\share\editor.exe",
        ] {
            let mut entry = target();
            entry.path = path.into();
            assert!(validate(&[entry]).is_err(), "{path}");
        }
        for path in [
            r"C:\Program Files\Editor\editor.exe",
            r"\\server\share\editor.exe",
            "/usr/bin/editor.exe",
        ] {
            let mut entry = target();
            entry.path = path.into();
            assert!(validate(&[entry]).is_ok(), "{path}");
        }
        let targets = (0..256)
            .map(|index| LocalApplication {
                id: format!("local:{index}"),
                ..target()
            })
            .collect::<Vec<_>>();
        assert!(validate(&targets).is_ok());
    }

    #[test]
    fn local_input_rejects_duplicate_ids_invalid_commands_and_limits() {
        let valid = target();
        assert!(validate(std::slice::from_ref(&valid)).is_ok());
        assert!(validate(&[valid.clone(), valid.clone()]).is_err());
        for id in ["", "editor", "local:", "local:bad/id", "local:日本語"] {
            let mut entry = valid.clone();
            entry.id = id.into();
            assert!(validate(&[entry]).is_err(), "{id}");
        }
        for executable in [
            "",
            "../editor",
            "C:\\editor.exe",
            "editor.exe --flag",
            "*.exe",
            "editor.exe;calc",
            "editor\n.exe",
            "\"editor.exe\"",
        ] {
            let mut entry = valid.clone();
            entry.executable = executable.into();
            assert!(validate(&[entry]).is_err(), "{executable}");
        }
        for path in [
            "tools/editor.exe",
            "C:editor.exe",
            "https://host/editor.exe",
            "C:/Tools/editor.exe --flag",
            "C:/Tools/*.exe",
            "C:/Tools/other.exe",
        ] {
            let mut entry = valid.clone();
            entry.path = path.into();
            assert!(validate(&[entry]).is_err(), "{path}");
        }
        let mut entry = valid.clone();
        entry.name = "字".repeat(80);
        assert!(validate(&[entry.clone()]).is_ok());
        entry.name.push('字');
        assert!(validate(&[entry]).is_err());
        let mut entry = valid.clone();
        entry.executable = "a".repeat(256);
        assert!(validate(&[entry]).is_err());
        let mut entry = valid.clone();
        entry.path = format!("C:/{}/editor.exe", "a".repeat(1024));
        assert!(validate(&[entry]).is_err());
        assert!(
            validate(
                &(0..257)
                    .map(|index| LocalApplication {
                        id: format!("local:{index}"),
                        ..valid.clone()
                    })
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }
}
