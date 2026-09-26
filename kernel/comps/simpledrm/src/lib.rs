// SPDX-License-Identifier: MPL-2.0

//! A simpledrm driver backed by the bootloader-provided framebuffer.
//!
//! It obtains framebuffer information from `aster-framebuffer` and registers
//! the resulting DRM device with `aster-drm`.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "simpledrm: "
    };
}

use alloc::sync::Arc;
use core::fmt::Debug;

use aster_core::prelude::*;
use aster_drm::device::{DrmDevice, DrmFeatures};
use aster_framebuffer::{framebuffer, framebuffer::FrameBuffer};
use component::{ComponentInitError, init_component};

const SIMPLEDRM_NAME: &str = "simpledrm";
const SIMPLEDRM_DESC: &str = "DRM driver for simple-framebuffer platform devices";

#[init_component(process)]
fn init() -> Result<(), ComponentInitError> {
    if framebuffer::FRAMEBUFFER.get().is_none() {
        ostd::warn!("Failed to init: boot framebuffer is unavailable");
        return Ok(());
    };

    let device = match SimpleDrmDevice::new() {
        Ok(device) => device,
        Err(err) => {
            ostd::warn!("Failed to create device: {:?}", err);
            return Ok(());
        }
    };

    // The card sits on the firmware platform device, like Linux's
    // simple-framebuffer.0/drm/card0.
    let parent = aster_core::device::simple_framebuffer_platform_device();
    if let Err(err) = aster_drm::register_device(Arc::new(device), parent) {
        ostd::warn!("Failed to register device: {:?}", err);
    }

    Ok(())
}

#[derive(Debug)]
struct SimpleDrmDevice {
    features: DrmFeatures,
}

impl SimpleDrmDevice {
    fn new() -> Result<Self> {
        // The device renders through the boot framebuffer, whose dumb-buffer
        // aliasing provides minimal kernel mode-setting.
        Ok(Self {
            features: DrmFeatures::MODESET | DrmFeatures::RENDER,
        })
    }
}

impl DrmDevice for SimpleDrmDevice {
    fn name(&self) -> &str {
        SIMPLEDRM_NAME
    }

    fn desc(&self) -> &str {
        SIMPLEDRM_DESC
    }

    fn features(&self) -> &DrmFeatures {
        &self.features
    }

    fn scanout(&self) -> Option<Arc<FrameBuffer>> {
        framebuffer::FRAMEBUFFER.get().cloned()
    }
}
