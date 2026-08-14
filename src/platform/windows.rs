use std::{env, ffi::OsStr, mem::size_of, os::windows::ffi::OsStrExt, ptr::null_mut};

use anyhow::{Context, Result};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR},
    System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    },
};

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "OpenMouse Bridge";

pub const fn platform_name() -> &'static str {
    "windows"
}

pub fn autostart_enabled() -> bool {
    let subkey = wide(RUN_SUBKEY);
    let value_name = wide(VALUE_NAME);
    let mut value_type = 0;
    let mut byte_count = 0;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value_name.as_ptr(),
            RRF_RT_REG_SZ,
            &mut value_type,
            null_mut(),
            &mut byte_count,
        ) == ERROR_SUCCESS
    }
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    let subkey = wide(RUN_SUBKEY);
    let value_name = wide(VALUE_NAME);
    let result = if enabled {
        let executable = env::current_exe().context("could not locate the Bridge executable")?;
        let value = format!("\"{}\"", executable.display());
        let value = wide(&value);
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value_name.as_ptr(),
                REG_SZ,
                value.as_ptr().cast(),
                (value.len() * size_of::<u16>()) as u32,
            )
        }
    } else {
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), value_name.as_ptr()) }
    };
    if result == ERROR_SUCCESS || (!enabled && result == ERROR_FILE_NOT_FOUND) {
        return Ok(());
    }
    Err(registry_error(result)).context("Windows rejected the autostart change")
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn registry_error(code: WIN32_ERROR) -> std::io::Error {
    std::io::Error::from_raw_os_error(code as i32)
}
