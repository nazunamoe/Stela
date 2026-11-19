//! What Stela knows of the firmware's runtime services after their boot
//! services end: the regions the firmware's memory map marks
//! `EFI_MEMORY_RUNTIME`, how the loader's tables map each one, and how a
//! restart comes back to the firmware entry this start came from.
//!
//! Stela never calls `SetVirtualAddressMap`, so the services run at their
//! physical addresses in the loader's root, which must map every runtime
//! region (UEFI 2.10 7.4.1, 8.4). RAM is mapped by identity whole, so a
//! runtime region in RAM needs nothing more; a region outside RAM, such as
//! a flash window or a controller's registers, is mapped page by page. A
//! region the firmware adds to its map at its exit, after the tables were
//! built, is not mapped: the services are then not called after the exit.
//! Each region is one boot log line ([`Region::line`]), and a fault inside
//! a firmware call says whether its address lies in one ([`Regions::place`]).
use crate::firmware_map::{self, RuntimeMapping};
use alloc::{format, string::String};

const PAGE: u64 = 4096;
/// The regions a boot keeps for its fault lines; the rest are counted.
pub const MAX_REGIONS: usize = 64;

/// How one runtime region is reached in the loader's root.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Reach {
    #[default]
    Unset = 0,
    /// Every page lies in RAM, which the identity map holds.
    Ram = 1,
    /// Every page was mapped for it.
    Mapped = 2,
    /// Some pages lie in RAM, the rest were mapped for it.
    Partly = 3,
    /// Its pages are the storage controllers' registers, mapped already,
    /// with RAM or mapped pages for the rest.
    Devices = 4,
    /// The firmware's map lists it only at the exit: not mapped.
    AtExit = 5,
}

impl Reach {
    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Ram,
            2 => Self::Mapped,
            3 => Self::Partly,
            4 => Self::Devices,
            5 => Self::AtExit,
            _ => Self::Unset,
        }
    }
}

/// One runtime region of the firmware's memory map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Region {
    pub start: u64,
    pub pages: u64,
    pub attributes: u64,
    pub kind: u32,
    reach: u8,
    mapping: u8,
    _reserved: [u8; 2],
}

const fn mapping_code(mapping: RuntimeMapping) -> u8 {
    match mapping {
        RuntimeMapping::Code => 1,
        RuntimeMapping::WriteBack => 2,
        RuntimeMapping::Uncached => 3,
    }
}

impl Region {
    /// A region the firmware's map lists only at its exit: not mapped.
    pub const fn at_exit(start: u64, pages: u64, kind: u32, attributes: u64) -> Self {
        Self {
            start,
            pages,
            attributes,
            kind,
            reach: Reach::AtExit as u8,
            mapping: mapping_code(firmware_map::runtime_mapping(kind, attributes)),
            _reserved: [0; 2],
        }
    }

    pub const fn end(&self) -> u64 {
        self.start.saturating_add(self.pages.saturating_mul(PAGE))
    }

    pub const fn reach(&self) -> Reach {
        Reach::from_u8(self.reach)
    }

    fn mapping_name(&self) -> &'static str {
        match self.mapping {
            1 => "executable",
            2 => "write-back",
            _ => "uncached",
        }
    }

    /// `FIRMWARE RUNTIME REGION ...`: the descriptor and how it is reached.
    pub fn line(&self) -> String {
        let reach = match self.reach() {
            Reach::Ram => String::from("in RAM, which the identity map holds"),
            Reach::Mapped => format!("mapped {}", self.mapping_name()),
            Reach::Partly => format!(
                "partly in RAM, which the identity map holds, the rest mapped {}",
                self.mapping_name()
            ),
            Reach::Devices => String::from(
                "over the storage controllers' registers, mapped uncached already, the rest in \
                 RAM or mapped",
            ),
            Reach::AtExit => String::from(
                "listed only in the firmware's map at its exit, after Stela's tables were \
                 built: not mapped",
            ),
            Reach::Unset => String::from("not mapped"),
        };
        format!(
            "FIRMWARE RUNTIME REGION type={} ({}) attributes={:#x} start={:#x} pages={:#x}: \
             {reach}\n",
            self.kind,
            firmware_map::kind_name(self.kind),
            self.attributes,
            self.start,
            self.pages
        )
    }
}

/// Whether `page` lies in one of `ranges` (start, end).
fn within(ranges: &[(u64, u64)], page: u64) -> bool {
    ranges
        .iter()
        .any(|&(start, end)| start <= page && page + PAGE <= end)
}

/// Whether `page` touches one of `ranges` (start, end), which need not be
/// page aligned.
fn touches(ranges: &[(u64, u64)], page: u64) -> bool {
    ranges
        .iter()
        .any(|&(start, end)| start & !(PAGE - 1) < page + PAGE && page < end.next_multiple_of(PAGE))
}

/// The pages of the runtime region `start`, `pages` that the loader maps
/// itself: those neither in `ram`, which the identity map holds, nor over
/// `devices`, mapped already. A region that only partly lies in RAM still
/// has its other pages mapped.
pub fn pages_to_map<'a>(
    start: u64,
    pages: u64,
    ram: &'a [(u64, u64)],
    devices: &'a [(u64, u64)],
) -> impl Iterator<Item = u64> + 'a {
    (0..pages)
        .map(move |index| start + index * PAGE)
        .filter(move |&page| !within(ram, page) && !touches(devices, page))
}

/// The region `start`, `pages` of `kind` and `attributes`, with how the
/// loader reaches it.
pub fn plan(
    start: u64,
    pages: u64,
    kind: u32,
    attributes: u64,
    ram: &[(u64, u64)],
    devices: &[(u64, u64)],
) -> Region {
    let (mut in_ram, mut over_devices, mut mapped) = (0u64, 0u64, 0u64);
    for index in 0..pages {
        let page = start + index * PAGE;
        if within(ram, page) {
            in_ram += 1;
        } else if touches(devices, page) {
            over_devices += 1;
        } else {
            mapped += 1;
        }
    }
    let reach = match (in_ram, over_devices, mapped) {
        (_, 1.., _) => Reach::Devices,
        (0, 0, _) => Reach::Mapped,
        (_, 0, 0) => Reach::Ram,
        _ => Reach::Partly,
    };
    Region {
        start,
        pages,
        attributes,
        kind,
        reach: reach as u8,
        mapping: mapping_code(firmware_map::runtime_mapping(kind, attributes)),
        _reserved: [0; 2],
    }
}

/// Whether the loader's tables reach every byte of `start`, `pages`: pages
/// in `ram`, or in one of the runtime regions `declared` (start, pages)
/// the loader mapped, each of them, whether or not [`Regions`] kept it for
/// the fault lines.
pub fn reached(start: u64, pages: u64, declared: &[(u64, u64)], ram: &[(u64, u64)]) -> bool {
    (0..pages).all(|index| {
        let page = start + index * PAGE;
        within(ram, page)
            || declared.iter().any(|&(first, count)| {
                first <= page && page + PAGE <= first.saturating_add(count.saturating_mul(PAGE))
            })
    })
}

/// The runtime regions a boot keeps for the engine's fault lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Regions {
    pub count: u32,
    /// Regions past [`MAX_REGIONS`], counted only.
    pub dropped: u32,
    pub regions: [Region; MAX_REGIONS],
}

impl Default for Regions {
    fn default() -> Self {
        Self {
            count: 0,
            dropped: 0,
            regions: [Region::default(); MAX_REGIONS],
        }
    }
}

impl Regions {
    pub fn push(&mut self, region: Region) {
        match self.regions.get_mut(self.count as usize) {
            Some(slot) => {
                *slot = region;
                self.count += 1;
            }
            None => self.dropped = self.dropped.saturating_add(1),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Region> {
        self.regions[..(self.count as usize).min(MAX_REGIONS)].iter()
    }

    /// Where `address`, which a firmware call faulted at, lies among the
    /// runtime regions: for the fault's line.
    pub fn place(&self, address: u64) -> String {
        if self.count == 0 {
            return String::from("the firmware's map declared no runtime region Stela kept");
        }
        match self
            .iter()
            .find(|region| region.start <= address && address < region.end())
        {
            Some(region) => format!(
                "the address lies in a runtime region the firmware declared: {}",
                region.line().trim_end()
            ),
            None => format!(
                "the address lies in none of the {} runtime regions the firmware declared{}, \
                 so its runtime services use memory their map does not give them",
                self.count,
                match self.dropped {
                    0 => String::new(),
                    dropped => format!(" (and {dropped} more Stela did not keep)"),
                }
            ),
        }
    }
}

/// How a restart comes back to the firmware entry this start came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Return {
    /// The firmware named no entry for this start (`BootCurrent`): it
    /// chooses what starts.
    Unnamed,
    /// The entry is first in the firmware's `BootOrder`, which starts it
    /// again: no variable is written.
    First(u16),
    /// `BootNext` is written at the restart, through the runtime services.
    AtRestart(u16),
    /// `BootNext` was written before the firmware's exit, as this PC's
    /// runtime variable service faulted in an earlier boot.
    BeforeExit(u16),
    /// This PC's runtime variable service faulted in an earlier boot and
    /// the firmware refused `BootNext` before its exit: nothing is written,
    /// the firmware chooses.
    Refused(u16),
}

impl Return {
    /// From `BootCurrent`, if the firmware has it, and `BootOrder`'s bytes.
    pub fn decide(current: Option<u16>, order: &[u8]) -> Self {
        let Some(current) = current else {
            return Self::Unnamed;
        };
        let first = order
            .get(..2)
            .map(|first| u16::from_le_bytes([first[0], first[1]]));
        match first {
            Some(first) if first == current => Self::First(current),
            _ => Self::AtRestart(current),
        }
    }

    /// For the handoff: kind in bits 16 and up, entry below.
    pub const fn encode(self) -> u64 {
        let (kind, entry) = match self {
            Self::Unnamed => (0, 0),
            Self::First(entry) => (1, entry),
            Self::AtRestart(entry) => (2, entry),
            Self::BeforeExit(entry) => (3, entry),
            Self::Refused(entry) => (4, entry),
        };
        (kind << 16) | entry as u64
    }

    pub const fn decode(value: u64) -> Self {
        let entry = value as u16;
        match value >> 16 {
            1 => Self::First(entry),
            2 => Self::AtRestart(entry),
            3 => Self::BeforeExit(entry),
            4 => Self::Refused(entry),
            _ => Self::Unnamed,
        }
    }

    /// The `POWER ...` line a restart writes, where no runtime call is
    /// made; `None` for [`Self::AtRestart`], whose line follows the call.
    pub fn line(self) -> Option<String> {
        Some(match self {
            Self::AtRestart(_) => return None,
            Self::Unnamed => String::from(
                "POWER the firmware chooses what starts after the restart: it named no entry for \
                 this start\n",
            ),
            Self::First(entry) => format!(
                "POWER the firmware starts its entry Boot{entry:04X} again after the restart: it \
                 is first in the firmware's boot order, so no variable is written\n"
            ),
            Self::BeforeExit(entry) => format!(
                "POWER the firmware starts its entry Boot{entry:04X} again after the restart, \
                 by the BootNext written before its exit\n"
            ),
            Self::Refused(entry) => format!(
                "POWER the firmware chooses what starts after the restart: it refused BootNext \
                 for its entry Boot{entry:04X} before its exit, and its variable service is not \
                 called after the exit on this PC\n"
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: [(u64, u64); 2] = [(0x1000, 0x8000_0000), (0x1_0000_0000, 0x2_0000_0000)];

    #[test]
    fn a_flash_window_outside_ram_is_mapped_uncached_page_by_page() {
        let region = plan(0xffd0_0000, 0x100, 11, 0x8000_0000_0000_0001, &RAM, &[]);
        assert_eq!(region.reach(), Reach::Mapped);
        assert_eq!(pages_to_map(0xffd0_0000, 0x100, &RAM, &[]).count(), 0x100);
        assert_eq!(
            region.line(),
            "FIRMWARE RUNTIME REGION type=11 (MMIO) attributes=0x8000000000000001 \
             start=0xffd00000 pages=0x100: mapped uncached\n"
        );
    }

    #[test]
    fn a_region_partly_in_ram_has_its_other_pages_mapped() {
        // Two pages below the end of RAM and two past it: all four were
        // left unmapped when any overlap with RAM skipped the region.
        let start = 0x8000_0000 - 2 * PAGE;
        let mapped: alloc::vec::Vec<u64> = pages_to_map(start, 4, &RAM, &[]).collect();
        assert_eq!(mapped, [0x8000_0000, 0x8000_1000]);
        let region = plan(start, 4, 0, 0x8000_0000_0000_0000, &RAM, &[]);
        assert_eq!(region.reach(), Reach::Partly);
        assert!(region.line().ends_with("the rest mapped uncached\n"));
        let code = plan(0x7000_0000, 16, 5, 0x8000_0000_0000_000f, &RAM, &[]);
        assert_eq!(code.reach(), Reach::Ram);
    }

    #[test]
    fn storage_registers_are_not_mapped_twice() {
        let devices = [(0xfe00_0010, 0xfe00_2000)];
        assert_eq!(
            pages_to_map(0xfe00_0000, 3, &RAM, &devices).collect::<alloc::vec::Vec<_>>(),
            [0xfe00_2000]
        );
        assert_eq!(
            plan(0xfe00_0000, 3, 11, 1 << 63, &RAM, &devices).reach(),
            Reach::Devices
        );
    }

    #[test]
    fn a_fault_says_whether_its_address_lies_in_a_declared_region() {
        let mut regions = Regions::default();
        assert_eq!(
            regions.place(0xffd0_c04c),
            "the firmware's map declared no runtime region Stela kept"
        );
        regions.push(plan(0x7700_0000, 0x40, 5, 1 << 63 | 0xf, &RAM, &[]));
        regions.push(plan(0xfed8_0000, 1, 11, 1 << 63 | 1, &RAM, &[]));
        let outside = regions.place(0xffd0_c04c);
        assert!(
            outside.starts_with("the address lies in none of the 2 runtime regions"),
            "{outside}"
        );
        regions.push(plan(0xffd0_0000, 0x100, 11, 1 << 63 | 1, &RAM, &[]));
        let inside = regions.place(0xffd0_c04c);
        assert!(inside.contains("start=0xffd00000"), "{inside}");
        assert!(inside.ends_with("mapped uncached"), "{inside}");
    }

    #[test]
    fn a_region_listed_only_at_the_exit_is_not_reached() {
        let declared = [(0xffd0_0000, 0x10)];
        assert!(reached(0xffd0_4000, 4, &declared, &RAM));
        // Runtime data the firmware took from RAM at its exit is reached.
        assert!(reached(0x7000_0000, 4, &declared, &RAM));
        assert!(!reached(0xff00_0000, 1, &declared, &RAM));
        let mut regions = Regions::default();
        regions.push(Region::at_exit(0xff00_0000, 1, 11, 1 << 63));
        assert!(regions.place(0xff00_0010).ends_with("not mapped"));
    }

    /// A PC with more runtime regions than the fault lines keep: one past
    /// them, outside RAM and mapped, was taken for a region added at the
    /// exit, and the variable service was refused after the exit, so a
    /// restart wrote no BootNext.
    #[test]
    fn a_mapped_region_past_the_kept_ones_is_reached_at_the_exit() {
        let mut regions = Regions::default();
        let declared: alloc::vec::Vec<(u64, u64)> = (0..MAX_REGIONS as u64 + 6)
            .map(|index| (0xfe00_0000 + index * 2 * PAGE, 2))
            .collect();
        for &(start, pages) in &declared {
            regions.push(plan(start, pages, 11, 1 << 63 | 1, &RAM, &[]));
        }
        assert_eq!(regions.dropped, 6);
        let &(last, pages) = declared.last().unwrap();
        assert!(reached(last, pages, &declared, &RAM));
        assert!(!reached(last + 2 * PAGE, 1, &declared, &RAM));
    }

    #[test]
    fn the_table_keeps_its_first_regions_and_counts_the_rest() {
        let mut regions = Regions::default();
        for index in 0..MAX_REGIONS as u64 + 3 {
            regions.push(plan(0xf000_0000 + index * PAGE, 1, 11, 1 << 63, &RAM, &[]));
        }
        assert_eq!((regions.count, regions.dropped), (MAX_REGIONS as u32, 3));
        assert!(
            regions
                .place(0x10)
                .contains("(and 3 more Stela did not keep)")
        );
    }

    #[test]
    fn a_restart_writes_boot_next_only_where_the_firmware_would_start_another_entry() {
        assert_eq!(Return::decide(None, &[0, 0]), Return::Unnamed);
        assert_eq!(Return::decide(Some(3), &[3, 0, 1, 0]), Return::First(3));
        assert_eq!(Return::decide(Some(3), &[1, 0, 3, 0]), Return::AtRestart(3));
        assert_eq!(Return::decide(Some(3), &[]), Return::AtRestart(3));
        assert_eq!(Return::decide(Some(0x103), &[3, 1]), Return::First(0x103));
        for value in [
            Return::Unnamed,
            Return::First(0),
            Return::AtRestart(0xffff),
            Return::BeforeExit(7),
            Return::Refused(2),
        ] {
            assert_eq!(Return::decode(value.encode()), value);
        }
        assert_eq!(Return::AtRestart(1).line(), None);
        assert!(
            Return::First(0)
                .line()
                .unwrap()
                .contains("Boot0000 again after the restart: it is first")
        );
    }
}
