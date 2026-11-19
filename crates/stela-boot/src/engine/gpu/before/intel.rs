//! The Intel stacks' part of the claim: xe's, i915's and the display code's
//! needs of the boot, in an image built with the Intel port.
use super::{Before, Identified, Observed, Searched, on_usb, select_firmware};
use crate::output;
use alloc::{format, vec::Vec};
use stela_boot_core::opregion;
use stela_crypto::{Sha256, digest::Digest};
use stela_drivers::{
    bus::{mmio::owned::Protection, pci::ConfigSpace},
    firmware::{self, Catalog, Image, Provenance, Provider, Requirement, Resource},
    gpu,
    probe::ServiceError,
};
use stela_loader::host_firmware::requirements::Inventory;

/// The Intel GPUs' part, for both drivers: i915 is offered a device first
/// where it is handed it (Skylake to Meteor Lake, DG1, DG2), as upstream
/// Linux binds those to i915 by default, and xe binds the device when
/// i915's probe fails; xe alone drives Lunar Lake, Battlemage and later. So
/// the claim prepares the device for each driver that may bind it: the
/// files of the platform, which serve both (`require_xe`), or i915's
/// display program on the older platforms; the PC's Video BIOS Table under
/// each driver's parameter; the stolen memory and the host bridge i915's
/// probe reads; and the HuC and GSC turned off. Nothing of the GPU's boots
/// by itself after a reset: the driver loads all of it.
pub(super) static INTEL: Before = Before {
    // The Video BIOS Table's word needs the OpRegion, which only the claim
    // reads; without it the drivers read the table from the OpRegion.
    ahead: |adapter| Drivers::of(adapter).words(adapter.device, false),
    admits: |adapter, catalog| {
        on_usb(
            &files(adapter.device)?,
            &format!("{:04x}", adapter.device),
            catalog,
        )
    },
    identify: |mut observed, catalog| {
        use stela_loader::host_firmware::recipes;
        let adapter = observed.adapter;
        let address = adapter.address;
        // Register 0 identifies nothing here; the read proves the window.
        observed
            .registers
            .read32(0)
            .map_err(ServiceError::Mapping)?;
        let drivers = Drivers::of(adapter);
        // The name people know the platform by, or i915's for the older
        // platforms.
        let chip = recipes::intel_gpu_platform(adapter.device)
            .or(drivers.i915.map(|platform| platform.name));
        let platform = chip.unwrap_or("this platform");
        match (drivers.i915.is_some(), drivers.xe) {
            (true, true) => output::arguments(format_args!(
                "GPU {address}: i915 is offered {platform} first, as upstream Linux binds it to \
                 i915; xe binds it if i915's probe fails\n"
            )),
            (false, true) if gpu::xe::force_probed(adapter.device) => {
                output::arguments(format_args!(
                    "GPU {address}: xe binds {platform} only by Linux's force probe, which \
                     Stela sets for every device of xe's table: upstream Linux calls xe \
                     experimental on it\n"
                ))
            }
            _ => {}
        }
        let inventory = files(adapter.device)?;
        let target = inventory
            .requests()
            .first()
            .ok_or(ServiceError::Refused(
                "the GPU has no pinned firmware recipe",
            ))?
            .view()
            .target;
        let table = video_bios_table(&mut observed, target);
        let name = format!("{:04x}", adapter.device);
        let names = drivers.names();
        // Both take it from Linux's early quirks where those name a method:
        // integrated graphics before Meteor Lake (xe through patch 0230).
        // Meteor Lake and later and the discrete GPUs find theirs through
        // BAR 2 themselves.
        let stolen = (gpu::intel_stolen::method(adapter.device).is_some()
            && (drivers.i915.is_some() || gpu::xe::stolen_from_early_quirks(adapter.device)))
        .then(|| graphics_stolen(&mut observed, names))
        .flatten();
        let mut searched = chipset_bridges(&observed, names);
        if drivers.i915.is_some() {
            searched.extend(host_bridge(&observed));
        }
        Ok(Identified {
            firmware: select_firmware(&inventory, &name, catalog, table.map(|table| (table, VBT)))?,
            identity: None,
            chip,
            parameters: Some(drivers.parameters(adapter.device, table.is_some(), address)),
            searched,
            stolen,
        })
    },
    register_page: Protection::ReadOnly,
    settle: |_, _, _| Ok(()),
};

/// The Intel drivers that may bind a device, in the order Linux offers it
/// to them.
struct Drivers {
    /// What i915's table says of it, when i915 is handed it.
    i915: Option<gpu::i915::Platform>,
    /// Whether xe is handed it.
    xe: bool,
}

impl Drivers {
    fn of(adapter: &gpu::Adapter) -> Self {
        Self {
            i915: gpu::i915::handed(adapter.device),
            xe: gpu::xe::admits(adapter).is_ok(),
        }
    }

    /// The drivers' names, as the log lines give them.
    fn names(&self) -> &'static str {
        match (self.i915.is_some(), self.xe) {
            (true, true) => "i915 or xe",
            (true, false) => "i915",
            _ => "xe",
        }
    }

    /// The words of Linux's command line each driver that may bind `device`
    /// needs: the file it finds the PC's Video BIOS Table under, when the
    /// OpRegion holds one (the display code asks the firmware service for it
    /// before it looks anywhere else, `intel_bios.c`), and, on a platform of
    /// xe's table, an empty path for the HuC image and, except where the
    /// media GT's C6 needs it, the GSC image, which turns those controllers
    /// off. Stela runs no media engine and no content protection, so
    /// neither image is on the USB; an empty path makes xe and i915 mark
    /// each disabled at a debug level and start without it: no fetch and no
    /// HuC authentication to wait for. The GuC loads without the GSC on
    /// every platform. On Meteor Lake, Arrow Lake and Lunar Lake the GSC
    /// stays: its power well never sleeps without its firmware, so the
    /// media GT never enters C6 (`intel_engine_cs.c`, `xe_gsc_init`); it
    /// loads asynchronously and its proxy, which needs the MEI driver Linux
    /// here does not build, is refused at once without a wait. The older
    /// platforms' i915 loads neither (its default `enable_guc` is 0 there).
    /// One line for each controller a driver would load there.
    fn parameters(
        &self,
        device: u16,
        table: bool,
        address: impl core::fmt::Display,
    ) -> alloc::string::String {
        let controllers = gpu::xe::firmware(device);
        let kept = stela_loader::host_firmware::recipes::intel_gsc(device);
        let words = self.words(device, table);
        // i915 loads a HuC on every platform of xe's table it is handed but
        // Tiger and Rocket Lake, DG2's (through its GSC) among them, which
        // xe's table names none for.
        let i915 = self.i915.is_some();
        let left_out = controllers.into_iter().flat_map(|files| {
            [
                (files.huc.is_some() || i915)
                    .then_some("media controller (HuC): Stela runs no video engine"),
                files
                    .gsc
                    .filter(|_| kept.is_none())
                    .map(|_| "security controller (GSC): Stela runs no content protection"),
            ]
        });
        for engine in left_out.flatten() {
            output::arguments(format_args!(
                "GPU {address}: {} leaves out its {engine}\n",
                self.names()
            ));
        }
        if let Some(file) = kept {
            output::arguments(format_args!(
                "GPU {address}: {} loads its security controller (GSC) firmware {file} only so \
                 its media GT can enter its C6 power state; content protection stays off, as \
                 Linux here has no MEI driver\n",
                self.names()
            ));
        }
        words.join(" ")
    }

    /// The words of [`Drivers::parameters`], without its lines; `table` is
    /// whether the OpRegion holds a Video BIOS Table.
    fn words(&self, device: u16, table: bool) -> Vec<alloc::string::String> {
        let controllers = gpu::xe::firmware(device);
        let kept = stela_loader::host_firmware::recipes::intel_gsc(device);
        let mut words = Vec::new();
        let drivers = [
            self.i915.is_some().then_some("i915"),
            self.xe.then_some("xe"),
        ];
        for driver in drivers.into_iter().flatten() {
            if table {
                words.push(gpu::command_line::video_bios_table(driver, VBT));
            }
            if controllers.is_some() {
                words.push(gpu::command_line::without_huc(driver));
                if kept.is_none() {
                    words.push(gpu::command_line::without_gsc(driver));
                }
            }
        }
        words
    }
}

/// The functions of the host bridge's device, with the configuration the
/// scan read, which i915 reads through `pci_get_domain_bus_and_slot(0, 0,
/// 0)` (`i915_gmch_bridge_setup` fails the probe without it).
fn host_bridge(observed: &Observed<'_>) -> Vec<Searched> {
    let address = observed.adapter.address;
    let functions: Vec<_> = observed
        .chipset
        .iter()
        .filter(|function| function.configuration.is_some())
        .map(|function| searched(function, function.configuration))
        .collect();
    if address.segment != 0 || functions.is_empty() {
        output::arguments(format_args!(
            "GPU {address}: no host bridge was read at 0000:00:00.0, so i915's probe fails at \
             its bridge setup\n"
        ));
    }
    functions
}

/// The graphics memory the firmware reserved for the GPU, by the method
/// Linux's early quirks name for the device, with one line saying what was
/// found for `driver` or why none is reported.
fn graphics_stolen(observed: &mut Observed<'_>, driver: &str) -> Option<(u64, u64)> {
    use gpu::intel_stolen::{self as stolen, Absent};
    let address = observed.adapter.address;
    let config = observed.function.config();
    let mut read = |offset: u16| config.read32(offset).ok();
    let found = stolen::method(observed.adapter.device)
        .ok_or(Absent::NoMethod)
        .and_then(|method| stolen::find(method, &mut read));
    match found {
        Ok(found) => {
            output::arguments(format_args!(
                "GPU {address}: graphics stolen memory of {} KiB at {:#x}, given to {driver}\n",
                found.size / 1024,
                found.base
            ));
            Some((found.base, found.size))
        }
        Err(why) => {
            output::arguments(format_args!(
                "GPU {address}: no graphics stolen memory is reported ({why:?}): {driver} runs \
                 without it\n"
            ));
            None
        }
    }
}

/// The PCI-to-ISA bridges of the GPU's segment, with one line each. The
/// display code names the PCH by the bridge's IDs (`intel_pch.c` searches
/// the class) and takes its panel power sequencer and port map from that;
/// with none it detects no PCH.
fn chipset_bridges(observed: &Observed<'_>, driver: &str) -> Vec<Searched> {
    let address = observed.adapter.address;
    let bridges: Vec<_> = observed
        .chipset
        .iter()
        .filter(|bridge| {
            bridge.identity.address.segment == address.segment
                && bridge.identity.class[..2] == [0x06, 0x01]
        })
        .collect();
    if bridges.is_empty() {
        output::arguments(format_args!(
            "GPU {address}: no PCI-to-ISA bridge was found, so the display code detects no PCH\n"
        ));
    }
    bridges
        .into_iter()
        .map(|bridge| {
            output::arguments(format_args!(
                "GPU {address}: PCI-to-ISA bridge {} {:04x}:{:04x} is shown to {driver} for its \
                 chipset search\n",
                bridge.identity.address, bridge.identity.vendor, bridge.identity.device
            ));
            searched(bridge, None)
        })
        .collect()
}

/// How Linux is shown a chipset function the scan read.
fn searched(
    function: &stela_drivers::probe::ChipsetFunction,
    configuration: Option<[u8; 256]>,
) -> Searched {
    let (at, class) = (function.identity.address, function.identity.class);
    Searched {
        identity: stela_linux::function::Identity {
            bus: u32::from(at.bus),
            devfn: u32::from(at.device << 3 | at.function),
            vendor: function.identity.vendor,
            device: function.identity.device,
            subsystem_vendor: function.subsystem_vendor,
            subsystem_device: function.subsystem_device,
            class_revision: u32::from_be_bytes([class[0], class[1], class[2], function.revision]),
        },
        configuration,
    }
}

/// The name the display code asks for the PC's Video BIOS Table by.
const VBT: &str = "intel/vbt.bin";

/// The Video BIOS Table of the OpRegion the function's `ASLS` register
/// points at, as an image of `target`, and one line saying where it came
/// from or why there is none. Linux reads it through its ACPI OpRegion
/// driver, which Stela's Linux profile does not build; the OpRegion is plain
/// firmware memory, read here through its owner.
fn video_bios_table(
    observed: &mut Observed<'_>,
    target: firmware::Target<'_>,
) -> Option<Image<'static>> {
    let address = observed.adapter.address;
    let absent = |why: &dyn core::fmt::Display| {
        output::arguments(format_args!(
            "GPU {address}: no Video BIOS Table from the OpRegion ({why}): the display code \
             looks in the GPU's own flash (a discrete GPU) and option ROM, and finds panels by \
             probing only where neither holds one\n"
        ));
    };
    let asls = match observed.function.config().read32(opregion::ASLS) {
        Ok(asls) => asls,
        Err(error) => {
            absent(&format_args!("its address could not be read: {error:?}"));
            return None;
        }
    };
    let table = match opregion::read(u64::from(asls), |physical, into| {
        observed.services.read_firmware_memory(physical, into)
    }) {
        Ok(table) => table,
        Err(why) => {
            absent(&why);
            return None;
        }
    };
    let (bytes, place, (major, minor)) = (table.bytes.len(), table.place, table.version);
    // What the display code will take from it for a built-in panel: one
    // line, so a dark or dim laptop panel's log names what the table said.
    let panel = opregion::panel(&table.bytes);
    let revision = format!("{major}.{minor}");
    let catalog = Catalog::from_handoff(alloc::vec![Resource {
        name: VBT,
        target,
        provenance: Provenance {
            source: "ACPI OpRegion",
            revision: &revision,
            member: "vbt",
            license: "the PC's firmware",
        },
        expected_length: bytes,
        expected_sha256: Sha256::digest(&table.bytes).into(),
        bytes: table.bytes,
    }]);
    // The table is held for the boot, like the stored catalog's images.
    let image = catalog.ok().and_then(|catalog| {
        let held: &'static Catalog = alloc::boxed::Box::leak(alloc::boxed::Box::new(catalog));
        held.load(Requirement {
            name: VBT,
            target,
            minimum_bytes: 1,
        })
        .ok()
    });
    match image {
        Some(_) => {
            output::arguments(format_args!(
                "GPU {address}: Video BIOS Table of {bytes} bytes from the OpRegion's {place} \
                 (OpRegion {major}.{minor})\n"
            ));
            match panel {
                Some(panel) => output::arguments(format_args!(
                    "GPU {address}: Video BIOS Table for a built-in panel: {panel}\n"
                )),
                None => output::arguments(format_args!(
                    "GPU {address}: Video BIOS Table holds no data block the display code \
                     can read\n"
                )),
            }
        }
        None => absent(&"it cannot be held as a firmware image"),
    }
    image
}

/// The files the driver that binds the Intel GPU with PCI device ID
/// `device` and the display code ask for: the platform's files, which serve
/// i915 and xe alike, on a device of xe's table, and i915's display program
/// on the older platforms only i915 drives.
fn files(device: u16) -> Result<Inventory, ServiceError> {
    use stela_loader::host_firmware::recipes;
    let mut inventory = Inventory::default();
    recipes::require_xe(&mut inventory, device)
        .or_else(|_| recipes::require_i915(&mut inventory, device))
        .map_err(|_| ServiceError::Refused("the GPU has no pinned Intel firmware recipe"))?;
    Ok(inventory)
}
