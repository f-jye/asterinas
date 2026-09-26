// SPDX-License-Identifier: MPL-2.0

//! GEM-style dumb buffers backed by a device-wide DMA arena.
//!
//! A dumb buffer is a CPU-accessible memory block that user space renders
//! into directly and that the kernel presents on the display. The buffers
//! live in one contiguous [`DmaCoherent`] arena per DRM device: the DRM file
//! maps the whole arena into the client's address space, and each buffer is
//! a page-aligned range inside it identified by a GEM handle.
//!
//! Presentation happens at mode-setting time: binding a framebuffer to the
//! CRTC (through `SETCRTC` or `PAGE_FLIP`) copies the framebuffer's lines
//! into the boot scanout, since the scanout itself has a fixed format and
//! geometry that user space cannot reconfigure.

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicU32, Ordering};

use align_ext::AlignExt;
use aster_core::prelude::*;
use aster_framebuffer::framebuffer::FrameBuffer;
use ostd::{
    mm::{HasSize, PAGE_SIZE, VmIo, dma::DmaCoherent, io::util::HasVmReaderWriter},
    sync::Mutex,
};

use super::device::KmsFb;

/// The first GEM handle handed out for real dumb buffers. Lower handles are
/// reserved (handle zero is the Linux "no buffer" marker).
const FIRST_HANDLE: u32 = 2;
/// The largest arena tried, so that even double-buffered full-screen
/// framebuffers plus scratch buffers fit many times over.
const ARENA_MAX_SIZE: usize = 64 << 20;
/// The smallest arena tried before allocation gives up entirely.
const ARENA_MIN_SIZE: usize = 4 << 20;
/// Pitch alignment in bytes, matching the 64-byte alignment typical of
/// hardware scanout units.
const PITCH_ALIGN: usize = 64;

/// A dumb buffer carved out of the device arena.
#[derive(Clone, Copy, Debug)]
pub(super) struct DumbBuffer {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    /// Always 32 for now; kept for the `MODE_GETFB` reply once it needs it.
    #[expect(dead_code)]
    pub bpp: u32,
    /// Byte offset of the buffer inside the arena (page aligned).
    pub offset: usize,
    /// The page-aligned size of the buffer inside the arena.
    pub len: usize,
}

/// The free-list entry of the arena allocator: one range of untouched or
/// freed space, keyed by its byte offset.
type FreeRanges = BTreeMap<usize, usize>;

/// The device-wide dumb buffer arena and handle table.
#[derive(Debug)]
pub(super) struct Gem {
    arena: Arc<DmaCoherent>,
    state: Mutex<GemState>,
    next_handle: AtomicU32,
}

#[derive(Debug)]
struct GemState {
    /// The size of the arena, duplicated from the allocation for cheap
    /// access while holding only the state lock.
    arena_size: usize,
    buffers: BTreeMap<u32, DumbBuffer>,
    free: FreeRanges,
    /// The offset one past the last ever-allocated byte; allocation grows
    /// into untouched arena space before reusing freed ranges.
    bump: usize,
}

impl Gem {
    /// Allocates the device arena, halving the requested size on failure.
    pub(super) fn new() -> Result<Arc<Self>> {
        let mut size = ARENA_MAX_SIZE;
        let arena = loop {
            match DmaCoherent::alloc(size / PAGE_SIZE, true) {
                Ok(arena) => break arena,
                Err(_) if size > ARENA_MIN_SIZE => size /= 2,
                Err(error) => return Err(Error::from(error)),
            }
        };
        let arena_size = arena.size();

        Ok(Arc::new(Self {
            arena: Arc::new(arena),
            state: Mutex::new(GemState {
                arena_size,
                buffers: BTreeMap::new(),
                free: BTreeMap::new(),
                bump: 0,
            }),
            next_handle: AtomicU32::new(FIRST_HANDLE),
        }))
    }

    /// The arena that DRM file mappings expose to user space.
    pub(super) fn arena(&self) -> Arc<DmaCoherent> {
        self.arena.clone()
    }

    /// Creates a dumb buffer and returns its handle, pitch, and byte size.
    pub(super) fn create_buffer(
        &self,
        width: u32,
        height: u32,
        bpp: u32,
    ) -> Result<(u32, u32, u64)> {
        if width == 0 || height == 0 || width > u16::MAX as u32 {
            return_errno_with_message!(Errno::EINVAL, "the dumb buffer size is invalid");
        }
        // The scanout is 32 bits per pixel and the blit copies lines verbatim.
        if bpp != 32 {
            return_errno_with_message!(Errno::EINVAL, "only 32bpp dumb buffers are supported");
        }

        let pitch = (width as usize * (bpp / 8) as usize).align_up(PITCH_ALIGN);
        let len = (pitch * height as usize).align_up(PAGE_SIZE);
        if len > self.arena.size() {
            return_errno_with_message!(Errno::ENOMEM, "the dumb buffer exceeds the arena");
        }

        let mut state = self.state.lock();
        let offset = state.alloc_range(len)?;

        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        state.buffers.insert(
            handle,
            DumbBuffer {
                width,
                height,
                pitch: pitch as u32,
                bpp,
                offset,
                len,
            },
        );

        Ok((handle, pitch as u32, len as u64))
    }

    /// Destroys a dumb buffer handle and frees its arena range.
    pub(super) fn destroy_buffer(&self, handle: u32) -> Result<()> {
        let mut state = self.state.lock();
        let Some(buffer) = state.buffers.remove(&handle) else {
            return_errno_with_message!(Errno::ENOENT, "no such dumb buffer");
        };
        state.free.insert(buffer.offset, buffer.len);
        Ok(())
    }

    /// Looks up a dumb buffer by handle.
    pub(super) fn buffer(&self, handle: u32) -> Option<DumbBuffer> {
        self.state.lock().buffers.get(&handle).copied()
    }

    /// The byte offset that user space must pass to `mmap` for the buffer.
    pub(super) fn map_offset(&self, handle: u32) -> Result<usize> {
        let state = self.state.lock();
        let Some(buffer) = state.buffers.get(&handle) else {
            return_errno_with_message!(Errno::ENOENT, "no such dumb buffer");
        };
        Ok(buffer.offset)
    }

    /// Copies a framebuffer's lines from the arena into the scanout.
    ///
    /// Called whenever the framebuffer becomes the scanout image (`SETCRTC`
    /// or `PAGE_FLIP`). Line-by-line because the buffer pitch may differ
    /// from the scanout pitch.
    pub(super) fn present(&self, scanout: &FrameBuffer, fb: &KmsFb) {
        let Some(buffer) = self.buffer(fb.handle) else {
            return;
        };

        let height = (buffer.height as usize).min(scanout.height());
        let copy_len = (buffer.pitch as usize).min(scanout.line_size());
        for y in 0..height {
            let mut reader = self.arena.reader();
            reader.skip(buffer.offset + y * buffer.pitch as usize);
            reader.limit(copy_len);
            let mut fallible_reader = reader.to_fallible();
            if scanout
                .io_mem()
                .write(y * scanout.line_size(), &mut fallible_reader)
                .is_err()
            {
                return;
            }
        }
    }
}

impl GemState {
    /// Reserves `len` bytes, preferring untouched arena space and reusing
    /// freed ranges otherwise. Adjacent freed ranges are not merged, so a
    /// range larger than every individual free entry only succeeds while
    /// untouched space remains.
    fn alloc_range(&mut self, len: usize) -> Result<usize> {
        let mut offset = None;
        for (&free_offset, &free_len) in self.free.iter() {
            if free_len >= len {
                offset = Some(free_offset);
                break;
            }
        }
        if let Some(free_offset) = offset {
            let free_len = self.free.remove(&free_offset).unwrap();
            if free_len > len {
                self.free.insert(free_offset + len, free_len - len);
            }
            return Ok(free_offset);
        }

        if self.bump + len > self.arena_size {
            return_errno_with_message!(Errno::ENOMEM, "no space left in the dumb buffer arena");
        }
        let assigned = self.bump;
        self.bump += len;
        Ok(assigned)
    }
}
