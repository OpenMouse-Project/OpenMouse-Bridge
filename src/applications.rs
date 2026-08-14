use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationInfo {
    pub name: String,
    pub executable: String,
    pub path: String,
    pub foreground: bool,
}

#[cfg(target_os = "windows")]
mod imp {
    use std::{collections::BTreeMap, ffi::OsString, os::windows::ffi::OsStringExt, path::Path};

    use windows_sys::Win32::{
        Foundation::{BOOL, CloseHandle, HWND, LPARAM},
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
            GetWindowThreadProcessId, IsWindowVisible,
        },
    };

    use super::ApplicationInfo;

    struct WindowCollection {
        foreground: HWND,
        applications: BTreeMap<String, ApplicationInfo>,
    }

    pub fn visible_applications() -> Vec<ApplicationInfo> {
        let mut collection = WindowCollection {
            foreground: unsafe { GetForegroundWindow() },
            applications: BTreeMap::new(),
        };
        unsafe {
            let _ = EnumWindows(
                Some(collect_window),
                &mut collection as *mut WindowCollection as LPARAM,
            );
        }
        collection.applications.into_values().collect()
    }

    unsafe extern "system" fn collect_window(hwnd: HWND, state: LPARAM) -> BOOL {
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let title_length = unsafe { GetWindowTextLengthW(hwnd) };
        if title_length <= 0 {
            return 1;
        }
        let mut title = vec![0_u16; title_length as usize + 1];
        let copied = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        if copied <= 0 {
            return 1;
        }
        let title = OsString::from_wide(&title[..copied as usize])
            .to_string_lossy()
            .trim()
            .to_owned();
        if title.is_empty() {
            return 1;
        }
        let mut process_id = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
        if process_id == 0 {
            return 1;
        }
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
        if handle.is_null() {
            return 1;
        }
        let mut path = vec![0_u16; 32_768];
        let mut length = path.len() as u32;
        let found =
            unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut length) };
        unsafe { CloseHandle(handle) };
        if found == 0 || length == 0 {
            return 1;
        }
        let path = OsString::from_wide(&path[..length as usize])
            .to_string_lossy()
            .into_owned();
        let executable = Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if executable.is_empty() {
            return 1;
        }
        let collection = unsafe { &mut *(state as *mut WindowCollection) };
        let key = path.to_ascii_lowercase();
        let foreground = hwnd == collection.foreground;
        collection
            .applications
            .entry(key)
            .and_modify(|application| application.foreground |= foreground)
            .or_insert(ApplicationInfo {
                name: title,
                executable,
                path,
                foreground,
            });
        1
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::ApplicationInfo;

    pub fn visible_applications() -> Vec<ApplicationInfo> {
        Vec::new()
    }
}

pub use imp::visible_applications;
