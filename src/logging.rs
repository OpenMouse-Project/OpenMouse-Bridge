//! File + console logging for the Bridge.
//!
//! The release build on Windows has no console (`windows_subsystem =
//! "windows"`), so testers never see stdout. This writes the same logs to a
//! fixed file — `bridge.log` in the data directory — that anyone can find and
//! send back. The file is truncated on each start so it holds exactly one
//! session: reproduce the issue, then share the file.

use std::{env, fs, io, path::PathBuf};

use directories::ProjectDirs;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Default verbosity when `RUST_LOG` is unset. Debug on our own crate so device
/// enumeration and HID errors are captured; quieter for dependencies.
const DEFAULT_FILTER: &str = "openmouse_bridge=debug,tower_http=info";

/// Directory the log file lives in. Overridable with `OPENMOUSE_BRIDGE_LOG_DIR`
/// so a tester can redirect it somewhere obvious if needed.
pub fn log_dir() -> PathBuf {
    if let Some(dir) = env::var_os("OPENMOUSE_BRIDGE_LOG_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dirs) = ProjectDirs::from("io", "OpenMouse", "OpenMouse Bridge") {
        return dirs.data_dir().join("logs");
    }
    env::temp_dir().join("openmouse-bridge-logs")
}

/// Full path to the log file testers should send.
pub fn log_file_path() -> PathBuf {
    log_dir().join("bridge.log")
}

fn make_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// Initialize logging to both stdout and the log file. The returned guard
/// flushes the non-blocking file writer on drop, so the caller must hold it for
/// the lifetime of the program.
pub fn init() -> WorkerGuard {
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    // Start each run with a fresh file so a shared log is only this session.
    let _ = fs::remove_file(dir.join("bridge.log"));

    let (file_writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(&dir, "bridge.log"));

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(io::stdout)
                .with_filter(make_filter()),
        )
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(make_filter()),
        )
        .init();

    tracing::info!(path = %log_file_path().display(), "OpenMouse Bridge logging started");
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_honors_the_override() {
        // SAFETY: single-threaded test, restores the variable before returning.
        let previous = env::var_os("OPENMOUSE_BRIDGE_LOG_DIR");
        unsafe { env::set_var("OPENMOUSE_BRIDGE_LOG_DIR", "/tmp/openmouse-test-logs") };
        assert_eq!(log_dir(), PathBuf::from("/tmp/openmouse-test-logs"));
        assert_eq!(
            log_file_path(),
            PathBuf::from("/tmp/openmouse-test-logs").join("bridge.log"),
        );
        match previous {
            Some(value) => unsafe { env::set_var("OPENMOUSE_BRIDGE_LOG_DIR", value) },
            None => unsafe { env::remove_var("OPENMOUSE_BRIDGE_LOG_DIR") },
        }
    }

    #[test]
    fn log_file_is_named_bridge_log() {
        assert_eq!(
            log_file_path().file_name().and_then(|name| name.to_str()),
            Some("bridge.log"),
        );
    }
}
