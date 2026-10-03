use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationInfo {
    pub name: String,
    pub executable: String,
    pub path: String,
    pub foreground: bool,
    pub icon_id: String,
}

#[cfg_attr(
    not(any(target_os = "windows", target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
fn icon_id(path: &str) -> String {
    let mut hasher = DefaultHasher::new();
    path.to_ascii_lowercase().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(target_os = "windows")]
mod imp {
    use std::{
        collections::BTreeMap,
        ffi::{OsStr, OsString, c_void},
        io::Cursor,
        mem::size_of,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::Path,
        ptr::{null_mut, slice_from_raw_parts, write_bytes},
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HWND, LPARAM},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, SelectObject,
        },
        Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
        UI::{
            Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW},
            WindowsAndMessaging::{
                DI_NORMAL, DestroyIcon, DrawIconEx, EnumWindows, GetForegroundWindow,
                GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
            },
        },
    };

    use super::{ApplicationInfo, icon_id};

    const ICON_SIZE: u32 = 48;

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

    unsafe extern "system" fn collect_window(hwnd: HWND, state: LPARAM) -> i32 {
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
        let icon_id = icon_id(&path);
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
                icon_id,
            });
        1
    }

    pub fn application_icon(path: &str) -> Option<Vec<u8>> {
        let wide_path: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
        let mut file_info = SHFILEINFOW::default();
        let result = unsafe {
            SHGetFileInfoW(
                wide_path.as_ptr(),
                0 as FILE_FLAGS_AND_ATTRIBUTES,
                &mut file_info,
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            )
        };
        if result == 0 || file_info.hIcon.is_null() {
            return None;
        }
        let pixels = unsafe { render_icon(file_info.hIcon) };
        unsafe { DestroyIcon(file_info.hIcon) };
        pixels.and_then(encode_png)
    }

    unsafe fn render_icon(
        icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON,
    ) -> Option<Vec<u8>> {
        let dc = unsafe { CreateCompatibleDC(null_mut()) };
        if dc.is_null() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ICON_SIZE as i32,
                biHeight: -(ICON_SIZE as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = null_mut();
        let bitmap =
            unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
        if bitmap.is_null() || bits.is_null() {
            unsafe { DeleteDC(dc) };
            return None;
        }
        let byte_count = (ICON_SIZE * ICON_SIZE * 4) as usize;
        unsafe { write_bytes(bits.cast::<u8>(), 0, byte_count) };
        let previous = unsafe { SelectObject(dc, bitmap) };
        let drawn = unsafe {
            DrawIconEx(
                dc,
                0,
                0,
                icon,
                ICON_SIZE as i32,
                ICON_SIZE as i32,
                0,
                null_mut(),
                DI_NORMAL,
            )
        } != 0;
        let mut rgba = if drawn {
            unsafe { &*slice_from_raw_parts(bits.cast::<u8>(), byte_count) }.to_vec()
        } else {
            Vec::new()
        };
        if !previous.is_null() {
            unsafe { SelectObject(dc, previous) };
        }
        unsafe {
            DeleteObject(bitmap);
            DeleteDC(dc);
        }
        if !drawn {
            return None;
        }
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Some(rgba)
    }

    fn encode_png(rgba: Vec<u8>) -> Option<Vec<u8>> {
        let mut output = Cursor::new(Vec::new());
        let mut encoder = png::Encoder::new(&mut output, ICON_SIZE, ICON_SIZE);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&rgba).ok()?;
        writer.finish().ok()?;
        Some(output.into_inner())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::{collections::BTreeMap, path::Path};

    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};

    use super::{ApplicationInfo, icon_id};

    pub fn visible_applications() -> Vec<ApplicationInfo> {
        let workspace = NSWorkspace::sharedWorkspace();
        let mut applications = BTreeMap::new();
        for application in workspace.runningApplications().iter() {
            if application.isTerminated()
                || application.activationPolicy() != NSApplicationActivationPolicy::Regular
            {
                continue;
            }
            let Some(name) = application.localizedName().map(|name| name.to_string()) else {
                continue;
            };
            let Some(path) = application
                .executableURL()
                .and_then(|url| url.path())
                .map(|path| path.to_string())
            else {
                continue;
            };
            let executable = Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.is_empty() || executable.is_empty() {
                continue;
            }
            let key = path.to_lowercase();
            applications.entry(key).or_insert(ApplicationInfo {
                name,
                executable,
                icon_id: icon_id(&path),
                path,
                foreground: application.isActive(),
            });
        }
        applications.into_values().collect()
    }

    pub fn application_icon(_path: &str) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::BTreeMap;
    use std::path::Path;

    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    use super::{ApplicationInfo, icon_id};

    /// Lists one entry per running process: `sysinfo` sees every process, but
    /// X11 only knows mapped windows, and matching the two for every process
    /// each poll costs a round trip per PID. Like macOS, only the foreground
    /// entry is matched to a window; on Wayland (no X connection) every
    /// entry reports `foreground: false`, so game-profile fallback still
    /// works but focus matching does not.
    pub fn visible_applications() -> Vec<ApplicationInfo> {
        let foreground_pid = active_window_pid();
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        let mut applications = BTreeMap::new();
        for process in system.processes().values() {
            let Some(path) = process
                .exe()
                .map(|path| path.to_string_lossy().into_owned())
            else {
                continue;
            };
            let executable = Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if executable.is_empty() {
                continue;
            }
            let name = process_name(process);
            if name.is_empty() {
                continue;
            }
            let foreground = foreground_pid.is_some_and(|pid| Pid::from_u32(pid) == process.pid());
            let key = path.to_lowercase();
            applications
                .entry(key)
                .and_modify(|application: &mut ApplicationInfo| {
                    application.foreground |= foreground;
                })
                .or_insert(ApplicationInfo {
                    name,
                    executable,
                    icon_id: icon_id(&path),
                    path,
                    foreground,
                });
        }
        applications.into_values().collect()
    }

    fn process_name(process: &sysinfo::Process) -> String {
        let name = process.name().to_string_lossy().into_owned();
        if name.is_empty() {
            return String::new();
        }
        // sysinfo truncates process names to 15 bytes on Linux (comm); prefer
        // the executable stem when it carries more of the name.
        if name.len() < 15 {
            return name;
        }
        process
            .exe()
            .and_then(|path| path.file_stem())
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or(name)
    }

    pub fn application_icon(_path: &str) -> Option<Vec<u8>> {
        None
    }

    /// The PID owning the active X11 window, via `_NET_ACTIVE_WINDOW` and
    /// `_NET_WM_PID`. `None` without an X connection (e.g. on Wayland) or
    /// when the window manager reports no active window.
    fn active_window_pid() -> Option<u32> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots[screen].root;
        let active_window = intern(&conn, b"_NET_ACTIVE_WINDOW")?;
        let pid_atom = intern(&conn, b"_NET_WM_PID")?;
        let reply = conn
            .get_property(false, root, active_window, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let window = reply.value32()?.next()?;
        if window == 0 {
            return None;
        }
        let reply = conn
            .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let pid = reply.value32()?.next()?;
        (pid != 0).then_some(pid)
    }

    fn intern(conn: &x11rb::rust_connection::RustConnection, name: &[u8]) -> Option<u32> {
        conn.intern_atom(false, name)
            .ok()?
            .reply()
            .ok()
            .map(|reply| reply.atom)
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
mod imp {
    use super::ApplicationInfo;

    pub fn visible_applications() -> Vec<ApplicationInfo> {
        Vec::new()
    }

    pub fn application_icon(_path: &str) -> Option<Vec<u8>> {
        None
    }
}

pub use imp::{application_icon, visible_applications};

#[cfg(test)]
mod tests {
    use super::icon_id;

    #[test]
    fn icon_ids_are_case_insensitive_like_application_paths() {
        assert_eq!(
            icon_id(r"C:\\Games\\GAME.EXE"),
            icon_id(r"c:\\games\\game.exe")
        );
    }
}
