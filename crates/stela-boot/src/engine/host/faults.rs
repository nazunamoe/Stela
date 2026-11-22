//! Native faults: recovery footers, abandoned blocks, page faults and native
//! exceptions.

use super::*;
use stela_compiler::x86_64::flag_word;

// Native code owns the FP registers: the host is soft-float, translated code
// keeps no guest state in them across an exit and IRQs use their IST images.
// recovery_rsp holds the host's registers, which return and recovery pop.
global_asm!(
    r#"
.global stela_enter_block
stela_enter_block:
    xor r10d, r10d
    jmp stela_enter_owned
.global stela_enter_context_block
stela_enter_context_block:
    mov r10d, 1
stela_enter_owned:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    push rdi
    push rsi
    push rdx
    cmp qword ptr gs:[{recovery}], 0
    jne {refuse}
    cmp qword ptr gs:[{helper}], 0
    jne {refuse}
    ldmxcsr [rip + {default_mxcsr}]
    mov gs:[{recovery}], rsp
    test r10d, r10d
    jne stela_enter_owned_context
    // Preserve the legacy generated-block entry alignment (RSP%16=0 in the
    // block); the separate context ABI remains SysV (RSP%16=8 in the block).
    sub rsp, 8
    mov rax, rsi
    // The pinned guest registers and the chain's account (`x86_64::pinned`)
    // live in these while native code runs, with the flag word
    // (`x86_64::flag_word`).
    mov ecx, [rdi + {nzcv}]
    shr ecx, {arm_shift}
    imul ecx, ecx, {pack}
    and ecx, {word_mask}
    mov [rdi + {nzcv}], ecx
    mov rbx, [rdi + {sp}]
    mov rsi, [rdi + 64]
    mov r14, [rdi + 152]
    mov r15, [rdi + 0]
    mov rbp, [rdi + 8]
    mov r12, [rdi + 128]
    mov r13, [rdi + {account}]
    // Every exit reloads RSP from here before its RET (block ABI v14).
    lea r11, [rsp - 8]
    mov [rdi + {exit_rsp}], r11
    call rax
    mov [rdi + {sp}], rbx
    mov [rdi + 64], rsi
    mov [rdi + 152], r14
    mov [rdi + 0], r15
    mov [rdi + 8], rbp
    mov [rdi + 128], r12
    mov [rdi + {account}], r13
    mov ecx, [rdi + {nzcv}]
    and ecx, {word_mask}
    imul ecx, ecx, {unpack}
    and ecx, {arm_mask}
    mov [rdi + {nzcv}], ecx
    jmp stela_native_owned_return
stela_enter_owned_context:
    lea r11, [rsp - 8]
    mov [rdi + {exit_rsp}], r11
    call rdx
stela_native_owned_return:
    cmp qword ptr gs:[{helper}], 0
    jne {refuse}
    jmp stela_native_restore_host
.global stela_native_recover
stela_native_recover:
    xor eax, eax
    mov edx, 0xffff
stela_native_restore_host:
    // An abandoned helper has no surviving borrowed state or image.
    mov rsp, gs:[{recovery}]
    test rsp, rsp
    jz {refuse}
    mov qword ptr gs:[{helper}], 0
    mov qword ptr gs:[{recovery}], 0
    add rsp, 24
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
"#,
    sp = const core::mem::offset_of!(GuestState, sp),
    account = const core::mem::offset_of!(GuestState, account),
    exit_rsp = const core::mem::offset_of!(GuestState, exit_rsp),
    nzcv = const core::mem::offset_of!(GuestState, nzcv),
    arm_shift = const flag_word::ARM_SHIFT,
    pack = const flag_word::PACK_MULTIPLIER,
    unpack = const flag_word::UNPACK_MULTIPLIER,
    word_mask = const flag_word::WORD_MASK,
    arm_mask = const flag_word::ARM_MASK,
    recovery = const local::RECOVERY_RSP_OFFSET,
    helper = const local::HELPER_RSP_OFFSET,
    refuse = sym super::super::xstate::stela_xstate_boundary_refuse,
    default_mxcsr = sym super::super::xstate::DEFAULT_MXCSR,
);

unsafe extern "sysv64" {
    pub(super) fn stela_enter_block(state: *mut GuestState, code: usize) -> BlockExit;
    pub(super) fn stela_enter_context_block(
        state: *mut GuestState,
        context: *mut stela_compiler::context::ExecutionContext,
        code: usize,
    ) -> BlockExit;
    pub(super) fn stela_native_recover();
}

/// Whether `rip` lies in native code this processor runs: its private arena
/// or the board's code window.
pub(super) fn in_code(state: &State, rip: u64) -> bool {
    state.native
        && ((state.code_virtual..state.code_virtual + state.code_bytes).contains(&rip)
            || (state.window_virtual..state.window_virtual + state.window_bytes).contains(&rip))
}

/// End the native block at `frame` and report `cause` to the engine.
pub(super) fn abandon(
    state: &mut State,
    frame: &mut Frame,
    address: u64,
    write: bool,
    cause: FaultCause,
    saved: &super::super::interrupts::SavedState<'_>,
) {
    state.recorded = Some(Recorded {
        rip: frame.rip,
        origin: stela_engine::machine::NativeFaultOrigin::Exception,
        trap: u16::try_from(frame.vector).unwrap_or(u16::MAX),
        error_code: frame.error,
        address,
        write,
        cause,
        registers: saved
            .registers()
            .unwrap_or_else(|_| {
                crate::output::fatal("native fault has no valid owned interrupt register image")
            })
            .with_vectors(saved.vectors()),
    });
    frame.rip = stela_native_recover as *const () as u64;
}

/// The guest's translation of the access at `address`, through the same
/// software walk for shadow faults and the translation helper: the
/// translation and the host physical address of its RAM page, or why the
/// engine must take the access (Arm checks watchpoints after translation,
/// `AArch64_TranslateAddress`, then Tag Checks).
pub(super) fn resolve(
    state: &mut State,
    address: u64,
    write: bool,
    unprivileged: bool,
) -> Result<(mmu::Translation, u64), FaultCause> {
    resolve_recording(state, address, write, unprivileged, true, &mut None)
}

/// [`resolve`], caching a device translation in the helper's software TLB
/// only with `record_device`: a fault-around walk ([`map_around`]) leaves
/// the TLB as no access touched it. A walk starts at `table` when it holds
/// one ([`mmu::translate_from`]), else it notes there the table it read
/// its last descriptor from, for the walks of the same fault after it.
fn resolve_recording(
    state: &mut State,
    address: u64,
    write: bool,
    unprivileged: bool,
    record_device: bool,
    table: &mut Option<mmu::WalkTable>,
) -> Result<(mmu::Translation, u64), FaultCause> {
    let regime = state.regime.as_ref().ok_or(FaultCause::NonCanonical)?;
    let access = if write { Access::Write } else { Access::Read };
    let requester = mmu::Requester {
        unprivileged: regime.requester.unprivileged || unprivileged,
        ..regime.requester
    };
    let virtual_page = address & !(PAGE - 1);
    if let Some(physical) = state.tlb.device_physical(address, requester, write) {
        // The cached translation already passed both stages and data
        // permissions. Watchpoint passes can change without a new walk.
        return Err(if regime.watch.blocks(virtual_page) {
            FaultCause::Watched
        } else {
            FaultCause::Device { physical }
        });
    }
    let outcome = match *table {
        Some(start) => mmu::translate_from(
            &regime.controls,
            &start,
            address,
            access,
            requester,
            &mut GuestTables::new(state),
        ),
        None => {
            let (outcome, noted) = mmu::translate_noting(
                &regime.controls,
                address,
                access,
                requester,
                &mut GuestTables::new(state),
            );
            *table = noted;
            outcome
        }
    };
    let translation = match outcome {
        Outcome::Mapped(translation) => translation,
        Outcome::Fault(fault) => return Err(FaultCause::Guest(fault)),
        Outcome::Unsupported(unsupported) => return Err(FaultCause::Translation(unsupported)),
    };
    if regime.watch.blocks(virtual_page) {
        return Err(FaultCause::Watched);
    }
    if regime.tag_blocks(address, translation.attributes) {
        return Err(FaultCause::TagChecked);
    }
    let page = translation.physical & !(PAGE - 1);
    let Some(host) = state.host_physical(page) else {
        // This walker makes no AF or dirty descriptor updates.
        if record_device {
            state.tlb.record_device(address, requester, translation);
        }
        return Err(FaultCause::Device {
            physical: translation.physical,
        });
    };
    Ok((translation, host))
}

/// #PF: fill the shadow from the guest's tables or end the block precisely.
pub(crate) fn page_fault(
    frame: &mut Frame,
    address: u64,
    saved: &super::super::interrupts::SavedState<'_>,
) {
    let entered = rdtsc();
    fill_or_end(frame, address, saved, entered);
    let state = state();
    state.faults += 1;
    state.fault_ticks += rdtsc().wrapping_sub(entered);
}

/// The #PF gate's fill (`interrupts::stela_pf_gate`), before the handler
/// saves any extended state: a fault from native code at `rip` that the
/// shadow fills from the guest's tables returns true, filled. Every other
/// outcome changes nothing it would not change again and returns false for
/// the full handler ([`page_fault`]) to take the fault from the start. The
/// target is soft-float, so this path leaves the guest's vector registers
/// as the fault found them.
pub(crate) fn gate_fill(error: u64, rip: u64, rflags: u64, address: u64) -> bool {
    let entered = rdtsc();
    if !in_code(state(), rip) {
        return false;
    }
    super::pmu::fill_begin();
    let filled = matches!(fill(error, rflags, address, entered), Filled::Done);
    super::pmu::fill_end();
    if filled {
        let state = state();
        state.faults += 1;
        state.gate_fills += 1;
        state.fault_ticks += rdtsc().wrapping_sub(entered);
    }
    filled
}

/// What [`fill`] came to.
enum Filled {
    /// The shadow maps the page, or nothing needed mapping.
    Done,
    /// The block ends with this cause.
    End(FaultCause),
    /// The shadow cannot map the page even after it was emptied.
    Fatal,
}

/// [`abandon`] for a #PF, counted by its cause's class ([`ABANDONED`]).
fn abandon_fault(
    state: &mut State,
    frame: &mut Frame,
    address: u64,
    write: bool,
    cause: FaultCause,
    saved: &super::super::interrupts::SavedState<'_>,
) {
    let class = match cause {
        FaultCause::Guest(_) => 0,
        FaultCause::Device { .. } => 1,
        _ => 2,
    };
    state.abandoned[class] += 1;
    abandon(state, frame, address, write, cause, saved);
}

/// [`page_fault`]'s work, from its handler's entry at TSC `entered`.
fn fill_or_end(
    frame: &mut Frame,
    address: u64,
    saved: &super::super::interrupts::SavedState<'_>,
    entered: u64,
) {
    let write = frame.error & 2 != 0;
    if !in_code(state(), frame.rip) {
        // A client's mapping Linux moved: its refault may sleep, which only
        // the faulting context may do, never this handler.
        #[cfg(feature = "nvk")]
        if super::super::gpu::client::client_fault(address) {
            match super::process_fault::redirect(frame, address) {
                Ok(()) => return,
                Err(refused) => {
                    super::super::gpu::client::note_unserved(address, frame.rip, refused)
                }
            }
        }
        #[cfg(feature = "nvk")]
        super::super::gpu::client::note_stack_guard(address);
        super::super::interrupts::fatal(frame);
    }
    match fill(frame.error, frame.rflags, address, entered) {
        Filled::Done => {}
        Filled::End(cause) => {
            let state = state();
            if matches!(cause, FaultCause::NonCanonical) {
                state.guard_faults += u64::from(state.placement.in_guard(address));
            }
            abandon_fault(state, frame, address, write, cause, saved);
        }
        Filled::Fatal => super::super::interrupts::fatal(frame),
    }
}

/// Fill the shadow for a #PF from native code at `address` with error
/// code `error` and RFLAGS `rflags`, its handler entered at TSC `entered`,
/// or say why not.
fn fill(error: u64, rflags: u64, address: u64, entered: u64) -> Filled {
    let write = error & 2 != 0;
    let state = state();
    state.follow_native();
    state.follow_pan(rflags & RFLAGS_AC == 0);
    let Some(tag_checks) = state.regime.as_ref().map(|regime| regime.tag_checks) else {
        return Filled::Fatal;
    };
    // Only the native ranges are shadow; the rest is the engine's. An
    // access past a check an earlier one stood for may reach the guard
    // (`mmu::placement`): its own check finds where it goes.
    if !state.placement.native(address) {
        return Filled::End(FaultCause::NonCanonical);
    }
    // The fault's own walk notes its leaf table; fault-around's walks of
    // the same window start there.
    let mut walk_table = None;
    let (translation, host) =
        match resolve_recording(state, address, write, false, true, &mut walk_table) {
            Ok(resolved) => resolved,
            Err(cause) => return Filled::End(cause),
        };
    let walked = rdtsc();
    state.walks += 1;
    let rights = shadow_rights(state, &translation);
    let root = state.roots[state.root_of(state.current)];
    let virtual_page = address & !(PAGE - 1);
    let half = if address >> 63 == 0 { LOWER } else { UPPER };
    if half == LOWER
        && translation.global
        && state.lower_globals.as_ref().is_some_and(|span| {
            let (first, last) = shadow::leaf(address, translation.size);
            span.widen(first, last)
        })
    {
        return Filled::Done;
    }
    state.pool.half = half;
    // A failed allocation can already have installed intermediate tables.
    state.legacy_translations_dirty = true;
    // The root's entry, which an upper half's fill that makes it gives
    // every set of the same rights.
    let root_entry = tables::index(virtual_page, layout::top_level());
    let entry_at = State::root_entry(root, root_entry);
    // SAFETY: an entry of the current set's permanent root.
    let mut before = unsafe { entry_at.read() };
    let mut mapped = tables::map_page_slot(&mut state.pool, root, virtual_page, host, rights);
    // The lower half's frames are exhausted: give up other sets' lower
    // halves for theirs, the least recently used first, before emptying
    // every set's.
    while mapped.is_err()
        && half == LOWER
        && let Some(oldest) = state.oldest_lower()
    {
        state.kept_dropped += 1;
        state.drop_lower(oldest);
        state.pool.half = half;
        mapped = tables::map_page_slot(&mut state.pool, root, virtual_page, host, rights);
    }
    let slot = match mapped {
        Ok(slot) => slot,
        Err(_) => {
            // The half's frames are exhausted: start it over and map again.
            state.exhausted += 1;
            if half == LOWER {
                state.reset_lower();
            } else {
                state.reset_shadow(Cause::Exhausted);
            }
            state.pool.half = half;
            state.legacy_translations_dirty = true;
            before = 0;
            match tables::map_page_slot(&mut state.pool, root, virtual_page, host, rights) {
                Ok(slot) => slot,
                Err(_) => return Filled::Fatal,
            }
        }
    };
    // A not-present fault has no cached translation to invalidate. Every
    // shadow removal already invalidates that view or marks it stale.
    // A protection fault can replace a present mapping.
    if error & 1 != 0 {
        invlpg(virtual_page);
    }
    if half == LOWER {
        state.record_lower(root_entry);
    } else if before == 0 {
        // SAFETY: as above.
        let after = unsafe { entry_at.read() };
        if after != 0 {
            state.share_upper(state.root_of(state.current), root_entry, after);
        }
    }
    state.extents[half].record(address, translation.size, translation.global);
    let space = match (half, state.regime.as_ref()) {
        (LOWER, Some(regime)) => {
            let controls = &regime.controls;
            controls.ttbr0 ^ (controls.tcr & LOWER_TCR).rotate_left(29)
        }
        _ => 0,
    };
    let view = state.root_of(state.current);
    let filled = state
        .history
        .fill(half, space, virtual_page, view, error & 1 != 0);
    state.fill_causes[filled.cause as usize] += 1;
    // The rest of the leaf's window has the same translation and rights:
    // map its RAM pages that are not watched now, as a TLB entry covers
    // the whole leaf. Pages that were absent need no flush. A Tagged leaf
    // whose tags are checked maps only the passed page.
    if tag_checks[half] && translation.attributes == mmu::attributes::TAGGED {
        state.fills += 1;
        state.fill_ticks += rdtsc().wrapping_sub(entered);
        return Filled::Done;
    }
    const { assert!(FILL_WINDOW.is_power_of_two() && FILL_WINDOW <= tables::LARGE) };
    let (first, last) = shadow::window(address, translation.size, FILL_WINDOW);
    if last - first >= PAGE {
        let physical = translation
            .physical
            .wrapping_sub(address)
            .wrapping_add(first);
        // Contiguous backing is common. Look up the first page once, then
        // fall back to page lookups past the end of that physical run.
        let (first_host, available) = state.run(physical).unwrap_or((0, 0));
        let table = slot.wrapping_sub(tables::index(virtual_page, 0));
        let watch = &state.regime.as_ref().expect("checked above").watch;
        let watched = watch.overlaps(first, last);
        let mut page = first;
        loop {
            if page != virtual_page && (!watched || !watch.blocks(page)) {
                let offset = page - first;
                let host = if offset < available {
                    first_host.checked_add(offset)
                } else {
                    state.host_physical(physical.wrapping_add(offset))
                };
                if let Some(host) = host {
                    // SAFETY: the power-of-two prefill window is contained
                    // in one leaf table. The just-mapped slot belongs to
                    // that live table, exclusively managed by this host.
                    unsafe {
                        table
                            .add(tables::index(page, 0))
                            .write_volatile((host & tables::ADDRESS) | rights)
                    };
                    state.prefills += 1;
                }
            }
            if page >= last & !(PAGE - 1) {
                break;
            }
            page += PAGE;
        }
    }
    // The window's other leaves, when the guest's are smaller than it. A
    // protection fault replaced a present page, whose neighbours its own
    // fill already mapped.
    let mapped_at = rdtsc();
    if error & 1 == 0 {
        let table = slot.wrapping_sub(tables::index(virtual_page, 0));
        for beside in shadow::around(address, translation.size, FILL_WINDOW) {
            state.around += map_around(state, table, half, beside, &mut walk_table).unwrap_or(0);
        }
        // The first fill of a region since its half was lost maps again
        // the granules of it the lost incarnation's faults filled, from walks
        // made now, as fault-around maps its window: one fault, rather than
        // one each, after a TLBI of everything, a half out of frames or a
        // kept half replaced. What it maps is not a fault's, so the next
        // loss names it only if a fault takes it again: the list follows
        // what the guest touches rather than growing with each restore.
        let mut restore = filled.restore;
        while restore != 0 {
            let bit = restore.trailing_zeros();
            restore &= restore - 1;
            let granule = shadow_fills::granule(virtual_page, bit);
            if let Some(pages) = map_around(state, table, half, granule, &mut walk_table) {
                state.restored += pages;
            }
        }
    }
    // A non-global upper leaf ties the upper half to the ASID.
    state.update_open();
    let done = rdtsc();
    state.fills += 1;
    state.fill_ticks += done.wrapping_sub(entered);
    state.fill_segments[0] += walked.wrapping_sub(entered);
    state.fill_segments[1] += mapped_at.wrapping_sub(walked);
    state.fill_segments[2] += done.wrapping_sub(mapped_at);
    Filled::Done
}

/// The shadow entry bits for `translation` in the current view. EL1 and
/// PAN share a root. SMAP guards a leaf if a read with PAN set would be
/// denied by either stage, or by stage 1's PAN rules.
fn shadow_rights(state: &State, translation: &mmu::Translation) -> u64 {
    let guarded = state.contexts.pan_flag
        && state.current % VIEWS == EL1_VIEW
        && (!translation.readable || translation.pan_sensitive);
    PRESENT
        | NX
        | if translation.writable { WRITABLE } else { 0 }
        | if guarded { tables::USER } else { 0 }
}

/// Fault-around: map the leaf at `address` in the window of a fault just
/// filled through `table` (the live leaf table that holds the whole
/// window) exactly as a read fault at `address` would fill it, from its own
/// walk in the current regime and view. The walk starts at the guest table
/// the fault's walk read its leaf from (`walk_table`, [`mmu::translate_from`]),
/// which holds the window's other leaves when they share it, as a walk
/// cache's entry would. Arm lets a TLB walk and cache any valid
/// translation before an access needs it, and a fault-around entry
/// is recorded in the half's extents like a demand fill's, so the same
/// TLBIs, context switches and resets remove it. Every walk outcome a read
/// fill would not map is left to the leaf's own fault: translation, Access
/// flag (no hardware AF update) and permission faults, watchpoints, Tag
/// Checks, Tagged leaves under checks and devices, whose translation is not
/// cached either. Lower-half global leaves are left to their fault too (the
/// shared span widens there), and pages already present are kept, so every
/// entry written was absent and needs no flush.
///
/// Returns the pages it mapped, or `None` when it maps nothing of the leaf
/// whether present or not.
fn map_around(
    state: &mut State,
    table: *mut u64,
    half: usize,
    address: u64,
    walk_table: &mut Option<mmu::WalkTable>,
) -> Option<u64> {
    if !state.placement.native(address) {
        return None;
    }
    state.walks += 1;
    state.table_walks += u64::from(walk_table.is_some_and(|table| table.holds(address)));
    let (translation, _) =
        resolve_recording(state, address, false, false, false, walk_table).ok()?;
    let tag_checks = state
        .regime
        .as_ref()
        .is_some_and(|regime| regime.tag_checks[half]);
    if (half == LOWER && translation.global)
        || (tag_checks && translation.attributes == mmu::attributes::TAGGED)
    {
        return None;
    }
    let rights = shadow_rights(state, &translation);
    let (first, last) = shadow::window(address, translation.size, FILL_WINDOW);
    // `table` maps the 2 MiB around `address` alone.
    if (first ^ address) >> 21 != 0 || (last ^ address) >> 21 != 0 {
        return None;
    }
    let physical = translation
        .physical
        .wrapping_sub(address)
        .wrapping_add(first);
    let watch = &state.regime.as_ref().expect("a fault was filled").watch;
    let mut page = first;
    let mut mapped = 0;
    loop {
        // SAFETY: the window lies in the 2 MiB around the fault just
        // filled (the caller's addresses and the check above), which the
        // one leaf table `table` names maps: the live table that fault's
        // slot belongs to, which only this host manages.
        let entry = unsafe { table.add(tables::index(page, 0)) };
        // SAFETY: as above.
        let absent = unsafe { entry.read_volatile() } & PRESENT == 0;
        if absent
            && !watch.blocks(page)
            && let Some(host) = state.host_physical(physical.wrapping_add(page - first))
        {
            // SAFETY: as above.
            unsafe { entry.write_volatile((host & tables::ADDRESS) | rights) };
            mapped += 1;
        }
        if page >= last & !(PAGE - 1) {
            break;
        }
        page += PAGE;
    }
    if mapped != 0 {
        state.extents[half].record(address, translation.size, translation.global);
    }
    Some(mapped)
}

/// #SS, #GP or #UD inside native code ends the block; elsewhere it is
/// fatal. #SS and #GP name a non-canonical address: #SS where RSP or RBP
/// is the access's base.
pub(crate) fn native_exception(
    frame: &mut Frame,
    saved: &super::super::interrupts::SavedState<'_>,
) {
    let state = state();
    if !in_code(state, frame.rip) {
        super::super::interrupts::fatal(frame);
    }
    let cause = if matches!(frame.vector, 12 | 13) {
        FaultCause::NonCanonical
    } else {
        FaultCause::InvalidOpcode
    };
    abandon(state, frame, 0, false, cause, saved);
}
