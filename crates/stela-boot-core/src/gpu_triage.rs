//! Lines a GPU's boot log carries for whoever reads it after a failure on a
//! PC nobody here can test: a one-time summary of what the GPU and its
//! driver are, and a bound on lines that repeat.
use core::fmt;

/// Counts a keyed line's repeats and says which to print: the first, then
/// each power of two, so a line that repeats every frame costs a log a few
/// lines that still show how often it came. Keys are kept as hashes of
/// their text in a table of fixed size, so a count allocates nothing: a
/// `WARN` may come while the heap is exhausted.
pub struct Repeats {
    /// Each tracked key's hash and count, the first `tracked` of them used.
    counts: [(u64, u64); KEYS],
    tracked: usize,
    /// The count of every key past the table.
    untracked: u64,
}

/// Keys a [`Repeats`] tracks; a key past them is printed at the powers of
/// two of the untracked count.
const KEYS: usize = 256;

/// How a counted line is printed: the suffix after its text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Seen {
    /// Its first time: no suffix.
    First,
    /// A power of two of its count.
    Times(u64),
    /// A power of two of the count of the keys past the table.
    PastTable(u64),
}

impl fmt::Display for Seen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::First => Ok(()),
            Self::Times(count) => write!(f, " (seen {count} times)"),
            Self::PastTable(count) => write!(f, " ({count} past the log's table)"),
        }
    }
}

impl Default for Repeats {
    fn default() -> Self {
        Self::new()
    }
}

impl Repeats {
    pub const fn new() -> Self {
        Self {
            counts: [(0, 0); KEYS],
            tracked: 0,
            untracked: 0,
        }
    }

    /// Count one more `key`: how to print it, `None` when it is not
    /// printed.
    pub fn count(&mut self, key: fmt::Arguments<'_>) -> Option<Seen> {
        let hash = crate::repeated_lines::hash(key);
        let slot = match self.counts[..self.tracked]
            .iter()
            .position(|&(known, _)| known == hash)
        {
            Some(slot) => Some(slot),
            None if self.tracked < KEYS => {
                self.counts[self.tracked] = (hash, 0);
                self.tracked += 1;
                Some(self.tracked - 1)
            }
            None => None,
        };
        let count = match slot {
            Some(slot) => &mut self.counts[slot].1,
            None => &mut self.untracked,
        };
        *count += 1;
        let count = *count;
        match (count.is_power_of_two(), slot.is_some()) {
            (false, _) => None,
            (true, true) if count == 1 => Some(Seen::First),
            (true, true) => Some(Seen::Times(count)),
            (true, false) => Some(Seen::PastTable(count)),
        }
    }
}

/// What a triager needs first of a GPU that opened, or failed to open, the
/// Metal service: printed once per GPU as `GPU SUMMARY …`.
pub struct Summary<'a> {
    /// The GPU's PCI address.
    pub address: &'a str,
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    /// Its market name, or its maker alone.
    pub market: &'a str,
    /// The kernel driver's name.
    pub driver: &'a str,
    /// What the kernel driver says of itself and the chip: its version, the
    /// chip's family, the firmware versions and the memory it reports.
    pub facts: &'a str,
    /// Whether the GPU is built into the processor, sharing system memory.
    pub integrated: bool,
    /// The memory Metal tells the guest it works in.
    pub working_set: Option<u64>,
    /// The Vulkan driver's heaps, `size/kind` each.
    pub heaps: &'a str,
    /// An integrated GPU's share of RAM: the bytes Linux holds of it and
    /// its size. None where the memory plan gave it none.
    pub share: Option<(u64, u64)>,
    /// A discrete GPU's bound on Linux's pages, which its system-memory
    /// buffers and its evictions from VRAM take: the bytes Linux holds and
    /// the most it may hold. None where the plan set no bound.
    pub pages: Option<(u64, u64)>,
    /// Stela's heap: bytes in use and its size, which an integrated GPU
    /// with no share takes its pages from.
    pub heap_live: usize,
    pub heap_total: usize,
}

impl core::fmt::Display for Summary<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mib = |bytes: u64| bytes >> 20;
        write!(
            f,
            "GPU SUMMARY {} {:04x}:{:04x} rev {:#04x} {}: driver {}",
            self.address, self.vendor, self.device, self.revision, self.market, self.driver
        )?;
        if !self.facts.is_empty() {
            write!(f, " {}", self.facts)?;
        }
        match (self.integrated, self.share) {
            (true, Some((held, limit))) => write!(
                f,
                "; integrated: its memory is the GPU's share of RAM, of which Linux holds {}M \
                 of {}M",
                mib(held),
                mib(limit)
            )?,
            (true, None) => write!(
                f,
                "; integrated with no share of RAM: its memory is system memory from Stela's \
                 heap of {}M ({}M in use)",
                mib(self.heap_total as u64),
                mib(self.heap_live as u64)
            )?,
            (false, _) => {
                f.write_str("; discrete")?;
                if let Some((held, limit)) = self.pages {
                    write!(
                        f,
                        ", its system-memory buffers among Linux's pages, of which Linux holds \
                         {}M of the {}M it may hold",
                        mib(held),
                        mib(limit)
                    )?;
                }
            }
        }
        match self.working_set {
            Some(bytes) => write!(f, "; Metal works in {}M", mib(bytes))?,
            None => f.write_str("; the driver gave no memory size")?,
        }
        if self.heaps.is_empty() {
            f.write_str("; no Vulkan device")
        } else {
            write!(f, "; Vulkan heaps{}", self.heaps)
        }
    }
}

/// What Linux held of its pages at a heap exhaustion stop, after the heap's
/// own figures: an integrated GPU's share (`share`), whose pages its GTT
/// takes, or the bound the plan set on Linux's pages beside a discrete GPU.
/// Written without allocating, as the heap is exhausted.
pub struct PagesAtStop {
    pub held: u64,
    pub limit: u64,
    pub share: bool,
}

impl core::fmt::Display for PagesAtStop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (held, limit) = (self.held >> 20, self.limit >> 20);
        match self.share {
            true => write!(
                f,
                "; the GPU's share of RAM: Linux holds {held}M of {limit}M, its GTT's pages \
                 among them"
            ),
            false => write!(
                f,
                "; Linux's pages, apart from the heap's own needs: it holds {held}M of the \
                 {limit}M it may hold"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::String, vec::Vec};

    #[test]
    fn a_repeated_line_prints_at_its_first_and_each_power_of_two() {
        let mut repeats = Repeats::new();
        let printed: Vec<(u64, String)> = (1..=17)
            .filter_map(|n| {
                repeats
                    .count(format_args!("{}:{}", "amdgpu_irq.c", 670))
                    .map(|seen| (n, format!("{seen}")))
            })
            .collect();
        assert_eq!(
            printed,
            [
                (1, String::new()),
                (2, String::from(" (seen 2 times)")),
                (4, String::from(" (seen 4 times)")),
                (8, String::from(" (seen 8 times)")),
                (16, String::from(" (seen 16 times)")),
            ]
        );
        // Another line is counted apart.
        assert_eq!(
            repeats.count(format_args!("dma-mapping.h:668")),
            Some(Seen::First)
        );
    }

    #[test]
    fn keys_past_the_table_share_one_count() {
        let mut repeats = Repeats::new();
        for key in 0..KEYS {
            assert_eq!(repeats.count(format_args!("{key}")), Some(Seen::First));
        }
        assert_eq!(repeats.count(format_args!("new")), Some(Seen::PastTable(1)));
        assert_eq!(
            format!("{}", Seen::PastTable(2)),
            " (2 past the log's table)"
        );
        assert_eq!(
            repeats.count(format_args!("other")),
            Some(Seen::PastTable(2))
        );
        assert_eq!(repeats.count(format_args!("third")), None);
        // A tracked key still counts on its own.
        assert_eq!(repeats.count(format_args!("0")), Some(Seen::Times(2)));
    }

    /// A `WARN` may come while the heap is exhausted: the count takes no
    /// memory beyond the table it was made with, and its suffix is written
    /// from the count alone.
    #[test]
    fn a_count_allocates_nothing() {
        use crate::native_memory::tests::allocation_failure;
        let mut repeats = Repeats::new();
        allocation_failure::arm(usize::MAX);
        let seen = [(); 4].map(|()| repeats.count(format_args!("{}:{}", "nouveau_bo.c", 42)));
        let observation = allocation_failure::disarm();
        assert_eq!(observation.attempts, 0);
        assert_eq!(
            seen,
            [
                Some(Seen::First),
                Some(Seen::Times(2)),
                None,
                Some(Seen::Times(4))
            ]
        );
    }

    /// A heap stop names an integrated GPU's share as such, and the bound
    /// beside a discrete GPU as Linux's pages, which are no share of RAM.
    #[test]
    fn a_heap_stop_names_linuxs_pages_as_the_plan_held_them() {
        let pages = |share| {
            format!(
                "{}",
                PagesAtStop {
                    held: 300 << 20,
                    limit: 392 << 20,
                    share,
                }
            )
        };
        assert_eq!(
            pages(true),
            "; the GPU's share of RAM: Linux holds 300M of 392M, its GTT's pages among them"
        );
        assert_eq!(
            pages(false),
            "; Linux's pages, apart from the heap's own needs: it holds 300M of the 392M it may \
             hold"
        );
    }

    #[test]
    fn the_summary_names_the_gpu_and_where_its_memory_comes_from() {
        let summary = Summary {
            address: "0000:01:00.0",
            vendor: 0x1002,
            device: 0x13c0,
            revision: 0xcb,
            market: "AMD Radeon Graphics",
            driver: "amdgpu",
            facts: "3.64.0 family GC_10_3_6",
            integrated: true,
            working_set: Some(1512 << 20),
            heaps: " 938M/host 1877M/local",
            share: Some((480 << 20, 2560 << 20)),
            pages: None,
            heap_live: 512 << 20,
            heap_total: 2816 << 20,
        };
        assert_eq!(
            format!("{summary}"),
            "GPU SUMMARY 0000:01:00.0 1002:13c0 rev 0xcb AMD Radeon Graphics: driver amdgpu \
             3.64.0 family GC_10_3_6; integrated: its memory is the GPU's share of RAM, of \
             which Linux holds 480M of 2560M; Metal works in 1512M; Vulkan heaps 938M/host \
             1877M/local"
        );
        let summary = Summary {
            share: None,
            ..summary
        };
        assert_eq!(
            format!("{summary}"),
            "GPU SUMMARY 0000:01:00.0 1002:13c0 rev 0xcb AMD Radeon Graphics: driver amdgpu \
             3.64.0 family GC_10_3_6; integrated with no share of RAM: its memory is system \
             memory from Stela's heap of 2816M (512M in use); Metal works in 1512M; Vulkan \
             heaps 938M/host 1877M/local"
        );
        let summary = Summary {
            integrated: false,
            working_set: None,
            heaps: "",
            facts: "",
            ..summary
        };
        assert_eq!(
            format!("{summary}"),
            "GPU SUMMARY 0000:01:00.0 1002:13c0 rev 0xcb AMD Radeon Graphics: driver amdgpu; \
             discrete; the driver gave no memory size; no Vulkan device"
        );
        // A discrete GPU's pages are held to the plan's bound, which is no
        // share of RAM of its own.
        let summary = Summary {
            pages: Some((212 << 20, 876 << 20)),
            ..summary
        };
        assert_eq!(
            format!("{summary}"),
            "GPU SUMMARY 0000:01:00.0 1002:13c0 rev 0xcb AMD Radeon Graphics: driver amdgpu; \
             discrete, its system-memory buffers among Linux's pages, of which Linux holds 212M \
             of the 876M it may hold; the driver gave no memory size; no Vulkan device"
        );
    }
}
