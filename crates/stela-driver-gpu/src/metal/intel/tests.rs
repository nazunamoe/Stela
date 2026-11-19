use super::*;
use crate::metal::open::PcieLink;

/// ANV's identity of Tiger Lake's UHD Graphics 0x9a49.
fn tiger_lake() -> Identity {
    Identity {
        api_version: (1 << 22) | (4 << 12),
        driver_id: DriverId::IntelOpenSourceMesa,
        driver_version: 0,
        vendor_id: VENDOR_ID,
        device_id: 0x9a49,
        pipeline_cache_uuid: [0; 16],
        device_type: PhysicalDeviceType::IntegratedGpu,
        device_name: String::from("Intel(R) Iris(R) Xe Graphics (TGL GT2)"),
        conformance_version: [1, 4, 0, 0],
        pci_bus: None,
    }
}

fn attachment(external: bool) -> Attachment {
    Attachment {
        link: PcieLink {
            speed: 4,
            width: 16,
        },
        external,
        location_number: 1,
    }
}

#[cfg(feature = "xe-port")]
#[test]
fn an_integrated_gpu_is_built_in_with_xes_platform_name() {
    let platform = admit_device(&tiger_lake(), 0x9a49, attachment(false)).unwrap();
    assert_eq!(platform.architecture, "TIGERLAKE");
    assert_eq!(platform.location, DeviceLocation::BuiltIn);
    // A built-in GPU reports no link bandwidth, whatever the function says.
    assert_eq!(platform.max_transfer_rate, 0);
    assert!(!platform.headless && !platform.removable);
    assert_eq!(platform.location_number, 1);
    assert_eq!(
        (
            platform.linear_row_alignment,
            platform.linear_offset_alignment
        ),
        (16, 16)
    );
    assert_eq!(platform.sparse_page_bytes, 64 << 10);
}

#[cfg(feature = "xe-port")]
#[test]
fn a_discrete_gpu_is_in_a_slot_or_external_with_its_link() {
    let mut arc = tiger_lake();
    arc.device_id = 0x56a0;
    arc.device_type = PhysicalDeviceType::DiscreteGpu;
    let slot = admit_device(&arc, 0x56a0, attachment(false)).unwrap();
    assert_eq!(slot.architecture, "DG2");
    assert_eq!(slot.location, DeviceLocation::Slot);
    assert_eq!(
        slot.max_transfer_rate,
        attachment(false).link.bytes_per_second()
    );
    let external = admit_device(&arc, 0x56a0, attachment(true)).unwrap();
    assert_eq!(external.location, DeviceLocation::External);
    assert!(external.removable);
    // The Data Center GPU Flex 170 has no display engine.
    arc.device_id = 0x56c0;
    assert!(
        admit_device(&arc, 0x56c0, attachment(false))
            .unwrap()
            .headless
    );
}

#[cfg(feature = "xe-port")]
#[test]
fn other_devices_and_drivers_are_refused_with_the_reason() {
    let identity = tiger_lake();
    assert_eq!(
        admit_device(&identity, 0x46a6, attachment(false)).err(),
        Some(Refusal::OtherDevice {
            vulkan: 0x9a49,
            kernel: 0x46a6
        })
    );
    let mut nvidia = tiger_lake();
    nvidia.vendor_id = 0x10de;
    assert_eq!(
        admit_device(&nvidia, 0x9a49, attachment(false)).err(),
        Some(Refusal::NotIntel { vendor_id: 0x10de })
    );
    let mut venus = tiger_lake();
    venus.driver_id = DriverId::MesaVenus;
    assert_eq!(
        admit_device(&venus, 0x9a49, attachment(false)).err(),
        Some(Refusal::NotAnv(DriverId::MesaVenus))
    );
    // Ice Lake is not xe's.
    let mut ice_lake = tiger_lake();
    ice_lake.device_id = 0x8a56;
    assert_eq!(
        admit_device(&ice_lake, 0x8a56, attachment(false)).err(),
        Some(Refusal::UnknownDevice { device: 0x8a56 })
    );
    let mut cpu = tiger_lake();
    cpu.device_type = PhysicalDeviceType::Cpu;
    assert_eq!(
        admit_device(&cpu, 0x9a49, attachment(false)).err(),
        Some(Refusal::NotGpu(PhysicalDeviceType::Cpu))
    );
    assert_eq!(
        (ADMISSION.admit)(&cpu, 0x9a49, attachment(false)).err(),
        Some(open::Refusal::Intel(Refusal::NotGpu(
            PhysicalDeviceType::Cpu
        )))
    );
}

#[cfg(feature = "i915-port")]
#[test]
fn the_gpus_i915_is_handed_are_admitted_with_i915s_platform_name() {
    // Ice Lake's Iris Plus Graphics G7, as ANV reports it over i915.
    let mut ice_lake = tiger_lake();
    ice_lake.device_id = 0x8a52;
    let platform = admit_i915_device(&ice_lake, 0x8a52, attachment(false)).unwrap();
    assert_eq!(platform.architecture, "ICELAKE");
    assert_eq!(platform.location, DeviceLocation::BuiltIn);
    assert!(!platform.headless && platform.max_transfer_rate == 0);
    // Skylake's HD Graphics 520.
    let mut skylake = tiger_lake();
    skylake.device_id = 0x1916;
    assert_eq!(
        admit_i915_device(&skylake, 0x1916, attachment(false))
            .unwrap()
            .architecture,
        "SKYLAKE"
    );
    // Tiger Lake, which i915 is offered first.
    assert_eq!(
        admit_i915_device(&tiger_lake(), 0x9a49, attachment(false))
            .unwrap()
            .architecture,
        "TIGERLAKE"
    );
    // Broadwell, which no driver is handed, and an ID of no table are no
    // devices of i915's here.
    for device in [0x1616, 0x0001] {
        let mut other = tiger_lake();
        other.device_id = device;
        assert_eq!(
            admit_i915_device(&other, device, attachment(false)).err(),
            Some(Refusal::UnknownDevice { device }),
            "{device:04x}"
        );
    }
    let mut broadwell = tiger_lake();
    broadwell.device_id = 0x1616;
    assert_eq!(
        (I915_ADMISSION.admit)(&broadwell, 0x1616, attachment(false)).err(),
        Some(open::Refusal::Intel(Refusal::UnknownDevice {
            device: 0x1616
        }))
    );
}
