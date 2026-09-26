// SPDX-License-Identifier: MPL-2.0

mod evdev;
mod fb;
mod mem;
pub(crate) mod misc;
mod model;
mod pty;
pub mod registry;
pub(crate) mod tty;

pub use fb::simple_framebuffer_platform_device;

use device_id::DeviceId;
pub(crate) use mem::{getrandom, geturandom};
pub(crate) use pty::{PtyMaster, PtySlave, new_pty_pair};
pub(crate) use registry::lookup;

use crate::{
    fs::{devtmpfs::DevtmpfsNodeMeta, file::PerOpenFileOps},
    prelude::*,
};

/// The abstraction of a device.
pub trait Device: Send + Sync + 'static {
    /// Returns the device type.
    fn type_(&self) -> DeviceType;

    /// Returns the device ID.
    fn id(&self) -> DeviceId;

    /// Returns the metadata that specifies a device inode to be created in devtmpfs, if any.
    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta>;

    /// Opens the device, returning a file-like object that the userspace can interact with by
    /// doing I/O.
    fn open(&self) -> Result<Box<dyn PerOpenFileOps>>;
}

impl Debug for dyn Device {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("type", &self.type_())
            .field("id", &self.id())
            .field("devtmpfs_meta", &self.devtmpfs_meta())
            .finish_non_exhaustive()
    }
}

/// Device type
#[derive(Clone, Copy, Debug)]
pub enum DeviceType {
    Char,
    Block,
}

pub(crate) fn init_in_first_kthread() {
    // `devtmpfsd` has been spawned by `fs::init_in_first_kthread`, so the
    // device model may now create device nodes.
    model::install_hooks();
    registry::init_in_first_kthread();
    mem::init_in_first_kthread();
    misc::init_in_first_kthread();
    evdev::init_in_first_kthread();
    fb::init_in_first_kthread();
}

/// The virtual `platform` bus hosting devices that Linux would attach to a
/// platform device (framebuffers, i8042 input devices, and so on). Devices on
/// this bus get the `device/subsystem` sysfs chain that user space expects.
pub(super) mod platform {
    use alloc::sync::Arc;

    use aster_device::{Bus, BusDevice, BusHandle};
    use spin::Once;

    /// The `platform` bus.
    #[expect(dead_code)]
    pub(super) struct PlatformBus;

    impl Bus for PlatformBus {
        const NAME: &'static str = "platform";
        type Device = ();
        type MatchData = ();

        fn matches(&self, _: &(), _: &()) -> bool {
            false
        }
    }

    /// Returns the registered platform bus, registering it on first use.
    #[expect(dead_code)]
    pub(super) fn bus() -> &'static Arc<BusHandle<PlatformBus>> {
        static BUS: Once<Arc<BusHandle<PlatformBus>>> = Once::new();
        BUS.call_once(|| {
            aster_device::register_bus(PlatformBus).expect("failed to register the platform bus")
        })
    }

    /// Returns the shared parent device for virtual peripherals, creating it
    /// on first use.
    #[expect(dead_code)]
    pub(super) fn parent() -> Arc<BusDevice<PlatformBus>> {
        static PARENT: Once<Arc<BusDevice<PlatformBus>>> = Once::new();
        PARENT
            .call_once(|| {
                let parent = BusDevice::builder(bus(), "virtual-peripherals", ()).build();
                aster_device::add(&parent).expect("failed to add the platform parent device");
                parent
            })
            .clone()
    }
}

/// Initializes device state after mounting rootfs.
pub(crate) fn init_in_first_process() -> Result<()> {
    tty::init_in_first_process()?;
    registry::init_in_first_process()?;

    Ok(())
}
