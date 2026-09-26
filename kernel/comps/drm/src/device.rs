// SPDX-License-Identifier: MPL-2.0

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use core::{
    fmt::Debug,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    time::Duration,
};

use aster_core::{prelude::*, sleep, spawn_kernel_thread};
use aster_framebuffer::framebuffer::FrameBuffer;
use ostd::sync::Mutex;
use sparse_id_alloc::SparseIdAlloc;

use super::gem;

/// The emulated refresh cycle of the continuous scanout.
///
/// Real hardware scans out the active framebuffer on every refresh cycle,
/// regardless of how user space writes into it. This device implements
/// scanout as a copy into the boot framebuffer, so user space writes that are
/// not followed by a mode-set or a page flip would otherwise never become
/// visible. The refresh thread re-presents the current framebuffer at this
/// rate to emulate the hardware behavior.
const SCANOUT_REFRESH_INTERVAL: Duration = Duration::from_millis(16);

static DRM_DEVICE_INDEX_ALLOCATOR: Mutex<SparseIdAlloc> = Mutex::new(SparseIdAlloc::new(0, 63));

/// Defines the top-level contract of a DRM device instance.
///
/// `DrmDevice` is the composition root for device-facing DRM behavior.
/// It provides stable identity metadata and shared capability discovery,
/// while higher-level DRM operations are expected to be layered as
/// dedicated operation traits.
pub trait DrmDevice: Debug + Send + Sync {
    fn name(&self) -> &str;
    fn desc(&self) -> &str;
    fn features(&self) -> &DrmFeatures;
    fn has_features(&self, feature: DrmFeatures) -> bool {
        self.features().contains(feature)
    }

    /// Returns the boot framebuffer that backs this device's scanout, if the
    /// device is a simple framebuffer display. The minimal KMS ioctls render
    /// into it directly.
    fn scanout(&self) -> Option<Arc<FrameBuffer>> {
        None
    }
}

bitflags::bitflags! {
    /// Capabilities provided by an Asterinas DRM device implementation.
    ///
    /// These flags are internal to the DRM subsystem. Their bit positions
    /// are not part of the DRM userspace ABI.
    pub struct DrmFeatures: u32 {
        /// Supports creation of a render device node.
        const RENDER           = 1 << 0;
        /// Supports kernel mode-setting (KMS) operations.
        const MODESET          = 1 << 1;
        /// Supports atomic mode-setting operations.
        const ATOMIC           = 1 << 2;
        /// Supports graphics execution manager (GEM) operations.
        const GEM              = 1 << 3;
        /// Supports DRM synchronization objects.
        const SYNCOBJ          = 1 << 4;
        /// Supports timeline synchronization objects.
        const SYNCOBJ_TIMELINE = 1 << 5;
        /// Requires userspace-aware cursor hotspot handling.
        const CURSOR_HOTSPOT   = 1 << 6;
    }
}

/// A registered DRM device together with its DRM-core-managed state.
#[derive(Debug)]
pub(super) struct RegisteredDrmDevice {
    index: DrmDeviceIndex,
    device: Arc<dyn DrmDevice>,
    /// The currently active master context.
    ///
    /// Primary files retain their own `Arc<DrmMaster>`, so clearing this
    /// pointer on `DROP_MASTER` does not destroy the former master's context.
    master: Mutex<Option<Arc<DrmMaster>>>,
    /// The minimal KMS state: framebuffers created through
    /// `DRM_IOCTL_MODE_ADDFB` and the framebuffer set by `DRM_IOCTL_MODE_SETCRTC`.
    pub(super) kms: Mutex<Kms>,
}

/// The framebuffer objects and scanout state of one DRM device.
#[derive(Debug)]
pub(super) struct Kms {
    fbs: Mutex<BTreeMap<u32, KmsFb>>,
    next_fb_id: AtomicU32,
    current_fb: Mutex<Option<u32>>,
    /// The dumb buffer arena, allocated lazily on first use so that a failed
    /// arena allocation only disables dumb buffers, not the device.
    gem: Mutex<Option<Arc<gem::Gem>>>,
    /// The per-CRTC flip counter reported in page flip events.
    flip_sequence: AtomicU32,
}

/// A framebuffer registered through `DRM_IOCTL_MODE_ADDFB` or
/// `DRM_IOCTL_MODE_ADDBFB2`.
#[derive(Clone, Copy, Debug)]
pub(super) struct KmsFb {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    /// The fourcc pixel format (`DRM_FORMAT_*`). Read back by `MODE_GETFB2`.
    #[expect(dead_code)]
    pub format: u32,
    /// The GEM handle of the backing dumb buffer.
    pub handle: u32,
}

pub(super) const FIRST_FB_ID: u32 = 1;

impl Default for Kms {
    fn default() -> Self {
        Self {
            fbs: Mutex::new(BTreeMap::new()),
            next_fb_id: AtomicU32::new(FIRST_FB_ID),
            current_fb: Mutex::new(None),
            gem: Mutex::new(None),
            flip_sequence: AtomicU32::new(0),
        }
    }
}
pub(super) const CONNECTOR_ID: u32 = 32;
pub(super) const ENCODER_ID: u32 = 33;
pub(super) const CRTC_ID: u32 = 34;

impl Kms {
    pub(super) fn alloc_fb_id(&self) -> u32 {
        self.next_fb_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn fbs(&self) -> &Mutex<BTreeMap<u32, KmsFb>> {
        &self.fbs
    }

    pub(super) fn current_fb(&self) -> &Mutex<Option<u32>> {
        &self.current_fb
    }

    pub(super) fn flip_sequence(&self) -> &AtomicU32 {
        &self.flip_sequence
    }

    /// The dumb buffer arena, allocating it on first use.
    pub(super) fn gem(&self) -> Result<Arc<gem::Gem>> {
        let mut slot = self.gem.lock();
        if let Some(gem) = slot.as_ref() {
            return Ok(gem.clone());
        }
        let gem = gem::Gem::new()?;
        *slot = Some(gem.clone());
        Ok(gem)
    }

    /// The dumb buffer arena if it is already allocated.
    pub(super) fn loaded_gem(&self) -> Option<Arc<gem::Gem>> {
        self.gem.lock().clone()
    }
}

impl RegisteredDrmDevice {
    pub(super) fn new(device: Arc<dyn DrmDevice>) -> Result<Self> {
        Ok(Self {
            index: DrmDeviceIndex::alloc()?,
            device,
            master: Mutex::new(None),
            kms: Mutex::new(Kms::default()),
        })
    }

    /// Spawns the thread that emulates the continuous scanout of hardware.
    ///
    /// See [`SCANOUT_REFRESH_INTERVAL`] for why the re-presentation loop is
    /// needed. The thread keeps only a weak reference to the device, so it
    /// stops when the device is unregistered.
    pub(super) fn spawn_scanout_refresh(self: &Arc<Self>) {
        let device = Arc::downgrade(self);
        spawn_kernel_thread(move || {
            loop {
                sleep(SCANOUT_REFRESH_INTERVAL);
                let Some(device) = device.upgrade() else {
                    return;
                };
                let Some(scanout) = device.device().scanout() else {
                    return;
                };
                let presentation = {
                    let kms = device.kms().lock();
                    let current_fb = kms.current_fb().lock();
                    let fb = current_fb
                        .as_ref()
                        .and_then(|fb_id| kms.fbs().lock().get(fb_id).copied());
                    kms.loaded_gem().zip(fb)
                };
                if let Some((gem, fb)) = presentation {
                    gem.present(&scanout, &fb);
                }
            }
        });
    }

    pub(super) fn index(&self) -> u32 {
        self.index.index()
    }

    pub(super) fn device(&self) -> &Arc<dyn DrmDevice> {
        &self.device
    }

    pub(super) fn kms(&self) -> &Mutex<Kms> {
        &self.kms
    }

    pub(super) fn is_client_master(&self, client_id: u64) -> bool {
        let master = self.master.lock();
        master
            .as_ref()
            .is_some_and(|master| master.owner_client_id == client_id)
    }

    /// Authenticates a magic value on behalf of the current master.
    ///
    /// The current-master check and authentication are performed while holding
    /// the same master lock, so master ownership cannot change between
    /// authorization and the operation.
    pub(super) fn authenticate_magic(&self, client_id: u64, magic: u32) -> Result<()> {
        let master = self.master.lock();
        let Some(current_master) = master
            .as_ref()
            .filter(|master| master.owner_client_id == client_id)
        else {
            return_errno_with_message!(Errno::EACCES, "the DRM client is not the current master");
        };

        current_master.authenticate_magic(magic)
    }

    /// Associates a newly opened primary file with a master context.
    pub(super) fn open_primary_client(&self, client_id: u64) -> Arc<DrmMaster> {
        let mut master = self.master.lock();
        match master.as_ref() {
            Some(master) => master.clone(),
            None => {
                let new_master = Arc::new(DrmMaster::new(client_id));
                *master = Some(new_master.clone());
                new_master
            }
        }
    }

    /// Makes a primary client the device's current DRM master.
    ///
    /// A previous master reacquires its retained context. A file becoming master
    /// for the first time receives a new context.
    pub(super) fn set_master(
        &self,
        client_id: u64,
        retained_master: Option<&Arc<DrmMaster>>,
    ) -> Result<Arc<DrmMaster>> {
        let mut current_master = self.master.lock();
        match current_master.as_ref() {
            Some(current_master) => {
                if current_master.owner_client_id == client_id {
                    Ok(current_master.clone())
                } else {
                    return_errno_with_message!(
                        Errno::EBUSY,
                        "another DRM client is already the current master"
                    )
                }
            }
            None => {
                let master = match retained_master {
                    Some(retained) if retained.owner_client_id == client_id => retained.clone(),
                    Some(_) => return_errno_with_message!(
                        Errno::EINVAL,
                        "the retained DRM master belongs to another client"
                    ),
                    None => Arc::new(DrmMaster::new(client_id)),
                };
                *current_master = Some(master.clone());
                Ok(master)
            }
        }
    }

    /// Removes the device's current-master reference.
    ///
    /// The owning DRM file retains its own `Arc`, allowing it to reacquire the
    /// same context later.
    pub(super) fn drop_master(&self, client_id: u64) -> Result<()> {
        let mut master = self.master.lock();
        if !master
            .as_ref()
            .is_some_and(|master| master.owner_client_id == client_id)
        {
            return_errno_with_message!(Errno::EINVAL, "the DRM client is not the current master");
        }
        *master = None;

        Ok(())
    }
}

/// An index shared by all minor nodes belonging to a DRM device.
#[derive(Debug)]
struct DrmDeviceIndex(u32);

impl DrmDeviceIndex {
    fn alloc() -> Result<Self> {
        let Some(index) = DRM_DEVICE_INDEX_ALLOCATOR.lock().alloc() else {
            return_errno_with_message!(Errno::ENOMEM, "no DRM device indices are available");
        };

        Ok(Self(index))
    }

    fn index(&self) -> u32 {
        self.0
    }
}

impl Drop for DrmDeviceIndex {
    fn drop(&mut self) {
        DRM_DEVICE_INDEX_ALLOCATOR.lock().free(self.0);
    }
}

/// A master-owned context shared with associated primary files.
///
/// Exactly one DRM file owns this context, while other primary files may
/// retain references to it for legacy magic authentication.
/// The context may outlive its role as the device's current master.
#[derive(Debug)]
pub(super) struct DrmMaster {
    owner_client_id: u64,
    magic_state: Mutex<DrmMagicState>,
}

impl DrmMaster {
    fn new(owner_client_id: u64) -> Self {
        Self {
            owner_client_id,
            magic_state: Mutex::new(DrmMagicState {
                allocator: SparseIdAlloc::new(1, u32::MAX),
                magic_table: BTreeMap::new(),
            }),
        }
    }

    /// Returns the client ID of the file that owns this master context.
    pub(super) fn owner_client_id(&self) -> u64 {
        self.owner_client_id
    }

    pub(super) fn allocate_magic(&self, authenticated: &Arc<AtomicBool>) -> Result<u32> {
        let mut state = self.magic_state.lock();
        let Some(magic) = state.allocator.alloc() else {
            return_errno_with_message!(Errno::ENOMEM, "no DRM magic identifiers are available");
        };
        state
            .magic_table
            .insert(magic, Arc::downgrade(authenticated));
        Ok(magic)
    }

    pub(super) fn authenticate_magic(&self, magic: u32) -> Result<()> {
        let Some(authenticated) = self
            .magic_state
            .lock()
            .magic_table
            .remove(&magic)
            .and_then(|authenticated| authenticated.upgrade())
        else {
            return_errno_with_message!(Errno::EINVAL, "the DRM magic identifier is invalid");
        };

        authenticated.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn release_magic(&self, magic: u32) {
        let mut state = self.magic_state.lock();
        state.magic_table.remove(&magic);
        state.allocator.free(magic);
    }
}

/// Magic IDs and their pending authentication targets.
///
/// Both fields are protected by the same lock so an ID cannot be reused while
/// its authentication entry is still pending. Authentication consumes the
/// table entry, but the ID remains allocated until the DRM file is released.
#[derive(Debug)]
struct DrmMagicState {
    allocator: SparseIdAlloc,
    magic_table: BTreeMap<u32, Weak<AtomicBool>>,
}
