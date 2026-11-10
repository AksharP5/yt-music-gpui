use anyhow::{Context, Result, ensure};
use gpui::{Bounds, Pixels, Window};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use std::{
    ffi::{c_ulong, c_void},
    ptr::NonNull,
    sync::{Mutex, TryLockError},
};

unsafe extern "C" {
    fn video_initialize() -> i32;
    fn video_create(display: *mut c_void, parent: *mut c_void) -> *mut c_void;
    fn video_create_x11(parent: c_ulong) -> *mut c_void;
    fn video_start(video: *mut c_void, core: *mut c_void) -> i32;
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

/// Call before GPUI or libmpv starts: hardware decoders can also use Xlib.
pub fn initialize() -> Result<()> {
    ensure!(
        unsafe { video_initialize() } != 0,
        "Could not initialize native video threading"
    );
    Ok(())
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
    fn new(bounds: Bounds<Pixels>, scale: f32, x11: bool) -> Result<Self> {
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
        // Wayland positions logical pixels; X11 positions physical pixels.
        let coordinates = if x11 { scale } else { 1. };
        let edges = [x, y, x + width, y + height].map(|edge| (edge * coordinates).round());
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
        let buffer_scale = if x11 { 1. } else { scale };
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
    x11: bool,
}

// All calls into this handle are serialized. Only start/stop cross to the actor;
// creation and geometry use the live window; close/quit fence background cleanup.
unsafe impl Send for NativeVideo {}
unsafe impl Sync for NativeVideo {}

impl NativeVideo {
    pub(crate) fn new(bounds: Bounds<Pixels>, window: &Window) -> Result<Self> {
        let display = HasDisplayHandle::display_handle(window)?.as_raw();
        let handle = HasWindowHandle::window_handle(window)?.as_raw();
        let (handle, x11) = match (display, handle) {
            (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(handle)) => (
                unsafe { video_create(display.display.as_ptr(), handle.surface.as_ptr()) },
                false,
            ),
            (RawDisplayHandle::Xcb(_), RawWindowHandle::Xcb(handle)) => (
                unsafe { video_create_x11(handle.window.get().into()) },
                true,
            ),
            _ => anyhow::bail!("Native video requires Wayland or X11"),
        };
        let handle = NonNull::new(handle).context("Could not create the native video surface")?;
        let video = Self {
            handle: Mutex::new(Some(handle)),
            x11,
        };
        video.set_bounds(bounds, window.scale_factor())?;
        Ok(video)
    }

    pub(crate) fn start(&self, core: *mut c_void) -> Result<()> {
        let handle = self.handle.lock().unwrap();
        let handle = handle.context("The video surface was detached")?;
        ensure!(
            unsafe { video_start(handle.as_ptr(), core) } != 0,
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
        let geometry = Geometry::new(bounds, scale, self.x11)?;
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

    pub(crate) fn stop(&self) {
        if let Some(handle) = *self.handle.lock().unwrap() {
            unsafe { video_stop(handle.as_ptr()) };
        }
    }

    pub(crate) fn destroy(&self) {
        if let Some(handle) = self.handle.lock().unwrap().take() {
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

    #[test]
    fn framebuffer_uses_current_fractional_scale() {
        let bounds = Bounds::new(point(px(190.), px(64.)), size(px(853.), px(499.)));
        let geometry = Geometry::new(bounds, 1.25, false).unwrap();
        assert_eq!((geometry.width, geometry.height), (853, 499));
        assert_eq!((geometry.pixel_width, geometry.pixel_height), (1066, 624));
        let changed = Geometry::new(bounds, 1.5, false).unwrap();
        assert_eq!((changed.pixel_width, changed.pixel_height), (1280, 749));
        let x11 = Geometry::new(bounds, 1.25, true).unwrap();
        assert_eq!((x11.x, x11.y), (238, 80));
        assert_eq!((x11.pixel_width, x11.pixel_height), (1066, 624));
        let fractional = Bounds::new(point(px(0.3), px(0.3)), size(px(10.2), px(10.2)));
        let x11 = Geometry::new(fractional, 1.5, true).unwrap();
        assert_eq!((x11.x, x11.pixel_width), (0, 16));
        assert!(Geometry::new(bounds, f32::NAN, false).is_err());
    }
}
