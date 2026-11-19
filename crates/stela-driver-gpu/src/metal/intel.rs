//! Intel GPUs under Stela's Metal device: xe or i915 in the kernel and ANV,
//! with BRW, as the Vulkan driver over either one's uAPI.
//!
//! Hardware access, firmware, memory management, command submission and
//! shader code generation stay with the kernel driver, ANV and BRW; this
//! module only admits a device and supplies the facts Vulkan does not
//! report, as the Intel stacks' part of [`open`](super::open).
use super::{
    abi::DeviceLocation,
    capabilities::Platform,
    open::{self, Admission, Attachment},
    vulkan::{
        DriverId, PhysicalDeviceType,
        physical::{HeapLayout, Identity},
    },
};
use alloc::string::String;

pub const VENDOR_ID: u32 = 0x8086;

/// Why a Vulkan device is not admitted as an Intel Metal device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    NotIntel {
        vendor_id: u32,
    },
    /// Only ANV is integrated with Stela's Intel stacks.
    NotAnv(DriverId),
    /// The kernel driver's table does not hold the device it reports.
    UnknownDevice {
        device: u32,
    },
    /// The Vulkan device is not the one the kernel driver bound.
    OtherDevice {
        vulkan: u32,
        kernel: u32,
    },
    /// Neither an integrated nor a discrete GPU.
    NotGpu(PhysicalDeviceType),
}

/// ISL lays out an explicitly pitched linear image (ANV's
/// `DRM_FORMAT_MOD_LINEAR`, which asks no display layout) with its row pitch
/// and base address a multiple of the element size when it may be a render
/// target, and of one byte otherwise (`isl_calc_row_pitch_alignment`,
/// `isl_calc_base_alignment`): 16 bytes covers every format's element.
const LINEAR_ROW_ALIGNMENT: u64 = 16;
const LINEAR_OFFSET_ALIGNMENT: u64 = 16;
/// ANV binds sparse resources in 64 KiB blocks (`ANV_SPARSE_BLOCK_SIZE`),
/// which are Vulkan's standard sparse block shapes' size.
const SPARSE_PAGE_BYTES: u64 = 64 << 10;

/// What the kernel driver's table says of an admitted device: its platform's
/// name and whether it has a display engine.
struct Known {
    name: &'static str,
    display: bool,
}

/// Admits the Vulkan device `identity` whose kernel driver xe reports the
/// PCI device ID `device` (`DRM_XE_QUERY_CONFIG_REV_AND_DEVICE_ID`).
#[cfg(feature = "xe-port")]
pub fn admit_device(
    identity: &Identity,
    device: u32,
    attachment: Attachment,
) -> Result<Platform, Refusal> {
    admit_known(identity, device, attachment, |device| {
        let platform = u16::try_from(device)
            .ok()
            .and_then(crate::xe::platform)
            .ok_or(Refusal::UnknownDevice { device })?;
        Ok(Known {
            name: platform.name,
            display: platform.display,
        })
    })
}

/// Admits the Vulkan device `identity` whose kernel driver i915 reports the
/// PCI device ID `device` (`I915_PARAM_CHIPSET_ID`): one of the GPUs i915 is
/// handed ([`crate::i915::handed`]). Every platform of those has a display
/// engine.
#[cfg(feature = "i915-port")]
pub fn admit_i915_device(
    identity: &Identity,
    device: u32,
    attachment: Attachment,
) -> Result<Platform, Refusal> {
    admit_known(identity, device, attachment, |device| {
        let platform = u16::try_from(device)
            .ok()
            .and_then(crate::i915::handed)
            .ok_or(Refusal::UnknownDevice { device })?;
        Ok(Known {
            name: platform.name,
            display: true,
        })
    })
}

/// Admits `identity` as ANV's device for the GPU the kernel driver bound,
/// which reports `device`, with what `known` says of it from that driver's
/// table.
fn admit_known(
    identity: &Identity,
    device: u32,
    attachment: Attachment,
    known: impl FnOnce(u32) -> Result<Known, Refusal>,
) -> Result<Platform, Refusal> {
    if identity.vendor_id != VENDOR_ID {
        return Err(Refusal::NotIntel {
            vendor_id: identity.vendor_id,
        });
    }
    if identity.driver_id != DriverId::IntelOpenSourceMesa {
        return Err(Refusal::NotAnv(identity.driver_id));
    }
    if identity.device_id != device {
        return Err(Refusal::OtherDevice {
            vulkan: identity.device_id,
            kernel: device,
        });
    }
    let platform = known(device)?;
    let (location, max_transfer_rate) = match identity.device_type {
        // Built into the processor, on its fabric rather than a link.
        PhysicalDeviceType::IntegratedGpu => (DeviceLocation::BuiltIn, 0),
        PhysicalDeviceType::DiscreteGpu if attachment.external => {
            (DeviceLocation::External, attachment.link.bytes_per_second())
        }
        PhysicalDeviceType::DiscreteGpu => {
            (DeviceLocation::Slot, attachment.link.bytes_per_second())
        }
        other => return Err(Refusal::NotGpu(other)),
    };
    Ok(Platform {
        architecture: String::from(platform.name),
        location,
        location_number: attachment.location_number,
        max_transfer_rate,
        headless: !platform.display,
        removable: attachment.external,
        linear_row_alignment: LINEAR_ROW_ALIGNMENT,
        linear_offset_alignment: LINEAR_OFFSET_ALIGNMENT,
        sparse_page_bytes: SPARSE_PAGE_BYTES,
        heaps: HeapLayout::Disjoint,
    })
}

/// The xe stack's admission: the Vulkan device must be ANV's for the
/// device xe bound, which xe's table holds.
#[cfg(feature = "xe-port")]
pub const ADMISSION: Admission = Admission { admit };

#[cfg(feature = "xe-port")]
fn admit(
    identity: &Identity,
    chip: u32,
    attachment: Attachment,
) -> Result<Platform, open::Refusal> {
    admit_device(identity, chip, attachment).map_err(open::Refusal::Intel)
}

/// The i915 stack's admission: the Vulkan device must be ANV's for the
/// device i915 bound, one of the Skylake to Ice Lake GPUs it is handed.
#[cfg(feature = "i915-port")]
pub const I915_ADMISSION: Admission = Admission { admit: admit_i915 };

#[cfg(feature = "i915-port")]
fn admit_i915(
    identity: &Identity,
    chip: u32,
    attachment: Attachment,
) -> Result<Platform, open::Refusal> {
    admit_i915_device(identity, chip, attachment).map_err(open::Refusal::Intel)
}

/// ANV as linked into the image, under its own loader entry
/// (`xtask/src/gpu/nvk.rs`), with its NIR options
/// (`compat/mesa/anv_nir_options.c`). It has neither of NVK's
/// driver-internal hooks, so it gets [`super::native::Driver`]'s documented
/// fallbacks: a commit's submission runs on the caller, and a refused
/// submission loses the device. An image whose build host lacked the tools
/// ANV's build runs holds no ANV, and then no driver here.
#[cfg(all(
    feature = "metal-linked",
    not(test),
    target_os = "uefi",
    stela_vulkan_intel
))]
pub const DRIVER: super::native::Driver = {
    use super::native::{Driver, vk};
    use core::ffi::{c_char, c_void};
    unsafe extern "system" {
        fn anv_icdGetInstanceProcAddr(
            instance: vk::Instance,
            name: *const c_char,
        ) -> vk::PFN_vkVoidFunction;
    }
    unsafe extern "C" {
        fn stela_anv_nir_options(physical: vk::PhysicalDevice, stage: u32) -> *const c_void;
    }
    Driver {
        get_instance_proc_addr: anv_icdGetInstanceProcAddr,
        nir_options: stela_anv_nir_options,
        submit_thread: None,
        refusals: None,
        // xe's and i915's closes of a file wait for no recovery: they kill
        // its queues and contexts, and the memory goes when its last fence
        // signals.
        keep_lost: false,
        // A GPU xe or i915 left with engines that do not run, or that hangs
        // again after their recovery, is found in 2 s rather than by their
        // job timeout and heartbeat, as every client's work waits on it.
        check_first_submission: true,
    }
};

#[cfg(test)]
mod tests;
