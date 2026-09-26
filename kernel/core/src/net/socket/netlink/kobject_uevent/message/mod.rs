// SPDX-License-Identifier: MPL-2.0

use core::str::FromStr as _;

use uevent::{SysObjAction, Uevent};

use crate::{
    net::socket::netlink::{
        NetlinkSocketAddr,
        addr::GroupIdSet,
        receiver::QueueableMessage,
        table::{MulticastMessage, NetlinkUeventProtocol, SupportedNetlinkProtocol},
    },
    prelude::*,
    util::MultiWrite,
};

mod syn_uevent;
#[cfg(ktest)]
mod test;
mod uevent;

/// Broadcasts a device uevent to the group-1 listeners (`udevd`), as Linux's
/// `kobject_uevent_env` does.
///
/// `action` is one of "add", "remove", "change", "bind" or "unbind"; `devpath`
/// is the device path under sysfs with a leading `/`; `envs` carries the
/// `KEY=VALUE` variables (e.g. `SUBSYSTEM`, `DEVNAME`, `MAJOR`, `MINOR`).
pub fn broadcast_device_uevent(
    action: &str,
    devpath: &str,
    subsystem: &str,
    envs: Vec<(String, String)>,
) -> Result<()> {
    let sys_action = SysObjAction::from_str(action)?;
    let uevent = Uevent::new(sys_action, devpath.to_string(), subsystem.to_string(), envs);
    let message = UeventMessage::new(uevent, NetlinkSocketAddr::new(0, GroupIdSet::new(0x1)));
    NetlinkUeventProtocol::multicast(GroupIdSet::new(0x1), message)
}

/// A uevent message.
///
/// Note that uevent messages are not the same as common netlink messages.
/// It does not have a netlink header.
#[derive(Clone, Debug)]
pub(crate) struct UeventMessage {
    uevent: String,
    src_addr: NetlinkSocketAddr,
}

impl UeventMessage {
    /// Creates a new uevent message.
    fn new(uevent: Uevent, src_addr: NetlinkSocketAddr) -> Self {
        Self {
            uevent: uevent.to_string(),
            src_addr,
        }
    }

    /// Returns the source address of the uevent message.
    pub(super) fn src_addr(&self) -> &NetlinkSocketAddr {
        &self.src_addr
    }

    /// Writes the uevent to the given `writer`.
    pub(super) fn write_to(&self, writer: &mut dyn MultiWrite) -> Result<()> {
        let _nbytes = writer.write(&mut VmReader::from(self.uevent.as_bytes()))?;
        // `_nbytes` may be smaller than the message size. We ignore it to truncate the message.

        Ok(())
    }
}

impl QueueableMessage for UeventMessage {
    fn total_len(&self) -> usize {
        self.uevent.len()
    }
}

impl MulticastMessage for UeventMessage {}
