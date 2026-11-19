//! Where an Intel GPU's Video BIOS Table lies, read from the ACPI OpRegion
//! the GPU's configuration register `ASLS` points at.
//!
//! The display code xe shares with i915 needs the VBT to know the panel a
//! laptop wires to the GPU: without it port A is an external DisplayPort and
//! the panel gets no power sequence. Linux reads the table through its ACPI
//! OpRegion driver (`intel_opregion.c`), which Stela's Linux profile does not
//! build. The OpRegion is plain firmware memory, so the boot reads it and
//! hands the table to the display code as the firmware image its
//! `vbt_firmware` parameter names, where `intel_bios.c` checks it again in
//! full (`intel_bios_is_valid_vbt`). The rules here are
//! `intel_opregion_setup`'s.
use alloc::{boxed::Box, vec, vec::Vec};
use core::ops::Range;

/// The configuration register holding the OpRegion's physical address
/// (`ASLS`, `intel_opregion.c`).
pub const ASLS: u16 = 0xfc;
/// The OpRegion's bytes: a header and five mailboxes.
pub const BYTES: usize = 8 * 1024;

const SIGNATURE: &[u8; 16] = b"IntelGraphicsMem";
/// The header's version: minor, then major.
const MINOR: usize = 0x16;
const MAJOR: usize = 0x17;
/// The header's mask of the mailboxes present.
const MAILBOXES: usize = 0x58;
const MAILBOX_ASLE: u32 = 1 << 2;
const MAILBOX_ASLE_EXT: u32 = 1 << 4;
/// Mailbox 3 (ASLE), with the address and size of a VBT too large for
/// mailbox 4.
const ASLE: usize = 0x300;
const RVDA: usize = ASLE + 0xba;
const RVDS: usize = ASLE + 0xc2;
/// Mailbox 4 holds the VBT, up to mailbox 5 (the ASLE extension) when the
/// header lists that one, else to the OpRegion's end.
const VBT: usize = 0x400;
const ASLE_EXT: usize = 0x1c00;

/// Why `ASLS` leads to no OpRegion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The firmware set no OpRegion address.
    NoAddress,
    /// The memory there does not begin with the OpRegion's signature.
    Signature,
}

/// Where the OpRegion says its VBT is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    /// A table outside the OpRegion (`RVDA`/`RVDS`, OpRegion 2.0 and later),
    /// which comes first when it is valid: its physical address and bytes.
    pub outside: Option<(u64, u32)>,
    /// Mailbox 4's bytes within the OpRegion, the table's place otherwise.
    pub mailbox: Range<usize>,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The VBT's places in the OpRegion `region`, which lies at physical address
/// `asls`.
pub fn locate(asls: u64, region: &[u8; BYTES]) -> Result<Location, Error> {
    if asls == 0 {
        return Err(Error::NoAddress);
    }
    if &region[..SIGNATURE.len()] != SIGNATURE {
        return Err(Error::Signature);
    }
    let mailboxes = word(region, MAILBOXES);
    let (major, minor) = (region[MAJOR], region[MINOR]);
    let rvda = u64::from(word(region, RVDA)) | u64::from(word(region, RVDA + 4)) << 32;
    let rvds = word(region, RVDS);
    let outside = (major >= 2 && mailboxes & MAILBOX_ASLE != 0 && rvda != 0 && rvds != 0)
        .then(|| {
            // OpRegion 2.0: a physical address. From 2.1: an offset from the
            // OpRegion's base, which never points into the OpRegion.
            if major > 2 || minor >= 1 {
                (rvda >= BYTES as u64)
                    .then(|| asls.checked_add(rvda))
                    .flatten()
            } else {
                Some(rvda)
            }
        })
        .flatten()
        .map(|address| (address, rvds));
    let end = if mailboxes & MAILBOX_ASLE_EXT != 0 {
        ASLE_EXT
    } else {
        BYTES
    };
    Ok(Location {
        outside,
        mailbox: VBT..end,
    })
}

/// The table at the start of `bytes`, cut to the size its header gives, if
/// it begins as a VBT and lies within `bytes` (the first checks of
/// `intel_bios_is_valid_vbt`; the display code makes them all).
pub fn table(bytes: &[u8]) -> Option<&[u8]> {
    /// `struct vbt_header`: `$VBT` and a name, version, header size, then the
    /// table's size at 0x18, in 48 bytes.
    const HEADER: usize = 48;
    const SIZE: usize = 0x18;
    if bytes.len() < HEADER || &bytes[..4] != b"$VBT" {
        return None;
    }
    let size = usize::from(u16::from_le_bytes([bytes[SIZE], bytes[SIZE + 1]]));
    (HEADER..=bytes.len())
        .contains(&size)
        .then(|| &bytes[..size])
}

/// Which of its two places the OpRegion's table came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Place {
    /// Outside the OpRegion, at `RVDA`.
    Outside,
    /// Mailbox 4.
    Mailbox,
}

impl core::fmt::Display for Place {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Outside => "RVDA",
            Self::Mailbox => "mailbox 4",
        })
    }
}

/// A Video BIOS Table read from an OpRegion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vbt {
    pub bytes: Vec<u8>,
    pub place: Place,
    /// The OpRegion's version, major then minor.
    pub version: (u8, u8),
}

/// Why an OpRegion gave no table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Absent<E> {
    /// `ASLS` leads to no OpRegion.
    OpRegion(Error),
    /// The firmware's memory could not be read.
    Read(E),
    /// Neither place begins as a table that fits it.
    NoTable,
}

impl<E: core::fmt::Display> core::fmt::Display for Absent<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OpRegion(Error::NoAddress) => f.write_str("the firmware set no address"),
            Self::OpRegion(Error::Signature) => {
                f.write_str("the memory at its address has no OpRegion signature")
            }
            Self::Read(error) => write!(f, "its memory could not be read: {error}"),
            Self::NoTable => f.write_str("it holds no Video BIOS Table"),
        }
    }
}

/// The largest table: its header gives the size in 16 bits.
const LARGEST: usize = u16::MAX as usize;

/// The table of the OpRegion at physical address `asls`, as
/// `intel_opregion_setup` chooses it: the one outside when it is a table,
/// else mailbox 4's. `read` copies firmware memory at a physical address.
pub fn read<E>(
    asls: u64,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), E>,
) -> Result<Vbt, Absent<E>> {
    if asls == 0 {
        return Err(Absent::OpRegion(Error::NoAddress));
    }
    let mut region: Box<[u8; BYTES]> = vec![0; BYTES]
        .into_boxed_slice()
        .try_into()
        .unwrap_or_else(|_| unreachable!());
    read(asls, &mut region[..]).map_err(Absent::Read)?;
    let location = locate(asls, &region).map_err(Absent::OpRegion)?;
    let version = (region[MAJOR], region[MINOR]);
    let mut unread = None;
    if let Some((address, bytes)) = location.outside {
        // A table is never larger than its 16-bit size, whatever RVDS says.
        let mut outside = vec![0; (bytes as usize).min(LARGEST)];
        match read(address, &mut outside) {
            Ok(()) => {
                if let Some(found) = table(&outside) {
                    return Ok(Vbt {
                        bytes: found.to_vec(),
                        place: Place::Outside,
                        version,
                    });
                }
            }
            // Mailbox 4 may still hold one; the failure is the answer if not.
            Err(error) => unread = Some(error),
        }
    }
    table(&region[location.mailbox])
        .map(|found| Vbt {
            bytes: found.to_vec(),
            place: Place::Mailbox,
            version,
        })
        .ok_or(unread.map_or(Absent::NoTable, Absent::Read))
}

/// What the display code reads from a Video BIOS Table for the built-in
/// panel (`intel_bios.c`): which panel entry is its, how its backlight is
/// driven, and the eDP power sequence delays it waits through.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Panel {
    /// The table's BDB version.
    pub version: u16,
    /// The panel's entry, or `None` when the table leaves it to the panel's
    /// EDID (255), which the display code matches against the table's
    /// panel IDs (entry 0 when none matches). A table without the LFP
    /// options block, or with an entry past 15, gives entry 0, the display
    /// code's fallback (`get_panel_type`), with [`Self::fallback`] set.
    pub panel_type: Option<u8>,
    /// Whether [`Self::panel_type`] is the display code's fallback, not the
    /// table's choice.
    pub fallback: bool,
    /// The panel entry's backlight, when the table has the block.
    pub backlight: Option<Backlight>,
    /// The panel entry's eDP power sequence, in 100 µs units: T1+T3, T8,
    /// T9, T10, T11+T12 (`struct edp_power_seq`), when the table has the
    /// eDP block.
    pub power_sequence: Option<[u16; 5]>,
}

/// A panel entry's backlight in the table (`bdb_lfp_backlight`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backlight {
    /// A PWM signal, of `hertz`, from `method` (`lfp_backlight_control_method`
    /// from BDB 191 on: 0 PMIC, 1 LPSS, 2 the display's own PWM, 3 DSI DCS,
    /// 4 the panel driver, 5 the eDP AUX interface; before 191 the
    /// display's own PWM) and its controller, `active_low` where its duty
    /// is inverted.
    Pwm {
        method: u8,
        controller: u8,
        hertz: u16,
        active_low: bool,
    },
    /// No PWM backlight (another entry type): the display code registers no
    /// backlight for the panel (`intel_backlight_setup`), so it sets no
    /// level.
    NoPwm(u8),
}

/// BDB block IDs (`enum bdb_block_id`).
const BDB_EDP: u8 = 27;
const BDB_LFP_OPTIONS: u8 = 40;
const BDB_LFP_BACKLIGHT: u8 = 43;
const BDB_MIPI_SEQUENCE: u8 = 53;

/// The data of the BDB block `id` of `bdb`, as `find_raw_section` walks the
/// blocks: an ID, a 16-bit size (32-bit for a MIPI sequence block of
/// version 3 and later), then the data.
fn block(bdb: &[u8], id: u8) -> Option<&[u8]> {
    let header = usize::from(u16::from_le_bytes([*bdb.get(18)?, *bdb.get(19)?]));
    let total = usize::from(u16::from_le_bytes([*bdb.get(20)?, *bdb.get(21)?])).min(bdb.len());
    let mut index = header;
    while index + 3 < total {
        let current = bdb[index];
        let size = if current == BDB_MIPI_SEQUENCE && *bdb.get(index + 3)? >= 3 {
            u32::from_le_bytes(bdb.get(index + 4..index + 8)?.try_into().ok()?) as usize
        } else {
            usize::from(u16::from_le_bytes([bdb[index + 1], bdb[index + 2]]))
        };
        index += 3;
        let end = index.checked_add(size)?;
        if end > total {
            return None;
        }
        if current == id {
            return Some(&bdb[index..end]);
        }
        index = end;
    }
    None
}

/// What the display code reads for the built-in panel from the Video BIOS
/// Table `vbt` (as [`table`] cut it), or `None` when its data block cannot
/// be found. Every field is read only where the block holds it.
pub fn panel(vbt: &[u8]) -> Option<Panel> {
    /// `struct vbt_header`'s `bdb_offset`.
    const BDB_OFFSET: usize = 0x1c;
    let offset = u32::from_le_bytes(vbt.get(BDB_OFFSET..BDB_OFFSET + 4)?.try_into().ok()?);
    let bdb = vbt.get(offset as usize..)?;
    if bdb.get(..16)? != b"BIOS_DATA_BLOCK " {
        return None;
    }
    let version = u16::from_le_bytes([*bdb.get(16)?, *bdb.get(17)?]);
    // The OpRegion's panel type, which the display code asks first, comes
    // through ACPI, which Stela's Linux does not build.
    let (panel_type, fallback) =
        match block(bdb, BDB_LFP_OPTIONS).and_then(|options| options.first().copied()) {
            Some(panel @ 0..16) => (Some(panel), false),
            Some(255) => (None, false),
            _ => (Some(0), true),
        };
    let backlight = panel_type.and_then(|panel| {
        let panel = usize::from(panel);
        let data = block(bdb, BDB_LFP_BACKLIGHT)?;
        // `entry_size`, then 16 entries of `lfp_backlight_data_entry`.
        const ENTRY: usize = 6;
        if usize::from(*data.first()?) != ENTRY {
            return None;
        }
        let entry = data.get(1 + panel * ENTRY..1 + (panel + 1) * ENTRY)?;
        let kind = entry[0] & 3;
        if kind != 2 {
            return Some(Backlight::NoPwm(kind));
        }
        let hertz = u16::from_le_bytes([entry[1], entry[2]]);
        // `level[16]`, then `backlight_control[16]` from 191 on.
        let (method, controller) = match data.get(1 + 16 * ENTRY + 16 + panel) {
            Some(&control) if version >= 191 => (control & 0xf, control >> 4),
            _ => (2, 0),
        };
        Some(Backlight::Pwm {
            method,
            controller,
            hertz,
            active_low: entry[0] & 4 != 0,
        })
    });
    let power_sequence = panel_type.and_then(|panel| {
        let data = block(bdb, BDB_EDP)?;
        let entry = data.get(usize::from(panel) * 10..usize::from(panel) * 10 + 10)?;
        let mut delays = [0; 5];
        for (delay, bytes) in delays.iter_mut().zip(entry.as_chunks::<2>().0) {
            *delay = u16::from_le_bytes(*bytes);
        }
        Some(delays)
    });
    Some(Panel {
        version,
        panel_type,
        fallback,
        backlight,
        power_sequence,
    })
}

impl core::fmt::Display for Panel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "BDB {}: ", self.version)?;
        match self.panel_type {
            Some(panel) if self.fallback => write!(
                f,
                "panel entry {panel}, the display code's fallback, as the table names no valid one"
            )?,
            Some(panel) => write!(f, "panel entry {panel}")?,
            None => f.write_str("panel entry chosen by the panel's EDID (0 where none matches)")?,
        }
        match self.backlight {
            Some(Backlight::Pwm {
                method,
                controller,
                hertz,
                active_low,
            }) => {
                let by = match method {
                    0 => "the PMIC",
                    1 => "the LPSS PWM",
                    2 => "the display's own PWM",
                    3 => "DSI DCS commands",
                    4 => "the panel driver",
                    5 => "the eDP AUX channel",
                    _ => "an unknown method",
                };
                write!(
                    f,
                    "; backlight by {by} (method {method}, controller {controller}) at {hertz} Hz{}",
                    if active_low { ", active low" } else { "" }
                )?;
            }
            Some(Backlight::NoPwm(kind)) => write!(
                f,
                "; no PWM backlight (type {kind}): the display code registers no backlight and \
                 sets no level"
            )?,
            None if self.panel_type.is_some() => f.write_str(
                "; no backlight block: the display code registers no backlight and sets no level",
            )?,
            None => {}
        }
        match self.power_sequence {
            Some([t1_t3, t8, t9, t10, t11_t12]) => write!(
                f,
                "; eDP power delays T1+T3 {}, T8 {}, T9 {}, T10 {}, T11+T12 {} ms",
                t1_t3 / 10,
                t8 / 10,
                t9 / 10,
                t10 / 10,
                t11_t12 / 10
            ),
            None if self.panel_type.is_some() => {
                f.write_str("; no eDP block: the registers' delays, or the eDP specification's where they read 0")
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Firmware memory holding an OpRegion at `asls` and one more range.
    fn memory<'a>(
        asls: u64,
        region: &'a [u8; BYTES],
        other: (u64, &'a [u8]),
    ) -> impl FnMut(u64, &mut [u8]) -> Result<(), &'static str> + 'a {
        move |address, into| {
            let (base, bytes): (u64, &[u8]) = if address == asls {
                (asls, region)
            } else {
                other
            };
            let start = address.checked_sub(base).ok_or("unmapped")? as usize;
            let from = bytes
                .get(start..start + into.len())
                .ok_or("past the range")?;
            into.copy_from_slice(from);
            Ok(())
        }
    }

    fn vbt(bytes: &mut [u8], size: u16) {
        bytes[..4].copy_from_slice(b"$VBT");
        bytes[0x18..0x1a].copy_from_slice(&size.to_le_bytes());
    }

    #[test]
    fn the_outside_table_comes_first_and_mailbox_four_stands_in() {
        let asls = 0x7a00_0000;
        let mut both = region(2, 1, MAILBOX_ASLE);
        outside(&mut both, 0x2000, 0x3000);
        vbt(&mut both[VBT..], 0x800);
        let mut far = [0u8; 0x3000];
        vbt(&mut far, 0x2345);
        let found = read(asls, memory(asls, &both, (asls + 0x2000, &far))).unwrap();
        assert_eq!((found.place, found.bytes.len()), (Place::Outside, 0x2345));
        assert_eq!(found.version, (2, 1));
        // What RVDA points at is no table: mailbox 4's is used.
        let blank = [0u8; 0x3000];
        let found = read(asls, memory(asls, &both, (asls + 0x2000, &blank))).unwrap();
        assert_eq!((found.place, found.bytes.len()), (Place::Mailbox, 0x800));
        assert_eq!(&found.bytes[..4], b"$VBT");
    }

    #[test]
    fn an_opregion_without_a_table_and_unreadable_memory_are_named() {
        let asls = 0x7a00_0000;
        let empty = region(2, 0, MAILBOX_ASLE);
        assert_eq!(
            read(asls, memory(asls, &empty, (0, &[]))),
            Err(Absent::NoTable)
        );
        assert_eq!(
            read::<&str>(0, |_, _| unreachable!()),
            Err(Absent::OpRegion(Error::NoAddress))
        );
        assert_eq!(
            read(asls, |_, _| Err("refused")),
            Err(Absent::Read("refused"))
        );
        let foreign = [0u8; BYTES];
        assert_eq!(
            read(asls, memory(asls, &foreign, (0, &[]))),
            Err(Absent::OpRegion(Error::Signature))
        );
        // An outside table that cannot be read leaves mailbox 4's, and is
        // the reason when that holds none.
        let mut far = region(2, 0, MAILBOX_ASLE);
        outside(&mut far, 0x7b00_0000, 0x1000);
        assert_eq!(
            read(asls, memory(asls, &far, (u64::MAX, &[]))),
            Err(Absent::Read("unmapped"))
        );
        vbt(&mut far[VBT..], 0x400);
        let found = read(asls, memory(asls, &far, (u64::MAX, &[]))).unwrap();
        assert_eq!((found.place, found.bytes.len()), (Place::Mailbox, 0x400));
    }

    fn region(major: u8, minor: u8, mailboxes: u32) -> [u8; BYTES] {
        let mut region = [0; BYTES];
        region[..16].copy_from_slice(SIGNATURE);
        region[MAJOR] = major;
        region[MINOR] = minor;
        region[MAILBOXES..MAILBOXES + 4].copy_from_slice(&mailboxes.to_le_bytes());
        region
    }

    fn outside(region: &mut [u8; BYTES], rvda: u64, rvds: u32) {
        region[RVDA..RVDA + 8].copy_from_slice(&rvda.to_le_bytes());
        region[RVDS..RVDS + 4].copy_from_slice(&rvds.to_le_bytes());
    }

    #[test]
    fn no_address_and_foreign_memory_are_no_opregion() {
        let valid = region(2, 0, 0);
        assert_eq!(locate(0, &valid), Err(Error::NoAddress));
        assert_eq!(locate(0x7000_0000, &[0; BYTES]), Err(Error::Signature));
    }

    #[test]
    fn mailbox_four_ends_at_the_extension_only_when_it_is_listed() {
        let asls = 0x7a00_0000;
        let plain = locate(asls, &region(2, 0, MAILBOX_ASLE)).unwrap();
        assert_eq!(plain.mailbox, 0x400..0x2000);
        assert_eq!(plain.outside, None);
        let extended = locate(asls, &region(2, 0, MAILBOX_ASLE | MAILBOX_ASLE_EXT)).unwrap();
        assert_eq!(extended.mailbox, 0x400..0x1c00);
    }

    #[test]
    fn a_table_outside_is_physical_in_2_0_and_relative_from_2_1() {
        let asls = 0x7a00_0000;
        let mut old = region(2, 0, MAILBOX_ASLE);
        outside(&mut old, 0x7b00_0000, 0x2400);
        assert_eq!(
            locate(asls, &old).unwrap().outside,
            Some((0x7b00_0000, 0x2400))
        );
        let mut new = region(2, 1, MAILBOX_ASLE);
        outside(&mut new, 0x2000, 0x2400);
        assert_eq!(
            locate(asls, &new).unwrap().outside,
            Some((0x7a00_2000, 0x2400))
        );
        let mut later = region(3, 0, MAILBOX_ASLE);
        outside(&mut later, 0x3000, 0x1000);
        assert_eq!(
            locate(asls, &later).unwrap().outside,
            Some((0x7a00_3000, 0x1000))
        );
    }

    #[test]
    fn an_outside_table_needs_version_two_the_asle_mailbox_and_both_fields() {
        let asls = 0x7a00_0000;
        let mut first = region(1, 9, MAILBOX_ASLE);
        outside(&mut first, 0x7b00_0000, 0x2400);
        assert_eq!(locate(asls, &first).unwrap().outside, None);
        let mut no_asle = region(2, 1, 0);
        outside(&mut no_asle, 0x2000, 0x2400);
        assert_eq!(locate(asls, &no_asle).unwrap().outside, None);
        let mut no_size = region(2, 1, MAILBOX_ASLE);
        outside(&mut no_size, 0x2000, 0);
        assert_eq!(locate(asls, &no_size).unwrap().outside, None);
        // From 2.1 an offset into the OpRegion itself is not a table.
        let mut inside = region(2, 1, MAILBOX_ASLE);
        outside(&mut inside, 0x400, 0x1000);
        assert_eq!(locate(asls, &inside).unwrap().outside, None);
    }

    #[test]
    fn a_table_is_cut_to_its_headers_size_and_refused_past_its_bytes() {
        let mut bytes = [0u8; 0x1c00];
        assert_eq!(table(&bytes), None);
        bytes[..4].copy_from_slice(b"$VBT");
        bytes[0x18..0x1a].copy_from_slice(&0x1234u16.to_le_bytes());
        assert_eq!(table(&bytes).map(<[u8]>::len), Some(0x1234));
        assert_eq!(table(&bytes[..0x1000]), None);
        bytes[0x18..0x1a].copy_from_slice(&20u16.to_le_bytes());
        assert_eq!(table(&bytes), None);
        assert_eq!(table(&bytes[..40]), None);
    }

    /// A table of BDB `version` with `blocks`, each an ID and its data.
    fn with_blocks(version: u16, blocks: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut bdb = b"BIOS_DATA_BLOCK ".to_vec();
        bdb.extend(version.to_le_bytes());
        bdb.extend(22u16.to_le_bytes());
        bdb.extend(0u16.to_le_bytes());
        for (id, data) in blocks {
            bdb.push(*id);
            bdb.extend((data.len() as u16).to_le_bytes());
            bdb.extend(data);
        }
        let size = bdb.len() as u16;
        bdb[20..22].copy_from_slice(&size.to_le_bytes());
        let mut table = vec![0; 48];
        table[..4].copy_from_slice(b"$VBT");
        table[0x1c..0x20].copy_from_slice(&48u32.to_le_bytes());
        table.extend(bdb);
        let total = table.len() as u16;
        table[0x18..0x1a].copy_from_slice(&total.to_le_bytes());
        table
    }

    #[test]
    fn the_panels_backlight_and_power_delays_are_its_entrys() {
        let mut backlight = vec![6];
        for panel in 0..16u8 {
            // Panel 3: a PWM of 200 Hz, active low; the others none.
            let kind = if panel == 3 { 2 | 4 } else { 0 };
            backlight.extend([kind, 200, 0, 0, 0, 0]);
        }
        backlight.extend([0; 16]);
        let mut methods = [0u8; 16];
        methods[3] = 5 | 1 << 4;
        backlight.extend(methods);
        let mut edp = vec![0; 160];
        edp[30..40].copy_from_slice(&[0xd2, 0, 0x64, 0, 0xc8, 0, 0xf4, 1, 0x88, 0x13]);
        // A MIPI block of version 3 comes first, its size in 32 bits after
        // its version (the 16-bit size is not read).
        let mut mipi = vec![3];
        mipi.extend(6u32.to_le_bytes());
        mipi.push(0);
        let vbt = with_blocks(
            236,
            &[
                (BDB_MIPI_SEQUENCE, mipi),
                (BDB_LFP_OPTIONS, vec![3, 0, 0, 0]),
                (BDB_LFP_BACKLIGHT, backlight),
                (BDB_EDP, edp),
            ],
        );
        let panel = panel(table(&vbt).unwrap()).unwrap();
        assert_eq!(
            panel,
            Panel {
                version: 236,
                panel_type: Some(3),
                fallback: false,
                backlight: Some(Backlight::Pwm {
                    method: 5,
                    controller: 1,
                    hertz: 200,
                    active_low: true,
                }),
                power_sequence: Some([210, 100, 200, 500, 5000]),
            }
        );
        assert_eq!(
            alloc::format!("{panel}"),
            "BDB 236: panel entry 3; backlight by the eDP AUX channel (method 5, controller 1) \
             at 200 Hz, active low; eDP power delays T1+T3 21, T8 10, T9 20, T10 50, T11+T12 \
             500 ms"
        );
        // Before 191 the PWM is the display's own; a panel the EDID picks
        // has no entry; no data block, no answer.
        let old = with_blocks(
            190,
            &[
                (BDB_LFP_OPTIONS, vec![3]),
                (BDB_LFP_BACKLIGHT, {
                    let mut data = vec![6];
                    data.extend([2, 0x10, 0x27, 0, 0, 0].repeat(16));
                    data
                }),
            ],
        );
        let panel = super::panel(&old).unwrap();
        assert_eq!(
            panel.backlight,
            Some(Backlight::Pwm {
                method: 2,
                controller: 0,
                hertz: 10000,
                active_low: false,
            })
        );
        assert_eq!(panel.power_sequence, None);
        assert_eq!(
            alloc::format!("{panel}"),
            "BDB 190: panel entry 3; backlight by the display's own PWM (method 2, controller \
             0) at 10000 Hz; no eDP block: the registers' delays, or the eDP specification's \
             where they read 0"
        );
        // An entry type other than PWM: no backlight at all.
        let unlit = with_blocks(
            240,
            &[
                (BDB_LFP_OPTIONS, vec![0]),
                (BDB_LFP_BACKLIGHT, {
                    let mut data = vec![6];
                    data.extend([1, 0, 0, 0, 0, 0].repeat(16));
                    data
                }),
            ],
        );
        let panel = super::panel(&unlit).unwrap();
        assert_eq!(panel.backlight, Some(Backlight::NoPwm(1)));
        assert!(
            alloc::format!("{panel}")
                .contains("no PWM backlight (type 1): the display code registers no backlight")
        );
        let by_edid = with_blocks(240, &[(BDB_LFP_OPTIONS, vec![255])]);
        let panel = super::panel(&by_edid).unwrap();
        assert_eq!((panel.panel_type, panel.backlight), (None, None));
        assert_eq!(
            alloc::format!("{panel}"),
            "BDB 240: panel entry chosen by the panel's EDID (0 where none matches)"
        );
        // An entry past 15, or no options block, is the fallback's entry 0.
        for vbt in [
            with_blocks(240, &[(BDB_LFP_OPTIONS, vec![16])]),
            with_blocks(240, &[]),
        ] {
            let panel = super::panel(&vbt).unwrap();
            assert_eq!((panel.panel_type, panel.fallback), (Some(0), true));
            assert!(alloc::format!("{panel}").starts_with(
                "BDB 240: panel entry 0, the display code's fallback, as the table names no \
                 valid one; no backlight block"
            ));
        }
        let mut broken = by_edid.clone();
        broken[48] = b'X';
        assert_eq!(super::panel(&broken), None);
        // A block that claims more than the table holds ends the walk.
        let mut short = with_blocks(240, &[(BDB_LFP_OPTIONS, vec![1])]);
        let last = short.len() - 2;
        short[last] = 0xff;
        let panel = super::panel(&short).unwrap();
        assert_eq!((panel.panel_type, panel.fallback), (Some(0), true));
    }
}
