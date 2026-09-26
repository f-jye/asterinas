// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use crate::{
    prelude::*,
    process::{
        Pid, kill,
        signal::{
            c_types::siginfo_t,
            constants::SI_TKILL,
            sig_num::SigNum,
            signals::{Signal, raw::RawSignal},
        },
    },
    syscall::SyscallReturn,
};

pub(super) fn sys_rt_sigqueueinfo(
    pid: u64,
    sig_num: u64,
    info_ptr: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let sig_num = SigNum::try_from(sig_num as u8)?;
    debug!("pid = {}, sig_num = {:?}", pid, sig_num);

    if pid.cast_signed() <= 0 {
        return_errno_with_message!(Errno::EINVAL, "non-positive PIDs are not valid");
    }
    let pid = pid as Pid;

    let mut siginfo = ctx.user_space().read_val::<siginfo_t>(info_ptr)?;
    if siginfo.si_signo != sig_num.as_u8() as i32 {
        return_errno_with_message!(
            Errno::EINVAL,
            "`siginfo.si_signo` does not match the specified signal number"
        );
    }

    // Following Linux, a sender may only pass user-space `si_code` values
    // (which are non-positive), unless the target is itself.
    let is_self = pid == ctx.process.pid();
    if !is_self && (siginfo.si_code >= 0 || siginfo.si_code == SI_TKILL as i32) {
        return_errno_with_message!(
            Errno::EPERM,
            "signals with kernel-generated `si_code` can only be sent to the current process"
        );
    }

    let signal = RawSignal::new(siginfo);
    kill(pid, Some(Box::new(signal) as Box<dyn Signal>), ctx)?;
    Ok(SyscallReturn::Return(0))
}
