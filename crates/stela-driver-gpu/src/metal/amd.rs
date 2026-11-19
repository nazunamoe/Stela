//! AMD GPUs under Stela's Metal device: amdgpu in the kernel and RADV, with
//! ACO, as the Vulkan driver.
//!
//! Hardware access, firmware, memory management, command submission and
//! shader code generation stay with amdgpu, RADV and ACO; this module only
//! admits a device and supplies the facts Vulkan does not report, as the AMD
//! stack's part of [`open`](super::open). Features, limits and formats are
//! what RADV reports, as for every stack.
#[cfg(feature = "amdgpu-port")]
use super::open::{self, Admission};
use super::{
    abi::DeviceLocation,
    capabilities::Platform,
    open::Attachment,
    vulkan::{
        DriverId, PhysicalDeviceType,
        physical::{HeapLayout, Identity},
    },
};
use alloc::string::String;

pub const VENDOR_ID: u32 = 0x1002;

/// Why a Vulkan device is not admitted as an AMD Metal device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    NotAmd {
        vendor_id: u32,
    },
    /// Only RADV is integrated with Stela's amdgpu winsys.
    NotRadv(DriverId),
    /// amdgpu's interface names no such family (`AMDGPU_FAMILY_*`).
    UnknownFamily {
        family: u32,
    },
    /// Neither an integrated nor a discrete GPU.
    NotGpu(PhysicalDeviceType),
}

/// addrlib lays out a linear image with a row pitch of 256 bytes' worth of
/// elements on GFX9 to GFX11, 128 on GFX12 and at most 128 before GFX9
/// (`ac_surface_get_pitch_align`); an explicit pitch must be a multiple of
/// it. 256 bytes satisfies every generation. GFX10 alone takes no pitch but
/// its own (`ac_surface_override_offset_stride`), so there an explicitly
/// pitched linear image whose pitch is not the 256-byte-aligned row is
/// refused by RADV.
const LINEAR_ROW_ALIGNMENT: u64 = 256;
/// An image descriptor holds its base address in 256-byte units
/// (`ac_build_texture_descriptor`), so a linear image starts 256-byte
/// aligned in its memory.
const LINEAR_OFFSET_ALIGNMENT: u64 = 256;
/// RADV binds sparse resources in the GPU's 64 KiB pages
/// (`RADV_SPARSE_BUFFER_ALIGNMENT`, the image mip tail's size), Vulkan's
/// standard sparse block shapes' size.
const SPARSE_PAGE_BYTES: u64 = 64 << 10;

/// Admits the Vulkan device `identity` whose kernel driver reports the
/// family `family` (`drm_amdgpu_info_device::family`), named as amdgpu's
/// interface names it.
#[cfg(feature = "amdgpu-port")]
pub fn admit_family(
    identity: &Identity,
    family: u32,
    attachment: Attachment,
) -> Result<Platform, Refusal> {
    admit_named(
        identity,
        family,
        crate::amdgpu::family_name(family),
        attachment,
    )
}

/// Admits the Vulkan device `identity` of the family `family`, which
/// amdgpu's interface (`AMDGPU_FAMILY_*` in `amdgpu_drm.h`) names `name`;
/// `None` when it names no such family.
pub fn admit_named(
    identity: &Identity,
    family: u32,
    name: Option<&str>,
    attachment: Attachment,
) -> Result<Platform, Refusal> {
    if identity.vendor_id != VENDOR_ID {
        return Err(Refusal::NotAmd {
            vendor_id: identity.vendor_id,
        });
    }
    if identity.driver_id != DriverId::MesaRadv {
        return Err(Refusal::NotRadv(identity.driver_id));
    }
    let name = name.ok_or(Refusal::UnknownFamily { family })?;
    let (location, max_transfer_rate) = match identity.device_type {
        // Part of the processor, on its fabric rather than a link.
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
        architecture: String::from(name),
        location,
        location_number: attachment.location_number,
        max_transfer_rate,
        // amdgpu's interface does not say whether a GPU has a display
        // engine. Every GPU RADV drives has one except the Vega 20 and
        // later compute accelerators, which have no outputs either way.
        headless: false,
        removable: attachment.external,
        linear_row_alignment: LINEAR_ROW_ALIGNMENT,
        linear_offset_alignment: LINEAR_OFFSET_ALIGNMENT,
        sparse_page_bytes: SPARSE_PAGE_BYTES,
        heaps: HeapLayout::Disjoint,
    })
}

/// The AMD stack's admission: the Vulkan device must be RADV's, for a
/// family amdgpu's interface names.
#[cfg(feature = "amdgpu-port")]
pub const ADMISSION: Admission = Admission { admit };

#[cfg(feature = "amdgpu-port")]
fn admit(
    identity: &Identity,
    chip: u32,
    attachment: Attachment,
) -> Result<Platform, open::Refusal> {
    admit_family(identity, chip, attachment).map_err(open::Refusal::Amd)
}

/// RADV as linked into the image, under its own loader entry
/// (`xtask/src/gpu/nvk.rs`), with its driver-internal calls
/// (`compat/mesa/radv_nir_options.c`, `compat/mesa/radv_submit.c`). A
/// commit's submission runs on Mesa's submit thread, where Linux runs, not
/// on its PE. A submission amdgpu refuses short of losing the context fails
/// its command buffer, not the device (Mesa patch 0038); a lost context
/// loses the device, which the service reopens. An image whose build host
/// lacked the tools RADV's build runs holds no RADV, and then no driver
/// here.
#[cfg(all(
    feature = "metal-linked",
    not(test),
    target_os = "uefi",
    stela_vulkan_amd
))]
pub const DRIVER: super::native::Driver = {
    use super::native::{Driver, Refusals, vk};
    use core::ffi::{c_char, c_void};
    unsafe extern "system" {
        fn radv_icdGetInstanceProcAddr(
            instance: vk::Instance,
            name: *const c_char,
        ) -> vk::PFN_vkVoidFunction;
    }
    unsafe extern "C" {
        fn stela_radv_nir_options(physical: vk::PhysicalDevice, stage: u32) -> *const c_void;
        fn stela_radv_submit_thread(queue: vk::Queue) -> vk::Result;
        fn stela_radv_submit_failure(
            queue: vk::Queue,
            semaphore: vk::Semaphore,
            value: u64,
        ) -> vk::Result;
        fn stela_radv_forget_failures(queue: vk::Queue) -> u32;
        fn stela_radv_count_failures(queue: vk::Queue, outstanding: *mut u32);
    }
    Driver {
        get_instance_proc_addr: radv_icdGetInstanceProcAddr,
        nir_options: stela_radv_nir_options,
        // A commit's submission runs where Linux does, not on its PE.
        submit_thread: Some(stela_radv_submit_thread),
        // A refused submission fails its command buffer, not the device.
        refusals: Some(Refusals {
            take: stela_radv_submit_failure,
            forget: stela_radv_forget_failures,
            count: stela_radv_count_failures,
        }),
        // amdgpu's recovery from a hung GPU holds back the VM work closing
        // RADV's file waits for.
        keep_lost: true,
        check_first_submission: true,
    }
};

#[cfg(test)]
mod tests;
