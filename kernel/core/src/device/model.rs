// SPDX-License-Identifier: MPL-2.0

//! The kernel side of the device model: the hooks that give the
//! `aster-device` component access to devtmpfs and to uevent delivery.

use alloc::format;

use aster_device::{DevKind, DevNodeRequest, HookError, KernelHooks, Uevent};

use crate::{
    device::DeviceType,
    fs::{
        devtmpfs::{self, DevtmpfsNode, DevtmpfsNodeMeta},
        file::InodeMode,
    },
    prelude::*,
};

struct Hooks;

fn to_devtmpfs_node(request: &DevNodeRequest) -> Result<DevtmpfsNode> {
    let device_type = match request.devnum.kind() {
        DevKind::Char => DeviceType::Char,
        DevKind::Block => DeviceType::Block,
    };
    let mode = InodeMode::from_bits(request.mode)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "invalid device node mode"))?;
    let meta = DevtmpfsNodeMeta::with_mode(request.path.clone(), mode)
        .map_err(|_| Error::with_message(Errno::EINVAL, "invalid device node path"))?;
    Ok(DevtmpfsNode::new(device_type, request.devnum.id(), meta))
}

impl KernelHooks for Hooks {
    fn create_devnode(&self, request: &DevNodeRequest) -> core::result::Result<(), HookError> {
        let node = to_devtmpfs_node(request).map_err(|_| HookError)?;
        devtmpfs::create_node(node).map_err(|error| {
            warn!(
                "failed to create devtmpfs node {:?}: {:?}",
                request.path, error
            );
            HookError
        })
    }

    fn delete_devnode(&self, request: &DevNodeRequest) -> core::result::Result<(), HookError> {
        let node = to_devtmpfs_node(request).map_err(|_| HookError)?;
        devtmpfs::delete_node(node).map_err(|error| {
            warn!(
                "failed to delete devtmpfs node {:?}: {:?}",
                request.path, error
            );
            HookError
        })
    }

    fn broadcast_uevent(&self, event: &Uevent) {
        // Deliver the event through the `NETLINK_KOBJECT_UEVENT` socket
        // family, multicasting to group 1 where `udevd` listens.
        let devpath = format!("/{}", event.devpath());
        let envs: Vec<(String, String)> = event.vars().vars().to_vec();
        let result = crate::net::socket::netlink::broadcast_device_uevent(
            &event.action().to_string(),
            &devpath,
            event.subsystem(),
            envs,
        );
        if let Err(error) = result {
            warn!("failed to broadcast uevent for {}: {:?}", devpath, error);
        }
    }
}

/// Installs the kernel hooks. Must run once `devtmpfsd` is running.
pub(super) fn install_hooks() {
    aster_device::install_hooks(Arc::new(Hooks));
}
