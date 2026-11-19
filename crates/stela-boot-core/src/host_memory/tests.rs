use super::*;
use alloc::{format, string::ToString};

/// Free RAM a PC of `installed` GiB shows Stela: firmware keeps about 3 %.
fn free(installed: u64) -> u64 {
    installed * GIB / 32 * 31
}

#[test]
fn every_host_size_keeps_the_parts_in_bounds_and_the_guest_above_its_floor() {
    for (installed, runners) in [
        (8, 2),
        (8, 4),
        (8, 8),
        (16, 8),
        (32, 8),
        (128, 8),
        (128, 64),
        (2048, 8),
        (2048, 64),
    ] {
        let free = free(installed);
        let sizes = sizes(free, runners, runners, GpuMemory::Own).unwrap();
        let floor = HEAP_FLOOR + sizes.code.window - FLOOR_WINDOW;
        assert!((floor..=HEAP_CEILING.max(floor)).contains(&sizes.heap));
        assert!(sizes.guest >= GUEST_FLOOR);
        assert_eq!(
            sizes.heap + sizes.code.bytes + sizes.pool + sizes.guest,
            free
        );
        // The pool maps each sharer's guest DRAM once, or its base and a
        // halves' sizes if less, up to its cap.
        let sharers = (runners as u64).min(POOL_SHARERS);
        let (lower, upper) = pool_halves(sizes.guest);
        let each = (sizes.guest / POOL_SHARE).min(lower + upper);
        let wanted = sharers * each;
        let cap = sharers * POOL_PER_RUNNER;
        let floor = POOL_FLOOR.max(runners as u64 * POOL_RUNNER_FLOOR);
        assert!(
            sizes.pool == cap || sizes.pool + ALIGNMENT * 2 >= wanted,
            "{installed} GiB"
        );
        assert!(sizes.pool <= wanted.max(floor) || sizes.pool == cap);
    }
}

#[test]
fn an_8_gib_host_gives_the_guest_what_the_parts_leave() {
    let sizes = sizes(free(8), 8, 8, GpuMemory::Own).unwrap();
    // The floor less the 16 MiB the 496 MiB window lacks of 512 MiB.
    assert_eq!(sizes.code.window, 496 * MIB);
    assert_eq!(sizes.heap, HEAP_FLOOR - 16 * MIB);
    assert_eq!(sizes.code.bytes, 512 * MIB);
    // 8 runners' sixty-fourths of the guest's 4 GiB, not of all 7.75 GiB.
    assert_eq!(sizes.pool, 512 * MIB);
    assert_eq!(sizes.guest, 4112 * MIB);
}

#[test]
fn a_16_gib_host_takes_a_sixteenth_for_its_window_and_the_heap_target() {
    let sizes = sizes(free(16), 8, 8, GpuMemory::Own).unwrap();
    // A sixteenth of its 15.5 GiB free (992 MiB, past 8 runners' 96 MiB
    // each), and each executor's 2 MiB of its own.
    assert_eq!(sizes.code.window, 992 * MIB);
    assert_eq!(sizes.code.bytes, 1008 * MIB);
    // The target plus a byte per window byte past 512 MiB: an eighth, the
    // window and 16 MiB per PE are under it, and the guest keeps its room.
    assert_eq!(sizes.heap, HEAP_TARGET + 480 * MIB);
    assert!(sizes.guest > 9 * GIB);
}

/// The guest's working set of code does not shrink with its PEs, so a host
/// whose guest keeps its room takes a sixteenth of its free RAM for the
/// window whatever its PEs, with the heap on its target for that window:
/// 2 PEs took 192 MiB and 4 took 384 MiB on any host.
#[test]
fn the_window_follows_the_hosts_memory_not_its_pes() {
    for (installed, window) in [(16, 992), (32, 1984), (48, 2048), (128, 2048)] {
        for runners in [1, 2, 4, 6, 8, 12] {
            let free = free(installed);
            let sizes = sizes(free, runners, runners, GpuMemory::Own).unwrap();
            assert_eq!(
                sizes.code.window,
                window * MIB,
                "{installed} GiB, {runners} PEs"
            );
            let shared = shared_heap(free, runners, sizes.code.window);
            assert_eq!(
                sizes.heap,
                shared.max(heap_target(shared, sizes.code.window))
            );
            assert!(sizes.guest >= GUEST_ROOM, "{installed} GiB, {runners} PEs");
        }
    }
    // An 8 GiB host's guest is under its room: each runner's share, or the
    // host's where that is less, as before.
    for (runners, window) in [(2, 192), (4, 384), (8, 496)] {
        let sizes = sizes(free(8), runners, runners, GpuMemory::Own).unwrap();
        assert_eq!(sizes.code.window, window * MIB, "{runners} PEs");
    }
}

/// On hosts between, a window past its runners' share is the largest that
/// leaves the guest its room with the heap on its target; more RAM never
/// gives a smaller window.
#[test]
fn a_window_past_its_runners_share_leaves_the_guest_its_room() {
    let roomy = |free: u64, runners: usize, sizes: &Sizes| {
        let shared = shared_heap(free, runners, sizes.code.window);
        sizes.guest >= GUEST_ROOM
            && sizes.heap == shared.max(heap_target(shared, sizes.code.window))
    };
    for runners in [1, 2, 4, 8, 16, 24] {
        let (mut last, mut grown) = (0, 0);
        for free in (6 * GIB..72 * GIB).step_by(61 * MIB as usize) {
            let Ok(sizes) = sizes(free, runners, runners, GpuMemory::Own) else {
                continue;
            };
            let window = sizes.code.window;
            assert!(window.is_multiple_of(CODE_ALIGNMENT));
            assert!(window <= window_share(free), "{runners} PEs, {free}");
            assert!(window >= last, "{runners} PEs, {free}");
            last = window;
            let each = runners as u64 * WINDOW_PER_RUNNER;
            let per_runner = estimate_within(free, runners, runners, each).unwrap();
            if window <= per_runner.code.window {
                continue;
            }
            grown += 1;
            assert!(roomy(free, runners, &sizes), "{runners} PEs, {free}");
            if window < window_share(free) {
                let more = estimate_within(free, runners, runners, window + CODE_ALIGNMENT);
                assert!(
                    !roomy(free, runners, &more.unwrap()),
                    "{runners} PEs, {free}"
                );
            }
        }
        // Up to 16 PEs the share passes each runner's on these hosts.
        assert_eq!(grown != 0, runners <= 16, "{runners} PEs");
    }
}

/// A 12 GiB host with 8 cores (11514 MiB free) takes a 704 MiB window,
/// 192 MiB more than 64 MiB per runner gave, and heap toward its target
/// until the guest has only its room.
#[test]
fn a_12_gib_8_core_host_takes_a_larger_window_and_heap_from_its_guest() {
    let sizes = sizes(11514 * MIB, 8, 8, GpuMemory::Own).unwrap();
    assert_eq!(
        [sizes.heap, sizes.code.bytes, sizes.pool, sizes.guest].map(|b| b / MIB),
        [3940, 720, 710, 6144]
    );
    assert_eq!(sizes.guest, GUEST_ROOM);
}

/// A host whose guest the larger window would put under its floor keeps
/// 64 MiB per runner, and boots where it booted before.
#[test]
fn a_host_short_of_the_larger_window_falls_back_to_the_smaller() {
    // Two runners near the floor: 192 MiB of window would leave the guest
    // under 3 GiB, 128 MiB does not.
    let short = (5 * GIB / MIB..8 * GIB / MIB)
        .step_by(16)
        .map(|free| free * MIB)
        .find(|&free| {
            let large = estimate_within(free, 2, 2, 2 * WINDOW_PER_RUNNER).unwrap();
            let small = estimate_within(free, 2, 2, 2 * SMALL_WINDOW_PER_RUNNER).unwrap();
            large.guest < GUEST_FLOOR && small.guest >= GUEST_FLOOR
        })
        .expect("a host the fallback lets boot");
    let sizes = sizes(short, 2, 2, GpuMemory::Own).unwrap();
    assert_eq!(sizes.code.window, 2 * SMALL_WINDOW_PER_RUNNER);
    assert!(sizes.guest >= GUEST_FLOOR);
    // With a little more RAM the larger window fits.
    let roomy = sizes_of(short + 512 * MIB);
    assert_eq!(roomy.code.window, 2 * WINDOW_PER_RUNNER);
}

fn sizes_of(free: u64) -> Sizes {
    sizes(free, 2, 2, GpuMemory::Own).unwrap()
}

#[test]
fn a_64_gib_host_grows_the_heap_with_its_share_window_and_pes() {
    let sizes = sizes(free(64), 8, 8, GpuMemory::Own).unwrap();
    let heap = free(64) / HEAP_SHARE + HEAP_PER_WINDOW * 2048 * MIB + 8 * PE_HEAP;
    assert_eq!(sizes.heap, heap / ALIGNMENT * ALIGNMENT);
    assert!(HEAP_FLOOR < sizes.heap && sizes.heap < HEAP_CEILING);
    // The window's 2 GiB bound, under a sixteenth's 3.9 GiB.
    assert_eq!(sizes.code.bytes, 2064 * MIB);
    // Each runner's halves for the guest's 49 GiB, not a 64th.
    assert_eq!(sizes.pool, 1226 * MIB);
    assert!(sizes.pool < 8 * POOL_PER_RUNNER);
    assert!(sizes.guest > 48 * GIB);
}

/// Heap, code, pool and guest MiB for `runners` PEs at `free` GiB, or None
/// when the guest's floor does not fit.
#[test]
fn stelas_share_stops_growing_per_pe_past_what_each_pe_takes() {
    for (runners, free, expected) in [
        (2, 4, None),
        (2, 8, Some([2496, 196, 166, 5334])),
        (2, 16, Some([4608, 1028, 190, 10558])),
        (2, 30, Some([5792, 1924, 226, 22778])),
        (2, 128, Some([16384, 2052, 488, 112148])),
        (8, 4, None),
        (8, 8, Some([2816, 528, 538, 4310])),
        (8, 16, Some([4608, 1040, 756, 9980])),
        (8, 30, Some([5888, 1936, 896, 22000])),
        (8, 128, Some([16384, 2064, 1936, 110688])),
        (24, 4, None),
        (24, 8, Some([2816, 560, 768, 4048])),
        (24, 16, Some([4608, 1072, 768, 9936])),
        (24, 30, Some([6144, 1968, 894, 21714])),
        (24, 128, Some([16384, 2096, 1936, 110656])),
        (64, 4, None),
        (64, 8, None),
        (64, 16, Some([4608, 1152, 2048, 8576])),
        (64, 30, Some([6784, 2048, 2048, 19840])),
        (64, 128, Some([16384, 2176, 2048, 110464])),
    ] {
        let actual = sizes(free * GIB, runners, runners, GpuMemory::Own)
            .ok()
            .map(|sizes| [sizes.heap, sizes.code.bytes, sizes.pool, sizes.guest].map(|b| b / MIB));
        assert_eq!(actual, expected, "{runners} PEs, {free} GiB");
        if let Some([_, _, pool, guest]) = actual {
            // The pool's total stays within an eighth of the guest, or each
            // runner's floor, whatever the PE count.
            assert!(
                pool <= (guest / 8).max(runners as u64 * 32),
                "{runners} {free}"
            );
        }
    }
}

/// The splits of measured boots' free RAM on 8 PEs, and the guest MiB each
/// boot gave:
/// - the 8 GiB VM, `HOST MEMORY free=7354M heap=2816M code=464M pool=452M
///   guest=3622M`: 58 MiB more guest;
/// - an 8 GB PC (7544 MiB free): 44 MiB more;
/// - the 12 GiB VM, `free=11514M heap=3008M code=720M pool=864M
///   guest=6922M`: 778 MiB less, to the heap's target;
/// - the 16 GiB VM, `free=15710M heap=3280M code=992M pool=1032M
///   guest=10406M`: 998 MiB less, to the heap's target at the window that
///   boot had;
/// - the 32 GB PC, `free=30242M heap=3672M code=1040M pool=2836M
///   guest=22694M`: 1044 MiB less, to a 1888 MiB window and its records.
#[test]
fn measured_boots_split_as_expected() {
    for (free, expected) in [
        (7354, [2752, 464, 458, 3680]),
        (HOST_8_GB / MIB, [2768, 480, 476, 3820]),
        (11514, [3940, 720, 710, 6144]),
        (15710, [4560, 992, 750, 9408]),
        (30242, [5796, 1904, 892, 21650]),
    ] {
        let sizes = sizes(free * MIB, 8, 8, GpuMemory::Own).unwrap();
        assert_eq!(
            [sizes.heap, sizes.code.bytes, sizes.pool, sizes.guest].map(|b| b / MIB),
            expected,
            "{free} MiB free"
        );
    }
}

/// Installed GiB and cores to the split, and the guest MiB the sizing of
/// v0.1.0 gave (8 GiB: +372 at 2 cores, +226 at 4, +16 at 8; 64 GiB: +302;
/// 128 GiB: +628). 16 and 32 GiB hosts give the heap its target and the
/// window the host's share from the guest instead (16 GiB: -1882 at 4
/// cores, -746 at 8; 32 GiB: -1162).
#[test]
fn host_sizes_split_as_expected() {
    for (installed, cores, expected, before) in [
        (8, 2, [2496, 196, 158, 5086], 4714),
        (8, 4, [2688, 392, 284, 4572], 4346),
        (8, 8, [2800, 512, 512, 4112], 4096),
        (16, 4, [4576, 1000, 378, 9918], 11800 - 1882),
        (16, 8, [4576, 1008, 750, 9538], 10284 - 746),
        (32, 8, [6080, 2000, 906, 22758], 23920 - 1162),
        (64, 8, [10112, 2064, 1226, 50086], 49784),
        (128, 8, [16384, 2064, 1888, 106640], 106012),
    ] {
        let sizes = sizes(free(installed), cores, cores, GpuMemory::Own).unwrap();
        let split = [sizes.heap, sizes.code.bytes, sizes.pool, sizes.guest].map(|b| b / MIB);
        assert_eq!(split, expected, "{installed} GiB, {cores} cores");
        assert!(split[3] >= before, "{installed} GiB, {cores} cores");
        // From 16 GiB up each runner's user half holds at least the 52 MiB
        // the 12 GiB VM filled, beside its kernel half.
        if installed >= 16 {
            let per = sizes.pool / cores as u64;
            assert!(per - upper_half(per, sizes.guest, 4096) >= 52 * MIB);
        }
    }
}

/// The 16 GiB PC that ran on the floor (`HOST MEMORY free=13590M heap=3072M
/// code=784M pool=744M guest=8990M`) and failed allocations with the heap
/// 2.6 GiB live gets the target at its share's window, more than a GiB
/// above the 2924 and 3168 MiB the 16 and 32 GiB PCs carved at most, and
/// the guest keeps its room.
#[test]
fn a_16_gib_pc_takes_the_heap_target_from_its_guest() {
    let sizes = sizes(13590 * MIB, 8, 8, GpuMemory::Own).unwrap();
    assert_eq!(sizes.code.window, 848 * MIB);
    assert_eq!(sizes.heap, HEAP_TARGET + 336 * MIB);
    assert!(sizes.heap >= 3168 * MIB + GIB);
    assert!(sizes.guest >= GUEST_ROOM, "{sizes:?}");
}

/// What a 16 GiB PC's 8-hour session needed of its heap (issue 154's log,
/// 8 PEs, a 768 MiB window): 4063 MiB live at its peak, NAK's compile of
/// Core Animation's generic shader on top of 3.3 GiB, and room for the
/// largest allocation a boot has asked for (267 386 880 bytes, which a 3072
/// MiB heap with 192 MiB extents failed).
const LONG_SESSION_PEAK: u64 = 4063 * MIB;
const PEAK_WINDOW: u64 = 768 * MIB;
const LARGEST_ASKED: u64 = 267_386_880;

/// On 8 to 64 GiB hosts with 4 to 32 cores, a heap whose guest keeps more
/// than its room (the target or the host's share) holds that session's
/// peak and largest allocation, moved by the window's block records; a
/// guest at its room keeps it, so 12 GiB hosts stay short of it.
#[test]
fn hosts_past_the_guests_room_hold_a_long_sessions_heap_peak() {
    for installed in [8, 12, 16, 24, 32, 64] {
        for cores in [4, 8, 12, 16, 24, 32] {
            let free = free(installed);
            let sizes = sizes(free, cores, cores, GpuMemory::Own).unwrap();
            assert_eq!(
                sizes.heap + sizes.code.bytes + sizes.pool + sizes.guest,
                free
            );
            assert!(sizes.guest >= GUEST_FLOOR, "{installed} GiB, {cores} cores");
            let need = (LONG_SESSION_PEAK + LARGEST_ASKED + HEAP_PER_WINDOW * sizes.code.window)
                .saturating_sub(HEAP_PER_WINDOW * PEAK_WINDOW);
            if sizes.guest > GUEST_ROOM {
                assert!(
                    sizes.heap >= need,
                    "{installed} GiB, {cores} cores: {sizes:?}"
                );
                // The target and the need move alike with the window: a
                // host on the target keeps 34 MiB past it.
                if sizes.heap == at_window(HEAP_TARGET, sizes.code.window) {
                    assert_eq!(sizes.heap - need, 34 * MIB);
                }
            } else {
                assert!(installed <= 12, "{installed} GiB, {cores} cores");
            }
        }
    }
    // The 16 GiB host on 8 PEs is on the target, moved from the session's
    // window to its share's.
    let sizes = sizes(free(16), 8, 8, GpuMemory::Own).unwrap();
    let moved = HEAP_PER_WINDOW * (sizes.code.window - PEAK_WINDOW);
    assert_eq!(
        sizes.heap - (LONG_SESSION_PEAK + LARGEST_ASKED + moved),
        34 * MIB
    );
}

/// The target only takes from a guest above its room, never below it, and
/// never gives a host less heap than its share or floor.
#[test]
fn the_heap_target_leaves_the_guest_its_room() {
    for runners in [1, 2, 4, 8, 16, 24, 64] {
        for free in (6 * GIB..40 * GIB).step_by(61 * MIB as usize) {
            let Ok(sizes) = sizes(free, runners, runners, GpuMemory::Own) else {
                continue;
            };
            let shared = shared_heap(free, runners, sizes.code.window);
            let target = at_window(HEAP_TARGET, sizes.code.window);
            assert!(sizes.heap >= shared, "{runners} PEs, {free}");
            assert!(sizes.heap <= shared.max(target), "{runners} PEs, {free}");
            assert!(sizes.heap.is_multiple_of(ALIGNMENT));
            if sizes.heap > shared {
                assert!(sizes.guest >= GUEST_ROOM, "{runners} PEs, {free}");
                // Short of the target only where one alignment more would
                // take the guest under its room.
                if sizes.heap < target {
                    let more = split(free, sizes.heap + ALIGNMENT, sizes.code, runners, runners);
                    assert!(more.guest < GUEST_ROOM, "{runners} PEs, {free}");
                }
            }
        }
    }
}

#[test]
fn an_8_gib_host_keeps_its_heap_floor() {
    for runners in [2, 4, 8] {
        let sizes = sizes(free(8), runners, runners, GpuMemory::Own).unwrap();
        assert_eq!(sizes.heap, shared_heap(free(8), runners, sizes.code.window));
        assert!(sizes.guest < GUEST_ROOM);
    }
}

#[test]
fn a_2_tib_host_caps_every_part() {
    let sizes = sizes(free(2048), 8, 8, GpuMemory::Own).unwrap();
    assert_eq!(sizes.heap, HEAP_CEILING);
    assert_eq!(sizes.code.bytes, 2064 * MIB);
    assert_eq!(sizes.pool, 8 * POOL_PER_RUNNER);
    assert_eq!(sizes.guest, free(2048) - 24 * GIB - 2064 * MIB);
}

#[test]
fn the_heap_grows_with_the_host_between_its_bounds() {
    let small = sizes(free(32), 8, 8, GpuMemory::Own).unwrap().heap;
    let large = sizes(free(64), 8, 8, GpuMemory::Own).unwrap().heap;
    assert!(HEAP_FLOOR < small && small < large && large < HEAP_CEILING);
}

#[test]
fn too_little_ram_says_how_much_is_needed() {
    let refusal = sizes(4 * GIB, 8, 8, GpuMemory::Own).unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "this PC has 4.0 GB of RAM free; Stela needs at least 7 GB free: \
         3 GB for macOS and the rest for its own memory"
    );
    let two_cores = sizes(5 * GIB, 2, 2, GpuMemory::Own).unwrap_err();
    assert!(
        two_cores.to_string().contains("at least 6 GB"),
        "{two_cores}"
    );
    assert!(sizes(6 * GIB, 2, 2, GpuMemory::Own).is_ok());
    assert!(sizes(free(8), 2, 2, GpuMemory::Own).is_ok());
}

#[test]
fn parked_executors_add_only_their_reserves() {
    let parked = sizes(free(16), 12, 8, GpuMemory::Own).unwrap();
    let rest = free(16) - parked.heap - parked.code.bytes;
    let share = 8 * rest / (POOL_SHARE + 8);
    let grown = 8 * (2048 * (LOWER_BASE + UPPER_BASE) + 3 * rest) / (2048 + 3 * 8);
    let running = share.min(grown) / ALIGNMENT * ALIGNMENT;
    assert_eq!(parked.pool, running + 4 * 4 * MIB);
}

/// The free RAM an 8 GB host with eight cores shows the loader with no pack
/// read (`HOST MEMORY free=7544M`).
const HOST_8_GB: u64 = 7544 * MIB;

#[test]
fn an_8_gb_pc_keeps_a_sixteenth_for_the_pack() {
    let budget = pack_budget(HOST_8_GB, 8, 8, GpuMemory::Own);
    assert_eq!(budget, 470 * MIB);
    // An 845 MB pack, which stopped such a host's boots, no longer fits whole,
    // and what is read of it leaves the parts and the guest's floor.
    assert!(budget < 845_496_619);
    let sizes = sizes(HOST_8_GB - budget - PACK_MARGIN, 8, 8, GpuMemory::Own).unwrap();
    assert!(sizes.guest > GUEST_FLOOR + 200 * MIB, "{sizes:?}");
    assert_eq!(pack_budget(free(8), 8, 8, GpuMemory::Own), 496 * MIB);
}

#[test]
fn a_16_gb_pc_keeps_about_a_gigabyte_for_the_pack() {
    assert_eq!(pack_budget(free(16), 8, 8, GpuMemory::Own), 992 * MIB);
}

#[test]
fn a_128_gb_pc_keeps_more_for_the_pack() {
    let budget = pack_budget(free(128), 16, 16, GpuMemory::Own);
    assert_eq!(budget, 7936 * MIB);
    let sizes = sizes(free(128) - budget - PACK_MARGIN, 16, 16, GpuMemory::Own).unwrap();
    assert!(sizes.guest > 72 * GIB, "{sizes:?}");
}

#[test]
fn the_pack_gets_only_what_the_parts_and_the_guest_leave() {
    // Just above what eight cores need, the pack gets the rest, not its
    // share; below it, nothing, and the boot goes on without a pack.
    let needed = (6000..7200)
        .map(|mib| mib * MIB)
        .find(|&free| sizes(free, 8, 8, GpuMemory::Own).is_ok())
        .unwrap();
    assert_eq!(pack_budget(needed, 8, 8, GpuMemory::Own), 0);
    // 100 MiB more leaves the pack about that much, less the margin: the
    // most that still fits.
    let budget = pack_budget(needed + 100 * MIB, 8, 8, GpuMemory::Own);
    assert!((PACK_FLOOR..=100 * MIB).contains(&budget), "{budget}");
    assert!(
        sizes(
            needed + 100 * MIB - budget - PACK_MARGIN,
            8,
            8,
            GpuMemory::Own
        )
        .is_ok()
    );
    assert!(
        sizes(
            needed + 100 * MIB - budget - ALIGNMENT - PACK_MARGIN,
            8,
            8,
            GpuMemory::Own
        )
        .is_err()
    );
    assert_eq!(pack_budget(4 * GIB, 8, 8, GpuMemory::Own), 0);
    for cores in [1, 2, 4, 8, 16, 64] {
        for free in (2 * GIB..40 * GIB).step_by(37 * MIB as usize) {
            let budget = pack_budget(free, cores, cores, GpuMemory::Own);
            assert!(budget.is_multiple_of(ALIGNMENT));
            assert!(budget <= free / PACK_SHARE);
            if budget != 0 {
                assert!(budget >= PACK_FLOOR);
                let left = free - budget - PACK_MARGIN;
                assert!(
                    sizes(left, cores, cores, GpuMemory::Own).is_ok(),
                    "{free} {cores}"
                );
            }
        }
    }
}

#[test]
fn the_guest_reports_the_dram_that_backs_it_not_the_hosts() {
    // The 8 GiB bench: 3920 MiB backed of 8190 MiB installed.
    let dram = guest_dram(0x1_ffe0_0000, 0xf500_0000);
    assert_eq!(dram.reported, 0xf500_0000);
    assert_eq!(dram.kept, 0x1_0ae0_0000);
    // `memSize` (the managed range above the carveouts and below the
    // framebuffer) stays within it, and tag storage sized from it lies at
    // the managed range's top.
    let dram_base = 0x100_0000_0000;
    let phys_base = dram_base + 32 * MIB;
    let managed = dram.reported - 32 * MIB - 16 * MIB;
    let value = stela_contracts::tag_storage::hand_off(phys_base, managed, dram.reported).unwrap();
    let base = stela_contracts::tag_storage::base(value);
    assert!(base >= phys_base);
    assert!(base + stela_contracts::tag_storage::bytes(dram.reported) <= phys_base + managed);
    // A host that backs everything it has keeps nothing.
    assert_eq!(guest_dram(8 * GIB, 8 * GIB).kept, 0);
}

/// Each runner's pool splits into the kernel half's 32 MiB and 1024th of
/// the guest and the user half's 48 MiB and 2048th; a pool smaller than
/// both (a sixty-fourth of a small guest, at 8 GiB) splits in their ratio.
#[test]
fn each_runners_pool_gives_the_kernel_half_what_it_needs_and_the_user_half_the_rest() {
    const PAGE: u64 = 4096;
    const ROOTS: u64 = 64 * PAGE;
    for (installed, sized) in [(8, false), (12, true), (16, true), (64, true)] {
        let sizes = sizes(free(installed), 8, 8, GpuMemory::Own).unwrap();
        let shadow = sizes.pool / 8 / PAGE * PAGE - ROOTS;
        let upper = upper_half(shadow, sizes.guest, PAGE);
        let lower = shadow - upper;
        let (lower_wanted, upper_wanted) = pool_halves(sizes.guest);
        assert!(upper.is_multiple_of(PAGE));
        assert!(upper <= upper_wanted, "{installed} GiB");
        if sized {
            // Both sizes, less the roots and the pool's 2 MiB rounding.
            assert!(upper + ROOTS + 2 * MIB >= upper_wanted, "{installed} GiB");
            assert!(lower + ROOTS + 2 * MIB >= lower_wanted, "{installed} GiB");
        } else {
            // Short of both: each the part's share in their ratio.
            let ratio = |half: u64| half as f64 / (lower_wanted + upper_wanted) as f64;
            let share = |bytes: u64| bytes as f64 / shadow as f64;
            assert!(
                (share(upper) - ratio(upper_wanted)).abs() < 0.01,
                "{installed} GiB"
            );
            assert!(lower > upper, "{installed} GiB");
        }
    }
    // The 16 GiB host's kernel half: 32 MiB and a MiB per GiB of its
    // 9.7 GiB guest.
    let sizes = sizes(free(16), 8, 8, GpuMemory::Own).unwrap();
    let upper = upper_half(sizes.pool / 8 / 4096 * 4096 - ROOTS, sizes.guest, PAGE);
    assert!((41..=42).contains(&(upper / MIB)));
    // A tiny pool's halves never go below a page each.
    assert_eq!(upper_half(2 * PAGE, 64 * GIB, PAGE), PAGE);
}

/// Heap, code, pool, guest and GPU share MiB for hosts whose Metal device is
/// an integrated GPU, at 8 PEs:
/// - the 8 GiB VM (`HOST MEMORY free=7354M`) and an 8 GiB host keep the
///   guest's floor and give the GPU what is left above it, 780 and 1362 MiB,
///   short of [`GPU_FLOOR`];
/// - the 12 GiB VM of the AMD port's first run (`free=10842M heap=3300M
///   code=688M pool=710M guest=6144M`, where the heap ran out holding the
///   GPU's pages) gives it the floor, 2.5 GiB, beside its 512 MiB carve-out;
///   its heap is that of a host with 8.1 GiB free, the heap's floor.
///   A 16 GiB VM's heap peaked at 4482 MiB with 2058 MiB of driver heaps in
///   a session of seven app launches, so its own part held 2.4 GiB, or up
///   to 2.9 GiB where the carve-out held some of the driver's buffers;
/// - 16 and 32 GiB hosts give it a sixth, the heap its target and the guest
///   its room.
#[test]
fn integrated_gpu_hosts_split_as_expected() {
    let hosts = [7354 * MIB, free(8), 10842 * MIB, free(16), free(32)];
    let split: alloc::vec::Vec<_> = hosts
        .iter()
        .map(|&free| {
            let sizes = sizes(free, 8, 8, GpuMemory::Shared).unwrap();
            [
                sizes.heap,
                sizes.code.bytes,
                sizes.pool,
                sizes.guest,
                sizes.gpu,
            ]
            .map(|b| b / MIB)
        })
        .collect();
    assert_eq!(
        split,
        [
            [2704, 416, 382, 3072, 780],
            [2704, 416, 382, 3072, 1362],
            [2816, 528, 548, 4390, 2560],
            [4400, 832, 724, 7272, 2644],
            [5232, 1664, 858, 18700, 5290],
        ]
    );
    // The same hosts with a GPU of its own give it nothing.
    for free in hosts {
        assert_eq!(sizes(free, 8, 8, GpuMemory::Own).unwrap().gpu, 0);
    }
}

/// On every host an integrated GPU takes a sixth within its bounds, the
/// parts are sized as on a host with that much less, and the share is short
/// of its target only where more would put the guest under its floor.
#[test]
fn an_integrated_gpus_share_comes_beside_the_parts() {
    for runners in [2, 4, 8, 16, 24] {
        for free in (6 * GIB..140 * GIB).step_by(173 * MIB as usize) {
            let Ok(shared) = sizes(free, runners, runners, GpuMemory::Shared) else {
                continue;
            };
            let gpu = shared.gpu;
            assert!((GPU_LEAST..=GPU_CEILING).contains(&gpu), "{runners} {free}");
            assert!(gpu.is_multiple_of(ALIGNMENT));
            assert!(shared.guest >= GUEST_FLOOR);
            assert_eq!(
                shared.heap + shared.code.bytes + shared.pool + shared.guest + gpu,
                free
            );
            let rest = sizes(free - gpu, runners, runners, GpuMemory::Own).unwrap();
            assert_eq!(Sizes { gpu, ..rest }, shared, "{runners} {free}");
            let target = (free / GPU_SHARE / ALIGNMENT * ALIGNMENT).clamp(GPU_FLOOR, GPU_CEILING);
            assert!(gpu <= target);
            if gpu < target {
                let more = estimate(free - gpu - ALIGNMENT, runners, runners).unwrap();
                assert!(more.guest < GUEST_FLOOR, "{runners} {free}");
            }
        }
    }
    assert_eq!(gpu_target(free(128)), GPU_CEILING);
}

/// A host that cannot give an integrated GPU its least share beside the
/// guest's floor is refused, saying how much it needs, as one short of the
/// guest's floor is; it needs a little more than with a discrete GPU.
#[test]
fn a_host_short_of_an_integrated_gpus_least_share_is_refused() {
    let needed = (6000..8000)
        .map(|mib| mib * MIB)
        .find(|&free| sizes(free, 8, 8, GpuMemory::Shared).is_ok())
        .unwrap();
    assert_eq!(
        sizes(needed, 8, 8, GpuMemory::Shared).unwrap().gpu,
        GPU_LEAST
    );
    let own = (6000..8000)
        .map(|mib| mib * MIB)
        .find(|&free| sizes(free, 8, 8, GpuMemory::Own).is_ok())
        .unwrap();
    assert!(needed > own && needed - own <= GPU_LEAST + ALIGNMENT * 8);
    let refusal = sizes(6 * GIB, 8, 8, GpuMemory::Shared).unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "this PC has 6.0 GB of RAM free; Stela needs at least 7 GB free: 3 GB for macOS \
         and the rest for its own memory and the graphics built into the processor"
    );
}

/// The pack never takes from an integrated GPU's share: what it leaves still
/// gives the GPU its whole target.
#[test]
fn the_pack_leaves_an_integrated_gpu_its_share() {
    for free in [free(8), 10842 * MIB + 554 * MIB, free(16), free(32)] {
        let budget = pack_budget(free, 8, 8, GpuMemory::Shared);
        assert!(budget <= pack_budget(free, 8, 8, GpuMemory::Own));
        if budget != 0 {
            let left = free - budget - PACK_MARGIN;
            let sizes = sizes(left, 8, 8, GpuMemory::Shared).unwrap();
            assert_eq!(sizes.gpu, gpu_target(left), "{free}");
        }
    }
}

/// An integrated GPU that only shows the display beside a drawing discrete
/// GPU adds its pages' part to the heap, as a host with that much less would
/// size the rest, and bounds nothing; a host too small for both keeps the
/// guest's floor and gives it none.
#[test]
fn a_display_only_integrated_gpu_adds_its_pages_to_the_heap() {
    for free in [7354 * MIB, free(8), 10842 * MIB, free(16), free(32)] {
        let display = sizes(free, 8, 8, GpuMemory::Display).unwrap();
        match display.gpu {
            DISPLAY_SHARE => {
                let rest = sizes(free - DISPLAY_SHARE, 8, 8, GpuMemory::Own).unwrap();
                assert_eq!(
                    Sizes {
                        gpu: DISPLAY_SHARE,
                        ..rest
                    },
                    display,
                    "{free}"
                );
            }
            gpu => {
                assert_eq!(gpu, 0, "{free}");
                assert_eq!(display, sizes(free, 8, 8, GpuMemory::Own).unwrap());
            }
        }
    }
    // The 12 GiB VM's plan beside a drawing discrete GPU.
    let display = sizes(10842 * MIB, 8, 8, GpuMemory::Display).unwrap();
    assert_eq!(
        [display.heap, display.guest, display.gpu].map(|b| b / MIB),
        [3060, 6144, 256]
    );
    assert!(!GpuMemory::Display.limits_pages());
    assert!(!GpuMemory::Own.limits_pages());
    assert!(GpuMemory::Shared.limits_pages());
}

/// The plan's line states the rule it chose, with the page bound and the
/// aperture's tables.
#[test]
fn the_plans_line_states_its_rule() {
    let display = Sizes {
        heap: 3060 * MIB,
        gpu: DISPLAY_SHARE,
        ..sizes(10842 * MIB, 8, 8, GpuMemory::Own).unwrap()
    };
    let bounded = |memory| GpuPlan {
        memory,
        aperture: 0,
        bounded: true,
        fallback: false,
    };
    assert_eq!(
        bounded(GpuMemory::Display).describe(&display),
        "a discrete GPU draws and the integrated GPU shows the display: the heap takes 256M \
         more for the integrated GPU's driver and scanout; Linux's pages, the discrete GPU's \
         system-memory buffers among them, are at most 892M, what the 3316M heap holds past \
         its own 2424M (at least 384M), TTM's limit the same; should the discrete GPU not \
         start and the integrated one draw, its Metal device works in those 892M"
    );
    let shared = Sizes {
        gpu: 2560 * MIB,
        ..display
    };
    assert!(
        bounded(GpuMemory::Shared)
            .describe(&shared)
            .contains("share of 2560M")
    );
    let own = Sizes {
        heap: 3300 * MIB,
        gpu: 0,
        aperture: 34 * MIB,
        ..display
    };
    assert_eq!(
        bounded(GpuMemory::Own).describe(&own),
        "no integrated GPU starts: Linux's pages, a discrete GPU's system-memory buffers and \
         its evictions from VRAM among them, come from the heap, at most 876M of them, what \
         the 3300M heap holds past its own 2424M (at least 384M); TTM's limit is the same, \
         and an allocation past it fails as the GPU's memory being full; the heap takes 34M \
         more for the page tables of the device-memory apertures drivers map whole"
    );
}

/// Linux's pages are held to the bound only where every GPU that starts has
/// a driver the heap's own need was measured beside: an NVIDIA PC keeps the
/// whole heap as Linux's RAM and TTM's own limit, as before the bound, and
/// says so; an integrated GPU's share holds Linux whatever the drivers.
#[test]
fn a_bound_holds_linux_only_where_its_heap_need_was_measured() {
    for installed in [8, 16, 32] {
        for memory in [GpuMemory::Own, GpuMemory::Display, GpuMemory::Shared] {
            let sizes = sizes(free(installed), 8, 8, memory).unwrap();
            for bounded in [false, true] {
                let plan = GpuPlan {
                    memory,
                    aperture: 0,
                    bounded,
                    fallback: false,
                };
                let held = held_bound(&sizes, plan);
                match (memory, bounded) {
                    (GpuMemory::Shared, _) => assert_eq!(held, Some(sizes.gpu)),
                    (_, true) => assert_eq!(held, Some(page_bound(&sizes, memory))),
                    (_, false) => assert_eq!(held, None, "{installed} {memory:?}"),
                }
            }
        }
    }
    // The plan's line says the pages have no bound of their own, and an
    // integrated GPU that draws in a discrete one's place is budgeted by it.
    let own = sizes(free(16), 8, 8, GpuMemory::Own).unwrap();
    let unbounded = GpuPlan::from(GpuMemory::Own);
    assert!(!unbounded.bounded);
    assert_eq!(
        unbounded.describe(&own),
        "no integrated GPU starts: Linux's pages, a discrete GPU's system-memory buffers and \
         its evictions from VRAM among them, come from the heap with no bound of their own: \
         Linux takes the heap as its RAM and TTM its own limit, half of it, as the heap's own \
         need was not measured beside every driver that starts"
    );
    let display = sizes(free(16), 8, 8, GpuMemory::Display).unwrap();
    assert!(
        GpuPlan::from(GpuMemory::Display)
            .describe(&display)
            .ends_with(&format!(
                "its Metal device budgets its allocations to {}M, what the {}M heap holds past \
                 its own 2424M (at least 384M)",
                page_bound(&display, GpuMemory::Display) >> 20,
                (display.heap + display.gpu) >> 20
            ))
    );
}

/// Where no share is planned, Linux's pages are held to the heap's spare
/// past its own needs, at least [`PAGES_LEAST`], where the plan holds them
/// to it ([`held_bound`]).
#[test]
fn a_discrete_gpus_pages_are_held_to_the_heaps_spare() {
    // The NVIDIA floor heap, at its window: the least.
    let floor = Sizes {
        heap: HEAP_FLOOR,
        gpu: 0,
        ..sizes(free(32), 8, 8, GpuMemory::Own).unwrap()
    };
    assert_eq!(page_bound(&floor, GpuMemory::Own), 392 * MIB);
    let small = Sizes {
        heap: 2720 * MIB,
        ..floor
    };
    assert_eq!(page_bound(&small, GpuMemory::Own), PAGES_LEAST);
    // Every host's plan: the bound leaves the heap at least its floor less
    // the least, and an integrated GPU's share is its own bound.
    for installed in [8, 12, 16, 32, 64, 128] {
        for memory in [GpuMemory::Own, GpuMemory::Display, GpuMemory::Shared] {
            let sizes = sizes(free(installed), 8, 8, memory).unwrap();
            let bound = page_bound(&sizes, memory);
            match memory {
                GpuMemory::Shared => assert_eq!(bound, sizes.gpu),
                _ => {
                    assert!(bound >= PAGES_LEAST, "{installed} {memory:?}");
                    assert!(
                        sizes.heap + sizes.gpu - bound >= HEAP_OWN.min(sizes.heap - PAGES_LEAST),
                        "{installed} {memory:?}"
                    );
                }
            }
        }
    }
}

/// A driver that maps a whole device-memory aperture (amdgpu's visible VRAM,
/// xe's LMEMBAR) makes the heap that much bigger for its 4 KiB page tables,
/// and the guest that much smaller; nothing else moves.
#[test]
fn an_apertures_page_tables_grow_the_heap() {
    assert_eq!(aperture_tables(256 * MIB), 2 * MIB);
    assert_eq!(aperture_tables(16 * GIB), 34 * MIB);
    assert_eq!(aperture_tables(24 * GIB), 50 * MIB);
    for installed in [8, 16, 32] {
        for memory in [GpuMemory::Own, GpuMemory::Display, GpuMemory::Shared] {
            let plain = sizes(free(installed), 8, 8, memory).unwrap();
            let plan = GpuPlan {
                memory,
                aperture: aperture_tables(16 * GIB),
                bounded: true,
                fallback: false,
            };
            let grown = sizes(free(installed) + plan.aperture, 8, 8, plan).unwrap();
            assert_eq!(
                Sizes {
                    aperture: 0,
                    ..grown
                },
                plain,
                "{installed} {memory:?}"
            );
            assert_eq!(page_bound(&grown, memory), page_bound(&plain, memory));
        }
    }
}

/// What an integrated GPU that draws leaves the guest, by host.
#[test]
fn an_integrated_gpus_share_leaves_the_guest() {
    let split = |free: u64| {
        let sizes = sizes(free, 8, 8, GpuMemory::Shared).unwrap();
        [sizes.heap, sizes.gpu, sizes.guest].map(|bytes| bytes / MIB)
    };
    let rows = [
        7354 * MIB,
        free(8),
        10854 * MIB,
        free(12),
        free(16),
        free(32),
    ]
    .map(split);
    // heap, share, guest (MiB): the 8 GiB VM and host keep the guest's
    // floor and give the share what is above it; the 12 GiB VM's measured
    // free RAM, a 12 GiB host, 16 and 32 GiB hosts.
    assert_eq!(
        rows,
        [
            [2704, 780, 3072],
            [2704, 1362, 3072],
            [2816, 2560, 4400],
            [2880, 2560, 5220],
            [4400, 2644, 7272],
            [5232, 5290, 18700],
        ]
    );
}

/// The PCs whose integrated GPU may draw in the discrete GPU's place: an
/// NVIDIA card beside an AMD or an Intel integrated GPU (nouveau's, amdgpu's
/// and xe's heap need beside them unmeasured: Linux's pages unbounded), and
/// an AMD card beside an AMD integrated GPU (bounded), each with the monitor
/// on the card (the integrated GPU left: no share) and with the panel on the
/// integrated GPU (it shows the display).
fn fallback_pcs() -> [(&'static str, GpuPlan); 6] {
    let plan = |memory, bounded| GpuPlan {
        memory,
        aperture: 0,
        bounded,
        fallback: true,
    };
    [
        (
            "NVIDIA card + AMD iGPU, monitor on the card",
            plan(GpuMemory::Own, false),
        ),
        (
            "NVIDIA card + AMD iGPU, panel on the iGPU",
            plan(GpuMemory::Display, false),
        ),
        (
            "NVIDIA card + Intel iGPU, monitor on the card",
            plan(GpuMemory::Own, false),
        ),
        (
            "NVIDIA card + Intel iGPU, panel on the iGPU",
            plan(GpuMemory::Display, false),
        ),
        (
            "AMD card + AMD iGPU, monitor on the card",
            plan(GpuMemory::Own, true),
        ),
        (
            "AMD card + AMD iGPU, panel on the iGPU",
            plan(GpuMemory::Display, true),
        ),
    ]
}

/// Free RAM of the 8 GiB and 12 GiB VMs, whose heaps sit on their floor,
/// and of 8, 16 and 32 GiB PCs.
fn fallback_hosts() -> [(&'static str, u64); 5] {
    [
        ("8 GiB VM", 7354 * MIB),
        ("8 GiB", free(8)),
        ("12 GiB VM", 10842 * MIB),
        ("16 GiB", free(16)),
        ("32 GiB", free(32)),
    ]
}

/// The pages Linux holds once the discrete GPU failed: after a failed
/// probe, whose pages its devres and the quiesce freed, and after a Metal
/// device that did not open, its driver bound with its firmware's image.
const HELD_AFTER_PROBE: u64 = 64 * MIB;
const HELD_AFTER_METAL: u64 = 160 * MIB;
const OPEN_HELD: u64 = FALLBACK_OPEN;

/// Where an integrated GPU may draw in a discrete GPU's place, the plan
/// makes the heap region hold what it needs past the heap's own, from the
/// guest, as far as its floor allows; once it draws, Linux's pages are held
/// to that bound, so the heap keeps its own need and the GPU's Metal device
/// still has room after its driver's first pages, on every PC and host.
#[test]
fn an_integrated_gpu_that_draws_in_a_discrete_ones_place_keeps_the_heap_its_own() {
    for (pc, plan) in fallback_pcs() {
        for (host, free) in fallback_hosts() {
            let sizes = sizes(free, 8, 8, plan).unwrap();
            let before = sizes_of_plan(
                free,
                GpuPlan {
                    fallback: false,
                    ..plan
                },
            );
            assert!(sizes.guest >= GUEST_FLOOR, "{pc}, {host}");
            // The heap's region only grows, the guest keeping its floor.
            assert!(
                sizes.heap + sizes.fallback >= before.heap && sizes.gpu == before.gpu,
                "{pc}, {host}"
            );
            let bound = page_bound(&sizes, plan.memory);
            let region = sizes.heap + sizes.gpu + sizes.fallback;
            match sizes.guest > GUEST_FLOOR + ALIGNMENT {
                // The guest above its floor gives the whole reserve.
                true => assert!(bound >= FALLBACK_LEAST, "{pc}, {host}: {bound}"),
                false => assert!(bound >= PAGES_LEAST, "{pc}, {host}: {bound}"),
            }
            // The heap keeps its own need past the bound wherever the region
            // holds more than the least bound.
            if region >= HEAP_OWN + PAGES_LEAST {
                assert!(region - bound >= HEAP_OWN, "{pc}, {host}");
            }
            // Linux at the fallback: held to its plan's bound already, or
            // held to it from now on.
            let limit = held_bound(&sizes, plan).unwrap_or(0);
            for held in [HELD_AFTER_PROBE, HELD_AFTER_METAL] {
                let decided = fallback_pages(bound, held, limit);
                assert_eq!(decided.limit(), Some(bound), "{pc}, {host}: {decided:?}");
                match plan.bounded {
                    true => assert!(matches!(decided, FallbackPages::Held { .. })),
                    false => assert!(matches!(decided, FallbackPages::Hold { .. })),
                }
                // The GPU's Metal device tells the guest a working set past
                // its driver's first pages, where the reserve was given.
                let state = stela_drivers::gpu::share::ShareState {
                    held: held + OPEN_HELD,
                    limit: bound,
                };
                if bound >= FALLBACK_LEAST && held == HELD_AFTER_PROBE {
                    assert!(
                        stela_drivers::gpu::share::working_set(state) >= 128 * MIB,
                        "{pc}, {host}"
                    );
                    assert!(!decided.starved(), "{pc}, {host}");
                }
            }
        }
    }
}

fn sizes_of_plan(free: u64, plan: GpuPlan) -> Sizes {
    sizes(free, 8, 8, plan).unwrap()
}

/// What the room costs the guest, by host: nothing on 16 and 32 GiB PCs,
/// whose heaps hold it already; on the small hosts the heap's spare past its
/// own need is brought to 768M, as far as the guest's floor allows. A plan
/// without such a GPU (a release, NVK alone) is unchanged.
#[test]
fn the_room_for_an_integrated_gpu_costs_only_small_hosts_guest_ram() {
    let cost = |memory, free: u64| {
        let plan = GpuPlan {
            memory,
            aperture: 0,
            bounded: false,
            fallback: true,
        };
        let with = sizes(free, 8, 8, plan).unwrap();
        let without = sizes(
            free,
            8,
            8,
            GpuPlan {
                fallback: false,
                ..plan
            },
        )
        .unwrap();
        assert_eq!(without.fallback, 0);
        [
            with.fallback,
            without.guest - with.guest,
            page_bound(&with, memory),
        ]
        .map(|b| b / MIB)
    };
    // Room, the guest's cost and the bound (MiB), the monitor on the card
    // and the panel on the integrated GPU: the 8 GiB VM, an 8 GiB PC, the
    // 12 GiB VM, 16 and 32 GiB PCs.
    let rows = fallback_hosts()
        .map(|(_, free)| [cost(GpuMemory::Own, free), cost(GpuMemory::Display, free)]);
    assert_eq!(
        rows,
        [
            [[472, 364, 768], [216, 164, 768]],
            [[424, 320, 768], [168, 120, 768]],
            [[0, 0, 876], [0, 0, 892]],
            [[0, 0, 2152], [0, 0, 2392]],
            [[0, 0, 3656], [0, 0, 3864]],
        ]
    );
}

/// A plan without an integrated GPU that may draw instead, as a release
/// with NVK alone makes on any PC, is the plan before the room: no reserve,
/// the same parts, the same unbounded pages and the same line.
#[test]
fn a_plan_without_such_a_gpu_is_unchanged() {
    for (_, free) in fallback_hosts() {
        let release = sizes(free, 8, 8, GpuPlan::from(GpuMemory::Own)).unwrap();
        assert_eq!(release.fallback, 0);
        assert_eq!(release, sizes(free, 8, 8, GpuMemory::Own).unwrap());
        assert_eq!(held_bound(&release, GpuPlan::from(GpuMemory::Own)), None);
        assert_eq!(
            pack_budget(free, 8, 8, GpuPlan::from(GpuMemory::Own)),
            pack_budget(free, 8, 8, GpuMemory::Own)
        );
    }
    // The NVIDIA release's numbers on the 12 GiB VM and a 16 GiB PC.
    let vm = sizes(10842 * MIB, 8, 8, GpuMemory::Own).unwrap();
    let pc = sizes(free(16), 8, 8, GpuMemory::Own).unwrap();
    assert_eq!(
        [
            vm.heap,
            vm.guest,
            pc.heap,
            pc.guest,
            page_bound(&vm, GpuMemory::Own)
        ]
        .map(|b| b / MIB),
        [3300, 6144, 4576, 9538, 876]
    );
}

/// At the floor heap, where the guest's floor leaves no room: the bound is
/// the least, an integrated GPU that falls back after a failed probe draws
/// in it with a small budget, said on the screen; one that would find less
/// than its driver needs to start is refused, so the heap never stops.
#[test]
fn at_the_floor_heap_a_fallback_draws_small_or_is_refused() {
    let floor = Sizes {
        heap: HEAP_FLOOR,
        gpu: 0,
        fallback: 0,
        ..sizes(free(32), 8, 8, GpuMemory::Own).unwrap()
    };
    let bound = page_bound(&floor, GpuMemory::Own);
    assert_eq!(bound, 392 * MIB);
    let probe = fallback_pages(bound, HELD_AFTER_PROBE, 0);
    assert_eq!(
        probe,
        FallbackPages::Hold {
            limit: bound,
            room: 328 * MIB
        }
    );
    assert!(probe.starved());
    assert_eq!(
        fallback_pages(bound, HELD_AFTER_METAL, 0),
        FallbackPages::Short {
            held: HELD_AFTER_METAL,
            bound
        }
    );
    // A plan that held Linux already: the same rule over its limit.
    assert_eq!(
        fallback_pages(bound, HELD_AFTER_PROBE, bound),
        FallbackPages::Held {
            limit: bound,
            room: 328 * MIB
        }
    );
    // With the room the plan keeps where the guest has it.
    let roomy = Sizes {
        fallback: 376 * MIB,
        ..floor
    };
    assert_eq!(page_bound(&roomy, GpuMemory::Own), FALLBACK_LEAST);
    assert!(!fallback_pages(FALLBACK_LEAST, HELD_AFTER_PROBE, 0).starved());
    // An integrated GPU started already draws whatever is left; Linux is
    // held where it holds no more than the bound.
    assert_eq!(
        drawing_pages(bound, HELD_AFTER_METAL, 0),
        FallbackPages::Hold {
            limit: bound,
            room: 232 * MIB
        }
    );
    assert_eq!(
        drawing_pages(bound, 400 * MIB, 0),
        FallbackPages::Over {
            held: 400 * MIB,
            bound
        }
    );
    assert_eq!(
        drawing_pages(bound, 400 * MIB, 512 * MIB).limit(),
        Some(512 * MIB)
    );
    // The lines say each.
    assert!(
        probe.describe().ends_with(
            "328M of them free, little past the 480M its driver holds as its Metal device \
                 opens, so some windows may not draw"
        ),
        "{}",
        probe.describe()
    );
    assert!(
        fallback_pages(bound, HELD_AFTER_METAL, 0)
            .describe()
            .starts_with(
                "not started in place of a GPU that could not draw: Linux holds 160M of the 392M"
            )
    );
}

/// The plan's line names the bound an integrated GPU drawing in a discrete
/// GPU's place gets, and what the heap took for it.
#[test]
fn the_plans_line_names_the_room_for_an_integrated_gpu() {
    let plan = GpuPlan {
        memory: GpuMemory::Own,
        aperture: 0,
        bounded: false,
        fallback: true,
    };
    let floor = Sizes {
        heap: HEAP_FLOOR,
        gpu: 0,
        fallback: 376 * MIB,
        aperture: 0,
        ..sizes(free(32), 8, 8, GpuMemory::Own).unwrap()
    };
    assert!(
        plan.describe(&floor).ends_with(
            "; an integrated GPU this image draws with may draw in the discrete GPU's place, \
             Linux's pages then held to 768M, for which the heap takes 376M more, to hold the \
             768M it needs"
        ),
        "{}",
        plan.describe(&floor)
    );
    let short = Sizes {
        fallback: 0,
        ..floor
    };
    assert!(plan.describe(&short).ends_with(
        "Linux's pages then held to 392M, under the 768M it needs, as the heap takes only 0M \
         more and the guest keeps its floor"
    ));
    let roomy = Sizes {
        heap: 4400 * MIB,
        fallback: 0,
        ..floor
    };
    assert!(
        plan.describe(&roomy)
            .ends_with("Linux's pages then held to 1976M")
    );
}
