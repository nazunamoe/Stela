//! An integrated GPU's share of RAM, as the device budgets it.
//!
//! The GPU's buffers are Linux's pages, and Linux may hold no more than the
//! share (the page catalog's limit). Neither the driver's heap sizes nor its
//! budget bound what it takes: RADV's budget is advisory and follows its own
//! use, and amdgpu only fails a submission (`Not enough memory for command
//! submission`) once a buffer it must place finds no page. So the device
//! refuses an allocation itself, at its out-of-memory answer, before the
//! share is gone, from what the platform says Linux holds of it now
//! ([`ShareState`]).
//!
//! The driver's heaps hold more than the guest's resources: its own
//! buffers (command streams, descriptor and shader arenas, queries), the
//! device's block slack and upload chunks, and the images' metadata. On
//! one 12 GB host with a 2 GiB share the heaps held 489M at the device's
//! first allocation and then grew 1.22 bytes for each byte the guest held
//! (1763M for 1045M held); on another session, 2058M for 1326M. So an
//! allocation is taken to cost [`GROWTH`] times its bytes in pages, and
//! what the guest held at the device's first allocation, with Linux's pages
//! then, is the base the growth starts from. The share's VRAM carve-out is
//! not counted in: it holds what the driver placed first.
//!
//! The pages an allocation may take are then the larger of what Linux holds
//! now and what the guest's growth since the base takes, plus its own
//! growth; past the share less a margin ([`margin`]) it is refused.
use core::sync::atomic::{AtomicPtr, Ordering};

/// Bytes of driver memory each byte the guest holds costs, as a fraction:
/// 1.25, above the 1.07 to 1.22 measured.
pub const GROWTH: (u64, u64) = (5, 4);

/// What the platform says of an integrated GPU's share now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShareState {
    /// The bytes Linux holds of it: every page, the GPU's buffers' and its own.
    pub held: u64,
    /// The share's bytes.
    pub limit: u64,
}

/// What the device saw at its first allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Base {
    /// Linux's bytes then.
    pub held: u64,
    /// The bytes the device held then.
    pub guest: u64,
}

/// The platform's probe of a share: its state now, when there is one.
pub type Probe = fn() -> Option<ShareState>;

/// A device's share: the probe, and what the device saw at its first
/// allocation, once it made one.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub probe: Probe,
    pub base: Option<Base>,
}

static PROBE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Sets what the device asks for the share's state: the platform's count
/// of Linux's pages, which it sets before the device opens when an
/// integrated GPU has a share.
pub fn set_probe(probe: Probe) {
    PROBE.store(probe as *mut (), Ordering::Release);
}

/// The platform's probe of the share, when one was set.
pub fn probe() -> Option<Probe> {
    let probe = PROBE.load(Ordering::Acquire);
    // SAFETY: only `set_probe` stores here, a function of this type.
    (!probe.is_null()).then(|| unsafe { core::mem::transmute::<*mut (), Probe>(probe) })
}

/// The budget of a device whose GPU is `unified` (shares the processor's
/// memory, as Vulkan's device type says of it), under `probe`: only such a
/// GPU takes its memory from the share. A discrete GPU beside an integrated
/// one (a laptop's NVIDIA or AMD GPU beside its AMD or Intel graphics) works
/// in its VRAM and is never budgeted by the share, nor told it.
pub fn budget(probe: Option<Probe>, unified: bool) -> Option<Budget> {
    probe
        .filter(|_| unified)
        .map(|probe| Budget { probe, base: None })
}

/// The share kept free of allocations: a sixteenth, at least 64 MiB, for
/// Linux's own pages and the work a refused allocation leaves in flight.
pub fn margin(limit: u64) -> u64 {
    (limit / 16).max(64 << 20)
}

fn growth(bytes: u64) -> u64 {
    bytes.saturating_mul(GROWTH.0) / GROWTH.1
}

/// The memory the guest is told it works in: what the share has left past
/// the margin, at [`GROWTH`] driver bytes for each of the guest's.
pub fn working_set(state: ShareState) -> u64 {
    let left = state
        .limit
        .saturating_sub(state.held)
        .saturating_sub(margin(state.limit));
    left.saturating_mul(GROWTH.1) / GROWTH.0
}

/// Whether an allocation of `request` bytes must be refused, the device
/// holding `guest` bytes now and the share being in `state`.
pub fn refuses(state: ShareState, base: Base, guest: u64, request: u64) -> bool {
    let committed = base
        .held
        .saturating_add(growth(guest.saturating_sub(base.guest)));
    let pages = state.held.max(committed).saturating_add(growth(request));
    pages.saturating_add(margin(state.limit)) > state.limit
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// On a laptop with an integrated GPU and a discrete one, the share's
    /// probe is set, but only the integrated GPU's device is budgeted by it.
    #[test]
    fn only_a_unified_gpus_device_is_budgeted_by_the_share() {
        fn state() -> Option<ShareState> {
            Some(ShareState {
                held: 0,
                limit: 2560 * MIB,
            })
        }
        assert!(budget(Some(state), false).is_none());
        let unified = budget(Some(state), true).unwrap();
        assert_eq!((unified.probe)(), state());
        assert!(unified.base.is_none());
        assert!(budget(None, true).is_none());
    }

    #[test]
    fn the_guest_is_told_what_the_share_backs() {
        // A 2.5 GiB share with 480M of Linux's pages at the device's open.
        let state = ShareState {
            held: 480 * MIB,
            limit: 2560 * MIB,
        };
        assert_eq!(margin(state.limit), 160 * MIB);
        assert_eq!(working_set(state), 1536 * MIB);
        // A small share keeps its 64 MiB margin; a full one tells nothing.
        let small = ShareState {
            held: 200 * MIB,
            limit: 512 * MIB,
        };
        assert_eq!(working_set(small), 198 * MIB + MIB * 2 / 5);
        let full = ShareState {
            held: 2560 * MIB,
            limit: 2560 * MIB,
        };
        assert_eq!(working_set(full), 0);
    }

    #[test]
    fn allocations_stop_before_the_share_is_gone() {
        let limit = 2048 * MIB;
        let base = Base {
            held: 489 * MIB,
            guest: 0,
        };
        let now = |held| ShareState { held, limit };
        // The run that filled its GTT held 1045M, which a whole share
        // backs; at 1300M the growth passes it, with Linux's pages not yet
        // all taken.
        assert!(!refuses(now(1400 * MIB), base, 1045 * MIB, 4 * MIB));
        assert!(refuses(now(1400 * MIB), base, 1300 * MIB, 4 * MIB));
        // Early, with room, an allocation is made.
        assert!(!refuses(now(600 * MIB), base, 300 * MIB, 64 * MIB));
        // Linux's own count decides when it is past the growth: pages the
        // guest freed but the driver kept, or Linux's own.
        assert!(refuses(now(1950 * MIB), base, 100 * MIB, MIB));
        // A request too large for what is left is refused at once.
        assert!(refuses(now(600 * MIB), base, 300 * MIB, 1024 * MIB));
        // The device holding less than at its base takes the base's pages.
        assert!(!refuses(now(0), base, 0, MIB));
    }
}
