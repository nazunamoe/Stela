//! A stop the USB's boot log cannot keep leaves its last lines in the PC's
//! firmware (`stela_boot_core::stop_record`): [`keep`] writes them with the
//! firmware's variable services, which still run after its boot services
//! have ended, and [`carry`] copies them into the log at the next start
//! and deletes them.
//!
//! [`keep`] runs once a boot, on the first processor that stops, and only
//! from a stop: a boot that does not stop writes no variable. The loader
//! calls the services where the firmware left them. The engine reaches
//! them in the loader's root, which keeps the firmware's runtime regions
//! mapped by identity, as `SetVirtualAddressMap` is never called.
use crate::output;
use alloc::{boxed::Box, format, string::String, vec::Vec};
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering},
};
use stela_boot_core::firmware_runtime::{Regions, Return};
use stela_boot_core::running_record::{self, Running, VariableService};
use stela_boot_core::stop_record::{
    ATTRIBUTES, FirmwareCall, HEADER_NAME, Header, Kept, PIECES, Space, Tail, VENDOR,
    carried_notice, fitting, piece_name, pieces, read, stop_reason,
};
use uefi::{
    CStr16, Guid,
    runtime::{self, VariableAttributes, VariableVendor},
    table::Revision,
};

/// `EFI_RUNTIME_SERVICES.SetVariable` (UEFI 2.10 section 8.2.3).
type SetVariable =
    unsafe extern "efiapi" fn(*const u16, *const Guid, u32, usize, *const u8) -> usize;
/// `EFI_RUNTIME_SERVICES.QueryVariableInfo` (section 8.2.4).
type QueryVariableInfo = unsafe extern "efiapi" fn(u32, *mut u64, *mut u64, *mut u64) -> usize;
/// `EFI_SUCCESS`.
const SUCCESS: usize = 0;

/// What a stop writes its record with, which the loader hands the engine.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub(crate) struct Services {
    /// The firmware's `SetVariable` and `QueryVariableInfo` entries; zero
    /// without both.
    set_variable: u64,
    query_variable_info: u64,
    /// This boot's number in the USB's log; 0 without one.
    boot: u64,
    /// How a restart comes back to the firmware entry this start came
    /// from (`BootCurrent`, section 3.3): `Return::encode`.
    restart: u64,
}

impl Services {
    /// The firmware's, read while its boot services run. Firmware older
    /// than UEFI 2.0 has no `QueryVariableInfo`, and gets no record.
    pub(crate) fn capture() -> Self {
        let services = Self::capture_without_heap();
        if services.set_variable == 0 {
            return services;
        }
        let variable = |name: &CStr16| {
            runtime::get_variable_boxed(name, &VariableVendor::GLOBAL_VARIABLE)
                .ok()
                .map(|(value, _)| value)
        };
        let current = variable(uefi::cstr16!("BootCurrent"))
            .and_then(|value| <[u8; 2]>::try_from(&*value).ok())
            .map(u16::from_le_bytes);
        let order = variable(uefi::cstr16!("BootOrder")).unwrap_or_default();
        Self {
            restart: Return::decide(current, &order).encode(),
            ..services
        }
    }

    /// [`Self::capture`] without the firmware entry this start came from,
    /// whose read needs the heap: for a stop before the heap exists.
    pub(crate) fn capture_without_heap() -> Self {
        let Some(table) = uefi::table::system_table_raw() else {
            return Self::default();
        };
        // SAFETY: the firmware's live system table, before boot services end.
        let runtime = unsafe { table.as_ref().runtime_services };
        if runtime.is_null() {
            return Self::default();
        }
        // SAFETY: as above; its runtime services table.
        let runtime = unsafe { &*runtime };
        if runtime.header.revision < Revision::EFI_2_00 {
            return Self::default();
        }
        Self {
            set_variable: runtime.set_variable as usize as u64,
            query_variable_info: runtime.query_variable_info as usize as u64,
            boot: 0,
            restart: Return::Unnamed.encode(),
        }
    }
}

static SET_VARIABLE: AtomicU64 = AtomicU64::new(0);
static QUERY_VARIABLE_INFO: AtomicU64 = AtomicU64::new(0);
static BOOT: AtomicU64 = AtomicU64::new(0);
static RESTART: AtomicU64 = AtomicU64::new(0);
/// The root the firmware's services are called in; zero for the tables
/// the processor has, the firmware's own.
static ROOT: AtomicU64 = AtomicU64::new(0);

/// Give a later stop the firmware's services, called in `root`: `None`
/// while this image runs on the firmware's tables. Until then, and with
/// services the firmware lacks, a stop writes nothing.
pub(crate) fn arm(services: Services, root: Option<u64>) {
    ROOT.store(root.unwrap_or(0), Ordering::Relaxed);
    BOOT.store(services.boot, Ordering::Relaxed);
    RESTART.store(services.restart, Ordering::Relaxed);
    QUERY_VARIABLE_INFO.store(services.query_variable_info, Ordering::Relaxed);
    SET_VARIABLE.store(services.set_variable, Ordering::Release);
}

/// From now on a stop writes nothing: this image is about to run on
/// tables that do not map the firmware's runtime regions.
pub(crate) fn disarm() {
    SET_VARIABLE.store(0, Ordering::Release);
}

/// This boot's number in the USB's log, once it is open.
pub(crate) fn set_boot(boot: u64) {
    BOOT.store(boot, Ordering::Relaxed);
}

/// This boot's number in the USB's log; 0 without one.
pub(crate) fn boot() -> u64 {
    BOOT.load(Ordering::Relaxed)
}

/// The last boot on this PC that reached the firmware's exit, as the
/// firmware's variable names it (`running_record`), and the USB log's
/// newest lines from before this boot's own, which [`tell_unfinished`]
/// decides from.
static LAST_RUN: output::Global<Option<(Running, Vec<u8>)>> =
    output::Global(core::cell::RefCell::new(None));

/// The firmware's record of the last boot that reached its exit; `None`
/// where it holds none. A read, while boot services run: nothing is
/// written.
pub(crate) fn running() -> Option<Running> {
    let name = CStr16::from_u16_with_nul(&running_record::NAME).ok()?;
    let (value, _) = runtime::get_variable_boxed(name, &VariableVendor(VENDOR)).ok()?;
    Running::decode(&value)
}

/// Keep `running` and the log's `newest` lines, read before this boot's
/// marker joined them, for [`tell_unfinished`].
pub(crate) fn keep_last_run(running: Running, newest: Vec<u8>) {
    if let Ok(mut last) = LAST_RUN.0.try_borrow_mut() {
        *last = Some((running, newest));
    }
}

/// Say, in the log and on the screen, that the boot before this one ended
/// without a stop, with the last step its log shows
/// (`running_record::unfinished`): it hung and was switched off, or the PC
/// reset. Called once the USB's log, on partition `log`, is open and
/// [`carry`] ran; `carried` is the boot of the stop record it copied.
/// Nothing is said, and nothing written, for a boot whose end is in the
/// log or the firmware, or for another USB's.
pub(crate) fn tell_unfinished(log: [u8; 16], carried: Option<u64>) {
    let Some((running, newest)) = LAST_RUN
        .0
        .try_borrow_mut()
        .ok()
        .and_then(|mut last| last.take())
    else {
        return;
    };
    let (service, flags) = running_record::variable_service(
        &running,
        boot(),
        &log,
        &newest,
        env!("CARGO_PKG_VERSION"),
    );
    SERVICE.store(service as u8, Ordering::Relaxed);
    FLAGS.store(flags, Ordering::Relaxed);
    let Some(found) = running_record::unfinished(&running, boot(), &log, carried, &newest) else {
        return;
    };
    output::arguments(format_args!("{}", found.line()));
    crate::status::warning(&found.notice());
}

/// Leave the firmware the record that this boot reached its exit: one
/// variable write, the only one a boot makes for it
/// (`running_record::WRITES_PER_BOOT`), made here while boot services run
/// and the log is open, and never after them. Firmware that does not
/// report its store, has too little of it free or refuses the write is
/// named in one line and the boot goes on: a boot that then ends without
/// a stop is not told at the next start. Without the USB's log, on
/// partition `log`, there is no boot to name and nothing is written.
pub(crate) fn mark_running(log: Option<[u8; 16]>) {
    let boot = boot();
    let (Some(log), true) = (log, boot != 0) else {
        return;
    };
    let Ok(name) = CStr16::from_u16_with_nul(&running_record::NAME) else {
        return;
    };
    let attributes = VariableAttributes::from_bits_retain(running_record::ATTRIBUTES);
    let started = output::now();
    let refused = match runtime::query_variable_info(attributes) {
        Err(error) => Some(format!(
            "the firmware does not report its variable store ({:?})",
            error.status()
        )),
        Ok(info) if !running_record::store_takes(info.remaining_variable_storage_size) => {
            Some(format!(
                "the firmware's variable store has only {} bytes free",
                info.remaining_variable_storage_size
            ))
        }
        Ok(_) => {
            let seconds = output::now() / output::counter_hz().max(1);
            let record = Running {
                boot,
                log,
                seconds: u32::try_from(seconds).unwrap_or(u32::MAX),
                flags: FLAGS.load(Ordering::Relaxed),
            };
            runtime::set_variable(name, &VariableVendor(VENDOR), attributes, &record.encode())
                .err()
                .map(|error| format!("the firmware refused the write ({:?})", error.status()))
        }
    };
    let millis = output::millis_since(started);
    match refused {
        None => output::arguments(format_args!(
            "BOOT RUNNING RECORD boot={boot} written in {millis} ms: the next start on this PC \
             says so if this boot ends without a stop\n"
        )),
        Some(why) => output::arguments(format_args!(
            "BOOT RUNNING RECORD not written: {why}; if this boot ends without a stop, the \
             next start does not say so\n"
        )),
    }
}

/// The firmware call a processor is in ([`in_firmware`]): its kind in the
/// low byte and the processor's APIC ID above it; left set by a fault inside
/// the call, which stops the boot.
static CALL: AtomicU64 = AtomicU64::new(FirmwareCall::None as u64);

/// This processor's APIC ID, as CPUID gives it: the x2APIC ID where leaf
/// 0xb reports one, else the initial APIC ID.
fn processor() -> u64 {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    u64::from(
        match __cpuid(0).eax >= 0xb && __cpuid_count(0xb, 0).ebx != 0 {
            true => __cpuid_count(0xb, 0).edx,
            false => __cpuid(1).ebx >> 24,
        },
    )
}

/// Run `call`, a call of the firmware's services, as `kind`, in the root
/// the services were armed with.
fn in_firmware<T>(kind: FirmwareCall, call: impl FnOnce() -> T) -> T {
    CALL.store(processor() << 8 | kind as u64, Ordering::Release);
    let result = match ROOT.load(Ordering::Relaxed) {
        0 => call(),
        // SAFETY: the loader's live root, which `arm` was given with the
        // engine's handoff; a restart resumes nothing here.
        root => unsafe { crate::engine::in_loader_root_at_stop(root, call) },
    };
    CALL.store(FirmwareCall::None as u64, Ordering::Release);
    result
}

/// The firmware call this processor's x86 exception came from, taken once:
/// a fault inside the firmware's services is the firmware's, not Stela's.
/// Another processor's call is not this exception's.
pub(crate) fn take_firmware_call() -> FirmwareCall {
    let call = CALL.load(Ordering::Acquire);
    let kind = FirmwareCall::from_u8(call as u8);
    if kind == FirmwareCall::None || call >> 8 != processor() {
        return FirmwareCall::None;
    }
    CALL.store(FirmwareCall::None as u64, Ordering::Release);
    // A fault inside the service may have left its store half written:
    // nothing calls it again in this boot.
    SET_VARIABLE.store(0, Ordering::Release);
    kind
}

/// The runtime regions the loader's root maps, for a fault's line; set once
/// by the engine from its handoff.
static REGIONS: AtomicPtr<Regions> = AtomicPtr::new(core::ptr::null_mut());

/// Keep the loader's record of the firmware's runtime regions.
pub(crate) fn set_regions(regions: &'static Regions) {
    REGIONS.store(core::ptr::from_ref(regions).cast_mut(), Ordering::Release);
}

/// Where `address`, which a firmware call faulted at, lies among the
/// runtime regions the firmware declared (`Regions::place`).
pub(crate) fn fault_place(address: u64) -> String {
    let regions = REGIONS.load(Ordering::Acquire);
    if regions.is_null() {
        return String::from("the firmware's runtime regions are unknown here");
    }
    // SAFETY: `set_regions` stored a 'static reference.
    unsafe { &*regions }.place(address)
}

/// What [`arm`] was given, with the boot's number, for the engine: without
/// the variable services where they are not to be called after the
/// firmware's exit ([`refuse_after_exit`]).
pub(crate) fn services() -> Services {
    let refused = AFTER_EXIT_REFUSED.load(Ordering::Relaxed);
    Services {
        set_variable: if refused {
            0
        } else {
            SET_VARIABLE.load(Ordering::Acquire)
        },
        query_variable_info: if refused {
            0
        } else {
            QUERY_VARIABLE_INFO.load(Ordering::Relaxed)
        },
        boot: BOOT.load(Ordering::Relaxed),
        restart: RESTART.load(Ordering::Relaxed),
    }
}

/// Ask the firmware to start the entry this start came from at the next
/// start (`BootNext`), for a restart macOS asked for: the firmware's own
/// order may put another system first, and a restart must come back here.
/// Nothing is written where the entry is first in the firmware's order,
/// where `BootNext` was written before the firmware's exit, or where the
/// firmware named no entry or its services are out of reach. The `POWER`
/// line that says which.
pub(crate) fn return_at_restart() -> String {
    /// `EFI_VARIABLE_NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS`.
    const BOOT_NEXT_ATTRIBUTES: u32 = 0x7;
    let entry = match Return::decode(RESTART.load(Ordering::Relaxed)) {
        Return::AtRestart(entry) => entry,
        other => return other.line().unwrap_or_default(),
    };
    let set = SET_VARIABLE.load(Ordering::Acquire) as usize;
    if set == 0 {
        return format!(
            "POWER the firmware chooses what starts after the restart: its variable service is \
             not called after its exit in this boot, as a line above says, so BootNext for its \
             entry Boot{entry:04X} is not written\n"
        );
    }
    // SAFETY: the firmware's entry, as `Services::capture` read it from its
    // runtime services table; code addresses fit a pointer.
    let set = unsafe { core::mem::transmute::<usize, SetVariable>(set) };
    let name = uefi::cstr16!("BootNext");
    let value = entry.to_le_bytes();
    let store = || {
        // SAFETY: a runtime service, in the tables its regions are mapped
        // in; the name, GUID and data outlive the call.
        unsafe {
            set(
                name.as_ptr().cast(),
                &VariableVendor::GLOBAL_VARIABLE.0,
                BOOT_NEXT_ATTRIBUTES,
                value.len(),
                value.as_ptr(),
            )
        }
    };
    match in_firmware(FirmwareCall::BootNext, store) {
        SUCCESS => {
            format!("POWER the firmware starts its entry Boot{entry:04X} again after the restart\n")
        }
        status => format!(
            "POWER the firmware chooses what starts after the restart: it refused BootNext for \
             its entry Boot{entry:04X} (status {status:#x})\n"
        ),
    }
}

/// The firmware's variable services are not called after its exit in this
/// boot: [`services`] hands the engine none.
static AFTER_EXIT_REFUSED: AtomicBool = AtomicBool::new(false);
/// How this boot calls the runtime variable service
/// (`running_record::variable_service`), as its discriminant, and the
/// flags its record keeps.
static SERVICE: AtomicU8 = AtomicU8::new(VariableService::Trusted as u8);
static FLAGS: AtomicU32 = AtomicU32::new(0);

/// From the firmware's exit on, this boot calls none of its variable
/// services: a stop keeps no record in the firmware, and a restart writes
/// no `BootNext` then.
pub(crate) fn refuse_after_exit() {
    AFTER_EXIT_REFUSED.store(true, Ordering::Relaxed);
}

/// Before the firmware's exit, on a PC whose runtime variable service
/// faulted in an earlier boot under this version: call it only while boot
/// services run. A restart that needs `BootNext` has it written now, while
/// boot services run; the firmware then starts the entry again at the next
/// start however this boot ends. Under another version the boot calls it
/// after the exit as any boot does, and says so. Called once, before
/// [`mark_running`], which keeps the PC's mark.
pub(crate) fn before_exit() {
    let service = SERVICE.load(Ordering::Relaxed);
    if service == VariableService::Tried as u8 {
        output::arguments(format_args!(
            "FIRMWARE VARIABLE SERVICE faulted after the firmware's exit under another version \
             on this PC: this boot calls it after the exit again, and the next start clears the \
             mark once a call there answers\n"
        ));
    }
    if service != VariableService::Avoided as u8 {
        return;
    }
    refuse_after_exit();
    output::arguments(format_args!(
        "FIRMWARE VARIABLE SERVICE faulted after the firmware's exit in an earlier boot on this \
         PC, which can leave its variable store half written: this boot calls it only before \
         the exit, and a stop keeps no record in the firmware\n"
    ));
    let Return::AtRestart(entry) = Return::decode(RESTART.load(Ordering::Relaxed)) else {
        return;
    };
    /// `EFI_VARIABLE_NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS`.
    const BOOT_NEXT_ATTRIBUTES: u32 = 0x7;
    let written = runtime::set_variable(
        uefi::cstr16!("BootNext"),
        &VariableVendor::GLOBAL_VARIABLE,
        VariableAttributes::from_bits_retain(BOOT_NEXT_ATTRIBUTES),
        &entry.to_le_bytes(),
    );
    let restart = match written {
        Ok(()) => {
            output::arguments(format_args!(
                "FIRMWARE BootNext set to Boot{entry:04X} before the exit: a restart comes back \
                 here, and so does the next start after a power-off or a stop\n"
            ));
            Return::BeforeExit(entry)
        }
        Err(error) => {
            output::arguments(format_args!(
                "FIRMWARE BootNext for Boot{entry:04X} refused before the exit ({:?}): the \
                 firmware chooses what starts after a restart\n",
                error.status()
            ));
            Return::Refused(entry)
        }
    };
    RESTART.store(restart.encode(), Ordering::Relaxed);
}

/// No stop has tried to write the record.
const IDLE: u8 = 0;
/// A processor is writing it, or stopped again inside the write.
const WRITING: u8 = 1;
/// The firmware took the whole record.
const WRITTEN: u8 = 2;
/// Nothing was written, or not all of it.
const SKIPPED: u8 = 3;
static STATE: AtomicU8 = AtomicU8::new(IDLE);

struct Lines(UnsafeCell<Tail>);
// SAFETY: reached only by the one processor that moved `STATE` from `IDLE`.
unsafe impl Sync for Lines {}
/// The record's text, outside the stack a stop runs on.
static LINES: Lines = Lines(UnsafeCell::new(Tail::new()));

/// Write the stop's record, the newest lines `fill` gives, into the
/// firmware's variables; whether the firmware holds it whole. Only the
/// first call writes, on whichever processor makes it: the firmware's
/// services are not called by two processors at once, and a fault inside
/// one comes back here and finds the write begun, so it is not tried
/// again. Later calls say what the first did.
///
/// Nothing is written without the services ([`arm`]), without lines, or
/// where the record would leave less than the reserve of the firmware's store
/// free (`Space::takes`).
pub(crate) fn keep(fill: impl FnOnce(&mut Tail)) -> bool {
    if STATE
        .compare_exchange(IDLE, WRITING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return STATE.load(Ordering::Acquire) == WRITTEN;
    }
    let written = write(fill);
    STATE.store(if written { WRITTEN } else { SKIPPED }, Ordering::Release);
    written
}

fn write(fill: impl FnOnce(&mut Tail)) -> bool {
    let (set, query) = (
        SET_VARIABLE.load(Ordering::Acquire) as usize,
        QUERY_VARIABLE_INFO.load(Ordering::Relaxed) as usize,
    );
    if set == 0 || query == 0 || AFTER_EXIT_REFUSED.load(Ordering::Relaxed) {
        return false;
    }
    // SAFETY: the firmware's entries, as `Services::capture` read them from
    // its runtime services table; code addresses fit a pointer.
    let (set, query) = unsafe {
        (
            core::mem::transmute::<usize, SetVariable>(set),
            core::mem::transmute::<usize, QueryVariableInfo>(query),
        )
    };
    // SAFETY: this processor alone moved `STATE` from `IDLE`.
    let lines = unsafe { &mut *LINES.0.get() };
    fill(lines);
    let text = lines.text();
    let boot = BOOT.load(Ordering::Relaxed);
    let store = || {
        let mut space = Space {
            maximum: 0,
            remaining: 0,
            largest: 0,
        };
        // SAFETY: a runtime service, in the tables its regions are mapped
        // in; the results are this frame's.
        let asked = unsafe {
            query(
                ATTRIBUTES,
                &mut space.maximum,
                &mut space.remaining,
                &mut space.largest,
            )
        };
        if asked != SUCCESS {
            return false;
        }
        // A store too short for all of it keeps the newest lines that fit.
        let text = fitting(&space, text);
        let Some(header) = Header::of(boot, text) else {
            return false;
        };
        for (index, piece) in pieces(text).enumerate() {
            let name = piece_name(index);
            // SAFETY: as above; the name, GUID and data outlive the call.
            let status = unsafe {
                set(
                    name.as_ptr(),
                    &VENDOR,
                    ATTRIBUTES,
                    piece.len(),
                    piece.as_ptr(),
                )
            };
            if status != SUCCESS {
                return false;
            }
        }
        // Last, so pieces without it, or under an older one, are no record.
        let header = header.encode();
        // SAFETY: as above.
        let status = unsafe {
            set(
                HEADER_NAME.as_ptr(),
                &VENDOR,
                ATTRIBUTES,
                header.len(),
                header.as_ptr(),
            )
        };
        status == SUCCESS
    };
    in_firmware(FirmwareCall::Record, store)
}

/// Copy the record an earlier start left on this PC into the log, between
/// marker lines, tell the screen that start stopped, why and where its
/// lines are (`carried_notice`), and delete its variables once the USB's
/// log holds the lines. Variables that are no whole record are reported in
/// one line and deleted. Called once the USB's log is open, while boot services run;
/// a PC that holds no record, as one the stick was moved to, gets no line
/// and no write. The boot a whole record names, for [`tell_unfinished`].
pub(crate) fn carry() -> Option<u64> {
    let vendor = VariableVendor(VENDOR);
    let value = |name: &[u16]| -> Option<Box<[u8]>> {
        let name = CStr16::from_u16_with_nul(name).ok()?;
        runtime::get_variable_boxed(name, &vendor)
            .ok()
            .map(|(value, _)| value)
    };
    let header = value(&HEADER_NAME);
    let values: [Option<Box<[u8]>>; PIECES] =
        core::array::from_fn(|index| value(&piece_name(index)));
    // The stop's reason, for the screen: a stop after the GPU's reset left
    // none of its own.
    let mut carried = None;
    let reason = match read(header.as_deref(), &values) {
        Kept::Absent => return None,
        Kept::Damaged(why) => {
            output::arguments(format_args!(
                "PREVIOUS BOOT left a stop record in this PC's firmware that cannot be read: \
                 {why}\n"
            ));
            None
        }
        Kept::Record { boot, text } => {
            output::arguments(format_args!(
                "PREVIOUS BOOT stopped (boot {boot} of its USB's log) where the USB's log was \
                 out of reach or the screen may have been dead; this PC's firmware kept its last \
                 lines:\n"
            ));
            for line in text.lines() {
                output::arguments(format_args!("PREVIOUS BOOT {line}\n"));
            }
            output::arguments(format_args!("PREVIOUS BOOT stop record ends\n"));
            carried = Some(boot);
            Some(stop_reason(&text).map(String::from))
        }
    };
    // On the stick first: the variables are the only other copy.
    output::flush_log();
    let in_log = output::usb_log_kept();
    if let Some(reason) = reason {
        crate::status::warning(&carried_notice(reason.as_deref(), in_log));
    }
    if !in_log {
        output::arguments(format_args!(
            "PREVIOUS BOOT stop record stays in this PC's firmware: the USB's log did not take \
             it\n"
        ));
        return carried;
    }
    let names = core::iter::once((HEADER_NAME.as_slice(), header.is_some())).chain(
        PIECE_NAMES
            .iter()
            .zip(&values)
            .map(|(name, value)| (name.as_slice(), value.is_some())),
    );
    let mut refused = None;
    // The header first: pieces without it are no record.
    for (name, _) in names.filter(|&(_, held)| held) {
        let deleted = CStr16::from_u16_with_nul(name)
            .map_err(|_| uefi::Status::INVALID_PARAMETER)
            .and_then(|name| {
                runtime::delete_variable(name, &vendor).map_err(|error| error.status())
            });
        if let Err(status) = deleted {
            refused.get_or_insert(status);
        }
    }
    match refused {
        None => output::arguments(format_args!(
            "PREVIOUS BOOT stop record cleared from this PC's firmware\n"
        )),
        Some(status) => output::arguments(format_args!(
            "PREVIOUS BOOT stop record not cleared from this PC's firmware: {status:?}. The \
             next start copies it again\n"
        )),
    }
    carried
}

/// Every piece's name, for [`carry`]'s borrow of them.
const PIECE_NAMES: [[u16; 11]; PIECES] = {
    let mut names = [[0; 11]; PIECES];
    let mut index = 0;
    while index < PIECES {
        names[index] = piece_name(index);
        index += 1;
    }
    names
};
