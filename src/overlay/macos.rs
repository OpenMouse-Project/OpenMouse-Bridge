//! The banner's window on macOS: a borderless non-activating panel that
//! ignores the mouse, floats at status-bar level and may join fullscreen
//! Spaces, so it can sit over a game without taking focus.

use anyhow::{Context, Result};
use objc2::{AnyThread as _, MainThreadMarker, MainThreadOnly as _, rc::Retained};
use objc2_app_kit::{
    NSBackingStoreType, NSBitmapFormat, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace, NSImage,
    NSImageView, NSPanel, NSScreen, NSStatusWindowLevel, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::render::Banner;

/// Distance below the menu bar, in points.
const TOP_MARGIN: f64 = 12.0;

pub struct Window {
    panel: Retained<NSPanel>,
    mtm: MainThreadMarker,
}

impl Window {
    pub fn new() -> Result<Self> {
        let mtm =
            MainThreadMarker::new().context("the overlay must be created on the main thread")?;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)),
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setLevel(NSStatusWindowLevel);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setIgnoresMouseEvents(true);
        panel.setHidesOnDeactivate(false);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        // Closing is never used; ordering out hides it.
        unsafe { panel.setReleasedWhenClosed(false) };
        Ok(Self { panel, mtm })
    }

    fn screen(&self) -> Option<Retained<NSScreen>> {
        // The first screen is the one with the menu bar.
        NSScreen::screens(self.mtm).firstObject()
    }

    /// Physical pixels per point on the menu-bar screen.
    pub fn scale(&self) -> f64 {
        self.screen()
            .map(|screen| screen.backingScaleFactor())
            .unwrap_or(2.0)
    }

    /// Shows `banner` fully transparent at the top centre of the menu-bar
    /// screen; `set_opacity` fades it in.
    pub fn present(&mut self, banner: &Banner) -> Result<()> {
        let scale = self.scale();
        let (width, height) = (usize::from(banner.width), usize::from(banner.height));
        let representation = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                std::ptr::null_mut(),
                width as isize,
                height as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                // Alpha last and premultiplied, like the banner.
                NSBitmapFormat::empty(),
                (width * 4) as isize,
                32,
            )
        }
        .context("macOS could not allocate the overlay bitmap")?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                banner.pixels.as_ptr(),
                representation.bitmapData(),
                banner.pixels.len(),
            );
        }
        let size = NSSize::new(width as f64 / scale, height as f64 / scale);
        representation.setSize(size);
        let image = NSImage::initWithSize(NSImage::alloc(), size);
        image.addRepresentation(&representation);
        let view = NSImageView::imageViewWithImage(&image, self.mtm);

        let area = self
            .screen()
            .map(|screen| screen.visibleFrame())
            .unwrap_or(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(1440.0, 900.0),
            ));
        // AppKit's origin is bottom left.
        let origin = NSPoint::new(
            area.origin.x + (area.size.width - size.width) / 2.0,
            area.origin.y + area.size.height - TOP_MARGIN - size.height,
        );
        self.panel.setContentView(Some(&view));
        self.panel.setFrame_display(NSRect::new(origin, size), true);
        self.set_opacity(0.0);
        self.panel.orderFrontRegardless();
        Ok(())
    }

    pub fn set_opacity(&mut self, opacity: f32) {
        self.panel.setAlphaValue(f64::from(opacity.clamp(0.0, 1.0)));
    }

    pub fn hide(&mut self) {
        self.panel.orderOut(None);
    }
}
