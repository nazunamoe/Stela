//! The host's Linux environment and the NVIDIA GPUs nouveau drives in it,
//! which back the guest's Metal device.
//!
//! It lives on the boot CPU, and any processor may run it in that CPU's
//! place, one at a time (`KernelWork::dispatch_linux_moved`). A GPU's MSI
//! targets the boot CPU, which takes it between slices or when the PE waits,
//! and only records its line ([`Fired`]). An event wakes a PE whose guest
//! waits for an interrupt, which holds no guest lock: its idle core runs
//! what is ready ([`pump_waiting_pe`]). A PE waiting for a call it posted
//! runs Linux for it itself. The boot PE pumps when no other did ([`pump`]):
//! like the host's service, it waits for a moment it holds no guest lock,
//! waiting for an interrupt or at EL0, but only for a bounded time
//! ([`pump_due`]), so other PEs do not spin on a lock while Linux runs and
//! GPU completions are not starved. Nothing reserves a core, and a Linux
//! environment with nothing to do costs the boot PE one comparison a slice.
#[cfg(feature = "nvk")]
pub(super) mod acpi_backlight;
mod before;
#[cfg(feature = "nvk")]
pub(super) mod client;
#[cfg(feature = "nvk")]
pub(super) mod display;
#[cfg(feature = "nvk")]
mod file;
#[cfg(feature = "nvk")]
pub(super) mod gate;
pub(super) mod link;
/// `xtask guest`'s objects of the StelaGPU kext.
#[cfg(feature = "nvk")]
mod kext {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/guest/StelaGPU.kext.rs"
    ));
}
#[cfg(feature = "nvk")]
mod refused;
#[cfg(feature = "nvk")]
mod slice;
#[cfg(feature = "nvk")]
mod stack;
#[cfg(feature = "nvk")]
mod waiter;

use super::{
    host,
    kernel_work::{self, KernelPages, KernelWork, SLOTS},
};
use crate::{handoff::RtcReading, native_irq, native_memory, native_mmio::Window, output};
use alloc::{
    borrow::ToOwned, boxed::Box, collections::BTreeMap, collections::BTreeSet, format, rc::Rc,
    string::String, sync::Arc, vec::Vec,
};
use core::{
    alloc::Layout,
    cell::{Cell, RefCell},
    ffi::{CStr, c_void},
    mem::ManuallyDrop,
    ops::Range,
    pin::Pin,
    ptr::{self, addr_of},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};
pub(super) use stela_boot_core::linux_pump::Calm;
use stela_boot_core::linux_pump::{BootPump, Limits, Taker, Turn, Wakes, boot_decision};
use stela_boot_core::page_window::{self, Cache};
#[cfg(feature = "nvk")]
use stela_drivers::gpu::metal::open::PcieLink;
use stela_drivers::{
    bus::{
        irq::{Disposition, Handler, IrqError, IrqRegistration, Route, Source},
        mmio::owned::{self, CacheAttribute, Mapping},
        pci::{
            Bar, ConfigError, ConfigFailure, ConfigSpace, Function,
            owned::{ConfigRetention, OwnedConfig},
        },
    },
    firmware::Catalog,
    gpu::{self, stack::GpuStack},
    platform::work::{
        WorkError,
        service::{Clock, Domain, Idle},
    },
    probe::{NativeServices, ServiceError},
};
use stela_engine::machine::engine::Hub;
use stela_linux::{
    catalogue::{FunctionCustody, PciAuthority, PciFirmware},
    function::{self, Attached, Msi, Placement, Region, msix},
    kernel::KernelOwner,
    owner::{Errno, Platform, RealtimeOrigin, RealtimeRead},
    random::Seeded,
    service::Environment as LinuxEnvironment,
    task::runtime::{ManagedJob, Outcome, TaskRuntime},
    threads::{KernelThreadPlatform, TaskDataBacking},
};

type Environment = LinuxEnvironment<SLOTS>;
type LinuxKernel = KernelOwner<Host, SLOTS>;
type LinuxThreads = TaskRuntime<Host, KernelPages, SLOTS>;

const PAGE: u64 = page_window::PAGE;
const EIO: Errno = -5;
const EFAULT: Errno = -14;
const ERANGE: Errno = -34;
const ENODEV: Errno = -19;
const EINVAL: Errno = -22;
const ENOMEM: Errno = -12;
const EBUSY: Errno = -16;
const ENOSPC: Errno = -28;
const EOPNOTSUPP: Errno = -95;

const MSI_CAPABILITY: u8 = 5;
#[cfg(feature = "nvk")]
const PCIE_CAPABILITY: u8 = 0x10;
/// `PCI_EXP_LNKSTA`, in the PCIe capability.
#[cfg(feature = "nvk")]
const LINK_STATUS: u16 = 0x12;
#[cfg(feature = "nvk")]
const REVISION: u16 = 0x08;
#[cfg(feature = "nvk")]
const SUBSYSTEM_ID: u16 = 0x2c;
/// Linux's `IORESOURCE_*` bits of a memory BAR.
const RESOURCE_MEMORY: u64 = 0x200;
const RESOURCE_PREFETCHABLE: u64 = 0x2000;
const RESOURCE_WIDE: u64 = 0x10_0000;
/// How long the GPUs may take to boot their firmware before the guest starts.
const START_SECONDS: u64 = 60;
/// The longest idle wait while they do, which the timeout above is checked at.
const WAIT_MICROS: u64 = 10_000;
/// How often a long wait in the start says where it is ([`wait_for_child`]).
const WAIT_REPORT_SECONDS: u64 = 2;
/// The longest one pump runs Linux between the boot PE's slices. What is
/// still runnable stays pending for the next, so a busy driver shares the
/// boot CPU with the guest and the host's devices instead of holding it.
const PUMP_NS: u64 = 2_000_000;
/// The longest a pump on a PE whose guest waits runs Linux, ending sooner
/// once its guest has work: a guest interrupt waits at most this and the
/// step running then.
const WAITING_PUMP_NS: u64 = 1_000_000;
/// The longest a PE waiting for its own posted call runs Linux at a time
/// before it lets another processor in.
#[cfg(feature = "nvk")]
const POSTER_PUMP_NS: u64 = 1_000_000;
/// The longest the GPUs' stop waits for a processor in the boot CPU's place
/// to leave Linux.
#[cfg(feature = "nvk")]
const MOVES_CLOSE_NS: u64 = 200_000_000;

/// How long a pump on a waiting PE waits for the host drivers' lock.
const MOVED_LOCK_WAIT_MICROS: u64 = 50;

/// The processor that pumps now ([`pump_as`]).
static TURN: Turn = Turn::new();
/// One wake of a sleeping PE for each episode of held-back work.
static WAKES: Wakes = Wakes::new();

/// The firmware's ACPI VFCT table, copied while the loader root mapped it:
/// the video BIOS images firmware publishes for GPUs without an option ROM
/// of their own (integrated AMD GPUs).
static VIDEO_BIOS_TABLE: AtomicPtr<Vec<u8>> = AtomicPtr::new(ptr::null_mut());

/// Keeps the ACPI VFCT table for the claims, which give each function its
/// image in it as the platform ROM Linux's drivers read.
///
/// # Safety
/// `root` is the loader root that maps the ACPI tables at `rsdp`. Called
/// once, before any claim.
pub(super) unsafe fn keep_video_bios_table(rsdp: u64, root: u64) {
    if rsdp == 0 {
        return;
    }
    let table = {
        // SAFETY: the caller's; the loader root maps every table.
        let _root = unsafe { super::storage::RestoreRoot::enter(root) };
        // SAFETY: the firmware's RSDP, whose tables the loader root maps.
        unsafe { stela_runtime::pc::acpi::find(rsdp, &stela_boot_core::vfct::SIGNATURE) }
            .ok()
            .map(<[u8]>::to_vec)
    };
    if let Some(table) = table {
        output::arguments(format_args!(
            "GPU: the ACPI VFCT table holds {} bytes of video BIOS images\n",
            table.len()
        ));
        VIDEO_BIOS_TABLE.store(Box::into_raw(Box::new(table)), Ordering::Release);
    }
}

/// The video BIOS a hybrid laptop's `ATRM` handed out during the PCI
/// census (`stela_drivers::probe::gpu_methods::atrm`).
static ATRM: AtomicPtr<stela_drivers::probe::gpu_methods::Atrm> = AtomicPtr::new(ptr::null_mut());

/// Keeps what `ATRM` handed out for the claims, with a line.
pub(super) fn keep_atrm(atrm: Option<stela_drivers::probe::gpu_methods::Atrm>) {
    let Some(atrm) = atrm else { return };
    output::arguments(format_args!(
        "GPU: ATRM at {} on {} handed out {} bytes of video BIOS\n",
        atrm.path,
        atrm.owner,
        atrm.image.len()
    ));
    let previous = ATRM.swap(Box::into_raw(Box::new(atrm)), Ordering::AcqRel);
    if !previous.is_null() {
        // SAFETY: only this function stores, from `Box::into_raw`, and the
        // claims read it only after the census that calls this once.
        drop(unsafe { Box::from_raw(previous) });
    }
}

/// The firmware's PCI map and the option ROMs it kept, for the claims.
struct FirmwareMap {
    functions: Vec<stela_boot_core::pci_map::Function>,
    /// Each display function's option ROM, by its place in `functions`.
    roms: Vec<(usize, Vec<u8>)>,
}

static FIRMWARE_MAP: AtomicPtr<FirmwareMap> = AtomicPtr::new(ptr::null_mut());

/// Keeps the firmware's PCI map (`pci_io::firmware_map`) and copies of the
/// option ROMs it names.
///
/// # Safety
/// `root` is the loader root that maps `region` and the copies it names,
/// pages the boot kept. Called once, before any claim.
pub(super) unsafe fn keep_pci_map(region: crate::handoff::Region, root: u64) {
    if region.bytes == 0 {
        return;
    }
    let map = {
        // SAFETY: the caller's; the loader root maps the kept pages.
        let _root = unsafe { super::storage::RestoreRoot::enter(root) };
        let read = |range: Range<u64>| -> Vec<u8> {
            // SAFETY: as above: kept pages the boot wrote before its exit.
            unsafe {
                core::slice::from_raw_parts(
                    range.start as *const u8,
                    (range.end - range.start) as usize,
                )
            }
            .to_vec()
        };
        let Some(functions) = stela_boot_core::pci_map::decode(&read(
            region.physical..region.physical + region.bytes,
        )) else {
            output::arguments(format_args!("GPU: the firmware's PCI map is unreadable\n"));
            return;
        };
        let roms = functions
            .iter()
            .enumerate()
            .filter_map(|(index, function)| Some((index, read(function.rom.clone()?))))
            .collect();
        FirmwareMap { functions, roms }
    };
    FIRMWARE_MAP.store(Box::into_raw(Box::new(map)), Ordering::Release);
}

/// What the memory plan saw of the display controllers (`Handoff::gpu_census`).
static CENSUS: AtomicPtr<stela_drivers::gpu::selection::Planned> = AtomicPtr::new(ptr::null_mut());

/// Keeps what the memory plan saw of the display controllers, so the
/// census takes the kinds it took. Called once, before any claim.
pub(super) fn keep_census(census: Option<stela_drivers::gpu::selection::Planned>) {
    if let Some(census) = census {
        CENSUS.store(Box::into_raw(Box::new(census)), Ordering::Release);
    }
}

fn planned_census() -> Option<&'static stela_drivers::gpu::selection::Planned> {
    // SAFETY: stored once before any claim and never freed.
    unsafe { CENSUS.load(Ordering::Acquire).as_ref() }
}

fn firmware_map() -> Option<&'static FirmwareMap> {
    // SAFETY: stored once before any claim and never freed.
    unsafe { FIRMWARE_MAP.load(Ordering::Acquire).as_ref() }
}

/// The function's platform ROM, a video BIOS image firmware publishes
/// apart from the function, which Linux keeps as `pci_dev::rom` and amdgpu
/// reads when the places it searches first hold none
/// (`amdgpu_read_platform_bios`). Linux's own places come in amdgpu's order,
/// those its ACPI support reads first, as Stela's Linux profile has none:
/// `ATRM` for a discrete GPU (`amdgpu_atrm_get_bios`), then the `VFCT`
/// image (`amdgpu_acpi_vfct_bios`), then the option ROM copy the firmware
/// kept (x86 Linux's `pci_dev::rom` from its EFI stub). One line says which.
/// A driver that does not read its video BIOS from those places
/// (`Kernel::reads_rom`) gets the `VFCT` image alone, as every claim did.
fn platform_rom(adapter: &gpu::Adapter, kernel: &gpu::stack::Kernel) -> &'static [u8] {
    let address = adapter.address;
    #[cfg(feature = "amdgpu")]
    if kernel.reads_rom
        && let Some(image) = atrm_image(adapter, (kernel.integrated)(adapter))
    {
        return image;
    }
    if let Some(image) = vfct_image(adapter) {
        return image;
    }
    if !kernel.reads_rom {
        return &[];
    }
    let Some(map) = firmware_map() else {
        return &[];
    };
    let Some((_, image)) = map.roms.iter().find(|(index, _)| {
        let function = &map.functions[*index];
        (
            function.segment,
            function.bus,
            function.device,
            function.function,
        ) == (
            address.segment,
            address.bus,
            address.device,
            address.function,
        )
    }) else {
        return &[];
    };
    output::arguments(format_args!(
        "GPU {address}: video BIOS of {} bytes from the option ROM the firmware read, given to \
         Linux as its platform ROM\n",
        image.len()
    ));
    image
}

/// `ATRM`'s image for an AMD GPU amdgpu takes as discrete
/// (`amdgpu_get_bios_dgpu` asks `ATRM` first; its APU path never does):
/// one its table names and does not flag as an APU, or one it identifies by
/// discovery whose image names it, as only the probe tells those apart.
/// Only an ATOM BIOS, as `check_atom_bios` takes one.
#[cfg(feature = "amdgpu")]
fn atrm_image(adapter: &gpu::Adapter, integrated: bool) -> Option<&'static [u8]> {
    use stela_boot_core::video_bios;
    use stela_loader::host_firmware::recipes::AmdGpu;
    if adapter.vendor != gpu::Vendor::Amd || integrated {
        return None;
    }
    // Whether amdgpu takes it as discrete: yes for one its table names
    // without the APU flag; unknown before the probe for a discovered one.
    let discrete = match AmdGpu::of(adapter.device) {
        AmdGpu::Named(..) => Some(true),
        AmdGpu::Discovered => None,
        AmdGpu::LeftToRadeon | AmdGpu::LeftOut(_) => return None,
    };
    // SAFETY: stored once by the census before any claim and never freed.
    let atrm = unsafe { ATRM.load(Ordering::Acquire).as_ref() }?;
    let address = adapter.address;
    let image = atrm.image.as_slice();
    let names_it = video_bios::names(image) == Some((0x1002, adapter.device));
    if !video_bios::atom(image) || (discrete.is_none() && !names_it) {
        output::arguments(format_args!(
            "GPU {address}: ATRM's {} bytes at {} are not taken: {}\n",
            image.len(),
            atrm.path,
            if video_bios::atom(image) {
                "the image names another device and amdgpu decides only at its probe whether this one is discrete"
            } else {
                "they are no ATOM BIOS"
            }
        ));
        return None;
    }
    output::arguments(format_args!(
        "GPU {address}: video BIOS of {} bytes from ATRM at {}, given to Linux as its platform \
         ROM\n",
        image.len(),
        atrm.path
    ));
    Some(image)
}

/// The function's image in the kept VFCT table, as Linux takes it
/// (`amdgpu_acpi_vfct_bios`); a refused image is said once.
fn vfct_image(adapter: &gpu::Adapter) -> Option<&'static [u8]> {
    use stela_boot_core::vfct;
    // SAFETY: stored once before any claim and never freed.
    let table = unsafe { VIDEO_BIOS_TABLE.load(Ordering::Acquire).as_ref() }?;
    let address = adapter.address;
    let function = vfct::Function {
        bus: address.bus,
        device: address.device,
        function: address.function,
        vendor: adapter.vendor.pci_id(),
        device_id: adapter.device,
    };
    match vfct::image(table, function) {
        Ok(image) => {
            if let Some(bus) = image.renumbered_from {
                output::arguments(format_args!(
                    "GPU {address}: its VFCT image names bus {bus:#x}; taken, as Linux takes it\n"
                ));
            }
            output::arguments(format_args!(
                "GPU {address}: video BIOS of {} bytes from the ACPI VFCT table, given to Linux as its platform ROM\n",
                image.bytes.len()
            ));
            Some(&table[image.bytes])
        }
        Err(vfct::Error::NoImage) => None,
        Err(error) => {
            output::arguments(format_args!(
                "GPU {address}: the ACPI VFCT table's image is refused ({error:?}); the driver looks for its video BIOS elsewhere\n"
            ));
            None
        }
    }
}

/// The expansion ROM BAR's register, its enable bit and its address bits.
const ROM_BAR: u16 = 0x30;
const ROM_ENABLE: u32 = 1;
const ROM_ADDRESS: u32 = 0xffff_f800;

/// The function's expansion ROM BAR as Linux's PCI core leaves it for
/// `pci_map_rom`, decoding off: sized, and kept where firmware assigned it
/// inside the window above it, else placed in that window clear of every
/// BAR the firmware sized below it (`stela_boot_core::pci_map::rom_room`),
/// as Linux assigns an unassigned resource. The region for Linux's
/// `PCI_ROM_RESOURCE` and its reservation, or `None` with a line where the
/// function has no ROM BAR or no room is found; the BAR is then put back
/// as firmware left it.
fn expansion_rom(
    services: &dyn NativeServices,
    function: &mut Function<OwnedConfig>,
) -> Option<(Region, owned::Resource)> {
    use stela_boot_core::{pci_map::rom_room, video_bios::rom_size};
    /// Linux's `IORESOURCE_READONLY` and `IORESOURCE_SIZEALIGN`.
    const RESOURCE_READONLY: u64 = 0x4000;
    const RESOURCE_SIZEALIGN: u64 = 0x4_0000;
    let address = function.address;
    let config = function.config();
    let original = config.read32(ROM_BAR).ok()?;
    // Sized with its decoding off, as `pci_read_bases` sizes it.
    let read = config
        .write32(ROM_BAR, ROM_ADDRESS)
        .and_then(|()| config.read32(ROM_BAR));
    let put_back = |config: &mut OwnedConfig| config.write32(ROM_BAR, original & !ROM_ENABLE);
    let Some(size) = read.ok().and_then(rom_size).map(u64::from) else {
        let _ = put_back(config);
        return None;
    };
    // A ROM smaller than a page takes a page, aligned to it.
    let span = size.max(PAGE);
    let claim = |start: u64| {
        services.bar_resource(
            address,
            Bar::Memory {
                address: start,
                size: span,
                prefetchable: false,
                wide: false,
            },
        )
    };
    let assigned = u64::from(original & ROM_ADDRESS);
    let region = |start: u64| Region {
        start,
        size,
        flags: RESOURCE_MEMORY | RESOURCE_PREFETCHABLE | RESOURCE_READONLY | RESOURCE_SIZEALIGN,
    };
    // Firmware's place, when it is one: aligned and not the all-ones the
    // firmware's own sizing left.
    if assigned != 0
        && assigned.is_multiple_of(span)
        && assigned != u64::from(ROM_ADDRESS) & !(size - 1)
        && let Ok(resource) = claim(assigned)
    {
        let _ = put_back(config);
        output::arguments(format_args!(
            "GPU {address}: its expansion ROM of {size} bytes stays where firmware placed it, at \
             {assigned:#x}\n"
        ));
        return Some((region(assigned), resource));
    }
    let placed = firmware_map()
        .and_then(|map| rom_room(&map.functions, address.segment, address.bus, span))
        .filter(|&start| start + span <= 1 << 32);
    let found = placed.and_then(|start| {
        let resource = claim(start).ok()?;
        config.write32(ROM_BAR, start as u32).ok()?;
        Some((region(start), resource))
    });
    match &found {
        Some((region, _)) => output::arguments(format_args!(
            "GPU {address}: its expansion ROM of {size} bytes, unassigned by firmware, placed at \
             {:#x} in the window above it, decoding off, as Linux assigns it\n",
            region.start
        )),
        None => {
            let _ = put_back(config);
            output::arguments(format_args!(
                "GPU {address}: its expansion ROM of {size} bytes has no place in the window above \
                 it; its driver reads its video BIOS from the other places it searches\n"
            ));
        }
    }
    found
}

/// Who runs PE work on the boot CPU, once the PEs run.
static HUB: AtomicPtr<Hub> = AtomicPtr::new(ptr::null_mut());
static BOOT_PE: AtomicUsize = AtomicUsize::new(0);
/// The processor the environment runs on.
#[cfg(feature = "nvk")]
static BOOT_CPU: AtomicU32 = AtomicU32::new(0);

/// End the boot PE's slices for Linux, through `hub`.
pub(super) fn set_hub(hub: Arc<Hub>, pe: usize) {
    BOOT_PE.store(pe, Ordering::Relaxed);
    HUB.store(Arc::into_raw(hub).cast_mut(), Ordering::Release);
}

/// Calls a PE whose guest sleeps waiting for an interrupt, never the boot
/// PE, for the GPUs' host work its idle service does at once: Linux's ready
/// work ([`pump_waiting_pe`]) and a deferred job a guest call waits for
/// (`gate::run_job` with `Taker::Waiting`). A sleeping PE wakes only for its
/// guest or the host's service, so the work otherwise waits for the boot
/// PE's pump or the next PE that stops to wait. Whether one was called;
/// with none asleep, the caller leaves the work to those. Safe with
/// interrupts on: the kick writes this processor's ICR with them off.
fn call_idle_core() -> bool {
    // SAFETY: a published hub is leaked, so it lives for the boot.
    let Some(hub) = (unsafe { HUB.load(Ordering::Acquire).as_ref() }) else {
        return false;
    };
    let flags: u64;
    // SAFETY: CPL0 IF save, restored below on this processor.
    unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags) };
    let called = hub.call_sleeping_server(BOOT_PE.load(Ordering::Relaxed));
    // SAFETY: the save above, on this processor.
    unsafe { core::arch::asm!("push {}", "popfq", in(reg) flags) };
    called
}

/// Calls a core for raised work: a PE whose guest waits for an interrupt
/// takes it; with none asleep, the boot PE's slice ends, and it pumps by
/// [`pump_due`]. A woken PE that found guest work first leaves it to the
/// boot PE's same limits. None while a processor holds the [`TURN`]: a
/// core called then would find it taken, and the holder takes the work in
/// its pass or calls a core as it gives the turn back ([`pump_as`]).
fn call_for_work() {
    if !TURN.wake_due() {
        PUMPS.wakes_held.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if may_move() && call_idle_core() {
        PUMPS.woken.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: a published hub is leaked, so it lives for the boot.
    if let Some(hub) = unsafe { HUB.load(Ordering::Acquire).as_ref() } {
        // A kick writes this processor's ICR, which needs interrupts off:
        // Linux, which raises events too, runs with them on.
        let flags: u64;
        // SAFETY: CPL0 IF save, restored below on this processor.
        unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags) };
        hub.call_host(BOOT_PE.load(Ordering::Relaxed));
        // SAFETY: the save above, on this processor.
        unsafe { core::arch::asm!("push {}", "popfq", in(reg) flags) };
    }
}

/// What wakes the service: a raised line or a call.
struct Events {
    clock: crate::native_clock::ClockServices,
    idle: Arc<dyn Idle>,
    pending: AtomicBool,
}

// SAFETY: the native idle owner consumes the software event under its own
// IRQ exclusion. Publish that event before ringing the boot PE, and use the
// paired installed clock for each absolute wait.
unsafe impl Idle for Events {
    fn clock(&self) -> &Arc<dyn Clock> {
        self.idle.clock()
    }
    fn notify(&self) -> Result<(), WorkError> {
        let raised = !self.pending.swap(true, Ordering::AcqRel);
        if raised {
            PENDING_SINCE.store(host::rdtsc(), Ordering::Release);
        }
        self.clock.notify_dispatch();
        // Only the first event since the last pump wakes a core: later ones
        // find one already called, and the pump takes them all.
        if raised {
            call_for_work();
        }
        Ok(())
    }
    fn wait_until_ns(&self, deadline: Option<u64>) -> Result<(), WorkError> {
        let limit = self
            .idle
            .clock()
            .now_ns()?
            .checked_add(WAIT_MICROS * 1000)
            .ok_or(WorkError::Exhausted)?;
        self.idle
            .wait_until_ns(Some(deadline.map_or(limit, |deadline| deadline.min(limit))))
    }
}

unsafe fn allocate_zeroed(layout: Layout) -> *mut u8 {
    // SAFETY: the registration's contract, which this allocator's one attempt
    // at its lock meets; the C side frees through the global allocator.
    unsafe { crate::ALLOCATOR.try_allocate_zeroed(layout) }
}

/// A function's configuration space, which its MSI source shares with Linux.
/// The two take turns under the host drivers' lock, which every Linux entry
/// holds on whichever processor it runs, and the source's route changes run
/// only in Linux on the boot CPU (`Host::refused_moved`); neither calls out
/// while it holds it.
#[derive(Clone)]
struct SharedConfig(Rc<RefCell<Function<OwnedConfig>>>);

impl ConfigSpace for SharedConfig {
    fn read32(&self, offset: u16) -> Result<u32, ConfigError> {
        self.0.borrow_mut().config().read32(offset)
    }
    fn write32(&mut self, offset: u16, value: u32) -> Result<(), ConfigError> {
        self.0.borrow_mut().config().write32(offset, value)
    }
    fn read16(&self, offset: u16) -> Result<u16, ConfigError> {
        self.0.borrow_mut().config().read16(offset)
    }
    fn read8(&self, offset: u16) -> Result<u8, ConfigError> {
        self.0.borrow_mut().config().read8(offset)
    }
    fn write16(&mut self, offset: u16, value: u16) -> Result<(), ConfigError> {
        self.0.borrow_mut().config().write16(offset, value)
    }
    fn write8(&mut self, offset: u16, value: u8) -> Result<(), ConfigError> {
        self.0.borrow_mut().config().write8(offset, value)
    }
}

/// The MSI of one function, routed to the boot CPU.
struct MsiSource {
    config: SharedConfig,
    msi: Msi,
    /// The function the route was attached for, which only it may program.
    requester: stela_drivers::bus::pci::requester::RequesterId,
    event: stela_linux::service::IrqEvent,
    environment: *const Environment,
    line: u32,
}

// SAFETY: the function was stopped when claimed, with no route of its own.
// Nothing here frees: config/capability stay with the registration. A close
// turns the message off and reads it back before the registry drains the
// vector, and joins Linux's work for the line (`quiesce`, `synchronize`).
unsafe impl Source for MsiSource {
    fn mask(&mut self) -> Result<(), IrqError> {
        let command = self.config.read16(4)?;
        if command == u16::MAX {
            return Err(IrqError::FailedRead);
        }
        self.config.write16(4, command | 1 << 10)?;
        self.msi.disable(&mut self.config)?;
        let actual = self.config.read16(4)?;
        let control = self.msi.control(&self.config)?;
        if actual == u16::MAX || control == u16::MAX {
            return Err(IrqError::FailedRead);
        }
        if actual & (1 << 10) == 0 || control & 1 != 0 {
            return Err(IrqError::Mask);
        }
        Ok(())
    }
    /// The function sends no message after this: `mask` turns MSI off and
    /// reads the capability back. That read's completion cannot pass a
    /// posted write the function sent before (PCIe ordering; a message
    /// write carries no Relaxed Ordering), so every earlier message has
    /// reached the root complex when it returns, and the registry then
    /// waits for the vector's request and in-flight count to drain. Linux's
    /// `free_irq` waits for no more. Linux's own work for the line is joined
    /// in `synchronize`.
    fn quiesce(&mut self) -> Result<(), IrqError> {
        self.mask()
    }
    /// As [`Self::quiesce`]: the function has one message and no other
    /// producer of it, and the read back is its payload drain.
    fn quiesce_for_fabric_fence(&mut self) -> Result<(), IrqError> {
        self.mask()
    }
    fn route_kind(&self) -> stela_drivers::bus::irq::RouteKind {
        stela_drivers::bus::irq::RouteKind::Msi
    }
    fn requester(&self) -> Option<&stela_drivers::bus::pci::requester::RequesterId> {
        Some(&self.requester)
    }
    unsafe fn program(
        &mut self,
        route: &stela_drivers::bus::irq::AllocatedRoute<'_>,
    ) -> Result<(), IrqError> {
        let Route::Msi(message) = route.programming() else {
            return Err(IrqError::Route);
        };
        let data = u16::try_from(message.data).map_err(|_| IrqError::Route)?;
        self.msi
            .program(&mut self.config, message.address, data)
            .map_err(IrqError::from)
    }
    unsafe fn unmask(&mut self) -> Result<(), IrqError> {
        self.msi.enable(&mut self.config)?;
        let control = self.msi.control(&self.config)?;
        if control == u16::MAX {
            return Err(IrqError::FailedRead);
        }
        if control & 1 == 0 {
            return Err(IrqError::Mask);
        }
        Ok(())
    }
    unsafe fn synchronize(&mut self) -> Result<(), IrqError> {
        unsafe extern "C" {
            fn stela_linux_irq_actions_joined(irq: u32) -> bool;
        }
        // SAFETY: native callback runs below the exact entered Linux owner;
        // C free_irq retains its closing request and joined work until return.
        if !unsafe { stela_linux_irq_actions_joined(self.line) } {
            return Err(IrqError::Drain);
        }
        self.event.begin_close();
        // SAFETY: Registry already closed this exact action and drained native
        // CPU/inflight/EOI; the original pinned environment/event are retained.
        unsafe { (&*self.environment).join_irq(&self.event) }.map_err(|_| IrqError::Drain)
    }
}

/// The uncached mapping of a function's MSI-X table, kept for the boot.
struct UcTable(*mut u32);

// SAFETY: `msix_for` maps exactly the table's pages uncached and never unmaps
// them; each volatile access is one aligned dword in order.
unsafe impl msix::TableAccess for UcTable {
    fn read32(&self, offset: u64) -> u32 {
        // SAFETY: offset below the table length, per the trait.
        unsafe { self.0.byte_add(offset as usize).read_volatile() }
    }
    fn write32(&self, offset: u64, value: u32) {
        // SAFETY: as above.
        unsafe { self.0.byte_add(offset as usize).write_volatile(value) }
    }
}

fn vector_error(error: msix::Error) -> IrqError {
    match error {
        msix::Error::Config(error) => error.into(),
        msix::Error::Readback { .. } | msix::Error::Disabled => IrqError::Mask,
        msix::Error::Control { .. } => IrqError::FailedRead,
        msix::Error::Index => IrqError::Route,
    }
}

/// A function's MSI-X: its table and the lines Linux knows its vectors by.
struct Msix {
    table: Rc<msix::Table<UcTable>>,
    lines: Rc<RefCell<Vec<Line>>>,
    /// The first line given to the vectors, and how many.
    given: Cell<Option<(u32, u32)>>,
}

/// The MSI-X table of `device`, mapped, where its capability places the
/// table and pending bits inside the function's memory BARs.
fn msix_for(
    window: &'static Window,
    device: &Device,
    placement: &Placement,
    lines: &Rc<RefCell<Vec<Line>>>,
) -> Option<Rc<Msix>> {
    let layout = msix::Layout::read(&device.config, device.msix_capability?).ok()?;
    let address = layout.table_address(&placement.regions)?;
    let first = address & !(PAGE - 1);
    let pages = usize::try_from((address - first + layout.table_bytes()).div_ceil(PAGE)).ok()?;
    let base = window.reserve(pages)?;
    for page in 0..pages as u64 {
        if !window.install(base + page * PAGE, first + page * PAGE, Cache::Uncached) {
            if window.remove(base, pages) {
                window.release(base, pages);
            }
            return None;
        }
    }
    let table = UcTable((base + (address - first)) as *mut u32);
    Some(Rc::new(Msix {
        table: Rc::new(msix::Table::new(layout, table)),
        lines: lines.clone(),
        given: Cell::new(None),
    }))
}

/// One MSI-X vector of a function, routed to the boot CPU.
struct MsixSource {
    table: Rc<msix::Table<UcTable>>,
    index: u16,
    requester: stela_drivers::bus::pci::requester::RequesterId,
    event: stela_linux::service::IrqEvent,
    environment: *const Environment,
    line: u32,
}

// SAFETY: as MsiSource's: the vector had no route of its own when its line
// was given. A close masks the vector's entry and reads it back before the
// registry drains the vector, and joins Linux's work for the line
// (`quiesce`, `synchronize`).
unsafe impl Source for MsixSource {
    fn mask(&mut self) -> Result<(), IrqError> {
        self.table.mask(self.index).map_err(vector_error)
    }
    /// The vector sends no message after this: `mask` sets the entry's mask
    /// bit and reads it back (`msix::Table::mask`). That read's completion
    /// cannot pass a posted message write the function sent before, so
    /// every earlier message of the vector has reached the root complex when
    /// it returns, as for `MsiSource`'s.
    fn quiesce(&mut self) -> Result<(), IrqError> {
        self.mask()
    }
    /// As [`Self::quiesce`]: the entry is the vector's one producer, and the
    /// read back is its payload drain.
    fn quiesce_for_fabric_fence(&mut self) -> Result<(), IrqError> {
        self.mask()
    }
    fn route_kind(&self) -> stela_drivers::bus::irq::RouteKind {
        stela_drivers::bus::irq::RouteKind::Msi
    }
    fn requester(&self) -> Option<&stela_drivers::bus::pci::requester::RequesterId> {
        Some(&self.requester)
    }
    unsafe fn program(
        &mut self,
        route: &stela_drivers::bus::irq::AllocatedRoute<'_>,
    ) -> Result<(), IrqError> {
        let Route::Msi(message) = route.programming() else {
            return Err(IrqError::Route);
        };
        self.table
            .program(self.index, message.address, message.data)
            .map_err(vector_error)
    }
    unsafe fn unmask(&mut self) -> Result<(), IrqError> {
        self.table.unmask(self.index).map_err(vector_error)
    }
    unsafe fn synchronize(&mut self) -> Result<(), IrqError> {
        unsafe extern "C" {
            fn stela_linux_irq_actions_joined(irq: u32) -> bool;
        }
        // SAFETY: as MsiSource's.
        if !unsafe { stela_linux_irq_actions_joined(self.line) } {
            return Err(IrqError::Drain);
        }
        self.event.begin_close();
        // SAFETY: as MsiSource's.
        unsafe { (&*self.environment).join_irq(&self.event) }.map_err(|_| IrqError::Drain)
    }
}

/// The GPU interrupts taken, which the host beat reports.
static FIRED: AtomicU64 = AtomicU64::new(0);

/// The GPU interrupts taken since boot.
pub(super) fn interrupts() -> u64 {
    FIRED.load(Ordering::Relaxed)
}

/// Records an interrupt for the service task.
struct Fired {
    event: stela_linux::service::IrqEvent,
}

impl Handler for Fired {
    fn handle(&self) -> Disposition {
        FIRED.fetch_add(1, Ordering::Relaxed);
        match self.event.raise() {
            Ok(()) => Disposition::Handled,
            Err(_) => Disposition::NotMine,
        }
    }
}

/// A function as Linux's PCI core reaches it.
struct Device {
    config: SharedConfig,
    msi: Option<Msi>,
    msix_capability: Option<u16>,
    /// Its MSI-X table, mapped when Linux starts.
    msix: Option<Rc<Msix>>,
    custody: Rc<FunctionCustody>,
    _views: ManuallyDrop<GpuViews>,
    _resources: ManuallyDrop<Vec<owned::Resource>>,
}

impl function::Host for Device {
    type Config = SharedConfig;
    fn config(&mut self) -> &mut SharedConfig {
        &mut self.config
    }
    fn enable_msi(&mut self) -> Result<(), function::MsiError> {
        // The line's route programs it, when nouveau requests its interrupt.
        self.msi
            .map(|_| ())
            .ok_or(function::MsiError::Native(EOPNOTSUPP))
    }
    fn disable_msi(&mut self) -> Result<(), function::MsiError> {
        if let Some(msi) = self.msi {
            msi.disable(&mut self.config)?;
        }
        Ok(())
    }
    fn routes_msix(&self) -> bool {
        self.msix.is_some()
    }
    fn enable_msix(&mut self, vectors: u32) -> Result<(u32, u32), function::MsiError> {
        let Some(msix) = self.msix.clone() else {
            return Err(function::MsiError::Native(EOPNOTSUPP));
        };
        let entries = u32::from(msix.table.layout().entries);
        let Ok(mut lines) = msix.lines.try_borrow_mut() else {
            return Err(function::MsiError::Native(EBUSY));
        };
        // The lines already given, up to the first one retired, serve a
        // write-back after a reset or vectors set up again. New ones stay
        // below the lines Linux leaves to owners.
        let given = msix.given.get();
        let reusable = given.map_or(0, |(first, given)| {
            lines[first as usize..(first + given) as usize]
                .iter()
                .take_while(|line| line.retired.is_none())
                .count() as u32
        });
        let free = stela_linux::service::LINES.saturating_sub(lines.len() as u32);
        if vectors == 0 {
            return Ok((0, reusable.max(free).min(entries)));
        }
        let wanted = vectors.min(entries);
        let (first, count) = match given {
            Some((first, _)) if reusable >= wanted => (first, wanted),
            _ => {
                let count = wanted.min(free);
                if count < wanted {
                    // A driver that needs every vector (xe asks for exactly
                    // the table's entries) then fails with -ENOSPC.
                    output::arguments(format_args!(
                        "GPU MSI-X: {wanted} vectors asked, {free} of Stela's {} interrupt lines \
                         for Linux left, so {count} given\n",
                        stela_linux::service::LINES
                    ));
                }
                if count == 0 {
                    return Err(function::MsiError::Native(ENOSPC));
                }
                let first = lines.len() as u32;
                lines
                    .try_reserve(count as usize)
                    .map_err(|_| function::MsiError::Native(ENOMEM))?;
                lines.extend((0..count).map(|index| Line {
                    config: self.config.clone(),
                    msi: None,
                    msix: Some((msix.table.clone(), index as u16)),
                    custody: self.custody.clone(),
                    registration: None,
                    retired: None,
                }));
                msix.given.set(Some((first, count)));
                (first, count)
            }
        };
        drop(lines);
        match msix.table.enable(&mut self.config) {
            Ok(()) => Ok((first, count)),
            Err(msix::Error::Config(error)) => Err(function::MsiError::Config(error)),
            Err(_) => Err(function::MsiError::Native(EIO)),
        }
    }
    fn disable_msix(&mut self) -> Result<(), function::MsiError> {
        let Some(msix) = &self.msix else {
            return Ok(());
        };
        match msix.table.disable(&mut self.config) {
            Ok(()) => Ok(()),
            Err(msix::Error::Config(error)) => Err(function::MsiError::Config(error)),
            Err(_) => Err(function::MsiError::Native(EIO)),
        }
    }
}

/// An interrupt line Linux knows a function by, its index in `Host::lines`.
struct Line {
    config: SharedConfig,
    msi: Option<Msi>,
    /// The table and entry of an MSI-X vector's line.
    msix: Option<(Rc<msix::Table<UcTable>>, u16)>,
    custody: Rc<FunctionCustody>,
    registration: Option<Box<dyn IrqRegistration>>,
    retired: Option<stela_drivers::bus::irq::RetiredSource<dyn Source>>,
}

/// What Linux code gets from Stela.
struct Host {
    work: &'static KernelWork,
    task_data: Arc<dyn TaskDataBacking>,
    tsc_hz: u64,
    clock: crate::native_clock::CounterEpoch,
    realtime: Option<RealtimeOrigin>,
    window: &'static Window,
    image: Arc<kernel_work::ReadOnlyImage>,
    random: RefCell<Seeded>,
    environment: Cell<*const Environment>,
    /// The memory BARs Stela claimed and the graphics stolen memory the
    /// firmware reserved for a claimed GPU: all the device memory Linux may
    /// map. A GPU claimed after a failed start adds its own ([`start_more`]).
    bars: RefCell<Vec<Range<u64>>>,
    /// Device mappings' sizes in pages, by address.
    devices: RefCell<BTreeMap<u64, usize>>,
    /// Shared with each function's MSI-X, which adds its vectors' lines.
    lines: Rc<RefCell<Vec<Line>>>,
    /// The unsupported operations already reported.
    reported: RefCell<BTreeSet<String>>,
    /// Linux's last line and its repeats in a row, which are counted, not
    /// written again ([`stela_boot_core::repeated_lines`]).
    repeats: RefCell<stela_boot_core::repeated_lines::Repeats>,
    /// How often each `WARN` site fired: a site that fires on every
    /// interrupt or frame prints at its first and each power of two.
    warned: RefCell<stela_boot_core::gpu_triage::Repeats>,
    /// The first error Linux reported, which a stop before the start
    /// finished names as the reason the GPU did not start.
    first_error: RefCell<Option<String>>,
    /// The first error Linux reported since the probe running now began,
    /// which names why that GPU did not start ([`Unbound::error`]).
    probe_error: RefCell<Option<String>>,
    /// What GSP-RM said about its start, which that reason then names.
    gsp_start: RefCell<stela_boot_core::gpu_status::GspStart>,
    /// Each claimed GPU's PCI address and maker, which that reason names
    /// the failed GPU by.
    gpus: RefCell<Vec<(String, gpu::Vendor)>>,
}

impl Host {
    fn environment(&self) -> Pin<&Environment> {
        // SAFETY: set before any task runs, to a pinned box kept for the boot.
        unsafe { Pin::new_unchecked(&*self.environment.get()) }
    }

    fn map(&self, pages: usize, frame: impl Fn(u64) -> u64, cache: Cache) -> Option<*mut c_void> {
        let address = self.window.reserve(pages)?;
        for page in 0..pages as u64 {
            if !self
                .window
                .install(address + page * PAGE, frame(page), cache)
            {
                self.unmap(address, pages);
                return None;
            }
        }
        Some(address as *mut c_void)
    }

    fn unmap(&self, address: u64, pages: usize) {
        // A processor that kept the translation keeps the addresses too.
        if self.window.remove(address, pages) {
            self.window.release(address, pages);
        }
    }

    fn report(&self, line: core::fmt::Arguments<'_>) {
        use stela_boot_core::repeated_lines::Verdict;
        // A report made while one is being counted is written as it is.
        let verdict = self
            .repeats
            .try_borrow_mut()
            .map_or(Verdict::Write { repeated: None }, |mut repeats| {
                repeats.line(line)
            });
        match verdict {
            Verdict::Write { repeated } => {
                if let Some(count) = repeated {
                    output::arguments(format_args!(
                        "LINUX {}\n",
                        stela_boot_core::repeated_lines::Counted { count, ended: true }
                    ));
                }
                output::arguments(format_args!("LINUX {line}\n"));
            }
            Verdict::Repeat { count: Some(count) } => output::arguments(format_args!(
                "LINUX {}\n",
                stela_boot_core::repeated_lines::Counted {
                    count,
                    ended: false
                }
            )),
            Verdict::Repeat { count: None } => {}
        }
    }

    /// Whether Linux runs on a processor in the boot CPU's place, which
    /// refuses `operation`: interrupt routes are the boot CPU's, and the
    /// GPUs' stop, which changes them, closes moves first (`close_moves`).
    /// Reported once.
    fn refused_moved(&self, operation: &str) -> bool {
        if !host::moved_here() {
            return false;
        }
        if self
            .reported
            .borrow_mut()
            .insert(format!("moved {operation}"))
        {
            self.report(format_args!(
                "unsupported: {operation} on a processor in the boot CPU's place; refused"
            ));
        }
        true
    }
}

// SAFETY: work owns the original queue, code and root. Its live dispatcher
// scope authenticates the same CPU and loader root for each bounded loan.
// task_data separately pins the original writable image and heap generation.
unsafe impl KernelThreadPlatform for Host {
    fn task_data_backing(&self) -> Result<Arc<dyn TaskDataBacking>, WorkError> {
        Ok(self.task_data.clone())
    }

    unsafe fn with_process<R>(
        &self,
        body: impl FnOnce(&dyn stela_drivers::platform::work::EnteredProcess) -> R,
    ) -> Result<R, WorkError> {
        // SAFETY: the caller supplies the bounded, non-switching metadata leaf.
        unsafe { self.work.with_linux_process(body) }
    }
}

impl Platform for Host {
    fn calibrated_tsc_hz(&self) -> Option<core::num::NonZeroU64> {
        core::num::NonZeroU64::new(self.tsc_hz)
    }

    fn now_ns(&self) -> i64 {
        let sample = self.clock.now_ns().and_then(|now| {
            i64::try_from(now)
                .ok()
                .filter(|&now| now != i64::MAX)
                .ok_or(WorkError::Exhausted)
        });
        sample.unwrap_or_else(|error| output::fatal(format_args!("Linux clock failed: {error}")))
    }

    fn boottime_ns(&self) -> Result<u64, WorkError> {
        // The retained native counter epoch starts before Linux starts, and
        // its admission excludes suspend. Both Linux clocks share this epoch.
        self.clock.now_ns()
    }

    fn realtime_at(&self, ns: i64, read: RealtimeRead) -> Option<(i64, u32)> {
        self.realtime.as_ref()?.read(ns, read)
    }

    fn minimum_delay_us(&self, minimum: u64) -> Result<(), WorkError> {
        self.clock.minimum_delay_us(minimum)
    }

    fn minimum_deadline_us(
        &self,
        minimum: u64,
    ) -> Result<Box<dyn stela_linux::owner::MinimumDeadline>, WorkError> {
        let nanoseconds = minimum.checked_mul(1000).ok_or(WorkError::Exhausted)?;
        Ok(Box::new(self.clock.minimum_deadline(nanoseconds)?))
    }

    fn log(&self, level: i32, message: &CStr) {
        // A line Linux prints is progress: a long step that reports it is
        // not stuck.
        super::step_watchdog::progress();
        let message = message.to_str().unwrap_or("(not UTF-8)").trim_end();
        self.report(format_args!("<{level}> {message}"));
        #[cfg(feature = "nvk")]
        gate::driver_line(message);
        // KERN_ALERT to KERN_ERR; KERN_EMERG is a terminal stop's own report.
        self.gsp_start.borrow_mut().observe(message);
        if (1..=3).contains(&level) {
            for kept in [&self.first_error, &self.probe_error] {
                let mut first = kept.borrow_mut();
                if first.is_none() {
                    *first = Some(message.to_owned());
                }
            }
        }
    }

    fn fatal(&self, message: &CStr) {
        // A named stop in place of the C caller's trap, which the engine
        // would report as an unexpected exception.
        let message = message.to_str().unwrap_or("(not UTF-8)").trim_end();
        // Before the start finished, its first error is why the GPU did not
        // start; after it, an error is a running GPU's and names nothing.
        let (failed_start, firmware) = match installed() {
            None => (
                self.first_error.borrow().clone(),
                self.gsp_start.borrow().failure(),
            ),
            Some(_) => (None, None),
        };
        let maker = failed_start.as_deref().and_then(|error| {
            stela_boot_core::gpu_status::failed_maker(error, &self.gpus.borrow())
        });
        output::fatal(stela_boot_core::gpu_status::driver_stopped(
            message,
            failed_start.as_deref(),
            firmware.as_deref(),
            maker,
        ))
    }

    fn warn(&self, file: &CStr, line: i32, condition: Option<&CStr>) {
        let file = file.to_str().unwrap_or("(not UTF-8)");
        let condition = condition.and_then(|text| text.to_str().ok()).unwrap_or("");
        // Counted without allocating, as a WARN may come while the heap is
        // exhausted; one raised while a count is made, from the line sink
        // below it, is written as it is.
        let seen = match self.warned.try_borrow_mut() {
            Ok(mut warned) => warned.count(format_args!("{file}:{line}")),
            Err(_) => Some(stela_boot_core::gpu_triage::Seen::First),
        };
        let Some(seen) = seen else {
            return;
        };
        self.report(format_args!("WARN {file}:{line} {condition}{seen}"));
    }

    fn unsupported(&self, operation: &CStr) {
        let operation = operation.to_str().unwrap_or("(not UTF-8)");
        if self.reported.borrow_mut().insert(operation.to_owned()) {
            self.report(format_args!("unsupported: {operation}"));
        }
    }

    fn random_bytes(&self, bytes: &mut [u8]) {
        self.random.borrow_mut().fill(bytes);
    }

    fn mix_random(&self, bytes: &[u8]) -> bool {
        self.random.borrow_mut().mix(host::rdtsc(), bytes);
        true
    }

    fn timer_changed(&self) -> Result<(), WorkError> {
        // SAFETY: after publication this points to the original pinned
        // environment, kept for the boot. A prepublication call is refused.
        let environment =
            unsafe { self.environment.get().as_ref() }.ok_or(WorkError::Unavailable)?;
        environment.timer_changed()
    }

    fn shutdown(&self, force: bool) {
        super::service::request_shutdown(force);
    }

    fn map_device(&self, physical: u64, size: usize, write_combining: bool) -> Option<*mut c_void> {
        let end = physical.checked_add(size as u64)?;
        if !self
            .bars
            .borrow()
            .iter()
            .any(|bar| bar.start <= physical && end <= bar.end)
        {
            return None;
        }
        let pages = size / PAGE as usize;
        let cache = if write_combining {
            Cache::WriteCombining
        } else {
            Cache::Uncached
        };
        let address = self.map(pages, |page| physical + page * PAGE, cache)?;
        self.devices.borrow_mut().insert(address as u64, pages);
        Some(address)
    }

    unsafe fn unmap_device(&self, address: *mut c_void) {
        let pages = self.devices.borrow_mut().remove(&(address as u64));
        if let Some(pages) = pages {
            unmapped(Unmapped::Device);
            // Device memory never becomes RAM, and Linux drops the mapping
            // with its last use: its shootdown waits for others'. Page
            // tables nouveau frees cost one iounmap each.
            self.window.remove_lazily(address as u64, pages);
        }
    }

    fn map_pages(&self, physical: &[u64], write_combining: bool) -> Option<*mut c_void> {
        let cache = if write_combining {
            Cache::WriteCombining
        } else {
            Cache::WriteBack
        };
        self.map(physical.len(), |page| physical[page as usize], cache)
    }

    unsafe fn unmap_pages(&self, address: *mut c_void, count: usize) {
        unmapped(Unmapped::Pages);
        self.unmap(address as u64, count);
    }

    fn irq_enable(&self, irq: u32) -> Result<(), stela_linux::owner::IrqEnableError> {
        use stela_linux::owner::IrqEnableError;
        if self.refused_moved("irq_enable") {
            // SAFETY: refused before any route, event or source.
            return Err(unsafe { IrqEnableError::unexposed(EOPNOTSUPP) });
        }
        // Refuse a foreign native execution context before classifying any
        // owner return; no errno itself proves a clean unpublished attempt.
        let controller = native_irq::service().map_err(|_| IrqEnableError::retained(ENODEV))?;
        let mut lines = self.lines.borrow_mut();
        let Some(line) = lines.get_mut(irq as usize) else {
            // SAFETY: checked original process, before any route/event/source.
            return Err(unsafe { IrqEnableError::unexposed(EINVAL) });
        };
        if line.retired.is_some() {
            return Err(IrqEnableError::retained(EOPNOTSUPP));
        }
        if let Some(registration) = &mut line.registration {
            return registration
                .unmask()
                .map_err(|_| IrqEnableError::retained(EIO));
        }
        let vector = match (&line.msix, line.msi) {
            (Some((table, index)), _) => Ok((table.clone(), *index)),
            (None, Some(msi)) => Err(msi),
            // SAFETY: no native attempt or service epoch was issued.
            (None, None) => return Err(unsafe { IrqEnableError::unexposed(EOPNOTSUPP) }),
        };
        // The attachment's own route: its root decides how messages arrive.
        // SAFETY: a short borrow on the original process; nothing reenters.
        let route = match unsafe {
            line.custody
                .with_native(|attachment| attachment.irq_route())
        } {
            Ok(Ok(route)) => route,
            // SAFETY: no route, event or source was issued.
            _ => return Err(unsafe { IrqEnableError::unexposed(EOPNOTSUPP) }),
        };
        // SAFETY: actual original entered process; any preceding epoch joined.
        let event = unsafe { self.environment().irq_epoch(irq) }
            .map_err(|_| IrqEnableError::retained(EIO))?;
        let source: Box<dyn Source> = match vector {
            Ok((table, index)) => Box::new(MsixSource {
                table,
                index,
                requester: line.custody.requester().clone(),
                event: event.clone(),
                environment: self.environment.get(),
                line: irq,
            }),
            Err(msi) => Box::new(MsiSource {
                config: line.config.clone(),
                msi,
                requester: line.custody.requester().clone(),
                event: event.clone(),
                environment: self.environment.get(),
                line: irq,
            }),
        };
        let handler = Arc::new(Fired {
            event: event.clone(),
        });
        match controller.request(stela_drivers::bus::irq::Request {
            source,
            handler,
            routing: stela_drivers::bus::irq::RouteRequest::Msi(route),
        }) {
            Ok(registration) => {
                line.registration = Some(registration);
                Ok(())
            }
            Err(refusal) => {
                let errno = match refusal.error {
                    stela_drivers::bus::irq::IrqError::Exhausted => ENOMEM,
                    stela_drivers::bus::irq::IrqError::Unavailable => ENODEV,
                    stela_drivers::bus::irq::IrqError::Unsupported
                    | stela_drivers::bus::irq::IrqError::Delivery(
                        stela_drivers::bus::irq::posted_msi::Error::RootUnavailable
                        | stela_drivers::bus::irq::posted_msi::Error::Unsupported,
                    ) => EOPNOTSUPP,
                    _ => EIO,
                };
                if errno == EOPNOTSUPP {
                    self.unsupported(c"interrupt notification admission");
                } else {
                    self.report(format_args!("interrupt {irq} refused: {refusal:?}"));
                }
                let Some(untouched) = refusal.returned else {
                    return Err(IrqEnableError::retained(EIO));
                };
                // SAFETY: actual checked prepublication return; extraction
                // repeats the original process/context/provider checks.
                let (source, handler, routing) =
                    unsafe { untouched.into_parts() }.map_err(|_| IrqEnableError::retained(EIO))?;
                // Retain actual inputs before any fallible cancellation/join.
                let mut source = core::mem::ManuallyDrop::new(source);
                let mut handler = core::mem::ManuallyDrop::new(handler);
                let stela_drivers::bus::irq::RouteRequest::Msi(route) = routing else {
                    return Err(IrqEnableError::retained(EIO));
                };
                // SAFETY: a short borrow on the original process; the grant
                // was never published, and a failed cancel keeps it retained.
                let cancelled = unsafe {
                    line.custody.with_native(|attachment| {
                        // A failed cancel hands the grant back; keep it.
                        attachment
                            .cancel_unpublished_irq(route)
                            .map_err(core::mem::forget)
                            .is_ok()
                    })
                };
                if cancelled != Ok(true) {
                    return Err(IrqEnableError::retained(EIO));
                }
                event.begin_close();
                // SAFETY: RequestError returned actual untouched owners before
                // Slot/root/source publication, so this new event has no native
                // callback producer. Join still checks exact epoch/activity.
                unsafe { self.environment().join_irq(&event) }
                    .map_err(|_| IrqEnableError::retained(EIO))?;
                // SAFETY: same original process, unused route canceled and exact
                // new service epoch actually joined; no C action reachability.
                unsafe {
                    core::mem::ManuallyDrop::drop(&mut source);
                    core::mem::ManuallyDrop::drop(&mut handler);
                }
                // SAFETY: all original return/cancel/join facts above, not errno.
                Err(unsafe { IrqEnableError::unexposed(errno) })
            }
        }
    }

    fn irq_disable(&self, irq: u32) -> Result<(), Errno> {
        if self.refused_moved("irq_disable") {
            return Err(EOPNOTSUPP);
        }
        let mut lines = self.lines.borrow_mut();
        let line = lines.get_mut(irq as usize).ok_or(EINVAL)?;
        match &mut line.registration {
            Some(registration) => registration.mask().map_err(|_| EIO),
            None => Ok(()),
        }
    }
    fn irq_retire(&self, irq: u32) -> Result<(), Errno> {
        if self.refused_moved("irq_retire") {
            return Err(EOPNOTSUPP);
        }
        let mut lines = self.lines.borrow_mut();
        let line = lines.get_mut(irq as usize).ok_or(EINVAL)?;
        if line.retired.is_some() {
            return Err(EOPNOTSUPP);
        }
        let Some(registration) = line.registration.as_mut() else {
            return Ok(());
        };
        match registration.retire() {
            Err(error) => {
                // What retirement lacked, at which of its steps: Linux keeps
                // the action and the line stays registered.
                let step = registration.retire_step();
                self.report(format_args!(
                    "interrupt {irq} action retained: {error:?} ({error}){}{} on vector {:#x}",
                    if step.is_empty() { "" } else { " at " },
                    step,
                    registration.vector(),
                ));
                Err(EOPNOTSUPP)
            }
            Ok(retired) => {
                // The action is closed: the function sends no message, the
                // vector drained and Linux's work for it joined
                // (`MsiSource::quiesce`, `synchronize`), so Linux may free
                // it. The retired owners stay with the line, which no later
                // request routes again (`irq_enable` refuses it).
                line.retired = Some(retired);
                Ok(())
            }
        }
    }

    unsafe fn copy_from_user(&self, to: *mut u8, from: *const u8, size: usize) -> usize {
        // SAFETY: clients are in-process, so their addresses are ordinary
        // valid memory by the caller's contract.
        unsafe { ptr::copy_nonoverlapping(from, to, size) };
        0
    }

    unsafe fn copy_to_user(&self, to: *mut u8, from: *const u8, size: usize) -> usize {
        // SAFETY: as in `copy_from_user`.
        unsafe { ptr::copy_nonoverlapping(from, to, size) };
        0
    }

    unsafe fn copy_from_kernel_nofault(
        &self,
        to: *mut u8,
        from: *const u8,
        size: usize,
    ) -> Result<(), Errno> {
        if native_irq::current_context() != Ok(None) {
            return Err(EFAULT);
        }
        let heap = native_memory::SOURCE.registration();
        let in_heap = heap.generation != 0
            && from
                .addr()
                .checked_sub(heap.cpu)
                .is_some_and(|offset| offset <= heap.bytes && size <= heap.bytes - offset);
        if !in_heap && !self.image.contains_range(from.addr(), size) {
            return Err(ERANGE);
        }
        if from.addr().abs_diff(to.addr()) < size {
            return Err(EINVAL);
        }
        // SAFETY: the retained heap or immutable image owns the full source
        // range. The engine host IDT is active on this CPU, and the C caller
        // owns the disjoint writable destination.
        if unsafe { stela_boot_core::nofault_read::read(to, from, size) } == 0 {
            Ok(())
        } else {
            Err(EFAULT)
        }
    }

    fn available_page_frames(&self, start: usize, pages: usize) -> Result<usize, Errno> {
        if native_irq::current_context() != Ok(None) {
            return Err(EFAULT);
        }
        let heap = native_memory::SOURCE.registration();
        let bytes = pages
            .checked_mul(stela_boot_core::native_memory::PAGE)
            .ok_or(ERANGE)?;
        let end = start.checked_add(bytes).ok_or(ERANGE)?;
        if heap.generation == 0 || start < heap.cpu || end - heap.cpu > heap.bytes {
            return Err(ERANGE);
        }
        Ok(crate::ALLOCATOR.available_page_frames_in(start..end))
    }

    fn map_user(&self, space: *mut c_void, address: u64, pfn: u64, prot: u64) -> Result<(), Errno> {
        if !space.is_null() {
            return Err(EOPNOTSUPP);
        }
        let entry = page_window::client_leaf(pfn, prot).ok_or(EINVAL)?;
        if self.window.install_client(address, entry) {
            Ok(())
        } else {
            Err(ENOMEM)
        }
    }

    fn unmap_user(&self, space: *mut c_void, address: u64, size: usize) -> Result<(), Errno> {
        if !space.is_null() {
            return Err(EOPNOTSUPP);
        }
        if self.window.remove(address, size / PAGE as usize) {
            Ok(())
        } else {
            Err(EIO)
        }
    }
}

/// The original pre-reset register observation and configuration epoch.
struct GpuViews {
    config: ConfigRetention,
    pci_words: [u32; 8],
    registers: Mapping,
    /// Register 0 as it read before the reset, where it identifies the chip
    /// and so reads the same after it. None: it is read and not compared.
    identity: Option<u32>,
}

fn gpu_pci_words(function: &mut Function<OwnedConfig>) -> Result<[u32; 8], ConfigError> {
    // Identity and class/revision, then all six restored BAR registers.
    let mut words = [0; 8];
    for (word, offset) in words
        .iter_mut()
        .zip([0, 8, 0x10, 0x14, 0x18, 0x1c, 0x20, 0x24])
    {
        *word = function.config().read32(offset)?;
    }
    Ok(words)
}

// SAFETY: the exclusive original preparation owns this exact configuration
// and register BAR. Any original GOP writers were joined before BAR sizing,
// and its terminal admission remains closed. This checks the same retained
// epoch and reads a harmless UC register on this function after the CPU fence,
// completing its posted writes before Cold is allowed to reset it.
unsafe impl gpu::cold::Views<OwnedConfig> for GpuViews {
    fn seal(&mut self, function: &mut Function<OwnedConfig>) -> Result<(), gpu::cold::ViewError> {
        self.check(function)
    }
}

impl GpuViews {
    fn check(&self, function: &mut Function<OwnedConfig>) -> Result<(), gpu::cold::ViewError> {
        use gpu::cold::ViewError;
        if !function
            .config()
            .matches_retention(&self.config)
            .map_err(|_| ViewError::Context)?
        {
            return Err(ViewError::ForeignSource);
        }
        if function
            .config()
            .read16(4)
            .map_err(|_| ViewError::RegisterReadback)?
            & 2
            == 0
            || gpu_pci_words(function).map_err(|_| ViewError::RegisterReadback)? != self.pci_words
        {
            return Err(ViewError::RegisterReadback);
        }
        // The read is what orders the CPU's posted writes before the reset.
        let read = self.registers.read32(0);
        if read.is_err() || self.identity.is_some_and(|identity| read != Ok(identity)) {
            return Err(ViewError::RegisterReadback);
        }
        Ok(())
    }
}

/// A function claimed from probe, before Linux sees it.
pub(super) struct Claimed {
    /// Whether this GPU may be the one showing the firmware's screen.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    shows_screen: bool,
    /// The firmware's picture this claim holds until its driver's probe
    /// tells whether it ended ([`RetiredScreen`]).
    screen: RetiredScreen,
    device: Device,
    firmware: PciFirmware,
    /// The words of Linux's command line its driver needs for it.
    parameters: Option<String>,
    /// Other functions its driver searches for, published before it.
    searched: Vec<before::Searched>,
    /// The graphics memory the firmware reserved for it.
    stolen: Option<(u64, u64)>,
    placement: Placement,
    bars: Vec<Range<u64>>,
    name: String,
    /// Its market name, or its maker alone.
    market: String,
    /// Its index in the census the selection chose it from.
    census: usize,
    /// Its probe waits for the first turn's: it may only show the frames
    /// another GPU draws ([`stela_drivers::gpu::selection::Choice::Later`]).
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    later: bool,
    /// Another GPU of the census that could draw in its place ([`beside`]),
    /// which a stop inside its probe names.
    beside: Option<String>,
    #[cfg_attr(not(feature = "nvk"), allow(dead_code, reason = "read to open Metal"))]
    stack: &'static GpuStack,
    /// The kernel drivers Linux offers it to, in registration order: its
    /// stack's, then any later one of the vendor whose table holds it (xe
    /// after i915), each of which binds it when those before it did not.
    offered: Vec<&'static str>,
    /// Whether it is integrated as its kernel driver's table says before the
    /// probe ([`stela_drivers::gpu::stack::Kernel::table_integrated`]).
    #[cfg(feature = "nvk")]
    integrated: Option<bool>,
    #[cfg(feature = "nvk")]
    drm: client::DrmDevice,
    #[cfg(feature = "nvk")]
    link: Option<PcieLink>,
    /// The PCI Express port above it, read for the link's lines.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code, reason = "read at a loss"))]
    port: Option<link::Port>,
}

fn claim(
    services: &dyn NativeServices,
    (stack, adapter): (&'static GpuStack, &gpu::Adapter),
    chipset: &[stela_drivers::probe::ChipsetFunction],
    line: u32,
    gop: crate::pci_io::ScreenOwner,
    catalog: Arc<Catalog>,
    port: Option<link::Port>,
) -> Result<Claimed, ServiceError> {
    use crate::pci_io::ScreenOwner;
    let address = adapter.address;
    let before = before::of(stack).ok_or(ServiceError::Refused(
        "the boot has no preparation for this GPU's kernel driver",
    ))?;
    // A GPU whose driver lacks its files is refused before anything of it
    // changes, the firmware's screen included.
    (before.admits)(adapter, &catalog)?;
    let timer = services.minimum_delay()?;
    let mut requester = services.prepare_requester(address)?;
    let bus_reset = requester.bus_reset_available();
    let function = requester.function_mut();
    if (function.vendor, function.device, function.class)
        != (stack.vendor.pci_id(), adapter.device, adapter.class)
    {
        return Err(ServiceError::Refused("the GPU's original identity changed"));
    }
    let translated = services.dma_translated(address);
    gpu::cold::check_candidate(
        function,
        timer.as_ref(),
        translated,
        bus_reset,
        stack.kernel.resets_itself,
    )
    .map_err(|error| cold_refusal(address, error))?;
    let owns_screen = match gop {
        ScreenOwner::Function(source) if source.address == address => {
            source.bar.words().try_for_each(|(offset, expected)| {
                let observed = function
                    .config()
                    .read32(offset)
                    .map_err(ServiceError::Config)?;
                if observed != expected {
                    return Err(ServiceError::Config(ConfigError::before_effect(
                        ConfigFailure::Readback {
                            offset,
                            width: 4,
                            expected,
                            observed,
                        },
                    )));
                }
                Ok(())
            })?;
            true
        }
        // A GPU below a bridge that forwards none of the screen cannot hold
        // it; any other may show it, so none starts beside its writer.
        ScreenOwner::Unknown(apart) => {
            let may = stela_boot_core::firmware_screen::gpu_may_show(
                &apart,
                address.segment,
                address.bus,
            );
            if !may {
                output::arguments(format_args!(
                    "GPU {address}: the firmware's screen lies outside the bridge windows \
                     above it, so it stays with the adapter showing it\n"
                ));
            }
            may
        }
        ScreenOwner::Function(_) | ScreenOwner::NoScreen => false,
    };
    // BAR sizing below temporarily turns off memory decode.
    if owns_screen && !crate::console::retire_firmware() {
        return Err(ServiceError::Refused(
            "a firmware framebuffer writer is still active",
        ));
    }
    // A refusal from here until the GPU is reset leaves what it scans out
    // as it was, so the firmware's screen is given back.
    let mut screen = RetiredScreen::hold(address, owns_screen);
    let mut placement = Placement {
        bus: address.bus,
        devfn: address.device << 3 | address.function,
        irq: line,
        // A mapping past the driver's mask is refused
        // (`function::Mappings`), not truncated.
        dma_limit: stack.kernel.dma_limit(adapter),
        rom: platform_rom(adapter, &stack.kernel),
        ..Placement::default()
    };
    let (mut bars, mut resources) = (Vec::new(), ManuallyDrop::new(Vec::new()));
    let function_bars = function.bars().map_err(ServiceError::Config)?;
    let register_bar = (stack.kernel.registers)(adapter);
    if let Err(why) = gpu::cold::register_bar(function_bars[register_bar], PAGE) {
        output::arguments(format_args!(
            "GPU {address} register BAR refused: {why:?} BAR{register_bar}={:?}\n",
            function_bars[register_bar]
        ));
        return Err(ServiceError::Refused(
            "the GPU's register BAR is not an assigned register window",
        ));
    }
    // The register BAR's resource, among those of the memory BARs.
    let mut registers_at = 0;
    for (index, (region, bar)) in placement.regions.iter_mut().zip(function_bars).enumerate() {
        let Some(
            bar @ Bar::Memory {
                address: start,
                size,
                prefetchable,
                wide,
            },
        ) = bar
        else {
            continue;
        };
        let resource = services.bar_resource(address, bar)?;
        let physical = resource.physical_span();
        if physical.start() != start || physical.bytes() != size {
            return Err(ServiceError::Refused(
                "the GPU BAR is translated by the host bridge",
            ));
        }
        if index == register_bar {
            registers_at = resources.len();
        }
        resources.push(resource);
        bars.push(start..start + size);
        *region = Region {
            start,
            size,
            flags: RESOURCE_MEMORY
                | if prefetchable {
                    RESOURCE_PREFETCHABLE
                } else {
                    0
                }
                | if wide { RESOURCE_WIDE } else { 0 },
        };
    }
    // Each memory BAR lies in a host aperture: decode it, as Linux
    // `pci_enable_device` does before the register reads below.
    let decode = gpu::cold::enable_decode(function, timer.as_ref()).map_err(|error| {
        output::arguments(format_args!(
            "GPU {address} memory decoding refused: {error:?}\n"
        ));
        ServiceError::Refused("the GPU's memory decoding did not turn on")
    })?;
    if decode.power_state != 0 || decode.memory_was_off {
        output::arguments(format_args!(
            "GPU {address}: firmware left it in D{} with memory decoding {}; brought to D0 with decoding on, as Linux pci_enable_device does\n",
            decode.power_state,
            if decode.memory_was_off { "off" } else { "on" }
        ));
    }
    link::log_link(
        address,
        "as the firmware left it",
        &*function.config(),
        port.as_ref(),
    );
    // Before the reset, which saves the GPU's side as set here and puts it
    // back, as Linux sets a link at enumeration before its driver resets.
    // SAFETY: the cold owner of `address` its fabric promoted, in D0 since
    // `enable_decode`, Bus Master sealed by its preparation; the fabric
    // keeps every other configuration writer out.
    unsafe { link::set_substates(address, function, port.as_ref()) };
    let config = function
        .config()
        .retain_epoch()
        .map_err(ServiceError::Mapping)?;
    let pci_words = gpu_pci_words(function).map_err(ServiceError::Config)?;
    let registers = services
        .authority()?
        .map(
            resources[registers_at].clone(),
            0,
            PAGE as usize,
            CacheAttribute::Uncached,
            before.register_page,
        )
        .map_err(ServiceError::Mapping)?;
    let identified = (before.identify)(
        before::Observed {
            services,
            adapter,
            chipset,
            function,
            registers: &registers,
            register_bar: &resources[registers_at],
        },
        catalog,
    )?;
    if let Some(chip) = identified.chip {
        crate::host_inventory::text(alloc::format!("GPU{line}Chip"), chip);
    }
    crate::host_inventory::text(
        alloc::format!("GPU{line}PCIID"),
        alloc::format!("{:04x}:{:04x}", stack.vendor.pci_id(), adapter.device),
    );
    let mut cold = gpu::cold::Cold::new(gpu::cold::Inputs {
        requester,
        views: GpuViews {
            config,
            pci_words,
            registers,
            identity: identified.identity,
        },
        timer,
    });
    if translated {
        // SAFETY: the mount's active remapper translates this requester, and
        // its private domain is the one the cold attempt binds.
        cold = unsafe { cold.translated() };
    }
    if stack.kernel.resets_itself {
        // SAFETY: this stack's driver takes the GPU from the firmware at its
        // probe, resetting it where a previous driver left it running.
        cold = unsafe { cold.driver_resets() };
    } else {
        // The reset below ends the firmware's picture on this GPU. A driver
        // that resets the GPU itself keeps it until a step of its probe ends
        // it, which the probe's result tells.
        screen.end(None);
    }
    let ready = cold
        .quiesce_and_bind()
        .map_err(|failure| cold_refusal(address, failure.error))?;
    // The BARs as firmware sized them, which the reset kept: a BAR as large
    // as the GPU's memory is a Resizable BAR firmware enabled.
    let sizes: Vec<(usize, u64)> = placement
        .regions
        .iter()
        .enumerate()
        .filter(|(_, region)| region.size != 0)
        .map(|(index, region)| (index, region.size))
        .collect();
    let sizes = stela_boot_core::gpu_status::bar_sizes(&sizes);
    // Only a reset of the boot's restores the BARs' firmware sizes.
    let reset = ready.reset().is_some_and(gpu::cold::Reset::by_claim);
    if reset {
        // It ended the firmware's picture too, also on a GPU whose driver
        // would have reset it at its probe: a function-local reset comes
        // first wherever the GPU has one.
        screen.end(None);
    }
    match ready.reset() {
        None => output::arguments(format_args!(
            "GPU {address}: no function-local reset; started as firmware left it, DMA translated; {sizes}\n"
        )),
        Some(gpu::cold::Reset::Driver) => output::arguments(format_args!(
            "GPU {address}: no function-local reset; its driver takes it from the firmware at its probe, with bus mastering off until then; kept {sizes}\n"
        )),
        Some(gpu::cold::Reset::SecondaryBus) => output::arguments(format_args!(
            "GPU {address}: no function-local reset; reset through its bridge's secondary bus; kept {sizes}\n"
        )),
        Some(gpu::cold::Reset::Function(method)) => {
            let name = match method {
                stela_drivers::bus::pci::ResetMethod::FunctionLevel => "Function Level Reset",
                stela_drivers::bus::pci::ResetMethod::PowerManagement => {
                    "a D3hot to D0 power state reset"
                }
            };
            output::arguments(format_args!(
                "GPU {address}: reset by {name}; kept {sizes}\n"
            ));
        }
    }
    // SAFETY: both original owners transfer into this retained function
    // lifetime before Linux probe. Only the temporary UC observation is retired.
    let (bound, views) = unsafe { ready.into_parts() };
    let attachment = ManuallyDrop::new(bound.attachment);
    let mut function = ManuallyDrop::new(bound.requester.into_function());
    if reset {
        resized_bars(address, &function, &mut placement, &mut bars);
    }
    // After the reset, which clears what a ROM BAR held; only for a driver
    // that reads its video BIOS through it.
    if stack.kernel.reads_rom
        && let Some((region, resource)) = expansion_rom(services, &mut function)
    {
        placement.rom_bar = region;
        bars.push(region.start..region.start + region.size.max(PAGE));
        resources.push(resource);
    }
    let mut views = ManuallyDrop::new(views);
    views
        .check(&mut function)
        .map_err(|error| cold_refusal(address, gpu::cold::Error::Views(error)))?;
    if reset {
        link::log_link(
            address,
            "after its reset",
            &*function.config(),
            port.as_ref(),
        );
    }
    (before.settle)(services, &resources[registers_at], &identified)?;
    // SAFETY: this scoped BAR observation was the sole user of the UC mapping.
    unsafe { views.registers.retire() }.map_err(ServiceError::Mapping)?;
    let capabilities = function
        .capabilities()
        .map_err(ServiceError::PciCapability)?;
    let msi = capabilities
        .iter()
        .find(|(_, id)| *id == MSI_CAPABILITY)
        .map(|(offset, _)| Msi::at(u16::from(*offset)));
    let msix_capability = capabilities
        .iter()
        .find(|(_, id)| *id == msix::CAPABILITY)
        .map(|(offset, _)| u16::from(*offset));
    #[cfg(feature = "nvk")]
    let drm = {
        let config = function.config();
        let subsystem = config.read32(SUBSYSTEM_ID).map_err(ServiceError::Config)?;
        let revision = config.read8(REVISION).map_err(ServiceError::Config)?;
        client::DrmDevice {
            segment: address.segment,
            vendor: function.vendor,
            device: function.device,
            subsystem_vendor: subsystem as u16,
            subsystem_device: (subsystem >> 16) as u16,
            bus: address.bus,
            slot: address.device,
            function: address.function,
            revision,
            minor: 0,
        }
    };
    #[cfg(feature = "nvk")]
    let link = capabilities
        .iter()
        .find(|(_, id)| *id == PCIE_CAPABILITY)
        .map(|(offset, _)| {
            function
                .config()
                .read16(u16::from(*offset) + LINK_STATUS)
                .map(PcieLink::from_link_status)
        })
        .transpose()
        .map_err(ServiceError::Config)?;
    Ok(Claimed {
        shows_screen: owns_screen,
        screen,
        device: Device {
            config: SharedConfig(Rc::new(RefCell::new(ManuallyDrop::into_inner(function)))),
            msi,
            msix_capability,
            msix: None,
            custody: FunctionCustody::new(ManuallyDrop::into_inner(attachment)),
            _views: views,
            _resources: resources,
        },
        firmware: identified.firmware,
        parameters: identified.parameters,
        searched: identified.searched,
        stolen: identified.stolen,
        placement,
        bars,
        name: format!("{address}"),
        market: adapter.name(),
        census: 0,
        later: false,
        beside: None,
        stack,
        offered: offered(stack, adapter),
        #[cfg(feature = "nvk")]
        integrated: stack.kernel.table_integrated(adapter),
        #[cfg(feature = "nvk")]
        drm,
        #[cfg(feature = "nvk")]
        link,
        port,
    })
}

/// The firmware's picture while GPU claims hold it
/// ([`stela_boot_core::firmware_picture`]).
static FIRMWARE_PICTURE: stela_boot_core::firmware_picture::Record =
    stela_boot_core::firmware_picture::Record::new();

/// The longest a claim's step waits for [`FIRMWARE_PICTURE`], which any
/// other step holds for a few instructions.
const PICTURE_WAIT_MS: u64 = 50;

/// Runs `f` on the record of the firmware's picture for the claim of the
/// GPU at `address` at its `step`, waiting [`PICTURE_WAIT_MS`] at most. A
/// wait that gives up leaves the picture taken as ended for the rest of the
/// boot, said in one line.
fn with_picture<R>(
    address: stela_drivers::bus::pci::Address,
    step: &str,
    f: impl FnOnce(&mut stela_boot_core::firmware_picture::Held) -> R,
) -> Option<R> {
    use stela_boot_core::firmware_picture::Waited;
    let limit = crate::firmware::tsc_hz() / 1000 * PICTURE_WAIT_MS;
    match FIRMWARE_PICTURE.with_within(host::rdtsc, limit, f) {
        Waited::Done(result) => Some(result),
        Waited::GaveUp { first } => {
            if first {
                output::arguments(format_args!(
                    "GPU {address}: the record of the firmware's picture stayed taken for \
                     {PICTURE_WAIT_MS} ms at its claim's {step}; the picture is taken as ended \
                     for the rest of the boot, so neither the boot's text nor a stop's reason \
                     is drawn on it again\n"
                ));
            }
            None
        }
    }
}

/// The firmware's screen a claim retired before it sized the GPU's BARs.
/// The picture lasts until a step ends it: the claim's reset, or a step of
/// the driver's probe that resets the GPU or starts its display engine
/// (`stela_linux::function::screen_ended`); a driver bound to the GPU drives
/// that screen from then on. A claim that lets go with none of those
/// changed nothing the GPU scans out, so the screen is given back: the
/// boot's text and, when another GPU draws, the guest's copied frames show
/// there again.
struct RetiredScreen {
    address: stela_drivers::bus::pci::Address,
    retired: bool,
}

impl RetiredScreen {
    /// The claim of the GPU at `address`, which retired the firmware's
    /// screen when `retired`.
    fn hold(address: stela_drivers::bus::pci::Address, retired: bool) -> Self {
        if retired {
            let devfn = address.device << 3 | address.function;
            with_picture(address, "hold", |held| held.hold(address.bus, devfn));
        }
        Self { address, retired }
    }

    /// The picture ended: the claim resets the GPU (`None`), or the
    /// driver's probe ended it with this step or bound the GPU.
    fn end(&mut self, step: Option<&str>) {
        if !core::mem::take(&mut self.retired) {
            return;
        }
        let devfn = self.address.device << 3 | self.address.function;
        with_picture(self.address, "end", |held| {
            held.end(self.address.bus, devfn)
        });
        if let Some(step) = step {
            output::arguments(format_args!(
                "GPU {}: the firmware's picture on it ended with its driver's {step}\n",
                self.address
            ));
        }
    }

    /// The claim lets go without ending the picture.
    fn let_go(&mut self) {
        if !core::mem::take(&mut self.retired) {
            return;
        }
        let devfn = self.address.device << 3 | self.address.function;
        let back = with_picture(self.address, "release", |held| {
            held.release(self.address.bus, devfn)
        })
        .is_some_and(|back| back && !FIRMWARE_PICTURE.lost());
        if back && crate::console::restore_firmware() {
            output::arguments(format_args!(
                "GPU {}: the firmware's picture stays on its screen, as nothing reset the GPU \
                 or started its display before its claim ended\n",
                self.address
            ));
        }
    }

    /// What became of the picture once the driver's probe returned without
    /// binding the GPU: the step that ended it is named, and a picture no
    /// step ended is the boot's again.
    fn after_failed_probe(
        &mut self,
        shows_screen: bool,
    ) -> stela_boot_core::gpu_status::Picture<'static> {
        use stela_boot_core::gpu_status::Picture;
        if !shows_screen {
            return Picture::Elsewhere;
        }
        if !self.retired {
            return Picture::ResetForClaim;
        }
        let devfn = self.address.device << 3 | self.address.function;
        match stela_linux::function::screen_ended(self.address.bus, devfn)
            .and_then(|step| step.to_str().ok())
        {
            Some(step) => {
                self.end(Some(step));
                Picture::EndedBy(step)
            }
            None => {
                self.let_go();
                Picture::Kept
            }
        }
    }
}

impl Drop for RetiredScreen {
    fn drop(&mut self) {
        self.let_go();
    }
}

/// At a stop: give the firmware's screen back to the stop's reason when GPU
/// claims hold it and no step ended its picture, as a driver's probe that
/// stops the boot before it reset the GPU or started its display.
pub(crate) fn picture_at_stop() {
    let shows = FIRMWARE_PICTURE.try_with(|held| {
        held.shows_at_stop(|bus, devfn| stela_linux::function::screen_ended(bus, devfn).is_some())
    });
    if shows == Some(true) && !FIRMWARE_PICTURE.lost() && crate::console::restore_firmware() {
        output::arguments(format_args!(
            "GPU: the firmware's picture still shows, so the stop's reason is drawn on it\n"
        ));
    }
}

/// The ports probe found above the GPU at `address`, nearest first, as
/// Linux's PCI code keeps them: its driver reads the link its slot gives it
/// from them, as amdgpu does to set its PCIe speeds.
fn upstream_ports(
    ports: &stela_drivers::probe::GpuPorts,
    address: stela_drivers::bus::pci::Address,
) -> Vec<function::Port> {
    ports
        .iter()
        .find(|(found, _)| *found == address)
        .map_or(&[][..], |(_, ports)| ports)
        .iter()
        .map(|port| function::Port {
            bus: port.address.bus.into(),
            devfn: (port.address.device << 3 | port.address.function).into(),
            config: port.config,
        })
        .collect()
}

/// After a reset, which puts each Resizable BAR back at the size firmware
/// set without checking it, as Linux `pci_restore_rebar_state` does: a BAR
/// that kept a smaller size decodes only that, so the GPU's placement and
/// windows take the size it reads back, with a line. One that reads back
/// larger keeps the size its window was placed for, which it decodes too.
fn resized_bars<C: ConfigSpace>(
    address: stela_drivers::bus::pci::Address,
    function: &Function<C>,
    placement: &mut Placement,
    bars: &mut [core::ops::Range<u64>],
) {
    let resizable = match function.resizable_bars() {
        Ok(resizable) => resizable,
        Err(error) => {
            output::arguments(format_args!(
                "GPU {address}: its Resizable BAR sizes did not read after its reset ({error:?}); \
                 going on with the sizes firmware set\n"
            ));
            return;
        }
    };
    for bar in resizable {
        let Some(region) = placement.regions.get_mut(usize::from(bar.bar)) else {
            continue;
        };
        if region.size == 0 || region.size == bar.bytes {
            continue;
        }
        let mib = |bytes: u64| bytes >> 20;
        if bar.bytes > region.size {
            output::arguments(format_args!(
                "GPU {address}: BAR{} reads back at {} MiB after its reset, where firmware set {} MiB; \
                 keeping {} MiB, the window it was placed in\n",
                bar.bar,
                mib(bar.bytes),
                mib(region.size),
                mib(region.size)
            ));
            continue;
        }
        output::arguments(format_args!(
            "GPU {address}: BAR{} came back at {} MiB after its reset, where firmware set {} MiB; \
             going on with {} MiB, as Linux goes on from the reset\n",
            bar.bar,
            mib(bar.bytes),
            mib(region.size),
            mib(bar.bytes)
        ));
        if let Some(window) = bars.iter_mut().find(|window| window.start == region.start) {
            window.end = window.start + bar.bytes;
        }
        region.size = bar.bytes;
    }
}

/// Read one BAR0 register through a scoped read-only page of `registers`,
/// or `None` when it cannot be mapped or read.
fn cold_refusal(
    address: stela_drivers::bus::pci::Address,
    error: gpu::cold::Error,
) -> ServiceError {
    output::arguments(format_args!("GPU {address} cold preparation: {error:?}\n"));
    ServiceError::Refused("the GPU's original cold handoff was refused")
}

/// An adapter with a driver stack that [`claim_all`] did not claim, for the
/// boot screen and the log.
pub(super) use stela_boot_core::gpu_step::Refused;

/// What [`claim_all`] chose from, kept for a fallback ([`claim_fallback`]):
/// the census, each adapter's kind, the selection as it stands, and the GPUs
/// that could not draw.
pub(super) struct Census {
    adapters: Vec<gpu::Adapter>,
    found: Vec<stela_drivers::gpu::selection::Found>,
    chosen: stela_drivers::gpu::selection::Selection,
    /// The census indices of the GPUs that failed at a stage after their
    /// claim, which a fallback leaves out.
    failed: Vec<usize>,
    source: crate::pci_io::ScreenOwner,
    /// The claims made so far, which number the next one's line.
    claims: u32,
}

impl Census {
    /// The words of Linux's command line the drivers of the GPUs left with
    /// the firmware need, should one be claimed after Linux started.
    pub(super) fn fallback_words(&self) -> Vec<String> {
        use stela_drivers::gpu::selection::Choice;
        let mut words = Vec::new();
        for (index, adapter) in self.adapters.iter().enumerate() {
            if !matches!(self.chosen.choices[index], Choice::Left { .. }) {
                continue;
            }
            let Some(before) = GpuStack::select(adapter).ok().and_then(before::of) else {
                continue;
            };
            words.extend((before.ahead)(adapter));
        }
        words
    }

    /// Whether the census index `index` already failed.
    fn has_failed(&self, index: usize) -> bool {
        self.failed.contains(&index)
    }
}

/// What a claim needs beyond the adapter: the machine's chipset, ports,
/// firmware catalogue, and the port above each function.
pub(super) struct ClaimInputs<'a> {
    pub chipset: &'a [stela_drivers::probe::ChipsetFunction],
    pub ports: &'a stela_drivers::probe::GpuPorts,
    pub catalog: Option<&'a Arc<Catalog>>,
    pub port_above: &'a dyn Fn(stela_drivers::bus::pci::Address) -> Option<link::Port>,
}

/// Claims the adapter at census index `index`, as its `line`-th claim, or
/// says why not and adds it to `refused`.
fn claim_at(
    services: &dyn NativeServices,
    adapters: &[gpu::Adapter],
    index: usize,
    line: u32,
    source: crate::pci_io::ScreenOwner,
    inputs: &ClaimInputs<'_>,
    refused: &mut Vec<Refused>,
) -> Option<Claimed> {
    use stela_boot_core::gpu_status::Reason;
    let adapter = &adapters[index];
    let Ok(stack) = GpuStack::select(adapter) else {
        return None;
    };
    let result = inputs
        .catalog
        .ok_or(ServiceError::Refused("no approved GPU firmware catalogue"))
        .and_then(|catalog| {
            claim(
                services,
                (stack, adapter),
                inputs.chipset,
                line,
                source,
                catalog.clone(),
                (inputs.port_above)(adapter.address),
            )
        });
    match result {
        Ok(mut function) => {
            function.market = adapter.name();
            function.census = index;
            function.placement.upstream = upstream_ports(inputs.ports, adapter.address);
            Some(function)
        }
        Err(error) => {
            output::arguments(format_args!("GPU {} refused: {error:?}\n", adapter.address));
            let reason = match error {
                ServiceError::Refused(before::NO_FIRMWARE) => Reason::MissingFirmware,
                ServiceError::Refused(before::FIRMWARE_READ_FAILED) => Reason::FirmwareReadFailed,
                ServiceError::Refused(before::FIRMWARE_NOT_READ) => Reason::FirmwareNotRead,
                _ => Reason::Other,
            };
            refused.push(Refused::new(
                adapter,
                stack.draws(),
                reason,
                &format!("{error}"),
            ));
            None
        }
    }
}

/// Claim the adapters [`selection::select`] starts, each reported if it
/// cannot be, and the ones refused, with the census a fallback chooses from
/// again. Every display controller found is logged once with its name, kind
/// and what was chosen for it, and why; a GPU that would have drawn and whose
/// claim was refused has a `GPU FALLBACK` line. In a foreground
/// continuation, with probe finished.
///
/// [`selection::select`]: stela_drivers::gpu::selection::select
pub(super) fn claim_all(
    services: &dyn NativeServices,
    adapters: &[gpu::Adapter],
    gop: Result<crate::pci_io::ScreenOwner, String>,
    inputs: &ClaimInputs<'_>,
) -> (Vec<Claimed>, Vec<Refused>, Option<Census>) {
    use stela_boot_core::gpu_status::Reason;
    use stela_drivers::gpu::selection::{self, Choice, Found};
    let refused_as = |adapter: &gpu::Adapter, reason, why: &dyn core::fmt::Display| {
        let draws = GpuStack::select(adapter).is_ok_and(GpuStack::draws);
        Refused::new(adapter, draws, reason, &format!("{why}"))
    };
    let stacks: Vec<_> = adapters.iter().map(GpuStack::select).collect();
    let driven = || {
        adapters
            .iter()
            .zip(&stacks)
            .filter(|(_, stack)| stack.is_ok())
            .map(|(adapter, _)| adapter)
    };
    if !cfg!(feature = "nvk") {
        let mut refused = Vec::new();
        for adapter in driven() {
            let why = "this image has no GPU driver: build it with `cargo xtask boot build`";
            output::arguments(format_args!(
                "GPU {} left with firmware: {why}\n",
                adapter.address
            ));
            refused.push(refused_as(adapter, Reason::Other, &why));
        }
        return (Vec::new(), refused, None);
    }
    let mut claimed: Vec<Claimed> = Vec::new();
    let mut refused = Vec::new();
    let source = match gop {
        Ok(source) => source,
        Err(why) => {
            for adapter in driven() {
                output::arguments(format_args!("GPU {} refused: {why}\n", adapter.address));
                refused.push(refused_as(adapter, Reason::Other, &why));
            }
            return (claimed, refused, None);
        }
    };
    // Each GPU's kind as the memory plan took it, so the selection here and
    // the plan's agree.
    let planned = planned_census();
    let found: Vec<Found> = adapters
        .iter()
        .zip(&stacks)
        .map(|(adapter, stack)| Found {
            stack: stack.map(|stack| {
                selection::driven(
                    stack,
                    adapter,
                    planned.and_then(|planned| planned.kind(adapter.address)),
                )
            }),
            firmware_screen: may_show_screen(source, adapter.address),
        })
        .collect();
    let chosen = selection::select(&found, |index| {
        let line = claimed.len() as u32;
        match claim_at(
            services,
            adapters,
            index,
            line,
            source,
            inputs,
            &mut refused,
        ) {
            Some(function) => {
                claimed.push(function);
                true
            }
            None => false,
        }
    });
    for line in stela_boot_core::gpu_step::selection_lines(adapters, &found, &chosen, &refused) {
        output::arguments(format_args!("GPU SELECT {line}\n"));
    }
    for line in stela_boot_core::gpu_step::refusal_lines(adapters, &found, &chosen, &refused) {
        output::arguments(format_args!("GPU FALLBACK {line}\n"));
    }
    if let Some(planned) = planned {
        let addresses: Vec<_> = adapters.iter().map(|adapter| adapter.address).collect();
        for index in selection::unplanned(planned, &addresses, &chosen) {
            output::arguments(format_args!(
                "GPU MEMORY {} starts, which the memory plan did not see (its port was powered \
                 on after the firmware's exit): the plan, made as {}, stands for this boot{}\n",
                adapters[index].address,
                planned.memory.describe(),
                match planned.memory {
                    selection::Memory::Shared => format!(
                        ", so the integrated GPU's share of {}M is kept from the guest while \
                         this GPU may draw",
                        crate::native_memory::gpu_share() >> 20
                    ),
                    _ => String::new(),
                }
            ));
        }
    }
    for function in &mut claimed {
        function.later = matches!(chosen.choices[function.census], Choice::Later { .. });
        function.beside = beside(&found, adapters, &[], function.census);
    }
    let census = Census {
        adapters: adapters.to_vec(),
        found,
        claims: claimed.len() as u32,
        chosen,
        failed: Vec::new(),
        source,
    };
    (claimed, refused, Some(census))
}

/// After the GPUs of `failed` (census index and the stage each failed at)
/// could not draw, claims the GPUs the selection starts without them
/// ([`stela_drivers::gpu::selection::fall_back`]), which were left with the
/// firmware: one `GPU FALLBACK` line for each failed GPU names its stage, its
/// error and what takes its place. A GPU that already failed or whose claim
/// was refused is not asked again. In a foreground continuation.
pub(super) fn claim_fallback(
    services: &dyn NativeServices,
    census: &mut Census,
    failed: &[(usize, stela_boot_core::gpu_step::Stage)],
    inputs: &ClaimInputs<'_>,
) -> (Vec<Claimed>, Vec<Refused>) {
    let mut refused = Vec::new();
    let claimed = fall_back_census(census, failed, |adapters, source, index, line| {
        claim_at(
            services,
            adapters,
            index,
            line,
            source,
            inputs,
            &mut refused,
        )
    });
    (claimed, refused)
}

/// [`claim_fallback`] with `claim` claiming the adapter at a census index
/// as the claim of a line, or not.
fn fall_back_census(
    census: &mut Census,
    failed: &[(usize, stela_boot_core::gpu_step::Stage)],
    mut claim: impl FnMut(&[gpu::Adapter], crate::pci_io::ScreenOwner, usize, u32) -> Option<Claimed>,
) -> Vec<Claimed> {
    use stela_boot_core::gpu_step::{self, Next};
    use stela_drivers::gpu::selection::{self, Choice};
    let before = census.chosen.clone();
    census.failed.extend(failed.iter().map(|&(index, _)| index));
    let mut claimed: Vec<Claimed> = Vec::new();
    let after = selection::fall_back(&census.found, &before, &census.failed, |index| {
        let line = census.claims + claimed.len() as u32;
        match claim(&census.adapters, census.source, index, line) {
            Some(function) => {
                claimed.push(function);
                true
            }
            None => false,
        }
    });
    let draws = |index: usize| census.found[index].stack.is_ok_and(|driven| driven.draws);
    let drawing = after.claimed.iter().copied().find(|&index| {
        after.choices[index] == Choice::First && draws(index) && !census.has_failed(index)
    });
    let next = Next::of(&before, &after, drawing);
    for (index, stage) in failed {
        output::arguments(format_args!(
            "GPU FALLBACK {}\n",
            gpu_step::fallback_line(&census.adapters, *index, stage, &next)
        ));
    }
    for function in &mut claimed {
        function.later = matches!(after.choices[function.census], Choice::Later { .. });
        function.beside = beside(
            &census.found,
            &census.adapters,
            &census.failed,
            function.census,
        );
    }
    census.claims += claimed.len() as u32;
    census.chosen = after;
    claimed
}

/// The name of a GPU of the census, other than the one at `index` and those
/// of `failed`, whose stack draws: one that could draw in its place.
fn beside(
    found: &[stela_drivers::gpu::selection::Found],
    adapters: &[gpu::Adapter],
    failed: &[usize],
    index: usize,
) -> Option<String> {
    (0..found.len())
        .find(|&other| {
            other != index
                && !failed.contains(&other)
                && found[other].stack.is_ok_and(|driven| driven.draws)
        })
        .map(|other| adapters[other].name())
}

/// The probe running now, with the processor it runs on, which a stop
/// raised inside it names ([`probe_stop`]).
struct Probing {
    gpu: String,
    address: String,
    driver: &'static str,
    turn: stela_boot_core::gpu_status::ProbeTurn,
    beside: Option<String>,
    processor: usize,
}

static PROBING: stela_boot_core::screen::Exclusive<Option<Probing>> =
    stela_boot_core::screen::Exclusive::new(None);

/// What a stop raised now adds to its reason: the GPU whose driver's probe
/// runs on this processor, the turn, and the GPU that could have drawn in
/// its place (`gpu_status::probe_stop`). None outside a probe.
pub(in crate::engine) fn probe_stop() -> Option<String> {
    PROBING
        .try_with(|probing| {
            let probing = probing.as_ref()?;
            stela_boot_core::gpu_status::probe_stop(
                Some(&stela_boot_core::gpu_status::Probing {
                    gpu: &probing.gpu,
                    address: &probing.address,
                    driver: probing.driver,
                    turn: probing.turn,
                    beside: probing.beside.as_deref(),
                    processor: probing.processor,
                }),
                this_processor() as usize,
            )
        })
        .flatten()
}

/// Whether the firmware's picture may be on the function at `address`, as
/// [`claim`] decides it, without its reads: Stela shows one display, which a
/// drawing GPU's outputs take first, so a GPU that only shows another's
/// frames is started only where the panel or monitor the firmware lit may
/// hang on it.
fn may_show_screen(
    gop: crate::pci_io::ScreenOwner,
    address: stela_drivers::bus::pci::Address,
) -> bool {
    use crate::pci_io::ScreenOwner;
    match gop {
        ScreenOwner::Function(source) => source.address == address,
        ScreenOwner::Unknown(apart) => {
            stela_boot_core::firmware_screen::gpu_may_show(&apart, address.segment, address.bus)
        }
        ScreenOwner::NoScreen => false,
    }
}

/// A GPU its stack's kernel driver bound.
#[cfg(feature = "nvk")]
struct Bound {
    name: String,
    /// Its market name, or its maker alone.
    market: String,
    /// Its index in the census the selection chose it from.
    census: usize,
    /// Whether it may be the GPU that showed the firmware's screen, when the
    /// screen's function is unknown.
    shows_screen: bool,
    stack: &'static GpuStack,
    /// As [`Claimed::integrated`].
    integrated: Option<bool>,
    drm: client::DrmDevice,
    link: Option<PcieLink>,
    /// The PCI Express port above it, read when it stops answering.
    port: Option<link::Port>,
    /// Its `struct pci_dev`, which its attachment keeps for the boot: its
    /// driver is stopped through it at the boot's stop, and unloaded where
    /// its Metal device did not open and its display is unused.
    device: usize,
}

/// The most GPUs one boot binds.
#[cfg(feature = "nvk")]
const MOST_BOUND: usize = 16;

/// The bound GPUs, in the order their drivers bound them: the start's, then
/// any a fallback started after a GPU could not draw ([`start_more`]). They
/// index the DRM minors, the Metal services and the display. Only the boot
/// CPU's start adds one, and only at the end; a reader on any processor
/// sees the GPUs added before it looked, each whole.
#[cfg(feature = "nvk")]
struct Bounds {
    items: *mut core::mem::MaybeUninit<Bound>,
    len: AtomicUsize,
}

#[cfg(feature = "nvk")]
impl Bounds {
    fn new() -> Self {
        Self {
            items: Box::leak(Box::<[Bound]>::new_uninit_slice(MOST_BOUND)).as_mut_ptr(),
            len: AtomicUsize::new(0),
        }
    }

    /// Adds a GPU its driver bound after the others, or says why it is not
    /// kept: past [`MOST_BOUND`].
    fn keep(&self, bound: Bound) {
        let len = self.len.load(Ordering::Relaxed);
        if len == MOST_BOUND {
            output::arguments(format_args!(
                "GPU {}: bound past the {MOST_BOUND} GPUs a boot keeps, so it is not used\n",
                bound.name
            ));
            // Its driver holds it, so nothing of it is released.
            core::mem::forget(bound);
            return;
        }
        // SAFETY: the slot at `len` is in the leaked allocation and past
        // every slice a reader holds; only the boot CPU's start adds.
        unsafe { (*self.items.add(len)).write(bound) };
        self.len.store(len + 1, Ordering::Release);
    }

    /// Moves the GPUs of `from`, a later turn's, after these.
    fn append(&self, from: &mut Self) {
        let count = core::mem::replace(from.len.get_mut(), 0);
        for index in 0..count {
            // SAFETY: the first `count` slots of `from` were written, and its
            // length is zero now, so each is read once.
            self.keep(unsafe { (*from.items.add(index)).assume_init_read() });
        }
    }
}

#[cfg(feature = "nvk")]
impl core::ops::Deref for Bounds {
    type Target = [Bound];
    fn deref(&self) -> &[Bound] {
        let len = self.len.load(Ordering::Acquire);
        // SAFETY: the first `len` slots were written before `len` was
        // published, and none is changed or moved after.
        unsafe { core::slice::from_raw_parts(self.items.cast::<Bound>(), len) }
    }
}

/// The original managed child runs the driver's initcalls and probe.
struct Start {
    kernel: &'static LinuxKernel,
    claimed: ManuallyDrop<Vec<Claimed>>,
    /// What this turn runs before its probes.
    turn: Option<ProbeTurn>,
    completed: Rc<Cell<Option<ManuallyDrop<Started>>>>,
}

/// What a turn of probes runs first.
enum ProbeTurn {
    /// Linux's start, before the first turn's probes.
    First(Setup),
    /// Nothing: the GPUs that only show another's frames, after the first.
    Later,
    /// For GPUs claimed after the start, in place of GPUs that could not
    /// draw: the functions their drivers search for that are not published
    /// yet, and the graphics memory the firmware reserved for one of them.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    Fallback {
        searched: Vec<before::Searched>,
        stolen: Option<(u64, u64)>,
        /// The pages Linux is held to before these probes, where the plan
        /// held it to none and an integrated GPU draws now
        /// (`host_memory::fallback_pages`).
        pages: Option<usize>,
    },
}

/// Linux's page catalog, which `TaskRuntime::install` holds for the boot: a
/// fallback holds Linux's pages to a bound with it ([`hold_pages`]).
#[cfg(feature = "nvk")]
static LINUX_PAGES: AtomicPtr<stela_linux::pages::Catalog<KernelPages>> =
    AtomicPtr::new(ptr::null_mut());

/// Holds Linux's pages to `pages` from now on, as the loader's plan would
/// have ([`stela_linux::pages::Catalog::limit_current`]): the RAM Linux
/// reports is then `pages`, from which the drivers probed after size their
/// memory, and a request past it fails as a full GPU memory does. Said in a
/// line either way.
///
/// # Safety
/// Entered, on the canonical kernel task, quiescent.
#[cfg(feature = "nvk")]
unsafe fn hold_pages(kernel: &LinuxKernel, pages: usize) {
    // SAFETY: stored once from the leaked catalog `TaskRuntime` holds.
    let Some(catalog) = (unsafe { LINUX_PAGES.load(Ordering::Acquire).as_ref() }) else {
        output::arguments(format_args!(
            "GPU MEMORY Linux's pages stay unbounded: its page catalog is not kept\n"
        ));
        return;
    };
    // SAFETY: the caller's contract, with the catalog's own owner.
    match unsafe { catalog.limit_current(kernel, pages) } {
        Ok(()) => {
            crate::native_memory::pages_held();
            output::arguments(format_args!(
                "GPU MEMORY Linux's pages are held to {}M from now on\n",
                (pages as u64 * PAGE) >> 20
            ));
        }
        Err(error) => output::arguments(format_args!(
            "GPU MEMORY Linux's pages could not be held to {}M ({error:?}): they stay \
             unbounded, and the integrated GPU's Metal device alone budgets its allocations \
             to it\n",
            (pages as u64 * PAGE) >> 20
        )),
    }
}

/// What Linux needs of every claimed GPU before the first probe: the words
/// of its command line each GPU's driver needs set before it registers, and
/// the functions a probe searches for, each once.
struct Setup {
    command_line: String,
    /// Words no parameter of the image takes, which Linux would refuse and
    /// fail its whole start on: each is named once and left out.
    left_out: Vec<String>,
    searched: Vec<before::Searched>,
    /// The graphics memory the firmware reserved for an integrated GPU.
    stolen: Option<(u64, u64)>,
}

impl Setup {
    /// The setup of `claimed`, with `fallback`, the words the drivers of
    /// GPUs left with the firmware need should one start after a failed
    /// start, when Linux's command line is set already.
    fn of(claimed: &[Claimed], fallback: &[String]) -> Self {
        // A part may need several words (the Intel part one set for each
        // driver that may bind the GPU); each is checked on its own.
        let mut words: Vec<String> = Vec::new();
        let given = claimed
            .iter()
            .filter_map(|claimed| claimed.parameters.as_deref())
            .chain(fallback.iter().map(String::as_str))
            .flat_map(str::split_whitespace);
        for word in given {
            if !words.iter().any(|seen| seen == word) {
                words.push(String::from(word));
            }
        }
        words.extend(gpu_page_parameter(
            crate::native_memory::gpu_share() / 4096,
            crate::native_memory::gpu_page_limit(),
        ));
        let (command_line, left_out) = gpu::command_line::compose(words);
        Self {
            command_line,
            left_out,
            searched: searched_of(claimed),
            stolen: claimed.iter().find_map(|claimed| claimed.stolen),
        }
    }
}

/// The functions the drivers of `claimed` search for, each once.
fn searched_of(claimed: &[Claimed]) -> Vec<before::Searched> {
    let mut searched: Vec<before::Searched> = Vec::new();
    for function in claimed.iter().flat_map(|claimed| &claimed.searched) {
        let at = |searched: &before::Searched| (searched.identity.bus, searched.identity.devfn);
        match searched.iter_mut().find(|seen| at(seen) == at(function)) {
            // One driver reads the configuration another only searches.
            Some(seen) => seen.configuration = seen.configuration.or(function.configuration),
            None => searched.push(function.clone()),
        }
    }
    searched
}

/// TTM's limit for the pages Linux may hold (`Handoff::gpu_pages`, `pages`
/// pages): for an integrated GPU's share (`share` pages, nonzero) twice it,
/// which lets its driver place the whole share in system memory and keeps
/// TTM from swapping to shmem ([`gpu::command_line::page_limit`]); else the
/// bound itself, the page catalog's limit, so a discrete GPU's GTT and its
/// evictions from VRAM are the pages the heap can give
/// ([`gpu::command_line::bound`]). Nothing where the loader set no bound.
fn gpu_page_parameter(share: u64, pages: Option<usize>) -> Option<String> {
    let pages = pages?;
    Some(match usize::try_from(share) {
        Ok(share) if share != 0 => gpu::command_line::page_limit(share),
        _ => gpu::command_line::bound(pages),
    })
}

/// A claimed GPU no driver bound.
struct Unbound {
    name: String,
    /// Its market name, or its maker alone.
    market: String,
    /// Its index in the census the selection chose it from.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    census: usize,
    driver: &'static str,
    /// That its drivers did not bind it, in the start's words.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    what: String,
    /// The first error Linux reported in its probe.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    error: Option<String>,
    /// What became of the firmware's picture on it.
    picture: stela_boot_core::gpu_status::Picture<'static>,
    /// Its stack draws.
    draws: bool,
    /// The boot screen said it did not start ([`warn_one`]).
    warned: Cell<bool>,
}

impl Unbound {
    /// Why it did not start, for the boot screen: Linux's first error in
    /// its probe, else what the start says of it.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    fn cause(&self) -> String {
        match &self.error {
            Some(error) => error.clone(),
            None => format!("{}; Linux reported no error", self.what),
        }
    }

    /// A GPU whose probe never returned: a picture on it stays held.
    fn of(claimed: &Claimed) -> Self {
        use stela_boot_core::gpu_status::Picture;
        Self {
            name: claimed.name.clone(),
            market: claimed.market.clone(),
            census: claimed.census,
            driver: claimed.stack.kernel.driver,
            what: format!(
                "{}'s probe did not finish in {START_SECONDS} s",
                claimed.stack.kernel.driver
            ),
            error: None,
            picture: match (claimed.shows_screen, claimed.screen.retired) {
                (false, _) => Picture::Elsewhere,
                (true, false) => Picture::ResetForClaim,
                (true, true) => Picture::Held,
            },
            draws: claimed.stack.draws(),
            warned: Cell::new(false),
        }
    }
}

struct Started {
    _attached: Vec<Attached<Device>>,
    _searched: Vec<stela_linux::function::Published>,
    /// The functions published for searches, by bus and devfn, which a
    /// later turn does not publish again.
    searched_at: RefCell<Vec<(u32, u32)>>,
    /// The claimed GPUs no driver bound, the start's and a fallback's.
    unbound: RefCell<Vec<Unbound>>,
    /// Linux's command line, which it read at its start.
    command_line: String,
    #[cfg(feature = "nvk")]
    bound: Bounds,
    result: Result<(), gpu::stack::StartError>,
}

// SAFETY: the original managed runtime enters the canonical owner on the
// child before run. The job retains the actual devices until it publishes
// the result, whose attachments remain owned for the boot.
unsafe impl ManagedJob for Start {
    fn run(mut self: Pin<&mut Self>) {
        let claimed = core::mem::take(&mut *self.claimed);
        let turn = self.turn.take().unwrap_or(ProbeTurn::Later);
        self.completed
            .set(Some(ManuallyDrop::new(start(self.kernel, claimed, turn))));
    }
}

/// Publishes `searched` for the drivers' searches, those `published`
/// already holds left out, into `job`.
fn publish_searched(job: &mut Started, searched: Vec<before::Searched>) {
    for before::Searched {
        identity,
        configuration,
    } in searched
    {
        let at = (identity.bus, identity.devfn);
        if job.searched_at.borrow().contains(&at) {
            continue;
        }
        // SAFETY: entered, on the canonical kernel task; the publication
        // is kept for the boot.
        let published = unsafe {
            match configuration {
                Some(bytes) => stela_linux::function::Published::snapshot(identity, &bytes),
                None => stela_linux::function::Published::new(identity),
            }
        };
        match published {
            Ok(published) => {
                job._searched.push(published);
                job.searched_at.borrow_mut().push(at);
            }
            Err(error) => output::arguments(format_args!(
                "GPU: PCI function {:02x}:{:02x}.{} could not be published for searches: \
                 Linux error {error}\n",
                identity.bus,
                identity.devfn >> 3,
                identity.devfn & 7
            )),
        }
    }
}

fn start(kernel: &LinuxKernel, claimed: Vec<Claimed>, turn: ProbeTurn) -> Started {
    #[cfg(feature = "nvk")]
    stela_boot_core::heap::set_peak_hook(gate::peak_passed);
    let mut job = Started {
        _attached: Vec::new(),
        _searched: Vec::new(),
        searched_at: RefCell::new(Vec::new()),
        unbound: RefCell::new(Vec::new()),
        command_line: String::new(),
        #[cfg(feature = "nvk")]
        bound: Bounds::new(),
        result: Ok(()),
    };
    let probe_turn = match turn {
        ProbeTurn::First(_) => stela_boot_core::gpu_status::ProbeTurn::First,
        ProbeTurn::Later => stela_boot_core::gpu_status::ProbeTurn::Later,
        ProbeTurn::Fallback { .. } => stela_boot_core::gpu_status::ProbeTurn::Fallback,
    };
    match turn {
        ProbeTurn::First(setup) => {
            for word in &setup.left_out {
                output::arguments(format_args!(
                    "GPU: no driver in this image declares the parameter \"{word}\" sets; it is \
                     left off Linux's command line\n"
                ));
            }
            if !setup.command_line.is_empty() {
                output::arguments(format_args!(
                    "GPU: Linux starts with \"{}\"\n",
                    setup.command_line
                ));
            }
            // SAFETY: entered, on a task.
            job.result = unsafe { gpu::stack::start_kernel(&setup.command_line) };
            job.command_line = setup.command_line;
            if job.result.is_err() {
                // No driver probed them, so a picture they hold stays.
                let mut claimed = claimed;
                for claimed in &mut claimed {
                    claimed.screen.let_go();
                }
                // No source-specific device return was acknowledged.
                core::mem::forget(claimed);
                return job;
            }
            // What the drivers search for, before any driver's probe: a probe
            // looks for them (`intel_pch.c` for the chipset).
            if let Some((base, size)) = setup.stolen {
                // SAFETY: entered, on the canonical kernel task, before any
                // driver registers.
                unsafe { stela_linux::function::set_graphics_stolen(base, size) };
            }
            publish_searched(&mut job, setup.searched);
        }
        ProbeTurn::Later => {}
        ProbeTurn::Fallback {
            searched,
            stolen,
            pages,
        } => {
            #[cfg(feature = "nvk")]
            if let Some(pages) = pages {
                // SAFETY: entered, on the canonical kernel task, before the
                // probes whose drivers size their memory from Linux's RAM.
                unsafe { hold_pages(kernel, pages) };
            }
            #[cfg(not(feature = "nvk"))]
            let _ = pages;
            if let Some((base, size)) = stolen {
                // SAFETY: entered, on the canonical kernel task; no probe of
                // the GPU it was reserved for has run, and no other GPU's
                // driver reads it (the start set none).
                unsafe { stela_linux::function::set_graphics_stolen(base, size) };
            }
            publish_searched(&mut job, searched);
        }
    }
    for mut claimed in claimed {
        let authority = {
            let mut function = claimed.device.config.0.borrow_mut();
            // SAFETY: the firmware keys came from the real chipset read on the
            // exact register BAR and were rechecked after its consumed reset.
            // Device and catalogue share the original native attachment.
            unsafe {
                PciAuthority::new(
                    kernel,
                    claimed.device.custody.clone(),
                    function.config(),
                    claimed.firmware,
                )
            }
        };
        let authority = match authority {
            Ok(authority) => authority,
            Err(error) => {
                output::arguments(format_args!(
                    "GPU {} catalogue refused: {error}\n",
                    claimed.name
                ));
                core::mem::forget(claimed.device);
                continue;
            }
        };
        // The first error Linux reports from here on is this probe's.
        kernel.platform().probe_error.take();
        // A stop inside the probe names it.
        PROBING.try_with(|probing| {
            *probing = Some(Probing {
                gpu: claimed.market.clone(),
                address: claimed.name.clone(),
                driver: claimed.stack.kernel.driver,
                turn: probe_turn,
                beside: claimed.beside.clone(),
                processor: this_processor() as usize,
            })
        });
        // SAFETY: entered on the same canonical kernel task. The device and
        // enrolled catalogue retain the exact original configuration/attachment.
        let attached = unsafe {
            Attached::new_catalogued(kernel, claimed.device, claimed.placement, authority)
        };
        PROBING.try_with(|probing| *probing = None);
        match attached {
            Ok(attached) => {
                // SAFETY: entered.
                if unsafe { attached.bound() } {
                    // SAFETY: entered.
                    let driver = unsafe { attached.driver() }.and_then(|name| name.to_str().ok());
                    let stack = bound_stack(claimed.stack, driver);
                    if core::ptr::eq(stack, claimed.stack) {
                        output::arguments(format_args!(
                            "GPU {}: {} bound it\n",
                            claimed.name, stack.kernel.driver
                        ));
                    } else {
                        output::arguments(format_args!(
                            "GPU {}: {} did not bind it, so {} did, the next registered \
                             driver whose table holds it; Linux's lines above say why\n",
                            claimed.name, claimed.stack.kernel.driver, stack.kernel.driver
                        ));
                    }
                    // Its driver drives what the screen on it shows.
                    claimed.screen.end(None);
                    #[cfg(feature = "nvk")]
                    job.bound.keep(Bound {
                        name: claimed.name,
                        market: claimed.market,
                        census: claimed.census,
                        shows_screen: claimed.shows_screen,
                        stack,
                        integrated: claimed.integrated,
                        drm: claimed.drm,
                        link: claimed.link,
                        port: claimed.port,
                        device: attached.device() as usize,
                    });
                } else {
                    use stela_boot_core::gpu_status::Picture;
                    let picture = claimed.screen.after_failed_probe(claimed.shows_screen);
                    let what = not_bound(&claimed.offered);
                    output::arguments(format_args!(
                        "GPU {}: {what}; Linux's lines above say why.{}\n",
                        claimed.name,
                        match picture {
                            Picture::Elsewhere | Picture::Held => "",
                            Picture::Kept =>
                                " The firmware's picture was on this GPU and stays, as the \
                                 driver stopped before it reset the GPU or started its display",
                            Picture::ResetForClaim =>
                                " The firmware's picture was on this GPU and ended when it was \
                                 reset for the claim, so a display wired to it stays dark",
                            Picture::EndedBy(_) =>
                                " The firmware's picture was on this GPU and its driver ended \
                                 it, so a display wired to it stays dark",
                        }
                    ));
                    job.unbound.get_mut().push(Unbound {
                        name: claimed.name,
                        market: claimed.market,
                        census: claimed.census,
                        driver: claimed.stack.kernel.driver,
                        what,
                        error: kernel.platform().probe_error.take(),
                        picture,
                        draws: claimed.stack.draws(),
                        warned: Cell::new(false),
                    });
                }
                job._attached.push(attached);
            }
            Err(error) => {
                output::arguments(format_args!(
                    "GPU {} presentation refused: {:?}\n",
                    claimed.name, error.error
                ));
                let picture = claimed.screen.after_failed_probe(claimed.shows_screen);
                job.unbound.get_mut().push(Unbound {
                    name: claimed.name,
                    market: claimed.market,
                    census: claimed.census,
                    driver: claimed.stack.kernel.driver,
                    what: String::from("Linux was not shown it, so no driver was offered it"),
                    error: Some(format!("{:?}", error.error)),
                    picture,
                    draws: claimed.stack.draws(),
                    warned: Cell::new(false),
                });
            }
        }
    }
    job
}

/// The kernel drivers Linux offers `adapter` to, `stack`'s first, then the
/// later stacks' of its vendor whose drivers hold it.
fn offered(stack: &'static GpuStack, adapter: &gpu::Adapter) -> Vec<&'static str> {
    let mut names = alloc::vec![stack.kernel.driver];
    names.extend(
        gpu::stack::STACKS
            .iter()
            .skip_while(|other| !core::ptr::eq(**other, stack))
            .skip(1)
            .filter(|other| other.vendor == stack.vendor && (other.kernel.admits)(adapter).is_ok())
            .map(|other| other.kernel.driver),
    );
    names
}

/// That none of the drivers `offered` bound a GPU, as a line says it.
fn not_bound(offered: &[&str]) -> String {
    match offered {
        [] => String::from("no driver bound it"),
        [driver] => format!("{driver} did not bind it"),
        [first, rest @ ..] => format!("neither {first} nor {} bound it", rest.join(" nor ")),
    }
}

/// The stack of the driver Linux bound a claimed GPU to, named `driver`:
/// the claimed stack's, or the one of the same vendor whose driver bound it
/// after the claimed driver's probe failed (xe after i915). A name no stack
/// of the vendor has keeps the claimed stack.
fn bound_stack(claimed: &'static GpuStack, driver: Option<&str>) -> &'static GpuStack {
    driver
        .filter(|driver| *driver != claimed.kernel.driver)
        .and_then(|driver| GpuStack::bound(claimed.vendor, driver))
        .unwrap_or(claimed)
}

/// The running environment.
#[cfg_attr(
    not(feature = "nvk"),
    allow(
        dead_code,
        reason = "held for the boot; only Mesa's client reads these"
    )
)]
struct Installed {
    work: &'static KernelWork,
    time: Arc<dyn Clock>,
    events: Arc<Events>,
    domain: Arc<Domain<SLOTS>>,
    /// When the service is next due, as a TSC value: the clock reads only
    /// in the Linux environment's root, and the boot loop asks outside it.
    next: AtomicU64,
    #[cfg(feature = "nvk")]
    window: &'static Window,
    tsc_hz: u64,
    #[cfg(feature = "nvk")]
    libc_clock: client::LibcClock,
    #[cfg(feature = "nvk")]
    heap_bytes: u64,
    /// Clients' mappings, by address, with the faults running in each. For
    /// tasks; no borrow lasts across a call into Linux, which may sleep.
    #[cfg(feature = "nvk")]
    mappings: RefCell<stela_boot_core::client_fault::Mappings<stela_linux::client::Mapping>>,
    /// Mesa's running threads, by the bottom of their stacks. Only Linux
    /// entries, under the host drivers' lock, read and change it.
    #[cfg(feature = "nvk")]
    mesa_threads: RefCell<BTreeMap<u64, client::Running>>,
    #[cfg(feature = "nvk")]
    service: client::Service,
    owner: &'static Arc<LinuxKernel>,
    threads: &'static LinuxThreads,
    start: ManuallyDrop<Started>,
}

impl Installed {
    fn advance_children(&self) {
        // SAFETY: callers are on the same dispatcher after physical return,
        // outside native/source stacks and metadata loans.
        if let Err(error) = unsafe { self.threads.dispatcher_step() } {
            output::fatal(format_args!(
                "Linux original task retirement failed: {error}"
            ));
        }
    }
}

static INSTALLED: AtomicPtr<Installed> = AtomicPtr::new(ptr::null_mut());

/// Whether a bound GPU's kernel driver watches for its own resets
/// ([`GpuStack`]'s `kernel.reset`, amdgpu's): a reset it makes holds back
/// every later close of its files, so a step that waits on one may never
/// return.
#[cfg(feature = "nvk")]
pub(super) fn bound_gpu_may_hang() -> bool {
    installed().is_some_and(|gpu| {
        gpu.start
            .bound
            .iter()
            .any(|bound| bound.stack.kernel.reset.is_some())
    })
}

fn installed() -> Option<&'static Installed> {
    // SAFETY: a published environment is leaked, so it lives for the boot.
    unsafe { INSTALLED.load(Ordering::Acquire).as_ref() }
}

/// What [`open`] needs beyond the claimed functions.
pub(super) struct Machine {
    pub work: &'static KernelWork,
    pub window: &'static Window,
    pub random: Seeded,
    pub tsc_hz: u64,
    pub wall: Option<RtcReading>,
    /// The words of Linux's command line the drivers of GPUs left with the
    /// firmware need, should one start after a failed start ([`Census`]).
    pub fallback_words: Vec<String>,
    /// The host heap capacity reported to the linked graphics runtime.
    #[cfg(feature = "nvk")]
    pub heap_bytes: u64,
}

fn environment_error(error: WorkError) -> kernel_work::Error {
    kernel_work::Error::Work(error)
}

/// The GPUs that started and whether each draws, and whether macOS has a
/// Metal device on them, for the boot's identity line (`ident`).
pub(super) fn ident() -> String {
    #[cfg(feature = "nvk")]
    {
        let bound = installed().map_or(&[][..], |gpu| &gpu.start.bound[..]);
        if bound.is_empty() {
            return String::from("none started");
        }
        let mut text = String::new();
        for gpu in bound {
            if !text.is_empty() {
                text.push_str(", ");
            }
            text.push_str(&gpu.name);
            text.push_str(match gpu.stack.draws() {
                true => " draws",
                false => " shows only",
            });
        }
        text.push_str(match gate::shared().is_empty() {
            true => "; no Metal device open",
            false => "; Metal device open",
        });
        text
    }
    #[cfg(not(feature = "nvk"))]
    String::from("no GPU driver in this build")
}

/// The boot screen's GPU step once the GPUs bound, its end first
/// ([`stela_boot_core::gpu_step::started`]): the GPU that is to draw macOS
/// ([`gate::metal_choice`]), the drawing GPUs among `refused` and those no
/// driver bound in whose place it draws, and a line for each other bound
/// GPU a screen may hang on.
pub(super) fn started(
    #[cfg_attr(not(feature = "nvk"), allow(unused_variables))] refused: &[Refused],
) -> Vec<(stela_boot_core::gpu_step::Mark, String)> {
    use stela_boot_core::gpu_step;
    #[cfg(feature = "nvk")]
    if let Some(gpu) = installed() {
        let bound: Vec<gpu_step::Bound> = gpu
            .start
            .bound
            .iter()
            .enumerate()
            .map(|(index, bound)| gpu_step::Bound {
                market: &bound.market,
                draws: bound.stack.draws(),
                integrated: gate::integrated(index, bound),
                shows_screen: bound.shows_screen,
            })
            .collect();
        use stela_boot_core::gpu_status::Picture;
        let unbound = gpu.start.unbound.borrow();
        let drawing: Vec<&str> = unbound
            .iter()
            .filter(|gpu| gpu.draws)
            .map(|gpu| gpu.market.as_str())
            .collect();
        // A screen wired to a GPU that did not start, whose picture ended.
        let dark = unbound
            .iter()
            .find(|gpu| {
                matches!(
                    gpu.picture,
                    Picture::ResetForClaim | Picture::EndedBy(_) | Picture::Held
                )
            })
            .map(|gpu| gpu.market.as_str());
        return gpu_step::started(&bound, &drawing, refused, dark);
    }
    alloc::vec![(gpu_step::Mark::Ok, String::from("started"))]
}

/// The name of the GPU that draws macOS: the one the first Metal service
/// runs on, else the one chosen for it, else the first GPU that started.
/// For the boot log's GPU summary and the screen's lines.
#[cfg(feature = "nvk")]
pub(super) fn drawing_name() -> Option<&'static str> {
    let bound = &installed()?.start.bound;
    let index = gate::served_gpu(0)
        .or_else(|| {
            gate::metal_choice(bound, display::taken_index())
                .serve
                .first()
                .copied()
        })
        .unwrap_or(0);
    market_name(index)
}

/// Without GPU drivers no GPU draws.
#[cfg(not(feature = "nvk"))]
pub(super) fn drawing_name() -> Option<&'static str> {
    None
}

/// The market name of the bound GPU at `index` (the claims' order, which
/// DRM minors and [`gate::served_gpu`] use), or its maker alone where the
/// table has no name.
#[cfg(feature = "nvk")]
pub(super) fn market_name(index: usize) -> Option<&'static str> {
    installed()?
        .start
        .bound
        .get(index)
        .map(|gpu| gpu.market.as_str())
}

/// Why this boot gives macOS no Metal device, if it does not. WindowServer
/// aborts without one and watchdogd
/// then panics the system, so a boot of macOS stops here instead.
pub(super) fn no_metal_device() -> Option<&'static str> {
    #[cfg(feature = "nvk")]
    return gate::shared().is_empty().then(|| {
        let bound = installed().map_or(&[][..], |gpu| &gpu.start.bound[..]);
        match (bound.is_empty(), bound.iter().any(|gpu| gpu.stack.draws())) {
            (true, _) => "no GPU started; the lines above say why",
            // Its stack has a kernel driver alone in this image.
            (false, false) => {
                "the GPU that started only shows a picture: this image has no Vulkan driver \
                 for it, and macOS needs a GPU that draws"
            }
            (false, true) => {
                "no Metal device opened on the GPUs that started; the lines above say why"
            }
        }
    });
    #[cfg(not(feature = "nvk"))]
    Some("this stela.efi was built without GPU drivers (the `nvk` feature)")
}

/// The kexts a boot that gives the GPUs to nouveau links into its system.
pub(super) fn kexts() -> Vec<stela_loader::aux_kc::Kext<'static>> {
    #[cfg(feature = "nvk")]
    return alloc::vec![stela_loader::aux_kc::Kext {
        info: kext::INFO,
        objects: kext::OBJECTS.to_vec(),
        bundle_path: "/Library/Extensions/StelaGPU.kext".into(),
        trusted_code: kext::TRUSTED_CODE.to_vec(),
    }];
    #[cfg(not(feature = "nvk"))]
    Vec::new()
}

/// Takes the display for the GPUs [`open`] bound, in the monitor's own mode
/// or else the firmware's `screen` size, and opens their Metal services. `firmware` names the PCI
/// function that showed the firmware's picture, `None` when there was a
/// picture whose function is unknown.
///
/// Where no Metal device opened on the GPUs that draw, `fallback` claims and
/// starts the GPUs that draw in their place, and their services open.
pub(super) fn serve(
    #[cfg_attr(not(feature = "nvk"), allow(unused_variables))] screen: Option<(usize, usize)>,
    #[cfg_attr(not(feature = "nvk"), allow(unused_variables))] firmware: Option<&str>,
    #[cfg_attr(not(feature = "nvk"), allow(unused_variables))] size: Option<(u32, u32)>,
    #[cfg_attr(not(feature = "nvk"), allow(unused_variables))] fallback: &mut Fallback,
) {
    #[cfg(feature = "nvk")]
    {
        let shown = match (screen, firmware) {
            (None, _) => display::Shown::Nothing,
            (Some(_), None) => display::Shown::Unknown,
            (Some(_), Some(function)) => display::Shown::By(function),
        };
        let wanted = screen.unwrap_or((0, 0));
        display::take(wanted, shown, size);
        let failed = gate::open_service();
        // A stop in a fallback's start stores the USB's log, as one in the
        // start does.
        let errors = super::storage::in_gpu_start(|| {
            fall_back_after_metal(failed, fallback, |from| {
                display::take_from(from, wanted, shown, size);
            })
        });
        report_drawer(&errors);
        super::storage::in_gpu_start(|| release_unshown(&errors));
    }
}

/// After the drawing GPUs of `failed` (bound index, why) opened no Metal
/// device: where another GPU's opened, a `GPU FALLBACK` line for each says
/// so; where none did, the GPUs that draw in their place are claimed and
/// started ([`Fallback`]), the display taken from them where none was
/// (`take_from`, given the first new bound index), and their services
/// opened, until one opens or no GPU is left.
#[cfg(feature = "nvk")]
fn fall_back_after_metal(
    mut failed: Vec<(usize, String)>,
    fallback: &mut Fallback,
    take_from: impl Fn(usize),
) -> Vec<(usize, String)> {
    use stela_boot_core::gpu_step::Stage;
    let mut every = failed.clone();
    let Some(gpu) = installed() else {
        return every;
    };
    let stage = |index: usize, error: &str| {
        let bound = &gpu.start.bound[index];
        (
            bound.census,
            Stage::Metal {
                driver: String::from(bound.stack.kernel.driver),
                error: String::from(error),
            },
        )
    };
    // Each round claims GPUs that have not failed yet, so the census ends it.
    for _ in 0..MOST_BOUND {
        let stages: Vec<_> = failed
            .iter()
            .map(|(index, error)| stage(*index, error))
            .filter(|(census, _)| !fallback.failed(*census))
            .collect();
        if stages.is_empty() {
            break;
        }
        if let Some(served) = gate::served_gpu(0) {
            if let Some(pages) = fallback.note(&stages, gpu.start.bound[served].census) {
                hold_pages_now(gpu, pages);
            }
            break;
        }
        let from = gpu.start.bound.len();
        let round = fallback.claim(stages, &mut |_| {});
        if round.claimed.is_empty() {
            if let Some(pages) = round.pages {
                hold_pages_now(gpu, pages);
            }
            break;
        }
        if let Err(error) = start_more(gpu, round.claimed, round.pages) {
            output::arguments(format_args!(
                "GPU FALLBACK: the GPUs claimed in place of the ones that could not draw did \
                 not start: {error}\n"
            ));
            break;
        }
        // A GPU claimed here whose probe failed gives way in turn.
        if fall_back_after_probe(Ok(()), fallback).is_err() || gpu.start.bound.len() == from {
            break;
        }
        client::find_new_minors();
        take_from(from);
        failed = gate::open_from(from);
        every.extend(failed.iter().cloned());
    }
    every
}

/// Holds Linux's pages to `pages` now, in a turn of Linux's of its own,
/// where an integrated GPU started already draws in place of one that could
/// not ([`hold_pages`]).
#[cfg(feature = "nvk")]
fn hold_pages_now(gpu: &'static Installed, pages: usize) {
    if let Err(error) = start_more(gpu, Vec::new(), Some(pages)) {
        output::arguments(format_args!(
            "GPU MEMORY Linux's pages could not be held to a bound: {error}\n"
        ));
    }
}

/// The seconds a GPU's driver has to unload ([`release_unshown`]): nouveau
/// bounds its GSP's unload wait in 4 s.
#[cfg(feature = "nvk")]
const RELEASE_SECONDS: u64 = 10;

#[cfg(feature = "nvk")]
static RELEASE: &str = "the unload of a GPU that cannot draw and shows no display";

/// Unloads each GPU of `failed` (bound index, why its Metal device did not
/// open) whose display is unused, another GPU's output having taken the
/// display: bound with its interrupts on, it would only hold Linux's pages
/// and answer its own interrupts. It stays where its output shows the
/// display, where no GPU's output took it (a screen connected to it later
/// may show macOS), and where its driver keeps a file of the Metal device's
/// open (a reset during its first submission). Each in a line.
#[cfg(feature = "nvk")]
fn release_unshown(failed: &[(usize, String)]) {
    let Some(gpu) = installed() else {
        return;
    };
    // Without a GPU that draws the boot ends here, and nothing is unloaded
    // before the stop's own unload.
    if gate::served_gpu(0).is_none() {
        return;
    }
    let taken = display::taken_index();
    let mut done: Vec<usize> = Vec::new();
    for &(index, _) in failed {
        if done.contains(&index) || gate::served_index(index) {
            continue;
        }
        done.push(index);
        let bound = &gpu.start.bound[index];
        let kept = match taken {
            Some(taken) if taken == index => Some("its output shows the display"),
            None => Some(
                "no GPU's output took the display, and a screen connected to it later may show \
                 macOS",
            ),
            Some(_) if gate::file_kept(index) => Some("its driver keeps the Metal device's file"),
            Some(_) => None,
        };
        if let Some(why) = kept {
            output::arguments(format_args!(
                "GPU {}: its Metal device did not open, and it stays bound: {why}\n",
                bound.name
            ));
            continue;
        }
        let device = bound.device;
        let released = Rc::new(Cell::new(None));
        let job = Box::pin(Release {
            device,
            released: released.clone(),
        });
        let time = gpu.time.as_ref();
        let result = gpu.work.dispatch_linux(|runtime| {
            Ok((|| -> Result<(), Error> {
                let deadline = time
                    .now_ns()
                    .and_then(|now| {
                        now.checked_add(RELEASE_SECONDS * 1_000_000_000)
                            .ok_or(WorkError::Exhausted)
                    })
                    .map_err(environment_error)?;
                // SAFETY: as for the start's jobs; nothing outside Linux
                // holds a file of this GPU's: its Metal device did not open
                // and the display's take closed its node.
                let pending = unsafe { gpu.threads.submit(job) }
                    .map_err(|refusal| Error::Submit(refusal.error))?;
                wait_for_child(
                    runtime,
                    gpu.threads,
                    time,
                    deadline,
                    &RELEASE,
                    false,
                    || pending.status().joined,
                )?;
                // SAFETY: physical return on the same dispatcher with the
                // joined status.
                let joined = unsafe { pending.try_join() }
                    .map_err(|_| environment_error(WorkError::Pending))?;
                drop(joined.into_job());
                Ok(())
            })())
        });
        let result = result.map_err(Error::from).and_then(|inner| inner);
        match (result, released.get()) {
            (Ok(()), Some(true)) => output::arguments(format_args!(
                "GPU {}: its Metal device did not open and another GPU's output shows the \
                 display, so {} is unloaded from it, its interrupts and bus mastering off\n",
                bound.name, bound.stack.kernel.driver
            )),
            (Ok(()), _) => output::arguments(format_args!(
                "GPU {}: no driver held it any more to unload\n",
                bound.name
            )),
            (Err(Error::Timeout), _) => output::arguments(format_args!(
                "GPU {}: {}'s unload did not finish in {RELEASE_SECONDS} s, and its task stays \
                 held\n",
                bound.name, bound.stack.kernel.driver
            )),
            (Err(error), _) => output::arguments(format_args!(
                "GPU {}: {}'s unload did not run: {error}\n",
                bound.name, bound.stack.kernel.driver
            )),
        }
    }
}

/// The Linux task that unloads one GPU's driver ([`release_unshown`]).
#[cfg(feature = "nvk")]
struct Release {
    device: usize,
    released: Rc<Cell<Option<bool>>>,
}

// SAFETY: the original managed runtime enters the canonical owner on the
// child before run; the function's attachment keeps its device for the boot.
#[cfg(feature = "nvk")]
unsafe impl ManagedJob for Release {
    fn run(self: Pin<&mut Self>) {
        // SAFETY: entered, on a task that may sleep, over the device the
        // attachment keeps.
        let released = unsafe { stela_linux::function::release_device(self.device as *mut c_void) };
        self.released.set(Some(released));
    }
}

/// When the GPU chosen to draw macOS opened no Metal device and another one
/// did, the boot screen says which draws instead; the log names the GPU that
/// draws and the one that shows the display either way.
#[cfg(feature = "nvk")]
fn report_drawer(errors: &[(usize, String)]) {
    let Some(gpu) = installed() else {
        return;
    };
    let bound = &gpu.start.bound;
    let taken = display::taken_index();
    let wanted = gate::metal_choice(bound, taken).serve.first().copied();
    let served = gate::served_gpu(0);
    if let (Some(wanted), Some(served)) = (wanted, served)
        && wanted != served
    {
        let error = errors
            .iter()
            .find(|(index, _)| *index == wanted)
            .map(|(_, error)| error.as_str());
        crate::status::warning(&stela_boot_core::gpu_status::draws_instead(
            &bound[wanted].market,
            &bound[served].market,
            error,
        ));
    }
    let named = |index: Option<usize>| match index.and_then(|index| bound.get(index)) {
        Some(gpu) => format!("the {} at {}", gpu.market, gpu.name),
        None => String::from("none"),
    };
    output::arguments(format_args!(
        "GPU USE draws macOS: {}; shows the display: {}\n",
        named(served),
        match taken {
            Some(_) => named(taken),
            None => String::from("no GPU output"),
        }
    ));
}

/// When a GPU's display shows on a laptop's built-in panel, set the panel's
/// brightness through ACPI video, as Linux's ACPI video driver does when it
/// registers the panel's backlight, which nouveau asks of it where it has
/// none of its own: the level read back where it is listed and not zero,
/// else the highest (`stela_drivers::probe::backlight`). The GPU's reset
/// may have left the backlight at zero, a picture with no light. One line
/// for each output device, or one saying none was found.
pub(super) fn panel_brightness(
    #[cfg_attr(not(feature = "nvk"), allow(unused_variables))] acpi: Option<
        &'static super::aml::HostAcpi,
    >,
) {
    #[cfg(feature = "nvk")]
    {
        use stela_drivers::probe::backlight::{self, Brightness};
        if !display::panel_shown() {
            return;
        }
        let Some(acpi) = acpi else {
            output::arguments(format_args!(
                "DISPLAY built-in panel: no ACPI namespace, so its brightness stays as it is\n"
            ));
            return;
        };
        let Some(found) = acpi.firmware(
            "panel brightness",
            "the panel keeps the brightness it has",
            false,
            backlight::initialize_all,
        ) else {
            return;
        };
        let outcomes = match found {
            Ok(outcomes) => outcomes,
            Err(error) => {
                output::arguments(format_args!(
                    "DISPLAY built-in panel: ACPI devices unreadable (status {:#x}), so its \
                     brightness stays as it is\n",
                    error.0
                ));
                return;
            }
        };
        if outcomes.is_empty() {
            output::arguments(format_args!(
                "DISPLAY built-in panel: ACPI has no display output with _BCL and _BCM, so \
                 only the GPU driver sets its brightness\n"
            ));
        }
        for outcome in outcomes {
            let path = outcome.path;
            match outcome.result {
                Ok(Brightness { level, max, read }) => {
                    let read = match read {
                        Some(read) => format!("read {read}"),
                        None => String::from("not readable"),
                    };
                    output::arguments(format_args!(
                        "DISPLAY panel brightness {path}: set to {level} of {max} ({read})\n"
                    ));
                }
                Err(None) => output::arguments(format_args!(
                    "DISPLAY panel brightness {path}: _BCL lists no levels, left as it is\n"
                )),
                Err(Some(error)) => output::arguments(format_args!(
                    "DISPLAY panel brightness {path}: _BCM refused (status {:#x})\n",
                    error.0
                )),
            }
        }
    }
}

/// The adapters among `adapters` that `claimed` holds, which the screen
/// names when none of them starts.
pub(super) fn claimed_adapters(
    claimed: &[Claimed],
    adapters: &[gpu::Adapter],
) -> Vec<gpu::Adapter> {
    adapters
        .iter()
        .filter(|adapter| {
            let address = format!("{}", adapter.address);
            claimed.iter().any(|gpu| gpu.name == address)
        })
        .copied()
        .collect()
}

/// Where an AMD GPU was claimed, name once each of AMD's graphics ACPI
/// methods a display function's companion holds, of the `companions` the
/// census found for `adapters`, which amdgpu calls only where Linux builds
/// its ACPI support and Stela's profile does not, and what it does without
/// it (`stela_drivers::probe::gpu_methods`).
pub(super) fn amd_acpi_methods(
    acpi: Option<&'static super::aml::HostAcpi>,
    claimed: &[Claimed],
    adapters: Vec<gpu::Adapter>,
    companions: Vec<(stela_drivers::bus::pci::Address, String)>,
) {
    use stela_drivers::probe::gpu_methods::{self, METHODS};
    if !claimed
        .iter()
        .any(|gpu| gpu.stack.vendor == stela_drivers::gpu::Vendor::Amd)
    {
        return;
    }
    let Some(acpi) = acpi else { return };
    let then = "amdgpu runs without AMD's graphics ACPI methods";
    let Some(found) = acpi.firmware("AMD graphics methods", then, false, move |session| {
        gpu_methods::find(session, &adapters, &companions)
    }) else {
        return;
    };
    for (method, path) in found {
        let response = METHODS
            .iter()
            .find(|(name, _)| *name == method)
            .map_or("", |(_, response)| response);
        output::arguments(format_args!(
            "UNSUPPORTED GPU ACPI {method} at {path}: amdgpu is built without ACPI; {response}\n"
        ));
    }
}

/// Bring Linux up over `claimed`, and return once nouveau has bound them or
/// failed, as the guest should not start with a half-started GPU beneath it.
pub(super) fn open(machine: Machine, claimed: Vec<Claimed>) -> Result<(), Error> {
    // Keep the exact original claims on every partial startup refusal.
    let mut claimed = ManuallyDrop::new(claimed);
    let Machine {
        work,
        window,
        random,
        tsc_hz,
        wall,
        fallback_words,
        #[cfg(feature = "nvk")]
        heap_bytes,
    } = machine;
    let image = work.image()?;
    let task_data = work.task_data()?;
    #[cfg(feature = "nvk")]
    let claiming = !claimed.is_empty();
    let lines = claimed
        .iter()
        .map(|claimed| Line {
            config: claimed.device.config.clone(),
            msi: claimed.device.msi,
            msix: None,
            custody: claimed.device.custody.clone(),
            registration: None,
            retired: None,
        })
        .collect();
    let lines = Rc::new(RefCell::new(lines));
    for claimed in claimed.iter_mut() {
        claimed.device.msix = msix_for(window, &claimed.device, &claimed.placement, &lines);
    }
    let bars = claimed
        .iter()
        .flat_map(|claimed| {
            let stolen = claimed.stolen.map(|(base, size)| base..base + size);
            claimed.bars.iter().cloned().chain(stolen)
        })
        .collect();
    let clock = work.service_clock().map_err(environment_error)?;
    let epoch = clock.epoch().map_err(environment_error)?;
    #[cfg(feature = "nvk")]
    let libc_clock = client::LibcClock::anchor(|| epoch.now_ns()).map_err(environment_error)?;
    let time = clock.clock();
    let events = Arc::new(Events {
        clock: clock.clone(),
        idle: clock.idle(),
        pending: AtomicBool::new(false),
    });
    // Linux's service runs for raised work and its earliest timer only:
    // nothing here consumes a periodic tick, and one would pump the boot
    // CPU every millisecond while the GPU idles.
    let domain = Domain::tickless(time.clone(), events.clone()).map_err(environment_error)?;
    let realtime = wall.and_then(|reading| {
        if tsc_hz == 0
            || !crate::cpu::INVARIANT_TSC.present()
            || reading.resolution == 0
            || reading.tsc_before > reading.tsc_after
        {
            return None;
        }
        // Move the firmware wall sample to a fresh observation in the
        // installed monotonic epoch, using the boot CPU's invariant TSC
        // only to carry UTC forward from the firmware read.
        let monotonic_ns = i64::try_from(epoch.now_ns().ok()?).ok()?;
        let elapsed = host::rdtsc().checked_sub(reading.tsc_after)?;
        let wall_ns = u128::from(elapsed) * 1_000_000_000 / u128::from(tsc_hz)
            + u128::from(reading.utc.nanosecond);
        let seconds = reading
            .utc
            .seconds
            .checked_add(u64::try_from(wall_ns / 1_000_000_000).ok()?)?;
        RealtimeOrigin::new(
            monotonic_ns,
            i64::try_from(seconds).ok()?,
            (wall_ns % 1_000_000_000) as u32,
        )
    });
    let platform = Host {
        work,
        task_data,
        tsc_hz,
        clock: epoch,
        realtime,
        window,
        image: image.clone(),
        random: RefCell::new(random),
        environment: Cell::new(ptr::null()),
        bars: RefCell::new(bars),
        devices: RefCell::new(BTreeMap::new()),
        lines,
        reported: RefCell::new(BTreeSet::new()),
        repeats: RefCell::new(Default::default()),
        warned: RefCell::new(stela_boot_core::gpu_triage::Repeats::new()),
        first_error: RefCell::new(None),
        probe_error: RefCell::new(None),
        gsp_start: RefCell::new(Default::default()),
        gpus: RefCell::new(
            claimed
                .iter()
                .map(|claimed| (claimed.name.clone(), claimed.stack.vendor))
                .collect(),
        ),
    };
    // SAFETY: boot's original code and allocator remain installed permanently.
    unsafe {
        stela_linux::image::register(image).map_err(|_| kernel_work::Error::Inventory)?;
        stela_linux::heap::register(allocate_zeroed).map_err(|_| kernel_work::Error::Inventory)?;
    }
    #[cfg(feature = "nvk")]
    BOOT_CPU.store(host::processor(), Ordering::Relaxed);
    // Linux's tasks run on 16 KiB stacks, where a driver's failed probe
    // stops the boot.
    #[cfg(feature = "nvk")]
    stack::keep_for_stop(window);
    let (owner, threads, job) = work.dispatch_linux(|runtime| {
        Ok(mount_and_start(
            work,
            runtime,
            platform,
            domain.clone(),
            claimed,
            time.as_ref(),
            &fallback_words,
        ))
    })??;
    // On the boot screen too, for the case that another adapter shows it;
    // a drawing GPU's after the fallback that may take its place.
    warn_unbound(&job.unbound.borrow(), !cfg!(feature = "nvk"));
    let result = job.result.clone().map_err(Error::Init);
    #[cfg(feature = "nvk")]
    let result = result.and_then(|()| {
        if claiming && job.bound.is_empty() {
            Err(Error::NoDevices)
        } else {
            Ok(())
        }
    });
    let installed = Box::new(Installed {
        work,
        time,
        events,
        domain,
        // Due at once: the first pump finds Linux's own deadline.
        next: AtomicU64::new(0),
        #[cfg(feature = "nvk")]
        window,
        tsc_hz,
        #[cfg(feature = "nvk")]
        libc_clock,
        #[cfg(feature = "nvk")]
        heap_bytes,
        #[cfg(feature = "nvk")]
        mappings: RefCell::new(stela_boot_core::client_fault::Mappings::default()),
        #[cfg(feature = "nvk")]
        mesa_threads: RefCell::new(BTreeMap::new()),
        #[cfg(feature = "nvk")]
        service: client::Service::default(),
        owner,
        threads,
        start: job,
    });
    INSTALLED.store(Box::into_raw(installed), Ordering::Release);
    result
}

/// Says on the boot screen, as in the log, that each GPU of `unbound` not
/// said yet did not start, for the case that another adapter shows the
/// screen; a GPU that draws only where `drawing`: the fallback that may take
/// its place says it with why ([`warn_one`]).
fn warn_unbound(unbound: &[Unbound], drawing: bool) {
    for unbound in unbound {
        if !unbound.warned.get() && (drawing || !unbound.draws) {
            warn_one(unbound, None);
        }
    }
}

/// Says on the boot screen that `unbound` did not start, with `cause`, its
/// stage and Linux's first error, where a fallback takes its place.
fn warn_one(unbound: &Unbound, cause: Option<&str>) {
    unbound.warned.set(true);
    crate::status::warning(&stela_boot_core::gpu_status::unbound(
        &unbound.market,
        &unbound.name,
        unbound.driver,
        unbound.picture,
        cause,
    ));
}

/// TSC ticks [`open`] took to start Linux and for nouveau's probe.
static START_TICKS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The ticks the last [`open`] took starting Linux and probing the GPUs.
pub(super) fn start_times() -> (u64, u64) {
    (
        START_TICKS[0].load(Ordering::Relaxed),
        START_TICKS[1].load(Ordering::Relaxed),
    )
}

type MountedStartup = (
    &'static Arc<LinuxKernel>,
    &'static LinuxThreads,
    ManuallyDrop<Started>,
);

fn mount_and_start(
    work: &'static KernelWork,
    runtime: Pin<&'static stela_drivers::platform::work::Runtime<SLOTS>>,
    platform: Host,
    domain: Arc<Domain<SLOTS>>,
    claimed: ManuallyDrop<Vec<Claimed>>,
    time: &dyn Clock,
    fallback_words: &[String],
) -> Result<MountedStartup, Error> {
    let deadline = time
        .now_ns()
        .and_then(|now| {
            now.checked_add(START_SECONDS * 1_000_000_000)
                .ok_or(WorkError::Exhausted)
        })
        .map_err(environment_error)?;
    let started = host::rdtsc();
    // SAFETY: the enclosing dispatch owns this exact root/CPU/clock and
    // queue. The original heap, executable backing, platform and catalogue
    // stay retained permanently, including any partial kernel publication.
    let kernel =
        unsafe { KernelOwner::new(platform, runtime, domain.clone(), work.linux_geometry()) }
            .map_err(|failure| Error::Kernel(failure.error))?;
    #[expect(
        clippy::arc_with_non_send_sync,
        reason = "the canonical broker retains this CPU-local kernel through Arc"
    )]
    let owner = &*Box::leak(Box::new(Arc::new(kernel)));
    owner
        .platform()
        .environment
        .set(addr_of!(*owner.environment()));
    // SAFETY: this same native dispatcher owns the already claimed kernel
    // and no task has been published. The catalogue must retain this owner.
    let pages = unsafe { work.register_linux_pages(owner.as_ref()) }?;
    // Kept for a fallback's bound on Linux's pages (`hold_pages`).
    #[cfg(feature = "nvk")]
    LINUX_PAGES.store(ptr::from_ref(pages).cast_mut(), Ordering::Release);
    // SAFETY: the permanent, same-owner runtime, catalogue and service
    // domain were installed above. No original child has started yet.
    let threads = unsafe { TaskRuntime::install(owner.clone(), runtime, pages, SLOTS) }
        .map_err(Error::Threads)?;
    let threads = &*Box::leak(Box::new(threads));
    // Reserve the error envelope before Initial can publish native state.
    let initial_failure = Box::new_uninit();
    // SAFETY: this exact source, root and broker remain kept on refusal. Each
    // runtime step runs inside the same live native execution segment.
    unsafe { Pin::new(owner.as_ref()).start_initial(work.linux_root(), threads) }
        .map_err(|error| Error::Initial(Box::write(initial_failure, error)))?;

    // SAFETY: submit via this actual original task broker. The service
    // initializes the original resources before dispatching accepted jobs.
    let srcu = unsafe { stela_linux::srcu::Bootstrap::start(owner.clone(), threads) }
        .map_err(|failure| Error::SrcuStart(failure.error))?;
    wait_for_child(
        runtime,
        threads,
        time,
        deadline,
        &FIRST_TASKS,
        true,
        || matches!(srcu.status(), stela_linux::srcu::Status::Pending(status) if status.joined),
    )?;
    // SAFETY: the taskless dispatcher observed the original joined job.
    // Only this consuming join can acknowledge public SRCU admission.
    let ready = unsafe { srcu.join() }.map_err(|failure| Error::Srcu(failure.error))?;
    let probing = host::rdtsc();
    START_TICKS[0].store(probing.wrapping_sub(started), Ordering::Relaxed);
    let setup = Setup::of(&claimed, fallback_words);
    let (claimed, later) = by_probe_turn(claimed);
    let completed = Rc::new(Cell::new(None));
    let job = Box::pin(Start {
        kernel: owner.as_ref(),
        claimed,
        turn: Some(ProbeTurn::First(setup)),
        completed: completed.clone(),
    });
    // SAFETY: the ready original task broker keeps the callback and all
    // original owners until its genuine managed child has joined.
    let pending = unsafe { threads.submit(job) }.map_err(|refusal| Error::Submit(refusal.error))?;
    // The second turn follows only when GPUs wait for it.
    wait_for_child(
        runtime,
        threads,
        time,
        deadline,
        &PROBE,
        !later.is_empty(),
        || pending.status().joined,
    )?;
    // SAFETY: physical return on the same dispatcher with the joined status.
    let joined =
        unsafe { pending.try_join() }.map_err(|_| environment_error(WorkError::Pending))?;
    if joined.outcome() != Outcome::CallbackReturned {
        return Err(Error::Child(joined.outcome()));
    }
    let mut job = completed
        .take()
        .ok_or_else(|| environment_error(WorkError::corrupt()))?;
    drop(joined.into_job());
    if !later.is_empty() {
        probe_later(owner.as_ref(), runtime, threads, time, later, &mut job)?;
    }
    START_TICKS[1].store(host::rdtsc().wrapping_sub(probing), Ordering::Relaxed);
    drop(ready);
    Ok((owner, threads, job))
}

/// The claimed GPUs in the two turns of their probes, as the selection put
/// them ([`stela_drivers::gpu::selection::Choice`]): first those that may
/// serve the Metal device, then those that may only show the frames another
/// draws, each in the claims' order. A probe of the second turn that hangs or
/// fails then costs its own GPU, not the one macOS draws on.
fn by_probe_turn(
    claimed: ManuallyDrop<Vec<Claimed>>,
) -> (ManuallyDrop<Vec<Claimed>>, ManuallyDrop<Vec<Claimed>>) {
    let (later, first): (Vec<_>, Vec<_>) = ManuallyDrop::into_inner(claimed)
        .into_iter()
        .partition(|claimed| cfg!(feature = "nvk") && claimed.later);
    (ManuallyDrop::new(first), ManuallyDrop::new(later))
}

/// Probes `later`, the GPUs that would only show another's frames, after the
/// first turn bound the others, within a deadline of its own. One that does
/// not finish in time is reported with its GPUs as unbound, and its task and
/// devices stay held; the GPUs of the first turn go on.
fn probe_later(
    kernel: &'static LinuxKernel,
    runtime: Pin<&'static stela_drivers::platform::work::Runtime<SLOTS>>,
    threads: &LinuxThreads,
    time: &dyn Clock,
    later: ManuallyDrop<Vec<Claimed>>,
    job: &mut Started,
) -> Result<(), Error> {
    if job.result.is_err() {
        // Linux did not start; nothing can bind them.
        return Ok(());
    }
    let mut names: Vec<Unbound> = later.iter().map(Unbound::of).collect();
    let deadline = time
        .now_ns()
        .and_then(|now| {
            now.checked_add(START_SECONDS * 1_000_000_000)
                .ok_or(WorkError::Exhausted)
        })
        .map_err(environment_error)?;
    let completed = Rc::new(Cell::new(None));
    let later_job = Box::pin(Start {
        kernel,
        claimed: later,
        turn: Some(ProbeTurn::Later),
        completed: completed.clone(),
    });
    // SAFETY: as for the first turn's job; Linux and its drivers started.
    let pending =
        unsafe { threads.submit(later_job) }.map_err(|refusal| Error::Submit(refusal.error))?;
    // Their GPUs are reported unbound with why, and the first turn's go on.
    let mut lost = |job: &mut Started, why: &dyn core::fmt::Display| {
        for unbound in names.drain(..) {
            output::arguments(format_args!(
                "GPU {}: {}'s probe {why}; the GPUs that started go on without it\n",
                unbound.name, unbound.driver
            ));
            job.unbound.get_mut().push(unbound);
        }
    };
    match wait_for_child(
        runtime,
        threads,
        time,
        deadline,
        &PROBE_LATER,
        false,
        || pending.status().joined,
    ) {
        Ok(()) => {}
        Err(Error::Timeout) => {
            lost(
                job,
                &format_args!("did not finish in {START_SECONDS} s, and its task stays held"),
            );
            // The child still runs and owns these devices; its handle
            // holds nothing of them and goes out of scope here.
            return Ok(());
        }
        Err(error) => return Err(error),
    }
    // SAFETY: physical return on the same dispatcher with the joined status.
    let joined =
        unsafe { pending.try_join() }.map_err(|_| environment_error(WorkError::Pending))?;
    if joined.outcome() != Outcome::CallbackReturned {
        lost(job, &format_args!("ended: {:?}", joined.outcome()));
        core::mem::forget(joined);
        return Ok(());
    }
    let mut later = completed
        .take()
        .ok_or_else(|| environment_error(WorkError::corrupt()))?;
    drop(joined.into_job());
    job._attached.append(&mut later._attached);
    job.unbound.get_mut().append(later.unbound.get_mut());
    #[cfg(feature = "nvk")]
    job.bound.append(&mut later.bound);
    Ok(())
}

/// Claims GPUs in place of GPUs chosen to draw that could not, as many
/// times as GPUs fail, each GPU once: `claim` runs [`claim_fallback`] in a
/// foreground continuation, which the Linux environment cannot call.
pub(super) struct Fallback {
    census: Option<Census>,
    claim: Box<FallbackClaim>,
}

/// [`Fallback`]'s claims: the census and the failed GPUs in, the census and
/// the claimed and refused GPUs out, or nothing where the claims could not
/// run.
pub(super) type FallbackClaim = dyn FnMut(
    Census,
    Vec<(usize, stela_boot_core::gpu_step::Stage)>,
) -> Option<(Census, Vec<Claimed>, Vec<Refused>)>;

impl Fallback {
    pub(super) fn new(census: Option<Census>, claim: Box<FallbackClaim>) -> Self {
        Self { census, claim }
    }

    /// Whether the GPU at census index `index` failed already, or no
    /// fallback can be made.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    fn failed(&self, index: usize) -> bool {
        self.census
            .as_ref()
            .is_none_or(|census| census.has_failed(index))
    }

    /// Whether the GPU at census index `index` is integrated, or of a kind
    /// only its probe tells.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    fn may_be_integrated(&self, index: usize) -> bool {
        self.census.as_ref().is_some_and(|census| {
            census.found[index]
                .stack
                .is_ok_and(|driven| driven.integrated.unwrap_or(true))
        })
    }

    /// Names each GPU of `failed`, census indices and the stage each failed
    /// at, in a `GPU FALLBACK` line, with `drawing`, the census index of a
    /// started GPU that draws in its place; no claim is made. Where that GPU
    /// is or may be integrated, the pages Linux is to be held to now
    /// ([`drawing_pages`]).
    #[cfg(feature = "nvk")]
    fn note(
        &mut self,
        failed: &[(usize, stela_boot_core::gpu_step::Stage)],
        drawing: usize,
    ) -> Option<usize> {
        use stela_boot_core::gpu_step::{self, Next};
        let census = self.census.as_mut()?;
        for (index, stage) in failed {
            output::arguments(format_args!(
                "GPU FALLBACK {}\n",
                gpu_step::fallback_line(&census.adapters, *index, stage, &Next::Drawing(drawing))
            ));
        }
        census.failed.extend(failed.iter().map(|&(index, _)| index));
        let name = format!("{}", census.adapters[drawing].address);
        self.may_be_integrated(drawing)
            .then(|| drawing_pages(&name))
            .flatten()
    }

    /// Claims the GPUs that start in place of `failed`, census indices and
    /// the stage each failed at, each named with its stage in a `GPU
    /// FALLBACK` line. Where the selection starts no GPU of the firmware's in
    /// their place, no claim runs. An integrated GPU is claimed only where
    /// Linux's pages leave its driver room to start, and Linux's pages are
    /// then held to the plan's bound where the plan held them to none
    /// ([`host_memory::fallback_pages`]).
    ///
    /// `warn` is told, before the claims' own lines, whether a GPU takes
    /// the failed GPUs' place: claimed now, or started already and drawing.
    ///
    /// [`host_memory::fallback_pages`]: stela_boot_core::host_memory::fallback_pages
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    fn claim(
        &mut self,
        failed: Vec<(usize, stela_boot_core::gpu_step::Stage)>,
        warn: &mut dyn FnMut(bool),
    ) -> Round {
        use stela_boot_core::host_memory::FallbackPages;
        use stela_drivers::gpu::selection::{self, Choice};
        let Some(mut census) = self.census.take() else {
            warn(false);
            return Round::default();
        };
        let mut without: Vec<usize> = census.failed.clone();
        without.extend(failed.iter().map(|&(index, _)| index));
        let unclaimed = |_: usize| true;
        let mut wanted = selection::fall_back(&census.found, &census.chosen, &without, unclaimed);
        // An integrated GPU that would start is asked first whether
        // Linux's pages leave its driver room; where not, it is left out as
        // a failed one is, and the selection made again.
        let mut pages = None;
        // Said on the screen after the failed GPUs' own lines.
        let mut starved = Vec::new();
        for _ in 0..census.adapters.len() {
            let integrated: Vec<usize> = selection::newly_claimed(&census.chosen, &wanted)
                .into_iter()
                .filter(|&index| {
                    census.found[index]
                        .stack
                        .is_ok_and(|driven| driven.integrated.unwrap_or(true))
                })
                .collect();
            if integrated.is_empty() {
                break;
            }
            let decided = crate::native_memory::fallback_pages();
            for &index in &integrated {
                output::arguments(format_args!(
                    "GPU MEMORY {} {}: {}\n",
                    census.adapters[index].address,
                    census.adapters[index].name(),
                    decided.describe()
                ));
                if decided.starved() {
                    starved.push(stela_boot_core::gpu_status::fallback_starved(
                        &census.adapters[index].name(),
                        decided.room().unwrap_or(0),
                    ));
                }
            }
            match decided {
                FallbackPages::Short { .. } | FallbackPages::Over { .. } => {
                    census.failed.extend(&integrated);
                    without.extend(&integrated);
                    wanted =
                        selection::fall_back(&census.found, &census.chosen, &without, unclaimed);
                }
                FallbackPages::Hold { limit, .. } => {
                    pages = usize::try_from(limit / PAGE).ok();
                    break;
                }
                FallbackPages::Held { .. } => break,
            }
        }
        let draws = |index: usize| census.found[index].stack.is_ok_and(|driven| driven.draws);
        if selection::newly_claimed(&census.chosen, &wanted).is_empty() {
            // Nothing to claim: the lines alone, without a continuation.
            let drawing = wanted.claimed.iter().copied().find(|&index| {
                wanted.choices[index] == Choice::First && draws(index) && !without.contains(&index)
            });
            warn(drawing.is_some());
            fall_back_census(&mut census, &failed, |_, _, _, _| None);
            self.census = Some(census);
            #[cfg(feature = "nvk")]
            let pages = drawing
                .filter(|&index| self.may_be_integrated(index))
                .and_then(|index| {
                    let census = self.census.as_ref()?;
                    drawing_pages(&format!("{}", census.adapters[index].address))
                });
            #[cfg(not(feature = "nvk"))]
            let pages = None;
            return Round {
                claimed: Vec::new(),
                pages,
            };
        }
        warn(true);
        for line in &starved {
            crate::status::warning(line);
        }
        match (self.claim)(census, failed) {
            Some((census, claimed, refused)) => {
                for refused in &refused {
                    crate::status::warning(&refused.line());
                }
                self.census = Some(census);
                Round {
                    pages: pages.filter(|_| !claimed.is_empty()),
                    claimed,
                }
            }
            None => {
                output::arguments(format_args!(
                    "GPU FALLBACK: the claims in place of the GPUs that could not draw did not \
                     run, so none is claimed\n"
                ));
                Round::default()
            }
        }
    }
}

/// What a round of [`Fallback::claim`] came to.
#[derive(Default)]
pub(super) struct Round {
    /// The GPUs claimed in the failed GPUs' place.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    claimed: Vec<Claimed>,
    /// The pages Linux is to be held to, before the claimed GPUs' probes or
    /// now, where the plan held it to none and an integrated GPU draws.
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    pages: Option<usize>,
}

/// The pages Linux is to be held to now that the integrated GPU at `name`,
/// started already, draws in place of a GPU that could not: the plan's
/// bound, where the plan held Linux to none and Linux holds no more than
/// it (`host_memory::drawing_pages`); none where Linux is held already.
/// Said in a `GPU MEMORY` line.
#[cfg(feature = "nvk")]
fn drawing_pages(name: &str) -> Option<usize> {
    let (held, limit) = stela_linux::pages::held_and_limit();
    let decided = stela_boot_core::host_memory::drawing_pages(
        crate::native_memory::fallback_bound(),
        held as u64 * PAGE,
        limit as u64 * PAGE,
    );
    output::arguments(format_args!("GPU MEMORY {name}: {}\n", decided.describe()));
    match decided {
        stela_boot_core::host_memory::FallbackPages::Hold { limit, .. } => {
            usize::try_from(limit / PAGE).ok()
        }
        _ => None,
    }
}

/// After the start, while a GPU chosen to draw is unbound and no fallback
/// for it was made, claims and starts the GPUs that draw in its place
/// ([`Fallback`]); the start's result as it stands then: no drawing GPU
/// bound is no device.
#[cfg(feature = "nvk")]
pub(super) fn fall_back_after_probe(
    opened: Result<(), Error>,
    fallback: &mut Fallback,
) -> Result<(), Error> {
    use stela_boot_core::gpu_step::Stage;
    let Some(gpu) = installed() else {
        return opened;
    };
    if !matches!(opened, Ok(()) | Err(Error::NoDevices)) {
        warn_unbound(&gpu.start.unbound.borrow(), true);
        return opened;
    }
    // Each round claims GPUs that have not failed yet, so the census ends it.
    for _ in 0..MOST_BOUND {
        let failed: Vec<(usize, Stage)> = gpu
            .start
            .unbound
            .borrow()
            .iter()
            .filter(|unbound| unbound.draws && !fallback.failed(unbound.census))
            .map(|unbound| {
                (
                    unbound.census,
                    Stage::Probe {
                        what: unbound.what.clone(),
                        error: unbound.error.clone(),
                    },
                )
            })
            .collect();
        if failed.is_empty() {
            break;
        }
        let censuses: Vec<usize> = failed.iter().map(|&(index, _)| index).collect();
        // Before the claims' own lines: each GPU's, with why it could not
        // draw where another takes its place.
        let round = fallback.claim(failed, &mut |replaced| {
            for unbound in gpu.start.unbound.borrow().iter() {
                if censuses.contains(&unbound.census) && !unbound.warned.get() {
                    warn_one(unbound, replaced.then(|| unbound.cause()).as_deref());
                }
            }
        });
        if round.claimed.is_empty() {
            if let Some(pages) = round.pages {
                hold_pages_now(gpu, pages);
            }
            break;
        }
        if let Err(error) = start_more(gpu, round.claimed, round.pages) {
            output::arguments(format_args!(
                "GPU FALLBACK: the GPUs claimed in place of the ones that could not draw did \
                 not start: {error}\n"
            ));
            break;
        }
    }
    warn_unbound(&gpu.start.unbound.borrow(), true);
    match gpu.start.bound.is_empty() {
        true => Err(Error::NoDevices),
        false => Ok(()),
    }
}

/// Starts `claimed`, GPUs claimed after Linux started in place of GPUs that
/// could not draw, as a turn of probes of their own within [`START_SECONDS`]:
/// their lines, register windows and searched functions join Linux's, and
/// the GPUs bound and unbound join the start's. A word of Linux's command
/// line a claim needs that the start did not set is named: Linux read its
/// command line once.
#[cfg(feature = "nvk")]
fn start_more(
    gpu: &'static Installed,
    claimed: Vec<Claimed>,
    pages: Option<usize>,
) -> Result<(), Error> {
    for claimed in &claimed {
        let words = claimed.parameters.as_deref().unwrap_or_default();
        for word in words.split_whitespace() {
            if !gpu
                .start
                .command_line
                .split_whitespace()
                .any(|set| set == word)
            {
                output::arguments(format_args!(
                    "GPU FALLBACK {}: Linux started without \"{word}\", which its driver reads \
                     only at Linux's start, so it runs without it\n",
                    claimed.name
                ));
            }
        }
    }
    let unbound_before = gpu.start.unbound.borrow().len();
    let mut claimed = ManuallyDrop::new(claimed);
    let result = gpu
        .work
        .dispatch_linux(|runtime| Ok(more_probes(gpu, runtime, &mut claimed, pages)));
    // On the boot screen too, as the start's are; a drawing GPU's after
    // the fallback that may take its place.
    warn_unbound(&gpu.start.unbound.borrow()[unbound_before..], false);
    result?
}

/// [`start_more`] in the Linux environment.
#[cfg(feature = "nvk")]
fn more_probes(
    gpu: &'static Installed,
    runtime: Pin<&'static stela_drivers::platform::work::Runtime<SLOTS>>,
    claimed: &mut ManuallyDrop<Vec<Claimed>>,
    pages: Option<usize>,
) -> Result<(), Error> {
    let kernel: &'static LinuxKernel = gpu.owner.as_ref();
    let host = kernel.platform();
    {
        let mut lines = host.lines.borrow_mut();
        for claimed in claimed.iter_mut() {
            // A line's number is its place among the lines, which MSI-X
            // vectors added to.
            claimed.placement.irq = lines.len() as u32;
            lines.push(Line {
                config: claimed.device.config.clone(),
                msi: claimed.device.msi,
                msix: None,
                custody: claimed.device.custody.clone(),
                registration: None,
                retired: None,
            });
        }
    }
    for claimed in claimed.iter_mut() {
        claimed.device.msix =
            msix_for(gpu.window, &claimed.device, &claimed.placement, &host.lines);
    }
    host.bars
        .borrow_mut()
        .extend(claimed.iter().flat_map(|claimed| {
            let stolen = claimed.stolen.map(|(base, size)| base..base + size);
            claimed.bars.iter().cloned().chain(stolen)
        }));
    host.gpus.borrow_mut().extend(
        claimed
            .iter()
            .map(|claimed| (claimed.name.clone(), claimed.stack.vendor)),
    );
    let published = gpu.start.searched_at.borrow().clone();
    let searched = searched_of(claimed)
        .into_iter()
        .filter(|searched| !published.contains(&(searched.identity.bus, searched.identity.devfn)))
        .collect();
    let stolen = claimed.iter().find_map(|claimed| claimed.stolen);
    let mut names: Vec<Unbound> = claimed.iter().map(Unbound::of).collect();
    let time = gpu.time.as_ref();
    let deadline = time
        .now_ns()
        .and_then(|now| {
            now.checked_add(START_SECONDS * 1_000_000_000)
                .ok_or(WorkError::Exhausted)
        })
        .map_err(environment_error)?;
    let completed = Rc::new(Cell::new(None));
    let job = Box::pin(Start {
        kernel,
        claimed: ManuallyDrop::new(core::mem::take(&mut **claimed)),
        turn: Some(ProbeTurn::Fallback {
            searched,
            stolen,
            pages,
        }),
        completed: completed.clone(),
    });
    // SAFETY: as for the start's jobs; Linux and its drivers started.
    let pending =
        unsafe { gpu.threads.submit(job) }.map_err(|refusal| Error::Submit(refusal.error))?;
    let mut lost = |why: &dyn core::fmt::Display| {
        for unbound in names.drain(..) {
            output::arguments(format_args!(
                "GPU {}: {}'s probe {why}\n",
                unbound.name, unbound.driver
            ));
            gpu.start.unbound.borrow_mut().push(unbound);
        }
    };
    match wait_for_child(
        runtime,
        gpu.threads,
        time,
        deadline,
        &PROBE_FALLBACK,
        false,
        || pending.status().joined,
    ) {
        Ok(()) => {}
        Err(Error::Timeout) => {
            lost(&format_args!(
                "did not finish in {START_SECONDS} s, and its task stays held"
            ));
            return Ok(());
        }
        Err(error) => return Err(error),
    }
    // SAFETY: physical return on the same dispatcher with the joined status.
    let joined =
        unsafe { pending.try_join() }.map_err(|_| environment_error(WorkError::Pending))?;
    if joined.outcome() != Outcome::CallbackReturned {
        lost(&format_args!("ended: {:?}", joined.outcome()));
        core::mem::forget(joined);
        return Ok(());
    }
    let mut more = completed
        .take()
        .ok_or_else(|| environment_error(WorkError::corrupt()))?;
    drop(joined.into_job());
    gpu.start
        .searched_at
        .borrow_mut()
        .extend(more.searched_at.get_mut().drain(..));
    gpu.start
        .unbound
        .borrow_mut()
        .append(more.unbound.get_mut());
    gpu.start.bound.append(&mut more.bound);
    // Its attachments and publications are kept for the boot, as the
    // start's are: `more` is never dropped.
    Ok(())
}

/// The start's stages [`wait_for_child`] names.
static FIRST_TASKS: &str = "Linux's first tasks";
static PROBE: &str = "the GPU's probe";
static PROBE_LATER: &str = "the probe of GPUs that only show frames";
#[cfg(feature = "nvk")]
static PROBE_FALLBACK: &str = "the probe of GPUs started in place of GPUs that could not draw";

/// Wait for `stage` of the start, which Linux's tasks run, until `finished`
/// or `deadline` on `time`. A long wait says every
/// [`WAIT_REPORT_SECONDS`] how long it has waited by the counter and by
/// Linux's clock, and how many service steps it took, and stores the USB's
/// log: a start that never returns then shows whether this loop still
/// runs, whether Linux's clock moves, and which stage it waits for. The
/// counter ends the wait after [`START_SECONDS`] even when the clock
/// stands still, and stores the log before the start is given up. A stage
/// another follows (`then`) stores the log when it finishes, so a next stage
/// that stops the machine within its first report leaves this one's lines on
/// the stick; the last stage's are stored by `open_gpus` once the start
/// returns, outside it.
fn wait_for_child(
    runtime: Pin<&stela_drivers::platform::work::Runtime<SLOTS>>,
    threads: &LinuxThreads,
    time: &dyn Clock,
    deadline: u64,
    stage: &'static &'static str,
    then: bool,
    finished: impl Fn() -> bool,
) -> Result<(), Error> {
    struct Staged;
    impl Drop for Staged {
        fn drop(&mut self) {
            super::step_watchdog::clear_stage();
        }
    }
    super::step_watchdog::stage(stage);
    let _staged = Staged;
    let began = host::rdtsc();
    let began_ns = time.now_ns().map_err(environment_error)?;
    let mut steps = 0u64;
    let mut reports = 0u64;
    loop {
        // SAFETY: runtime is on its dispatcher after real physical return.
        unsafe { threads.dispatcher_step() }.map_err(Error::Threads)?;
        if finished() {
            if then {
                super::storage::store_usb_log_in_gpu_start();
            }
            return Ok(());
        }
        let now = time.now_ns().map_err(environment_error)?;
        let waited_ms = output::millis_since(began);
        // The counter bounds the wait too, when Linux's clock stands still.
        if now >= deadline || waited_ms >= START_SECONDS * 1000 {
            super::storage::store_usb_log_in_gpu_start();
            // A probe left running is no longer the one a stop comes in.
            PROBING.try_with(|probing| *probing = None);
            return Err(Error::Timeout);
        }
        if waited_ms / 1000 / WAIT_REPORT_SECONDS > reports {
            reports = waited_ms / 1000 / WAIT_REPORT_SECONDS;
            output::arguments(format_args!(
                "GPU START waiting for {stage}: {waited_ms} ms by the counter, {} ms by \
                 Linux's clock, {steps} steps\n",
                now.saturating_sub(began_ns) / 1_000_000,
            ));
            super::storage::store_usb_log_in_gpu_start();
        }
        runtime.step_or_idle().map_err(environment_error)?;
        steps += 1;
    }
}

/// Startup failures distinguish the work environment from driver admission.
#[derive(Debug)]
pub(super) enum Error {
    Environment(kernel_work::Error),
    Kernel(stela_linux::kernel::Error),
    Threads(stela_linux::task::runtime::RuntimeError),
    Initial(Box<stela_linux::kernel::InitialStartError>),
    SrcuStart(stela_linux::srcu::StartError),
    Srcu(stela_linux::srcu::Error),
    Submit(stela_linux::task::runtime::SubmitError),
    Child(Outcome),
    Init(gpu::stack::StartError),
    Timeout,
    #[cfg(feature = "nvk")]
    NoDevices,
}

impl From<kernel_work::Error> for Error {
    fn from(error: kernel_work::Error) -> Self {
        Self::Environment(error)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Environment(error) => write!(f, "Linux environment: {error}"),
            Self::Kernel(error) => write!(f, "{error}"),
            Self::Threads(error) => write!(f, "{error}"),
            Self::Initial(error) => write!(f, "{error}"),
            Self::SrcuStart(error) => write!(f, "{error}"),
            Self::Srcu(error) => write!(f, "{error}"),
            Self::Submit(error) => write!(f, "GPU startup job refused: {error:?}"),
            Self::Child(outcome) => write!(f, "GPU startup child ended: {outcome:?}"),
            Self::Init(error) => write!(f, "Linux initialization failed: {error}"),
            Self::Timeout => {
                f.write_str("GPU startup exceeded its deadline; native owners remain held")
            }
            #[cfg(feature = "nvk")]
            Self::NoDevices => f.write_str("no Linux driver bound a claimed GPU"),
        }
    }
}

impl Installed {
    /// The TSC ticks in `ns` nanoseconds.
    fn tsc_after(&self, ns: u64) -> u64 {
        let ticks = u128::from(ns) * u128::from(self.tsc_hz) / 1_000_000_000;
        ticks.min(u128::from(u64::MAX)) as u64
    }
}

/// When the environment's pending events were raised, as a TSC value: the
/// age the boot PE's run loop bounds ([`pump_due`]).
static PENDING_SINCE: AtomicU64 = AtomicU64::new(0);

/// How long the environment's work may wait while the boot PE runs guest
/// code at EL1 or EL2 with interrupts unmasked: it may hold a lock that
/// disables only preemption, which other PEs spin on while it pumps.
const DEFER_UNMASKED_NS: u64 = 1_000_000;
/// The longest the work waits whatever the boot PE runs, interrupts masked
/// included, where XNU holds its spinlocks: GPU completions and the calls
/// other PEs posted are never later than this.
const DEFER_LONGEST_NS: u64 = 4_000_000;
/// How often a slice of a boot PE that holds work back ends to look again.
const RECHECK_NS: u64 = 250_000;

/// Why pumps ran, and what they took, since the last [`pump_report`].
struct PumpCounts {
    /// Pumps that ran the environment, by the boot PE's [`Calm`].
    quiet: AtomicU64,
    unmasked: AtomicU64,
    forced: AtomicU64,
    /// Pumps from a wait for the service's turn ([`gate::relax`]).
    relaxed: AtomicU64,
    /// Pumps for a posted call from the host service ([`pump_posted`]).
    posted: AtomicU64,
    /// Pumps by who ran them ([`Taker`]): the boot PE, a PE whose guest
    /// waits, and Linux runs of a PE waiting for its own posted call.
    by_taker: [AtomicU64; 3],
    /// Ticks from work being ready until a pump took it, by taker, and the
    /// longest, for the boot PE and a waiting PE.
    taker_delay: [AtomicU64; 2],
    taker_delay_longest: [AtomicU64; 2],
    /// Sleeping PEs woken for ready work, and pumps a processor did not run
    /// because another held the turn or the host drivers' lock.
    woken: AtomicU64,
    turn_busy: AtomicU64,
    /// Calls for raised work not made, as a processor held the turn.
    wakes_held: AtomicU64,
    /// Pumps for raised events, and for Linux's deadline alone.
    by_event: AtomicU64,
    by_deadline: AtomicU64,
    /// Pumps whose first step found nothing to run.
    idle: AtomicU64,
    /// Slice ends that held ready work back for a calmer boot PE.
    deferred: AtomicU64,
    /// Ticks pumping, and the longest pump.
    ticks: AtomicU64,
    longest: AtomicU64,
    /// Ticks from work being ready until a pump took it, and the longest.
    delay: AtomicU64,
    delay_longest: AtomicU64,
    /// The longest single step, which no budget ends: one Linux task run
    /// until it sleeps. With the host's mapping shootdowns during it and the
    /// posted calls (request code plus one) workers were performing as it
    /// began.
    step_longest: AtomicU64,
    step_shootdowns: AtomicU64,
    step_serving: [AtomicU32; 4],
    /// Linux steps the pumps ran.
    steps: AtomicU64,
    /// Ticks the pumps spent by part ([`PumpPart`]), and the longest one
    /// part took in a pump: what a long pump spent its time in.
    parts: [AtomicU64; PumpPart::COUNT],
    part_longest: [AtomicU64; PumpPart::COUNT],
}

/// The parts of a pump, timed for [`pump_report`].
#[derive(Clone, Copy)]
enum PumpPart {
    /// Entering the environment: the host drivers' lock, the root switch
    /// and the identity checks, until the first step.
    Enter,
    /// Retiring tasks and taking posted calls ([`client::start`]).
    Children,
    /// Linux's steps.
    Steps,
    /// Linux's next deadline, and leaving the environment.
    Leave,
    /// Looking for completed work ([`look_due`]).
    Look,
}

impl PumpPart {
    const COUNT: usize = 5;
    const NAMES: [&str; Self::COUNT] = ["enter", "children", "steps", "leave", "look"];

    fn add(self, ticks: u64) {
        PUMPS.parts[self as usize].fetch_add(ticks, Ordering::Relaxed);
        PUMPS.part_longest[self as usize].fetch_max(ticks, Ordering::Relaxed);
    }
}

/// Host page-window unmappings by their source since [`pump_report`]: Linux
/// device memory (`iounmap`), Linux pages (`vfree`, `vunmap`), the C
/// library's file mappings (`munmap`, fixed remaps) and task stacks. Each
/// costs one shootdown of every other processor.
static UNMAPS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The [`UNMAPS`] sources.
#[derive(Clone, Copy)]
pub(super) enum Unmapped {
    Device,
    Pages,
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    Files,
    #[cfg_attr(not(feature = "nvk"), allow(dead_code))]
    Stacks,
}

/// Counts one page-window unmapping from `source`.
pub(super) fn unmapped(source: Unmapped) {
    UNMAPS[source as usize].fetch_add(1, Ordering::Relaxed);
}

static PUMPS: PumpCounts = PumpCounts {
    quiet: AtomicU64::new(0),
    unmasked: AtomicU64::new(0),
    forced: AtomicU64::new(0),
    relaxed: AtomicU64::new(0),
    posted: AtomicU64::new(0),
    by_taker: [const { AtomicU64::new(0) }; 3],
    taker_delay: [const { AtomicU64::new(0) }; 2],
    taker_delay_longest: [const { AtomicU64::new(0) }; 2],
    woken: AtomicU64::new(0),
    turn_busy: AtomicU64::new(0),
    wakes_held: AtomicU64::new(0),
    by_event: AtomicU64::new(0),
    by_deadline: AtomicU64::new(0),
    idle: AtomicU64::new(0),
    deferred: AtomicU64::new(0),
    ticks: AtomicU64::new(0),
    longest: AtomicU64::new(0),
    delay: AtomicU64::new(0),
    delay_longest: AtomicU64::new(0),
    step_longest: AtomicU64::new(0),
    step_shootdowns: AtomicU64::new(0),
    step_serving: [const { AtomicU32::new(0) }; 4],
    steps: AtomicU64::new(0),
    parts: [const { AtomicU64::new(0) }; PumpPart::COUNT],
    part_longest: [const { AtomicU64::new(0) }; PumpPart::COUNT],
};

/// Ticks the boot CPU spent in posted pumps ([`pump_posted`]).
#[cfg(feature = "nvk")]
static POSTED_TICKS: AtomicU64 = AtomicU64::new(0);

/// The ticks this processor spent in posted pumps so far: the boot CPU's
/// total, and nothing on another processor, which runs none.
pub(super) fn posted_ticks_here() -> u64 {
    #[cfg(feature = "nvk")]
    if host::processor() == BOOT_CPU.load(Ordering::Relaxed) {
        return POSTED_TICKS.load(Ordering::Relaxed);
    }
    0
}

/// The shootdowns counted so far, for a difference across a step.
fn shootdowns() -> u64 {
    #[cfg(feature = "nvk")]
    return crate::native_mmio::shootdown::count();
    #[cfg(not(feature = "nvk"))]
    0
}

/// Notes a step of `ticks` that began with `serving` and overlapped
/// `shot` shootdowns, when it is the longest since the last report.
fn note_step(ticks: u64, shot: u64, serving: [u32; 4]) {
    if PUMPS.step_longest.fetch_max(ticks, Ordering::Relaxed) < ticks {
        PUMPS.step_shootdowns.store(shot, Ordering::Relaxed);
        for (into, code) in PUMPS.step_serving.iter().zip(serving) {
            into.store(code, Ordering::Relaxed);
        }
    }
}

/// Since when the environment has had work ready, as a TSC value: events
/// raised or its deadline passed.
fn ready_since(gpu: &Installed, now: u64) -> Option<u64> {
    let raised = gpu.events.pending.load(Ordering::Acquire).then(|| {
        // An event raised as a pump cleared its time ages from here.
        match PENDING_SINCE.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => now,
            Err(since) => since.min(now),
        }
    });
    let next = gpu.next.load(Ordering::Relaxed);
    let due = (now >= next).then_some(next);
    match (raised, due) {
        (Some(raised), Some(due)) => Some(raised.min(due)),
        (raised, due) => raised.or(due),
    }
}

/// Opens the boot CPU's interrupt window, which a GPU's MSI needs: compiled
/// code runs with interrupts off and a busy PE never waits. Taking one only
/// records it, so this is cheap whatever the guest holds.
pub(super) fn interrupt_window() {
    if installed().is_some() {
        native_irq::interrupt_window();
    }
}

/// Whether the run loop pumps now, the boot PE being `calm`: at once when it
/// holds no guest lock, and otherwise only once the ready work has waited
/// [`DEFER_UNMASKED_NS`] with interrupts unmasked or [`DEFER_LONGEST_NS`]
/// whatever it runs, so completions and posted calls are never starved
/// ([`boot_decision`]). Work it holds back first wakes a PE whose guest
/// waits, once for each episode, which pumps it at once: the boot PE pumps
/// only what no such PE took.
pub(super) fn pump_due(calm: Calm) -> bool {
    let Some(gpu) = installed() else {
        return false;
    };
    let now = host::rdtsc();
    let Some(since) = ready_since(gpu, now) else {
        return false;
    };
    let waited = now.saturating_sub(since);
    let limits = Limits {
        unmasked: gpu.tsc_after(DEFER_UNMASKED_NS),
        longest: gpu.tsc_after(DEFER_LONGEST_NS),
    };
    let Some(why) = boot_decision(calm, waited, limits) else {
        PUMPS.deferred.fetch_add(1, Ordering::Relaxed);
        // A Linux timer that came due raised no event, so nothing woke a
        // sleeper for it yet. Not while a processor pumps: it runs the timer.
        if may_move() && TURN.wake_due() && WAKES.first_for(since) && call_idle_core() {
            PUMPS.woken.fetch_add(1, Ordering::Relaxed);
        }
        return false;
    };
    let counter = match why {
        BootPump::Forced => &PUMPS.forced,
        BootPump::Quiet => &PUMPS.quiet,
        BootPump::Unmasked => &PUMPS.unmasked,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    true
}

/// Whether a processor other than the boot CPU may run Linux in its place:
/// with the GPUs' Metal service, until the GPUs' stop closes it
/// ([`close_moves`]).
fn may_move() -> bool {
    cfg!(feature = "nvk") && !host::moves_closed()
}

/// Whether this is the boot CPU, which Linux lives on.
fn on_boot_cpu() -> bool {
    #[cfg(feature = "nvk")]
    return host::processor() == BOOT_CPU.load(Ordering::Relaxed);
    #[cfg(not(feature = "nvk"))]
    true
}

/// This processor's index: the boot CPU's, without the Metal service.
fn this_processor() -> u32 {
    #[cfg(feature = "nvk")]
    return host::processor();
    #[cfg(not(feature = "nvk"))]
    0
}

/// Has every later Linux entry run on the boot CPU, and waits for one in
/// its place to leave, for the GPUs' stop: its work changes interrupt
/// routes and unloads firmware, which the boot CPU owns.
#[cfg(feature = "nvk")]
pub(super) fn close_moves() {
    let Some(gpu) = installed() else {
        return;
    };
    if !host::close_moves(gpu.tsc_after(MOVES_CLOSE_NS)) {
        output::arguments(format_args!(
            "GPU stop: Linux still runs on another processor after {} ms\n",
            MOVES_CLOSE_NS / 1_000_000
        ));
    }
}

/// Run what is ready in the environment from the boot PE's run loop:
/// between its slices once [`pump_due`], or while it waits for the
/// service's turn.
pub(super) fn pump() {
    pump_as(Taker::Boot, &|| false);
}

/// Run what is ready from the idle service of a PE whose guest waits for an
/// interrupt, on its own core, within [`WAITING_PUMP_NS`] and only until its
/// guest has work (`guest_work`). Nothing with no work ready, or while the
/// GPUs stop.
pub(super) fn pump_waiting_pe(guest_work: &dyn Fn() -> bool) {
    if on_boot_cpu() || !may_move() {
        return;
    }
    if installed().is_some_and(|gpu| ready_since(gpu, host::rdtsc()).is_some()) {
        pump_as(Taker::Waiting, guest_work);
    }
}

/// One pump by `taker` on this processor, under the [`TURN`]: what was
/// raised and Linux's due timers, its steps until nothing runs, the budget
/// ends, another core wants the host drivers or, off the boot CPU,
/// `guest_work` says this PE's guest has work. A processor that finds the
/// turn or the host drivers' lock taken leaves the work pending for the one
/// that has it, or for the boot PE's fallback.
fn pump_as(taker: Taker, guest_work: &dyn Fn() -> bool) {
    let Some(gpu) = installed() else {
        return;
    };
    let boot = on_boot_cpu();
    if boot {
        native_irq::interrupt_window();
    }
    let Some(since) = ready_since(gpu, host::rdtsc()) else {
        return;
    };
    let Some(turn) = TURN.try_take(this_processor()) else {
        PUMPS.turn_busy.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let entered = host::rdtsc();
    let budget = gpu.tsc_after(if boot { PUMP_NS } else { WAITING_PUMP_NS });
    let mut steps = 0u32;
    let mut started = entered;
    let mut part_at = entered;
    let mut lap = |part: PumpPart| {
        let now = host::rdtsc();
        part.add(now.wrapping_sub(part_at));
        part_at = now;
    };
    let body = |runtime: Pin<&'static stela_drivers::platform::work::Runtime<SLOTS>>| {
        lap(PumpPart::Enter);
        // Taken here, inside: a pump that could not enter leaves the work
        // pending with its age. Cleared before the flag: an event raised
        // between the two stores its own time once the flag is clear.
        started = host::rdtsc();
        PENDING_SINCE.store(0, Ordering::Release);
        let raised = gpu.events.pending.swap(false, Ordering::AcqRel);
        let delay = started.saturating_sub(since);
        PUMPS.delay.fetch_add(delay, Ordering::Relaxed);
        PUMPS.delay_longest.fetch_max(delay, Ordering::Relaxed);
        PUMPS.by_taker[taker as usize].fetch_add(1, Ordering::Relaxed);
        if let Some(index) = match taker {
            Taker::Boot => Some(0),
            Taker::Waiting => Some(1),
            Taker::Poster => None,
        } {
            PUMPS.taker_delay[index].fetch_add(delay, Ordering::Relaxed);
            PUMPS.taker_delay_longest[index].fetch_max(delay, Ordering::Relaxed);
        }
        let cause = if raised {
            &PUMPS.by_event
        } else {
            &PUMPS.by_deadline
        };
        cause.fetch_add(1, Ordering::Relaxed);
        let mut first = true;
        loop {
            // What is raised from here on this pass takes: the step runs
            // the tasks it woke, and `client::start` the calls posted. Only
            // what comes after stays pending for the next pump, which does
            // not then run for nothing.
            if !first {
                PENDING_SINCE.store(0, Ordering::Release);
                gpu.events.pending.store(false, Ordering::Release);
            }
            // Tasks retire and calls arrive only through steps and posts:
            // a pump's first pass looks only if something may have.
            #[cfg(feature = "nvk")]
            if !first || client::start_due() {
                gpu.advance_children();
                client::start(gpu)?;
            }
            #[cfg(not(feature = "nvk"))]
            gpu.advance_children();
            first = false;
            lap(PumpPart::Children);
            #[cfg(feature = "nvk")]
            let serving = client::serving();
            #[cfg(not(feature = "nvk"))]
            let serving = [0; 4];
            let (step_started, shot) = (host::rdtsc(), shootdowns());
            let stepped = runtime.step()?;
            lap(PumpPart::Steps);
            if !stepped {
                break;
            }
            let now_shot = shootdowns();
            note_step(
                host::rdtsc().wrapping_sub(step_started),
                now_shot.checked_sub(shot).unwrap_or(now_shot),
                serving,
            );
            steps += 1;
            // A core spinning for the host drivers' lock this pump holds
            // (a disk or input call) takes it after this step; the rest
            // stays pending for the next pump. So does it once this PE's
            // guest has work, off the boot CPU.
            if super::service::shutdown_pending()
                || host::rdtsc().wrapping_sub(started) >= budget
                || super::storage::host_drivers_wanted()
                || (!boot && guest_work())
            {
                if !gpu.events.pending.swap(true, Ordering::AcqRel) {
                    PENDING_SINCE.store(host::rdtsc(), Ordering::Release);
                }
                break;
            }
        }
        let next = match gpu.domain.next_deadline()? {
            Some(next) => {
                let left = next.saturating_sub(gpu.time.now_ns()?);
                host::rdtsc().saturating_add(gpu.tsc_after(left))
            }
            None => u64::MAX,
        };
        gpu.next.store(next, Ordering::Relaxed);
        Ok(())
    };
    // A waiting PE waits briefly for the lock: a poster running Linux for
    // its own call lets go at its next step once a core waits.
    let ran = if boot {
        Some(gpu.work.dispatch_linux(body))
    } else {
        gpu.work.dispatch_linux_moved(MOVED_LOCK_WAIT_MICROS, body)
    };
    match ran {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            output::fatal(format_args!("the host's Linux environment failed: {error}"))
        }
        None => {
            // The host drivers' lock was taken: whoever holds it runs Linux
            // or a driver, and the work stays pending for the next pump.
            PUMPS.turn_busy.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    lap(PumpPart::Leave);
    if steps == 0 {
        PUMPS.idle.fetch_add(1, Ordering::Relaxed);
    }
    PUMPS.steps.fetch_add(u64::from(steps), Ordering::Relaxed);
    // The turn goes before the look, which may wait for Linux: for a call
    // it posts, which off the boot CPU this processor runs Linux for
    // itself (`serve_posted`), or for a lock of Mesa's a job holds while
    // its own posted call waits for a pump, which the wait runs on this
    // processor (`gate::relax`). Kept, every pump found the turn taken
    // until the call was withdrawn. Work raised while it was held called
    // no core ([`call_for_work`]), so one is called for it now.
    if turn.release(|| gpu.events.pending.load(Ordering::Acquire)) {
        call_for_work();
    }
    #[cfg(feature = "nvk")]
    match look_due(gpu) {
        Look::Device => {
            gate::interrupted();
            gate::watch_completions();
        }
        Look::Waiters => gate::watch_completions(),
        Look::No => {}
    }
    // A lost GPU that did not reopen, with no other to serve macOS.
    #[cfg(feature = "nvk")]
    gate::watch_losses();
    lap(PumpPart::Look);
    // The pipelines the device added go to the store's file, queued a
    // piece at a time, from the boot CPU.
    #[cfg(feature = "nvk")]
    if boot {
        super::pipelines::step();
    }
    let took = host::rdtsc().wrapping_sub(entered);
    PUMPS.ticks.fetch_add(took, Ordering::Relaxed);
    PUMPS.longest.fetch_max(took, Ordering::Relaxed);
}

/// Run Linux on this processor, not the boot CPU, for a call it posted and
/// waits for (`client::spin`), while `waiting`: in the boot CPU's place, as
/// the boot CPU would in its run loop, for [`POSTER_PUMP_NS`] at most or
/// until nothing runs. `None` when it did not enter: with the host drivers'
/// lock taken, or where it does not serve its own calls
/// ([`serves_own_posts`]), the caller spins. Otherwise whether a step ran:
/// with none, the caller waits for an event or a due timer
/// ([`posted_work_ready`]) before it enters again.
#[cfg(feature = "nvk")]
fn serve_posted(gpu: &Installed, waiting: &dyn Fn() -> bool) -> Option<bool> {
    if !serves_own_posts() {
        return None;
    }
    let started = host::rdtsc();
    let budget = gpu.tsc_after(POSTER_PUMP_NS);
    let mut stepped = false;
    let ran = gpu.work.dispatch_linux_moved(0, |runtime| {
        PUMPS.by_taker[Taker::Poster as usize].fetch_add(1, Ordering::Relaxed);
        while waiting() {
            gpu.advance_children();
            client::start(gpu)?;
            if !waiting() || !runtime.step()? {
                break;
            }
            stepped = true;
            client::children_stale();
            if host::rdtsc().wrapping_sub(started) >= budget
                || super::storage::host_drivers_wanted()
            {
                break;
            }
        }
        Ok(())
    })?;
    if let Err(error) = ran {
        output::fatal(format_args!("the host's Linux environment failed: {error}"));
    }
    Some(stepped)
}

/// How long a poster that found nothing to run ([`serve_posted`]) spins
/// before it enters again: with work raised or due, which the processor it
/// woke may be taking, and without, for a call no other processor took.
#[cfg(feature = "nvk")]
const POSTER_READY_NS: u64 = 20_000;
#[cfg(feature = "nvk")]
const POSTER_RECHECK_NS: u64 = 100_000;

/// Whether a poster that found nothing to run when it left at `since`
/// enters again ([`POSTER_READY_NS`], [`POSTER_RECHECK_NS`]).
#[cfg(feature = "nvk")]
fn posted_work_ready(gpu: &Installed, since: u64) -> bool {
    let now = host::rdtsc();
    let idle = now.wrapping_sub(since);
    idle >= gpu.tsc_after(POSTER_RECHECK_NS)
        || (idle >= gpu.tsc_after(POSTER_READY_NS) && ready_since(gpu, now).is_some())
}

/// Whether this processor runs Linux itself for the calls it posts
/// ([`serve_posted`]), so that none needs waking for them: a processor
/// other than the boot CPU, outside a job's slice and an interrupt handler,
/// while the GPUs do not stop.
#[cfg(feature = "nvk")]
fn serves_own_posts() -> bool {
    host::processor() != BOOT_CPU.load(Ordering::Relaxed)
        && may_move()
        && !slice::in_job()
        && !host::in_interrupt()
}

/// A pump for a posted call, from host work on the boot CPU that can run
/// for milliseconds between the run loop's pumps (the host service's device
/// polls): an AP waiting for a DRM call waits for the boot CPU, not for that
/// work. Nothing runs elsewhere, or with no call posted.
pub(super) fn pump_posted() {
    #[cfg(feature = "nvk")]
    if client::posted_waiting() && host::processor() == BOOT_CPU.load(Ordering::Relaxed) {
        PUMPS.posted.fetch_add(1, Ordering::Relaxed);
        let started = host::rdtsc();
        pump();
        POSTED_TICKS.fetch_add(host::rdtsc().wrapping_sub(started), Ordering::Relaxed);
    }
}

/// A pump from a wait for the service's turn on the boot CPU, which runs no
/// guest code meanwhile.
#[cfg(feature = "nvk")]
fn pump_waiting() {
    if installed().is_some_and(|gpu| ready_since(gpu, host::rdtsc()).is_some()) {
        PUMPS.relaxed.fetch_add(1, Ordering::Relaxed);
    }
    pump();
}

/// `GPU PUMP runs=… quiet=… unmasked=… forced=… relaxed=… posted=… by_event=…
/// by_deadline=… idle=… deferred=… mean_us=… longest_us=… delay_mean_us=…
/// delay_longest_us=… next_us=… next_timer=…`: the pumps since the last
/// report, by what let each run and why it was due, what they took, how long
/// ready work waited for one, and Linux's next deadline with the function
/// of its first timer-list timer, as an offset in this image. Then
/// `step_longest_us=… step_shootdowns=… step_serving=…`: the longest single
/// Linux step, the shootdowns during it and the posted calls being performed
/// as it began (`-` for none), and `unmaps=device/pages/files/stacks`
/// ([`UNMAPS`]). Then `by_boot=… by_waiting=… by_poster=… woken=…
/// turn_busy=… wakes_held=… boot_delay_mean_us=… boot_delay_longest_us=…
/// waiting_delay_mean_us=… waiting_delay_longest_us=…`: who ran Linux
/// ([`Taker`]: the boot PE's pumps, pumps on PEs whose guest waits, and runs
/// of PEs for their own posted calls), the sleeping PEs woken for work, the
/// pumps not run as another processor held the turn or the host drivers'
/// lock, the calls for work not made as a processor held the turn, and how
/// long ready work waited for the boot PE's pumps and the
/// waiting PEs'. And `total_us=enter/children/steps/leave/look:…` the
/// pumps' time by [`PumpPart`], then `part_longest_us=…` the longest each
/// part took in one pump. Then `\nGPU LINUX timers=… works=… sleepers=… timeouts=…
/// steps=…` ([`linux_report`]).
pub(super) fn pump_report(tsc_hz: u64) -> Option<String> {
    let gpu = installed()?;
    let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
    let micros = |ticks: u64| u128::from(ticks) * 1_000_000 / u128::from(tsc_hz.max(1));
    let (by_event, by_deadline) = (take(&PUMPS.by_event), take(&PUMPS.by_deadline));
    let runs = by_event + by_deadline;
    let (ticks, delay) = (take(&PUMPS.ticks), take(&PUMPS.delay));
    let next = gpu.next.load(Ordering::Relaxed);
    let next_us = match next {
        u64::MAX => -1,
        next => micros(next.saturating_sub(host::rdtsc())) as i128,
    };
    let mut serving = String::new();
    for code in &PUMPS.step_serving {
        let code = code.swap(0, Ordering::Relaxed);
        if code != 0 {
            if !serving.is_empty() {
                serving.push('+');
            }
            serving.push_str(&format!("{:#x}", code.wrapping_sub(1)));
        }
    }
    if serving.is_empty() {
        serving.push('-');
    }
    let [device, pages, files, stacks] = UNMAPS.each_ref().map(take);
    let boot = take(&PUMPS.by_taker[Taker::Boot as usize]);
    let waiting = take(&PUMPS.by_taker[Taker::Waiting as usize]);
    #[cfg(feature = "nvk")]
    let nvkm = nvkm_report() + &perf_report();
    #[cfg(not(feature = "nvk"))]
    let nvkm = String::new();
    let pumps = format!(
        "GPU PUMP runs={runs} quiet={} unmasked={} forced={} relaxed={} posted={} by_event={by_event} \
         by_deadline={by_deadline} idle={} deferred={} mean_us={} longest_us={} \
         delay_mean_us={} delay_longest_us={} next_us={next_us} next_timer={} \
         step_longest_us={} step_shootdowns={} step_serving={serving} \
         unmaps={device}/{pages}/{files}/{stacks} by_boot={boot} by_waiting={waiting} \
         by_poster={} woken={} turn_busy={} wakes_held={} boot_delay_mean_us={} boot_delay_longest_us={} \
         waiting_delay_mean_us={} waiting_delay_longest_us={}",
        take(&PUMPS.quiet),
        take(&PUMPS.unmasked),
        take(&PUMPS.forced),
        take(&PUMPS.relaxed),
        take(&PUMPS.posted),
        take(&PUMPS.idle),
        take(&PUMPS.deferred),
        micros(ticks.checked_div(runs).unwrap_or(0)),
        micros(take(&PUMPS.longest)),
        micros(delay.checked_div(runs).unwrap_or(0)),
        micros(take(&PUMPS.delay_longest)),
        next_timer(),
        micros(take(&PUMPS.step_longest)),
        take(&PUMPS.step_shootdowns),
        take(&PUMPS.by_taker[Taker::Poster as usize]),
        take(&PUMPS.woken),
        take(&PUMPS.turn_busy),
        take(&PUMPS.wakes_held),
        micros(take(&PUMPS.taker_delay[0]).checked_div(boot).unwrap_or(0)),
        micros(take(&PUMPS.taker_delay_longest[0])),
        micros(
            take(&PUMPS.taker_delay[1])
                .checked_div(waiting)
                .unwrap_or(0)
        ),
        micros(take(&PUMPS.taker_delay_longest[1])),
    );
    // Each part's total over the pumps since the last report.
    let parts: Vec<String> = PUMPS
        .parts
        .iter()
        .map(|part| format!("{}", micros(take(part))))
        .collect();
    let longest: Vec<String> = PUMPS
        .part_longest
        .iter()
        .map(|part| format!("{}", micros(take(part))))
        .collect();
    let pumps = format!(
        "{pumps} total_us={names}/{} part_longest_us={names}/{}",
        parts.join("/"),
        longest.join("/"),
        names = PumpPart::NAMES.join("/"),
    );
    let linux = linux_report(take(&PUMPS.steps), tsc_hz);
    Some(pumps + &linux + &nvkm)
}

/// `compat/stela/counts.h`: `struct stela_linux_count_kind`.
#[repr(C)]
struct LinuxCountKind {
    /// Per slot the function and its count.
    slots: [[AtomicUsize; 2]; 32],
    other: AtomicUsize,
}

/// `compat/stela/counts.h`: `struct stela_linux_counts`.
#[repr(C)]
struct LinuxCounts {
    timers: LinuxCountKind,
    works: LinuxCountKind,
    sleepers: LinuxCountKind,
    timeouts: AtomicUsize,
}

/// `\nGPU LINUX timers=+0x…:N,… works=… sleepers=… timeouts=N steps=N`:
/// since the last report, the timer callbacks (timer-list and
/// high-resolution) and work items that ran in the environment, most first,
/// by function as an offset in this image (`other:N` for the rest), the tasks
/// a timeout woke by the work item or thread function they ran (`-` for
/// neither) and in all, and the Linux steps the pumps ran. Then
/// `step_us=select/body/after empty_us=N empties=N scans=N locks=N clocks=N`:
/// the work dispatchers' time (all runtimes) in steps that ran a task (entry to the switch, the
/// task's run, return to exit), its time and count of steps that found
/// nothing, its job-slot scans for idle tasks, its queue locks, and the
/// native clock's samples.
fn linux_report(steps: u64, tsc_hz: u64) -> String {
    /// Functions listed per kind.
    const TOP: usize = 4;
    unsafe extern "C" {
        /// `compat/kernel/counts.c`, which updates it only atomically.
        static stela_linux_counts: LinuxCounts;
        static __ImageBase: u8;
    }
    // SAFETY: counters the environment updates only atomically.
    let counts = unsafe { &stela_linux_counts };
    let base = &raw const __ImageBase as usize;
    let list = |kind: &LinuxCountKind| {
        let mut taken: Vec<(usize, usize)> = kind
            .slots
            .iter()
            .filter_map(|[function, count]| {
                let count = count.swap(0, Ordering::Relaxed);
                (count != 0).then(|| (function.load(Ordering::Relaxed), count))
            })
            .collect();
        taken.sort_unstable_by_key(|&(_, count)| core::cmp::Reverse(count));
        let other = kind.other.swap(0, Ordering::Relaxed)
            + taken
                .iter()
                .skip(TOP)
                .map(|&(_, count)| count)
                .sum::<usize>();
        let mut line = String::new();
        for &(function, count) in taken.iter().take(TOP) {
            if !line.is_empty() {
                line.push(',');
            }
            match function {
                // The counters' mark for no function.
                1 => line.push_str(&format!("-:{count}")),
                function => line.push_str(&format!("+{:#x}:{count}", function.wrapping_sub(base))),
            }
        }
        if other != 0 {
            if !line.is_empty() {
                line.push(',');
            }
            line.push_str(&format!("other:{other}"));
        }
        if line.is_empty() {
            line.push('-');
        }
        line
    };
    let micros = |ticks: u64| u128::from(ticks) * 1_000_000 / u128::from(tsc_hz.max(1));
    let step = stela_drivers::platform::work::step_counts::take();
    format!(
        "\nGPU LINUX timers={} works={} sleepers={} timeouts={} steps={steps} \
         step_us={}/{}/{} empty_us={} empties={} scans={} locks={} clocks={}",
        list(&counts.timers),
        list(&counts.works),
        list(&counts.sleepers),
        counts.timeouts.swap(0, Ordering::Relaxed),
        micros(step.select),
        micros(step.body),
        micros(step.after),
        micros(step.empty),
        step.empty_steps,
        step.scans,
        step.locks,
        crate::native_clock::SAMPLES.swap(0, Ordering::Relaxed),
    )
}

/// `\nGPU NVKM kind:count/total_us/longest_us …`: where nouveau's page-table
/// and instance-memory paths spent their time since the last report
/// (`include/nvkm/core/counts.h`), for kinds that ran, then
/// `\nGPU NVKM SIZES kind:count/total_kib/largest_kib …` for its page-table
/// references by page size and its sparse references. Empty when none ran.
#[cfg(feature = "nvk")]
fn nvkm_report() -> String {
    /// `enum nvkm_count`'s kinds, in order.
    const KINDS: [&str; 12] = [
        "flush",
        "pt_new",
        "pt_cached",
        "pt_free",
        "kmap",
        "evict",
        "kmap_slow",
        "rpc",
        "get_4k",
        "get_64k",
        "get_2m",
        "sparse",
    ];
    /// The first kind that counts bytes, not nanoseconds.
    const SIZES: usize = 8;
    unsafe extern "C" {
        /// `nvkm/subdev/mmu/base.c`: per kind a count, nanoseconds (or
        /// bytes) and the longest (or largest), which nvkm updates
        /// atomically.
        static nvkm_counts: [[AtomicU64; 3]; 12];
    }
    let mut times = String::new();
    let mut sizes = String::new();
    // SAFETY: nvkm's static counters, which it only updates atomically.
    let counts = unsafe { &nvkm_counts };
    for (index, (kind, [count, total, longest])) in KINDS.iter().zip(counts).enumerate() {
        let count = count.swap(0, Ordering::Relaxed);
        let total = total.swap(0, Ordering::Relaxed);
        let longest = longest.swap(0, Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        let (line, head, unit) = if index < SIZES {
            (
                &mut times,
                "\nGPU NVKM kind:count/total_us/longest_us",
                1000,
            )
        } else {
            (
                &mut sizes,
                "\nGPU NVKM SIZES kind:count/total_kib/largest_kib",
                1024,
            )
        };
        if line.is_empty() {
            line.push_str(head);
        }
        line.push_str(&format!(
            " {kind}:{count}/{}/{}",
            total / unit,
            longest / unit
        ));
    }
    times + &sizes
}

/// `\nGPU PERF pstate=… seen=… pcie=… rounds=…`: the GPU's P-state as GSP-RM
/// reported it and its PCIe link since the last report
/// (`stela_drivers::gpu::gsp_perf`), at the first report after GSP starts and
/// then when they changed. Empty otherwise.
#[cfg(feature = "nvk")]
fn perf_report() -> String {
    use stela_drivers::gpu::gsp_perf::{Report, Reported};
    unsafe extern "C" {
        /// `nvkm/subdev/gsp/rm/r535/perf.c`, which nouveau updates only
        /// atomically.
        static nvkm_perf: Report;
    }
    static REPORTED: Reported = Reported::new();
    // SAFETY: nouveau's static report, which it only updates atomically.
    let report = unsafe { &nvkm_perf };
    report
        .take()
        .and_then(|taken| REPORTED.line(&taken))
        .map(|line| format!("\n{line}"))
        .unwrap_or_default()
}

/// The function of the timer-list timer due first when Linux last looked,
/// as an offset in this image, or `-` for none.
fn next_timer() -> String {
    unsafe extern "C" {
        /// `compat/kernel/timer.c`: the first pending timer's function.
        static stela_linux_timer_next_function: AtomicUsize;
        static __ImageBase: u8;
    }
    // SAFETY: a word the environment stores whenever it reads its queue.
    let function = unsafe { stela_linux_timer_next_function.load(Ordering::Relaxed) };
    if function == 0 {
        return String::from("-");
    }
    let base = &raw const __ImageBase as usize;
    format!("+{:#x}", function.wrapping_sub(base))
}

#[cfg(feature = "nvk")]
/// The GPU interrupts taken when the Metal services last looked for
/// completed work.
static LOOKED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "nvk")]
/// A service a call held asked to look again.
static LOOK: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "nvk")]
/// When a pump next looks though nothing asked, as a TSC value.
static NEXT_LOOK: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "nvk")]
/// How long a pump goes without looking when the GPU has not interrupted: a
/// fence Linux signals outside its interrupt, as a submission or a job's
/// timeout may, is seen within this.
const LOOK_FALLBACK_NS: u64 = 10_000_000;

#[cfg(feature = "nvk")]
/// What a pump looks at after its steps ([`look_due`]).
enum Look {
    /// Each GPU's completion, which enters the driver, and the devices'
    /// new completion timelines.
    Device,
    /// Only the devices' new completion timelines, which need no driver.
    Waiters,
    No,
}

#[cfg(feature = "nvk")]
/// What this pump looks at: each look asks the driver for each GPU's
/// completion, which only an interrupt or a waiter's point makes likely to
/// have moved. Most pumps run for a call a PE posted or a Linux timer. The
/// fallback look every [`LOOK_FALLBACK_NS`] is for a device no waiter
/// watches: while one waits on the only device's timeline, every fence it
/// signals reaches the waiter, which asks for the look itself, so while
/// every device's waiter waits the fallback only starts the waiters of new
/// timelines.
fn look_due(gpu: &Installed) -> Look {
    let fired = FIRED.load(Ordering::Acquire);
    let interrupted = LOOKED.swap(fired, Ordering::Relaxed) != fired;
    let asked = LOOK.swap(false, Ordering::AcqRel);
    let now = host::rdtsc();
    let fallback = now >= NEXT_LOOK.load(Ordering::Relaxed);
    if interrupted || asked || fallback {
        NEXT_LOOK.store(
            now.saturating_add(gpu.tsc_after(LOOK_FALLBACK_NS)),
            Ordering::Relaxed,
        );
    }
    if interrupted || asked {
        Look::Device
    } else if !fallback {
        Look::No
    } else if waiter::watching(gate::shared().len()) {
        Look::Waiters
    } else {
        Look::Device
    }
}

#[cfg(feature = "nvk")]
/// Pumps the service again soon, from any processor, and has that pump look
/// for completed work.
fn look_again() {
    LOOK.store(true, Ordering::Release);
    if let Some(gpu) = installed() {
        // This raises a flag and calls the boot PE, so it cannot fail.
        let _ = gpu.events.notify();
    }
}

/// `ticks` of the host counter, cut short to end at Linux's next deadline,
/// or, while work is ready but held back for a calmer boot PE, when the
/// run loop next looks whether to pump ([`pump_due`]).
pub(super) fn slice(ticks: u64) -> u64 {
    let Some(gpu) = installed() else {
        return ticks;
    };
    let now = host::rdtsc();
    let end = match ready_since(gpu, now) {
        Some(since) => {
            let unmasked = since.saturating_add(gpu.tsc_after(DEFER_UNMASKED_NS));
            let longest = since.saturating_add(gpu.tsc_after(DEFER_LONGEST_NS));
            // Every recheck, so that a boot PE that reaches EL0 or waits
            // for an interrupt (calm, `pump_due`) pumps then rather than at
            // the unmasked deadline: a submission or completion waits a
            // quarter of a millisecond for a calm moment, not the whole one.
            let recheck = now.saturating_add(gpu.tsc_after(RECHECK_NS));
            if now < unmasked {
                recheck.min(unmasked)
            } else {
                recheck.min(longest)
            }
        }
        None => gpu.next.load(Ordering::Relaxed),
    };
    let left = end.saturating_sub(now);
    let left = u128::from(left) * u128::from(host::COUNTER_HZ) / u128::from(gpu.tsc_hz.max(1));
    ticks.min(left.min(u128::from(u64::MAX)) as u64).max(1)
}
