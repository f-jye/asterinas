// SPDX-License-Identifier: MPL-2.0

//! The Direct Rendering Manager subsystem of Asterinas.
//!
//! This crate provides the kernel-side framework for exposing graphics devices
//! through the Linux-compatible DRM userspace API. It sits between graphics
//! devices and the character-device layer, providing the shared object model,
//! lifecycle management, permission checks, and ioctl infrastructure needed by
//! DRM devices.
//!
//! Graphics devices implement [`device::DrmDevice`] and the relevant operation
//! traits to supply hardware-specific behavior. The DRM core owns the common
//! userspace-facing semantics and coordinates access to the device, keeping
//! policy and ABI handling independent of individual device implementations.

#![no_std]
#![deny(unsafe_code)]

use alloc::{boxed::Box, format, sync::Arc};

use aster_core::{
    device::{Device, DeviceType, registry::char},
    fs::{devtmpfs::DevtmpfsNodeMeta, file::PerOpenFileOps},
    prelude::*,
    process::{UserNamespace, credentials::capabilities::CapSet, posix_thread::AsPosixThread},
    security::lsm::hooks::{self as lsm_hook, CapableContext},
};
use aster_device::{
    AnyDevice, Class, ClassDevice, ClassHandle, DevNode, DevNum, SysStr, UeventVars,
};
use device_id::{DeviceId, MajorId, MinorId};
use ostd::{sync::Mutex, task::Task};

use crate::{
    device::{DrmDevice, DrmFeatures, RegisteredDrmDevice},
    minor::{DrmMinor, DrmMinorType},
};

extern crate alloc;
#[macro_use]
extern crate ostd_pod;

// Sets this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "drm: "
    };
}

pub mod device;
mod file;
mod gem;
mod ioctl;
mod minor;

pub fn register_device(
    device: Arc<dyn DrmDevice>,
    parent: Option<Arc<dyn AnyDevice>>,
) -> Result<()> {
    let registered_device = Arc::new(RegisteredDrmDevice::new(device)?);
    if registered_device
        .device()
        .has_features(DrmFeatures::MODESET)
        && registered_device.device().scanout().is_some()
    {
        registered_device.spawn_scanout_refresh();
    }
    let render_minor = if registered_device.device().has_features(DrmFeatures::RENDER) {
        let minor = DrmMinor::new(registered_device.clone(), DrmMinorType::Render);
        char::register(minor.clone())?;
        Some(minor)
    } else {
        None
    };

    let primary_minor = DrmMinor::new(registered_device, DrmMinorType::Primary);

    let mut card = ClassDevice::builder(&drm_class(), "card0", primary_minor.clone())
        .devnum(DevNum::char(DeviceId::new(
            MajorId::new(DRM_MAJOR_ID),
            MinorId::new(DRM_PRIMARY_MINOR_BASE),
        )));
    // The card hangs off the underlying hardware device, as Linux's
    // `simple-framebuffer.0/drm/card0` does; libdrm's `drmGetDevice` walks
    // this sysfs chain, and without it mesa cannot identify the device.
    if let Some(parent) = parent {
        card = card.parent(parent);
    }
    let card = card.build();

    // The device model publishes sysfs topology (class dir, subsystem link,
    // dev attribute, /sys/dev/char entry) and creates the /dev node itself,
    // so the char registry only wires `open` to the minor.
    if let Err(error) = aster_device::add(&card) {
        if let Some(render_minor) = render_minor {
            let _ = char::unregister(render_minor.id());
        }
        return Err(Error::from(error));
    }

    // The char registry routes `open` back to the minor.
    char::register(Arc::new(DrmCard(card)))?;

    Ok(())
}

fn drm_class() -> Arc<ClassHandle<DrmClass>> {
    static DRM_CLASS: Mutex<Option<Arc<ClassHandle<DrmClass>>>> = Mutex::new(None);
    DRM_CLASS
        .lock()
        .get_or_insert_with(|| {
            aster_device::register_class(DrmClass)
                .expect("the `drm` class must not be registered twice")
        })
        .clone()
}
const DRM_MAJOR_ID: u16 = 226;
const DRM_PRIMARY_MINOR_BASE: u32 = 0;

/// The `drm` class: display devices published through the DRM subsystem,
/// placed under `/sys/devices/virtual/drm/` and exposed as `/dev/dri/card0`.
struct DrmClass;

impl Class for DrmClass {
    const NAME: &'static str = "drm";
    type Device = Arc<DrmMinor>;

    fn devnode(&self, dev: &ClassDevice<Self>) -> Option<DevNode> {
        Some(DevNode {
            path: Some(SysStr::from(format!("dri/{}", dev.base().name()))),
            mode: None,
        })
    }

    fn uevent(&self, _dev: &ClassDevice<Self>, vars: &mut UeventVars) {
        // The Linux device type of a DRM card minor. Mutter's native backend
        // only treats udev devices carrying this type as GPUs.
        // Reference: <https://elixir.bootlin.com/linux/v6.17/source/drivers/gpu/drm/drm_sysfs.c#L56>
        vars.add("DEVTYPE", "drm_minor");
    }
}

/// A newtype that lets the char registry open DRM files for a card device
/// published through the device model (orphan rule: `ClassDevice` is foreign).
struct DrmCard(Arc<ClassDevice<DrmClass>>);

impl Device for DrmCard {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        self.0.payload().id()
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        // The device model creates the node when the device is added.
        None
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        self.0.payload().open()
    }
}

fn has_current_sys_admin() -> bool {
    let task = Task::current().unwrap();
    let posix_thread = task.as_posix_thread().unwrap();

    lsm_hook::on_capable(CapableContext::new(
        UserNamespace::get_init_singleton().as_ref(),
        posix_thread,
        CapSet::SYS_ADMIN,
    ))
    .is_ok()
}
