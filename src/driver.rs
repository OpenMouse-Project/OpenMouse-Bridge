//! One-click WinUSB driver install for the Attack Shark X11 config interface.
//!
//! On Windows the mouse's config interface (interface 2) is owned by the HID
//! driver, which refuses raw config traffic. To reach it the interface must be
//! bound to WinUSB — the same thing Zadig does, but here we ship our own scoped
//! driver package (`driver/OpenMouse-AttackShark-X11.inf`) and install it with
//! `pnputil` behind a single UAC prompt.
//!
//! Safety: the package matches only interface 2 of the three known product IDs,
//! writes no firmware, and is fully reversible (`uninstall`). See
//! `driver/README.md`.
//!
//! Other platforms don't need this — the Bridge claims the interface directly —
//! so the calls there just explain that.

use anyhow::Result;

/// Whether a driver install is meaningful on this platform (Windows only).
pub fn is_supported() -> bool {
    cfg!(target_os = "windows")
}

#[cfg(target_os = "windows")]
pub fn install() -> Result<()> {
    windows_impl::install()
}

#[cfg(target_os = "windows")]
pub fn uninstall() -> Result<()> {
    windows_impl::uninstall()
}

#[cfg(not(target_os = "windows"))]
pub fn install() -> Result<()> {
    anyhow::bail!(
        "the WinUSB driver install is only needed on Windows; elsewhere the Bridge claims the \
         device directly (Linux needs only a udev rule)"
    )
}

#[cfg(not(target_os = "windows"))]
pub fn uninstall() -> Result<()> {
    anyhow::bail!("no OpenMouse driver is installed on this platform")
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::{env, ffi::OsStr, mem::zeroed, os::windows::ffi::OsStrExt, path::PathBuf};

    use anyhow::{Context, Result, bail};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
        UI::{
            Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_HIDE,
        },
    };

    const INSTALL_SCRIPT: &str = "sign-and-install.ps1";
    const UNINSTALL_SCRIPT: &str = "uninstall.ps1";
    /// Name of the transcript the scripts write in %TEMP% (same user, so the
    /// non-elevated Bridge can read it back after an elevated run).
    const TRANSCRIPT: &str = "openmouse-driver.log";

    /// Locate a shipped driver-folder file. Next to the executable in a release
    /// install; the repo's `driver/` folder as a fallback for `cargo run`.
    fn driver_file(name: &str) -> Result<PathBuf> {
        let exe = env::current_exe().context("could not locate the Bridge executable")?;
        let dir = exe
            .parent()
            .context("the Bridge executable has no parent directory")?;
        let dev_fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("driver");
        for base in [dir.join("driver"), dir.to_path_buf(), dev_fallback] {
            let candidate = base.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        bail!(
            "could not find {name}. Ship the driver folder next to the Bridge executable \
             (see driver/README.md)."
        )
    }

    pub fn install() -> Result<()> {
        run_script(INSTALL_SCRIPT).context(
            "enabling native control failed. This self-signs and installs the WinUSB driver for \
             interface 2 of the Attack Shark; it needs the UAC prompt to be approved.",
        )
    }

    pub fn uninstall() -> Result<()> {
        run_script(UNINSTALL_SCRIPT)
    }

    /// Run one of the bundled driver scripts elevated, returning a rich error
    /// (including the script transcript) when it fails.
    fn run_script(name: &str) -> Result<()> {
        let script = driver_file(name)?;
        let args = format!(
            "-NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            script.display()
        );
        match run_elevated("powershell.exe", &args)? {
            0 => Ok(()),
            other => {
                let detail = transcript_tail().unwrap_or_default();
                if detail.is_empty() {
                    bail!("{name} exited with code {other}")
                } else {
                    bail!("{name} exited with code {other}:\n{detail}")
                }
            }
        }
    }

    /// Read the tail of the script transcript for error reporting.
    fn transcript_tail() -> Option<String> {
        let path = env::var_os("TEMP").map(PathBuf::from)?.join(TRANSCRIPT);
        let text = std::fs::read_to_string(path).ok()?;
        let tail: Vec<&str> = text.lines().rev().take(20).collect();
        Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }

    /// Run a program elevated (UAC "runas") and return its exit code, waiting
    /// for it to finish.
    fn run_elevated(program: &str, args: &str) -> Result<u32> {
        let verb = wide("runas");
        let file = wide(program);
        let params = wide(args);

        let mut info: SHELLEXECUTEINFOW = unsafe { zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;

        let launched = unsafe { ShellExecuteExW(&mut info) };
        if launched == 0 {
            return Err(std::io::Error::last_os_error()).context(
                "could not start the elevated installer (the UAC prompt may have been declined)",
            );
        }
        if info.hProcess.is_null() {
            bail!("the elevated installer did not start");
        }

        unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 0u32;
            let read = GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            if read == 0 {
                bail!("could not read the installer exit code");
            }
            Ok(code)
        }
    }

    fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value.as_ref().encode_wide().chain(Some(0)).collect()
    }
}
