// SPDX-License-Identifier: MPL-2.0

use super::TidDirOps;
use crate::{
    fs::{
        file::mkmod,
        procfs::template::{ProcSym, ProcSymOps},
        vfs::inode::{Inode, SymbolicLink},
    },
    prelude::*,
    process::posix_thread::AsPosixThread,
    thread::Thread,
};

/// Represents the inode at `/proc/[pid]/task/[tid]/cwd` (and also `/proc/[pid]/cwd`).
pub(super) struct CwdSymOps(TidDirOps);

impl CwdSymOps {
    pub(super) fn new_inode(dir: &TidDirOps, parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference:
        // <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/base.c#L3350>
        // <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/base.c#L176-L177>
        ProcSym::new(Self(dir.clone()), parent, mkmod!(a+rwx))
    }
}

impl ProcSymOps for CwdSymOps {
    fn owner_thread(&self) -> Option<Arc<Thread>> {
        self.0.thread()
    }

    fn read_link(&self) -> Result<SymbolicLink> {
        let Some((thread, _process)) = self.0.thread_and_process() else {
            return_errno_with_message!(Errno::ESRCH, "the process does not exist");
        };

        let posix_thread = thread.as_posix_thread().unwrap();
        let fs = posix_thread.read_fs();
        let path = fs.resolver().read().cwd().clone();

        Ok(SymbolicLink::Path(path))
    }
}
