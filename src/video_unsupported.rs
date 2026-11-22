//! Windows has no native video renderer yet, so there the app plays audio only.
//! A `NativeVideo` cannot exist, which leaves every video path unreachable.

use crate::os::Waker;
use anyhow::{Result, bail};
use gpui::{Bounds, Pixels, Window};
use std::ffi::c_void;

pub const SUPPORTED: bool = false;

pub fn initialize() -> Result<()> {
    Ok(())
}

pub(crate) enum NativeVideo {}

impl NativeVideo {
    pub(crate) fn new(_: Bounds<Pixels>, _: &Window) -> Result<Self> {
        bail!("Video on this system is coming soon")
    }

    pub(crate) fn start(&self, _: *mut c_void, _: &Waker) -> Result<()> {
        match *self {}
    }

    pub(crate) fn ready(&self) -> Result<bool> {
        match *self {}
    }

    pub(crate) fn ready_for_player(&self) -> Result<bool> {
        match *self {}
    }

    pub(crate) fn set_bounds(&self, _: Bounds<Pixels>, _: f32) -> Result<()> {
        match *self {}
    }

    pub(crate) fn set_cutouts(&self, _: &[Bounds<Pixels>], _: f32) {
        match *self {}
    }

    pub(crate) fn stop(&self) {
        match *self {}
    }

    pub(crate) fn destroy(&self) {
        match *self {}
    }
}
