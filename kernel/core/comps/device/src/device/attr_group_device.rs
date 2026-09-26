// SPDX-License-Identifier: MPL-2.0

//! Attribute groups: directories of attributes under a device.

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use super::{AnyDevice, DevNode, DeviceBase, DeviceInternals, Subsystem, impl_device_node};
use crate::{
    SysStr,
    attr::{Attr, TyErasedAttr},
    uevent::UeventVars,
};

/// A group of attributes exposed as a directory under a device, such as the
/// `capabilities/` and `id/` groups of an input device.
///
/// It has neither bus nor class: it is listed in no index, sends no uevents,
/// and gets no `/dev` node. It exists so that user space can read related
/// attributes together at a subdirectory path, as Linux sysfs does.
pub struct AttrGroupDevice<P: Send + Sync + 'static> {
    base: DeviceBase,
    payload: P,
    attrs: &'static [Attr<Self>],
}

impl<P: Send + Sync + 'static> AttrGroupDevice<P> {
    /// Creates an attribute group under `parent`.
    ///
    /// It is not registered until [`add`](super::add) is called.
    pub fn with_parent(
        name: impl Into<SysStr>,
        parent: Arc<dyn AnyDevice>,
        payload: P,
        attrs: &'static [Attr<Self>],
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<Self>| {
            let weak_self: Weak<dyn AnyDevice> = weak.clone();
            Self {
                base: DeviceBase::new(name.into(), Some(parent), None, weak_self),
                payload,
                attrs,
            }
        })
    }

    /// Returns the payload.
    pub fn payload(&self) -> &P {
        &self.payload
    }
}

impl<P: Send + Sync + 'static> core::fmt::Debug for AttrGroupDevice<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AttrGroupDevice")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl<P: Send + Sync + 'static> AnyDevice for AttrGroupDevice<P> {
    fn base(&self) -> &DeviceBase {
        &self.base
    }

    fn subsystem(&self) -> Subsystem {
        Subsystem::bare()
    }

    fn driver_name(&self) -> Option<String> {
        None
    }
}

impl<P: Send + Sync + 'static> DeviceInternals for AttrGroupDevice<P> {
    fn type_name(&self) -> Option<&'static str> {
        None
    }

    fn attr_groups(&self) -> Vec<TyErasedAttr> {
        TyErasedAttr::from_typed_slice(self.attrs)
    }

    fn subsystem_uevent(&self, _vars: &mut UeventVars) {}

    fn devnode_override(&self) -> Option<DevNode> {
        None
    }
}

impl_device_node!(AttrGroupDevice, (P: Send + Sync + 'static), (P));
