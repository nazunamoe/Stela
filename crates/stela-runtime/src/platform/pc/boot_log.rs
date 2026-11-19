//! The persistent boot log: `stela.efi`'s output kept on the machine-state
//! partition, so a bare-metal run leaves a log to read after the machine
//! reboots.
//!
//! The region starts at [`OFFSET`] of the partition, after the NOR image and
//! the SEP's state, and is [`LOG_BYTES`] long; a partition of at least
//! [`PARTITION_BYTES`] holds it. Its first [`HEADER_BYTES`] are the header:
//!
//! | Offset | Bytes | Field |
//! | --- | --- | --- |
//! | 0 | 8 | magic `STELALOG` |
//! | 8 | 4 | version, 2 |
//! | 12 | 4 | reserved, zero |
//! | 16 | 8 | `head`: every byte ever appended, monotonic |
//! | 24 | 8 | `boots`: boots that opened the log |
//! | 32 | 8 | `ring_bytes` |
//! | 40 | 4 | `records`: boot records after the ring, at most 16 |
//! | 44 | 4 | `first_bytes`: a record's room for a boot's first lines |
//! | 48 | 4 | `last_bytes`: a record's room for a boot's last lines |
//! | 52 | 4 | reserved, zero |
//! | 56 | 24 each | per record: boot, `start`, first length, last length |
//! | after them | 32 | SHA-256 of every byte before it |
//! | 3584 | 512 | a copy of the header's first 512 bytes ([`COPY_AT`]) |
//!
//! Every store writes the 4 KiB whole, the copy with it. A header that does
//! not decode is read from its copy, and so is the log a loader older than
//! the copy started over a longer one's start: such a loader writes its
//! header through a 512-byte block, never the copy, which then still names
//! the longer log, whose records and ring past the older log stay
//! ([`merged`]).
//!
//! The ring follows the header. Stream byte `i` lives at
//! `ring[i % ring_bytes]`, so the valid stream is
//! `[head.saturating_sub(ring_bytes), head)`. A blank or invalid header is a
//! fresh log, reported by its [`HeaderError`]; [`read`] refuses it.
//!
//! The boot records follow the ring ([`Layout`]): a running macOS writes
//! through the whole ring in a few hours, and the lines that say what the
//! PC is (version, processor, devices, GPU) are a boot's first. Each of the
//! last [`RECORDS`] boots keeps a copy of its first [`FIRST_BYTES`] of
//! lines and its last [`LAST_BYTES`], whatever the ring holds by then. A
//! record's `start` is the stream position of the boot's marker line. Its
//! first lines are stored once, in the store during which they are
//! complete, or the next one where no header on the device names the
//! record as this boot's yet (the room may still be an older boot's); a
//! boot that ends before that, and every boot's last lines, are copied from
//! the ring when the next boot opens the log, which is always in time: the
//! ring's newest bytes are the last boot's. So a line costs what it did
//! without records, and a boot costs two record writes. A record's first
//! lines start with its boot's marker; [`read`] drops one that does not.
//!
//! Version 1 had no records, 40 bytes of fields, and a ring filling the
//! region. [`read`] reads both. A [`Writer`] that opens a version 1 log
//! converts it in place ([`convert`]): the ring keeps its newest bytes, the
//! bytes the record area takes are the oldest, and the header is written
//! last. Software older than the records refuses a version 2 header by its
//! version and starts a fresh log; it never reads one as the other.
//!
//! [`Writer`] opens the log once per boot, counts the boot and appends a
//! `=== STELA BOOT n ===` line, with a note after it on what the boot is
//! and when it started ([`Writer::open_noted`]). Appends wait in a bounded [`Pending`]
//! buffer; a flush writes the changed ring blocks, makes them durable, then
//! writes the header and makes it durable, so the header never counts bytes
//! the ring does not hold. Once the ring has wrapped, an interrupted flush can
//! leave the bytes it wrote in place of the oldest ones the old header still
//! counts, at most the bytes of that one flush.
//!
//! A file preallocated on a FAT32 volume can hold a log too: a machine-state
//! file (`--nor-file`) laid out as the partition is, or the USB's own log
//! (`stela_loader::mount_interface::BOOT_LOG_FILE`), the region alone, as
//! long as the file is ([`file_region`]): [`LOG_BYTES`] on a stick an older
//! `stela` made, [`FILE_BYTES`] where its EFI partition has room for it.
//! [`Layout::of`] gives a region of [`LARGE_BYTES`] or more [`LARGE_RECORDS`]
//! larger records. `stela` makes a file longer with [`relayout`], which keeps
//! its lines at their positions. A loader older than the longer file opens
//! it as a [`LOG_BYTES`] region and starts a log of that size at its start,
//! whose header names a smaller region than the file's: [`read`] reads that
//! log after the longer one its header's copy names, as [`read_newest`]
//! does, and a [`Writer`] lays both out in the whole file in place
//! ([`grow`]), so the boots of both stay. Writes reach its blocks directly, never
//! the volume's directory or FAT: [`file_runs`] gives the block-aligned runs
//! of its clusters, wherever they lie, that hold the bytes its layout needs.
use super::fat32;
use alloc::{collections::VecDeque, format, vec, vec::Vec};
use core::{
    convert::Infallible,
    fmt::{self, Write as _},
};
use stela_crypto::Sha256;
use stela_crypto::digest::Digest;
use stela_drivers::block::{BlockDevice, BlockError, Finished, Request};
use stela_soc_data::nor::CAPACITY;

/// Where the region starts in the machine-state partition.
pub const OFFSET: u64 = CAPACITY as u64 + stela_formats::machine_state::SEP_BYTES;
/// The region on a machine-state partition, and the smallest log file: the
/// header, the ring and [`RECORDS`] boot records.
pub const LOG_BYTES: u64 = 4 << 20;
/// The USB's log file as `stela` makes it, where the stick's 200 MiB EFI
/// partition keeps room for two loaders beside it. Its ring of about 25 MiB
/// holds a day's run of a booted desktop (2.8 MB in its first ten minutes,
/// 3.4 MB in each of the next five ten-minute steps of its first hour,
/// 0.8 MB in each later hour: `stela_boot_core::beat_pace`), so an
/// eight-hour run is kept whole and a run of several days keeps its last
/// day, and its records the first lines of the last [`LARGE_RECORDS`] boots.
pub const FILE_BYTES: u64 = 32 << 20;
/// The longest log file a boot writes: a longer one's first bytes.
pub const MAX_FILE_BYTES: u64 = 64 << 20;
/// A region this long or longer has [`LARGE_RECORDS`] records of
/// [`LARGE_FIRST_BYTES`] and [`LARGE_LAST_BYTES`].
pub const LARGE_BYTES: u64 = 16 << 20;
/// The boot records of a region of [`LARGE_BYTES`] or more.
pub const LARGE_RECORDS: u32 = 16;
/// A large record's room for a boot's first lines: up to macOS's start, 56
/// to 126 KB on the reporters' PCs, 278 KB on a PC whose GPU's firmware
/// files the bundle lists one record line each (165 KB of it after the
/// log's per-family budgets), with room to spare.
pub const LARGE_FIRST_BYTES: u32 = 384 << 10;
/// A large record's room for a boot's last lines: about a minute of a
/// booted desktop's lines before it stopped or restarted.
pub const LARGE_LAST_BYTES: u32 = 64 << 10;
/// The smallest machine-state partition that holds the log.
pub const PARTITION_BYTES: u64 = OFFSET + LOG_BYTES;
pub const HEADER_BYTES: u64 = 4096;
/// Where the header's copy starts in the header's 4 KiB: its last 512
/// bytes, which no loader older than the copy writes (they write the
/// header's first block alone, of 512 bytes on every path a stick takes).
pub const COPY_AT: u64 = HEADER_BYTES - 512;
/// The boot records of a [`LOG_BYTES`] region.
pub const RECORDS: u32 = 8;
/// A record's room for a boot's first lines: what a boot prints before it
/// starts macOS, 90 to 130 KiB on the PCs seen.
pub const FIRST_BYTES: u32 = 112 << 10;
/// A record's room for a boot's last lines.
pub const LAST_BYTES: u32 = 16 << 10;
/// The most records a header names: they fit its first 512-byte block.
const MAX_RECORDS: u32 = 16;
/// The ring of a [`LOG_BYTES`] region.
pub const RING_BYTES: u64 = Layout::of(LOG_BYTES).ring_bytes;
/// The bound of a pending buffer between flushes.
pub const PENDING_BYTES: usize = 1 << 20;
pub const MAGIC: &[u8; 8] = b"STELALOG";
pub const VERSION: u32 = 2;
/// The version without boot records.
const VERSION_1: u32 = 1;
/// The fields a version 1 header's checksum covers.
const FIELDS_1: usize = 40;
/// The fields before a version 2 header's records, and each record's.
const FIELDS: usize = 56;
const RECORD_FIELDS: usize = 24;

/// How a region is divided after its header: the ring, then the boot
/// records, each `first_bytes + last_bytes` long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub ring_bytes: u64,
    pub records: u32,
    pub first_bytes: u32,
    pub last_bytes: u32,
}

impl Layout {
    /// The layout a writer gives a `region`-byte region: [`RECORDS`]
    /// records in a whole log's, [`LARGE_RECORDS`] larger ones from
    /// [`LARGE_BYTES`], none in a shorter one.
    pub const fn of(region: u64) -> Self {
        let (records, first_bytes, last_bytes) = if region >= LARGE_BYTES {
            (LARGE_RECORDS, LARGE_FIRST_BYTES, LARGE_LAST_BYTES)
        } else if region >= LOG_BYTES {
            (RECORDS, FIRST_BYTES, LAST_BYTES)
        } else {
            return Self::ring_only(region);
        };
        let area = records as u64 * (first_bytes as u64 + last_bytes as u64);
        Self {
            ring_bytes: region - HEADER_BYTES - area,
            records,
            first_bytes,
            last_bytes,
        }
    }

    /// A region that is all ring, as every version 1 log is.
    pub const fn ring_only(region: u64) -> Self {
        Self {
            ring_bytes: region.saturating_sub(HEADER_BYTES),
            records: 0,
            first_bytes: 0,
            last_bytes: 0,
        }
    }

    const fn record_bytes(&self) -> u64 {
        self.first_bytes as u64 + self.last_bytes as u64
    }

    /// The region this layout fills.
    const fn region(&self) -> u64 {
        HEADER_BYTES + self.ring_bytes + self.records as u64 * self.record_bytes()
    }

    /// Where record `index` starts in the region.
    const fn record_at(&self, index: usize) -> u64 {
        HEADER_BYTES + self.ring_bytes + index as u64 * self.record_bytes()
    }

    /// Whether a `region`-byte region of `block`-byte blocks takes it: a
    /// ring, and every part on a block boundary.
    fn fits(&self, region: u64, block: u64) -> bool {
        self.region() == region
            && self.ring_bytes != 0
            && self.records <= MAX_RECORDS
            && [
                self.ring_bytes,
                u64::from(self.first_bytes),
                u64::from(self.last_bytes),
            ]
            .iter()
            .all(|bytes| bytes.is_multiple_of(block))
            && (self.records == 0 || self.first_bytes != 0)
    }
}

/// Why a region holds no log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// All zeros: no boot has written a log here.
    Blank,
    /// The region does not start with `STELALOG`.
    NotALog,
    Version(u32),
    /// Reserved header bytes are not zero.
    Reserved,
    /// The header names more boot records than a header holds.
    Records(u32),
    Checksum,
    /// The header's ring and boot records are not what this region holds
    /// after its header.
    RingSize {
        recorded: u64,
        region: u64,
    },
    /// The region is too short for the header and a ring.
    Short {
        bytes: u64,
    },
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => f.write_str("the log region is blank"),
            Self::NotALog => f.write_str("the log region holds no STELALOG header"),
            Self::Version(version) => write!(
                f,
                "the log header is version {version}, not {VERSION} or {VERSION_1}"
            ),
            Self::Records(records) => write!(
                f,
                "the log header names {records} boot records, more than {MAX_RECORDS}"
            ),
            Self::Reserved => f.write_str("the log header's reserved bytes are not zero"),
            Self::Checksum => f.write_str("the log header's checksum does not match"),
            Self::RingSize { recorded, region } => write!(
                f,
                "the log header's ring and boot records take {recorded:#x} bytes; the region \
                 holds {region:#x}"
            ),
            Self::Short { bytes } => write!(
                f,
                "the log region holds {bytes:#x} bytes, no more than its {HEADER_BYTES:#x}-byte header"
            ),
        }
    }
}

impl core::error::Error for HeaderError {}

/// Why a file cannot stand in for the machine-state partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileError {
    /// A run of its clusters starts or ends off a block boundary.
    Misaligned { offset: u64, block: u64 },
    /// It holds fewer than the `needed` bytes in whole blocks.
    Short { bytes: u64, needed: u64 },
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Misaligned { offset, block } => write!(
                f,
                "the file has bytes from {offset:#x}, not on a {block}-byte block boundary"
            ),
            Self::Short { bytes, needed } => write!(
                f,
                "the file holds {bytes:#x} bytes in whole blocks; it needs {needed:#x}"
            ),
        }
    }
}

/// Why the device does not hold what a flush stored ([`Writer::verify`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unkept {
    Read(BlockError),
    Header(HeaderError),
    /// The stored header counts other bytes than the flush wrote.
    Head {
        stored: u64,
        written: u64,
    },
}

impl fmt::Display for Unkept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(f, "the header cannot be read back: {error}"),
            Self::Header(error) => {
                write!(f, "the header read back is not the one written: {error}")
            }
            Self::Head { stored, written } => write!(
                f,
                "the header read back counts {stored:#x} bytes; {written:#x} were written"
            ),
        }
    }
}

impl core::error::Error for Unkept {}

/// The region a `bytes`-long log file holds: its whole 4 KiB pages, up to
/// [`MAX_FILE_BYTES`]; `None` for a file shorter than [`LOG_BYTES`]. The
/// firmware's path to the file and Stela's own give the same file the same
/// region, so they agree on its layout.
pub const fn file_region(bytes: u64) -> Option<u64> {
    if bytes < LOG_BYTES {
        return None;
    }
    let bytes = if bytes > MAX_FILE_BYTES {
        MAX_FILE_BYTES
    } else {
        bytes
    };
    Some(bytes - bytes % HEADER_BYTES)
}

/// The region a header names that does not fit a `region`-byte region of
/// `block`-byte blocks, when it is a smaller one at the region's start: a
/// log an older loader started in a longer file.
fn prefix(error: &HeaderError, region: u64, block: u64) -> Option<u64> {
    let HeaderError::RingSize { recorded, .. } = *error else {
        return None;
    };
    let prefix = recorded.checked_add(HEADER_BYTES)?;
    (prefix > HEADER_BYTES && prefix < region && prefix.is_multiple_of(block.max(1)))
        .then_some(prefix)
}

/// The runs of `file`'s blocks that hold its first `needed` bytes, each a
/// first block and a count, in order: `file` is on a partition of
/// `block`-byte blocks and holds [`PARTITION_BYTES`] for a machine-state
/// file, [`LOG_BYTES`] for a log file.
pub fn file_runs(
    file: &fat32::File,
    block: u64,
    needed: u64,
) -> Result<Vec<(u64, u64)>, FileError> {
    let (mut runs, mut bytes) = (Vec::<(u64, u64)>::new(), 0);
    // Where the last run ended inside a block, which no next run can follow.
    let mut partial = None;
    for extent in &file.extents {
        if bytes >= needed {
            break;
        }
        if let Some(offset) =
            partial.or((!extent.offset.is_multiple_of(block)).then_some(extent.offset))
        {
            return Err(FileError::Misaligned { offset, block });
        }
        let count = (extent.bytes / block).min((needed - bytes).div_ceil(block));
        if !extent.bytes.is_multiple_of(block) {
            partial = Some(extent.offset + extent.bytes);
        }
        let first = extent.offset / block;
        match runs.last_mut() {
            Some((start, blocks)) if *start + *blocks == first => *blocks += count,
            _ => runs.push((first, count)),
        }
        bytes += count * block;
    }
    if bytes < needed {
        return Err(FileError::Short { bytes, needed });
    }
    Ok(runs)
}

/// One boot's record: where its lines start in the stream, and how much of
/// its first and last lines the record holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Record {
    /// The boot's number; 0 for a record no boot has used.
    boot: u64,
    start: u64,
    first: u32,
    last: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Header {
    version: u32,
    head: u64,
    boots: u64,
    layout: Layout,
    /// One per record of the layout.
    records: Vec<Record>,
}

impl Header {
    /// A log no boot has written, in `layout`.
    fn fresh(layout: Layout) -> Self {
        Self {
            version: VERSION,
            head: 0,
            boots: 0,
            layout,
            records: vec![Record::default(); layout.records as usize],
        }
    }

    /// `block` with the header at its start and zeros after it.
    fn encode(&self, block: &mut [u8]) {
        block.fill(0);
        block[..8].copy_from_slice(MAGIC);
        block[8..12].copy_from_slice(&VERSION.to_le_bytes());
        block[16..24].copy_from_slice(&self.head.to_le_bytes());
        block[24..32].copy_from_slice(&self.boots.to_le_bytes());
        block[32..40].copy_from_slice(&self.layout.ring_bytes.to_le_bytes());
        block[40..44].copy_from_slice(&self.layout.records.to_le_bytes());
        block[44..48].copy_from_slice(&self.layout.first_bytes.to_le_bytes());
        block[48..52].copy_from_slice(&self.layout.last_bytes.to_le_bytes());
        let mut at = FIELDS;
        for record in &self.records {
            block[at..at + 8].copy_from_slice(&record.boot.to_le_bytes());
            block[at + 8..at + 16].copy_from_slice(&record.start.to_le_bytes());
            block[at + 16..at + 20].copy_from_slice(&record.first.to_le_bytes());
            block[at + 20..at + 24].copy_from_slice(&record.last.to_le_bytes());
            at += RECORD_FIELDS;
        }
        let digest = Sha256::digest(&block[..at]);
        block[at..at + 32].copy_from_slice(&digest);
    }

    /// The header's 4 KiB `page`: the header at its start, zeros, and the
    /// header's copy in its last 512 bytes ([`COPY_AT`]).
    fn encode_page(&self, page: &mut [u8]) {
        self.encode(page);
        let (header, copy) = page.split_at_mut(COPY_AT as usize);
        copy.copy_from_slice(&header[..copy.len()]);
    }

    /// The header the 4 KiB `page` names where its own does not decode
    /// (`error`) for a region of `region` bytes: its copy ([`COPY_AT`]),
    /// when that decodes. A header of a version this software does not
    /// write is a newer one's, whose copy it does not know.
    fn copy(page: &[u8], region: u64, error: &HeaderError) -> Option<Self> {
        if matches!(error, HeaderError::Version(_)) {
            return None;
        }
        let copy = page.get(COPY_AT as usize..HEADER_BYTES as usize)?;
        Self::decode(copy, region)
            .ok()
            .filter(|header| header.version == VERSION)
    }

    /// The header at the start of `bytes`, of either version, for a region
    /// of `region` bytes.
    fn decode(bytes: &[u8], region: u64) -> Result<Self, HeaderError> {
        if bytes.iter().all(|&byte| byte == 0) {
            return Err(HeaderError::Blank);
        }
        if bytes.len() < FIELDS_1 + 32 || &bytes[..8] != MAGIC {
            return Err(HeaderError::NotALog);
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let version = u32_at(8);
        let (layout, fields) = match version {
            VERSION_1 => (
                Layout {
                    ring_bytes: u64_at(32),
                    records: 0,
                    first_bytes: 0,
                    last_bytes: 0,
                },
                FIELDS_1,
            ),
            VERSION if bytes.len() >= FIELDS + 32 => {
                let records = u32_at(40);
                if records > MAX_RECORDS {
                    return Err(HeaderError::Records(records));
                }
                (
                    Layout {
                        ring_bytes: u64_at(32),
                        records,
                        first_bytes: u32_at(44),
                        last_bytes: u32_at(48),
                    },
                    FIELDS + records as usize * RECORD_FIELDS,
                )
            }
            VERSION => return Err(HeaderError::NotALog),
            version => return Err(HeaderError::Version(version)),
        };
        if bytes.len() < fields + 32
            || Sha256::digest(&bytes[..fields])[..] != bytes[fields..fields + 32]
        {
            return Err(HeaderError::Checksum);
        }
        if bytes[12..16] != [0; 4] || (version == VERSION && bytes[52..56] != [0; 4]) {
            return Err(HeaderError::Reserved);
        }
        // Checked sizes: a header with a valid checksum and sizes that
        // overflow is not this region's either.
        let taken = (layout.records as u64)
            .checked_mul(layout.record_bytes())
            .and_then(|area| area.checked_add(layout.ring_bytes));
        if taken != Some(region.saturating_sub(HEADER_BYTES)) || layout.ring_bytes == 0 {
            return Err(HeaderError::RingSize {
                recorded: taken.unwrap_or(u64::MAX),
                region: region.saturating_sub(HEADER_BYTES),
            });
        }
        let records = (0..layout.records as usize)
            .map(|index| {
                let at = FIELDS + index * RECORD_FIELDS;
                Record {
                    boot: u64_at(at),
                    start: u64_at(at + 8),
                    first: u32_at(at + 16).min(layout.first_bytes),
                    last: u32_at(at + 20).min(layout.last_bytes),
                }
            })
            .collect();
        Ok(Self {
            version,
            head: u64_at(16),
            boots: u64_at(24),
            layout,
            records,
        })
    }
}

/// The line that stands for bytes an overflow dropped.
struct DroppedLine {
    bytes: [u8; 64],
    len: usize,
}

impl DroppedLine {
    fn new(dropped: u64) -> Self {
        let mut line = Self {
            bytes: [0; 64],
            len: 0,
        };
        let _ = writeln!(line, "[boot log dropped {dropped} bytes]");
        line
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Write for DroppedLine {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        self.bytes
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Bytes waiting for a flush, at most the storage's length. An overflow
/// drops the oldest and counts them; [`Pending::drain`] puts a
/// `[boot log dropped N bytes]` line in their place. Several logs can read
/// the same bytes instead, each from its own position
/// ([`Pending::copy_since`]).
pub struct Pending<S> {
    storage: S,
    start: usize,
    len: usize,
    dropped: u64,
    /// Every byte pushed, held or not.
    pushed: u64,
}

impl<S> Pending<S> {
    pub const fn new(storage: S) -> Self {
        Self {
            storage,
            start: 0,
            len: 0,
            dropped: 0,
            pushed: 0,
        }
    }
}

impl<S: AsRef<[u8]> + AsMut<[u8]>> Pending<S> {
    pub fn push(&mut self, mut bytes: &[u8]) {
        self.pushed += bytes.len() as u64;
        let capacity = self.storage.as_ref().len();
        if capacity == 0 {
            self.dropped += bytes.len() as u64;
            return;
        }
        if bytes.len() >= capacity {
            self.dropped += (self.len + bytes.len() - capacity) as u64;
            self.start = 0;
            self.len = 0;
            bytes = &bytes[bytes.len() - capacity..];
        } else if self.len + bytes.len() > capacity {
            let excess = self.len + bytes.len() - capacity;
            self.start = (self.start + excess) % capacity;
            self.len -= excess;
            self.dropped += excess as u64;
        }
        let at = (self.start + self.len) % capacity;
        let first = bytes.len().min(capacity - at);
        let storage = self.storage.as_mut();
        storage[at..at + first].copy_from_slice(&bytes[..first]);
        storage[..bytes.len() - first].copy_from_slice(&bytes[first..]);
        self.len += bytes.len();
    }

    /// Nothing waits, and nothing was dropped.
    pub fn is_empty(&self) -> bool {
        self.len == 0 && self.dropped == 0
    }

    /// The bytes [`Pending::drain`] would hand over.
    pub fn bytes(&self) -> usize {
        self.len
            + if self.dropped == 0 {
                0
            } else {
                DroppedLine::new(self.dropped).len
            }
    }

    /// The position after every byte pushed so far.
    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    /// Hand `sink` the bytes pushed since position `from` (an earlier
    /// [`Pending::pushed`]) and keep them, after a dropped-bytes line for
    /// those no longer held; the position reached.
    pub fn copy_since<E>(
        &self,
        from: u64,
        mut sink: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<u64, E> {
        let oldest = self.pushed - self.len as u64;
        if from < oldest {
            sink(DroppedLine::new(oldest - from).as_bytes())?;
        }
        let skip = from.saturating_sub(oldest).min(self.len as u64) as usize;
        let storage = self.storage.as_ref();
        let start = (self.start + skip) % storage.len().max(1);
        let len = self.len - skip;
        let first = len.min(storage.len() - start);
        if first != 0 {
            sink(&storage[start..start + first])?;
        }
        if len > first {
            sink(&storage[..len - first])?;
        }
        Ok(self.pushed)
    }

    /// Hand every pending byte to `sink` in order, after the dropped-bytes
    /// line when an overflow dropped any, and empty the buffer.
    pub fn drain<E>(&mut self, mut sink: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let (dropped, start, len) = (self.dropped, self.start, self.len);
        (self.dropped, self.start, self.len) = (0, 0, 0);
        if dropped != 0 {
            sink(DroppedLine::new(dropped).as_bytes())?;
        }
        let storage = self.storage.as_ref();
        let first = len.min(storage.len() - start);
        if first != 0 {
            sink(&storage[start..start + first])?;
        }
        if len > first {
            sink(&storage[..len - first])?;
        }
        Ok(())
    }
}

/// The ring on the device and its block at the head, kept in memory.
struct Ring {
    block: usize,
    ring_bytes: u64,
    head: u64,
    /// The ring block holding stream position `head`: its bytes before
    /// `head % block` are the stream's newest, those after are the oldest
    /// once the ring has wrapped.
    tail: Vec<u8>,
    /// `tail` holds bytes the device does not.
    dirty: bool,
    /// The ring has wrapped and `tail`'s block still holds oldest stream
    /// bytes past the head that `tail` lacks: read before it is written.
    unread: bool,
}

impl Ring {
    /// The device block of the ring block holding stream position `at`.
    fn lba(&self, at: u64) -> u64 {
        HEADER_BYTES / self.block as u64 + (at % self.ring_bytes) / self.block as u64
    }

    /// Copy `bytes` to the ring at the head; each block they fill joins
    /// `writes`, consecutive blocks one write. `remaining` counts this
    /// store's bytes still to come: a block they will overwrite whole is
    /// not read first.
    fn stage(&mut self, mut bytes: &[u8], remaining: &mut usize, writes: &mut Vec<(u64, Vec<u8>)>) {
        while !bytes.is_empty() {
            let offset = (self.head % self.block as u64) as usize;
            let count = (self.block - offset).min(bytes.len());
            self.tail[offset..offset + count].copy_from_slice(&bytes[..count]);
            self.head += count as u64;
            self.dirty = true;
            bytes = &bytes[count..];
            *remaining -= count;
            if self.head.is_multiple_of(self.block as u64) {
                join(writes, self.lba(self.head - 1), &self.tail, self.block);
                self.dirty = false;
                // The next block holds the oldest stream bytes once the ring
                // has wrapped; keep those this store does not overwrite.
                self.unread = self.head >= self.ring_bytes && *remaining < self.block;
                self.tail.fill(0);
            }
        }
    }
}

/// Add block `lba`'s `bytes` to `writes`, joining the last write when it
/// ends just before.
fn join(writes: &mut Vec<(u64, Vec<u8>)>, lba: u64, bytes: &[u8], block: usize) {
    match writes.last_mut() {
        Some((first, data)) if *first + (data.len() / block) as u64 == lba => {
            data.extend_from_slice(bytes);
        }
        _ => writes.push((lba, bytes.to_vec())),
    }
}

/// A step of a store ([`Writer::begin_store`]), each a request the last
/// one's completion lets go: the ring's bytes, the boot records' when one
/// is due, then the header.
enum Step {
    /// Ring blocks from this one; `tail` when the last is the head's.
    Blocks {
        lba: u64,
        data: Vec<u8>,
        tail: bool,
    },
    /// The head's block, for the oldest bytes `tail` lacks.
    ReadTail,
    /// The head's block.
    Tail,
    /// A boot record's lines, in whole blocks.
    Record {
        lba: u64,
        data: Vec<u8>,
    },
    Header,
}

/// This boot's first lines, collected as the ring takes them until the
/// record's room is full.
struct Capture {
    /// The boot's record.
    index: usize,
    bytes: Vec<u8>,
}

/// `bytes` up to the end of its last whole line; all of it without one.
fn whole_lines(bytes: &[u8]) -> &[u8] {
    match bytes.iter().rposition(|&byte| byte == b'\n') {
        Some(end) => &bytes[..=end],
        None => bytes,
    }
}

/// Stream bytes `[from, to)` of the ring on `device`, which holds them:
/// whole blocks around each part, each within the ring.
fn read_stream(
    device: &mut dyn BlockDevice,
    ring_bytes: u64,
    from: u64,
    to: u64,
) -> Result<Vec<u8>, BlockError> {
    let block = u64::from(device.identity().block_size);
    let mut read = Vec::with_capacity((to - from) as usize);
    let mut at = from - from % block;
    while at < to {
        let in_ring = at % ring_bytes;
        let run = (to.next_multiple_of(block) - at).min(ring_bytes - in_ring);
        let mut buffer = vec![0; run as usize];
        device.read((HEADER_BYTES + in_ring) / block, &mut buffer)?;
        read.extend_from_slice(&buffer);
        at += run;
    }
    let skip = (from % block) as usize;
    read.truncate(skip + (to - from) as usize);
    read.drain(..skip);
    Ok(read)
}

/// Write `bytes` at byte `offset` of the region on `device`, keeping the
/// other bytes of the blocks they share.
fn write_region(device: &mut dyn BlockDevice, offset: u64, bytes: &[u8]) -> Result<(), BlockError> {
    if bytes.is_empty() {
        return Ok(());
    }
    let block = u64::from(device.identity().block_size);
    let first = offset - offset % block;
    let end = (offset + bytes.len() as u64).next_multiple_of(block);
    let mut blocks = vec![0; (end - first) as usize];
    device.read(first / block, &mut blocks)?;
    let at = (offset - first) as usize;
    blocks[at..at + bytes.len()].copy_from_slice(bytes);
    device.write(first / block, &blocks)
}

/// The line that marks the bytes [`convert`] removed from the middle of a
/// version 1 stream.
fn removed_line(bytes: u64) -> Vec<u8> {
    format!("\n[boot log: {bytes} older bytes were removed here to keep each boot's first lines]\n")
        .into_bytes()
}

/// Make the version 1 log on `device`, whose ring fills the region, a
/// version 2 log in `layout`, whose ring is shorter by the boot records'
/// area at the region's end; the new head. The new ring starts where the
/// old one did, so only what the area held moves:
///
/// - Nothing, where the stream never reached the area.
/// - The stream's newest bytes, where the head lies inside the area: they
///   move to the ring's start, over its oldest bytes, and the ring then
///   holds the newest `ring_bytes` of the stream in order.
/// - Otherwise the area held bytes from the middle of the wrapped stream:
///   they are dropped, and [`removed_line`] takes the place of the last
///   bytes before them.
///
/// The head keeps counting upward, moved on to the position the new ring
/// gives the same place. The caller writes the header after this, last:
/// until then the region reads as the version 1 log it was, with at most
/// these bytes changed, and a boot interrupted here converts it again from
/// the same bytes.
fn convert(device: &mut dyn BlockDevice, old: &Header, layout: Layout) -> Result<u64, BlockError> {
    let (ring, kept) = (old.layout.ring_bytes, layout.ring_bytes);
    if kept >= ring {
        return Ok(old.head);
    }
    let wrapped = old.head >= ring;
    // Where the old ring's next byte goes: its newest bytes end here.
    let place = if wrapped { old.head % ring } else { old.head };
    if !wrapped && place <= kept {
        return Ok(old.head);
    }
    // The place in the new ring after its newest byte.
    let at = if place > kept {
        let newest = read_stream(device, ring, old.head - (place - kept), old.head)?;
        write_region(device, HEADER_BYTES, &newest)?;
        place - kept
    } else {
        let line = removed_line(ring - kept);
        let room = (kept - place) as usize;
        let mark = match room >= line.len() {
            true => line,
            // Too little of the older part is left to say it: blank lines.
            false => vec![b'\n'; room],
        };
        write_region(device, HEADER_BYTES + kept - mark.len() as u64, &mark)?;
        place % kept
    };
    if device.identity().features.write_cache {
        device.flush_cache()?;
    }
    // A full ring, at the first position not below the old head.
    let base = old.head.max(kept) - old.head.max(kept) % kept;
    Ok(match base + at >= old.head.max(kept) {
        true => base + at,
        false => base + kept + at,
    })
}

/// Lay the smaller log at the start of the `region`-byte region on
/// `device`, whose header names a `prefix`-byte region, out in `layout` in
/// place, as [`relayout`] lays it out in memory: its kept stream at its
/// positions in the new ring, zeros in the ring's window before them, and
/// each kept boot's record; the header that names them, which the caller
/// writes last, and how many boots of the longer log the header's copy
/// still named came before ([`merged`]: the region is read whole then).
/// `None` when the smaller region does not read as a log. An interruption
/// leaves the smaller log's header and its copy, and lines it read in the
/// ring's bytes written over by the new layout's: the next boot lays out
/// what then reads.
fn grow(
    device: &mut dyn BlockDevice,
    region: u64,
    prefix: u64,
    layout: Layout,
) -> Result<Option<(Header, Option<u64>)>, BlockError> {
    let mut page = vec![0; HEADER_BYTES as usize];
    device.read(0, &mut page)?;
    let error = Header::decode(&page, region).err();
    let earlier = error
        .and_then(|error| Header::copy(&page, region, &error))
        .filter(|copy| copy.layout == layout)
        .map(|copy| copy.boots);
    let mut bytes = vec![0; if earlier.is_some() { region } else { prefix } as usize];
    device.read(0, &mut bytes)?;
    let Ok(log) = read(&bytes) else {
        return Ok(None);
    };
    drop(bytes);
    let (pieces, header) = placed(&log, layout);
    let ring = layout.ring_bytes;
    let kept = (log.stream.len() as u64).min(ring);
    let (mut from, to) = (log.head.saturating_sub(ring), log.head - kept);
    while from < to {
        let at = from % ring;
        let run = (to - from).min(ring - at).min(1 << 20);
        write_region(device, HEADER_BYTES + at, &vec![0; run as usize])?;
        from += run;
    }
    for (offset, bytes) in &pieces {
        write_region(device, *offset, bytes)?;
    }
    if device.identity().features.write_cache {
        device.flush_cache()?;
    }
    Ok(Some((header, earlier)))
}

/// The log of one boot: appends wait in memory until a flush stores them.
pub struct Writer {
    ring: Ring,
    boots: u64,
    layout: Layout,
    records: Vec<Record>,
    /// This boot's first lines while its record still takes them.
    capture: Option<Capture>,
    /// Record blocks the next store writes before its header, which names
    /// their lengths.
    record_writes: Vec<(u64, Vec<u8>)>,
    /// Ring blocks a failed store did not write, which the next writes
    /// first: the ring's head moved past them already.
    ring_writes: Vec<(u64, Vec<u8>)>,
    /// The header on the device names this boot's record as this boot's.
    /// Until it does, the record's room may still be named as an older
    /// boot's, or hold the bytes a version 1 log's conversion moves, and
    /// this boot's first lines wait.
    claimed: bool,
    /// The head the header on the device records.
    durable: Option<u64>,
    fresh: Option<HeaderError>,
    converted: bool,
    /// The smaller region of the log this open laid out in the whole one.
    grown: Option<u64>,
    /// The boots of the longer log the header's copy named, which this open
    /// kept before the smaller log's ([`merged`]).
    earlier: Option<u64>,
    /// The header did not decode and this open took its copy's.
    restored: bool,
    pending: Pending<Vec<u8>>,
    /// The store in progress: its steps to come and the one running.
    steps: VecDeque<Step>,
    running: Option<Step>,
}

impl Writer {
    /// Open the log `device` holds whole (a [`LOG_BYTES`] window of the
    /// machine-state partition), count this boot and append its marker
    /// line. A blank or invalid header starts a fresh log ([`Writer::fresh`]);
    /// a version 1 log is converted ([`Writer::converted`]).
    pub fn open(device: &mut dyn BlockDevice) -> Result<Self, BlockError> {
        Self::open_noted(device, "")
    }

    /// [`Self::open`], with `note` after the marker on its line: what the
    /// boot is and when it started (`=== STELA BOOT 12 === version=0.5.0
    /// firmware_clock=2026-11-18 01:44:12`), so each boot in the log says
    /// so where it begins, whatever the ring kept of its first lines.
    pub fn open_noted(device: &mut dyn BlockDevice, note: &str) -> Result<Self, BlockError> {
        Self::with_pending(device, PENDING_BYTES, Some(note))
    }

    /// Open the log again later in the same boot, as the system engine does
    /// after the loader wrote it: the boot is not counted again and gets no
    /// second marker line.
    pub fn resume(device: &mut dyn BlockDevice) -> Result<Self, BlockError> {
        Self::with_pending(device, PENDING_BYTES, None)
    }

    /// A new boot with the note after its marker, or the same boot again.
    fn with_pending(
        device: &mut dyn BlockDevice,
        pending: usize,
        new_boot: Option<&str>,
    ) -> Result<Self, BlockError> {
        let identity = device.identity();
        let region = identity
            .blocks
            .saturating_mul(u64::from(identity.block_size));
        Self::with_layout(device, pending, new_boot, Layout::of(region))
    }

    /// [`Self::open`] or [`Self::resume`], giving a fresh or version 1 log
    /// `layout` where the region takes it and no records where it does not.
    fn with_layout(
        device: &mut dyn BlockDevice,
        pending: usize,
        noted: Option<&str>,
        layout: Layout,
    ) -> Result<Self, BlockError> {
        let new_boot = noted.is_some();
        let identity = device.identity();
        let block = identity.block_size as usize;
        if block == 0 || !(HEADER_BYTES as usize).is_multiple_of(block) {
            return Err(BlockError::Unsupported(
                "the boot log needs blocks that divide its 4 KiB header",
            ));
        }
        if identity.read_only {
            return Err(BlockError::ReadOnly);
        }
        let bytes = identity.blocks.saturating_mul(block as u64);
        if bytes <= HEADER_BYTES {
            return Err(BlockError::Unsupported("the boot log region holds no ring"));
        }
        let layout = match layout.fits(bytes, block as u64) {
            true => layout,
            false => Layout::ring_only(bytes),
        };
        let mut page = vec![0; HEADER_BYTES as usize];
        device.read(0, &mut page)?;
        let (mut grown, mut earlier, mut restored) = (None, None, false);
        let (header, fresh, converted) = match Header::decode(&page, bytes) {
            Ok(header) if header.version == VERSION => (header, None, false),
            Ok(old) => {
                let head = convert(device, &old, layout)?;
                let header = Header {
                    head,
                    boots: old.boots,
                    ..Header::fresh(layout)
                };
                (header, None, true)
            }
            Err(error) => {
                let laid_out = match prefix(&error, bytes, block as u64) {
                    Some(smaller) => grow(device, bytes, smaller, layout)?.map(|(header, kept)| {
                        (grown, earlier) = (Some(smaller), kept);
                        header
                    }),
                    None => Header::copy(&page, bytes, &error).inspect(|_| restored = true),
                };
                match laid_out {
                    // Its header is still to be written, as a converted one's.
                    Some(header) => (header, None, true),
                    None => (Header::fresh(layout), Some(error), false),
                }
            }
        };
        let layout = header.layout;
        let ring_bytes = layout.ring_bytes;
        let mut ring = Ring {
            block,
            ring_bytes,
            head: header.head,
            tail: vec![0; block],
            dirty: false,
            unread: false,
        };
        let head = header.head;
        let offset = (head % block as u64) as usize;
        if offset != 0 || head >= ring_bytes {
            device.read(ring.lba(head), &mut ring.tail)?;
        }
        // A boot that stopped mid-line gets its line ended first.
        let last = match (head, offset) {
            (0, _) => b'\n',
            (_, 0) => {
                let mut previous = vec![0; block];
                device.read(ring.lba(head - 1), &mut previous)?;
                previous[block - 1]
            }
            _ => ring.tail[offset - 1],
        };
        let mut writer = Self {
            ring,
            boots: header.boots.saturating_add(u64::from(new_boot)),
            layout,
            records: header.records,
            capture: None,
            record_writes: Vec::new(),
            ring_writes: Vec::new(),
            // A resumed boot's record is named by the header it found.
            claimed: !new_boot,
            // A converted log's header is still to be written.
            durable: (fresh.is_none() && !converted).then_some(head),
            fresh,
            converted,
            grown,
            earlier,
            restored,
            pending: Pending::new(vec![0; pending]),
            steps: VecDeque::new(),
            running: None,
        };
        if last != b'\n' {
            writer.append(b"\n");
        }
        if let Some(note) = noted {
            writer.begin_record(device)?;
            let marker = match note.trim() {
                "" => format!("=== STELA BOOT {} ===\n", writer.boots),
                note => format!("=== STELA BOOT {} === {note}\n", writer.boots),
            };
            writer.append(marker.as_bytes());
        } else {
            writer.resume_record(device)?;
        }
        Ok(writer)
    }

    /// The stream bytes the ring on the device still holds from.
    fn oldest(&self) -> u64 {
        self.ring.head.saturating_sub(self.ring.ring_bytes)
    }

    /// Queue `bytes` for record `index`, `after` bytes into it, in whole
    /// blocks: the rest of the last one is zero and beyond the lengths the
    /// header names.
    fn queue_record(&mut self, index: usize, after: u64, bytes: &[u8]) {
        let block = self.ring.block;
        let mut data = bytes.to_vec();
        data.resize(bytes.len().next_multiple_of(block), 0);
        self.record_writes
            .push(((self.layout.record_at(index) + after) / block as u64, data));
    }

    /// A new boot opens the log: the last boot's record takes from the
    /// ring what it lacks, its first lines where it ended before they were
    /// complete and its last lines, and this boot gets the record after it.
    fn begin_record(&mut self, device: &mut dyn BlockDevice) -> Result<(), BlockError> {
        let records = self.records.len();
        if records == 0 {
            return Ok(());
        }
        let index = (self.boots % records as u64) as usize;
        let (head, oldest) = (self.ring.head, self.oldest());
        // The boot before this one's: an older loader's boots after it
        // have none, and the newest lines are theirs ([`merged`]).
        let last = (0..records).find(|&other| {
            other != index
                && self.records[other].boot != 0
                && self.records[other].boot + 1 == self.boots
        });
        if let Some(last) = last {
            let mut record = self.records[last];
            if record.first == 0 && (oldest..head).contains(&record.start) {
                let end = head.min(record.start + u64::from(self.layout.first_bytes));
                let read = read_stream(device, self.ring.ring_bytes, record.start, end)?;
                let first = match end < head {
                    true => whole_lines(&read),
                    false => &read,
                };
                record.first = first.len() as u32;
                self.queue_record(last, 0, first);
            }
            let after_first = record.start.saturating_add(u64::from(record.first));
            let from = after_first
                .max(head.saturating_sub(u64::from(self.layout.last_bytes)))
                .max(oldest);
            if record.last == 0 && from < head && record.start <= head {
                let read = read_stream(device, self.ring.ring_bytes, from, head)?;
                // From the start of a line, unless it follows the first
                // lines at once.
                let skip = match from == after_first {
                    true => 0,
                    false => read
                        .iter()
                        .position(|&byte| byte == b'\n')
                        .map_or(0, |end| end + 1),
                };
                record.last = (read.len() - skip) as u32;
                self.queue_record(last, u64::from(self.layout.first_bytes), &read[skip..]);
            }
            self.records[last] = record;
        }
        self.records[index] = Record {
            boot: self.boots,
            start: self.head(),
            first: 0,
            last: 0,
        };
        self.capture = Some(Capture {
            index,
            bytes: Vec::with_capacity(self.layout.first_bytes as usize),
        });
        Ok(())
    }

    /// The same boot opens the log again: its record goes on taking its
    /// first lines after those the ring already holds.
    fn resume_record(&mut self, device: &mut dyn BlockDevice) -> Result<(), BlockError> {
        let head = self.ring.head;
        let Some(index) = self
            .records
            .iter()
            .position(|record| record.boot == self.boots && record.boot != 0 && record.first == 0)
        else {
            return Ok(());
        };
        let start = self.records[index].start;
        if !(self.oldest()..=head).contains(&start) {
            return Ok(());
        }
        let end = head.min(start + u64::from(self.layout.first_bytes));
        let bytes = read_stream(device, self.ring.ring_bytes, start, end)?;
        self.capture = Some(Capture { index, bytes });
        self.close_full_record();
        Ok(())
    }

    /// Once this boot's first lines fill its record's room and the header
    /// on the device names the record as this boot's, queue them, cut at a
    /// line's end, and stop collecting.
    fn close_full_record(&mut self) {
        if !self.claimed {
            return;
        }
        let room = self.layout.first_bytes as usize;
        let Some(capture) = self.capture.take_if(|capture| capture.bytes.len() >= room) else {
            return;
        };
        let first = whole_lines(&capture.bytes[..room]);
        self.records[capture.index].first = first.len() as u32;
        self.queue_record(capture.index, 0, first);
    }

    /// Read the header back and check that it records the last flush: a
    /// device that accepts writes and keeps none fails here.
    pub fn verify(&self, device: &mut dyn BlockDevice) -> Result<(), Unkept> {
        let mut block = vec![0; self.ring.block];
        device.read(0, &mut block).map_err(Unkept::Read)?;
        let stored = Header::decode(&block, self.layout.region()).map_err(Unkept::Header)?;
        let written = self.durable.unwrap_or_default();
        if stored.head != written || stored.boots != self.boots {
            return Err(Unkept::Head {
                stored: stored.head,
                written,
            });
        }
        Ok(())
    }

    /// Why the log started fresh this boot, if it did.
    pub fn fresh(&self) -> Option<&HeaderError> {
        self.fresh.as_ref()
    }

    /// Whether this open converted a version 1 log ([`convert`]).
    pub fn converted(&self) -> bool {
        self.converted && self.grown.is_none() && !self.restored
    }

    /// The boots of the longer log an older loader's log followed, which
    /// this open kept before that log's ([`merged`]), if it did.
    pub fn kept_earlier(&self) -> Option<u64> {
        self.earlier
    }

    /// Whether the header did not decode and this open read its copy.
    pub fn restored(&self) -> bool {
        self.restored
    }

    /// The smaller region of the log an older loader started at the
    /// region's start, which this open laid out in the whole region
    /// ([`grow`]), if it did.
    pub fn grown(&self) -> Option<u64> {
        self.grown
    }

    /// How the region is divided.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// This boot's number.
    pub fn boots(&self) -> u64 {
        self.boots
    }

    /// Every byte appended so far, including those still pending.
    pub fn head(&self) -> u64 {
        self.ring.head + self.pending.bytes() as u64
    }

    pub fn ring_bytes(&self) -> u64 {
        self.ring.ring_bytes
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.pending.push(bytes);
    }

    /// Store the pending bytes, the caller waiting: [`Self::begin_store`]
    /// with each request run to completion.
    pub fn flush(&mut self, device: &mut dyn BlockDevice) -> Result<(), BlockError> {
        if !self.begin_store() {
            return Ok(());
        }
        while let Some(request) = self.next_request() {
            self.completed(request.run(device))?;
        }
        Ok(())
    }

    /// Whether a store has requests left ([`Self::next_request`]).
    pub fn storing(&self) -> bool {
        self.running.is_some() || !self.steps.is_empty()
    }

    /// Begin storing the pending bytes; whether there are any. The store
    /// is the ring's blocks, each durable once written, a boot record's
    /// when one is due, then the header, durable too: a header never names
    /// bytes the device lacks, and no cache is flushed whole. Bytes
    /// appended meanwhile wait for the next.
    pub fn begin_store(&mut self) -> bool {
        if self.storing() {
            return true;
        }
        self.close_full_record();
        let mut remaining = self.pending.bytes();
        if remaining == 0
            && self.durable == Some(self.ring.head)
            && self.record_writes.is_empty()
            && self.ring_writes.is_empty()
        {
            return false;
        }
        // A failed store's ring blocks first: this store's own write the
        // same blocks again where they share them, with newer bytes.
        for (lba, data) in self.ring_writes.drain(..) {
            self.steps.push_back(Step::Blocks {
                lba,
                data,
                tail: false,
            });
        }
        let mut writes = Vec::new();
        let ring = &mut self.ring;
        let room = self.layout.first_bytes as usize;
        let capture = &mut self.capture;
        let Ok(()) = self.pending.drain(|bytes| {
            // The boot's first lines, as the ring takes them.
            if let Some(capture) = capture
                .as_mut()
                .filter(|capture| capture.bytes.len() < room)
            {
                let take = bytes.len().min(room - capture.bytes.len());
                capture.bytes.extend_from_slice(&bytes[..take]);
            }
            ring.stage(bytes, &mut remaining, &mut writes);
            Ok::<_, Infallible>(())
        });
        // The head's partial block joins the ring's writes unless its old
        // bytes must be read first.
        let mut tail = false;
        if ring.dirty && !ring.unread {
            join(&mut writes, ring.lba(ring.head), &ring.tail, ring.block);
            tail = true;
        }
        let count = writes.len();
        for (index, (lba, data)) in writes.into_iter().enumerate() {
            let tail = tail && index + 1 == count;
            self.steps.push_back(Step::Blocks { lba, data, tail });
        }
        if ring.unread {
            self.steps.push_back(Step::ReadTail);
        }
        if ring.dirty && !tail {
            self.steps.push_back(Step::Tail);
        }
        self.close_full_record();
        for (lba, data) in self.record_writes.drain(..) {
            self.steps.push_back(Step::Record { lba, data });
        }
        self.steps.push_back(Step::Header);
        true
    }

    /// The store's next request once the last one completed
    /// ([`Self::completed`]); `None` when it has no more.
    pub fn next_request(&mut self) -> Option<Request> {
        if self.running.is_some() {
            return None;
        }
        let ring = &self.ring;
        let step = self.steps.pop_front()?;
        let (request, step) = match step {
            // The step keeps its data: a failed store queues it again.
            Step::Blocks { lba, data, tail } => (
                Request::Write {
                    lba,
                    data: data.clone(),
                    durable: true,
                },
                Step::Blocks { lba, data, tail },
            ),
            Step::ReadTail => (
                Request::Read {
                    lba: ring.lba(ring.head),
                    bytes: ring.block,
                },
                Step::ReadTail,
            ),
            Step::Tail => (
                Request::Write {
                    lba: ring.lba(ring.head),
                    data: ring.tail.clone(),
                    durable: true,
                },
                Step::Tail,
            ),
            // The step keeps its data: a failed store queues it again.
            Step::Record { lba, data } => (
                Request::Write {
                    lba,
                    data: data.clone(),
                    durable: true,
                },
                Step::Record { lba, data },
            ),
            Step::Header => {
                let mut page = vec![0; HEADER_BYTES as usize];
                Header {
                    version: VERSION,
                    head: ring.head,
                    boots: self.boots,
                    layout: self.layout,
                    records: self.records.clone(),
                }
                .encode_page(&mut page);
                let request = Request::Write {
                    lba: 0,
                    data: page,
                    durable: true,
                };
                (request, Step::Header)
            }
        };
        self.running = Some(step);
        Some(request)
    }

    /// The running request finished with `finished`. A failure ends the
    /// store: the next writes the ring blocks and boot records this one did
    /// not, before the header that names them; the head's own block and
    /// the header are written from what they then hold.
    pub fn completed(&mut self, finished: Finished) -> Result<(), BlockError> {
        let step = self.running.take();
        let data = match finished {
            Ok(data) => data,
            Err(error) => {
                let (mut ring, mut records) = (Vec::new(), Vec::new());
                for step in step.into_iter().chain(self.steps.drain(..)) {
                    match step {
                        Step::Blocks { lba, data, .. } => ring.push((lba, data)),
                        Step::Record { lba, data } => records.push((lba, data)),
                        Step::ReadTail | Step::Tail | Step::Header => {}
                    }
                }
                self.ring_writes.splice(0..0, ring);
                self.record_writes.splice(0..0, records);
                return Err(error);
            }
        };
        let ring = &mut self.ring;
        match step {
            Some(Step::Blocks { tail: true, .. }) | Some(Step::Tail) => ring.dirty = false,
            Some(Step::ReadTail) => {
                // The bytes before the head are this store's; keep the old
                // ones after it.
                let old = data.ok_or(BlockError::Protocol("a log read returned no data"))?;
                let offset = (ring.head % ring.block as u64) as usize;
                ring.tail[offset..].copy_from_slice(&old[offset..]);
                ring.unread = false;
            }
            Some(Step::Header) => {
                self.durable = Some(ring.head);
                self.claimed = true;
            }
            Some(Step::Blocks { .. }) | Some(Step::Record { .. }) | None => {}
        }
        Ok(())
    }
}

/// What a boot's record keeps of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootStart {
    /// The boot's number.
    pub boot: u64,
    /// The stream position of its marker line.
    pub start: u64,
    /// Its first lines, from the marker; empty while the boot that wrote
    /// them is the last and ended before they were complete, when the
    /// stream still holds them.
    pub first: Vec<u8>,
    /// Its last lines, which follow the first at once or after others;
    /// empty for the last boot.
    pub last: Vec<u8>,
}

/// A region's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    pub boots: u64,
    pub head: u64,
    pub ring_bytes: u64,
    /// The valid stream, oldest byte first: the ring's, from its first
    /// byte a boot wrote.
    pub stream: Vec<u8>,
    /// The boots with a record, oldest first; none in a version 1 log.
    pub starts: Vec<BootStart>,
}

impl Log {
    /// The stream bytes the ring no longer holds: those it has overwritten,
    /// and those a longer file's ring never got ([`relayout`]).
    pub fn overwritten(&self) -> u64 {
        self.head.saturating_sub(self.stream.len() as u64)
    }

    /// The records of the boots whose first lines the stream no longer
    /// holds, oldest first, each with whether the stream no longer holds
    /// its last lines either: the stream then starts in a later boot.
    pub fn kept_starts(&self) -> impl Iterator<Item = (&BootStart, bool)> {
        let overwritten = self.overwritten();
        self.starts
            .iter()
            .enumerate()
            .filter_map(move |(index, start)| {
                let ended = self
                    .starts
                    .get(index + 1)
                    .is_some_and(|next| next.start <= overwritten);
                (start.start < overwritten && !start.first.is_empty()).then_some((start, ended))
            })
    }
}

/// The log a whole region holds, of either version: its stream in order
/// and its boot records. A blank or invalid header is refused by name. A
/// header that does not decode is read from its copy ([`COPY_AT`]); a
/// smaller log an older loader started at the region's start is read with
/// the log the copy names, where the copy has it ([`merged`]), else alone.
pub fn read(region: &[u8]) -> Result<Log, HeaderError> {
    let bytes = region.len() as u64;
    if bytes <= HEADER_BYTES {
        return Err(HeaderError::Short { bytes });
    }
    // A log file's region: its whole pages up to the longest a boot writes.
    let bytes = file_region(bytes).unwrap_or(bytes);
    let region = &region[..bytes as usize];
    let page = &region[..HEADER_BYTES as usize];
    match Header::decode(page, bytes) {
        Ok(header) => Ok(read_with(region, &header)),
        Err(error) => match (prefix(&error, bytes, 1), Header::copy(page, bytes, &error)) {
            (Some(smaller), Some(copy)) => Ok(merged(region, &copy, smaller)),
            // A smaller log at the region's start: an older loader's.
            (Some(smaller), None) => read(&region[..smaller as usize]),
            (None, Some(copy)) => Ok(read_with(region, &copy)),
            (None, None) => Err(error),
        },
    }
}

/// The log `header` names in `region`.
fn read_with(region: &[u8], header: &Header) -> Log {
    let (head, layout) = (header.head, header.layout);
    let ring_bytes = layout.ring_bytes;
    let ring = &region[HEADER_BYTES as usize..(HEADER_BYTES + ring_bytes) as usize];
    let start = head.saturating_sub(ring_bytes);
    let len = (head - start) as usize;
    let at = (start % ring_bytes) as usize;
    let first = len.min(ring.len() - at);
    let mut stream = Vec::with_capacity(len);
    stream.extend_from_slice(&ring[at..at + first]);
    stream.extend_from_slice(&ring[..len - first]);
    // Bytes no boot wrote are zero, and lines are text: a ring that a
    // longer file gave more room than its stream ([`relayout`]) starts
    // with them.
    let unwritten = stream.iter().take_while(|&&byte| byte == 0).count();
    stream.drain(..unwritten);
    Log {
        boots: header.boots,
        head,
        ring_bytes,
        stream,
        starts: records(region, header),
    }
}

/// The boot records `header` names in `region` that hold their boot's
/// lines, oldest boot first.
fn records(region: &[u8], header: &Header) -> Vec<BootStart> {
    let layout = header.layout;
    let mut starts: Vec<BootStart> = header
        .records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.boot != 0 && record.boot <= header.boots)
        .filter_map(|(index, record)| {
            let at = layout.record_at(index) as usize;
            let last = at + layout.first_bytes as usize;
            let first = &region[at..at + record.first as usize];
            // First lines start with their boot's marker, and its note:
            // other bytes are a record a write did not finish, never
            // another boot's lines.
            let marked = marker_of(first) == Some(record.boot);
            (first.is_empty() || marked).then(|| BootStart {
                boot: record.boot,
                start: record.start,
                first: first.to_vec(),
                last: region[last..last + record.last as usize].to_vec(),
            })
        })
        .collect();
    starts.sort_by_key(|start| start.boot);
    starts
}

/// The boot whose marker `line` starts with: `=== STELA BOOT n ===` and the
/// line's end or its note.
fn marker_of(line: &[u8]) -> Option<u64> {
    let rest = line.strip_prefix(b"=== STELA BOOT ")?;
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    let boot = core::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
    let after = rest[digits..].strip_prefix(b" ===")?;
    matches!(after.first(), Some(b'\n' | b' ')).then_some(boot)
}

/// The line that stands for `bytes` of a log that are gone, and why.
fn missing_line(bytes: u64, why: &str) -> Vec<u8> {
    format!("[boot log: {bytes} bytes are missing here: {why}]\n").into_bytes()
}

/// Why the newer loader's lines past an older loader's log are gone.
const WRITTEN_OVER: &str = "an older Stela wrote its own log over them";

/// The log a loader older than the longer file started at the start of
/// `region`, `smaller` bytes long, read after the log it found there, which
/// `copy` (the header's copy, [`COPY_AT`], which that loader leaves alone)
/// still names: one log of every boot both kept. The older loader wrote its
/// header block and the part of its ring its stream reached, nothing past
/// it: the boot records of the longer log lie beyond, and so does the rest
/// of its ring, whose lines stay where they were with a line in place of
/// those written over. The older loader's boots follow, numbered on from
/// the longer log's last, so the boot that opens the log next counts on
/// from both. A smaller log with boot records of its own, which no
/// released loader writes, is read alone, as is one whose header does not
/// decode.
fn merged(region: &[u8], copy: &Header, smaller: u64) -> Log {
    let page = &region[..HEADER_BYTES as usize];
    let small = match Header::decode(page, smaller) {
        Ok(small) if small.layout.records == 0 => small,
        _ => {
            return read(&region[..smaller as usize]).unwrap_or_else(|_| read_with(region, copy));
        }
    };
    let older = read_with(&region[..smaller as usize], &small);
    let layout = copy.layout;
    let ring = layout.ring_bytes;
    // The ring positions the older log's stream reached, in whole pages.
    let lost = small
        .head
        .min(small.layout.ring_bytes)
        .next_multiple_of(HEADER_BYTES)
        .min(smaller - HEADER_BYTES);
    // What the longer ring keeps: its window without the lost positions.
    let oldest = copy.head.saturating_sub(ring);
    let mut parts: Vec<(u64, &[u8])> = Vec::new();
    for turn in oldest / ring..copy.head.div_ceil(ring) {
        let from = oldest.max(turn * ring + lost);
        let to = copy.head.min((turn + 1) * ring);
        if from < to {
            let at = (HEADER_BYTES + from % ring) as usize;
            parts.push((from, &region[at..at + (to - from) as usize]));
        }
    }
    // Bytes no boot wrote, before the first.
    if let Some((from, bytes)) = parts.first_mut() {
        let unwritten = bytes.iter().take_while(|&&byte| byte == 0).count();
        *from += unwritten as u64;
        *bytes = &bytes[unwritten..];
    }
    parts.retain(|(_, bytes)| !bytes.is_empty());
    // The stream starts where the first part does: a position where its
    // bytes are, as overwritten ones stay where they were.
    let base = parts
        .first()
        .map_or(oldest.max(copy.head.min(lost)), |(from, _)| *from);
    let mut stream = Vec::new();
    // Where each kept part, and the line in each gap's place, went: its
    // positions in the longer log, and its first in the new one.
    let mut moved: Vec<(u64, u64, u64, bool)> = Vec::new();
    let mut at = base;
    let say_gap = |stream: &mut Vec<u8>, moved: &mut Vec<_>, from: u64, to: u64| {
        if stream.last().is_some_and(|&byte| byte != b'\n') {
            stream.push(b'\n');
        }
        moved.push((from, to, base + stream.len() as u64, false));
        stream.extend_from_slice(&missing_line(to - from, WRITTEN_OVER));
    };
    for (from, bytes) in &parts {
        if *from > at {
            say_gap(&mut stream, &mut moved, at, *from);
        }
        let to = *from + bytes.len() as u64;
        moved.push((*from, to, base + stream.len() as u64, true));
        stream.extend_from_slice(bytes);
        at = to;
    }
    if at < copy.head {
        say_gap(&mut stream, &mut moved, at, copy.head);
    }
    let newer_end = stream.len();
    // A position of the longer log's stream in the new one: those before
    // it stay, as overwritten ones; one written over is its gap's line's.
    let place = |position: u64| -> u64 {
        match moved
            .iter()
            .find(|(from, to, ..)| (*from..*to).contains(&position))
        {
            Some((from, _, placed, true)) => placed + (position - from),
            Some((_, _, placed, false)) => *placed,
            None if position < base => position,
            None => base + newer_end as u64,
        }
    };
    // The longer log's records lie past the smaller log, or are not its.
    let mut starts: Vec<BootStart> = match layout.record_at(0) >= smaller {
        true => records(region, copy),
        false => Vec::new(),
    };
    for start in &mut starts {
        start.start = place(start.start);
    }
    // Its last boot's lines end where the older loader's began: they are
    // kept in its record now, as a later boot would take the newest lines
    // of the stream, the older loader's, for them.
    if let Some(last) = starts.iter_mut().find(|start| start.boot == copy.boots) {
        let begin = (last.start.saturating_sub(base) as usize).min(newer_end);
        let own = &stream[begin..newer_end];
        if last.first.is_empty() && marker_of(own) == Some(last.boot) {
            let room = own.len().min(layout.first_bytes as usize);
            last.first = match room < own.len() {
                true => whole_lines(&own[..room]).to_vec(),
                false => own.to_vec(),
            };
        }
        if last.last.is_empty() {
            let after = (begin + last.first.len()).min(newer_end);
            let tail = &stream[after..newer_end];
            let mut kept = &tail[tail.len() - tail.len().min(layout.last_bytes as usize)..];
            if kept.len() < tail.len()
                && let Some(end) = kept.iter().position(|&byte| byte == b'\n')
            {
                kept = &kept[end + 1..];
            }
            last.last = kept.to_vec();
        }
    }
    // The older loader's boots, numbered on, from a line's start.
    if stream.last().is_some_and(|&byte| byte != b'\n') {
        stream.push(b'\n');
    }
    if older.overwritten() != 0 {
        stream.extend_from_slice(&missing_line(
            older.overwritten(),
            "the older Stela's log overwrote them with its own later lines",
        ));
    }
    let mut line_start = true;
    for line in older.stream.split_inclusive(|&byte| byte == b'\n') {
        match (line_start, marker_of(line)) {
            (true, Some(boot)) if (1..=small.boots).contains(&boot) => {
                let digits = format!("{boot}").len();
                stream
                    .extend_from_slice(format!("=== STELA BOOT {}", boot + copy.boots).as_bytes());
                stream.extend_from_slice(&line[15 + digits..]);
            }
            _ => stream.extend_from_slice(line),
        }
        line_start = line.ends_with(b"\n");
    }
    Log {
        boots: copy.boots + small.boots,
        head: base + stream.len() as u64,
        ring_bytes: ring,
        stream,
        starts,
    }
}

/// The log `old` holds (of either version), laid out in a `bytes`-long
/// region for a log file made longer: every stream byte at its position,
/// the boot count, and each kept boot's record, in the record its boot
/// number gives in the new layout, its lines cut at a line's end where a
/// record is smaller. A stream longer than the new ring keeps its newest
/// bytes. A blank or invalid `old` is refused by name.
pub fn relayout(old: &[u8], bytes: u64) -> Result<Vec<u8>, HeaderError> {
    let log = read(old)?;
    let layout = Layout::of(bytes);
    if bytes <= HEADER_BYTES || !layout.fits(bytes, HEADER_BYTES) {
        return Err(HeaderError::Short { bytes });
    }
    let mut region = vec![0; bytes as usize];
    let (pieces, header) = placed(&log, layout);
    for (offset, bytes) in pieces {
        region[offset as usize..offset as usize + bytes.len()].copy_from_slice(&bytes);
    }
    header.encode_page(&mut region[..HEADER_BYTES as usize]);
    Ok(region)
}

/// Where `log` goes in `layout`: its stream's newest bytes at their
/// positions in the ring and each kept boot's record, in the record its boot
/// number gives, its lines cut at a line's end where a record is smaller,
/// each as a byte offset in the region and its bytes; and the header that
/// names them.
fn placed(log: &Log, layout: Layout) -> (Vec<(u64, Vec<u8>)>, Header) {
    let mut pieces = Vec::new();
    let ring = layout.ring_bytes;
    let kept = log.stream.len().min(ring as usize);
    let stream = &log.stream[log.stream.len() - kept..];
    let at = (log.head - kept as u64) % ring;
    let first = stream.len().min((ring - at) as usize);
    pieces.push((HEADER_BYTES + at, stream[..first].to_vec()));
    if first < stream.len() {
        pieces.push((HEADER_BYTES, stream[first..].to_vec()));
    }
    let mut records = vec![Record::default(); layout.records as usize];
    for start in &log.starts {
        let index = (start.boot % u64::from(layout.records.max(1))) as usize;
        let Some(record) = records.get_mut(index) else {
            continue;
        };
        let cut = |lines: &[u8], room: u32| -> usize {
            match lines.len() <= room as usize {
                true => lines.len(),
                false => whole_lines(&lines[..room as usize]).len(),
            }
        };
        let first = cut(&start.first, layout.first_bytes);
        // The newest last lines that fit, from a line's start when cut.
        let mut last =
            &start.last[start.last.len() - start.last.len().min(layout.last_bytes as usize)..];
        if last.len() < start.last.len()
            && let Some(end) = last.iter().position(|&byte| byte == b'\n')
        {
            last = &last[end + 1..];
        }
        let at = layout.record_at(index);
        pieces.push((at, start.first[..first].to_vec()));
        pieces.push((at + u64::from(layout.first_bytes), last.to_vec()));
        *record = Record {
            boot: start.boot,
            start: start.start,
            first: first as u32,
            last: last.len() as u32,
        };
    }
    let header = Header {
        version: VERSION,
        head: log.head,
        boots: log.boots,
        layout,
        records,
    };
    (pieces, header)
}

/// The newest bytes of the stream the log on `device` holds, oldest first,
/// read back from the head `chunk` bytes at a time until `enough` says the
/// bytes read answer the question, or the whole stream is read: a boot
/// that needs only its last boots' lines reads a few blocks, not the
/// region. A blank or invalid header is a log with no stream.
pub fn read_newest(
    device: &mut dyn BlockDevice,
    chunk: u64,
    enough: impl Fn(&[u8]) -> bool,
) -> Result<Vec<u8>, BlockError> {
    let identity = device.identity();
    let block = u64::from(identity.block_size);
    let bytes = identity.blocks.saturating_mul(block);
    if block == 0 || !HEADER_BYTES.is_multiple_of(block) || bytes <= HEADER_BYTES {
        return Ok(Vec::new());
    }
    let mut page = vec![0; HEADER_BYTES as usize];
    device.read(0, &mut page)?;
    // A smaller log at the region's start, an older loader's, and a header
    // read from its copy, as `read`.
    let header = match Header::decode(&page, bytes) {
        Ok(header) => header,
        Err(error) => match (
            prefix(&error, bytes, block),
            Header::copy(&page, bytes, &error),
        ) {
            // Both logs, as one: the region whole, once after an older loader.
            (Some(_), Some(_)) => {
                let mut region = vec![0; bytes as usize];
                device.read(0, &mut region)?;
                return Ok(read(&region).map(|log| log.stream).unwrap_or_default());
            }
            (Some(smaller), None) => match Header::decode(&page, smaller) {
                Ok(header) => header,
                Err(_) => return Ok(Vec::new()),
            },
            (None, Some(copy)) => copy,
            (None, None) => return Ok(Vec::new()),
        },
    };
    let (head, ring_bytes) = (header.head, header.layout.ring_bytes);
    let oldest = head.saturating_sub(ring_bytes);
    let mut newest = Vec::new();
    let mut from = head;
    while from > oldest {
        let to = from;
        from = to.saturating_sub(chunk.max(block)).max(oldest);
        let mut read = read_stream(device, ring_bytes, from, to)?;
        read.extend_from_slice(&newest);
        newest = read;
        if enough(&newest) {
            break;
        }
    }
    Ok(newest)
}

#[cfg(test)]
mod tests;
