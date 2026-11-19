//! The processors that run no PE, started once into an idle loop of their
//! own (`stela_boot_core::parking`): CR4.MCE from the startup trampoline,
//! the engine's descriptors, whose #MC and NMI gates stop the boot with the
//! fatal report, interrupts masked, and the deepest MWAIT its CPUID offers
//! or HLT. A parked processor does no host service, joins no shootdown and
//! takes no interrupt: nothing but NMI, SMI, INIT or a machine check ends
//! its wait.
//!
//! The parking fails safe. A processor that does not reach its loop within a
//! second, or faults on the way, may still read the startup page, so no
//! other processor starts from it: it and the processors after it stay in
//! wait-for-SIPI as before, the boot goes on and the log line says so.
//!
//! Before the firmware's AML first runs, every processor but the boot
//! processor is parked the same way ([`park_before_firmware`]). In INIT's
//! wait-for-SIPI state a processor does not take an SMI: the firmware's
//! system management code then finds it absent, and a handler that waits
//! for every processor to arrive waits until its own timeout or for good,
//! with the boot processor inside it, where no watchdog of Stela's can
//! act. Linux runs the firmware's AML with every other processor in the
//! firmware's own idle loop or online, never in wait-for-SIPI. A PE's
//! processor parked there is started from the page again later, by INIT
//! and SIPI, as the startup always does; one that does not park is sent
//! INIT again, so it reads nothing, and stays in wait-for-SIPI. One that
//! takes no INIT either holds the page: the startup sends it INIT once more
//! and, when that fails too, starts no processor from the page and boots on
//! the boot processor alone ([`release_page`]).
use super::super::{bootstrap, firmware, memory_types::Image, topology};
use crate::{
    apic::{self, Apic},
    engine::interrupts,
    handoff::Handoff,
    layout,
};
use alloc::{boxed::Box, vec, vec::Vec};
use core::{
    arch::{
        asm,
        x86_64::{__cpuid, CpuidResult},
    },
    cell::UnsafeCell,
    sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};
use stela_boot_core::parking::{Census, Early, Holder, Idle, IdleCpuid, Left, Page, UNPUBLISHED};
use stela_engine::machine::mmu::placement::Placement;

/// A parked processor's stack: its descriptor installation runs on it, then
/// the idle loop. Exceptions run on the descriptors' own stacks.
const STACK_BYTES: usize = 32 * 1024;

const STARTING: u8 = 0;
const PARKED: u8 = 1;

/// What one parked processor takes at entry. Kept with its tables and stack
/// while it may run there, which an NMI or machine check still enters: for
/// the whole boot, or until it runs a PE on its own ([`reclaim`]).
struct Parking {
    tables: UnsafeCell<Option<&'static mut interrupts::Tables>>,
    /// The boot processor runs x2APIC, which this one then takes too.
    x2apic: bool,
    status: AtomicU8,
    /// Its wait (`Idle::encode`).
    idle: AtomicU32,
}

// SAFETY: the boot processor fills `tables` and `x2apic` before the SIPI;
// only the parked processor takes `tables`, once. The rest are atomics.
unsafe impl Sync for Parking {}

/// A parked processor's heap allocations, never dropped in place: freeing
/// one gives the memory back without running anything of it.
struct Resources {
    stack: *mut [u8],
    tables: *mut core::mem::ManuallyDrop<interrupts::Tables>,
    parking: *mut Parking,
}

/// The line MONITOR arms. Nothing writes it.
#[repr(align(64))]
struct Line([u8; 64]);
static LINE: Line = Line([0; 64]);

/// Any exception on a parked processor stops the boot: it runs nothing a
/// fault could be resumed into.
extern "sysv64" fn interrupt(frame: &mut interrupts::Frame, _: &interrupts::SavedState<'_>) {
    interrupts::fatal(frame)
}

/// This processor's CPUID facts for its wait.
fn cpuid() -> IdleCpuid {
    let identity = __cpuid(0);
    let leaf1 = __cpuid(1);
    let leaf5 = match identity.eax >= 5 {
        true => __cpuid(5),
        false => CpuidResult {
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        },
    };
    IdleCpuid {
        // "GenuineIntel".
        intel: (identity.ebx, identity.edx, identity.ecx)
            == (0x756e_6547, 0x4965_6e69, 0x6c65_746e),
        max_leaf: identity.eax,
        leaf1_ecx: leaf1.ecx,
        leaf1_edx: leaf1.edx,
        leaf5_ecx: leaf5.ecx,
        leaf5_edx: leaf5.edx,
    }
}

unsafe extern "sysv64" fn enter(argument: u64) -> ! {
    // SAFETY: the boot processor passed this processor one leaked Parking.
    let parking = unsafe { &*(argument as *const Parking) };
    // SAFETY: this processor alone takes its tables, once.
    let tables = unsafe {
        (&mut *parking.tables.get())
            .take()
            .expect("parked processor's descriptors")
    };
    // SAFETY: interrupts are masked since the trampoline; the tables are
    // this processor's and leaked, and nothing used the trampoline's own.
    unsafe { interrupts::install_parked(tables, interrupt) };
    if parking.x2apic {
        // SAFETY: CPL0 with interrupts masked; this processor makes no APIC
        // handle.
        unsafe { apic::prefer_x2apic() };
    }
    let idle = stela_boot_core::parking::idle(cpuid());
    parking.idle.store(idle.encode(), Ordering::Relaxed);
    parking.status.store(PARKED, Ordering::Release);
    loop {
        match idle {
            Idle::Mwait { hint } => {
                // SAFETY: MONITOR arms a write-back line of the image that
                // nothing writes. MWAIT without interrupt break (ECX 0) and
                // with interrupts masked returns only for NMI, SMI, INIT or
                // a machine check, whose gates are installed.
                unsafe {
                    asm!("monitor", in("rax") LINE.0.as_ptr(), in("ecx") 0, in("edx") 0,
                        options(nostack));
                    asm!("mwait", in("eax") hint, in("ecx") 0, options(nostack));
                }
            }
            Idle::Halt => {
                // SAFETY: with interrupts masked HLT returns only for NMI,
                // SMI, INIT or a machine check, whose gates are installed.
                unsafe { asm!("hlt", options(nomem, nostack)) }
            }
        }
    }
}

/// Start every enabled processor that runs no PE into [`enter`], one at a
/// time from the startup page, and log the census
/// (`stela_boot_core::parking::Census`). Nothing is started after a PE's
/// processor failed (`startup_failed`), on a boot without a startup page,
/// or after a processor that did not park.
///
/// # Safety
/// As `executor::start`, after every PE's processor became ready: no
/// processor reads the startup page. `memory_types` and `pat` are the boot
/// processor's checked memory types.
pub(super) unsafe fn park(
    handoff: &Handoff,
    capture: &firmware::Capture,
    sender: &Apic,
    memory_types: &Image,
    pat: u64,
    startup_failed: bool,
) {
    let cores = handoff.native_resources.cores as usize;
    let processors = &capture.processors[..capture.processor_count as usize];
    let Ok(unused) = topology::enabled_unselected(processors, &capture.selected[..cores]) else {
        return;
    };
    let mut census = Census {
        unused: unused.clone().count(),
        ..Census::default()
    };
    census.left = match (startup_failed || page_held(), handoff.native_resources.sipi) {
        (true, _) => Some(Left::StartupFailed),
        (false, 0) => Some(Left::NoStartupPage),
        _ => None,
    };
    let early = early();
    let mut flush = true;
    if census.left.is_none() {
        for index in unused {
            // Parked before the firmware's AML, and not started since.
            if let Some(idle) = early.get(index).and_then(|&word| Idle::decode(word)) {
                census.parked(idle);
                continue;
            }
            let processor = processors[index];
            let Some(destination) = u32::try_from(processor.apic_id)
                .ok()
                .filter(|&id| sender.validate_init(id, handoff.tsc_hz).is_ok())
            else {
                census.unreachable += 1;
                continue;
            };
            // SAFETY: the caller's; the previous processor started from
            // the page parked, so none reads it.
            match unsafe {
                park_one(
                    handoff,
                    capture,
                    sender,
                    memory_types,
                    pat,
                    index,
                    destination,
                    core::mem::take(&mut flush),
                )
            } {
                // It stays parked on them for the boot.
                Some((idle, _)) => census.parked(idle),
                None => {
                    census.left = Some(Left::ParkFailed {
                        apic: processor.apic_id,
                    });
                    break;
                }
            }
        }
    }
    crate::output::arguments(format_args!("{census}\n"));
}

/// The other processors [`park_before_firmware`] parked, and those it left
/// in wait-for-SIPI.
pub(super) static OTHERS_PARKED: AtomicU32 = AtomicU32::new(0);
pub(super) static OTHERS_WAITING: AtomicU32 = AtomicU32::new(0);

/// A processor that did not park before the firmware's AML and did not take
/// INIT again either, which may still read the startup page: its firmware
/// number above its APIC ID, or [`NO_HOLDER`].
static HOLDER: AtomicU64 = AtomicU64::new(NO_HOLDER);
const NO_HOLDER: u64 = u64::MAX;

/// Whether the startup page may still be read by a processor that did not
/// park before the firmware's AML.
pub(super) fn page_held() -> bool {
    HOLDER.load(Ordering::Acquire) != NO_HOLDER
}

/// Send the processor that holds the startup page, if one does, INIT once
/// more, and log what came of it: the page is free when it took it. When it
/// did not, the page stays held, no processor starts from it, and the boot
/// goes on with the boot processor alone (`Page::Held`).
///
/// # Safety
/// On the boot processor, which alone sends interprocessor interrupts, with
/// interrupts masked, before any processor starts from the startup page.
pub(super) unsafe fn release_page(sender: &Apic, tsc_hz: u64) -> Page {
    let word = HOLDER.load(Ordering::Acquire);
    let holder = (word != NO_HOLDER).then_some(Holder {
        apic: word as u32,
        firmware_cpu: (word >> 32) as u32,
    });
    let page = stela_boot_core::parking::release(holder, |apic| {
        // SAFETY: a destination validated before its parking; the caller's.
        unsafe { sender.init_only(apic, tsc_hz) }.is_ok()
    });
    if let Page::Released(_) = page {
        HOLDER.store(NO_HOLDER, Ordering::Release);
    }
    if page != Page::Free {
        crate::output::arguments(format_args!("{page}\n"));
    }
    page
}

/// Each processor's wait from [`park_before_firmware`] (`Idle::encode`), by
/// its index in the capture, `UNPUBLISHED` where it did not park then;
/// empty before.
static EARLY: (AtomicPtr<u32>, AtomicUsize) =
    (AtomicPtr::new(core::ptr::null_mut()), AtomicUsize::new(0));

/// What each processor [`park_before_firmware`] parked runs on, until
/// [`reclaim`] gives it back; null before.
static EARLY_RESOURCES: AtomicPtr<Early<Resources>> = AtomicPtr::new(core::ptr::null_mut());

/// Free what processor `index` of the capture ran on while parked before
/// the firmware's AML, when it did.
///
/// # Safety
/// On the boot processor, after processor `index` took INIT and SIPI again
/// and reported ready on its own stack and descriptors: it never runs on
/// what it parked on again.
pub(super) unsafe fn reclaim(index: usize) {
    let early = EARLY_RESOURCES.load(Ordering::Acquire);
    if early.is_null() {
        return;
    }
    // SAFETY: leaked by `park_before_firmware`; only the boot processor
    // reaches it, one call at a time.
    let Some(resources) = unsafe { &mut *early }.started(index) else {
        return;
    };
    // SAFETY: each a leaked box of this type, which the caller's processor no
    // longer runs on and nothing else reaches; the tables are freed as
    // they are, without dropping their contents.
    unsafe {
        drop(Box::from_raw(resources.stack));
        drop(Box::from_raw(resources.tables));
        drop(Box::from_raw(resources.parking));
    }
}

fn early() -> &'static [u32] {
    let (pointer, length) = (
        EARLY.0.load(Ordering::Acquire),
        EARLY.1.load(Ordering::Relaxed),
    );
    if pointer.is_null() {
        return &[];
    }
    // SAFETY: `park_before_firmware` leaked this slice, written once.
    unsafe { core::slice::from_raw_parts(pointer, length) }
}

/// Park every enabled processor but the boot processor before the
/// firmware's AML runs, so each answers an SMI (module documentation),
/// and log how many did, by their wait. `progress` is called before each
/// processor's bounded wait.
///
/// # Safety
/// On the boot processor in the engine's root, its host installed, with
/// interrupts masked, before any processor starts from the startup page
/// and before the firmware's AML runs. `memory_types` and `pat` are the
/// boot processor's.
pub(super) unsafe fn park_before_firmware(
    handoff: &Handoff,
    capture: &firmware::Capture,
    sender: &Apic,
    memory_types: &Image,
    pat: u64,
    progress: fn(),
) {
    let cores = handoff.native_resources.cores as usize;
    let processors = &capture.processors[..capture.processor_count as usize];
    let Some(boot) = capture.selected.get(..cores.max(1)) else {
        return;
    };
    let Ok(others) = topology::enabled_unselected(processors, &boot[..1]) else {
        return;
    };
    let mut census = Census {
        unused: others.clone().count(),
        ..Census::default()
    };
    let mut waits: Vec<u32> = vec![UNPUBLISHED; processors.len()];
    let mut kept = Early::new(processors.len());
    let mut flush = true;
    if handoff.native_resources.sipi == 0 {
        census.left = Some(Left::NoStartupPage);
    } else {
        for index in others {
            // Each processor's wait is bounded on its own (a second).
            progress();
            let processor = processors[index];
            let Some(destination) = u32::try_from(processor.apic_id)
                .ok()
                .filter(|&id| sender.validate_init(id, handoff.tsc_hz).is_ok())
            else {
                census.unreachable += 1;
                continue;
            };
            // SAFETY: the caller's; every processor started from the page
            // before parked, or was sent INIT again below.
            match unsafe {
                park_one(
                    handoff,
                    capture,
                    sender,
                    memory_types,
                    pat,
                    index,
                    destination,
                    core::mem::take(&mut flush),
                )
            } {
                Some((idle, resources)) => {
                    census.parked(idle);
                    waits[index] = idle.encode();
                    kept.parked(index, resources);
                }
                None => {
                    census.left = Some(Left::ParkFailed {
                        apic: processor.apic_id,
                    });
                    // It may still read the startup page: INIT puts it back
                    // in wait-for-SIPI, where it reads nothing. Without that
                    // no other processor starts from the page here.
                    // SAFETY: a validated destination; the BSP alone sends.
                    if unsafe { sender.init_only(destination, handoff.tsc_hz) }.is_err() {
                        HOLDER.store(
                            u64::from(processor.processor_number) << 32 | u64::from(destination),
                            Ordering::Release,
                        );
                        break;
                    }
                }
            }
        }
    }
    OTHERS_PARKED.store((census.mwait + census.halted) as u32, Ordering::Relaxed);
    OTHERS_WAITING.store(
        (census.waiting() + census.unreachable) as u32,
        Ordering::Relaxed,
    );
    EARLY_RESOURCES.store(Box::into_raw(Box::new(kept)), Ordering::Release);
    let waits = Box::leak(waits.into_boxed_slice());
    EARLY.1.store(waits.len(), Ordering::Relaxed);
    EARLY.0.store(waits.as_mut_ptr(), Ordering::Release);
    crate::output::arguments(format_args!(
        "NATIVE PARKED BEFORE FIRMWARE {} of {} other processors, so each answers the \
         firmware's SMIs while its AML runs: mwait={} hlt={} wait_for_sipi={}{}\n",
        census.mwait + census.halted,
        census.unused,
        census.mwait,
        census.halted,
        census.waiting(),
        match census.left {
            None => alloc::string::String::new(),
            Some(Left::NoStartupPage) => alloc::string::String::from(" (no startup page)"),
            Some(Left::ParkFailed { apic }) => {
                alloc::format!(" (processor {apic:#x} did not reach its idle loop)")
            }
            Some(Left::StartupFailed) => alloc::string::String::new(),
        }
    ));
}

/// Start processor `index`, APIC ID `destination`, into [`enter`] and wait
/// a second at most for its wait; `None` when it did not park, which leaves
/// what it may run on leaked. `flush` stores the log before the startup,
/// once for a pass: a reset during the first startup leaves its line last.
///
/// # Safety
/// As [`park`], and no processor reads the startup page.
#[allow(clippy::too_many_arguments)]
unsafe fn park_one(
    handoff: &Handoff,
    capture: &firmware::Capture,
    sender: &Apic,
    memory_types: &Image,
    pat: u64,
    index: usize,
    destination: u32,
    flush: bool,
) -> Option<(Idle, Resources)> {
    let processor = capture.processors[index];
    // The trampoline checks an AMD processor's signature: its own where
    // the firmware's snapshot has one, else the boot processor's.
    let slot = capture.selected[..capture.core_count as usize]
        .iter()
        .position(|&selected| selected as usize == index)
        .unwrap_or(0);
    let signature = capture.snapshots[slot].get(1, 0)?.eax;
    let stack = Box::into_raw(vec![0u8; STACK_BYTES].into_boxed_slice());
    let stack_top = (stack as *mut u8 as u64 + STACK_BYTES as u64) & !15;
    let tables = Box::into_raw(Box::new(core::mem::ManuallyDrop::new(
        interrupts::Tables::new(),
    )));
    let parking = Box::into_raw(Box::new(Parking {
        // SAFETY: a fresh box, which only the parked processor reaches.
        tables: UnsafeCell::new(Some(unsafe { &mut **tables })),
        x2apic: handoff.apic_base & apic::X2APIC_MODE != 0,
        status: AtomicU8::new(STARTING),
        idle: AtomicU32::new(UNPUBLISHED),
    }));
    // SAFETY: freed only by `reclaim`, once the processor runs elsewhere.
    let parking: &'static Parking = unsafe { &*parking };
    let physical = handoff.native_resources.sipi;
    crate::output::arguments(format_args!(
        "NATIVE PARK apic={destination:#x} firmware_cpu={}\n",
        processor.processor_number,
    ));
    if flush {
        crate::output::flush_log();
    }
    // SAFETY: the caller's: the page is owned and no processor reads it; the
    // stack, argument and tables are leaked heap in the shared physmap,
    // which `handoff.root` maps.
    let vector = unsafe {
        bootstrap::prepare(
            layout::physical(physical) as *mut u8,
            physical,
            bootstrap::Entry {
                processor_signature: signature,
                root: handoff.root,
                five_level: matches!(handoff.placement, Placement::FiveLevel),
                pat,
                memory_types,
                stack_top,
                argument: parking as *const Parking as u64,
                entry: enter,
            },
        )
    }
    .ok()?;
    // SAFETY: a validated destination, and complete startup resources that
    // stay for the whole boot.
    unsafe { sender.startup(destination, vector, handoff.tsc_hz) }.ok()?;
    let since = apic::rdtsc();
    loop {
        // SAFETY: the startup page stays this processor's until it parks.
        match unsafe { bootstrap::failure(layout::physical(physical) as *const u8) } {
            Ok(None) => {}
            _ => return None,
        }
        if parking.status.load(Ordering::Acquire) == PARKED {
            let resources = Resources {
                stack,
                tables,
                parking: parking as *const Parking as *mut Parking,
            };
            return Idle::decode(parking.idle.load(Ordering::Relaxed))
                .map(|idle| (idle, resources));
        }
        if apic::rdtsc().wrapping_sub(since) >= handoff.tsc_hz {
            return None;
        }
        core::hint::spin_loop();
    }
}
