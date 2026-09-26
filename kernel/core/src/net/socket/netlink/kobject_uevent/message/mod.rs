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
    payload: Vec<u8>,
    src_addr: NetlinkSocketAddr,
    /// The credentials the receiver should attribute to this message. Kernel
    /// broadcasts use the kernel identity (PID 0); messages relayed between
    /// user-space sockets carry the sender's real credentials, as Linux does.
    cred: CUserCred,
}

impl UeventMessage {
    /// Creates a new uevent message originated from the kernel.
    fn new(uevent: Uevent, src_addr: NetlinkSocketAddr) -> Self {
        Self {
            payload: uevent.to_string().into_bytes(),
            src_addr,
            cred: CUserCred::new_kernel(),
        }
    }

    /// Creates a message from the raw bytes sent by a user-space socket.
    pub(super) fn from_bytes(bytes: Vec<u8>, src_addr: NetlinkSocketAddr, cred: CUserCred) -> Self {
        Self {
            payload: bytes,
            src_addr,
            cred,
        }
    }

    /// Returns the source address of the uevent message.
    pub(super) fn src_addr(&self) -> &NetlinkSocketAddr {
        &self.src_addr
    }

    /// Returns the credentials attributed to this message.
    pub(super) fn cred(&self) -> &CUserCred {
        &self.cred
    }

    /// Writes the uevent to the given `writer`.
    pub(super) fn write_to(&self, writer: &mut dyn MultiWrite) -> Result<()> {
        let _nbytes = writer.write(&mut VmReader::from(self.payload.as_slice()))?;
        // `_nbytes` may be smaller than the message size. We ignore it to truncate the message.

        Ok(())
    }
}

impl QueueableMessage for UeventMessage {
    fn total_len(&self) -> usize {
        self.payload.len()
    }
}

impl MulticastMessage for UeventMessage {}
