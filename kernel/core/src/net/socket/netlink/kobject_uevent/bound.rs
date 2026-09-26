// SPDX-License-Identifier: MPL-2.0

use super::message::UeventMessage;
use crate::{
    events::IoEvents,
    net::socket::{
        netlink::{
            NetlinkSocketAddr,
            common::BoundNetlink,
            table::{NetlinkUeventProtocol, SupportedNetlinkProtocol},
        },
        unix::CUserCred,
        util::{RecvFlags, RecvOutput, SendFlags, datagram_common},
    },
    prelude::*,
    process::posix_thread::AsPosixThread,
    util::{MultiRead, MultiWrite},
};

pub(super) type BoundNetlinkUevent = BoundNetlink<UeventMessage>;

impl datagram_common::Bound for BoundNetlinkUevent {
    type Endpoint = NetlinkSocketAddr;

    fn local_endpoint(&self) -> Self::Endpoint {
        self.handle.addr()
    }

    fn bind(&mut self, endpoint: &Self::Endpoint) -> Result<()> {
        self.bind_common(endpoint)
    }

    fn remote_endpoint(&self) -> Option<&Self::Endpoint> {
        Some(&self.remote_addr)
    }

    fn set_remote_endpoint(&mut self, endpoint: &Self::Endpoint) {
        self.remote_addr = *endpoint;
    }

    fn try_send(
        &self,
        reader: &mut dyn MultiRead,
        remote: &Self::Endpoint,
        flags: SendFlags,
    ) -> Result<usize> {
        // TODO: Deal with flags
        if !flags.is_all_supported() {
            warn!("unsupported flags: {:?}", flags);
        }

        let total = reader.sum_lens();

        // A zero port addresses the kernel socket, which has nothing to do
        // with the message; ignore it and report success, as before.
        if remote.port() == 0 {
            return Ok(total);
        }

        // Unicast to another user-space netlink socket. This is how udevd
        // relays a uevent to its worker processes: each worker's monitor
        // binds its own port and the manager sends the message there.
        let mut data = vec![0u8; total];
        let mut writer = VmWriter::from(data.as_mut_slice());
        let copied = reader
            .read(&mut writer)
            .map_err(|(err, _)| Error::from(err))?;
        data.truncate(copied);

        let cred = {
            let thread = current_thread!();
            let credentials = thread.as_posix_thread().unwrap().credentials();
            CUserCred::new(current!().pid(), credentials.ruid(), credentials.rgid())
        };
        let message = UeventMessage::from_bytes(data, self.handle.addr(), cred);
        NetlinkUeventProtocol::unicast(remote.port(), message)?;

        Ok(total)
    }

    fn try_recv(
        &self,
        writer: &mut dyn MultiWrite,
        flags: RecvFlags,
    ) -> Result<(RecvOutput, Self::Endpoint)> {
        // TODO: Deal with other flags.
        if !flags.is_all_supported() {
            warn!("unsupported flags: {:?}", flags);
        }

        let mut receive_queue = self.receive_queue.lock();

        receive_queue.dequeue_if(|response, response_len| {
            let copied_len = response_len.min(writer.sum_lens());
            response.write_to(writer)?;

            let remote = *response.src_addr();
            *self.last_recv_cred.lock() = *response.cred();

            let should_dequeue = flags.receive_behavior().will_consume_data();
            let output = RecvOutput::new_for_packet(flags, copied_len, response_len);
            Ok((should_dequeue, (output, remote)))
        })
    }

    fn check_io_events(&self) -> IoEvents {
        self.check_io_events_common()
    }
}
