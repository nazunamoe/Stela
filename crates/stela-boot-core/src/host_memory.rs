//! The parts of Stela's memory sized from the host RAM free at boot (the
//! engine heap, native code arena and shadow page-table pool), the share
//! left for the guest, and what is left of it for the translation pack.
use crate::Result;
use crate::native_resources::{
    CODE_ALIGNMENT, CodeReservation, SMALL_WINDOW_PER_RUNNER, WINDOW_PER_RUNNER,
    code_reservation_within, parked_reserve, window_share,
};
use stela_contracts::refuse;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// Each part's alignment: the large page the engine maps it with.
const ALIGNMENT: u64 = 2 * MIB;
/// What the heap needs does not shrink with the RAM, which bounds it from
/// below: a 26A434 desktop boot holds about 1.5 GiB live when WindowServer
/// compiles Core Animation's generic Vfx shader (`fixed_frag_lph_cpf` with
/// `fc_generic`, 42 529 NAK instructions in about 4800 blocks), whose
/// per-block liveness takes NAK another 450 MiB for the compile. That
/// peaked at 2150 MiB, and a boot with a fuller translation pack ran out of
/// a 2304 MiB heap there.
pub const HEAP_FLOOR: u64 = 2816 * MIB;
/// Above the floor, the heap grows with the host to this: a larger host
/// runs more and larger pipelines, apps and translation packs.
pub const HEAP_CEILING: u64 = 16 * GIB;
/// Free host RAM bytes per byte of the engine heap's base share, which
/// passes the floor on hosts with more than about 16 GiB free. The floor
/// is what a boot was measured to need, with little to spare: a PC with 30
/// GiB free ran its first boot after an install on the floor (3072 MiB with
/// a 784 MiB window), carved 2673 MiB of it and failed an allocation with
/// 700 MiB free in extents of at most 192 MiB, while its guest held 25 GiB.
/// An eighth gives that host 4.5 GiB of heap and takes a twentieth of the
/// guest's DRAM; hosts near the guest's floor keep the heap's floor.
const HEAP_SHARE: u64 = 8;
/// The heap a desktop session needs, with room for the large allocations a
/// fragmented heap must still place, at [`FLOOR_WINDOW`]. A macOS desktop
/// holds 2.3 to 2.7 GiB live from about 80 s on, whatever the host, and
/// carved at most 2924 MiB on a 16 GiB PC and 3168 MiB on a 32 GiB PC, both
/// with a 768 MiB window. The 16 GiB PC ran on the floor (3072 MiB): with
/// 450 MiB free in extents of at most 16 MiB it failed a 17 609 526 byte
/// allocation, and an earlier boot a 267 386 880 byte one with 192 MiB
/// extents. The 32 GiB PC's 4666 MiB kept extents of 1.2 GiB. Hosts between
/// the floor and their share take the heap up to this from the guest, as
/// far as [`GUEST_ROOM`] allows. A 16 GiB PC's 8-hour session on its 4352
/// MiB (the target at a 768 MiB window) held 2.8-3.3 GiB live in 1.3-1.8
/// million free extents, the largest 194-272 MiB, and peaked at 4063 MiB
/// when NAK compiled Core Animation's generic shader 8 hours in: defined
/// at that window, the target would leave such a session 33 MiB.
const HEAP_TARGET: u64 = 4 * GIB;
/// The guest DRAM [`HEAP_TARGET`] leaves at least: twice [`GUEST_FLOOR`].
/// A guest under it keeps the heap's share or floor, so 8 GiB hosts boot
/// with what they had.
const GUEST_ROOM: u64 = 2 * GUEST_FLOOR;
/// Heap bytes per byte of the board's code window: the shared block records,
/// successors, recovery rows, index and lookup of the blocks it holds,
/// once for the board since the block store is shared. An 8-PE 26A434
/// boot with a full 448 MiB window (1.56 million of 1.69 million slots
/// live) held 437 MiB of them (`ENGINE MEMORY blocks=231M successors=53M
/// rows=14M index=19M lookup=120M`), 0.98 bytes per window byte. Before
/// the shared store each core kept its own, 2.9 bytes per byte.
const HEAP_PER_WINDOW: u64 = 1;
/// The code window [`HEAP_FLOOR`] was measured with. The floor moves by
/// [`HEAP_PER_WINDOW`] per byte the window differs from it: its block
/// records come on top of the rest of the peak, which does not depend on
/// the window. A 12 GiB VM with a 704 MiB window peaked at 2815 MiB carved,
/// 2623 MiB less the 192 MiB of window past this one.
const FLOOR_WINDOW: u64 = 512 * MIB;
/// Heap each running PE keeps for itself: its machine (about 1 MiB at
/// start), ITLB (512 KiB), fetch and jump caches (about 0.8 MiB) and
/// successor scratch (under 1 MiB); four times that, for its own compiles.
const PE_HEAP: u64 = 16 * MIB;
/// Guest DRAM bytes per byte of a running executor's shadow page-table
/// pool: room for its 8 roots to map all of guest DRAM once in 4 KiB pages
/// (8 bytes per page). An exhausted half resets and refills everything; a
/// 26A434 restore's userland filled 8 MiB halves 20 to 80 times a beat.
const POOL_SHARE: u64 = 64;
/// Runners whose pools together map the guest's DRAM once each. Past them
/// the runners divide that total: each PE's tables are a cache of what it
/// touches, and more PEs divide the guest's working set between them.
const POOL_SHARERS: u64 = 8;
const POOL_FLOOR: u64 = 64 * MIB;
/// The least pool a running executor gets, a 16 MiB half, at any PE count.
const POOL_RUNNER_FLOOR: u64 = 32 * MIB;
/// Each sharer's pool maps at most 64 GiB of guest DRAM once; past that its
/// halves reset as a cache of the guest's working set, rather than hold a
/// tenth of a large host's RAM.
const POOL_PER_RUNNER: u64 = GIB;
/// Past small guests a runner's pool is its two halves' sizes, not a
/// sixty-fourth of the guest: [`LOWER_BASE`] and a [`LOWER_GROWTH`]th of the
/// guest's DRAM for the lower half, [`UPPER_BASE`] and an
/// [`UPPER_GROWTH`]th for the upper. The lower half (TTBR0, user address
/// spaces) is a cache of recent address spaces: it fills whatever it has,
/// and ran out about once in 1.6 heartbeats with 52 MiB as with 64 MiB (74
/// times in 117, 106 in 178), while TLB maintenance reset the shadow about
/// 190 times a heartbeat (`SHADOW exhausted= maintained=`); 48 MiB and a
/// 2048th holds about the 52 MiB of the 12 GiB run. The upper half (TTBR1,
/// the kernel) peaked at 22-29 MiB per runner whatever the guest's size
/// (`BEAT shadow_peak=52M/23M/52M`, `64M/22M/64M`, 29 MiB in a 4-hour 16
/// GiB run): 32 MiB and a 1024th, 42 MiB at 16 GiB.
const LOWER_BASE: u64 = 48 * MIB;
const LOWER_GROWTH: u64 = 2048;
const UPPER_BASE: u64 = 32 * MIB;
const UPPER_GROWTH: u64 = 1024;
/// The least guest DRAM Stela boots macOS with. Below it macOS spends its
/// time paging; 26A434 reaches the desktop with 3.1 GiB.
pub const GUEST_FLOOR: u64 = 3 * GIB;
/// The step [`Shortfall::needed`] is found in.
const NEEDED_STEP: u64 = 256 * MIB;
/// Free host RAM bytes per byte of the persistent translation pack the
/// loader may hold: a cache, it takes no more than this share and only what
/// the parts and the guest's floor leave.
pub const PACK_SHARE: u64 = 16;
/// The least pack worth reading. Below it the loader leaves the pack on its
/// volume, unread and unwritten, for a PC with more RAM.
pub const PACK_FLOOR: u64 = 64 * MIB;
/// What the loader takes between measuring the free RAM for the pack and
/// reserving the parts, beyond the pack itself: the pack's placement page,
/// the storage drivers' rings and the firmware ranges' rounding to 2 MiB.
const PACK_MARGIN: u64 = 16 * MIB;
/// Free host RAM bytes per byte of the share an integrated GPU, which has
/// no memory of its own past its carve-out, takes for its pages. Linux and
/// Windows let such a GPU map up to half of RAM, and macOS on Apple silicon
/// about two thirds, but those are limits on memory the system takes back
/// under pressure; here the share is held from the guest for the boot, so
/// it is a sixth: 2.6 GiB on a 16 GB PC, 5.2 GiB on a 32 GB PC, and the
/// floor's 2.5 GiB below about 15 GiB free.
const GPU_SHARE: u64 = 6;
/// The share a macOS desktop needs. A 16 GiB VM's AMD integrated GPU (512
/// MiB carve-out, 3272 MiB of GTT) held 2058 MiB of RADV's heaps after a
/// sign-in and seven app launches, the guest holding 1326 MiB (`driver
/// heaps use/budget/size: 277/1176/1176M/host 1781/2352/2352M/local`). A 12
/// GiB VM's heaps held 489 MiB at the Metal device's first allocation and
/// grew 1.22 bytes for each byte the guest held, to 1763 MiB for 1045 MiB,
/// where its GTT, half of a 2 GiB share, ran out. The device budgets each
/// allocation at 1.25 bytes of the share and keeps a sixteenth free
/// (`metal::device::share` in the GPU crate), and Linux's own pages and the
/// driver's first buffers take about 480 MiB of it, so the guest is told
/// about (share - 480 MiB - share/16) * 0.8: 1.2 GiB of a 2 GiB share, short
/// of the session above, and 1.5 GiB of 2.5 GiB, which holds it. A host
/// that leaves the guest its floor only with less gets less, down to
/// [`GPU_LEAST`], and says so.
pub const GPU_FLOOR: u64 = 2560 * MIB;
/// The most an integrated GPU's share grows to. A GPU whose firmware gives
/// it a large carve-out (up to many GiB on recent processors) works in that
/// beside the share; the guest keeps the rest of a large host.
const GPU_CEILING: u64 = 8 * GIB;
/// The least share Stela boots an integrated GPU's Metal device with:
/// twice the 129 MiB of RADV's host heap the desktop above held, for its
/// command buffers, uploads and mapped buffers. A host that cannot give it
/// with the guest's floor is refused, as one short of the guest's floor is.
const GPU_LEAST: u64 = 256 * MIB;

/// What an integrated GPU that only shows the display (beside a discrete
/// GPU that draws) takes of the heap beside its own needs: its driver's
/// pages that its carve-out does not hold, and a few scanout and cursor
/// buffers, 32 MiB each at 3840x2160. Linux's pages are not bounded then:
/// the discrete GPU's system-memory buffers (TTM's system pages, a GTT,
/// page tables) come from the heap as its floor was measured with.
pub const DISPLAY_SHARE: u64 = 256 * MIB;

/// What the heap's own allocations held at most beside an integrated GPU's
/// pages: a 16 GiB VM's heap peaked at 4482 MiB with 2058 MiB of the driver's
/// heaps in it, beside amdgpu and RADV. Where no share is planned and every
/// GPU that starts has a driver this was measured beside
/// ([`GpuPlan::bounded`]), Linux's pages (a discrete GPU's system-memory
/// buffers, its evictions from VRAM, an integrated GPU that draws because
/// the discrete one did not start) are held to what the heap holds past
/// this ([`page_bound`]).
pub const HEAP_OWN: u64 = 2424 * MIB;

/// The least of the heap Linux's pages may hold where no share is planned,
/// for heaps whose spare past [`HEAP_OWN`] is less (a floor heap at a small
/// code window).
pub const PAGES_LEAST: u64 = 384 * MIB;

/// What an integrated GPU's Linux driver and RADV held of Linux's pages at
/// its Metal device's first allocation (AMD, with a 2 to 2.5 GiB share).
pub const FALLBACK_OPEN: u64 = 480 * MIB;

/// Linux's pages an integrated GPU needs to draw a desktop in place of a
/// discrete GPU that did not start: [`FALLBACK_OPEN`], and the least share
/// Stela boots an integrated GPU's Metal device with ([`GPU_LEAST`]), in
/// whole 256 MiB. Where such a GPU may draw
/// instead ([`GpuPlan::fallback`]), the plan makes the heap region hold this
/// past [`HEAP_OWN`] ([`Sizes::fallback`]), as far as the guest's floor
/// allows.
pub const FALLBACK_LEAST: u64 = 768 * MIB;

/// The least of Linux's pages an integrated GPU needs free under their bound
/// for its driver to bind it at all: what one that only shows the display
/// takes ([`DISPLAY_SHARE`]). With less, a fallback to it is refused
/// ([`fallback_pages`]).
pub const FALLBACK_START: u64 = DISPLAY_SHARE;

/// The page tables a driver's mapping of a whole device-memory aperture of
/// `bytes` takes from the heap: Stela's page window maps device memory in
/// 4 KiB pages, an 8-byte entry each (`bytes / 512`, 32 MiB for 16 GiB),
/// and a directory entry for each table, in whole alignments.
pub fn aperture_tables(bytes: u64) -> u64 {
    let leaves = bytes.div_ceil(4096) * 8;
    let directories = leaves.div_ceil(4096) * 8;
    (leaves + directories).next_multiple_of(ALIGNMENT)
}

/// What the memory plan knows of the GPUs before the parts are sized: how
/// they use the host's RAM, the page tables of the device-memory apertures
/// their drivers map whole ([`aperture_tables`]), which the heap grows by,
/// and whether Linux's pages are held to [`page_bound`] where no share is
/// planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuPlan {
    pub memory: GpuMemory,
    pub aperture: u64,
    /// Every GPU that starts has a driver whose heap need beside it was
    /// measured ([`HEAP_OWN`]), so Linux's pages are held to [`page_bound`]
    /// where no share is planned ([`held_bound`]). Otherwise, as beside
    /// nouveau and NVK, Linux takes the whole heap as its RAM and TTM keeps
    /// its own limit, half of that, as every boot gave them before the
    /// bound: a bound measured beside another driver could fail a session
    /// that ran.
    pub bounded: bool,
    /// An integrated GPU this image draws with stands beside the discrete
    /// GPU the plan gives the Metal device, and draws in its place should it
    /// not start (`selection::integrated_may_draw_instead`): where no share
    /// is planned, the heap region is made to hold [`FALLBACK_LEAST`] of
    /// Linux's pages past [`HEAP_OWN`] ([`Sizes::fallback`]), and the engine
    /// holds Linux's pages to [`page_bound`] once it draws
    /// ([`fallback_pages`]). False in an image without such a GPU's driver,
    /// which plans as before.
    pub fallback: bool,
}

impl From<GpuMemory> for GpuPlan {
    fn from(memory: GpuMemory) -> Self {
        Self {
            memory,
            aperture: 0,
            bounded: false,
            fallback: false,
        }
    }
}

/// The most bytes of pages Linux may hold at once under `sizes`, planned
/// for `memory`: an integrated GPU's share where it draws
/// ([`GpuMemory::Shared`]); otherwise what the heap region (its display
/// reserve with it) holds past [`HEAP_OWN`], at least [`PAGES_LEAST`], so
/// that a discrete GPU's system-memory pages fail their allocation before
/// the heap runs out. Where [`held_bound`] holds Linux to it, the page
/// catalog refuses more and TTM's limit is the same; an integrated GPU that
/// draws without a share budgets its Metal device by it either way.
pub fn page_bound(sizes: &Sizes, memory: GpuMemory) -> u64 {
    match memory {
        GpuMemory::Shared => sizes.gpu,
        GpuMemory::Own | GpuMemory::Display => {
            let region = sizes.heap + sizes.gpu + sizes.fallback;
            region.saturating_sub(HEAP_OWN).max(PAGES_LEAST).min(region) / ALIGNMENT * ALIGNMENT
        }
    }
}

/// What the engine does with Linux's pages once an integrated GPU is to
/// draw in place of a GPU that could not ([`fallback_pages`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackPages {
    /// Linux's pages are held to `limit` bytes already (an integrated GPU's
    /// share, or a plan that holds them), with `room` of it free.
    Held { limit: u64, room: u64 },
    /// The plan held Linux's pages to no bound: they are held to `limit`
    /// bytes from now on, before the integrated GPU's driver sizes its
    /// memory from the RAM Linux reports, with `room` of it free.
    Hold { limit: u64, room: u64 },
    /// Linux holds `held` bytes of the `bound` already, which leaves less
    /// than [`FALLBACK_START`] for the integrated GPU's driver: the fallback
    /// to it is refused, as its start could only fail.
    Short { held: u64, bound: u64 },
    /// An integrated GPU started already draws, and Linux holds `held`
    /// bytes, past the `bound`: its pages stay unbounded, and the GPU's
    /// Metal device alone budgets its allocations to the bound
    /// ([`drawing_pages`]).
    Over { held: u64, bound: u64 },
}

impl FallbackPages {
    /// The bytes Linux's pages are held to from now on, where the fallback
    /// goes on.
    pub fn limit(self) -> Option<u64> {
        match self {
            Self::Held { limit, .. } | Self::Hold { limit, .. } => Some(limit),
            Self::Short { .. } | Self::Over { .. } => None,
        }
    }

    /// The bytes of the limit free for the integrated GPU, where the
    /// fallback goes on.
    pub fn room(self) -> Option<u64> {
        match self {
            Self::Held { room, .. } | Self::Hold { room, .. } => Some(room),
            Self::Short { .. } | Self::Over { .. } => None,
        }
    }

    /// Whether the integrated GPU's Metal device, once its driver holds
    /// [`FALLBACK_OPEN`] more, tells the guest a working set under half of
    /// [`GPU_LEAST`] (`share::working_set`): it draws, and some windows may
    /// not.
    pub fn starved(self) -> bool {
        use stela_drivers::gpu::share::{ShareState, working_set};
        match self {
            Self::Held { limit, room } | Self::Hold { limit, room } => {
                let held = limit - room + FALLBACK_OPEN;
                working_set(ShareState { held, limit }) < GPU_LEAST / 2
            }
            Self::Short { .. } | Self::Over { .. } => false,
        }
    }

    /// The `GPU MEMORY` line's words, after the GPU's address.
    pub fn describe(self) -> alloc::string::String {
        let mib = |bytes: u64| bytes >> 20;
        let desktop = match self.starved() {
            true => alloc::format!(
                ", little past the {}M its driver holds as its Metal device opens, so some \
                 windows may not draw",
                mib(FALLBACK_OPEN)
            ),
            false => alloc::string::String::new(),
        };
        match self {
            Self::Held { limit, room } => alloc::format!(
                "draws in place of a GPU that could not, in Linux's pages, held to {}M as \
                 planned: {}M of them free{desktop}",
                mib(limit),
                mib(room)
            ),
            Self::Hold { limit, room } => alloc::format!(
                "draws in place of a GPU that could not: Linux's pages, which the plan left \
                 unbounded for the discrete GPU, are held from now on to {}M, what the heap \
                 holds past its own {}M, so the integrated GPU cannot take the heap's own: {}M \
                 of them free{desktop}",
                mib(limit),
                mib(HEAP_OWN),
                mib(room)
            ),
            Self::Short { held, bound } => alloc::format!(
                "not started in place of a GPU that could not draw: Linux holds {}M of the {}M \
                 of pages the heap holds past its own {}M, which leaves under the {}M its \
                 driver needs to start",
                mib(held),
                mib(bound),
                mib(HEAP_OWN),
                mib(FALLBACK_START)
            ),
            Self::Over { held, bound } => alloc::format!(
                "draws in place of a GPU that could not, while Linux holds {}M of pages, past \
                 the {}M the heap holds past its own {}M: they stay unbounded, and its Metal \
                 device alone budgets its allocations to them",
                mib(held),
                mib(bound),
                mib(HEAP_OWN)
            ),
        }
    }
}

/// Linux's pages once an integrated GPU is to draw in place of a GPU that
/// could not, with `bound` the plan's [`page_bound`] (or the share), `held`
/// the bytes Linux holds now and `limit` the bytes it is held to now, zero
/// for none: a plan that held Linux to no bound (beside nouveau, whose
/// heap need was not measured) holds it to `bound` from now on, so that the
/// integrated GPU's pages, which its driver sizes from the RAM Linux
/// reports, never take the heap's own and the worst is its Metal device's
/// out-of-memory answer; where too little is left for its driver to start,
/// the fallback is refused ([`FallbackPages::Short`]).
pub fn fallback_pages(bound: u64, held: u64, limit: u64) -> FallbackPages {
    let limit_now = match limit {
        0 => bound,
        limit => limit,
    };
    let room = limit_now.saturating_sub(held);
    if room < FALLBACK_START {
        return FallbackPages::Short {
            held,
            bound: limit_now,
        };
    }
    match limit {
        0 => FallbackPages::Hold { limit: bound, room },
        limit => FallbackPages::Held { limit, room },
    }
}

/// [`fallback_pages`] for an integrated GPU started already, which draws
/// now in place of a GPU that could not: its driver bound it, so nothing is
/// refused; a plan that held Linux to no bound holds it to `bound` from now
/// on wherever Linux holds no more than that ([`FallbackPages::Over`]
/// otherwise).
pub fn drawing_pages(bound: u64, held: u64, limit: u64) -> FallbackPages {
    match limit {
        0 if held > bound => FallbackPages::Over { held, bound },
        0 => FallbackPages::Hold {
            limit: bound,
            room: bound - held,
        },
        limit => FallbackPages::Held {
            limit,
            room: limit.saturating_sub(held),
        },
    }
}

/// The bound the page catalog and TTM hold Linux's pages to under `sizes`
/// for `plan`: the share where an integrated GPU draws, [`page_bound`] where
/// no share is planned and the plan is [`GpuPlan::bounded`], else none.
pub fn held_bound(sizes: &Sizes, plan: GpuPlan) -> Option<u64> {
    match plan.memory {
        GpuMemory::Shared => Some(sizes.gpu),
        GpuMemory::Own | GpuMemory::Display => plan.bounded.then(|| page_bound(sizes, plan.memory)),
    }
}

/// How the GPUs that start use the host's RAM, as the PCI census and the
/// GPU stacks know it before the parts are sized (the selection's rule,
/// `stela_driver_gpu::selection::memory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMemory {
    /// No integrated GPU starts: a discrete GPU's system-memory pages come
    /// from the heap, bounded by [`page_bound`] where the plan is
    /// [`GpuPlan::bounded`].
    Own,
    /// An integrated GPU draws, sharing the host's RAM: the plan gives it a
    /// share of its own beside the heap ([`Sizes::gpu`]), which bounds all
    /// of Linux's pages.
    Shared,
    /// An integrated GPU only shows the display beside a discrete GPU that
    /// draws: the heap grows by [`DISPLAY_SHARE`] ([`Sizes::gpu`]), and
    /// Linux's pages are bounded by [`page_bound`] where the plan is
    /// [`GpuPlan::bounded`], never by that reserve.
    Display,
}

impl GpuMemory {
    /// Whether [`Sizes::gpu`] is a share that alone bounds Linux's pages,
    /// told to TTM at twice its size ([`page_bound`] for the others).
    pub fn limits_pages(self) -> bool {
        self == Self::Shared
    }
}

impl GpuPlan {
    /// The rule for the plan's line, for `sizes`.
    pub fn describe(&self, sizes: &Sizes) -> alloc::string::String {
        let mib = |bytes: u64| bytes >> 20;
        let bound = page_bound(sizes, self.memory);
        let past = alloc::format!(
            "what the {}M heap holds past its own {}M (at least {}M)",
            mib(sizes.heap + sizes.gpu),
            mib(HEAP_OWN),
            mib(PAGES_LEAST)
        );
        const UNBOUNDED: &str = "Linux takes the heap as its RAM and TTM its own limit, half of \
            it, as the heap's own need was not measured beside every driver that starts";
        let rule = match (self.memory, self.bounded) {
            (GpuMemory::Own, true) => alloc::format!(
                "no integrated GPU starts: Linux's pages, a discrete GPU's system-memory \
                 buffers and its evictions from VRAM among them, come from the heap, at most \
                 {}M of them, {past}; TTM's limit is the same, and an allocation past it fails \
                 as the GPU's memory being full",
                mib(bound),
            ),
            (GpuMemory::Own, false) => alloc::format!(
                "no integrated GPU starts: Linux's pages, a discrete GPU's system-memory \
                 buffers and its evictions from VRAM among them, come from the heap with no \
                 bound of their own: {UNBOUNDED}"
            ),
            (GpuMemory::Shared, _) => alloc::format!(
                "the integrated GPU draws: its share of {}M bounds all of Linux's pages, and \
                 TTM's limit is twice it, so amdgpu's or xe's GTT is the share",
                mib(sizes.gpu)
            ),
            (GpuMemory::Display, true) => alloc::format!(
                "a discrete GPU draws and the integrated GPU shows the display: the heap takes \
                 {}M more for the integrated GPU's driver and scanout; Linux's pages, the \
                 discrete GPU's system-memory buffers among them, are at most {}M, {past}, \
                 TTM's limit the same; should the discrete GPU not start and the integrated \
                 one draw, its Metal device works in those {}M",
                mib(sizes.gpu),
                mib(bound),
                mib(bound)
            ),
            (GpuMemory::Display, false) => alloc::format!(
                "a discrete GPU draws and the integrated GPU shows the display: the heap takes \
                 {}M more for the integrated GPU's driver and scanout; Linux's pages, the \
                 discrete GPU's system-memory buffers among them, have no bound of their own: \
                 {UNBOUNDED}; should the discrete GPU not start and the integrated one draw, \
                 its Metal device budgets its allocations to {}M, {past}",
                mib(sizes.gpu),
                mib(bound)
            ),
        };
        let rule = match (self.fallback, self.memory) {
            (false, _) | (true, GpuMemory::Shared) => rule,
            (true, _) => alloc::format!(
                "{rule}; an integrated GPU this image draws with may draw in the discrete GPU's \
                 place, Linux's pages then held to {}M{}",
                mib(bound),
                match (sizes.fallback, bound >= FALLBACK_LEAST) {
                    (0, true) => alloc::string::String::new(),
                    (more, true) => alloc::format!(
                        ", for which the heap takes {}M more, to hold the {}M it needs",
                        mib(more),
                        mib(FALLBACK_LEAST)
                    ),
                    (more, false) => alloc::format!(
                        ", under the {}M it needs, as the heap takes only {}M more and the guest \
                         keeps its floor",
                        mib(FALLBACK_LEAST),
                        mib(more)
                    ),
                }
            ),
        };
        if sizes.aperture == 0 {
            return rule;
        }
        alloc::format!(
            "{rule}; the heap takes {}M more for the page tables of the device-memory \
             apertures drivers map whole",
            mib(sizes.aperture)
        )
    }
}

/// The parts' sizes and the guest DRAM left after them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    pub heap: u64,
    pub code: CodeReservation,
    pub pool: u64,
    /// Free RAM after the three parts and the GPU's share; Stela's stacks,
    /// tables and image take a few more MiB of it.
    pub guest: u64,
    /// The share of an integrated GPU ([`GpuMemory::Shared`]): the most
    /// system memory Linux's pages hold at once, reserved beside the heap
    /// and taken from the same region, on top of what the heap itself
    /// needs. For one that only shows ([`GpuMemory::Display`]) its pages'
    /// part of the heap; zero with no integrated GPU.
    pub gpu: u64,
    /// The page tables of the device-memory apertures drivers map whole
    /// ([`GpuPlan::aperture`]), reserved beside the heap and taken from the
    /// same region.
    pub aperture: u64,
    /// What the heap region takes beyond the heap and `gpu` so that it
    /// holds [`FALLBACK_LEAST`] of Linux's pages past [`HEAP_OWN`], for an
    /// integrated GPU that may draw in a discrete GPU's place
    /// ([`GpuPlan::fallback`]), as much as the guest's floor allows; part of
    /// the heap's region, which the heap uses while no such GPU draws. Zero
    /// without such a GPU, and where the heap holds it already.
    pub fallback: u64,
}

/// The DRAM the guest Mac reports and the host RAM it does not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestDram {
    /// `/chosen dram-size` and `boot_args.memSizeActual`.
    pub reported: u64,
    /// Installed host RAM that Stela and the firmware keep.
    pub kept: u64,
}

/// The guest reports the DRAM that backs it, `backed` bytes, not the
/// host's `installed` RAM.
///
/// XNU sets `max_mem_actual` from `memSizeActual` and `max_mem` and
/// `sane_size` from `memSize` (the managed range). `hw.memsize` reads
/// `max_mem_actual`; `hw.memsize_usable` reads `max_mem`. User space
/// (`NSProcessInfo.physicalMemory`, Metal's working-set size, jetsam
/// limits, cache sizes) uses `hw.memsize`. A reported size above what
/// exists makes it plan for memory that is not there: a 26A434 boot with
/// 3.9 GB backed and 8 GB reported ran its compressor and pager at full
/// load after login. On a Mac the gap is iBoot's carveouts, a few percent;
/// here those carveouts lie inside the backed range (`memSize` excludes
/// them), so the backed size is the truthful `max_mem_actual`.
pub fn guest_dram(installed: u64, backed: u64) -> GuestDram {
    GuestDram {
        reported: backed,
        kept: installed.saturating_sub(backed),
    }
}

/// Too little RAM for the guest's floor (and an integrated GPU's least
/// share): `free` bytes, of `needed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortfall {
    pub free: u64,
    pub needed: u64,
    pub gpu: GpuMemory,
}

impl Shortfall {
    /// The refusal the boot stops with, in whole GB rounded up, as RAM is
    /// sold, and the free RAM to a tenth.
    pub fn refusal(self) -> crate::Refusal {
        let needed = self.needed.div_ceil(GIB);
        let free = alloc::format!("{}.{}", self.free / GIB, self.free % GIB * 10 / GIB);
        let floor = GUEST_FLOOR / GIB;
        let rest = match self.gpu {
            GpuMemory::Own | GpuMemory::Display => "its own memory",
            GpuMemory::Shared => "its own memory and the graphics built into the processor",
        };
        refuse!(
            "this PC has {free} GB of RAM free; Stela needs at least {needed} GB free: \
             {floor} GB for macOS and the rest for {rest}",
            free = %free,
            needed = %needed,
            floor = %floor,
            rest = %rest
        )
    }
}

/// The share an integrated GPU would take of `free` bytes with the guest's
/// floor left aside: a [`GPU_SHARE`]th, within [`GPU_FLOOR`]..=[`GPU_CEILING`].
fn gpu_target(free: u64) -> u64 {
    (free / GPU_SHARE / ALIGNMENT * ALIGNMENT).clamp(GPU_FLOOR, GPU_CEILING)
}

/// The sizes for `free` host bytes and `runners` of the `cores` executors
/// running PEs; the others add only their parked reserves.
///
/// Each part takes a share of `free`, between its bounds:
/// - the heap a twenty-fourth, [`HEAP_PER_WINDOW`] byte per byte of the board's
///   code window and [`PE_HEAP`] per runner, within
///   [`HEAP_FLOOR`]..=[`HEAP_CEILING`], the floor moved by the window's
///   difference from [`FLOOR_WINDOW`]; below [`HEAP_TARGET`] (moved the
///   same way), as much more as leaves the guest [`GUEST_ROOM`];
/// - the code arena each executor's own part and the board's window: free
///   RAM / 16 where the guest keeps [`GUEST_ROOM`] beside it and the heap's
///   target, as much of it as does on hosts between, and no less than
///   [`WINDOW_PER_RUNNER`] per runner, or [`SMALL_WINDOW_PER_RUNNER`] where
///   that would leave the guest under its floor;
/// - the pool, for each of up to [`POOL_SHARERS`] runners, 1/[`POOL_SHARE`]
///   of the guest's DRAM, which is what the other parts leave, or its halves'
///   sizes ([`pool_halves`]) if that is less: `pool = sharers * (free - heap -
///   code - pool) / 64`, solved for `pool`; at least [`POOL_RUNNER_FLOOR`] per
///   runner, at most [`POOL_PER_RUNNER`] per sharer.
///
/// A GPU that shares the host's RAM ([`GpuMemory::Shared`]) first takes its
/// share ([`gpu_target`]), and the parts are sized as on a host with that
/// much less: the GPU's memory is no more Stela's to translate in than the
/// guest's. Where that leaves the guest under its floor, the share is the
/// most that does not, down to [`GPU_LEAST`].
///
/// A guest share under [`GUEST_FLOOR`] is a [`Shortfall`].
pub fn sizes(free: u64, cores: usize, runners: usize, gpu: impl Into<GpuPlan>) -> Result<Sizes> {
    let gpu = gpu.into();
    if let Some(sizes) = plan(free, cores, runners, gpu)? {
        return Ok(sizes);
    }
    let mut needed = free.next_multiple_of(NEEDED_STEP);
    while plan(needed, cores, runners, gpu)?.is_none() {
        needed += NEEDED_STEP;
    }
    Err(Shortfall {
        free,
        needed,
        gpu: gpu.memory,
    }
    .refusal())
}

/// [`sizes`], or `None` where the guest's floor (and the GPU's least
/// share) does not fit.
fn plan(free: u64, cores: usize, runners: usize, gpu: GpuPlan) -> Result<Option<Sizes>> {
    let with_room = |share: u64, fallback: u64| -> Result<Option<Sizes>> {
        let sizes = estimate(
            free.saturating_sub(share)
                .saturating_sub(gpu.aperture)
                .saturating_sub(fallback),
            cores,
            runners,
        )?;
        Ok((sizes.guest >= GUEST_FLOOR).then_some(Sizes {
            gpu: share,
            aperture: gpu.aperture,
            fallback,
            ..sizes
        }))
    };
    let with_share = |share: u64| -> Result<Option<Sizes>> {
        match gpu.fallback {
            false => with_room(share, 0),
            true => with_fallback(share, &with_room),
        }
    };
    let target = match gpu.memory {
        GpuMemory::Own => return with_share(0),
        GpuMemory::Shared => gpu_target(free.saturating_sub(gpu.aperture)),
        // The display's pages where the guest keeps its floor beside them;
        // a host too small for both gives them none, and they come from
        // the heap.
        GpuMemory::Display => {
            return Ok(with_share(DISPLAY_SHARE)?.or(with_share(0)?));
        }
    };
    let with_share = |share: u64| with_room(share, 0);
    // The most share up to the target that leaves the guest its floor, in
    // whole alignments; the guest grows with the RAM the parts are sized
    // for, so the first found from the top is the most.
    let mut share = target;
    while share >= GPU_LEAST {
        if let Some(sizes) = with_share(share)? {
            return Ok(Some(sizes));
        }
        share -= ALIGNMENT;
    }
    Ok(None)
}

/// What the heap region lacks of [`HEAP_OWN`] and [`FALLBACK_LEAST`] beside
/// `share` for sizes `sizes`, in whole alignments.
fn fallback_lack(sizes: &Sizes, share: u64) -> u64 {
    (HEAP_OWN + FALLBACK_LEAST)
        .saturating_sub(sizes.heap + share)
        .next_multiple_of(ALIGNMENT)
}

/// The plan with `share` beside the heap and room in the heap's region for
/// an integrated GPU that may draw in a discrete GPU's place: what the heap
/// and the share lack of [`HEAP_OWN`] and [`FALLBACK_LEAST`], found as the
/// heap shrinks with the RAM the reserve takes, or the most of it that
/// leaves the guest its floor; `with_room` plans a share and a reserve.
fn with_fallback(
    share: u64,
    with_room: &dyn Fn(u64, u64) -> Result<Option<Sizes>>,
) -> Result<Option<Sizes>> {
    let Some(mut sizes) = with_room(share, 0)? else {
        return Ok(None);
    };
    // The heap shrinks, or stays, as the reserve grows, so the lack grows
    // to a fixed point from below, at most the whole of it.
    let mut reserve = 0;
    for _ in 0..=(HEAP_OWN + FALLBACK_LEAST) / ALIGNMENT {
        let lack = fallback_lack(&sizes, share);
        if lack <= reserve {
            return Ok(Some(sizes));
        }
        reserve = lack;
        match with_room(share, reserve)? {
            Some(next) => sizes = next,
            None => break,
        }
    }
    // The whole reserve leaves the guest under its floor: the most that
    // does not.
    while reserve > ALIGNMENT {
        reserve -= ALIGNMENT;
        if let Some(sizes) = with_room(share, reserve)? {
            return Ok(Some(sizes));
        }
    }
    with_room(share, 0)
}

/// The bytes of persistent translation pack the loader may read for
/// `free` host bytes, measured just before it reads the pack, with
/// `runners` of `cores` executors running PEs: at most a
/// 1/[`PACK_SHARE`] of `free`, and only as much as still leaves [`sizes`]
/// the guest's floor, after the pack and a margin for the loader's other
/// allocations. Zero, below [`PACK_FLOOR`]: the parts and the guest come
/// first, and the pack never makes a boot fail.
///
/// An integrated GPU's whole share ([`gpu_target`] of what the pack leaves)
/// comes before the pack: the pack never makes the GPU's share smaller.
pub fn pack_budget(free: u64, cores: usize, runners: usize, gpu: impl Into<GpuPlan>) -> u64 {
    let gpu = gpu.into();
    let mut budget = free / PACK_SHARE / ALIGNMENT * ALIGNMENT;
    while budget >= PACK_FLOOR {
        let mut left = free.saturating_sub(budget + PACK_MARGIN + gpu.aperture);
        let share = match gpu.memory {
            GpuMemory::Shared => gpu_target(left),
            GpuMemory::Display => DISPLAY_SHARE,
            GpuMemory::Own => 0,
        };
        left = left.saturating_sub(share);
        // The room for an integrated GPU that may draw in a discrete one's
        // place comes before the pack too.
        if gpu.fallback && gpu.memory != GpuMemory::Shared {
            let lack =
                estimate(left, cores, runners).map_or(0, |sizes| fallback_lack(&sizes, share));
            left = left.saturating_sub(lack);
        }
        if estimate_guest(left, cores, runners).is_ok_and(|guest| guest >= GUEST_FLOOR) {
            return budget;
        }
        budget -= ALIGNMENT;
    }
    0
}

fn estimate_guest(free: u64, cores: usize, runners: usize) -> Result<u64> {
    estimate(free, cores, runners).map(|sizes| sizes.guest)
}

/// The sizes with the code window the host's memory allows: the host's
/// share where the guest keeps its room beside it and the heap's target,
/// else the larger window per runner, or the smaller one where the larger
/// would leave the guest under its floor.
fn estimate(free: u64, cores: usize, runners: usize) -> Result<Sizes> {
    let per_runner = |each: u64| estimate_within(free, cores, runners, runners as u64 * each);
    let large = per_runner(WINDOW_PER_RUNNER)?;
    if large.guest < GUEST_FLOOR {
        return per_runner(SMALL_WINDOW_PER_RUNNER);
    }
    // The guest shrinks as the window grows, by the window and its block
    // records: the largest window up to the host's share that leaves the
    // guest its room with the heap on its target, in whole steps.
    let roomy = |window: u64| -> Result<bool> {
        let code = code_reservation_within(free, cores, runners, window)?;
        let shared = shared_heap(free, runners, code.window);
        let heap = shared.max(heap_target(shared, code.window));
        Ok(split(free, heap, code, cores, runners).guest >= GUEST_ROOM)
    };
    let (mut low, mut high) = (
        large.code.window / CODE_ALIGNMENT,
        window_share(free) / CODE_ALIGNMENT,
    );
    if low >= high || !roomy((low + 1) * CODE_ALIGNMENT)? {
        return Ok(large);
    }
    while low < high {
        let middle = (low + high).div_ceil(2);
        if roomy(middle * CODE_ALIGNMENT)? {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    estimate_within(free, cores, runners, low * CODE_ALIGNMENT)
}

/// The heap's target for a `window`, on a host whose share of heap is
/// `shared`: [`HEAP_TARGET`] moved by the window, within the ceiling.
fn heap_target(shared: u64, window: u64) -> u64 {
    at_window(HEAP_TARGET, window).min(HEAP_CEILING.max(shared))
}

/// The sizes with a code window of at most `most` bytes.
fn estimate_within(free: u64, cores: usize, runners: usize, most: u64) -> Result<Sizes> {
    let code = code_reservation_within(free, cores, runners, most)?;
    let shared = shared_heap(free, runners, code.window);
    let base = split(free, shared, code, cores, runners);
    let target = heap_target(shared, code.window);
    if target <= shared || base.guest <= GUEST_ROOM {
        return Ok(base);
    }
    // The guest shrinks as the heap grows: the most heap up to the target
    // that leaves the guest its room, in whole alignments.
    let fits = |heap: u64| split(free, heap, code, cores, runners).guest >= GUEST_ROOM;
    let (mut low, mut high) = (shared / ALIGNMENT, target / ALIGNMENT);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(middle * ALIGNMENT) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok(split(free, low * ALIGNMENT, code, cores, runners))
}

/// `bytes` measured with [`FLOOR_WINDOW`], moved by [`HEAP_PER_WINDOW`] per
/// byte a `window` differs from it, in whole alignments.
fn at_window(bytes: u64, window: u64) -> u64 {
    (bytes + HEAP_PER_WINDOW * window).saturating_sub(HEAP_PER_WINDOW * FLOOR_WINDOW) / ALIGNMENT
        * ALIGNMENT
}

/// The heap's share of `free` for `runners` PEs and a `window`, between its
/// floor and ceiling.
fn shared_heap(free: u64, runners: usize, window: u64) -> u64 {
    let heap = free / HEAP_SHARE + HEAP_PER_WINDOW * window + runners as u64 * PE_HEAP;
    let floor = at_window(HEAP_FLOOR, window);
    (heap / ALIGNMENT * ALIGNMENT).clamp(floor, HEAP_CEILING.max(floor))
}

/// The pool and the guest for `heap` and `code`, of `free`.
fn split(free: u64, heap: u64, code: CodeReservation, cores: usize, runners: usize) -> Sizes {
    let running = runners as u64;
    let rest = free.saturating_sub(heap).saturating_sub(code.bytes);
    let sharers = u128::from(running.min(POOL_SHARERS));
    let rest = u128::from(rest);
    // A sixty-fourth of the guest each, or both halves' sizes if less:
    // pool = sharers * (bases + guest / 2048 + guest / 1024), guest = rest - pool.
    let share = sharers * rest / (u128::from(POOL_SHARE) + sharers);
    let (lower, upper) = (u128::from(LOWER_GROWTH), u128::from(UPPER_GROWTH));
    let base = u128::from(LOWER_BASE + UPPER_BASE);
    let parts = lower / upper + 1;
    let grown = sharers * (lower * base + parts * rest) / (lower + parts * sharers);
    let pool = share.min(grown) as u64;
    let (rest, sharers) = (rest as u64, sharers as u64);
    let pool = (pool / ALIGNMENT * ALIGNMENT).clamp(
        POOL_FLOOR.max(running * POOL_RUNNER_FLOOR),
        sharers * POOL_PER_RUNNER,
    );
    let (_, parked) = parked_reserve(cores, runners);
    let pool = pool + parked;
    Sizes {
        heap,
        code,
        pool,
        guest: rest.saturating_sub(pool),
        gpu: 0,
        aperture: 0,
        fallback: 0,
    }
}

/// The bytes a runner's pool keeps for each guest half's shadow tables, for
/// `guest` bytes of guest DRAM: the lower half's (user address spaces) and
/// the upper half's (the kernel). [`sizes`] sizes the pool as their sum.
pub fn pool_halves(guest: u64) -> (u64, u64) {
    (
        LOWER_BASE + guest / LOWER_GROWTH,
        UPPER_BASE + guest / UPPER_GROWTH,
    )
}

/// The bytes of a runner's `shadow` frames (its pool less the permanent
/// roots) that the upper half takes, in whole `page`s, for `guest` bytes of
/// guest DRAM: its [`pool_halves`] size, or its share of the part in their
/// sizes' ratio where the part is smaller than both (a pool sized as a
/// sixty-fourth of a small guest, or the pool's rounding); at least a page.
pub fn upper_half(shadow: u64, guest: u64, page: u64) -> u64 {
    let (lower, upper) = pool_halves(guest);
    let share = (u128::from(shadow) * u128::from(upper) / u128::from(lower + upper)) as u64;
    (upper.min(share) / page * page).max(page)
}

#[cfg(test)]
mod tests;
