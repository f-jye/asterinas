// SPDX-License-Identifier: MPL-2.0

//! Minimal kernel mode-setting ioctls backed by the boot framebuffer.
//!
//! User space renders into dumb buffers carved out of a device-wide DMA
//! arena (see [`crate::gem`]) and submits them as framebuffers through
//! `ADDFB`/`ADDBFB2`. Binding a framebuffer to the CRTC via `SETCRTC` or
//! presenting it with `PAGE_FLIP` copies its lines into the fixed-geometry
//! boot scanout, so whatever buffer user space bound last is what the
//! display shows. Page flip completions are delivered as DRM events that
//! user space reads from the DRM file descriptor.

use alloc::{boxed::Box, format, sync::Arc};
use core::sync::atomic::Ordering;

use aster_core::{prelude::*, util::ioctl::write_user_value};
use aster_framebuffer::framebuffer::FrameBuffer;
use ostd::timer::Jiffies;
use ostd_pod::IntoBytes;

use super::{
    super::{
        device::{CONNECTOR_ID, CRTC_ID, ENCODER_ID, Kms, KmsFb},
        gem::Gem,
    },
    ioctl_defs::*,
};
use crate::file::DrmFile;

/// `DRM_FORMAT_XRGB8888`.
const FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// `DRM_FORMAT_ARGB8888`.
const FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// `DRM_FORMAT_MOD_LINEAR`.
const MODIFIER_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`, which Linux passes when modifiers are unused.
const MODIFIER_INVALID: u64 = (1 << 56) - 1;
/// `DRM_MODE_PAGE_FLIP_EVENT`: report completion through a DRM event.
const PAGE_FLIP_EVENT: u32 = 0x1;
/// `DRM_MODE_PAGE_FLIP_ASYNC`: flip as soon as possible.
const PAGE_FLIP_ASYNC: u32 = 0x2;
/// `DRM_EVENT_FLIP_COMPLETE`.
const EVENT_FLIP_COMPLETE: u32 = 0x2;

/// The payload of a page flip completion event, matching Linux's
/// `struct drm_event_vblank` (with the `crtc_id` field Linux always writes).
#[padding_struct]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
struct FlipEvent {
    type_: u32,
    length: u32,
    user_data: u64,
    tv_sec: u64,
    tv_usec: u32,
    sequence: u32,
    crtc_id: u32,
}

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

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L669>.
    #[padding_struct]
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeFbCmd2 {
        pub fb_id: u32,
        pub width: u32,
        pub height: u32,
        pub pixel_format: u32,
        pub flags: u32,
        pub handles: [u32; 4],
        pub pitches: [u32; 4],
        pub offsets: [u32; 4],
        pub modifier: [u64; 4],
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L851>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeCrtcPageFlip {
        pub crtc_id: u32,
        pub fb_id: u32,
        pub flags: u32,
        pub reserved: u32,
        pub user_data: u64,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm.h#L607>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct GemClose {
        pub handle: u32,
        pub pad: u32,
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

    /// `struct drm_mode_destroy_dumb` in Linux.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L1266>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeDestroyDumb {
        pub handle: u32,
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

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L796>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeGetPlaneRes {
        pub plane_id_ptr: u64,
        pub count_planes: u32,
        pub __pad: u32,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L456>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeCrtcLut {
        pub crtc_id: u32,
        pub gamma_size: u32,
        pub red_ptr: u64,
        pub green_ptr: u64,
        pub blue_ptr: u64,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L319>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeGetPlane {
        pub plane_id: u32,
        pub crtc_id: u32,
        pub fb_id: u32,
        pub possible_crtcs: u32,
        pub gamma_size: u32,
        pub count_format_types: u32,
        pub format_type_ptr: u64,
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L580>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeGetProperty {
        pub values_ptr: u64,
        pub enum_blob_ptr: u64,
        pub count_values: u32,
        pub count_enum_blobs: u32,
        pub prop_id: u32,
        pub flags: u32,
        pub name: [u8; 32],
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L544>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModePropEnum {
        pub value: u64,
        pub name: [u8; 32],
    }

    /// Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/drm/drm_mode.h#L1533>.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Pod)]
    pub struct ModeCloseFb {
        pub fb_id: u32,
        pub pad: u32,
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

    /// The dumb buffer arena of the device, allocating it on first use.
    fn gem_or_err(&self) -> Result<Arc<Gem>> {
        self.minor().registered_device().kms().lock().gem()
    }

    /// Validates a framebuffer request against its backing dumb buffer and
    /// registers it, returning the new framebuffer id.
    fn register_fb(&self, fb: KmsFb) -> Result<u32> {
        let gem = self.gem_or_err()?;
        let Some(buffer) = gem.buffer(fb.handle) else {
            return_errno_with_message!(Errno::ENOENT, "no such dumb buffer");
        };
        // Linux does not require an FB to cover its backing buffer exactly:
        // user space (e.g. mesa's aligned allocations) may submit smaller
        // framebuffers. Only require the pitch to cover the width.
        let bytes_per_pixel = (fb.bpp as usize).div_ceil(8);
        if fb.width as usize * bytes_per_pixel > fb.pitch as usize {
            return_errno_with_message!(Errno::EINVAL, "the framebuffer pitch is too small");
        }
        let _ = buffer;

        let kms = self.minor().registered_device().kms().lock();
        let fb_id = kms.alloc_fb_id();
        kms.fbs().lock().insert(fb_id, fb);
        Ok(fb_id)
    }

    /// Presents the framebuffer on the scanout, if presentation is possible.
    ///
    /// The caller passes the device's KMS state; presentation must not
    /// re-lock it.
    fn present(&self, kms: &Kms, fb: &KmsFb) {
        let Some(scanout) = self.minor().device().scanout() else {
            return;
        };
        let Some(gem) = kms.loaded_gem() else {
            return;
        };
        gem.present(&scanout, fb);
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

        let registered = self.minor().registered_device();
        let kms = registered.kms().lock();
        let fb = if args.fb_id != 0 {
            Some(
                *kms.fbs()
                    .lock()
                    .get(&args.fb_id)
                    .ok_or_else(|| Error::with_message(Errno::ENOENT, "no such framebuffer"))?,
            )
        } else {
            None
        };

        if let Some(fb) = &fb {
            self.present(&kms, fb);
        }
        // Binding a framebuffer records which dumb buffer user space
        // presents; the scanout itself has a fixed geometry.
        *kms.current_fb().lock() = (args.fb_id != 0).then_some(args.fb_id);
        Ok(0)
    }

    pub(super) fn mode_add_fb(&self, cmd: DrmIoctlAddFb) -> Result<i32> {
        let _scanout = self.scanout_or_err()?;
        let mut args: abi::ModeFbCmd = cmd.read()?;

        // Legacy ADDFB identifies the format by depth/bpp; derive the fourcc
        // the way Linux's framebuffer lookup does.
        let format = match (args.depth, args.bpp) {
            (24, 32) => FORMAT_XRGB8888,
            (32, 32) => FORMAT_ARGB8888,
            _ => {
                ostd::warn!("drm: addfb unsupported format depth={} bpp={}", args.depth, args.bpp);
                return_errno_with_message!(Errno::EINVAL, "the framebuffer format is unsupported")
            }
        };

        args.fb_id = self.register_fb(KmsFb {
            width: args.width,
            height: args.height,
            pitch: args.pitch,
            bpp: args.bpp,
            depth: args.depth,
            format,
            handle: args.handle,
        })?;
        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_add_fb2(&self, cmd: DrmIoctlAddFb2) -> Result<i32> {
        let _scanout = self.scanout_or_err()?;
        let mut args: abi::ModeFbCmd2 = cmd.read()?;

        if args.flags != 0 {
            ostd::warn!("drm: addfb2 flags={:#x}", args.flags);
            return_errno_with_message!(Errno::EINVAL, "framebuffer flags are unsupported");
        }
        // Only single-plane, linearly addressed framebuffers are supported.
        if args.handles[1..].iter().any(|handle| *handle != 0)
            || args.offsets[0] != 0
            || args.offsets[1..].iter().any(|offset| *offset != 0)
        {
            ostd::warn!("drm: addfb2 multi-plane");
            return_errno_with_message!(Errno::EINVAL, "multi-plane framebuffers are unsupported");
        }
        // Only the first plane carries a modifier for our single-plane
        // framebuffers; Linux userspace leaves the unused entries zero or
        // fills them with INVALID.
        let modifier = args.modifier[0];
        if modifier != MODIFIER_LINEAR && modifier != MODIFIER_INVALID {
            ostd::warn!("drm: addfb2 modifier={:#x}", modifier);
            return_errno_with_message!(Errno::EINVAL, "the framebuffer modifiers are unsupported");
        }
        let format = match args.pixel_format {
            FORMAT_XRGB8888 => FORMAT_XRGB8888,
            FORMAT_ARGB8888 => FORMAT_ARGB8888,
            _ => {
                return_errno_with_message!(Errno::EINVAL, "the framebuffer format is unsupported")
            }
        };
        let depth = if format == FORMAT_ARGB8888 { 32 } else { 24 };

        args.fb_id = self.register_fb(KmsFb {
            width: args.width,
            height: args.height,
            pitch: args.pitches[0],
            bpp: 32,
            depth,
            format,
            handle: args.handles[0],
        })?;
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
        out.handle = fb.handle;
        cmd.write(&out)?;
        Ok(0)
    }

    pub(super) fn mode_rm_fb(&self, cmd: DrmIoctlRmFb) -> Result<i32> {
        let fb_id: u32 = cmd.read()?;
        let kms = self.minor().registered_device().kms().lock();
        if kms.fbs().lock().remove(&fb_id).is_none() {
            return_errno_with_message!(Errno::ENOENT, "no such framebuffer");
        }
        let mut current = kms.current_fb().lock();
        if *current == Some(fb_id) {
            *current = None;
        }
        Ok(0)
    }

    /// `DRM_IOCTL_MODE_CLOSEFB`: the modern equivalent of `RMFB`, taking the
    /// framebuffer id in a struct instead of a bare `unsigned int`.
    pub(super) fn mode_close_fb(&self, cmd: DrmIoctlModeCloseFb) -> Result<i32> {
        let args: abi::ModeCloseFb = cmd.read()?;
        let kms = self.minor().registered_device().kms().lock();
        if kms.fbs().lock().remove(&args.fb_id).is_none() {
            return_errno_with_message!(Errno::ENOENT, "no such framebuffer");
        }
        let mut current = kms.current_fb().lock();
        if *current == Some(args.fb_id) {
            *current = None;
        }
        Ok(0)
    }

    pub(super) fn mode_page_flip(&self, cmd: DrmIoctlModePageFlip) -> Result<i32> {
        let args: abi::ModeCrtcPageFlip = cmd.read()?;
        if args.crtc_id != CRTC_ID {
            return_errno_with_message!(Errno::ENOENT, "no such CRTC");
        }
        if args.reserved != 0 || args.flags & !(PAGE_FLIP_EVENT | PAGE_FLIP_ASYNC) != 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid page flip flags");
        }

        let registered = self.minor().registered_device();
        let kms = registered.kms().lock();
        let Some(fb) = kms.fbs().lock().get(&args.fb_id).copied() else {
            return_errno_with_message!(Errno::ENOENT, "no such framebuffer");
        };

        self.present(&kms, &fb);
        *kms.current_fb().lock() = Some(args.fb_id);

        if args.flags & PAGE_FLIP_EVENT != 0 {
            let elapsed = Jiffies::elapsed().as_duration();
            // The padding fields added by `padding_struct` are not part of
            // the wire format, so they are filled through `Default`.
            let event = FlipEvent {
                type_: EVENT_FLIP_COMPLETE,
                length: size_of::<FlipEvent>() as u32,
                user_data: args.user_data,
                tv_sec: elapsed.as_secs(),
                tv_usec: elapsed.subsec_micros(),
                sequence: kms.flip_sequence().fetch_add(1, Ordering::Relaxed),
                crtc_id: CRTC_ID,
                ..FlipEvent::default()
            };
            self.queue_event(Box::from(event.as_bytes()));
        }
        Ok(0)
    }

    pub(super) fn mode_create_dumb(&self, cmd: DrmIoctlCreateDumb) -> Result<i32> {
        let _scanout = self.scanout_or_err()?;
        let mut args: abi::ModeCreateDumb = cmd.read()?;
        if args.flags != 0 {
            return_errno_with_message!(Errno::EINVAL, "dumb buffer flags are unsupported");
        }

        let gem = self.gem_or_err()?;
        let (handle, pitch, size) = gem.create_buffer(args.width, args.height, args.bpp)?;
        args.handle = handle;
        args.pitch = pitch;
        args.size = size;
        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_map_dumb(&self, cmd: DrmIoctlMapDumb) -> Result<i32> {
        let args: abi::ModeMapDumb = cmd.read()?;
        let gem = self.gem_or_err()?;

        // The DRM file maps the whole arena, so the buffer's offset inside
        // the arena is the offset user space passes to `mmap`.
        let mut out = args;
        out.offset = gem.map_offset(args.handle)? as u64;
        cmd.write(&out)?;
        Ok(0)
    }

    pub(super) fn mode_destroy_dumb(&self, cmd: DrmIoctlDestroyDumb) -> Result<i32> {
        let args: abi::ModeDestroyDumb = cmd.read()?;
        let gem = self.gem_or_err()?;
        gem.destroy_buffer(args.handle)?;
        Ok(0)
    }

    pub(super) fn gem_close(&self, cmd: DrmIoctlGemClose) -> Result<i32> {
        let args: abi::GemClose = cmd.read()?;
        let gem = self.gem_or_err()?;
        gem.destroy_buffer(args.handle)?;
        Ok(0)
    }

    pub(super) fn mode_obj_get_props(&self, cmd: DrmIoctlObjGetProps) -> Result<i32> {
        let mut args: abi::ModeObjGetProps = cmd.read()?;

        // `DRM_MODE_OBJECT_PLANE`: expose the plane's `type` enum property,
        // whose value identifies it as the primary plane. Everything else
        // reports no properties, which the modesetting drivers tolerate.
        // Reporting success (instead of ENOTTY) matters because libdrm's
        // `drmModeObjectGetProperties` returns NULL on failure and the
        // drivers dereference it unchecked.
        const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
        if args.obj_type == DRM_MODE_OBJECT_PLANE && args.obj_id == Self::PLANE_ID {
            args.count_props = 1;
            cmd.write(&args)?;
            if args.props_ptr != 0 {
                write_user_value(args.props_ptr as usize, &Self::PLANE_TYPE_PROP_ID)?;
            }
            if args.prop_values_ptr != 0 {
                write_user_value(args.prop_values_ptr as usize, &1u64)?;
            }
        } else {
            args.count_props = 0;
            cmd.write(&args)?;
        }
        Ok(0)
    }

    pub(super) fn mode_list_lessees(&self, cmd: DrmIoctlListLessees) -> Result<i32> {
        let mut args: abi::ModeListLessees = cmd.read()?;

        // DRM leases are not implemented; this device has no lessees.
        args.count_lessees = 0;
        cmd.write(&args)?;
        Ok(0)
    }

    pub(super) fn mode_get_plane_resources(&self, cmd: DrmIoctlModeGetPlaneRes) -> Result<i32> {
        let _scanout = self.scanout_or_err()?;
        let mut args: abi::ModeGetPlaneRes = cmd.read()?;

        // The single primary plane covering the only CRTC.
        args.count_planes = 1;
        cmd.write(&args)?;
        if args.plane_id_ptr != 0 {
            write_user_value(args.plane_id_ptr as usize, &Self::PLANE_ID)?;
        }
        Ok(0)
    }

    pub(super) fn mode_get_gamma(&self, cmd: DrmIoctlModeGetGamma) -> Result<i32> {
        let args: abi::ModeCrtcLut = cmd.read()?;
        if args.crtc_id != CRTC_ID {
            return_errno_with_message!(Errno::ENOENT, "no such CRTC");
        }
        // The scanout has no programmable LUT; the CRTC reports
        // `gamma_size == 0`, and Linux rejects a nonzero read size.
        if args.gamma_size != 0 {
            return_errno_with_message!(Errno::EINVAL, "the CRTC has no gamma LUT");
        }
        Ok(0)
    }

    /// The id of the single primary plane exposed for the CRTC, and of its
    /// `type` property. The values are arbitrary but must be consistent.
    pub(super) const PLANE_ID: u32 = 100;
    pub(super) const PLANE_TYPE_PROP_ID: u32 = 62;

    /// `DRM_FORMAT_XRGB8888`, the only format the plane advertises.
    const PLANE_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

    pub(super) fn mode_get_plane(&self, cmd: DrmIoctlModeGetPlane) -> Result<i32> {
        let scanout = self.scanout_or_err()?;
        let mut args: abi::ModeGetPlane = cmd.read()?;
        if args.plane_id != Self::PLANE_ID {
            return_errno_with_message!(Errno::ENOENT, "no such plane");
        }

        let kms = self.minor().registered_device().kms().lock();
        args.crtc_id = CRTC_ID;
        args.fb_id = kms.current_fb().lock().unwrap_or_default();
        args.possible_crtcs = 1;
        args.gamma_size = 0;
        // XRGB8888 is the only supported format.
        args.count_format_types = 1;
        cmd.write(&args)?;
        if args.format_type_ptr != 0 {
            write_user_value(args.format_type_ptr as usize, &Self::PLANE_FORMAT_XRGB8888)?;
        }
        let _ = scanout;
        Ok(0)
    }

    pub(super) fn mode_get_property(&self, cmd: DrmIoctlModeGetProperty) -> Result<i32> {
        let mut args: abi::ModeGetProperty = cmd.read()?;
        if args.prop_id != Self::PLANE_TYPE_PROP_ID {
            return_errno_with_message!(Errno::ENOENT, "no such property");
        }

        args.flags = 1 << 3; // DRM_MODE_PROP_ENUM
        args.count_values = 0;
        args.count_enum_blobs = 1;
        let mut name = [0u8; 32];
        name[..4].copy_from_slice(b"type");
        args.name = name;
        cmd.write(&args)?;

        if args.enum_blob_ptr != 0 {
            let mut entry = abi::ModePropEnum {
                value: 1, // DRM_PLANE_TYPE_PRIMARY
                name: [0u8; 32],
            };
            entry.name[..7].copy_from_slice(b"primary");
            write_user_value(args.enum_blob_ptr as usize, &entry)?;
        }
        Ok(0)
    }
}
