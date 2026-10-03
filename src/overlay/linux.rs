//! The banner's window on Linux: an override-redirect X11 window that is
//! topmost, click-through and never activated, so it can sit over a
//! borderless game without taking focus. It is kept out of the taskbar and
//! pager as a notification-type window. Exclusive-fullscreen games draw over
//! every window, so it does not show there.
//!
//! This needs only `x11rb` (pure Rust): the banner pixels come from the
//! shared CPU renderer, so the window just blits them with `PutImage` and
//! fades by scaling the alpha channel and re-blitting.
//!
//! Wayland has no X connection, so `Window::new` fails there and notices
//! fall back to system notifications (see `TrayApp::present`).

use anyhow::{Context, Result, bail, ensure};

use x11rb::connection::Connection;
use x11rb::protocol::shape;
use x11rb::protocol::xproto::{
    AtomEnum, ClipOrdering, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    EventMask, ImageFormat, PropMode, StackMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::render::Banner;

/// Distance from the top of the screen, in pixels.
const TOP_MARGIN: i16 = 24;

/// A banner blitted into an X pixmap and copied to the window, ready to be
/// shown. The pixmap is recreated only when the banner size changes, so
/// opacity fades just re-blit into it.
struct Surface {
    pixmap: u32,
    width: u16,
    height: u16,
}

pub struct Window {
    conn: RustConnection,
    screen: usize,
    window: u32,
    gc: u32,
    depth: u8,
    surface: Option<Surface>,
    banner: Option<Banner>,
}

impl Window {
    pub fn new() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("could not open the X display")?;
        let (root, depth) = {
            let setup = conn.setup();
            let screen_info = &setup.roots[screen];
            (screen_info.root, screen_info.root_depth)
        };
        ensure!(
            depth >= 24,
            "the X screen depth ({depth}) cannot show the banner"
        );

        let window: u32 = conn
            .generate_id()
            .context("could not allocate the overlay window")?;
        let aux = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0)
            .event_mask(EventMask::EXPOSURE);
        conn.create_window(
            depth,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &aux,
        )
        .context("could not create the overlay window")?
        .check()
        .context("could not create the overlay window")?;

        let gc: u32 = conn
            .generate_id()
            .context("could not allocate the overlay graphics context")?;
        conn.create_gc(gc, window, &CreateGCAux::new())
            .context("could not create the overlay graphics context")?
            .check()
            .context("could not create the overlay graphics context")?;

        set_hints(&conn, window);
        make_click_through(&conn, window);

        Ok(Self {
            conn,
            screen,
            window,
            gc,
            depth,
            surface: None,
            banner: None,
        })
    }

    /// Physical pixels per point. The X blit is unscaled, so one point is
    /// one pixel.
    pub fn scale(&self) -> f64 {
        1.0
    }

    /// Shows `banner` fully transparent at the top centre of the screen;
    /// `set_opacity` fades it in.
    pub fn present(&mut self, banner: &Banner) -> Result<()> {
        ensure!(
            banner.width > 0 && banner.height > 0,
            "the overlay banner is empty"
        );
        self.banner = Some(Banner {
            width: banner.width,
            height: banner.height,
            pixels: banner.pixels.clone(),
        });
        self.blit(0.0)?;
        let screen_width = {
            let setup = self.conn.setup();
            setup.roots[self.screen].width_in_pixels
        };
        let x = (i32::from(screen_width) - i32::from(banner.width)) / 2;
        let aux = ConfigureWindowAux::new()
            .x(x)
            .y(i32::from(TOP_MARGIN))
            .width(u32::from(banner.width))
            .height(u32::from(banner.height))
            .stack_mode(StackMode::ABOVE);
        self.conn
            .configure_window(self.window, &aux)
            .context("could not place the overlay window")?
            .check()
            .context("could not place the overlay window")?;
        self.conn
            .map_window(self.window)
            .context("could not show the overlay window")?
            .check()
            .context("could not show the overlay window")?;
        self.conn
            .flush()
            .context("could not show the overlay window")
    }

    pub fn set_opacity(&mut self, opacity: f32) {
        if self.banner.is_none() {
            return;
        }
        if self.blit(opacity).is_err() {
            return;
        }
        let _ = self.conn.flush();
    }

    pub fn hide(&mut self) {
        self.banner = None;
        let _ = self
            .conn
            .unmap_window(self.window)
            .map(|cookie| cookie.check())
            .map(|_| self.conn.flush());
    }

    /// Blits the stored banner into the pixmap at `opacity` and copies it to
    /// the window.
    fn blit(&mut self, opacity: f32) -> Result<()> {
        let banner = self.banner.as_ref().context("no banner to show")?;
        let (width, height) = (banner.width, banner.height);
        let reuse = self
            .surface
            .as_ref()
            .is_some_and(|surface| surface.width == width && surface.height == height);
        if !reuse {
            if let Some(surface) = self.surface.take() {
                self.conn
                    .free_pixmap(surface.pixmap)
                    .context("could not release the overlay pixmap")?
                    .check()
                    .context("could not release the overlay pixmap")?;
            }
            let root = self.conn.setup().roots[self.screen].root;
            let pixmap: u32 = self
                .conn
                .generate_id()
                .context("could not allocate the overlay pixmap")?;
            self.conn
                .create_pixmap(self.depth, pixmap, root, width, height)
                .context("could not allocate the overlay pixmap")?
                .check()
                .context("could not allocate the overlay pixmap")?;
            self.surface = Some(Surface {
                pixmap,
                width,
                height,
            });
        }
        let surface = self.surface.as_ref().context("no overlay pixmap")?;
        let pixels = pack_bgrx(&banner.pixels, opacity);
        self.conn
            .put_image(
                ImageFormat::Z_PIXMAP,
                surface.pixmap,
                self.gc,
                width,
                height,
                0,
                0,
                0,
                self.depth,
                &pixels,
            )
            .context("could not draw the overlay banner")?
            .check()
            .context("could not draw the overlay banner")?;
        self.conn
            .copy_area(
                surface.pixmap,
                self.window,
                self.gc,
                0,
                0,
                0,
                0,
                width,
                height,
            )
            .context("could not show the overlay banner")?
            .check()
            .context("could not show the overlay banner")?;
        self.conn
            .flush()
            .context("could not show the overlay banner")
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if let Some(surface) = self.surface.take() {
            let _ = self
                .free_pixmap_and_window(surface.pixmap)
                .map(|_| self.conn.flush());
        } else {
            let _ = self
                .conn
                .destroy_window(self.window)
                .map(|cookie| cookie.check())
                .map(|_| self.conn.flush());
        }
        let _ = self.conn.free_gc(self.gc).map(|_| self.conn.flush());
    }
}

impl Window {
    fn free_pixmap_and_window(&self, pixmap: u32) -> Result<()> {
        self.conn
            .free_pixmap(pixmap)
            .context("could not release the overlay pixmap")?
            .check()
            .context("could not release the overlay pixmap")?;
        self.conn
            .destroy_window(self.window)
            .context("could not destroy the overlay window")?
            .check()
            .context("could not destroy the overlay window")?;
        Ok(())
    }
}

/// Marks the window as a notification that stays above and out of the
/// taskbar and pager. Best effort: a window manager is free to ignore these.
fn set_hints(conn: &RustConnection, window: u32) {
    let intern = |name: &[u8]| {
        conn.intern_atom(false, name)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.atom)
    };
    let (
        Some(window_type),
        Some(window_type_notification),
        Some(state),
        Some(state_above),
        Some(state_skip_taskbar),
        Some(state_skip_pager),
        Some(wm_name),
        Some(utf8),
    ) = (
        intern(b"_NET_WM_WINDOW_TYPE"),
        intern(b"_NET_WM_WINDOW_TYPE_NOTIFICATION"),
        intern(b"_NET_WM_STATE"),
        intern(b"_NET_WM_STATE_ABOVE"),
        intern(b"_NET_WM_STATE_SKIP_TASKBAR"),
        intern(b"_NET_WM_STATE_SKIP_PAGER"),
        intern(b"_NET_WM_NAME"),
        intern(b"UTF8_STRING"),
    )
    else {
        return;
    };
    let _ = conn
        .change_property32(
            PropMode::REPLACE,
            window,
            window_type,
            AtomEnum::ATOM,
            &[window_type_notification],
        )
        .map(|cookie| cookie.check());
    let _ = conn
        .change_property32(
            PropMode::REPLACE,
            window,
            state,
            AtomEnum::ATOM,
            &[state_above, state_skip_taskbar, state_skip_pager],
        )
        .map(|cookie| cookie.check());
    let _ = conn
        .change_property8(
            PropMode::REPLACE,
            window,
            wm_name,
            utf8,
            b"OpenMouse Bridge",
        )
        .map(|cookie| cookie.check());
    let _ = conn
        .change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"openmouse-bridge\0OpenMouse-Bridge\0",
        )
        .map(|cookie| cookie.check());
    let _ = conn.flush();
}

/// Empties the window's input shape so clicks pass through to the game
/// below. Best effort: needs the Shape extension, and layered transparency
/// still depends on a compositing window manager.
fn make_click_through(conn: &RustConnection, window: u32) {
    if conn
        .extension_information(shape::X11_EXTENSION_NAME)
        .map(|info| info.is_some())
        .unwrap_or(false)
    {
        let _ = shape::rectangles(
            conn,
            shape::SO::SET,
            shape::SK::INPUT,
            ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &[],
        )
        .map(|cookie| cookie.check());
        let _ = conn.flush();
    }
}

/// Converts premultiplied RGBA8 rows to the BGRX bytes an X `ZPixmap` takes,
/// scaling the banner to `opacity` first (scaling premultiplied channels and
/// alpha together keeps the blend correct).
fn pack_bgrx(pixels: &[u8], opacity: f32) -> Vec<u8> {
    let opacity = opacity.clamp(0.0, 1.0);
    let (source, _) = pixels.as_chunks::<4>();
    let mut packed = Vec::with_capacity(source.len() * 4);
    for &[red, green, blue, alpha] in source {
        let scale = f32::from(alpha) / 255.0 * opacity;
        let unpremultiply = if alpha == 0 {
            0.0
        } else {
            255.0 / f32::from(alpha)
        };
        packed.push(
            (f32::from(blue) * unpremultiply * scale)
                .round()
                .clamp(0.0, 255.0) as u8,
        );
        packed.push(
            (f32::from(green) * unpremultiply * scale)
                .round()
                .clamp(0.0, 255.0) as u8,
        );
        packed.push(
            (f32::from(red) * unpremultiply * scale)
                .round()
                .clamp(0.0, 255.0) as u8,
        );
        packed.push(0);
    }
    packed
}

/// Plays the chime through PulseAudio/PipeWire (`paplay`) or ALSA (`aplay`),
/// piping the synthesized WAV on stdin so no audio crate or asset is needed.
/// A still-playing chime is cut off first. Silent when neither player exists.
#[derive(Default)]
pub struct Speaker {
    playback: Option<std::process::Child>,
}

impl Speaker {
    pub fn play(&mut self, wav: Vec<u8>) -> Result<()> {
        if let Some(mut previous) = self.playback.take() {
            let _ = previous.kill();
            let _ = previous.wait();
        }
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let mut child = Command::new("paplay")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        if child.is_none() {
            child = Command::new("aplay")
                .arg("-q")
                .arg("-")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
        }
        let Some(mut child) = child else {
            tracing::warn!("No audio player (paplay or aplay) for the notification chime");
            return Ok(());
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(&wav).is_err() {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
        }
        self.playback = Some(child);
        Ok(())
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        if let Some(mut playback) = self.playback.take() {
            let _ = playback.kill();
            let _ = playback.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_pixels_pack_to_zero() {
        let packed = pack_bgrx(&[200, 100, 50, 0], 1.0);
        assert_eq!(packed, vec![0, 0, 0, 0]);
    }

    #[test]
    fn opacity_scales_premultiplied_channels() {
        // Opaque red at half opacity is half red.
        let packed = pack_bgrx(&[255, 0, 0, 255], 0.5);
        assert_eq!(packed, vec![0, 0, 128, 0]);
    }

    #[test]
    fn premultiplied_grey_round_trips() {
        // Half-transparent white stored premultiplied stays grey, not white.
        let packed = pack_bgrx(&[128, 128, 128, 128], 1.0);
        assert_eq!(packed, vec![128, 128, 128, 0]);
    }

    #[test]
    fn window_needs_an_x_display() {
        if std::env::var_os("DISPLAY").is_some() {
            return;
        }
        assert!(Window::new().is_err());
    }
}
