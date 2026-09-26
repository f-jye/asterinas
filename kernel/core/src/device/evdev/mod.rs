// SPDX-License-Identifier: MPL-2.0

//! Event device (evdev) support.
//!
//! Character device with major number 13. The minor numbers are dynamically allocated.
//! Devices appear as `/dev/input/eventX` where X is the minor number.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.17/source/include/uapi/linux/major.h>

mod file;

use alloc::format;
use core::{
    fmt::Debug,
    sync::atomic::{AtomicU32, Ordering},
};

use aster_device::{
    AnyDevice, Attr, AttrGroupDevice, Class, ClassDevice, ClassHandle, DevNode,
    Error as DeviceError,
};
use aster_input::{
    event_type_codes::SynEvent,
    input_dev::{InputDevice, InputEvent},
    input_handler::{ConnectError, InputHandler, InputHandlerClass},
};
use device_id::{DeviceId, MajorId, MinorId};
use file::{
    EVDEV_BUFFER_SIZE, EvdevEvent, EvdevFile, EvdevFileInner, is_syn_dropped_event,
    is_syn_report_event,
};
use spin::Once;

use super::{
    Device, DeviceType,
    registry::char::{MajorIdOwner, acquire_major, register, unregister},
};
use crate::{
    fs::{devtmpfs::DevtmpfsNodeMeta, file::PerOpenFileOps},
    prelude::*,
    util::ring_buffer::RbProducer,
};

/// Major device number for evdev devices.
const EVDEV_MAJOR_ID: u16 = 13;

/// Global minor number allocator for evdev devices.
static EVDEV_MINOR_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Global registry of evdev devices.
static EVDEV_DEVICES: Mutex<BTreeMap<MinorId, Arc<EvdevDevice>>> = Mutex::new(BTreeMap::new());

/// The device-model nodes published for one evdev device, kept so that
/// disconnection can remove them children-first.
struct SysfsNodes {
    event: Arc<EvdevClassDevice>,
    input: Arc<EvdevClassDevice>,
    capabilities: Arc<InputAttrGroup>,
    id: Arc<InputAttrGroup>,
}

static EVDEV_SYSFS_NODES: Mutex<BTreeMap<MinorId, SysfsNodes>> = Mutex::new(BTreeMap::new());

struct EvdevDevice {
    /// Input device associated with this evdev.
    device: Arc<dyn InputDevice>,
    /// List of opened evdev files with their producers.
    ///
    /// # Deadlock Prevention
    ///
    /// This lock is acquired in both the task and interrupt contexts.
    /// We must make sure that this lock is taken with the local IRQs disabled.
    /// Otherwise, we would be vulnerable to deadlock.
    opened_files: SpinLock<Vec<(Arc<EvdevFileInner>, RbProducer<EvdevEvent>)>>,
    /// Device ID.
    id: DeviceId,
}

impl Debug for EvdevDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let device_name = self.device.name();
        let opened_count = self.opened_files.disable_irq().lock().len();
        let id_minor = self.id.minor();
        f.debug_struct("EvdevDevice")
            .field("device_name", &device_name)
            .field("opened_count", &opened_count)
            .field("id_minor", &id_minor)
            .finish_non_exhaustive()
    }
}

impl EvdevDevice {
    pub(self) fn new(minor: u32, device: Arc<dyn InputDevice>) -> Self {
        let major = MajorId::new(EVDEV_MAJOR_ID);
        let minor_id = MinorId::new(minor);

        Self {
            device,
            opened_files: SpinLock::new(Vec::new()),
            id: DeviceId::new(major, minor_id),
        }
    }

    /// Checks if this evdev device is associated with the given input device.
    pub(self) fn matches_input_device(&self, input_device: &Arc<dyn InputDevice>) -> bool {
        Arc::ptr_eq(&self.device, input_device)
    }

    /// Adds an opened evdev file to this evdev device.
    fn attach_file(&self, file: Arc<EvdevFileInner>, producer: RbProducer<EvdevEvent>) {
        let mut opened_files = self.opened_files.disable_irq().lock();
        opened_files.push((file, producer));
    }

    /// Removes the closed evdev file from this evdev device.
    pub(self) fn detach_closed_file(&self, file: &Arc<EvdevFileInner>) {
        let mut opened_files = self.opened_files.disable_irq().lock();
        let pos = opened_files
            .iter()
            .position(|(f, _)| Arc::ptr_eq(f, file))
            .unwrap();
        opened_files.swap_remove(pos);
    }

    pub(self) fn with_producer_locked<F>(&self, file: &Arc<EvdevFileInner>, f: F)
    where
        F: FnOnce(&mut RbProducer<EvdevEvent>),
    {
        let mut opened_files = self.opened_files.disable_irq().lock();
        let pos = opened_files
            .iter()
            .position(|(f, _)| Arc::ptr_eq(f, file))
            .unwrap();
        f(&mut opened_files[pos].1)
    }

    /// Distributes events to all opened evdev files.
    fn pass_events(&self, events: &[InputEvent]) {
        // No need to disable IRQs because this method can only be called in the interrupt context.
        let mut opened_files = self.opened_files.lock();

        // Send events to all opened evdev files using their producers.
        for (file, producer) in opened_files.iter_mut() {
            for event in events {
                // Read the current time according to the opened evdev file's clock type.
                let time = file.read_clock();

                // When the buffer is full and a new event arrives, Linux drops all unconsumed
                // events and queues a `SYN_DROPPED` event with the new one [1].
                //
                // We follow the Linux implementation to try to drop unconsumed events. However, if
                // there is a concurrent consumer, `try_clear_with_producer_locked` may not be able
                // to make progress because we're in the interrupt context. So we will always push
                // a `SYN_DROPPED` event when the buffer is almost full to indicate that events are
                // about to be dropped. This should match the correct semantics of the
                // `SYN_DROPPED` event [2].
                //
                // [1]: https://elixir.bootlin.com/linux/v6.17.9/source/drivers/input/evdev.c#L221-L225
                // [2]: https://elixir.bootlin.com/linux/v6.17.9/source/Documentation/input/event-codes.rst#L113-L118
                if producer.free_len() <= 1 {
                    file.try_clear_with_producer_locked();

                    let dropped_event = EvdevEvent::from_event_and_time(
                        &InputEvent::from_sync_event(SynEvent::Dropped),
                        time,
                    );
                    // This fails if the buffer is full and `try_clear_with_producer_locked` cannot
                    // make progress. A `SYN_DROPPED` event must have already been pushed.
                    if producer.push(dropped_event).is_some() {
                        file.increment_packet_count();
                    }
                }

                let timed_event = EvdevEvent::from_event_and_time(event, time);
                if is_syn_dropped_event(&timed_event) {
                    // This is a bug in the device driver. We ignore the event to prevent bugs in
                    // the device drivers from breaking the invariant of the packet count.
                    ostd::warn!(
                        "Received dropped event from evdev device '{}'",
                        self.device.name()
                    );
                    continue;
                }
                if producer.push(timed_event).is_some() && is_syn_report_event(&timed_event) {
                    file.increment_packet_count();
                }
            }
        }
    }

    /// Creates a new opened evdev file for this evdev device.
    fn create_file(self: &Arc<Self>, buffer_size: usize) -> Result<Box<EvdevFile>> {
        // Create the evdev file and get the producer.
        let (file, producer) = EvdevFile::new(buffer_size, Arc::downgrade(self));

        // Attach the opened evdev file to this device.
        self.attach_file(file.inner().clone(), producer);

        Ok(Box::new(file))
    }
}

impl InputHandler for EvdevDevice {
    fn handle_events(&self, events: &[InputEvent]) {
        self.pass_events(events);
    }
}

/// The `input` class: evdev devices published through the device model, so
/// that `/sys/class/input/eventX` and the `/sys/dev/char/13:X` entry exist
/// and user-space device enumeration (libudev, hence libinput and Xorg) can
/// discover them.
struct InputClass;

impl Class for InputClass {
    const NAME: &'static str = "input";
    type Device = Arc<EvdevDevice>;

    fn devnode(&self, dev: &ClassDevice<Self>) -> Option<DevNode> {
        // Only the `eventN` nodes have a device number and thus a `/dev`
        // node; the `inputN` devices exist for sysfs alone, as in Linux.
        dev.base().devnum()?;
        // Linux places evdev nodes at /dev/input/eventX.
        Some(DevNode {
            path: Some(aster_device::SysStr::from(format!(
                "input/{}",
                dev.base().name()
            ))),
            mode: None,
        })
    }
}

/// An evdev device, as seen by the char registry.
type EvdevClassDevice = ClassDevice<InputClass>;

/// The payload of an input device's sysfs attribute groups: the registered
/// input device itself.
type InputAttrPayload = Arc<dyn InputDevice>;

/// An input device's sysfs attribute group.
type InputAttrGroup = AttrGroupDevice<InputAttrPayload>;

/// Writes a capability bitmap the way Linux formats its sysfs bitmap
/// attributes: hexadecimal 64-bit words separated by spaces, most
/// significant word first, with leading zero words dropped. This is the
/// format udev's `input_id` builtin parses to classify an input device: it
/// splits from the right, so the leftmost word is the highest one.
fn write_bitmap(bitmap: &[u8], w: &mut dyn core::fmt::Write) -> aster_device::Result<()> {
    let mut words: Vec<u64> = bitmap
        .chunks(8)
        .map(|word| {
            let mut buffer = [0u8; 8];
            buffer[..word.len()].copy_from_slice(word);
            u64::from_le_bytes(buffer)
        })
        .collect();
    while let Some(&0) = words.last() {
        words.pop();
    }
    if words.is_empty() {
        write!(w, "0").map_err(|_| DeviceError::Format)?;
        return Ok(());
    }
    for (i, &word) in words.iter().enumerate().rev() {
        if i + 1 < words.len() {
            write!(w, " ").map_err(|_| DeviceError::Format)?;
        }
        write!(w, "{:x}", word).map_err(|_| DeviceError::Format)?;
    }
    Ok(())
}

/// The `capabilities/` group: which event types, keys, and relative axes the
/// device reports. udev's `input_id` builtin reads these bitmaps to assign
/// the `ID_INPUT_*` properties that libinput and Xorg select devices by.
const CAPABILITIES_ATTRS: &[Attr<InputAttrGroup>] = &[
    Attr::ro("ev", |dev, w| {
        write!(w, "{:x}", dev.payload().capability().event_types_bits())
            .map_err(|_| DeviceError::Format)
    }),
    Attr::ro("key", |dev, w| {
        write_bitmap(dev.payload().capability().supported_keys_bitmap(), w)
    }),
    Attr::ro("rel", |dev, w| {
        write_bitmap(
            dev.payload().capability().supported_relative_axes_bitmap(),
            w,
        )
    }),
];

/// The `id/` group: the input device identifier, as in Linux.
const ID_ATTRS: &[Attr<InputAttrGroup>] = &[
    Attr::ro("bustype", |dev, w| {
        writeln!(w, "{:04x}", dev.payload().id().bustype()).map_err(|_| DeviceError::Format)
    }),
    Attr::ro("vendor", |dev, w| {
        writeln!(w, "{:04x}", dev.payload().id().vendor()).map_err(|_| DeviceError::Format)
    }),
    Attr::ro("product", |dev, w| {
        writeln!(w, "{:04x}", dev.payload().id().product()).map_err(|_| DeviceError::Format)
    }),
    Attr::ro("version", |dev, w| {
        writeln!(w, "{:04x}", dev.payload().id().version()).map_err(|_| DeviceError::Format)
    }),
];

/// The input device's own attributes, as on Linux's `inputN` devices.
const INPUT_DEVICE_ATTRS: &[Attr<EvdevClassDevice>] = &[
    Attr::ro("name", |dev, w| {
        writeln!(w, "{}", dev.payload().device.name()).map_err(|_| DeviceError::Format)
    }),
    Attr::ro("phys", |dev, w| {
        writeln!(w, "{}", dev.payload().device.phys()).map_err(|_| DeviceError::Format)
    }),
    Attr::ro("uniq", |dev, w| {
        writeln!(w, "{}", dev.payload().device.uniq()).map_err(|_| DeviceError::Format)
    }),
    // No input properties (INPUT_PROP_*) are tracked yet, so report the
    // empty bitmap, as Linux does for devices without properties.
    Attr::ro("properties", |_dev, w| {
        writeln!(w, "0").map_err(|_| DeviceError::Format)
    }),
];

impl Device for EvdevClassDevice {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        self.base()
            .devnum()
            .expect("evdev devices always have a device number")
            .id()
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        // The device model creates the node when the device is added.
        None
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        // The class-device payload is the registered evdev device itself.
        let evdev = self.payload().clone();
        let file = evdev.create_file(EVDEV_BUFFER_SIZE)?;
        Ok(file as Box<dyn PerOpenFileOps>)
    }
}

/// The evdev handler class that creates device nodes for input devices.
#[derive(Debug)]
struct EvdevHandlerClass;

/// The registered `input` class.
static INPUT_CLASS: Once<Arc<ClassHandle<InputClass>>> = Once::new();

impl InputHandlerClass for EvdevHandlerClass {
    fn name(&self) -> &str {
        "evdev"
    }

    fn connect(&self, dev: Arc<dyn InputDevice>) -> Result<Arc<dyn InputHandler>, ConnectError> {
        // Allocate a new minor number.
        let minor = EVDEV_MINOR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let minor_id = MinorId::new(minor);

        // Create an evdev device.
        let evdev = Arc::new(EvdevDevice::new(minor, dev.clone()));

        // Publish through the device model, mirroring Linux's sysfs layout:
        // an `inputN` device carrying the identity attributes (`name`,
        // `properties`, and the `capabilities/` and `id/` groups), with the
        // `eventN` character device below it. udev's `input_id` builtin
        // classifies the device by reading those bitmaps; without them no
        // `ID_INPUT` property is assigned and Xorg ignores the device.
        let class = INPUT_CLASS.call_once(|| {
            aster_device::register_class(InputClass)
                .expect("the `input` class must not be registered twice")
        });
        let input = ClassDevice::builder(class, format!("input{}", minor), evdev.clone())
            .attrs(INPUT_DEVICE_ATTRS)
            .build();
        aster_device::add(&input).map_err(|_| ConnectError::InternalError)?;

        // The groups are attached before the `eventN` uevent is emitted, so
        // that the device is fully classified by the time user space reacts.
        let capabilities = AttrGroupDevice::with_parent(
            "capabilities",
            input.clone(),
            dev.clone(),
            CAPABILITIES_ATTRS,
        );
        if aster_device::add(&capabilities).is_err() {
            let _ = aster_device::remove(&input);
            return Err(ConnectError::InternalError);
        }
        let id_group = AttrGroupDevice::with_parent("id", input.clone(), dev.clone(), ID_ATTRS);
        if aster_device::add(&id_group).is_err() {
            let _ = aster_device::remove(&capabilities);
            let _ = aster_device::remove(&input);
            return Err(ConnectError::InternalError);
        }

        let id = DeviceId::new(MajorId::new(EVDEV_MAJOR_ID), minor_id);
        let device = ClassDevice::builder(class, format!("event{}", minor), evdev.clone())
            .parent(input.clone())
            .devnum(aster_device::DevNum::char(id))
            .build();
        if aster_device::add(&device).is_err() {
            let _ = aster_device::remove(&id_group);
            let _ = aster_device::remove(&capabilities);
            let _ = aster_device::remove(&input);
            return Err(ConnectError::InternalError);
        }

        // The char registry routes `open` to the device.
        if register(device.clone()).is_err() {
            let _ = aster_device::remove(&device);
            let _ = aster_device::remove(&id_group);
            let _ = aster_device::remove(&capabilities);
            let _ = aster_device::remove(&input);
            return Err(ConnectError::InternalError);
        }

        EVDEV_SYSFS_NODES.lock().insert(
            minor_id,
            SysfsNodes {
                event: device,
                input,
                capabilities,
                id: id_group,
            },
        );

        // Add to our registry for looking up during disconnection.
        EVDEV_DEVICES.lock().insert(minor_id, evdev.clone());

        // Return the device as a handler instance.
        Ok(evdev as _)
    }

    fn disconnect(&self, dev: &Arc<dyn InputDevice>) {
        let mut devices = EVDEV_DEVICES.lock();
        let device_name = dev.name();

        // Find the device by checking if it matches the input device.
        let mut found_minor = None;
        for (minor, evdev) in devices.iter() {
            if evdev.matches_input_device(dev) {
                found_minor = Some(*minor);
                break;
            }
        }

        let Some(minor) = found_minor else {
            ostd::warn!(
                "Attempted to disconnect device '{}' but it did not connect to evdev",
                device_name
            );
            return;
        };

        let evdev = devices.remove(&minor).unwrap();
        let device_id = evdev.id;

        // Unregister from the char device subsystem.
        if let Err(err) = unregister(device_id) {
            ostd::warn!(
                "Failed to unregister evdev device '{}' (minor: {}): {:?}",
                device_name,
                minor.get(),
                err
            );
        }
        // Remove the device-model nodes children-first: the model refuses to
        // remove a device that still has children.
        if let Some(nodes) = EVDEV_SYSFS_NODES.lock().remove(&minor) {
            let _ = aster_device::remove(&nodes.event);
            let _ = aster_device::remove(&nodes.capabilities);
            let _ = aster_device::remove(&nodes.id);
            let _ = aster_device::remove(&nodes.input);
        }

        // TODO: Implement device node deletion when the functionality is available.
        ostd::info!(
            "Disconnected evdev device '{}' (minor: {}), device node /dev/input/event{} still exists",
            device_name,
            minor.get(),
            minor.get()
        );
    }
}

pub(super) fn init_in_first_kthread() {
    use aster_input::input_handler::RegisteredInputHandlerClass;

    static EVDEV_MAJOR: Once<MajorIdOwner> = Once::new();
    EVDEV_MAJOR.call_once(|| acquire_major(MajorId::new(EVDEV_MAJOR_ID)).unwrap());

    static REGISTERED_EVDDEV_CLASS: Once<RegisteredInputHandlerClass> = Once::new();
    let handler_class = Arc::new(EvdevHandlerClass);
    let handle = aster_input::register_handler_class(handler_class);
    REGISTERED_EVDDEV_CLASS.call_once(|| handle);

    ostd::info!("Evdev device support initialized");
}
