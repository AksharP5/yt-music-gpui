use crate::os::Waker;
use anyhow::{Context, Result, ensure};
use gpui::{Bounds, Pixels, Window};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
#[cfg(target_os = "linux")]
use std::ffi::c_ulong;
use std::{
    ffi::c_void,
    os::fd::{AsFd, AsRawFd},
    ptr::NonNull,
    sync::{Mutex, PoisonError, TryLockError},
};

// native/video.c on Linux, native/video_macos.m on macOS.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn video_initialize() -> i32;
    fn video_create(display: *mut c_void, parent: *mut c_void) -> *mut c_void;
    fn video_create_x11(parent: c_ulong) -> *mut c_void;
    fn video_set_cutouts(video: *mut c_void, rects: *const i32, count: i32) -> i32;
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn video_create(app_view: *mut c_void) -> *mut c_void;
}

unsafe extern "C" {
    fn video_start(video: *mut c_void, core: *mut c_void, wake: i32) -> i32;
    fn video_resize(
        video: *mut c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        pixel_width: i32,
        pixel_height: i32,
    ) -> i32;
    fn video_status(video: *mut c_void) -> i32;
    fn video_stop(video: *mut c_void);
    fn video_destroy(video: *mut c_void);
}

pub const SUPPORTED: bool = true;

/// `VIDEO_CUTOUTS` in native/video.c.
#[cfg(target_os = "linux")]
const CUTOUTS: usize = 4;

/// Call before GPUI or libmpv starts: hardware decoders can also use Xlib.
#[cfg(target_os = "linux")]
pub fn initialize() -> Result<()> {
    ensure!(
        unsafe { video_initialize() } != 0,
        "Could not initialize native video threading"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn initialize() -> Result<()> {
    Ok(())
}

/// Where the platform puts the video, which decides how it is positioned.
#[derive(Clone, Copy)]
enum Placement {
    /// A subsurface below the window, positioned in logical pixels.
    #[cfg(target_os = "linux")]
    Wayland,
    /// A child window above the app, positioned in physical pixels.
    #[cfg(target_os = "linux")]
    X11,
    /// A view below the app's view, positioned in physical pixels rounded
    /// outward, so it covers every pixel of the hole the transparent window
    /// leaves. The app draws over any overlap.
    #[cfg(target_os = "macos")]
    AppKit,
}

#[derive(Debug, PartialEq)]
struct Geometry {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    pixel_width: i32,
    pixel_height: i32,
}

impl Geometry {
    fn new(bounds: Bounds<Pixels>, scale: f32, placement: Placement) -> Result<Self> {
        let x = f32::from(bounds.origin.x);
        let y = f32::from(bounds.origin.y);
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        ensure!(
            [x, y, width, height, scale]
                .iter()
                .all(|value| value.is_finite())
                && width > 0.
                && height > 0.
                && scale > 0.,
            "Invalid video bounds"
        );
        let (coordinates, buffer_scale) = match placement {
            #[cfg(target_os = "linux")]
            Placement::Wayland => (1., scale),
            #[cfg(target_os = "linux")]
            Placement::X11 => (scale, 1.),
            #[cfg(target_os = "macos")]
            Placement::AppKit => (scale, 1.),
        };
        let [left, top, right, bottom] =
            [x, y, x + width, y + height].map(|edge| edge * coordinates);
        let edges = match placement {
            #[cfg(target_os = "linux")]
            Placement::Wayland | Placement::X11 => [left, top, right, bottom].map(f32::round),
            #[cfg(target_os = "macos")]
            Placement::AppKit => [left.floor(), top.floor(), right.ceil(), bottom.ceil()],
        };
        ensure!(
            edges.iter().all(|edge| edge.is_finite()
                && *edge >= i32::MIN as f32
                && *edge < i32::MAX as f32),
            "Video bounds are too large"
        );
        let [left, top, right, bottom] = edges.map(|edge| edge as i32);
        ensure!(right > left && bottom > top, "Video bounds are empty");
        let width = right
            .checked_sub(left)
            .context("Video bounds are too large")?;
        let height = bottom
            .checked_sub(top)
            .context("Video bounds are too large")?;
        let pixels = [width, height].map(|value| (value as f32 * buffer_scale).round().max(1.));
        ensure!(
            pixels
                .iter()
                .all(|value| value.is_finite() && *value < i32::MAX as f32),
            "Video framebuffer is too large"
        );
        Ok(Self {
            x: left,
            y: top,
            width,
            height,
            pixel_width: pixels[0] as i32,
            pixel_height: pixels[1] as i32,
        })
    }
}

pub(crate) struct NativeVideo {
    handle: Mutex<Option<NonNull<c_void>>>,
    placement: Placement,
}

// All calls into this handle are serialized. Only start/stop cross to the actor;
// creation and geometry use the live window; close/quit fence background cleanup.
unsafe impl Send for NativeVideo {}
unsafe impl Sync for NativeVideo {}

impl NativeVideo {
    pub(crate) fn new(bounds: Bounds<Pixels>, window: &Window) -> Result<Self> {
        let display = HasDisplayHandle::display_handle(window)?.as_raw();
        let handle = HasWindowHandle::window_handle(window)?.as_raw();
        let (handle, placement) = match (display, handle) {
            #[cfg(target_os = "linux")]
            (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(handle)) => (
                unsafe { video_create(display.display.as_ptr(), handle.surface.as_ptr()) },
                Placement::Wayland,
            ),
            #[cfg(target_os = "linux")]
            (RawDisplayHandle::Xcb(_), RawWindowHandle::Xcb(handle)) => (
                unsafe { video_create_x11(handle.window.get().into()) },
                Placement::X11,
            ),
            #[cfg(target_os = "macos")]
            (RawDisplayHandle::AppKit(_), RawWindowHandle::AppKit(handle)) => (
                unsafe { video_create(handle.ns_view.as_ptr()) },
                Placement::AppKit,
            ),
            _ => anyhow::bail!("Native video requires Wayland, X11 or macOS"),
        };
        let handle = NonNull::new(handle).context("Could not create the native video surface")?;
        let video = Self {
            handle: Mutex::new(Some(handle)),
            placement,
        };
        video.set_bounds(bounds, window.scale_factor())?;
        Ok(video)
    }

    pub(crate) fn start(&self, core: *mut c_void, wake: &Waker) -> Result<()> {
        let handle = self.handle.lock().unwrap_or_else(PoisonError::into_inner);
        let handle = handle.context("The video surface was detached")?;
        ensure!(
            unsafe { video_start(handle.as_ptr(), core, wake.as_fd().as_raw_fd()) } != 0,
            "Could not attach the video renderer"
        );
        Ok(())
    }

    pub(crate) fn ready(&self) -> Result<bool> {
        let handle = match self.handle.try_lock() {
            Ok(handle) => handle,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(_)) => anyhow::bail!("The video renderer stopped"),
        };
        Self::read_status(*handle)
    }

    pub(crate) fn ready_for_player(&self) -> Result<bool> {
        // The actor must observe the last status notification even during a resize.
        let handle = self
            .handle
            .lock()
            .map_err(|_| anyhow::anyhow!("The video renderer stopped"))?;
        Self::read_status(*handle)
    }

    fn read_status(handle: Option<NonNull<c_void>>) -> Result<bool> {
        let handle = handle.context("The video surface was detached")?;
        match unsafe { video_status(handle.as_ptr()) } {
            0 | 1 => Ok(false),
            2 => Ok(true),
            -1 => anyhow::bail!("Could not initialize the video renderer"),
            -2 => anyhow::bail!("The video renderer stopped. Switch to Audio and try Video again."),
            -3 => anyhow::bail!("The video surface stopped. Switch to Audio and try Video again."),
            -4 => anyhow::bail!("The video display connection stopped"),
            _ => anyhow::bail!("The video renderer is no longer attached"),
        }
    }

    pub(crate) fn set_bounds(&self, bounds: Bounds<Pixels>, scale: f32) -> Result<()> {
        let geometry = Geometry::new(bounds, scale, self.placement)?;
        let handle = match self.handle.try_lock() {
            Ok(handle) => handle,
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Poisoned(_)) => anyhow::bail!("The video renderer stopped"),
        };
        let Some(handle) = *handle else { return Ok(()) };
        ensure!(
            unsafe {
                video_resize(
                    handle.as_ptr(),
                    geometry.x,
                    geometry.y,
                    geometry.width,
                    geometry.height,
                    geometry.pixel_width,
                    geometry.pixel_height,
                )
            } != 0,
            "Could not resize the video surface"
        );
        Ok(())
    }

    /// X11 draws the video window above the app, so it leaves out the given window
    /// rectangles, where dialogs and menus cover the video. Wayland and macOS place
    /// the video below the window, where they already draw over it.
    #[cfg(target_os = "macos")]
    pub(crate) fn set_cutouts(&self, _: &[Bounds<Pixels>], _: f32) {}

    #[cfg(target_os = "linux")]
    pub(crate) fn set_cutouts(&self, cutouts: &[Bounds<Pixels>], scale: f32) {
        if !matches!(self.placement, Placement::X11) {
            return;
        }
        let rects: Vec<i32> = cutouts
            .iter()
            .filter_map(|bounds| Geometry::new(*bounds, scale, Placement::X11).ok())
            .take(CUTOUTS)
            .flat_map(|cutout| [cutout.x, cutout.y, cutout.width, cutout.height])
            .collect();
        let Ok(handle) = self.handle.try_lock() else {
            return;
        };
        if let Some(handle) = *handle {
            unsafe { video_set_cutouts(handle.as_ptr(), rects.as_ptr(), rects.len() as i32 / 4) };
        }
    }

    pub(crate) fn stop(&self) {
        if let Some(handle) = *self.handle.lock().unwrap_or_else(PoisonError::into_inner) {
            unsafe { video_stop(handle.as_ptr()) };
        }
    }

    pub(crate) fn destroy(&self) {
        if let Some(handle) = self
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            unsafe { video_destroy(handle.as_ptr()) };
        }
    }
}

impl Drop for NativeVideo {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    #[cfg(target_os = "linux")]
    #[test]
    fn framebuffer_uses_current_fractional_scale() {
        let bounds = Bounds::new(point(px(190.), px(64.)), size(px(853.), px(499.)));
        let geometry = Geometry::new(bounds, 1.25, Placement::Wayland).unwrap();
        assert_eq!((geometry.width, geometry.height), (853, 499));
        assert_eq!((geometry.pixel_width, geometry.pixel_height), (1066, 624));
        let changed = Geometry::new(bounds, 1.5, Placement::Wayland).unwrap();
        assert_eq!((changed.pixel_width, changed.pixel_height), (1280, 749));
        let x11 = Geometry::new(bounds, 1.25, Placement::X11).unwrap();
        assert_eq!((x11.x, x11.y), (238, 80));
        assert_eq!((x11.pixel_width, x11.pixel_height), (1066, 624));
        let fractional = Bounds::new(point(px(0.3), px(0.3)), size(px(10.2), px(10.2)));
        let x11 = Geometry::new(fractional, 1.5, Placement::X11).unwrap();
        assert_eq!((x11.x, x11.pixel_width), (0, 16));
        assert!(Geometry::new(bounds, f32::NAN, Placement::Wayland).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn view_covers_every_pixel_of_a_fractional_hole() {
        let bounds = Bounds::new(point(px(100.25), px(40.75)), size(px(200.5), px(100.3)));
        let geometry = Geometry::new(bounds, 2., Placement::AppKit).unwrap();
        // The hole spans pixels 200.5 to 601.5 and 81.5 to 282.1.
        assert_eq!((geometry.x, geometry.y), (200, 81));
        assert_eq!((geometry.width, geometry.height), (402, 202));
        assert_eq!((geometry.pixel_width, geometry.pixel_height), (402, 202));
        assert!(Geometry::new(bounds, f32::NAN, Placement::AppKit).is_err());
    }
}
