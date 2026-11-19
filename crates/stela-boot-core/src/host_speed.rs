//! What a host core can do apart from its clock, for one boot line per core
//! class (`HOST SPEED`), the memory the firmware describes (`HOST MEMORY
//! MODULES`), and the clock with every PE busy against the single-core
//! clock measured at boot (`HOST CLOCK ALL BUSY`). Together with `PES CLOCK`
//! they split a slow PC's guest speed into clock, cache and memory.
//!
//! - Caches: Intel CPUID.04H and AMD CPUID 8000_001DH (with topology
//!   extensions, 8000_0001H:ECX[22]): per level the size (ways x partitions
//!   x line x sets) and the most logical processors that share it
//!   (EAX[25:14] + 1), as Linux's `cpuid4_cache_lookup_regs` decodes them.
//! - Calibrations, once per core class before the PEs start, on a ready
//!   core of that class, each within [`BOUND_MS`]: a fixed loop of 14 instructions in four independent
//!   chains (instructions per second, and per cycle at the core's measured
//!   clock); a pointer chase over half the L3 (its load latency); a pointer
//!   chase over a buffer eight times the L3, 32-128 MiB (memory latency);
//!   and independent random reads over the same buffer, as the block table
//!   (128 MiB on a 768 MiB code window) and the ITLBs are probed (memory
//!   throughput per load). About 20-40 ms per class, most of it writing the
//!   buffer; nothing runs after boot.
//! - Memory modules: SMBIOS type 17 (DSP0134 7.18): populated devices, size
//!   (0CH, or the extended size at 1CH), type (12H), data width (0AH), speed
//!   (15H, extended at 54H) and configured speed (20H, extended at 58H), and
//!   the channel each sits on from the device and bank locator strings
//!   ("ChannelA", "CHANNEL B", "DIMM_A1"), which only gives a channel count
//!   when every populated module names one. Serial numbers, asset tags and
//!   part numbers are never read.
use alloc::{format, string::String, vec::Vec};

/// One cache level as CPUID describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cache {
    pub level: u8,
    pub bytes: u64,
    /// The most logical processors that share it.
    pub shared_by: u32,
}

/// A data or unified cache from one CPUID.04H / 8000_001DH subleaf; none for
/// the end of the list (type 0) or an instruction cache (type 2).
pub fn cache(eax: u32, ebx: u32, ecx: u32) -> Option<Cache> {
    if !matches!(eax & 0x1f, 1 | 3) {
        return None;
    }
    let ways = u64::from(ebx >> 22) + 1;
    let partitions = u64::from((ebx >> 12) & 0x3ff) + 1;
    let line = u64::from(ebx & 0xfff) + 1;
    let sets = u64::from(ecx) + 1;
    Some(Cache {
        level: ((eax >> 5) & 7) as u8,
        bytes: ways * partitions * line * sets,
        shared_by: ((eax >> 14) & 0xfff) + 1,
    })
}

/// The L2 and L3 among the subleaves `read` returns (EAX, EBX, ECX), up to
/// the first of type 0.
pub fn levels(read: impl Fn(u32) -> (u32, u32, u32)) -> (Option<Cache>, Option<Cache>) {
    let (mut l2, mut l3) = (None, None);
    for subleaf in 0..16 {
        let (eax, ebx, ecx) = read(subleaf);
        if eax & 0x1f == 0 {
            break;
        }
        match cache(eax, ebx, ecx) {
            Some(found) if found.level == 2 => l2 = Some(found),
            Some(found) if found.level == 3 => l3 = Some(found),
            _ => {}
        }
    }
    (l2, l3)
}

/// This processor's L2 and L3.
#[cfg(target_arch = "x86_64")]
pub fn host_levels() -> (Option<Cache>, Option<Cache>) {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    let identity = __cpuid(0);
    let vendor = [identity.ebx, identity.edx, identity.ecx];
    let leaf = if vendor == crate::msr::AMD_VENDOR {
        let extended = __cpuid(0x8000_0000).eax;
        let topology = extended >= 0x8000_0001 && __cpuid(0x8000_0001).ecx & (1 << 22) != 0;
        if !topology || extended < 0x8000_001d {
            return (None, None);
        }
        0x8000_001d
    } else if identity.eax >= 4 {
        4
    } else {
        return (None, None);
    };
    levels(|subleaf| {
        let result = __cpuid_count(leaf, subleaf);
        (result.eax, result.ebx, result.ecx)
    })
}

/// One populated memory device (SMBIOS type 17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Module {
    /// MiB; 0 when the firmware says unknown.
    pub mib: u64,
    /// MT/s; 0 unknown.
    pub speed: u32,
    pub configured: u32,
    pub kind: u8,
    pub data_bits: u16,
    /// Controller and channel the locators name.
    pub channel: Option<(u8, u8)>,
}

/// The memory devices the firmware describes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// Type 17 structures, populated or not.
    pub devices: u32,
    pub modules: Vec<Module>,
}

fn word(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn dword(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// String `index` (1-based) of a structure's string set.
fn string(strings: &[u8], index: u8) -> Option<&[u8]> {
    strings
        .split(|&byte| byte == 0)
        .nth(usize::from(index).checked_sub(1)?)
        .filter(|value| !value.is_empty())
}

/// The controller and channel a locator names: "Controller0-ChannelA",
/// "P0 CHANNEL A", "ChannelB-DIMM0", "DIMM_A1", "DIMM B2".
pub fn channel_of(text: &[u8]) -> Option<(u8, u8)> {
    let upper: Vec<u8> = text.iter().map(u8::to_ascii_uppercase).collect();
    let after = |word: &[u8]| -> Option<u8> {
        let at = upper
            .windows(word.len())
            .position(|window| window == word)?
            + word.len();
        upper[at..]
            .iter()
            .copied()
            .find(|byte| !matches!(byte, b' ' | b'-' | b'_' | b':' | b'#'))
            .filter(u8::is_ascii_alphanumeric)
    };
    let controller = after(b"CONTROLLER").unwrap_or(b'0');
    if let Some(channel) = after(b"CHANNEL") {
        return Some((controller, channel));
    }
    // "DIMM_A1": a letter then a digit after DIMM.
    let at = upper.windows(4).position(|window| window == b"DIMM")? + 4;
    let rest: Vec<u8> = upper[at..]
        .iter()
        .copied()
        .skip_while(|byte| matches!(byte, b' ' | b'-' | b'_'))
        .collect();
    match rest.as_slice() {
        [letter, digit, ..] if letter.is_ascii_uppercase() && digit.is_ascii_digit() => {
            Some((controller, *letter))
        }
        _ => None,
    }
}

/// A type 17 structure (`structure`, its formatted area) with its strings:
/// none when no module is installed.
pub fn module(structure: &[u8], strings: &[u8]) -> Option<Module> {
    let length = structure.len();
    let size = word(structure, 0x0c)?;
    if size == 0 {
        return None;
    }
    let mib = match size {
        0xffff => 0,
        0x7fff => dword(structure, 0x1c).map_or(0, |extended| u64::from(extended & 0x7fff_ffff)),
        size if size & 0x8000 != 0 => u64::from(size & 0x7fff) / 1024,
        size => u64::from(size),
    };
    let speed_at = |at: usize, extended: usize| -> u32 {
        match word(structure, at).filter(|_| length >= at + 2) {
            Some(0xffff) => dword(structure, extended).unwrap_or(0),
            Some(speed) => u32::from(speed),
            None => 0,
        }
    };
    let locator = |at: usize| {
        structure
            .get(at)
            .and_then(|&index| string(strings, index))
            .and_then(channel_of)
    };
    Some(Module {
        mib,
        speed: speed_at(0x15, 0x54),
        configured: speed_at(0x20, 0x58),
        kind: structure.get(0x12).copied().unwrap_or(0),
        data_bits: word(structure, 0x0a)
            .filter(|&bits| bits != 0xffff)
            .unwrap_or(0),
        // The bank locator names the channel on most boards, the device
        // locator on others.
        channel: locator(0x11).or_else(|| locator(0x10)),
    })
}

/// Every type 17 structure of an SMBIOS table.
pub fn memory(table: &[u8]) -> Memory {
    let mut found = Memory::default();
    let mut at = 0usize;
    while let Some(head) = table.get(at..at + 4) {
        let length = usize::from(head[1]);
        let Some(rest) = table.get(at + length..) else {
            break;
        };
        let Some(end) = rest.windows(2).position(|pair| pair == [0, 0]) else {
            break;
        };
        if head[0] == 17 && length >= 0x15 {
            found.devices += 1;
            if let Some(module) = module(&table[at..at + length], &rest[..end + 1]) {
                found.modules.push(module);
            }
        }
        if head[0] == 127 {
            break;
        }
        at += length + end + 2;
    }
    found
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        0x18 => "DDR3",
        0x1a => "DDR4",
        0x1b => "LPDDR",
        0x1c => "LPDDR2",
        0x1d => "LPDDR3",
        0x1e => "LPDDR4",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        _ => "other",
    }
}

impl Memory {
    /// Channels: the distinct ones the locators name when every populated
    /// module names one, one for a single module, else unknown.
    pub fn channels(&self) -> Option<usize> {
        let named: Option<Vec<(u8, u8)>> =
            self.modules.iter().map(|module| module.channel).collect();
        match named {
            Some(mut named) if !named.is_empty() => {
                named.sort_unstable();
                named.dedup();
                Some(named.len())
            }
            _ if self.modules.len() == 1 => Some(1),
            _ => None,
        }
    }

    /// `HOST MEMORY MODULES devices=4 populated=2 type=DDR5 sizes_mib=8192+8192
    /// speed=5600 configured=5600 data_bits=128 channels=2(dual)`.
    pub fn line(&self) -> String {
        let mut kinds: Vec<&str> = self
            .modules
            .iter()
            .map(|module| kind_name(module.kind))
            .collect();
        kinds.dedup();
        let join = |values: Vec<String>| {
            if values.is_empty() {
                String::from("-")
            } else {
                values.join("+")
            }
        };
        // Speeds the firmware gives (0 is unknown).
        let range = |values: Vec<u32>| {
            let known = values.iter().copied().filter(|&value| value != 0);
            match (known.clone().min(), known.max()) {
                (Some(low), Some(high)) if low == high => format!("{low}"),
                (Some(low), Some(high)) => format!("{low}-{high}"),
                _ => String::from("-"),
            }
        };
        let channels = match self.channels() {
            Some(1) => String::from("1(single)"),
            Some(2) => String::from("2(dual)"),
            Some(count) => format!("{count}"),
            None => String::from("unknown"),
        };
        format!(
            "HOST MEMORY MODULES devices={} populated={} type={} sizes_mib={} speed={} \
             configured={} data_bits={} channels={channels}\n",
            self.devices,
            self.modules.len(),
            if kinds.is_empty() {
                String::from("-")
            } else {
                kinds.join("/")
            },
            join(
                self.modules
                    .iter()
                    .map(|module| format!("{}", module.mib))
                    .collect()
            ),
            range(self.modules.iter().map(|module| module.speed).collect()),
            range(
                self.modules
                    .iter()
                    .map(|module| module.configured)
                    .collect()
            ),
            self.modules
                .iter()
                .map(|module| u32::from(module.data_bits))
                .sum::<u32>(),
        )
    }
}

/// One core class's calibration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Speed {
    pub l2: Option<Cache>,
    pub l3: Option<Cache>,
    /// The core's single-core clock (`guest_timeouts::measure_core_hz`).
    pub core_hz: u64,
    /// Instructions per second of the fixed loop.
    pub instructions_per_s: u64,
    /// Picoseconds per load: chase within half the L3, chase over the large
    /// buffer, independent random reads over it.
    pub l3_ps: u64,
    pub memory_ps: u64,
    pub random_ps: u64,
    pub buffer_bytes: u64,
    /// The time bound ended the calibration before its last figure, which
    /// it and the figures after it leave out.
    pub cut: bool,
}

fn size(bytes: u64) -> String {
    if bytes >= 1 << 20 && bytes.is_multiple_of(1 << 20) {
        format!("{}M", bytes >> 20)
    } else {
        format!("{}K", bytes >> 10)
    }
}

fn nanoseconds(picoseconds: u64) -> String {
    if picoseconds == 0 {
        String::from("-")
    } else {
        format!("{}.{}", picoseconds / 1000, picoseconds % 1000 / 100)
    }
}

impl Speed {
    /// `HOST SPEED class=performance l2=1024K/2 l3=16M/16 core_mhz=4690
    /// compute_mips=17800 ipc=3.79 l3_ns=11.2 memory_ns=104.3 random_ns=9.8
    /// buffer=128M`: the L2 and L3 sizes with the most logical processors
    /// sharing each, the core's single-core clock, the fixed loop's rate and
    /// instructions per cycle, and the three latencies.
    pub fn line(&self, class: &str) -> String {
        let cache = |cache: Option<Cache>| {
            cache.map_or(String::from("-"), |cache| {
                format!("{}/{}", size(cache.bytes), cache.shared_by)
            })
        };
        let ipc = (self.core_hz != 0)
            .then(|| u128::from(self.instructions_per_s) * 100 / u128::from(self.core_hz));
        format!(
            "HOST SPEED class={class} l2={} l3={} core_mhz={} compute_mips={} ipc={} l3_ns={} \
             memory_ns={} random_ns={} buffer={}{}\n",
            cache(self.l2),
            cache(self.l3),
            self.core_hz / 1_000_000,
            self.instructions_per_s / 1_000_000,
            ipc.map_or(String::from("-"), |ipc| format!(
                "{}.{:02}",
                ipc / 100,
                ipc % 100
            )),
            nanoseconds(self.l3_ps),
            nanoseconds(self.memory_ps),
            nanoseconds(self.random_ps),
            if self.buffer_bytes == 0 {
                String::from("-")
            } else {
                size(self.buffer_bytes)
            },
            if self.cut {
                format!(" cut_at_ms={BOUND_MS}")
            } else {
                String::new()
            },
        )
    }
}

/// The large buffer for an L3 of `l3` bytes: eight times it, a power of two
/// within 32-128 MiB.
pub fn buffer_bytes(l3: Option<u64>) -> u64 {
    (l3.unwrap_or(16 << 20) * 8)
        .next_power_of_two()
        .clamp(32 << 20, 128 << 20)
}

/// Words per cache line of a chase buffer.
const LINE_WORDS: usize = 8;

/// Make `buffer` one cycle through all its cache lines in a scattered
/// order: line `i`'s first word holds the word index of line
/// `(a * i + c) mod lines`, a full-period congruential step for a power of
/// two lines (a = 1 mod 4, c odd), written in address order so that setting
/// it up streams.
pub fn chain(buffer: &mut [u64]) {
    let lines = buffer.len() / LINE_WORDS;
    chain_lines(buffer, 0..lines);
}

/// [`chain`]'s writes for the lines in `part`.
fn chain_lines(buffer: &mut [u64], part: core::ops::Range<usize>) {
    let lines = buffer.len() / LINE_WORDS;
    debug_assert!(lines.is_power_of_two());
    let mask = lines as u64 - 1;
    for line in part {
        let next = (6_364_136_223_846_793_005u64)
            .wrapping_mul(line as u64)
            .wrapping_add(1_442_695_040_888_963_407)
            & mask;
        buffer[line * LINE_WORDS] = next * LINE_WORDS as u64;
    }
}

/// Follow the chain from word 0 for `loads` loads; the word reached.
pub fn chase(buffer: &[u64], loads: u64) -> u64 {
    let mut at = 0u64;
    for _ in 0..loads {
        at = buffer[at as usize];
    }
    core::hint::black_box(at)
}

/// `loads` reads at scattered lines in eight independent streams; their sum.
pub fn scatter(buffer: &[u64], loads: u64) -> u64 {
    let mask = (buffer.len() / LINE_WORDS - 1) as u64;
    let mut state = [0x9e37_79b9_7f4a_7c15u64; 8];
    for (index, value) in state.iter_mut().enumerate() {
        *value ^= (index as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    }
    let mut sum = 0u64;
    for _ in 0..loads / 8 {
        for value in &mut state {
            *value ^= *value << 13;
            *value ^= *value >> 7;
            *value ^= *value << 17;
            sum = sum.wrapping_add(buffer[((*value & mask) as usize) * LINE_WORDS]);
        }
    }
    core::hint::black_box(sum)
}

/// Picoseconds per load of `ticks` TSC ticks at `tsc_hz` over `loads`.
pub fn per_load(ticks: u64, tsc_hz: u64, loads: u64) -> u64 {
    if tsc_hz == 0 || loads == 0 {
        return 0;
    }
    u64::try_from(u128::from(ticks) * 1_000_000_000_000 / u128::from(tsc_hz) / u128::from(loads))
        .unwrap_or(u64::MAX)
}

/// Instructions of one pass of [`compute`]'s loop.
pub const LOOP_INSTRUCTIONS: u64 = 14;

/// `iterations` passes of a fixed loop: four independent chains of add,
/// xor and rotate, then the count and branch.
#[cfg(target_arch = "x86_64")]
pub fn compute(iterations: u64) {
    let (mut a, mut b, mut c, mut d) = (1u64, 2u64, 3u64, 4u64);
    // SAFETY: register arithmetic only.
    unsafe {
        core::arch::asm!(
            "2:",
            "add {a}, 1",
            "add {b}, 3",
            "add {c}, 5",
            "add {d}, 7",
            "xor {a}, 0x55",
            "xor {b}, 0x33",
            "xor {c}, 0x0f",
            "xor {d}, 0x71",
            "rol {a}, 1",
            "rol {b}, 3",
            "rol {c}, 5",
            "rol {d}, 7",
            "dec {n}",
            "jnz 2b",
            a = inout(reg) a,
            b = inout(reg) b,
            c = inout(reg) c,
            d = inout(reg) d,
            n = inout(reg) iterations => _,
            options(nomem, nostack),
        );
    }
    core::hint::black_box((a, b, c, d));
}

/// How long a calibration runs at most. Each of its steps, and each 1 MiB
/// part of setting up its large buffer, begins only within it; the figures
/// it did not reach are left out (`Speed::cut`). A step takes a few
/// milliseconds at most, so the calibration ends within about this.
pub const BOUND_MS: u64 = 400;
/// The words of one part of the large buffer's setup.
#[cfg(target_arch = "x86_64")]
const PART_WORDS: usize = (1 << 20) / 8;

/// Calibrate the calling core within [`BOUND_MS`], its TSC at `tsc_hz` and
/// `now` a TSC read. `buffer` holds the large buffer's `words` (at least 32
/// MiB of words, a power of two of lines, or none to skip the memory
/// figures), reserved and not yet written. The fastest of a few runs counts
/// for the loop and the in-cache chase; memory figures take one run.
#[cfg(target_arch = "x86_64")]
pub fn calibrate(buffer: &mut Vec<u64>, words: usize, tsc_hz: u64, now: impl Fn() -> u64) -> Speed {
    let started = now();
    let bound = tsc_hz / 1000 * BOUND_MS;
    let within = || now().wrapping_sub(started) < bound;
    let (l2, l3) = host_levels();
    let mut speed = Speed {
        l2,
        l3,
        cut: true,
        ..Speed::default()
    };
    if !within() {
        return speed;
    }
    speed.core_hz = crate::guest_timeouts::measure_core_hz(tsc_hz);
    if !within() {
        return speed;
    }
    const ITERATIONS: u64 = 200_000;
    let loop_ticks = (0..4)
        .map(|_| {
            let start = now();
            compute(ITERATIONS);
            now().wrapping_sub(start)
        })
        .min()
        .unwrap_or(0);
    speed.instructions_per_s = if loop_ticks == 0 {
        0
    } else {
        u64::try_from(
            u128::from(ITERATIONS * LOOP_INSTRUCTIONS) * u128::from(tsc_hz)
                / u128::from(loop_ticks),
        )
        .unwrap_or(u64::MAX)
    };
    if words < (32 << 20) / 8 || !(words / LINE_WORDS).is_power_of_two() {
        speed.cut = false;
        return speed;
    }
    // Written in parts, each begun within the bound.
    buffer.clear();
    while buffer.len() < words {
        if !within() {
            return speed;
        }
        let part = (words - buffer.len()).min(PART_WORDS);
        buffer.resize(buffer.len() + part, 0);
    }
    speed.buffer_bytes = (words * 8) as u64;
    let chained = |buffer: &mut [u64]| {
        let lines = buffer.len() / LINE_WORDS;
        (0..lines).step_by(PART_WORDS / LINE_WORDS).all(|first| {
            within() && {
                chain_lines(buffer, first..(first + PART_WORDS / LINE_WORDS).min(lines));
                true
            }
        })
    };
    // Half the L3 at the buffer's start: chained, walked once to load it,
    // then timed.
    let l3_words = (l3.map_or(8 << 20, |cache| cache.bytes / 2) / 8)
        .next_power_of_two()
        .min(words as u64 / 2) as usize;
    {
        let near = &mut buffer[..l3_words];
        if !chained(near) {
            return speed;
        }
        let lines = (l3_words / LINE_WORDS) as u64;
        chase(near, lines);
        let loads = lines.min(100_000);
        let ticks = (0..3)
            .map(|_| {
                let start = now();
                chase(near, loads);
                now().wrapping_sub(start)
            })
            .min()
            .unwrap_or(0);
        speed.l3_ps = per_load(ticks, tsc_hz, loads);
    }
    if !chained(buffer) {
        return speed;
    }
    const FAR_LOADS: u64 = 20_000;
    let start = now();
    chase(buffer, FAR_LOADS);
    speed.memory_ps = per_load(now().wrapping_sub(start), tsc_hz, FAR_LOADS);
    if !within() {
        return speed;
    }
    const SCATTER_LOADS: u64 = 80_000;
    let start = now();
    scatter(buffer, SCATTER_LOADS);
    speed.random_ps = per_load(now().wrapping_sub(start), tsc_hz, SCATTER_LOADS);
    speed.cut = false;
    speed
}

/// The all-busy clock of one beat: the clock weighted by running time over
/// PEs that each ran at least `min_running` percent of `interval` TSC
/// ticks, from their (actual, reference) counter deltas; none when a PE
/// has no counters or ran less.
pub fn all_busy_mhz(
    tsc_hz: u64,
    interval: u64,
    deltas: &[Option<(u64, u64)>],
    min_running: u64,
) -> Option<(u64, u64)> {
    let (mut actual, mut reference, mut least) = (0u128, 0u128, 100u64);
    if deltas.is_empty() || interval == 0 {
        return None;
    }
    for delta in deltas {
        let (pe_actual, pe_reference) = (*delta)?;
        let running = (u128::from(pe_reference) * 100 / u128::from(interval)).min(100) as u64;
        if running < min_running {
            return None;
        }
        least = least.min(running);
        actual += u128::from(pe_actual);
        reference += u128::from(pe_reference);
    }
    (reference != 0).then(|| {
        (
            (u128::from(tsc_hz) * actual / reference / 1_000_000) as u64,
            least,
        )
    })
}

/// `HOST CLOCK ALL BUSY all_mhz=3010 single_core_mhz=4691 ratio%=64
/// least_running%=86`, once, at the first beat in which every PE ran.
pub fn all_busy_line(all_mhz: u64, single_core_hz: u64, least: u64) -> String {
    let single = single_core_hz / 1_000_000;
    format!(
        "HOST CLOCK ALL BUSY all_mhz={all_mhz} single_core_mhz={single} ratio%={} \
         least_running%={least}\n",
        (all_mhz * 100)
            .checked_div(single)
            .map_or(String::from("-"), |ratio| format!("{ratio}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuid_cache_leaves_decode_size_and_sharing() {
        // A 1 MiB 16-way L2 shared by 2, a 16 MiB 16-way L3 shared by 16 (a
        // Zen 4 laptop), an L1 instruction cache that is left out.
        let l2 = (0b011 | 2 << 5 | 1 << 14, 15 << 22 | 63, 1023);
        let l3 = (0b011 | 3 << 5 | 15 << 14, 15 << 22 | 63, 16383);
        let l1i = (0b010 | 1 << 5, 7 << 22 | 63, 63);
        assert_eq!(
            cache(l2.0, l2.1, l2.2),
            Some(Cache {
                level: 2,
                bytes: 1 << 20,
                shared_by: 2
            })
        );
        assert_eq!(cache(l1i.0, l1i.1, l1i.2), None);
        let leaves = [l1i, l2, l3, (0, 0, 0), l2];
        let (two, three) = levels(|subleaf| leaves[subleaf as usize]);
        assert_eq!(two.map(|cache| cache.bytes), Some(1 << 20));
        assert_eq!(
            three,
            Some(Cache {
                level: 3,
                bytes: 16 << 20,
                shared_by: 16
            })
        );
    }

    /// A type 17 structure of `length` bytes with these fields, and its
    /// strings.
    fn device(size: u16, kind: u8, speed: u16, configured: u16, bank: &str) -> Vec<u8> {
        let mut bytes = alloc::vec![0u8; 0x28];
        bytes[0] = 17;
        bytes[1] = 0x28;
        bytes[0x0a..0x0c].copy_from_slice(&64u16.to_le_bytes());
        bytes[0x0c..0x0e].copy_from_slice(&size.to_le_bytes());
        bytes[0x10] = 1;
        bytes[0x11] = 2;
        bytes[0x12] = kind;
        bytes[0x15..0x17].copy_from_slice(&speed.to_le_bytes());
        bytes[0x20..0x22].copy_from_slice(&configured.to_le_bytes());
        bytes.extend_from_slice(b"DIMM 0\0");
        if !bank.is_empty() {
            bytes.extend_from_slice(bank.as_bytes());
            bytes.push(0);
        }
        bytes.push(0);
        bytes
    }

    fn table(devices: &[Vec<u8>]) -> Vec<u8> {
        let mut table: Vec<u8> = devices.concat();
        table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        table
    }

    #[test]
    fn populated_modules_and_their_channels_are_counted() {
        let memory = memory(&table(&[
            device(8192, 0x22, 5600, 5200, "P0 CHANNEL A"),
            device(0, 0, 0, 0, "P0 CHANNEL B"),
            device(8192, 0x22, 5600, 5200, "P0 CHANNEL B"),
        ]));
        assert_eq!((memory.devices, memory.modules.len()), (3, 2));
        assert_eq!(memory.channels(), Some(2));
        assert_eq!(
            memory.line(),
            "HOST MEMORY MODULES devices=3 populated=2 type=DDR5 sizes_mib=8192+8192 \
             speed=5600 configured=5200 data_bits=128 channels=2(dual)\n"
        );
        // One module: single channel whatever its locator says.
        let one = memory_of(&[device(16384, 0x1a, 3200, 3200, "BANK 0")]);
        assert_eq!(one.channels(), Some(1));
        assert!(
            one.line().ends_with(" channels=1(single)\n"),
            "{}",
            one.line()
        );
        // Two modules whose locators name no channel: unknown.
        let unnamed = memory_of(&[
            device(8192, 0x1a, 3200, 3200, "BANK 0"),
            device(8192, 0x1a, 3200, 3200, "BANK 1"),
        ]);
        assert_eq!(unnamed.channels(), None);
        assert!(unnamed.line().ends_with(" channels=unknown\n"));
        // Two modules on one channel.
        let shared = memory_of(&[
            device(8192, 0x1a, 3200, 3200, "ChannelA-DIMM0"),
            device(8192, 0x1a, 3200, 3200, "ChannelA-DIMM1"),
        ]);
        assert_eq!(shared.channels(), Some(1));
    }

    fn memory_of(devices: &[Vec<u8>]) -> Memory {
        memory(&table(devices))
    }

    #[test]
    fn sizes_and_speeds_take_their_extended_forms() {
        let mut big = device(0x7fff, 0x23, 0xffff, 0xffff, "Controller1-ChannelA");
        big[0x1c..0x20].copy_from_slice(&(64u32 * 1024).to_le_bytes());
        big.splice(0x28..0x28, [0u8; 0x34]);
        big[1] = 0x5c;
        big[0x54..0x58].copy_from_slice(&8533u32.to_le_bytes());
        big[0x58..0x5c].copy_from_slice(&7500u32.to_le_bytes());
        let found = memory_of(&[big]);
        assert_eq!(
            found.modules[0],
            Module {
                mib: 65536,
                speed: 8533,
                configured: 7500,
                kind: 0x23,
                data_bits: 64,
                channel: Some((b'1', b'A')),
            }
        );
        // Kilobyte units, and a size the firmware does not know.
        assert_eq!(
            memory_of(&[device(0x8000 | 2048, 0x1a, 0, 0, "")]).modules[0].mib,
            2
        );
        assert_eq!(
            memory_of(&[device(0xffff, 0x1a, 0, 0, "")]).modules[0].mib,
            0
        );
    }

    #[test]
    fn locators_name_controller_and_channel() {
        assert_eq!(
            channel_of(b"Controller0-ChannelA-DIMM0"),
            Some((b'0', b'A'))
        );
        assert_eq!(channel_of(b"P0 CHANNEL B"), Some((b'0', b'B')));
        assert_eq!(channel_of(b"DIMM_A1"), Some((b'0', b'A')));
        assert_eq!(channel_of(b"DIMM B2"), Some((b'0', b'B')));
        assert_eq!(channel_of(b"DIMM 0"), None);
        assert_eq!(channel_of(b"BANK 0"), None);
    }

    #[test]
    fn a_chain_visits_every_line_once_per_cycle() {
        let mut buffer = alloc::vec![0u64; 1024 * LINE_WORDS];
        chain(&mut buffer);
        let mut seen = alloc::vec![false; 1024];
        let mut at = 0u64;
        for _ in 0..1024 {
            at = buffer[at as usize];
            assert_eq!(at % LINE_WORDS as u64, 0);
            let line = (at / LINE_WORDS as u64) as usize;
            assert!(!seen[line], "line {line} twice");
            seen[line] = true;
        }
        assert!(seen.iter().all(|&seen| seen));
        assert_eq!(chase(&buffer, 1024), 0, "back at the start");
        // Scattered reads stay in the buffer.
        scatter(&buffer, 4096);
    }

    #[test]
    fn the_buffer_is_eight_l3s_within_bounds_and_loads_are_timed() {
        assert_eq!(buffer_bytes(Some(16 << 20)), 128 << 20);
        assert_eq!(buffer_bytes(Some(4 << 20)), 32 << 20);
        assert_eq!(buffer_bytes(Some(96 << 20)), 128 << 20);
        assert_eq!(buffer_bytes(None), 128 << 20);
        // 2,000,000 ticks at 1 GHz over 20,000 loads: 100 ns.
        assert_eq!(per_load(2_000_000, 1_000_000_000, 20_000), 100_000);
        assert_eq!(per_load(1, 0, 1), 0);
    }

    #[test]
    fn the_speed_line_names_every_figure() {
        let speed = Speed {
            l2: Some(Cache {
                level: 2,
                bytes: 1 << 20,
                shared_by: 2,
            }),
            l3: Some(Cache {
                level: 3,
                bytes: 16 << 20,
                shared_by: 16,
            }),
            core_hz: 4_690_000_000,
            instructions_per_s: 17_780_000_000,
            l3_ps: 11_250,
            memory_ps: 104_300,
            random_ps: 9_870,
            buffer_bytes: 128 << 20,
            cut: false,
        };
        assert_eq!(
            speed.line("all"),
            "HOST SPEED class=all l2=1M/2 l3=16M/16 core_mhz=4690 compute_mips=17780 ipc=3.79 \
             l3_ns=11.2 memory_ns=104.3 random_ns=9.8 buffer=128M\n"
        );
        assert_eq!(
            Speed::default().line("efficiency"),
            "HOST SPEED class=efficiency l2=- l3=- core_mhz=0 compute_mips=0 ipc=- l3_ns=- \
             memory_ns=- random_ns=- buffer=-\n"
        );
        assert!(
            Speed { cut: true, ..speed }
                .line("all")
                .ends_with(" buffer=128M cut_at_ms=400\n")
        );
    }

    #[test]
    fn the_all_busy_clock_needs_every_pe_running() {
        let interval = 10_000_000_000;
        // Two PEs at 3.0 and 3.4 GHz on a 2 GHz TSC, both busy.
        let busy = [
            Some((15_000_000_000, 10_000_000_000)),
            Some((15_300_000_000, 9_000_000_000)),
        ];
        assert_eq!(
            all_busy_mhz(2_000_000_000, interval, &busy, 80),
            Some((3189, 90))
        );
        // One PE ran too little, or has no counters.
        let idle = [busy[0], Some((1, 5_000_000_000))];
        assert_eq!(all_busy_mhz(2_000_000_000, interval, &idle, 80), None);
        assert_eq!(
            all_busy_mhz(2_000_000_000, interval, &[busy[0], None], 80),
            None
        );
        assert_eq!(
            all_busy_line(3010, 4_691_000_000, 86),
            "HOST CLOCK ALL BUSY all_mhz=3010 single_core_mhz=4691 ratio%=64 \
             least_running%=86\n"
        );
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod hosted {
    use super::*;

    fn tsc() -> u64 {
        // SAFETY: RDTSC has no memory effects.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    /// The figures on the machine running the tests, with its TSC rate in
    /// `STELA_TSC_HZ`: `cargo test -p stela-boot-core --lib hosted --
    /// --nocapture` prints the line.
    #[test]
    fn calibrate_this_host() {
        let tsc_hz = std::env::var("STELA_TSC_HZ")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4_700_000_000);
        let (_, l3) = host_levels();
        let words = buffer_bytes(l3.map(|cache| cache.bytes)) as usize / 8;
        let mut buffer = Vec::with_capacity(words);
        let speed = calibrate(&mut buffer, words, tsc_hz, tsc);
        std::print!("{}", speed.line("all"));
        assert!(speed.instructions_per_s > 0);
    }

    /// A clock at 1 GHz that moves `step` ticks at each read, and its reads.
    fn stepping(step: u64) -> (impl Fn() -> u64, std::rc::Rc<core::cell::Cell<u64>>) {
        let reads = std::rc::Rc::new(core::cell::Cell::new(0u64));
        let counted = reads.clone();
        (
            move || {
                counted.set(counted.get() + 1);
                counted.get() * step
            },
            reads,
        )
    }

    #[test]
    fn a_calibration_ends_at_its_time_bound() {
        let words = (32 << 20) / 8;
        // Each read 30 ms on: the bound passes while the buffer is set up,
        // which stops after a few parts, and every memory figure is left
        // out.
        let (now, reads) = stepping(30_000_000);
        let mut buffer = Vec::with_capacity(words);
        let speed = calibrate(&mut buffer, words, 1_000_000_000, now);
        assert!(speed.cut);
        assert!(buffer.len() < words, "{} words written", buffer.len());
        assert_eq!(
            (
                speed.buffer_bytes,
                speed.l3_ps,
                speed.memory_ps,
                speed.random_ps
            ),
            (0, 0, 0, 0)
        );
        assert!(reads.get() < 20, "{} clock reads", reads.get());
        // A bound already spent: nothing is measured.
        let (now, _) = stepping(1_000_000_000);
        let speed = calibrate(&mut Vec::new(), words, 1_000_000_000, now);
        assert_eq!(
            (speed.cut, speed.core_hz, speed.instructions_per_s),
            (true, 0, 0)
        );
        // A clock 1 us on at each read: every figure, uncut.
        let (now, _) = stepping(1000);
        let mut buffer = Vec::with_capacity(words);
        let speed = calibrate(&mut buffer, words, 1_000_000_000, now);
        assert!(!speed.cut);
        assert_eq!(speed.buffer_bytes, 32 << 20);
        assert!(speed.memory_ps > 0 && speed.random_ps > 0);
    }
}
