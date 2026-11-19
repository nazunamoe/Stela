//! What each stack's kernel driver needs of the boot before Linux sees its
//! function: what identifies the device to its firmware files, the files
//! themselves, and what must have settled after the function's reset. The
//! claim's own steps (identity, the firmware's screen, BARs, decoding, the
//! reset and the private domain, MSI) are the same for every stack.
use super::{PAGE, output};
use alloc::{format, string::String, sync::Arc, vec::Vec};
use stela_drivers::{
    bus::{
        mmio::owned::{self, CacheAttribute, Mapping, Protection},
        pci::{Function, owned::OwnedConfig},
    },
    firmware::{self, Catalog, ErrorKind as FirmwareError, Image, Provider, Requirement},
    gpu::{self, stack::GpuStack},
    probe::{NativeServices, ServiceError},
};
use stela_linux::catalogue::{FirmwareTarget, PciFirmware};
use stela_loader::host_firmware::requirements::Inventory;

#[cfg(feature = "intel")]
mod intel;

/// What a stack's part found on the function before its reset.
pub(super) struct Identified {
    /// The files the driver may ask for, each under the name it asks by.
    pub(super) firmware: PciFirmware,
    /// Register 0 where it identifies the chip, so it reads the same after
    /// the reset.
    pub(super) identity: Option<u32>,
    /// The chip or platform as people know it.
    pub(super) chip: Option<&'static str>,
    /// Words of Linux's command line (`module.parameter=value`) the driver
    /// needs set for this device before it registers.
    pub(super) parameters: Option<String>,
    /// Other functions the driver's probe searches for and reads the
    /// identity of. Linux is shown them for searches only.
    pub(super) searched: Vec<Searched>,
    /// The graphics memory the firmware reserved for the GPU, as Linux's
    /// x86 early quirks find it, by base and size.
    pub(super) stolen: Option<(u64, u64)>,
}

/// A function shown to Linux for a probe's searches, with the
/// configuration the scan read for it, if the probe reads any.
#[derive(Clone)]
pub(super) struct Searched {
    pub(super) identity: stela_linux::function::Identity,
    pub(super) configuration: Option<[u8; 256]>,
}

/// The function as a stack's part may read it before its reset.
pub(super) struct Observed<'a> {
    /// The platform's services; the AMD and Intel parts read through them.
    #[cfg_attr(not(any(feature = "amdgpu", feature = "intel")), allow(dead_code))]
    pub(super) services: &'a dyn NativeServices,
    /// The adapter; the AMD and Intel parts name it.
    #[cfg_attr(not(any(feature = "amdgpu", feature = "intel")), allow(dead_code))]
    pub(super) adapter: &'a gpu::Adapter,
    /// The PCI-to-ISA bridges and host bridge functions the probe read,
    /// which the Intel parts show their drivers.
    #[cfg_attr(not(feature = "intel"), allow(dead_code))]
    pub(super) chipset: &'a [stela_drivers::probe::ChipsetFunction],
    /// Its configuration, for reads only; the Intel parts read it.
    #[cfg_attr(not(feature = "intel"), allow(dead_code))]
    pub(super) function: &'a mut Function<OwnedConfig>,
    /// The first page of the stack's register BAR, mapped as the stack's
    /// part asks ([`Before::register_page`]).
    pub(super) registers: &'a Mapping,
    /// The stack's register BAR, for the reads a stack's part maps itself:
    /// amdgpu's IP discovery table.
    #[cfg_attr(not(feature = "amdgpu"), allow(dead_code))]
    pub(super) register_bar: &'a owned::Resource,
}

/// One stack's part of the claim.
pub(super) struct Before {
    /// What the boot can check from the function's PCI identity alone, before
    /// the claim changes anything on the function or takes the firmware's
    /// screen: that the driver's files for the device are pinned and on the
    /// USB. A GPU refused here keeps the firmware's picture.
    pub(super) admits: fn(&gpu::Adapter, &Catalog) -> Result<(), ServiceError>,
    /// Reads what selects the driver's firmware, through the first page of
    /// the stack's register BAR mapped read-only before the reset, and
    /// selects it.
    pub(super) identify: fn(Observed<'_>, Arc<Catalog>) -> Result<Identified, ServiceError>,
    /// How the first page of the register BAR is mapped for `identify`:
    /// read-write for a part that writes there (amdgpu's index registers),
    /// as a page already mapped is not mapped a second time.
    pub(super) register_page: Protection,
    /// Waits, after the reset, for what the driver's probe expects to find
    /// done. The register BAR is the resource.
    pub(super) settle:
        fn(&dyn NativeServices, &owned::Resource, &Identified) -> Result<(), ServiceError>,
    /// The words of Linux's command line the driver needs for the device,
    /// as far as its PCI identity tells them, for a GPU that may be claimed
    /// after Linux started, in place of one that could not draw: Linux reads
    /// its command line once, at its start.
    pub(super) ahead: fn(&gpu::Adapter) -> Vec<String>,
}

/// The part for `stack`'s kernel driver, if the boot has one.
pub(super) fn of(stack: &GpuStack) -> Option<&'static Before> {
    match stack.kernel.driver {
        "nouveau" => Some(&NOUVEAU),
        #[cfg(feature = "amdgpu")]
        "amdgpu" => Some(&AMDGPU),
        // One part for both: either driver may bind the device.
        #[cfg(feature = "intel")]
        "xe" | "i915" => Some(&intel::INTEL),
        _ => None,
    }
}

/// nouveau: the chip is `PMC_BOOT_0`'s, its GSP-RM files are that chip's, and
/// a Turing-to-Ada GPU's own firmware must have booted again after the reset.
static NOUVEAU: Before = Before {
    ahead: |_| Vec::new(),
    // Its files are the chip's, which only its registers name.
    admits: |_, _| Ok(()),
    identify: |observed, catalog| {
        let registers = observed.registers;
        let endian = registers.read32(4).map_err(ServiceError::Mapping)?;
        if endian == u32::MAX || endian == 0x01000001 {
            return Err(ServiceError::Refused(
                "the GPU register byte order is unavailable",
            ));
        }
        let boot0 = registers.read32(0).map_err(ServiceError::Mapping)?;
        let chip = gpu::nouveau_chip::Chip::from_boot0(boot0).ok_or(ServiceError::Refused(
            "the GPU's PMC chipset is unsupported",
        ))?;
        let name = chip.name.to_ascii_lowercase();
        let mut inventory = Inventory::default();
        stela_loader::host_firmware::recipes::require_gsp(&mut inventory, &name)
            .map_err(|_| ServiceError::Refused("the GPU has no pinned GSP recipe"))?;
        Ok(Identified {
            firmware: select_firmware(&inventory, &name, catalog, None)?,
            identity: Some(boot0),
            chip: Some(chip.name),
            parameters: None,
            searched: Vec::new(),
            stolen: None,
        })
    },
    register_page: Protection::ReadOnly,
    settle: |services, registers, identified| match identified
        .identity
        .and_then(gpu::nouveau_chip::Chip::from_boot0)
        .filter(|chip| chip.waits_for_firmware_boot())
    {
        Some(chip) => wait_for_firmware_boot(services, registers, chip),
        None => Ok(()),
    },
};

/// amdgpu: the files are those it may ask for on the PCI device ID's GPU,
/// as pinned: its family's for a device its table names; for one it
/// identifies by the device's own IP discovery table, the files of the
/// block versions that table names, read here before the reset as amdgpu
/// reads it ([`ip_discovery`]), or every file named after an IP version
/// where it cannot be read. The driver loads every engine's firmware
/// itself, so nothing boots by itself after the reset and nothing waits
/// here.
#[cfg(feature = "amdgpu")]
static AMDGPU: Before = Before {
    ahead: |_| Vec::new(),
    admits: |adapter, catalog| {
        use stela_loader::host_firmware::recipes::AmdGpu;
        let files = amdgpu_files(adapter.device)?;
        // A discovered GPU's files are known only from its table, which
        // `identify` reads; it checks them there.
        if AmdGpu::of(adapter.device) == AmdGpu::Discovered {
            return Ok(());
        }
        on_usb(&files, &format!("{:04x}", adapter.device), catalog)
    },
    identify: |observed, catalog| {
        use stela_loader::host_firmware::recipes;
        let adapter = observed.adapter;
        // Register 0 identifies nothing here; the read proves the window.
        observed
            .registers
            .read32(0)
            .map_err(ServiceError::Mapping)?;
        let name = format!("{:04x}", adapter.device);
        let inventory = if recipes::AmdGpu::of(adapter.device) == recipes::AmdGpu::Discovered {
            discovered_files(&observed)?
        } else {
            amd_claim_notes(&observed, None);
            amdgpu_files(adapter.device)?
        };
        Ok(Identified {
            firmware: select_firmware(&inventory, &name, catalog, None)?,
            identity: None,
            chip: recipes::amd_gpu_family(adapter.device),
            parameters: None,
            searched: Vec::new(),
            stolen: None,
        })
    },
    // The IP discovery read writes the index registers in it.
    register_page: Protection::ReadWrite,
    settle: |_, _, _| Ok(()),
};

/// The files amdgpu may ask for on the AMD GPU with PCI device ID `device`.
#[cfg(feature = "amdgpu")]
fn amdgpu_files(device: u16) -> Result<Inventory, ServiceError> {
    let mut inventory = Inventory::default();
    stela_loader::host_firmware::recipes::require_amdgpu(&mut inventory, device).map_err(|_| {
        ServiceError::Refused("amdgpu leaves this GPU to an older driver and loads no files for it")
    })?;
    Ok(inventory)
}

/// The files amdgpu asks for on the AMD GPU it identifies by its IP
/// discovery table: those of the block versions the table names, else
/// every file named after an IP version, with a line saying which.
#[cfg(feature = "amdgpu")]
fn discovered_files(observed: &Observed<'_>) -> Result<Inventory, ServiceError> {
    use stela_boot_core::amd_discovery::{self as discovery, Ip};
    use stela_loader::host_firmware::recipes::{self, DiscoveredIp};
    let address = observed.adapter.address;
    let device = observed.adapter.device;
    let ips = match ip_discovery(observed.services, observed.registers, observed.register_bar) {
        Ok(ips) => ips,
        Err(why) => {
            output::arguments(format_args!(
                "GPU {address}: its IP discovery table was not read before its reset ({why}); \
                 its driver is given every firmware file named after an IP version\n"
            ));
            amd_claim_notes(observed, None);
            return amdgpu_files(device);
        }
    };
    amd_claim_notes(observed, Some(&ips));
    // A compute accelerator is refused here, before its reset, as the
    // port's rule leaves out every GPU RADV opens no device on.
    #[cfg(feature = "amdgpu")]
    if let Some(graphics) = discovery::version(&ips, discovery::GC) {
        gpu::amdgpu::discovered_serves(graphics).map_err(ServiceError::Refused)?;
    }
    let named: Vec<String> = [
        ("GC", discovery::GC),
        ("MP0", discovery::MP0),
        ("MP1", discovery::MP1),
        ("SDMA", discovery::SDMA0),
        ("VCN", discovery::VCN),
        ("VPE", discovery::VPE),
        ("DCN", discovery::DMU),
    ]
    .iter()
    .filter_map(|&(block, hw_id)| {
        discovery::version(&ips, hw_id)
            .map(|(major, minor, revision)| format!("{block} {major}.{minor}.{revision}"))
    })
    .collect();
    output::arguments(format_args!(
        "GPU {address}: its IP discovery table names {}; its driver is given those blocks' \
         firmware but the media engines' (VCN, VPE), which it leaves out\n",
        named.join(", ")
    ));
    let ips: Vec<DiscoveredIp> = ips
        .iter()
        .map(
            |&Ip {
                 hw_id,
                 major,
                 minor,
                 revision,
                 ..
             }| DiscoveredIp {
                hw_id,
                major,
                minor,
                revision,
            },
        )
        .collect();
    // A block amdgpu requires and cannot start would fail its probe after
    // the reset; refused here, the GPU keeps what the firmware left.
    if let Some((block, (major, minor, revision))) = recipes::discovered_lacks(&ips) {
        output::arguments(format_args!(
            "GPU {address}: its IP discovery table names {block} {major}.{minor}.{revision}, \
             and no pinned amdgpu firmware file is that block's (linux-firmware does not \
             publish it, or the block is newer than the pinned driver); amdgpu could not start \
             it, so it stays as the firmware left it\n"
        ));
        return Err(ServiceError::Refused(
            "no pinned amdgpu firmware for a block the GPU's IP discovery table names",
        ));
    }
    let mut inventory = Inventory::default();
    recipes::require_amdgpu_discovered(&mut inventory, device, &ips)
        .map_err(|_| ServiceError::Refused("the GPU's discovered firmware cannot be selected"))?;
    Ok(inventory)
}

/// What an AMD GPU's claim reads for its triage, before its reset: where
/// its IP discovery table lists its security processor (MP0), that
/// processor's state as the previous owner left it, which amdgpu's own start
/// then meets; and for an integrated GPU, the memory it is given: its share
/// of RAM, the GTT and TTM limits that follow from it, its VRAM carve-out
/// and the most Metal may tell the guest. One line each.
#[cfg(feature = "amdgpu")]
fn amd_claim_notes(observed: &Observed<'_>, ips: Option<&[stela_boot_core::amd_discovery::Ip]>) {
    use stela_boot_core::amd_discovery::{self as discovery};
    let adapter = observed.adapter;
    let address = adapter.address;
    let bar = observed.register_bar.physical_span().bytes() as usize;
    let read = |offset: usize| {
        if offset + 4 <= bar {
            read_register(observed.services, observed.register_bar, offset).unwrap_or(u32::MAX)
        } else {
            u32::MAX
        }
    };
    if let Some((major, minor, revision)) =
        ips.and_then(|ips| discovery::version(ips, discovery::MP0))
        && bar >= discovery::SECURITY_PROCESSOR_BYTES
    {
        output::arguments(format_args!(
            "GPU {address}: its security processor (MP0 {major}.{minor}.{revision}) before its \
             reset: {}\n",
            discovery::security_processor(
                read(discovery::MP0_SMN_C2PMSG_33),
                read(discovery::MP0_SMN_C2PMSG_64),
                read(discovery::MP0_SMN_C2PMSG_81),
            )
        ));
    }
    // Its kind as the loader took it: the driver's table, else the
    // graphics version its IP discovery table lists, else integrated.
    let integrated = GpuStack::select(adapter)
        .ok()
        .and_then(|stack| {
            let discovered = stack.kernel.discovered_integrated;
            stack
                .kernel
                .table_integrated(adapter)
                .or_else(|| discovered.and_then(|discovered| (discovered.named)(adapter)))
                .or_else(|| {
                    let graphics = discovery::version(ips?, discovery::GC)?;
                    Some((discovered?.graphics)(graphics))
                })
        })
        .unwrap_or(true);
    if !integrated {
        return;
    }
    let mib = |bytes: u64| bytes >> 20;
    let carve_out = match read(discovery::RCC_CONFIG_MEMSIZE) {
        u32::MAX => String::from("was not read (RCC_CONFIG_MEMSIZE reads all ones)"),
        megabytes => {
            format!("is {megabytes}M (RCC_CONFIG_MEMSIZE), which the share does not count")
        }
    };
    let share = crate::native_memory::gpu_share();
    if share == 0 {
        let heap = crate::ALLOCATOR.usage();
        let bound = match crate::native_memory::gpu_page_limit() {
            Some(pages) => format!("at most {}M of them", mib(pages as u64 * 4096)),
            None => String::from("with no bound of their own"),
        };
        output::arguments(format_args!(
            "GPU {address}: integrated, and the memory plan gave it no share of RAM: Linux's \
             pages for it come from Stela's heap of {}M, {bound}; its VRAM carve-out \
             {carve_out}\n",
            mib(heap.total as u64),
        ));
        return;
    }
    #[cfg(feature = "nvk")]
    let told = format!(
        "; Metal tells the guest at most {}M, 0.8M less for each 1M Linux holds as it opens",
        mib(stela_drivers::gpu::metal::device::share::working_set(
            stela_drivers::gpu::metal::device::share::ShareState {
                held: 0,
                limit: share,
            }
        ))
    );
    #[cfg(not(feature = "nvk"))]
    let told = String::new();
    output::arguments(format_args!(
        "GPU {address}: integrated: its share of RAM is {}M, the most Linux may hold (the page \
         catalog's limit); amdgpu caps its GTT at it and TTM's page limit is {}M; its VRAM \
         carve-out {carve_out}{told}\n",
        mib(share),
        mib(share.saturating_mul(2))
    ));
}

/// The IP blocks of the AMD GPU whose register BAR is `registers`, from
/// its IP discovery table, read before the reset as amdgpu reads it at its
/// probe ([`stela_boot_core::amd_discovery::read`]): through the `MM_INDEX`
/// and `MM_DATA` registers of `page`, the BAR's first page mapped
/// read-write, whose index is put back after.
#[cfg(feature = "amdgpu")]
fn ip_discovery(
    services: &dyn NativeServices,
    page: &Mapping,
    registers: &owned::Resource,
) -> Result<Vec<stela_boot_core::amd_discovery::Ip>, String> {
    use stela_boot_core::amd_discovery::{self as discovery, INDEX_PAGE_BYTES};
    if (registers.physical_span().bytes() as usize) < discovery::REGISTER_BYTES {
        return Err(String::from(
            "its register BAR is smaller than amdgpu's discovery reads",
        ));
    }
    struct Bar<'a> {
        services: &'a dyn NativeServices,
        page: &'a Mapping,
        registers: &'a owned::Resource,
    }
    impl discovery::Registers for Bar<'_> {
        fn read(&self, offset: usize) -> u32 {
            if offset < INDEX_PAGE_BYTES {
                return self.page.read32(offset).unwrap_or(u32::MAX);
            }
            read_register(self.services, self.registers, offset).unwrap_or(u32::MAX)
        }
        fn write(&self, offset: usize, value: u32) -> bool {
            offset < INDEX_PAGE_BYTES && self.page.write32(offset, value).is_ok()
        }
    }
    discovery::read(&Bar {
        services,
        page,
        registers,
    })
    .map_err(|failure| format!("{failure}"))
}

/// The image `selection` names in `stored`: its exact binding, else the
/// bundle's file by its pinned digest.
fn find<'a>(
    stored: &'a Catalog,
    selection: &stela_loader::host_firmware::recipes::Selection,
) -> Result<Image<'a>, firmware::Error> {
    use stela_loader::host_firmware::bundle;
    let request = &selection.request;
    match stored.load(request.view()) {
        Err(error) if error.kind == FirmwareError::Missing => {
            let name = bundle::name(selection.recipe.image.sha256);
            match stored.load(Requirement {
                name: &name,
                target: bundle::TARGET,
                minimum_bytes: request.minimum_bytes,
            }) {
                Err(stored_error) if stored_error.kind == FirmwareError::Missing => Err(error),
                other => other,
            }
        }
        other => other,
    }
}

/// Whether every file of `inventory` the driver needs is in `catalog`,
/// before anything of the GPU `name` changes: one line naming the files
/// missing and why ([`lacks`]), and the refusal [`select_firmware`] would
/// give for them later.
/// A file the driver starts without (`recipes::optional`) may be missing.
#[cfg_attr(not(any(feature = "amdgpu", feature = "intel")), allow(dead_code))]
fn on_usb(inventory: &Inventory, name: &str, catalog: &Catalog) -> Result<(), ServiceError> {
    use stela_loader::host_firmware::recipes;
    let plan = recipes::Plan::select(inventory)
        .map_err(|_| ServiceError::Refused("the GPU firmware recipe cannot be selected"))?;
    let missing: Vec<_> = plan
        .selections()
        .iter()
        .filter(|selection| {
            !recipes::optional(&selection.request)
                && find(catalog, selection).is_err_and(|error| error.kind == FirmwareError::Missing)
        })
        .map(|selection| {
            (
                selection.request.name.clone(),
                selection.recipe.image.sha256,
            )
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(lacks(name, &missing, plan.selections().len()))
}

/// Says which of the `files` files of the GPU `name` are `missing`, each
/// named with its digest, and why, by what the loader recorded of the USB's
/// bundle ([`crate::host_firmware::bundle_held`]); the refusal that says it
/// on the screen.
fn lacks(name: &str, missing: &[(String, &str)], files: usize) -> ServiceError {
    use stela_loader::host_firmware::bundle::{Absent, Held};
    let held: Vec<Held> = missing
        .iter()
        .map(|(_, sha256)| crate::host_firmware::bundle_held(sha256))
        .collect();
    let names = missing
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let count = missing.len();
    match Absent::of(held.iter().copied()) {
        Absent::NotOnUsb => {
            output::arguments(format_args!(
                "GPU {name}: the USB's firmware bundle lacks {count} of this chip's {files} files \
                 ({names}); `stela repair` completes it\n"
            ));
            ServiceError::Refused(NO_FIRMWARE)
        }
        Absent::ReadFailed => {
            output::arguments(format_args!(
                "GPU {name}: the USB's firmware bundle holds {count} of this chip's {files} files \
                 ({names}) that were not in memory: reading them before the firmware's exit \
                 failed, as the HOST FIRMWARE BUNDLE LACKS lines say\n"
            ));
            ServiceError::Refused(FIRMWARE_READ_FAILED)
        }
        Absent::AnsweredLate => {
            let why = if held.contains(&Held::NoRoom) {
                "the files read for such a GPU did not fit the firmware handoff"
            } else {
                "the firmware showed a GPU of this family Stela drives or no PCI port with \
                 nothing below it, so none was expected"
            };
            output::arguments(format_args!(
                "GPU {name}: the USB's firmware bundle holds {count} of this chip's {files} files \
                 ({names}), but they were not read: the firmware did not show this GPU before \
                 its exit, so it answered only after that, and {why}\n"
            ));
            ServiceError::Refused(FIRMWARE_NOT_READ)
        }
    }
}

/// The firmware of every request in `inventory`, each file bound under the
/// exact name and target its driver asks for. A driver needs every file of
/// its recipe but those it starts without (`recipes::optional`): one of the
/// others missing fails its start later with a generic error, so the GPU is
/// refused; an optional one missing is said once and its request answers
/// that the file does not exist, as on Linux.
/// `read` is an image the boot read from the PC itself and the name the
/// driver asks for it by, bound beside the files under the same target.
fn select_firmware(
    inventory: &Inventory,
    name: &str,
    catalog: Arc<Catalog>,
    read: Option<(Image<'static>, &str)>,
) -> Result<PciFirmware, ServiceError> {
    use stela_loader::host_firmware::recipes;
    let mut names = Vec::new();
    let request = inventory.requests().first().ok_or(ServiceError::Refused(
        "the GPU has no pinned firmware recipe",
    ))?;
    let target = FirmwareTarget::new(
        request.target.driver.clone(),
        request.target.hardware.clone(),
        request.target.revision.clone(),
        request.target.board.clone(),
    )
    .map_err(|_| ServiceError::Refused("the GPU firmware target is invalid"))?;
    let plan = recipes::Plan::select(inventory)
        .map_err(|_| ServiceError::Refused("the GPU firmware recipe cannot be selected"))?;
    // SAFETY: the reference count is never given back, so the catalog
    // stays allocated for every image borrowed from it for the boot.
    let stored: &'static Catalog = unsafe { &*Arc::into_raw(catalog.clone()) };
    let mut bindings = Vec::new();
    let mut missing = Vec::new();
    // A driver that chooses among many files by what the device reports
    // (amdgpu by IP discovery) gets one line for them, not one each.
    let summarized = plan.selections().len() > SUMMARIZED;
    for selection in plan.selections() {
        let request = &selection.request;
        let found = find(stored, selection);
        if let Ok(image) = found {
            if !summarized {
                output::arguments(format_args!(
                    "GPU {name}: firmware {} of {} bytes\n",
                    request.name,
                    image.bytes().len()
                ));
            }
            bindings.push((image, request));
        }
        match found {
            Ok(image) => {
                selection
                    .recipe
                    .image
                    .verify_digest(image.bytes().len(), image.sha256())
                    .map_err(|_| {
                        ServiceError::Refused(
                            "the GPU firmware digest does not match its pinned recipe",
                        )
                    })?;
            }
            Err(error) if error.kind == FirmwareError::Missing && recipes::optional(request) => {
                output::arguments(format_args!(
                    "GPU {name}: firmware {} is not on the USB; its driver starts without it\n",
                    request.name
                ));
            }
            Err(error) if error.kind == FirmwareError::Missing => {
                missing.push((request.name.clone(), selection.recipe.image.sha256));
            }
            Err(error) => {
                output::arguments(format_args!(
                    "GPU {name}: firmware {} refused: {error:?}\n",
                    request.name
                ));
                return Err(ServiceError::Refused(
                    "the GPU firmware does not match its exact catalogue target",
                ));
            }
        }
        names.push(request.name.clone());
    }
    if !missing.is_empty() {
        return Err(lacks(name, &missing, names.len()));
    }
    if summarized {
        // Each file by its name less its directory and `.bin`, so a file
        // the selection left out shows without a line for each.
        let given: Vec<&str> = bindings
            .iter()
            .map(|(_, request)| {
                let file = request.name.as_str();
                let file = file.rsplit('/').next().unwrap_or(file);
                file.strip_suffix(".bin").unwrap_or(file)
            })
            .collect();
        output::arguments(format_args!(
            "GPU {name}: {} firmware files of {} bytes, of which its driver loads those the \
             device names: {}\n",
            bindings.len(),
            bindings
                .iter()
                .map(|(image, _)| image.bytes().len())
                .sum::<usize>(),
            given.join(" ")
        ));
    }
    names.extend(read.iter().map(|(_, name)| String::from(*name)));
    // Each file under the exact name and target the driver asks for.
    let catalog = Catalog::rebind_all(
        bindings
            .iter()
            .map(|(image, request)| {
                let view = request.view();
                (*image, view.name, view.target)
            })
            .chain(
                read.iter()
                    .map(|(image, name)| (*image, *name, request.view().target)),
            ),
    )
    .map_err(|_| ServiceError::Refused("the GPU firmware cannot be bound to its requests"))?;
    PciFirmware::new(target, Arc::new(catalog), names.into_boxed_slice())
        .map_err(|_| ServiceError::Refused("the GPU firmware image names are invalid"))
}

fn read_register(
    services: &dyn NativeServices,
    registers: &owned::Resource,
    offset: usize,
) -> Option<u32> {
    let page = offset & !(PAGE as usize - 1);
    let mut window = services
        .authority()
        .ok()?
        .map(
            registers.clone(),
            page,
            PAGE as usize,
            CacheAttribute::Uncached,
            Protection::ReadOnly,
        )
        .ok()?;
    let value = window.read32(offset - page).ok();
    // SAFETY: the read above was this scoped mapping's only use.
    unsafe { window.retire() }.ok()?;
    value
}

/// After the claim's function-level reset a Turing-to-Ada GPU's own firmware
/// runs devinit again. nouveau reads the VBIOS before it waits for that
/// (`firmware_boot`), so wait here first, by nouveau's bound, and print what
/// the start of the old boot flow depends on: the firmware boot, WPR2 (which
/// FWSEC-FRTS sets) and the VBIOS window the PRAMIN shadow reads.
/// `GPU AD107: firmware boot completed after N polls: gfw=… plm=… wpr2=…-…
/// display_fuse=… vbios_window=…`, or `did not complete` and a refusal.
fn wait_for_firmware_boot(
    services: &dyn NativeServices,
    registers: &owned::Resource,
    chip: &gpu::nouveau_chip::Chip,
) -> Result<(), ServiceError> {
    use gpu::nouveau_chip::firmware_boot::{self, Error as BootError};
    let page = firmware_boot::PRIV_LEVEL_MASK & !(PAGE as usize - 1);
    let mut window = services
        .authority()?
        .map(
            registers.clone(),
            page,
            PAGE as usize,
            CacheAttribute::Uncached,
            Protection::ReadOnly,
        )
        .map_err(ServiceError::Mapping)?;
    let timer = services.minimum_delay()?;
    let result = firmware_boot::wait(
        |offset| {
            window
                .read32(offset - page)
                .map_err(|_| "a register read failed")
        },
        |micros| timer.wait_us(micros).map_err(|_| "the timer failed"),
    );
    // SAFETY: the polls above were this scoped mapping's only use.
    unsafe { window.retire() }.map_err(ServiceError::Mapping)?;
    let read = |offset| read_register(services, registers, offset);
    let hex =
        |value: Option<u32>| value.map_or_else(|| String::from("unread"), |v| format!("{v:#010x}"));
    // nouveau's PRAMIN shadow reads the window only with the display fused
    // on (`shadowramin.c` `pramin_init`: 0x820c04 from GA100, else 0x021c04).
    let fuse = read(match chip.architecture {
        gpu::nouveau_chip::Architecture::Turing => 0x02_1c04,
        _ => 0x82_0c04,
    });
    let window = fuse
        .filter(|fuse| fuse & 1 == 0)
        .and_then(|_| read(0x62_5f04));
    let state = format!(
        "wpr2={}-{} display_fuse={} vbios_window={}",
        hex(read(0x1f_a824)),
        hex(read(0x1f_a828)),
        hex(fuse),
        hex(window),
    );
    match result {
        Ok(reading) => {
            output::arguments(format_args!(
                "GPU {}: firmware boot completed after {} polls: gfw={:#010x} plm={:#010x} {state}\n",
                chip.name, reading.polls, reading.progress, reading.priv_level_mask
            ));
            Ok(())
        }
        Err(BootError::Timeout(reading)) => {
            output::arguments(format_args!(
                "GPU {}: firmware boot did not complete after its reset in {} polls of {} us: \
                 gfw={:#010x} (expected low byte {:#04x}) plm={:#010x} (expected bit 0) {state}\n",
                chip.name,
                reading.polls,
                firmware_boot::POLL_MICROS,
                reading.progress,
                firmware_boot::COMPLETED,
                reading.priv_level_mask
            ));
            Err(ServiceError::Refused(
                "the GPU's firmware boot did not complete after its reset",
            ))
        }
        Err(BootError::Access(error)) => {
            output::arguments(format_args!(
                "GPU {}: firmware boot wait stopped: {error}\n",
                chip.name
            ));
            Err(ServiceError::Refused(
                "the GPU's firmware boot registers could not be read",
            ))
        }
    }
}

/// More firmware files than this for one GPU are logged as one line.
const SUMMARIZED: usize = 16;

/// Why a GPU was refused when the USB lacks any of its chip's firmware files.
pub(super) const NO_FIRMWARE: &str = "no pinned firmware for the observed GPU chipset was loaded";
/// Why a GPU was refused when the USB holds its chip's files and reading
/// some of them before the firmware's exit failed.
pub(super) const FIRMWARE_READ_FAILED: &str =
    "the USB holds the GPU's firmware, but reading it before the firmware's exit failed";
/// Why a GPU was refused when the USB holds its chip's files and they were
/// not read, the GPU having answered only after the firmware's exit.
pub(super) const FIRMWARE_NOT_READ: &str = "the USB holds the GPU's firmware, but it was not read: the GPU answered only after the \
     firmware's exit";
