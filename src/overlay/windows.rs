//! The banner's window on Windows: a layered popup that is topmost,
//! click-through and never activated, so it can sit over a borderless game
//! without taking focus. It is kept out of the taskbar and Alt+Tab as a tool
//! window. Exclusive-fullscreen games draw over every window, so it does not
//! show there.

use std::{ffi::c_void, ptr::null_mut};

use anyhow::{Result, bail};
use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT, SIZE},
    Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
        CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, HBITMAP, HDC,
        HGDIOBJ, SelectObject,
    },
    Media::{
        Audio::{
            CALLBACK_NULL, HWAVEOUT, WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEFORMATEX, WAVEHDR,
            waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutUnprepareHeader,
            waveOutWrite,
        },
        MMSYSERR_NOERROR,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        HiDpi::GetDpiForSystem,
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOPMOST, RegisterClassW,
            SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            SWP_SHOWWINDOW, SetWindowPos, ShowWindow, SystemParametersInfoW, ULW_ALPHA,
            UpdateLayeredWindow, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        },
    },
};

use super::render::Banner;

/// Distance from the top of the work area, in points.
const TOP_MARGIN: f64 = 24.0;

/// A banner currently selected into a memory DC, ready to be composited.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    position: POINT,
    size: SIZE,
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

pub struct Window {
    hwnd: HWND,
    surface: Option<Surface>,
}

impl Window {
    pub fn new() -> Result<Self> {
        let class = wide("OpenMouseBridgeOverlay");
        let hwnd = unsafe {
            let instance = GetModuleHandleW(null_mut());
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..Default::default()
            };
            // Registering twice fails harmlessly; creation below is what counts.
            RegisterClassW(&window_class);
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                class.as_ptr(),
                null_mut(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            )
        };
        if hwnd.is_null() {
            bail!("Windows could not create the overlay window");
        }
        Ok(Self {
            hwnd,
            surface: None,
        })
    }

    /// Physical pixels per point. Bridge's event loop runs DPI aware, so
    /// window coordinates are physical pixels.
    pub fn scale(&self) -> f64 {
        f64::from(unsafe { GetDpiForSystem() }.max(96)) / 96.0
    }

    /// Shows `banner` fully transparent at the top centre of the primary work
    /// area; `set_opacity` fades it in.
    pub fn present(&mut self, banner: &Banner) -> Result<()> {
        self.surface = None;
        let (width, height) = (i32::from(banner.width), i32::from(banner.height));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative height: rows run top to bottom, like the banner.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = null_mut();
        let surface = unsafe {
            let dc = CreateCompatibleDC(null_mut());
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(dc);
                bail!("Windows could not allocate the overlay bitmap");
            }
            // Premultiplied RGBA to the premultiplied BGRA a layered window takes.
            let target = std::slice::from_raw_parts_mut(bits.cast::<u8>(), banner.pixels.len());
            let (target, _) = target.as_chunks_mut::<4>();
            let (source, _) = banner.pixels.as_chunks::<4>();
            for (out, [red, green, blue, alpha]) in target.iter_mut().zip(source) {
                *out = [*blue, *green, *red, *alpha];
            }
            let previous = SelectObject(dc, bitmap);
            let mut area = RECT::default();
            SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut area).cast(), 0);
            Surface {
                dc,
                bitmap,
                previous,
                position: POINT {
                    x: area.left + (area.right - area.left - width) / 2,
                    y: area.top + (TOP_MARGIN * self.scale()) as i32,
                },
                size: SIZE {
                    cx: width,
                    cy: height,
                },
            }
        };
        self.surface = Some(surface);
        self.set_opacity(0.0);
        unsafe {
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        Ok(())
    }

    pub fn set_opacity(&mut self, opacity: f32) {
        let Some(surface) = &self.surface else {
            return;
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let origin = POINT { x: 0, y: 0 };
        unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                null_mut(),
                &surface.position,
                &surface.size,
                surface.dc,
                &origin,
                0,
                &blend,
                ULW_ALPHA,
            );
        }
    }

    pub fn hide(&mut self) {
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
        self.surface = None;
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.surface = None;
        unsafe {
            DestroyWindow(self.hwnd);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// Plays the chime through waveOut rather than PlaySound: PlaySound is mixed
/// into the shared "System sounds" session, which many players mute, while
/// waveOut plays in Bridge's own session with its own Volume Mixer slider.
#[derive(Default)]
pub struct Speaker {
    playback: Option<Playback>,
}

impl Speaker {
    pub fn play(&mut self, wav: Vec<u8>) -> Result<()> {
        self.playback = None;
        self.playback = Some(Playback::start(wav)?);
        Ok(())
    }
}

/// One chime on an open waveOut device. waveOut reads the samples while they
/// play, so the buffer and its header stay put until the playback is dropped,
/// which cuts it off and closes the device.
struct Playback {
    device: HWAVEOUT,
    header: Box<WAVEHDR>,
    _wav: Vec<u8>,
}

impl Playback {
    fn start(wav: Vec<u8>) -> Result<Self> {
        // The chime is always the canonical 44-byte-header PCM WAV that
        // chime::wav writes, so its format sits at fixed offsets.
        let field = |offset: usize| u16::from_le_bytes([wav[offset], wav[offset + 1]]);
        let channels = field(22);
        let sample_rate = u32::from_le_bytes(wav[24..28].try_into()?);
        let bits = field(34);
        let block_align = channels * bits / 8;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate * u32::from(block_align),
            nBlockAlign: block_align,
            wBitsPerSample: bits,
            cbSize: 0,
        };

        let mut device: HWAVEOUT = null_mut();
        let opened = unsafe { waveOutOpen(&mut device, WAVE_MAPPER, &format, 0, 0, CALLBACK_NULL) };
        if opened != MMSYSERR_NOERROR {
            bail!("Windows could not open an audio device for the chime (error {opened})");
        }
        let samples = &wav[44..];
        let mut playback = Self {
            device,
            header: Box::new(WAVEHDR {
                lpData: samples.as_ptr().cast_mut(),
                dwBufferLength: samples.len() as u32,
                ..Default::default()
            }),
            _wav: wav,
        };
        let header: *mut WAVEHDR = &mut *playback.header;
        let size = size_of::<WAVEHDR>() as u32;
        let prepared = unsafe { waveOutPrepareHeader(device, header, size) };
        if prepared != MMSYSERR_NOERROR {
            bail!("Windows could not prepare the chime (error {prepared})");
        }
        let written = unsafe { waveOutWrite(device, header, size) };
        if written != MMSYSERR_NOERROR {
            bail!("Windows could not play the chime (error {written})");
        }
        Ok(playback)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let header: *mut WAVEHDR = &mut *self.header;
        unsafe {
            waveOutReset(self.device);
            waveOutUnprepareHeader(self.device, header, size_of::<WAVEHDR>() as u32);
            waveOutClose(self.device);
        }
    }
}
