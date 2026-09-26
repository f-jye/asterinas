// SPDX-License-Identifier: MPL-2.0

//! Minimal kernel mode-setting ioctls backed by the boot framebuffer.
//!
//! The scanout framebuffer doubles as the only dumb buffer: `CREATE_DUMB`
//! hands out an alias of it, `MAP_DUMB` maps offset zero, and `SETCRTC`
//! simply records the framebuffer that user space has bound to the CRTC.
//! Writes to the mapped buffer are therefore visible on the display without
//! any copy step.

use alloc::{format, sync::Arc};

use aster_core::{prelude::*, util::ioctl::write_user_value};
use aster_framebuffer::framebuffer::FrameBuffer;
use ostd::mm::HasSize;

use super::{
    super::device::{CONNECTOR_ID, CRTC_ID, ENCODER_ID, KmsFb},
    ioctl_defs::*,
};
use crate::file::DrmFile;

/// Modesetting ioctl argument layouts, transcribed from Linux's
/// `include/uapi/drm/drm_mode.h`.
pub(super) mod abi {
    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L252>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeCardRes {
        pub fb_id_ptr: u64,
        pub crtc_id_ptr: u64,
        pub connector_id_ptr: u64,
        pub encoder_id_ptr: u64,
        pub count_fbs: u32,
        pub count_crtcs: u32,
        pub count_connectors: u32,
        pub count_encoders: u32,
        pub min_width: u32,
        pub max_width: u32,
        pub min_height: u32,
        pub max_height: u32,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L232>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeModeInfo {
        pub clock: u32,
        pub hdisplay: u16,
        pub hsync_start: u16,
        pub hsync_end: u16,
        pub htotal: u16,
        pub hskew: u16,
        pub vdisplay: u16,
        pub vsync_start: u16,
        pub vsync_end: u16,
        pub vtotal: u16,
        pub vscan: u16,
        pub vrefresh: u32,
        pub flags: u32,
        pub type_: u32,
        pub name: [u8; 32],
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L267>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeCrtc {
        pub set_connectors_ptr: u64,
        pub count_connectors: u32,
        pub crtc_id: u32,
        pub fb_id: u32,
        pub x: u32,
        pub y: u32,
        pub gamma_size: u32,
        pub mode_valid: u32,
        pub mode: ModeModeInfo,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L448>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeGetConnector {
        pub encoders_ptr: u64,
        pub modes_ptr: u64,
        pub props_ptr: u64,
        pub prop_values_ptr: u64,
        pub count_modes: u32,
        pub count_props: u32,
        pub count_encoders: u32,
        pub encoder_id: u32,
        pub connector_id: u32,
        pub connector_type: u32,
        pub connector_type_id: u32,
        pub connection: u32,
        pub mm_width: u32,
        pub mm_height: u32,
        pub subpixel: u32,
        pub __pad: u32,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L365>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeGetEncoder {
        pub encoder_id: u32,
        pub encoder_type: u32,
        pub crtc_id: u32,
        pub possible_crtcs: u32,
        pub possible_clones: u32,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L646>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeFbCmd {
        pub fb_id: u32,
        pub width: u32,
        pub height: u32,
        pub pitch: u32,
        pub bpp: u32,
        pub depth: u32,
        pub handle: u32,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L1250>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeCreateDumb {
        pub height: u32,
        pub width: u32,
        pub bpp: u32,
        pub flags: u32,
        pub handle: u32,
        pub pitch: u32,
        pub size: u64,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L1262>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeMapDumb {
        pub handle: u32,
        pub pad: u32,
        pub offset: u64,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L625>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeObjGetProps {
        pub props_ptr: u64,
        pub prop_values_ptr: u64,
        pub count_props: u32,
        pub obj_id: u32,
        pub obj_type: u32,
        pub __pad: u32,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L1452>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeListLessees {
        pub count_lessees: u32,
        pub pad: u32,
        pub lessees_ptr: u64,
    }
}

impl DrmFile {
    /// Builds the single fixed display mode reported for the scanout.
    fn mode_info(scanout: &FrameBuffer) -> abi::ModeModeInfo {
        let (width, height) = (scanout.width() as u32, scanout.height() as u32);
        // A reduced-blanking-style mode whose only job is to be self-consistent
        // and match the scanout dimensions; the hardware cannot re-time anyway.
        let clock = width * height * 60 / 1000;
        let mut name = [0u8; 32];
        let text = format!("{}x{}", width, height);
        name[..text.len()].copy_from_slice(text.as_bytes());
        abi::ModeModeInfo {
            clock,
            hdisplay: width as u16,
            hsync_start: (width + 48) as u16,
            hsync_end: (width + 80) as u16,
            htotal: (width + 160) as u16,
            hskew: 0,
            vdisplay: height as u16,
            vsync_start: (height + 3) as u16,
            vsync_end: (height + 8) as u16,
            vtotal: (height + 40) as u16,
            vscan: 0,
            vrefresh: 60,
            flags: 0,
            // DRM_MODE_TYPE_DRIVER | DRM_MODE_TYPE_PREFERRED
            type_: 0x4 | 0x8,
            name,
        }
    }

    /// Writes one `u32` object id into a user-space id array.
    fn write_id(addr: usize, pos: usize, id: &u32) -> Result<()> {
        write_user_value(addr + pos * size_of::<u32>(), id)
    }

    fn scanout_or_err(&self) -> Result<Arc<FrameBuffer>> {
        self.minor()
            .device()
            .scanout()
            .ok_or_else(|| Error::with_message(Errno::EOPNOTSUPP, "the device has no scanout"))
    }

    pub(super) fn mode_get_resources(&self, cmd: DrmIoctlModeGetResources) -> Result<i32> {
        let scanout = self.scanout_or_err()?;
        let kms = self.minor().registered_device().kms().lock();
        let fbs = kms.fbs().lock();
        let mut args: abi::ModeCardRes = cmd.read()?;

        let user_count = args.count_fbs as usize;
        args.count_fbs = fbs.len() as u32;
        args.count_crtcs = 1;
        args.count_connectors = 1;
        args.count_encoders = 1;
        args.min_width = scanout.width() as u32;
        args.max_width = scanout.width() as u32;
        args.min_height = scanout.height() as u32;
        args.max_height = scanout.height() as u32;

        if args.fb_id_ptr != 0 {
            for (pos, fb_id) in fbs.keys().take(user_count).enumerate() {
                Self::write_id(args.fb_id_ptr as usize, pos, fb_id)?;
            }
        }
        if args.crtc_id_ptr != 0 {
            Self::write_id(args.crtc_id_ptr as usize, 0, &CRTC_ID)?;
        }
        if args.connector_id_ptr != 0 {
            Self::write_id(args.connector_id_ptr as usize, 0, &CONNECTOR_ID)?;
        }
        if args.encoder_id_ptr != 0 {
            Self::write_id(args.encoder_id_ptr as usize, 0, &ENCODER_ID)?;
        }

        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_get_connector(&self, cmd: DrmIoctlModeGetConnector) -> Result<i32> {
        let scanout = self.scanout_or_err()?;
        let mut args: abi::ModeGetConnector = cmd.read()?;

        // Tell user space how many modes exist, then provide the only mode on
        // the follow-up call that passes a modes array.
        args.count_modes = 1;
        args.count_props = 0;
        args.count_encoders = 1;
        args.encoder_id = ENCODER_ID;
        args.connector_id = CONNECTOR_ID;
        // DRM_MODE_CONNECTOR_VIRTUAL
        args.connector_type = 11;
        args.connector_type_id = 1;
        // DRM_MODE_CONNECTED
        args.connection = 1;
        args.mm_width = 0;
        args.mm_height = 0;
        // DRM_MODE_SUBPIXEL_UNKNOWN
        args.subpixel = 1;

        if args.modes_ptr != 0 && args.count_modes >= 1 {
            let mode = Self::mode_info(&scanout);
            write_user_value(args.modes_ptr as usize, &mode)?;
        }
        if args.encoders_ptr != 0 {
            Self::write_id(args.encoders_ptr as usize, 0, &ENCODER_ID)?;
        }

        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_get_encoder(&self, cmd: DrmIoctlModeGetEncoder) -> Result<i32> {
        let mut args: abi::ModeGetEncoder = cmd.read()?;
        if args.encoder_id != ENCODER_ID {
            return_errno_with_message!(Errno::ENOENT, "no such encoder");
        }
        // DRM_MODE_ENCODER_VIRTUAL
        args.encoder_type = 10;
        args.crtc_id = CRTC_ID;
        args.possible_crtcs = 1;
        args.possible_clones = 0;
        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_get_crtc(&self, cmd: DrmIoctlModeGetCrtc) -> Result<i32> {
        let scanout = self.scanout_or_err()?;
        let mut args: abi::ModeCrtc = cmd.read()?;
        if args.crtc_id != CRTC_ID {
            return_errno_with_message!(Errno::ENOENT, "no such CRTC");
        }

        let kms = self.minor().registered_device().kms().lock();
        args.fb_id = kms.current_fb().lock().unwrap_or_default();
        args.x = 0;
        args.y = 0;
        args.gamma_size = 0;
        args.mode_valid = 1;
        args.mode = Self::mode_info(&scanout);
        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_set_crtc(&self, cmd: DrmIoctlModeSetCrtc) -> Result<i32> {
        let args: abi::ModeCrtc = cmd.read()?;
        if args.crtc_id != CRTC_ID {
            return_errno_with_message!(Errno::ENOENT, "no such CRTC");
        }
        if args.fb_id != 0
            && !self
                .minor()
                .registered_device()
                .kms()
                .lock()
                .fbs()
                .lock()
                .contains_key(&args.fb_id)
        {
            return_errno_with_message!(Errno::ENOENT, "no such framebuffer");
        }
        // The scanout is fixed; binding a framebuffer only records which dumb
        // buffer user space presents.
        let kms = self.minor().registered_device().kms().lock();
        *kms.current_fb().lock() = (args.fb_id != 0).then_some(args.fb_id);
        Ok(0)
    }

    pub(super) fn mode_add_fb(&self, cmd: DrmIoctlAddFb) -> Result<i32> {
        let scanout = self.scanout_or_err()?;
        let mut args: abi::ModeFbCmd = cmd.read()?;

        // Every dumb buffer aliases the scanout, so the only handle that can
        // appear here is the scanout's own.
        if args.handle != 1 {
            return_errno_with_message!(Errno::ENOENT, "no such dumb buffer");
        }
        if args.width != scanout.width() as u32
            || args.height != scanout.height() as u32
            || args.pitch != scanout.line_size() as u32
        {
            return_errno_with_message!(Errno::EINVAL, "the framebuffer mismatches the scanout");
        }

        let kms = self.minor().registered_device().kms().lock();
        args.fb_id = kms.alloc_fb_id();
        kms.fbs().lock().insert(
            args.fb_id,
            KmsFb {
                width: args.width,
                height: args.height,
                pitch: args.pitch,
                bpp: args.bpp,
                depth: args.depth,
            },
        );
        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_get_fb(&self, cmd: DrmIoctlModeGetFb) -> Result<i32> {
        let args: abi::ModeFbCmd = cmd.read()?;
        let kms = self.minor().registered_device().kms().lock();
        let Some(fb) = kms.fbs().lock().get(&args.fb_id).copied() else {
            return_errno_with_message!(Errno::ENOENT, "no such framebuffer");
        };
        let mut out = args;
        out.width = fb.width;
        out.height = fb.height;
        out.pitch = fb.pitch;
        out.bpp = fb.bpp;
        out.depth = fb.depth;
        out.handle = 1;
        cmd.write(&out)?;
        Ok(0)
    }

    pub(super) fn mode_rm_fb(&self, cmd: DrmIoctlRmFb) -> Result<i32> {
        let fb_id: u32 = cmd.read()?;
        let kms = self.minor().registered_device().kms().lock();
        kms.fbs().lock().remove(&fb_id);
        let mut current = kms.current_fb().lock();
        if *current == Some(fb_id) {
            *current = None;
        }
        Ok(0)
    }

    pub(super) fn mode_create_dumb(&self, cmd: DrmIoctlCreateDumb) -> Result<i32> {
        let scanout = self.scanout_or_err()?;
        let mut args: abi::ModeCreateDumb = cmd.read()?;

        if args.width != scanout.width() as u32
            || args.height != scanout.height() as u32
            || args.bpp != 32
            || args.flags != 0
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "only the native 32bpp scanout size is supported"
            );
        }
        // The returned buffer aliases the scanout, so the pitch and size come
        // from the framebuffer, and the fixed handle identifies the alias.
        args.handle = 1;
        args.pitch = scanout.line_size() as u32;
        args.size = scanout.io_mem().size() as u64;
        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_map_dumb(&self, cmd: DrmIoctlMapDumb) -> Result<i32> {
        let args: abi::ModeMapDumb = cmd.read()?;
        if args.handle != 1 {
            return_errno_with_message!(Errno::ENOENT, "no such dumb buffer");
        }
        // Offset zero makes the DRM file's mmap cover the whole scanout.
        let mut out = args;
        out.offset = 0;
        cmd.write(&out)?;
        Ok(0)
    }

    pub(super) fn mode_destroy_dumb(&self, cmd: DrmIoctlDestroyDumb) -> Result<i32> {
        let args: abi::ModeCreateDumb = cmd.read()?;
        if args.handle != 1 {
            return_errno_with_message!(Errno::ENOENT, "no such dumb buffer");
        }
        Ok(0)
    }

    pub(super) fn mode_obj_get_props(&self, cmd: DrmIoctlObjGetProps) -> Result<i32> {
        let mut args: abi::ModeObjGetProps = cmd.read()?;

        // No properties are exposed: user space sees a connector without
        // EDID or DPMS properties, which the modesetting driver tolerates.
        // Reporting success (instead of ENOTTY) matters because libdrm's
        // `drmModeObjectGetProperties` returns NULL on failure and the
        // driver dereferences it unchecked.
        args.count_props = 0;
        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_list_lessees(&self, cmd: DrmIoctlListLessees) -> Result<i32> {
        let mut args: abi::ModeListLessees = cmd.read()?;

        // DRM leases are not implemented; this device has no lessees.
        args.count_lessees = 0;
        cmd.write(&args)?;
        Ok(0)
    }
}
