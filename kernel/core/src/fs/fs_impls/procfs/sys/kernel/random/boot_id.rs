// SPDX-License-Identifier: MPL-2.0

use alloc::format;

use aster_util::printer::VmPrinter;
use spin::Once;

use crate::{
    fs::{
        file::mkmod,
        procfs::template::{ProcFile, ProcFileOps},
        vfs::inode::Inode,
    },
    prelude::*,
    util::random::getrandom,
};

/// Represents the inode at `/proc/sys/kernel/random/boot_id`.
///
/// The file contains a random UUID generated once per boot. User space
/// (systemd, journald, mutter) uses it to identify the current boot.
pub(super) struct BootIdFileOps;

fn boot_id() -> &'static str {
    static BOOT_ID: Once<&'static str> = Once::new();
    BOOT_ID.call_once(|| {
        let mut bytes = [0u8; 16];
        getrandom(bytes.as_mut_slice());
        // Format as a UUID v4: set the version and variant bits.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;

        let hex: Vec<String> = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        let id = Box::leak(
            format!(
                "{}-{}-{}-{}-{}\n",
                hex[0..4].concat(),
                hex[4..6].concat(),
                hex[6..8].concat(),
                hex[8..10].concat(),
                hex[10..16].concat()
            )
            .into_boxed_str(),
        );
        id
    })
}

impl BootIdFileOps {
    pub(super) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference:
        // <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/proc_sysctl.c#L978>
        ProcFile::new(Self, parent, mkmod!(a+r))
    }
}

impl ProcFileOps for BootIdFileOps {
    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let mut printer = VmPrinter::new_skip(writer, offset);

        write!(printer, "{}", boot_id())?;

        Ok(printer.bytes_written())
    }
}
