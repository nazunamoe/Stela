//! One vendor's GPU driver stack as data: the Linux kernel driver that binds
//! the PCI function, the Mesa Vulkan driver over it, and the few facts and
//! driver-internal calls Vulkan does not carry. Everything above and below
//! (the Linux environment, the DRM ioctl path, the Metal device, the AIR
//! compiler, the gate) is shared and reads a vendor only through this.
//!
//! [`GpuStack::select`] chooses by PCI vendor and the kernel drivers' own ID
//! tables, as Linux's driver core binds a function: to the first registered
//! driver whose table holds it. A vendor with two drivers lists them in
//! their registration order.
use crate::{Adapter, Vendor};
use alloc::string::String;
use core::ffi::c_void;

/// A device-control call on the bound GPU's open DRM file: the request code
/// and its argument, answering Linux's result (zero or a negative errno).
pub type Control<'a> = &'a mut dyn FnMut(u32, *mut c_void) -> i32;

/// The Linux kernel driver of a stack.
pub struct Kernel {
    /// The name its `pci_driver` registers under.
    pub driver: &'static str,
    /// Whether the driver's ID table holds the adapter, or why it does not.
    /// One it does not hold is left as firmware set it.
    pub admits: fn(&Adapter) -> Result<(), &'static str>,
    /// The BAR of the device's register window, whose first page the boot
    /// reads before the function's reset.
    pub registers: fn(&Adapter) -> usize,
    /// The BAR of device memory the driver maps whole as it starts (amdgpu's
    /// `ioremap_wc` of all visible VRAM, xe's of its LMEM BAR), which Stela's
    /// page window maps in 4 KiB pages whose tables the memory plan counts;
    /// `None` for a driver that maps device memory a buffer at a time.
    pub aperture: Option<u8>,
    /// Whether the driver's probe takes the GPU from the firmware itself,
    /// resetting it with the family's own reset where a previous driver left
    /// it running (amdgpu's `amdgpu_device_need_reset_on_init`), so that a
    /// function without a function-local reset is handed over with bus
    /// mastering off and no reset of the boot's, as Linux binds it.
    pub resets_itself: bool,
    /// The address bits of the DMA mask the driver sets on this device; a
    /// mapping past it is refused, not truncated.
    pub dma_bits: fn(&Adapter) -> u8,
    /// Whether the driver's own table describes the device as built into
    /// the processor, sharing its memory and on its fabric rather than a
    /// PCIe link. [`metal_devices`] reads it.
    pub integrated: fn(&Adapter) -> bool,
    /// Whether the bound device is built into the processor, by the driver's
    /// own query, for a driver that decides it during the probe (amdgpu's
    /// IP discovery). Where there is no such query, or it does not answer,
    /// [`Self::integrated`] stands. [`metal_devices`] reads the bound GPU's
    /// answer.
    pub probed_integrated: Option<fn(Control) -> Option<bool>>,
    /// For a driver that decides [`Self::probed_integrated`] during its
    /// probe, what the boot can tell before the memory plan, where no driver
    /// has probed yet.
    pub discovered_integrated: Option<Discovered>,
    /// The driver's own identifier of the bound chip, by its query ioctl.
    pub chip: fn(Control) -> Option<u32>,
    /// The bytes of device memory the driver makes available.
    pub memory_bytes: fn(Control) -> Option<u64>,
    /// For a driver whose recovery from a hung GPU holds back every later
    /// close of its files, how it tells whether it reset the GPU, so a GPU
    /// it reset during its first work is left unreleased.
    pub reset: Option<Reset>,
    /// What the driver says of the bound chip, by its query ioctls, for the
    /// boot log's one summary of the GPU: its family, the firmware versions
    /// it runs and the memory it found.
    pub describe: Option<fn(Control) -> Option<String>>,
    /// The driver's DRM version, its `struct drm_driver`'s major, minor and
    /// patchlevel, which the build reads from its source
    /// ([`crate::drm_version`]), so the boot names it without opening a file
    /// of the GPU; `None` where the build had no source to read.
    pub version: Option<(u32, u32, u32)>,
    /// Whether the driver reads its video BIOS from the platform ROM and the
    /// expansion ROM BAR where the GPU's own copy is not found, so its claim
    /// gives it the option ROM the firmware read (and ATRM's image) and
    /// places a ROM BAR the firmware left unassigned: amdgpu
    /// (`amdgpu_get_bios`) and the Intel display code xe and i915 build
    /// (`intel_bios_init`, its VBT through `pci_map_rom`). nouveau shadows its image from PRAMIN or
    /// PROM, and reaches the PCI ROM and the platform ROM only when those
    /// and ACPI give none (`nvbios_shadow`'s `skip`), so its claim does
    /// neither and gives it the VFCT image alone, as before.
    pub reads_rom: bool,
    /// Whether a fatal stop raised on the boot CPU, outside Linux, runs this
    /// driver's unload there before the machine resets: amdgpu, whose PSP
    /// ring and video decoder a reset leaves running, so the GPU's next
    /// start fails at its PSP ring. nouveau's unload waits on GSP-RM's
    /// replies and was never run beside the stopped work's own state on
    /// that processor, so a stop there leaves its firmware running, as
    /// every such stop did; a stop on another processor unloads it.
    pub unloads_on_boot_cpu: bool,
    /// Whether the heap's own need beside this driver and its Vulkan driver
    /// was measured (`host_memory::HEAP_OWN`, beside amdgpu and RADV), so
    /// that where no share is planned Linux's pages may be held to what the
    /// heap holds past it ([`pages_bounded`]). nouveau's pages beside NVK
    /// were not: its GSP-RM image and buffers, NVK's GART buffers (most of
    /// a GPU's mapped memory where its BAR 1 is not resized) and its
    /// evictions from VRAM keep the whole heap as Linux's RAM and TTM's own
    /// limit, half of it, as every boot gave them.
    pub pages_bounded: bool,
}

/// Whether the memory plan holds Linux's pages to what the heap holds past
/// its own need where no share is planned (`host_memory::GpuPlan::bounded`):
/// some GPU starts, and every one that does has a driver that need was
/// measured beside ([`Kernel::pages_bounded`]).
pub fn pages_bounded<'a>(kernels: impl IntoIterator<Item = &'a Kernel>) -> bool {
    let mut kernels = kernels.into_iter().peekable();
    kernels.peek().is_some() && kernels.all(|kernel| kernel.pages_bounded)
}

/// The driver among `kernels`, those of the bound GPUs, whose unload a
/// fatal stop on the boot CPU does not run there
/// ([`Kernel::unloads_on_boot_cpu`]): one is enough to leave every GPU's
/// firmware running, as the unload stops them all together.
pub fn kept_from_boot_cpu_unload<'a>(
    kernels: impl IntoIterator<Item = &'a Kernel>,
) -> Option<&'static str> {
    kernels
        .into_iter()
        .find(|kernel| !kernel.unloads_on_boot_cpu)
        .map(|kernel| kernel.driver)
}

/// The drivers' DRM versions as the build read them; an image without a
/// driver's port uses none of its.
#[allow(dead_code)]
mod versions {
    include!(concat!(env!("OUT_DIR"), "/drm_versions.rs"));
}

/// How a driver that decides whether a GPU is integrated during its probe
/// decides it, for the boot to tell before any probe.
#[derive(Clone, Copy)]
pub struct Discovered {
    /// Its kind as the driver's own table gives it for a device the table
    /// names (`AMD_IS_APU` on amdgpu's entries); `None` for one it takes by
    /// class and identifies by the device's IP discovery table.
    pub named: fn(&Adapter) -> Option<bool>,
    /// For one it identifies by its IP discovery table, whether the graphics
    /// block version that table lists (major, minor, revision) is one the
    /// driver flags built into the processor.
    pub graphics: fn((u8, u8, u8)) -> bool,
}

/// A kernel driver's watch for resets: a context on a file of its own,
/// made before the GPU's first work.
pub struct Reset {
    /// Makes the context, answering its identifier.
    pub watch: fn(Control) -> Option<u32>,
    /// Whether the driver reset the GPU since the context was made.
    pub state: fn(Control, u32) -> Option<ResetState>,
}

/// What a [`Reset`] watch saw.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetState {
    /// No reset since the watch was made.
    Running,
    /// The driver reset the GPU, and the reset ended.
    Reset,
    /// The driver is resetting the GPU now.
    Resetting,
}

impl ResetState {
    /// Why a lost device's reopen is refused, `None` where it may run: the
    /// driver still resets the GPU after [`ResetWait`] waited for it. The
    /// open of a new device, its first work and every close would wait for
    /// the reset's end, in the turn every client waits on.
    pub fn refuses_reopen(self) -> Option<&'static str> {
        (self == Self::Resetting).then_some(RESET_DID_NOT_END)
    }
}

/// The seconds a lost device's reopen waits for its kernel driver's reset
/// of the GPU to end: the driver's own job timeout (10 s for amdgpu's
/// graphics queues) and a full reset of the GPU with its firmware reloaded
/// fit well within it.
pub const RESET_WAIT_SECONDS: u64 = 30;

const RESET_DID_NOT_END: &str = "its kernel driver's reset of the GPU did not end within 30 s";

/// How long a lost device's reopen has waited for the driver's reset of
/// the GPU. While the reset runs, a reopen waits: no new device is opened,
/// the lost one stays, and the connection that asked says why. That is no
/// failed reopen. Once the reset ran [`RESET_WAIT_SECONDS`], the reopen
/// runs and is refused ([`ResetState::refuses_reopen`]), as any reopen that
/// fails.
#[derive(Debug, Default)]
pub struct ResetWait {
    since: Option<u64>,
}

impl ResetWait {
    /// Why the reopen waits now, `None` where it runs: `state` is what the
    /// watch answers at `now`, in ticks of `hz` a second.
    pub fn wait(&mut self, state: Option<ResetState>, now: u64, hz: u64) -> Option<&'static str> {
        if state != Some(ResetState::Resetting) {
            self.since = None;
            return None;
        }
        let since = *self.since.get_or_insert(now);
        (now.saturating_sub(since) < hz.saturating_mul(RESET_WAIT_SECONDS))
            .then_some("its kernel driver is still resetting the GPU")
    }
}

/// One vendor's stack.
pub struct GpuStack {
    pub vendor: Vendor,
    pub kernel: Kernel,
    /// Admission of the stack's Vulkan device as a Metal device. A stack
    /// without one drives displays only.
    #[cfg(feature = "metal")]
    pub metal: Option<crate::metal::open::Admission>,
    /// The Vulkan driver linked into the image. A stack without one drives
    /// displays only. Only the UEFI image links the driver archives: this
    /// crate's own test binary and every host binary that holds it with
    /// `metal-linked` (a workspace build with every feature) have none, so
    /// their descriptors name none of the archives' symbols.
    #[cfg(all(feature = "metal-linked", not(test), target_os = "uefi"))]
    pub vulkan: Option<crate::metal::native::Driver>,
}

impl Kernel {
    /// The widest DMA address the driver's mask allows `adapter`.
    pub fn dma_limit(&self, adapter: &Adapter) -> u64 {
        u64::MAX >> (64 - (self.dma_bits)(adapter))
    }

    /// Whether `adapter` is integrated as far as the driver's table says
    /// before its probe: `None` for a device the table does not flag of a
    /// driver that decides during its probe ([`Self::probed_integrated`]).
    pub fn table_integrated(&self, adapter: &Adapter) -> Option<bool> {
        match ((self.integrated)(adapter), self.probed_integrated) {
            (true, _) => Some(true),
            (false, Some(_)) => None,
            (false, None) => Some(false),
        }
    }
}

/// Every stack this build holds.
pub static STACKS: &[&GpuStack] = &[
    &NVIDIA,
    #[cfg(feature = "amdgpu-port")]
    &AMD,
    #[cfg(feature = "i915-port")]
    &INTEL_I915,
    #[cfg(feature = "xe-port")]
    &INTEL,
];

/// nouveau with GSP firmware, NVK and NAK.
pub static NVIDIA: GpuStack = GpuStack {
    vendor: Vendor::Nvidia,
    kernel: Kernel {
        driver: "nouveau",
        // nouveau's table holds every NVIDIA display function; what it does
        // with a chip it does not know is its probe's answer.
        admits: |_| Ok(()),
        registers: |_| 0,
        // nouveau maps BAR1 a buffer at a time (`nouveau_ttm_io_mem_reserve`).
        aperture: None,
        resets_itself: false,
        // The Turing-and-later MMUs' `dma_bits` (`nvkm/subdev/mmu/tu102.c`
        // 47, `gh100.c` 52), which `nvkm_device_pci_new` sets as the mask.
        dma_bits: |_| 47,
        // nouveau drives discrete GPUs only (`nvkm_device_pci_new`); Tegra's
        // built-in GPUs are platform devices, not PCI functions.
        integrated: |_| false,
        probed_integrated: None,
        discovered_integrated: None,
        chip: crate::nouveau_chip::query::chipset,
        memory_bytes: crate::nouveau_chip::query::memory_bytes,
        reset: None,
        describe: None,
        version: versions::NOUVEAU,
        reads_rom: false,
        unloads_on_boot_cpu: false,
        pages_bounded: false,
    },
    #[cfg(feature = "metal")]
    metal: Some(crate::metal::nvidia::ADMISSION),
    #[cfg(all(feature = "metal-linked", not(test), target_os = "uefi"))]
    vulkan: Some(crate::metal::nvidia::DRIVER),
};

/// amdgpu with its display core, RADV's admission and RADV, with ACO, as
/// its Vulkan driver when the image holds it ([`AMD_VULKAN`]). Whether its
/// GPU serves the Metal device is [`metal_devices`]' choice; otherwise, or
/// in an image built without RADV's tools, it shows the frames another GPU
/// draws on the displays wired to it.
#[cfg(feature = "amdgpu-port")]
pub static AMD: GpuStack = GpuStack {
    vendor: Vendor::Amd,
    kernel: Kernel {
        driver: "amdgpu",
        admits: crate::amdgpu::admits,
        // `rmmio_base` (`amdgpu_device.c`): BAR 5 for every family Stela
        // drives; BAR 0 is the memory aperture.
        registers: crate::amdgpu::registers,
        // `amdgpu_ttm_init`: `ioremap_wc(aper_base, visible_vram_size)`, BAR 0.
        aperture: Some(0),
        resets_itself: true,
        // The mask the device's memory controller block sets
        // (`gmc_v*_0.c`), or the narrowest of those IP discovery chooses.
        dma_bits: crate::amdgpu::dma_bits,
        // Its table flags only the older APUs it names by ID; IP discovery
        // flags the rest during the probe, after the claim, so the bound
        // device's answer decides for them.
        integrated: crate::amdgpu::integrated,
        probed_integrated: Some(crate::amdgpu::query::integrated),
        discovered_integrated: Some(Discovered {
            named: crate::amdgpu::named_integrated,
            graphics: crate::amdgpu::discovered_integrated,
        }),
        chip: crate::amdgpu::query::family,
        memory_bytes: crate::amdgpu::query::memory_bytes,
        // A hung GPU's recovery holds back the VM work every later close of
        // a file waits for, which on Navi 21 has not been seen to end.
        reset: Some(Reset {
            watch: crate::amdgpu::query::reset_watch,
            state: crate::amdgpu::query::reset_state,
        }),
        describe: Some(crate::amdgpu::query::describe),
        version: versions::AMDGPU,
        reads_rom: true,
        unloads_on_boot_cpu: true,
        pages_bounded: true,
    },
    #[cfg(feature = "metal")]
    metal: Some(crate::metal::amd::ADMISSION),
    #[cfg(all(feature = "metal-linked", not(test), target_os = "uefi"))]
    vulkan: AMD_VULKAN,
};

/// RADV, in an image whose Mesa build holds it.
#[cfg(all(
    feature = "amdgpu-port",
    feature = "metal-linked",
    not(test),
    target_os = "uefi",
    stela_vulkan_amd
))]
const AMD_VULKAN: Option<crate::metal::native::Driver> = Some(crate::metal::amd::DRIVER);
/// No Vulkan driver: an image built where RADV's tools were missing.
#[cfg(all(
    feature = "amdgpu-port",
    feature = "metal-linked",
    not(test),
    target_os = "uefi",
    not(stela_vulkan_amd)
))]
const AMD_VULKAN: Option<crate::metal::native::Driver> = None;

/// xe with the display code it shares with i915, ANV's admission and ANV
/// as its Vulkan driver when the image holds it. It drives Lunar Lake,
/// Battlemage, Panther Lake and later, the platforms upstream binds to xe by
/// default, and is the fallback of the devices i915 is handed first
/// ([`INTEL_I915`]). Whether its GPU serves the
/// Metal device is [`metal_devices`]' choice: an integrated GPU does when no
/// discrete GPU draws, and otherwise shows the frames another GPU draws on
/// the displays wired to it. An image built without ANV's tools holds no
/// ANV, and then its GPUs only show frames.
#[cfg(feature = "xe-port")]
pub static INTEL: GpuStack = GpuStack {
    vendor: Vendor::Intel,
    kernel: Kernel {
        driver: "xe",
        admits: crate::xe::admits,
        // GTTMMADR (`regs/xe_bars.h`): the registers, then the GTT.
        registers: |_| 0,
        // `xe_vram.c`: `devm_ioremap_wc(io_start, io_size)` of LMEMBAR, BAR 2.
        aperture: Some(2),
        resets_itself: false,
        // The platform's `dma_mask_size` (`xe_pci.c`), which
        // `xe_set_dma_info` sets as the mask.
        dma_bits: crate::xe::dma_bits,
        // The platform's `is_dgfx`.
        integrated: crate::xe::integrated,
        probed_integrated: None,
        discovered_integrated: None,
        chip: crate::xe::query::chip,
        memory_bytes: crate::xe::query::memory_bytes,
        reset: None,
        describe: Some(crate::xe::query::describe),
        version: versions::XE,
        reads_rom: true,
        unloads_on_boot_cpu: false,
        pages_bounded: false,
    },
    #[cfg(feature = "metal")]
    metal: Some(crate::metal::intel::ADMISSION),
    #[cfg(all(
        feature = "metal-linked",
        not(test),
        target_os = "uefi",
        stela_vulkan_intel
    ))]
    vulkan: Some(crate::metal::intel::DRIVER),
    #[cfg(all(
        feature = "metal-linked",
        not(test),
        target_os = "uefi",
        not(stela_vulkan_intel)
    ))]
    vulkan: None,
};

/// Why Linux did not start: its error, and the command-line word it
/// refused or the parameter that refused its value, when one did.
#[cfg(feature = "linux-drm")]
#[derive(Clone, Debug)]
pub struct StartError {
    pub errno: stela_linux::owner::Errno,
    pub rejected: Option<alloc::string::String>,
}

#[cfg(feature = "linux-drm")]
impl core::fmt::Display for StartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "errno {}", -self.errno)?;
        match &self.rejected {
            Some(word) => write!(f, ", refusing \"{word}\" on its command line"),
            None => Ok(()),
        }
    }
}

/// i915 with its display code. It registers before xe, so it binds the
/// devices of its table ANV opens that upstream Linux binds to i915 by
/// default, Skylake to Meteor Lake, DG1 and DG2 (`crate::i915::admits`); xe
/// binds one of them whose i915 probe fails, and the boot then takes xe's
/// stack for it ([`GpuStack::bound`]). ANV, over i915's uAPI, is its Vulkan
/// driver when the image holds it, and the Metal device admits its GPUs as
/// it admits xe's. Whether a GPU serves the Metal device is
/// [`metal_devices`]' choice, as for xe's.
#[cfg(feature = "i915-port")]
pub static INTEL_I915: GpuStack = GpuStack {
    vendor: Vendor::Intel,
    kernel: Kernel {
        driver: "i915",
        admits: crate::i915::admits,
        // GTTMMADR (`i915_reg.h` `GTTMMADR_BAR`), BAR 0 from Gen3 on.
        registers: |_| 0,
        // `i915_ggtt_init_hw`'s `io_mapping_init_wc` of the GGTT aperture,
        // or DG1's and DG2's LMEM, BAR 2: mapped whole, as the build has no
        // CONFIG_HAVE_ATOMIC_IOMAP.
        aperture: Some(2),
        resets_itself: false,
        // The platform's `dma_mask_size` (`i915_pci.c`), which
        // `i915_set_dma_info` sets as the mask.
        dma_bits: crate::i915::dma_bits,
        // The platform's `is_dgfx`.
        integrated: crate::i915::integrated,
        probed_integrated: None,
        discovered_integrated: None,
        chip: crate::i915::query::chip,
        memory_bytes: crate::i915::query::memory_bytes,
        reset: None,
        describe: Some(crate::i915::query::describe),
        version: versions::I915,
        // The display code's VBT falls back to the PCI ROM through
        // `intel_rom_pci` (`intel_bios.c`), as xe's does.
        reads_rom: true,
        // i915's unload waits on its GuC and engines; it was never run
        // beside a stopped processor's own state.
        unloads_on_boot_cpu: false,
        // The heap's need beside i915 and ANV was not measured.
        pages_bounded: false,
    },
    #[cfg(feature = "metal")]
    metal: Some(crate::metal::intel::I915_ADMISSION),
    #[cfg(all(
        feature = "metal-linked",
        not(test),
        target_os = "uefi",
        stela_vulkan_intel
    ))]
    vulkan: Some(crate::metal::intel::DRIVER),
    #[cfg(all(
        feature = "metal-linked",
        not(test),
        target_os = "uefi",
        not(stela_vulkan_intel)
    ))]
    vulkan: None,
};

/// Brings Linux up and registers every kernel driver built into the image,
/// stopping at the first stage that fails. Linux then matches each presented
/// function against the registered drivers' own ID tables, so one image
/// serves every vendor's devices and registration is not per stack.
/// `command_line` is Linux's: space-separated `module.parameter=value` words
/// that set the built-in drivers' parameters before any of them registers
/// ([`crate::command_line`]).
///
/// # Safety
/// On the canonical Linux owner's original managed task after Initial and
/// SRCU startup have completed, once per environment. Initcalls may sleep.
#[cfg(feature = "linux-drm")]
pub unsafe fn start_kernel(command_line: &str) -> Result<(), StartError> {
    unsafe extern "C" {
        /// The build's one registration image (`build/image.rs`).
        static stela_linux_gpu_image: c_void;
        fn stela_linux_start(
            image: *const c_void,
            command_line: *const u8,
            length: usize,
            registry: *mut *mut c_void,
            rejected: *mut u8,
            rejected_size: usize,
        ) -> core::ffi::c_int;
    }
    let mut registry = core::ptr::null_mut();
    let mut rejected = [0u8; 128];
    // SAFETY: the caller's contract; the image is the build's, the registry
    // copies the command line, and its state lasts as long as what it started.
    // The start writes at most `rejected.len()` bytes, NUL-terminated.
    match unsafe {
        stela_linux_start(
            &raw const stela_linux_gpu_image,
            command_line.as_ptr(),
            command_line.len(),
            &raw mut registry,
            rejected.as_mut_ptr(),
            rejected.len(),
        )
    } {
        0 => Ok(()),
        errno => Err(StartError {
            errno,
            rejected: core::ffi::CStr::from_bytes_until_nul(&rejected)
                .ok()
                .map(|word| word.to_string_lossy().into_owned())
                .filter(|word| !word.is_empty()),
        }),
    }
}

impl GpuStack {
    /// Whether this image holds the stack's Vulkan driver, so its GPU draws
    /// the Metal device's frames. Otherwise its kernel driver only shows
    /// frames another GPU drew, on the displays wired to it.
    pub fn draws(&self) -> bool {
        #[cfg(all(feature = "metal-linked", not(test), target_os = "uefi"))]
        return self.vulkan.is_some();
        #[cfg(not(all(feature = "metal-linked", not(test), target_os = "uefi")))]
        false
    }

    /// Where `adapter` comes among the GPUs claimed: drawing discrete GPUs
    /// first, then drawing GPUs whose kind only their probe tells, then
    /// drawing integrated ones, then those that only show frames, each group
    /// in the firmware's order (the sort is stable). The GPUs
    /// [`metal_devices`] prefers then have the first DRM minors, so a PC
    /// numbers them as it would without the others, and a GPU that may turn
    /// out integrated never comes before one known to be discrete.
    pub fn claim_order(&self, adapter: &Adapter) -> u8 {
        claim_rank(self.draws(), self.kernel.table_integrated(adapter))
    }

    /// The stack of the kernel driver Linux bound a GPU of `vendor` to, named
    /// as Linux names the driver: the claimed stack's, or another of its
    /// vendor's whose driver registered later and bound the device after the
    /// first one's probe failed (i915's devices, which xe binds then).
    pub fn bound(vendor: Vendor, driver: &str) -> Option<&'static GpuStack> {
        STACKS
            .iter()
            .copied()
            .find(|stack| stack.vendor == vendor && stack.kernel.driver == driver)
    }

    /// The stack for `adapter`: the first of its vendor's, in registration
    /// order, whose kernel driver holds the device. Otherwise why Stela
    /// drives it with none: the last of those drivers' answers.
    pub fn select(adapter: &Adapter) -> Result<&'static GpuStack, &'static str> {
        let mut refusal = None;
        for stack in STACKS
            .iter()
            .copied()
            .filter(|stack| stack.vendor == adapter.vendor)
        {
            match (stack.kernel.admits)(adapter) {
                Ok(()) => return Ok(stack),
                Err(why) => refusal = Some(why),
            }
        }
        Err(refusal.unwrap_or_else(|| adapter.unavailable_reason()))
    }
}

/// [`GpuStack::claim_order`] of a GPU that draws or not and is integrated,
/// discrete or not known before its probe.
pub(crate) fn claim_rank(draws: bool, integrated: Option<bool>) -> u8 {
    match (draws, integrated) {
        (true, Some(false)) => 0,
        (true, None) => 1,
        (true, Some(true)) => 2,
        (false, _) => 3,
    }
}

/// A bound GPU as the choice of Metal devices sees it: only what its stack
/// reports and where the display was taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Candidate {
    /// Its stack draws ([`GpuStack::draws`]): it admits the GPU's Vulkan
    /// device as a Metal device and the image holds that driver.
    pub draws: bool,
    /// Its kernel driver's [`Kernel::integrated`].
    pub integrated: bool,
    /// The connected display was taken on one of its outputs.
    pub drives_display: bool,
}

/// What [`metal_devices`] chose, by index into its candidates.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetalDevices {
    /// The GPUs that serve a Metal device, the PC's Metal device first.
    pub serve: alloc::vec::Vec<usize>,
    /// GPUs that draw but stay display-only, because a more capable GPU
    /// serves Metal.
    pub display_only: alloc::vec::Vec<usize>,
}

/// Which bound GPUs serve Metal devices. They are the drawing GPUs of the
/// most capable kind present: discrete GPUs, with memory and a link of their
/// own, before GPUs built into the processor. When a discrete GPU draws, an
/// integrated one only shows the displays wired to it, as a GPU without a
/// Vulkan driver does, and serves no second Metal device; an integrated GPU
/// alone serves the Metal device. Any drawing GPU feeds any display, since
/// each frame is copied into the buffer of the GPU that drives it, so among
/// equals the GPU that drives the connected display comes first and the
/// others keep the firmware's order.
pub fn metal_devices(candidates: &[Candidate]) -> MetalDevices {
    let drawing = || candidates.iter().enumerate().filter(|(_, gpu)| gpu.draws);
    let discrete = drawing().any(|(_, gpu)| !gpu.integrated);
    let (mut serve, display_only): (alloc::vec::Vec<_>, alloc::vec::Vec<_>) =
        drawing().partition(|(_, gpu)| !discrete || !gpu.integrated);
    serve.sort_by_key(|(_, gpu)| !gpu.drives_display);
    MetalDevices {
        serve: serve.into_iter().map(|(index, _)| index).collect(),
        display_only: display_only.into_iter().map(|(index, _)| index).collect(),
    }
}

/// The drawing GPU, among those already claimed, whose frames a GPU would
/// only show, if any, as [`metal_devices`] will choose: any drawing GPU for
/// one whose stack draws nothing, a discrete one for an integrated one.
/// `drawing` says of each claimed drawing GPU whether it is integrated;
/// claims take drawing GPUs first ([`GpuStack::claim_order`]). Stela shows
/// one display and a drawing GPU's outputs take it first, so such a GPU is
/// worth starting only where the firmware's picture may be on it.
fn shows_frames_of(drawing: &[bool], draws: bool, integrated: bool) -> Option<usize> {
    drawing
        .iter()
        .position(|&other_integrated| !draws || (integrated && !other_integrated))
}

/// The drawing GPU whose frames a GPU surely would only show
/// ([`shows_frames_of`]), so that it is left with the firmware unless the
/// firmware's picture may be on it. `integrated` is
/// [`Kernel::table_integrated`]: a GPU whose kind only its probe tells may
/// be discrete and serve a Metal device of its own, so it is claimed.
pub fn only_shows_frames_of(
    drawing: &[bool],
    draws: bool,
    integrated: Option<bool>,
) -> Option<usize> {
    shows_frames_of(drawing, draws, integrated.unwrap_or(false))
}

/// The drawing GPU whose frames a GPU may only show ([`shows_frames_of`]),
/// so that it is probed after the GPUs that serve Metal devices, under a
/// deadline of its own. `integrated` is [`Kernel::table_integrated`]: a GPU
/// whose kind only its probe tells may be integrated, so its probe waits.
pub fn may_only_show_frames_of(
    drawing: &[bool],
    draws: bool,
    integrated: Option<bool>,
) -> Option<usize> {
    shows_frames_of(drawing, draws, integrated.unwrap_or(true))
}

/// How a drawing GPU counts in the `drawing` list of
/// [`only_shows_frames_of`] and [`may_only_show_frames_of`]: one whose kind
/// only its probe tells counts as integrated, so it never leaves another
/// GPU out.
pub fn drawing_kind(integrated: Option<bool>) -> bool {
    integrated.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::pci::Address;
    use alloc::vec;

    #[test]
    fn a_gpu_shows_frames_only_beside_a_more_capable_drawing_gpu() {
        // Nothing claimed before it: it may be the only GPU.
        assert_eq!(shows_frames_of(&[], false, true), None);
        assert_eq!(shows_frames_of(&[], true, true), None);
        // An integrated GPU beside a drawing discrete one, drawing or not.
        assert_eq!(shows_frames_of(&[false], true, true), Some(0));
        assert_eq!(shows_frames_of(&[false], false, true), Some(0));
        // A display-only stack beside any drawing GPU, the first one.
        assert_eq!(shows_frames_of(&[true, false], false, false), Some(0));
        // Two drawing integrated GPUs, or a drawing discrete GPU beside
        // another: both serve Metal.
        assert_eq!(shows_frames_of(&[true], true, true), None);
        assert_eq!(shows_frames_of(&[false], true, false), None);
        // The discrete one is found behind an integrated one.
        assert_eq!(shows_frames_of(&[true, false], true, true), Some(1));
    }

    fn candidate(draws: bool, integrated: bool, drives_display: bool) -> Candidate {
        Candidate {
            draws,
            integrated,
            drives_display,
        }
    }

    #[test]
    fn an_integrated_gpu_alone_serves_the_metal_device() {
        let chosen = metal_devices(&[candidate(true, true, true)]);
        assert_eq!(chosen.serve, [0]);
        assert!(chosen.display_only.is_empty());
        // Also when its panel is dark and the display is elsewhere.
        assert_eq!(metal_devices(&[candidate(true, true, false)]).serve, [0]);
    }

    #[test]
    fn a_drawing_discrete_gpu_leaves_the_integrated_one_display_only() {
        // A muxless laptop: the panel on the integrated GPU, which draws too.
        let laptop = [candidate(true, false, false), candidate(true, true, true)];
        let chosen = metal_devices(&laptop);
        assert_eq!(chosen.serve, [0]);
        assert_eq!(chosen.display_only, [1]);
        // In either order of the firmware's enumeration.
        let chosen = metal_devices(&[laptop[1], laptop[0]]);
        assert_eq!(chosen.serve, [1]);
        assert_eq!(chosen.display_only, [0]);
    }

    #[test]
    fn a_gpu_that_draws_nothing_is_neither_served_nor_chosen_against() {
        // A discrete GPU without a Vulkan driver leaves the integrated GPU
        // the Metal device.
        let chosen = metal_devices(&[candidate(false, false, true), candidate(true, true, false)]);
        assert_eq!(chosen.serve, [1]);
        assert!(chosen.display_only.is_empty());
        // A display-only stack is no candidate of either list.
        let chosen = metal_devices(&[candidate(true, false, false), candidate(false, true, true)]);
        assert_eq!(chosen.serve, [0]);
        assert!(chosen.display_only.is_empty());
        assert_eq!(metal_devices(&[]), MetalDevices::default());
    }

    #[test]
    fn among_equals_the_gpu_driving_the_display_comes_first() {
        let desktop = [
            candidate(true, false, false),
            candidate(true, false, false),
            candidate(true, false, true),
            candidate(true, true, false),
        ];
        let chosen = metal_devices(&desktop);
        assert_eq!(chosen.serve, [2, 0, 1]);
        assert_eq!(chosen.display_only, [3]);
        // Without a display taken, the firmware's order stands.
        let headless = vec![candidate(true, false, false); 3];
        assert_eq!(metal_devices(&headless).serve, [0, 1, 2]);
    }

    fn adapter(vendor: u16) -> Adapter {
        Adapter {
            address: Address {
                segment: 0,
                bus: 1,
                device: 0,
                function: 0,
            },
            vendor: Vendor::from_pci(vendor),
            device: 0x1234,
            class: [3, 0, 0],
            revision: 0,
        }
    }

    #[test]
    fn a_stack_is_selected_by_pci_vendor_alone() {
        let nvidia = GpuStack::select(&adapter(0x10de)).unwrap();
        assert!(core::ptr::eq(nvidia, &NVIDIA));
        assert_eq!(nvidia.kernel.driver, "nouveau");
        for vendor in [
            #[cfg(not(feature = "amdgpu-port"))]
            0x1002,
            #[cfg(not(any(feature = "xe-port", feature = "i915-port")))]
            0x8086,
            // ASPEED's BMC display and a vendor no stack names.
            0x1a03,
            0x1234,
        ] {
            let adapter = adapter(vendor);
            assert_eq!(
                GpuStack::select(&adapter).err(),
                Some(adapter.unavailable_reason())
            );
        }
    }

    #[cfg(feature = "amdgpu-port")]
    #[test]
    fn amd_is_selected_for_what_amdgpus_probe_takes_and_draws_nothing() {
        // A GPU amdgpu's table does not name, found by its VGA class.
        let mut integrated = adapter(0x1002);
        integrated.device = 0x0001;
        let amd = GpuStack::select(&integrated).unwrap();
        assert!(core::ptr::eq(amd, &AMD));
        assert_eq!(amd.kernel.driver, "amdgpu");
        assert_eq!(amd.kernel.dma_limit(&integrated), (1 << 44) - 1);
        assert!(!amd.draws());
        #[cfg(feature = "metal")]
        assert!(amd.metal.is_some());
        // An audio function of the same vendor is no device of the table.
        let mut audio = adapter(0x1002);
        audio.class = [4, 3, 0];
        assert_eq!(
            GpuStack::select(&audio).err(),
            Some("amdgpu's PCI ID table does not hold this device")
        );
    }

    #[cfg(feature = "xe-port")]
    #[test]
    fn intel_is_selected_only_for_a_device_of_xes_table() {
        // Lunar Lake's Arc Graphics, then an ID no table holds.
        let mut lunar_lake = adapter(0x8086);
        lunar_lake.device = 0x64a0;
        let intel = GpuStack::select(&lunar_lake).unwrap();
        assert!(core::ptr::eq(intel, &INTEL));
        assert_eq!(intel.kernel.driver, "xe");
        assert_eq!(intel.kernel.dma_limit(&lunar_lake), (1 << 46) - 1);
        assert!((intel.kernel.integrated)(&lunar_lake));
        // This test binary links no Vulkan driver, ANV included.
        assert!(!intel.draws());
        #[cfg(feature = "metal")]
        assert!(intel.metal.is_some());
        // An Intel GPU no driver is handed has the answer of any GPU
        // without a driver.
        let unknown = adapter(0x8086);
        assert_eq!(
            GpuStack::select(&unknown).err(),
            Some(unknown.unavailable_reason())
        );
    }

    /// Upstream Linux's default per platform: i915 binds Skylake to Meteor
    /// Lake, DG1 and DG2, xe binds Lunar Lake, Battlemage, Panther Lake and
    /// the Data Center GPU Flex (which i915 binds only by force probe). The
    /// first registered driver whose table holds a device binds it, and i915
    /// registers first (`build/i915.rs` ordinal 33, `build/xe.rs` 36), so
    /// the claimed stack is i915's wherever i915 is handed the device, and
    /// xe's for the rest of its table. No device either table admits is left
    /// without a driver.
    #[cfg(all(feature = "xe-port", feature = "i915-port"))]
    #[test]
    fn intel_devices_go_to_i915_where_upstream_binds_it_then_to_xe() {
        let with = |device| Adapter {
            device,
            ..adapter(0x8086)
        };
        let (mut i915, mut xe, mut fallback) = (0, 0, 0);
        for &(device, ..) in crate::i915::DEVICES {
            let adapter = with(device);
            match GpuStack::select(&adapter) {
                Ok(stack) if core::ptr::eq(stack, &INTEL_I915) => {
                    assert_eq!(crate::i915::admits(&adapter), Ok(()));
                    i915 += 1;
                    // xe binds it if i915's probe fails, where its table
                    // holds it.
                    if crate::xe::admits(&adapter).is_ok() {
                        fallback += 1;
                    }
                }
                Ok(stack) => {
                    // Only a device i915 binds by force probe goes to xe.
                    assert!(core::ptr::eq(stack, &INTEL), "{device:04x}");
                    assert!(
                        crate::i915::platform(device).unwrap().forced,
                        "{device:04x}"
                    );
                }
                // Every Intel GPU no driver is handed has the one answer.
                Err(why) => assert_eq!(why, adapter.unavailable_reason(), "{device:04x}"),
            }
        }
        for &(device, ..) in crate::xe::DEVICES {
            let adapter = with(device);
            match GpuStack::select(&adapter) {
                Ok(stack) if core::ptr::eq(stack, &INTEL) => {
                    assert!(crate::i915::admits(&adapter).is_err(), "{device:04x}");
                    xe += 1;
                }
                Ok(stack) => assert!(core::ptr::eq(stack, &INTEL_I915), "{device:04x}"),
                Err(why) => {
                    assert!(crate::xe::admits(&adapter).is_err(), "{device:04x}");
                    assert!(crate::i915::admits(&adapter).is_err(), "{device:04x}");
                    assert_eq!(why, adapter.unavailable_reason());
                }
            }
        }
        // i915: Skylake to Ice Lake (118) and Tiger Lake to Meteor Lake, DG1
        // and DG2 (105), each of which xe takes if i915's probe fails; xe:
        // Lunar Lake, Battlemage, Panther Lake, Wildcat Lake and ATS-M.
        assert_eq!((i915, fallback, xe), (223, 105, 33));
        for (device, driver) in [
            (0x1916, "i915"),
            (0x8a56, "i915"),
            (0x9a49, "i915"),
            (0x46a6, "i915"),
            (0x56a0, "i915"),
            (0x7d55, "i915"),
            (0x56c0, "xe"),
            (0x64a0, "xe"),
            (0xe20b, "xe"),
            (0xb080, "xe"),
        ] {
            let stack = GpuStack::select(&with(device)).unwrap();
            assert_eq!(stack.kernel.driver, driver, "{device:04x}");
        }
        // The stack follows the driver that bound the device.
        assert!(core::ptr::eq(
            GpuStack::bound(Vendor::Intel, "xe").unwrap(),
            &INTEL
        ));
        assert!(core::ptr::eq(
            GpuStack::bound(Vendor::Intel, "i915").unwrap(),
            &INTEL_I915
        ));
        assert!(GpuStack::bound(Vendor::Intel, "nouveau").is_none());
        // This test binary links no Vulkan driver, ANV included.
        assert!(!INTEL_I915.draws());
        #[cfg(feature = "metal")]
        assert!(INTEL_I915.metal.is_some());
        // Broadwell, Haswell, i830, the IDs Mesa leaves out and Nova Lake,
        // which xe names no GuC image for, are no driver's.
        for device in [0x1616, 0x0416, 0x3577, 0x8a70, 0x4c80, 0xd750] {
            let adapter = with(device);
            assert_eq!(
                GpuStack::select(&adapter).err(),
                Some(adapter.unavailable_reason()),
                "{device:04x}"
            );
        }
    }

    #[test]
    fn each_driver_has_one_stack_and_a_usable_dma_mask() {
        for (index, stack) in STACKS.iter().enumerate() {
            assert!(
                STACKS[..index]
                    .iter()
                    .all(|other| other.kernel.driver != stack.kernel.driver)
            );
            let adapter = adapter(stack.vendor.pci_id());
            let bits = (stack.kernel.dma_bits)(&adapter);
            assert!((32..=64).contains(&bits));
            assert_eq!(
                stack.kernel.dma_limit(&adapter).count_ones(),
                u32::from(bits)
            );
            assert!(!stack.kernel.driver.is_empty());
        }
    }

    #[test]
    fn claims_put_drawing_discrete_gpus_first_then_integrated_then_display_only() {
        // (draws, integrated) in the firmware's order: an integrated GPU on
        // bus 0, a display-only GPU, a GPU only its probe tells the kind of
        // (an AMD GPU amdgpu matches by class), then a discrete GPU.
        let mut gpus = vec![
            (true, Some(true)),
            (false, Some(false)),
            (true, None),
            (true, Some(false)),
            (false, None),
        ];
        gpus.sort_by_key(|&(draws, integrated)| claim_rank(draws, integrated));
        assert_eq!(
            gpus,
            [
                (true, Some(false)),
                (true, None),
                (true, Some(true)),
                (false, Some(false)),
                (false, None)
            ]
        );
        // This test binary links no Vulkan driver, so its stacks come last.
        assert_eq!(NVIDIA.claim_order(&adapter(0x10de)), 3);
        assert_eq!(
            NVIDIA.kernel.table_integrated(&adapter(0x10de)),
            Some(false)
        );
    }

    #[cfg(feature = "amdgpu-port")]
    #[test]
    fn an_amd_gpu_matched_by_class_has_no_kind_before_its_probe() {
        // An iGPU beside a drawing NVIDIA GPU on a lower bus would otherwise
        // take DRM minor 0 and the first claim.
        let mut integrated = adapter(0x1002);
        integrated.device = 0x13c0;
        assert_eq!(AMD.kernel.table_integrated(&integrated), None);
        assert!(claim_rank(true, None) > claim_rank(true, Some(false)));
        // An APU the table names by ID and flags, Renoir, is integrated
        // before its probe; its registers are in BAR 5.
        integrated.device = 0x1636;
        assert_eq!(AMD.kernel.table_integrated(&integrated), Some(true));
        assert_eq!((AMD.kernel.registers)(&integrated), 5);
        // amdgpu resets its GPUs at its probe; nouveau and xe do not.
        assert!(AMD.kernel.resets_itself);
        assert!(
            STACKS
                .iter()
                .all(|stack| stack.kernel.resets_itself == (stack.vendor == Vendor::Amd))
        );
    }

    #[test]
    fn a_gpu_of_unknown_kind_beside_a_drawing_discrete_gpu_is_claimed_and_probed_after_it() {
        // amdgpu's class-matched GPU beside a drawing NVIDIA GPU: it may be
        // a discrete card that serves Metal too, so it is claimed, and it
        // may be the processor's GPU, so its probe waits for the NVIDIA one.
        assert_eq!(only_shows_frames_of(&[false], true, None), None);
        assert_eq!(may_only_show_frames_of(&[false], true, None), Some(0));
        // Alone, it is claimed and probed first.
        assert_eq!(only_shows_frames_of(&[], true, None), None);
        assert_eq!(may_only_show_frames_of(&[], true, None), None);
        // Without a Vulkan driver it only shows frames, whatever its kind.
        assert_eq!(only_shows_frames_of(&[false], false, None), Some(0));
        // Drawing ahead of an integrated GPU, it leaves that one claimed.
        let drawing = [drawing_kind(None)];
        assert_eq!(only_shows_frames_of(&drawing, true, Some(true)), None);
        assert_eq!(may_only_show_frames_of(&drawing, true, Some(true)), None);
        // A known kind decides both the same way.
        for integrated in [false, true] {
            assert_eq!(
                only_shows_frames_of(&[false], true, Some(integrated)),
                may_only_show_frames_of(&[false], true, Some(integrated))
            );
        }
    }

    #[test]
    fn a_stack_draws_only_with_a_vulkan_driver_in_the_image() {
        // This test binary links no Vulkan driver, whatever the features.
        assert!(!NVIDIA.draws());
        #[cfg(feature = "metal")]
        assert!(NVIDIA.metal.is_some());
    }

    #[test]
    fn nouveau_queries_ask_its_getparam_and_keep_refusals() {
        // `struct drm_nouveau_getparam`: the parameter, then its value.
        let mut asked = alloc::vec::Vec::new();
        let mut answer = |request: u32, argument: *mut c_void| {
            // SAFETY: the query passes its own two-word record.
            let words = unsafe { &mut *argument.cast::<[u64; 2]>() };
            asked.push((request, words[0]));
            words[1] = if words[0] == 11 { 0x1b3 } else { 16 << 30 };
            0
        };
        assert_eq!((NVIDIA.kernel.chip)(&mut answer), Some(0x1b3));
        assert_eq!((NVIDIA.kernel.memory_bytes)(&mut answer), Some(16 << 30));
        assert_eq!(asked, [(0xc010_6440, 11), (0xc010_6440, 8)]);
        let mut refuse = |_: u32, _: *mut c_void| -22;
        assert_eq!((NVIDIA.kernel.chip)(&mut refuse), None);
        assert_eq!((NVIDIA.kernel.memory_bytes)(&mut refuse), None);
        let mut empty = |_: u32, _: *mut c_void| 0;
        assert_eq!((NVIDIA.kernel.memory_bytes)(&mut empty), None);
        // nouveau's table answers alone, without a query.
        assert!(NVIDIA.kernel.probed_integrated.is_none());
    }

    #[cfg(feature = "amdgpu-port")]
    #[test]
    fn amdgpus_bound_device_says_whether_it_is_integrated() {
        // amdgpu's `ids_flags` with fusion: what IP discovery found.
        let mut fused = |_: u32, argument: *mut c_void| {
            // SAFETY: the query passes its record, whose answer buffer
            // holds the 18 words it asks for.
            unsafe {
                let record = &*argument.cast::<[u64; 4]>();
                *(record[0] as *mut u64).add(17) = 1;
            }
            0
        };
        let query = AMD.kernel.probed_integrated.unwrap();
        assert_eq!(query(&mut fused), Some(true));
        let mut refuse = |_: u32, _: *mut c_void| -19;
        assert_eq!(query(&mut refuse), None);
    }

    #[cfg(feature = "amdgpu-port")]
    #[test]
    fn amdgpu_watches_resets_on_a_context_of_its_own() {
        let reset = AMD.kernel.reset.as_ref().unwrap();
        assert!(NVIDIA.kernel.reset.is_none());
        // `union drm_amdgpu_ctx`: the operation, flags, context and priority
        // in; `alloc.ctx_id` or `state.flags` out over the first words.
        let mut asked = alloc::vec::Vec::new();
        let flags = core::cell::Cell::new(0u64);
        let mut answer = |request: u32, argument: *mut c_void| {
            // SAFETY: the query passes its own four-word record.
            let words = unsafe { &mut *argument.cast::<[u32; 4]>() };
            asked.push((request, words[0], words[2]));
            match words[0] {
                1 => words[0] = 7,
                _ => [words[0], words[1]] = [flags.get() as u32, (flags.get() >> 32) as u32],
            }
            0
        };
        assert_eq!((reset.watch)(&mut answer), Some(7));
        assert_eq!((reset.state)(&mut answer, 7), Some(ResetState::Running));
        flags.set(1);
        assert_eq!((reset.state)(&mut answer, 7), Some(ResetState::Reset));
        flags.set(1 | 1 << 5);
        assert_eq!((reset.state)(&mut answer, 7), Some(ResetState::Resetting));
        assert_eq!(asked[..2], [(0xc010_6442, 1, 0), (0xc010_6442, 4, 7)]);
        // A lost device's reopen waits while the reset runs, at most
        // RESET_WAIT_SECONDS, and runs once it ended.
        let mut wait = ResetWait::default();
        let resetting = Some(ResetState::Resetting);
        assert!(wait.wait(resetting, 100, 10).is_some());
        assert!(wait.wait(resetting, 100 + 299, 10).is_some());
        assert_eq!(wait.wait(resetting, 100 + 300, 10), None);
        // A reopen past the wait is refused while the reset still runs.
        assert_eq!(
            ResetState::Resetting.refuses_reopen(),
            Some(RESET_DID_NOT_END)
        );
        assert!(RESET_DID_NOT_END.contains(&alloc::format!("{RESET_WAIT_SECONDS} s")));
        for ended in [ResetState::Running, ResetState::Reset] {
            assert_eq!(wait.wait(Some(ended), 1000, 10), None);
            assert_eq!(ended.refuses_reopen(), None);
        }
        // A later reset waits from its own start; a watch that answers
        // nothing holds nothing back; without a clock no wait is made.
        assert!(wait.wait(resetting, 5000, 10).is_some());
        assert_eq!(wait.wait(None, 5001, 10), None);
        assert_eq!(ResetWait::default().wait(resetting, 7, 0), None);
        let mut refuse = |_: u32, _: *mut c_void| -22;
        assert_eq!((reset.watch)(&mut refuse), None);
        assert_eq!((reset.state)(&mut refuse, 7), None);
    }

    /// A claim touches the expansion ROM and hands over the firmware's
    /// option ROM only for a driver that reads its video BIOS there; the
    /// released NVIDIA stack's claim does neither, as before.
    #[test]
    fn only_a_driver_that_reads_its_rom_gets_the_rom_bar_and_the_option_rom() {
        assert!(!NVIDIA.kernel.reads_rom);
        #[cfg(feature = "amdgpu-port")]
        assert!(AMD.kernel.reads_rom);
        #[cfg(feature = "xe-port")]
        assert!(INTEL.kernel.reads_rom);
    }

    /// A stop on the boot CPU unloads the GPUs there only when every bound
    /// GPU's driver unloads from such a stop: the released NVIDIA stack
    /// keeps its firmware running there, as before, an AMD GPU alone is
    /// unloaded, and an AMD GPU beside an NVIDIA one is not.
    #[test]
    fn a_stop_on_the_boot_cpu_unloads_only_drivers_shown_to_unload_there() {
        assert_eq!(kept_from_boot_cpu_unload(core::iter::empty()), None);
        assert_eq!(kept_from_boot_cpu_unload([&NVIDIA.kernel]), Some("nouveau"));
        #[cfg(feature = "amdgpu-port")]
        {
            assert_eq!(kept_from_boot_cpu_unload([&AMD.kernel]), None);
            assert_eq!(
                kept_from_boot_cpu_unload([&AMD.kernel, &NVIDIA.kernel]),
                Some("nouveau")
            );
        }
        #[cfg(feature = "xe-port")]
        assert_eq!(kept_from_boot_cpu_unload([&INTEL.kernel]), Some("xe"));
    }

    /// Linux's pages are held to the heap's spare only where every GPU that
    /// starts has a driver the heap's own need was measured beside: an
    /// NVIDIA GPU, alone or beside an AMD one, keeps what every boot gave
    /// it, and with no GPU there is nothing to bound.
    #[test]
    fn pages_are_bounded_only_beside_drivers_the_heap_was_measured_with() {
        assert!(!pages_bounded(core::iter::empty()));
        assert!(!pages_bounded([&NVIDIA.kernel]));
        #[cfg(feature = "amdgpu-port")]
        {
            assert!(pages_bounded([&AMD.kernel]));
            assert!(pages_bounded([&AMD.kernel, &AMD.kernel]));
            assert!(!pages_bounded([&NVIDIA.kernel, &AMD.kernel]));
        }
        #[cfg(feature = "xe-port")]
        assert!(!pages_bounded([&INTEL.kernel]));
    }

    /// Every stack names its driver's DRM version without a client: the
    /// build read it from the driver's source, which this checkout holds.
    #[test]
    fn each_drivers_version_is_known_without_opening_its_gpu() {
        for stack in STACKS {
            let (major, minor, patch) = stack
                .kernel
                .version
                .unwrap_or_else(|| panic!("{}'s version", stack.kernel.driver));
            assert!(
                major >= 1,
                "{} {major}.{minor}.{patch}",
                stack.kernel.driver
            );
        }
    }
}
