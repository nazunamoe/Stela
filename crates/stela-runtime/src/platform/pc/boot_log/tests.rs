use super::*;
use alloc::{string::String, vec::Vec};
use stela_drivers::block::{Identity, RamDisk};

/// A RAM disk that records its transfers and can fail from a given
/// write on.
struct Recorder {
    disk: RamDisk,
    /// Each write's block and each flush (`None`), in order.
    events: Vec<Option<u64>>,
    reads: usize,
    writes_left: Option<usize>,
    /// Accept writes and keep none, as a device that loses them.
    dropping: bool,
}

impl Recorder {
    /// A disk with a write cache, so each durable write is followed
    /// by a flush.
    fn new(region: u64, block: u32) -> Self {
        let mut disk = RamDisk::new("log", region / u64::from(block), block);
        disk.identity.features.write_cache = true;
        Self {
            disk,
            events: Vec::new(),
            reads: 0,
            writes_left: None,
            dropping: false,
        }
    }

    fn log(&self) -> Result<Log, HeaderError> {
        read(&self.disk.bytes)
    }
}

impl BlockDevice for Recorder {
    fn identity(&self) -> &Identity {
        self.disk.identity()
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.reads += 1;
        self.disk.read(lba, buffer)
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        if let Some(left) = self.writes_left.as_mut() {
            if *left == 0 {
                return Err(BlockError::Timeout);
            }
            *left -= 1;
        }
        self.events.push(Some(lba));
        if self.dropping {
            return Ok(());
        }
        self.disk.write(lba, data)
    }

    fn flush_cache(&mut self) -> Result<(), BlockError> {
        self.events.push(None);
        self.disk.flush_cache()
    }
}

fn text(log: &Log) -> String {
    String::from_utf8(log.stream.clone()).unwrap()
}

#[test]
fn a_device_that_keeps_no_writes_fails_the_check() {
    let mut disk = Recorder::new(HEADER_BYTES + 4096, 512);
    let mut writer = Writer::open(&mut disk).unwrap();
    writer.flush(&mut disk).unwrap();
    assert_eq!(writer.verify(&mut disk), Ok(()));
    writer.append(b"more\n");
    disk.dropping = true;
    writer.flush(&mut disk).unwrap();
    assert_eq!(
        writer.verify(&mut disk),
        Err(Unkept::Head {
            stored: 0x15,
            written: 0x1a
        })
    );
    let mut blank = Recorder::new(HEADER_BYTES + 4096, 512);
    blank.dropping = true;
    let mut writer = Writer::open(&mut blank).unwrap();
    writer.flush(&mut blank).unwrap();
    assert_eq!(
        writer.verify(&mut blank),
        Err(Unkept::Header(HeaderError::Blank))
    );
}

/// What the device start's store before each device costs: its start
/// line's tail block and the header, each made durable, and no read.
#[test]
fn storing_one_device_start_line_writes_two_blocks() {
    for block in [512, 4096] {
        let mut disk = Recorder::new(HEADER_BYTES + 8 * 4096, block);
        let mut writer = Writer::open(&mut disk).unwrap();
        writer.append(b"earlier lines\n");
        writer.flush(&mut disk).unwrap();
        for device in 0..32 {
            disk.events.clear();
            let reads = disk.reads;
            writer.append(
                alloc::format!("HOST DEVICE PCI 0000:{device:02x}:00.0 start\n").as_bytes(),
            );
            writer.flush(&mut disk).unwrap();
            let writes = disk.events.iter().filter(|event| event.is_some()).count();
            assert_eq!(writes, 2, "block {block} device {device}");
            assert_eq!(disk.reads, reads);
        }
    }
}

#[test]
fn a_resumed_log_continues_the_same_boot() {
    let mut disk = Recorder::new(HEADER_BYTES + 4 * 4096, 512);
    let mut loader = Writer::open(&mut disk).unwrap();
    loader.append(b"before the takeover\n");
    loader.flush(&mut disk).unwrap();
    let mut engine = Writer::resume(&mut disk).unwrap();
    assert_eq!(engine.boots(), 1);
    engine.append(b"after it\n");
    engine.flush(&mut disk).unwrap();
    let log = disk.log().unwrap();
    assert_eq!(log.boots, 1);
    assert_eq!(
        text(&log),
        "=== STELA BOOT 1 ===\nbefore the takeover\nafter it\n"
    );
}

#[test]
fn boots_append_markers_on_512_and_4096_byte_blocks() {
    for block in [512, 4096] {
        let mut disk = Recorder::new(HEADER_BYTES + 4 * 4096, block);
        let mut writer = Writer::open(&mut disk).unwrap();
        assert_eq!(writer.fresh(), Some(&HeaderError::Blank));
        assert_eq!(writer.boots(), 1);
        writer.append(b"first line\n");
        writer.flush(&mut disk).unwrap();
        assert_eq!(
            text(&disk.log().unwrap()),
            "=== STELA BOOT 1 ===\nfirst line\n"
        );
        // Small flushes rewrite the tail block from memory, never
        // reading it back.
        let reads = disk.reads;
        for _ in 0..20 {
            writer.append(b"0123456789abcdef");
            writer.flush(&mut disk).unwrap();
        }
        assert_eq!(disk.reads, reads, "{block}-byte blocks");
        // A flush with nothing new writes nothing.
        let events = disk.events.len();
        writer.flush(&mut disk).unwrap();
        assert_eq!(disk.events.len(), events);
        // The next boot counts itself and ends the unfinished line.
        writer.append(b"cut off");
        writer.flush(&mut disk).unwrap();
        let mut again = Writer::open(&mut disk).unwrap();
        assert_eq!(again.fresh(), None);
        assert_eq!(again.boots(), 2);
        again.flush(&mut disk).unwrap();
        let log = disk.log().unwrap();
        assert_eq!(log.boots, 2);
        assert_eq!(log.head, log.stream.len() as u64);
        assert!(
            text(&log).ends_with("cut off\n=== STELA BOOT 2 ===\n"),
            "{}",
            text(&log)
        );
        assert_eq!(log.stream.len(), 21 + 11 + 320 + 7 + 1 + 21);
    }
}

#[test]
fn the_ring_wraps_and_the_reader_keeps_order() {
    for block in [512, 4096] {
        // A two-block ring.
        let ring = 2 * u64::from(block);
        let mut disk = Recorder::new(HEADER_BYTES + ring, block);
        let mut writer = Writer::open(&mut disk).unwrap();
        let mut stream: Vec<u8> = b"=== STELA BOOT 1 ===\n".to_vec();
        for line in 0..(3 * ring / 10) {
            let bytes = format!("line {line:04}\n");
            stream.extend_from_slice(bytes.as_bytes());
            writer.append(bytes.as_bytes());
            if line % 7 == 0 {
                writer.flush(&mut disk).unwrap();
            }
        }
        writer.flush(&mut disk).unwrap();
        let log = disk.log().unwrap();
        assert_eq!(log.head, stream.len() as u64);
        assert_eq!(log.overwritten(), stream.len() as u64 - ring);
        assert_eq!(log.stream, stream[stream.len() - ring as usize..]);
        // A boot opening a wrapped log keeps the oldest bytes of its
        // tail block.
        let mut again = Writer::open(&mut disk).unwrap();
        again.flush(&mut disk).unwrap();
        stream.extend_from_slice(b"=== STELA BOOT 2 ===\n");
        assert_eq!(
            disk.log().unwrap().stream,
            stream[stream.len() - ring as usize..]
        );
    }
}

#[test]
fn reading_back_from_the_head_gives_the_newest_stream_and_stops_when_enough() {
    for block in [512, 4096] {
        let ring = 4 * u64::from(block);
        let mut disk = Recorder::new(HEADER_BYTES + ring, block);
        // A blank region holds no stream.
        assert!(read_newest(&mut disk, 100, |_| false).unwrap().is_empty());
        let mut writer = Writer::open(&mut disk).unwrap();
        for line in 0..(3 * ring / 10) {
            writer.append(format!("line {line:04}\n").as_bytes());
        }
        writer.flush(&mut disk).unwrap();
        // Wrapped, with its head inside a block.
        let whole = disk.log().unwrap().stream;
        assert!(disk.log().unwrap().overwritten() > 0);
        for chunk in [1, 100, u64::from(block), 3 * u64::from(block) + 7, 1 << 20] {
            assert_eq!(read_newest(&mut disk, chunk, |_| false).unwrap(), whole);
            // Stopping early gives a suffix that answers.
            let newest = read_newest(&mut disk, chunk, |read| read.len() >= 300).unwrap();
            assert!(newest.len() >= 300, "{chunk}");
            assert!(whole.ends_with(&newest), "{chunk}");
            assert!(newest.len() < 300 + chunk.max(u64::from(block)) as usize);
        }
    }
}

#[test]
fn the_reader_orders_a_wrapped_region() {
    let ring = 16u64;
    let mut region = vec![0u8; (HEADER_BYTES + ring) as usize];
    Header {
        head: ring + 5,
        boots: 3,
        ..Header::fresh(Layout::ring_only(HEADER_BYTES + ring))
    }
    .encode(&mut region[..HEADER_BYTES as usize]);
    // Stream bytes 5..21: 16..21 wrapped to ring 0..5.
    for at in 5..ring + 5 {
        region[(HEADER_BYTES + at % ring) as usize] = b'a' + at as u8;
    }
    let log = read(&region).unwrap();
    assert_eq!(log.boots, 3);
    assert_eq!(log.overwritten(), 5);
    let expected: Vec<u8> = (5..ring + 5).map(|at| b'a' + at as u8).collect();
    assert_eq!(log.stream, expected);
}

#[test]
fn the_header_is_written_last_so_an_interrupted_flush_keeps_the_old_stream() {
    for block in [512, 4096] {
        let mut disk = Recorder::new(HEADER_BYTES + 8 * 4096, block);
        let mut writer = Writer::open(&mut disk).unwrap();
        writer.append(b"kept\n");
        writer.flush(&mut disk).unwrap();
        let before = disk.log().unwrap();
        // Each flush ends with: data, flush, header, flush.
        let header = disk.events.len() - 2;
        assert_eq!(disk.events[header], Some(0));
        assert_eq!(disk.events[header - 1], None);
        assert_eq!(&disk.events[header + 1..], &[None]);
        assert!(disk.events[..header - 1].iter().all(|e| *e != Some(0)));
        // Data fills three blocks, one write; the device fails before
        // the header.
        writer.append(&vec![b'x'; 3 * block as usize]);
        disk.writes_left = Some(1);
        assert_eq!(writer.flush(&mut disk), Err(BlockError::Timeout));
        assert_eq!(disk.log().unwrap(), before);
    }
}

#[test]
fn a_pending_overflow_is_reported_in_the_stream() {
    let mut pending = Pending::new([0u8; 8]);
    pending.push(b"abc");
    pending.push(b"defgh");
    pending.push(b"ij");
    assert_eq!(pending.bytes(), 8 + "[boot log dropped 2 bytes]\n".len());
    let mut out = Vec::new();
    pending
        .drain(|bytes| {
            out.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(out, b"[boot log dropped 2 bytes]\ncdefghij");
    assert!(pending.is_empty());
    pending.push(b"0123456789AB");
    let mut out = Vec::new();
    pending
        .drain(|bytes| {
            out.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(out, b"[boot log dropped 4 bytes]\n456789AB");

    // Logs that read the same bytes from their own positions.
    let mut pending = Pending::new([0u8; 8]);
    let copy = |pending: &Pending<[u8; 8]>, from| {
        let mut out = Vec::new();
        let at = pending
            .copy_since(from, |bytes| {
                out.extend_from_slice(bytes);
                Ok::<_, ()>(())
            })
            .unwrap();
        (at, out)
    };
    pending.push(b"abcdef");
    assert_eq!(copy(&pending, 0), (6, b"abcdef".to_vec()));
    pending.push(b"ghij");
    assert_eq!(copy(&pending, 6), (10, b"ghij".to_vec()));
    assert_eq!(
        copy(&pending, 0),
        (10, b"[boot log dropped 2 bytes]\ncdefghij".to_vec())
    );
    assert_eq!(copy(&pending, 10), (10, Vec::new()));

    // The writer's own buffer between flushes.
    let mut disk = Recorder::new(HEADER_BYTES + 4 * 4096, 512);
    let mut writer = Writer::with_pending(&mut disk, 64, Some("")).unwrap();
    for line in 0..10 {
        writer.append(format!("line {line:02}\n").as_bytes());
    }
    writer.flush(&mut disk).unwrap();
    // The marker (21 bytes) and 10 lines of 8 bytes; 64 are kept.
    assert_eq!(
        text(&disk.log().unwrap()),
        "[boot log dropped 37 bytes]\n\
             line 02\nline 03\nline 04\nline 05\nline 06\nline 07\nline 08\nline 09\n"
    );
}

#[test]
fn a_blank_or_invalid_header_starts_a_fresh_log_and_the_reader_refuses_it() {
    let mut disk = Recorder::new(HEADER_BYTES + 4096, 512);
    assert_eq!(disk.log(), Err(HeaderError::Blank));
    let mut writer = Writer::open(&mut disk).unwrap();
    writer.append(b"old boot\n");
    writer.flush(&mut disk).unwrap();
    // Without the header's copy, as a writer older than it left the log.
    let mut good = disk.disk.bytes.clone();
    good[COPY_AT as usize..HEADER_BYTES as usize].fill(0);

    let damage = |offset: usize| {
        let mut bytes = good.clone();
        bytes[offset] ^= 0x5a;
        bytes
    };
    let sealed = |mut bytes: Vec<u8>| {
        let digest = Sha256::digest(&bytes[..FIELDS]);
        bytes[FIELDS..FIELDS + 32].copy_from_slice(&digest);
        bytes
    };
    let mut wrong_version = good.clone();
    wrong_version[8] = 3;
    let wrong_version = sealed(wrong_version);
    let mut other_ring = good.clone();
    other_ring[32..40].copy_from_slice(&8192u64.to_le_bytes());
    let other_ring = sealed(other_ring);
    let mut many_records = good.clone();
    many_records[40] = 17;
    let many_records = sealed(many_records);
    let cases = [
        (damage(0), HeaderError::NotALog),
        (damage(16), HeaderError::Checksum),
        (damage(FIELDS + 3), HeaderError::Checksum),
        (wrong_version, HeaderError::Version(3)),
        (many_records, HeaderError::Records(17)),
        (
            other_ring,
            HeaderError::RingSize {
                recorded: 8192,
                region: 4096,
            },
        ),
    ];
    for (bytes, error) in cases {
        disk.disk.bytes = bytes;
        assert_eq!(disk.log(), Err(error.clone()));
        let mut writer = Writer::open(&mut disk).unwrap();
        assert_eq!(writer.fresh(), Some(&error));
        assert_eq!(writer.boots(), 1);
        writer.flush(&mut disk).unwrap();
        // The old ring bytes are not taken as log data.
        assert_eq!(text(&disk.log().unwrap()), "=== STELA BOOT 1 ===\n");
    }
    assert_eq!(
        read(&good[..HEADER_BYTES as usize]),
        Err(HeaderError::Short {
            bytes: HEADER_BYTES
        })
    );
}

#[test]
fn unsupported_geometry_is_refused() {
    let mut big = RamDisk::new("log", 4, 8192);
    assert!(matches!(
        Writer::open(&mut big),
        Err(BlockError::Unsupported(_))
    ));
    let mut tiny = RamDisk::new("log", 8, 512);
    assert!(matches!(
        Writer::open(&mut tiny),
        Err(BlockError::Unsupported(_))
    ));
    assert_eq!(PARTITION_BYTES, 0xc1_0000);
    assert_eq!(OFFSET, 0x81_0000);
}

#[test]
fn a_file_gives_the_block_runs_of_its_clusters_that_hold_the_layout() {
    use stela_formats::payload::pieces::Extent;
    let file = |extents: &[(u64, u64)]| fat32::File {
        bytes: extents.iter().map(|e| e.1).sum(),
        extents: extents
            .iter()
            .map(|&(offset, bytes)| Extent { offset, bytes })
            .collect(),
    };
    let size = 16 << 20;
    assert_eq!(
        file_runs(&file(&[(0x10_0000, size)]), 512, PARTITION_BYTES),
        Ok(vec![(0x800, PARTITION_BYTES / 512)])
    );
    // Scattered clusters give their runs in file order, adjacent ones
    // joined, up to the bytes the layout needs.
    assert_eq!(
        file_runs(
            &file(&[
                (0x20_0000, 0x1000),
                (0x20_1000, 0x1000),
                (0x10_0000, 0x1000),
                (0x40_0000, size)
            ]),
            512,
            PARTITION_BYTES
        ),
        Ok(vec![
            (0x1000, 16),
            (0x800, 8),
            (0x2000, (PARTITION_BYTES - 0x3000) / 512)
        ])
    );
    assert_eq!(
        file_runs(&file(&[]), 512, PARTITION_BYTES),
        Err(FileError::Short {
            bytes: 0,
            needed: PARTITION_BYTES
        })
    );
    assert_eq!(
        file_runs(&file(&[(0x10_0200, size)]), 4096, PARTITION_BYTES),
        Err(FileError::Misaligned {
            offset: 0x10_0200,
            block: 4096
        })
    );
    // A run may not end inside a block when another follows it.
    assert_eq!(
        file_runs(&file(&[(0, 0x1200), (0x8000, size)]), 4096, PARTITION_BYTES),
        Err(FileError::Misaligned {
            offset: 0x1200,
            block: 4096
        })
    );
    // A partial last block does not count.
    assert_eq!(
        file_runs(
            &file(&[(0, PARTITION_BYTES + 0x800)]),
            4096,
            PARTITION_BYTES
        ),
        Ok(vec![(0, PARTITION_BYTES / 4096)])
    );
    assert_eq!(
        file_runs(&file(&[(0, PARTITION_BYTES - 1)]), 512, PARTITION_BYTES),
        Err(FileError::Short {
            bytes: PARTITION_BYTES - 512,
            needed: PARTITION_BYTES
        })
    );
    // A log file needs only the region.
    assert_eq!(
        file_runs(&file(&[(0, LOG_BYTES)]), 512, LOG_BYTES),
        Ok(vec![(0, LOG_BYTES / 512)])
    );
}

/// A small region with three boot records of one 4 KiB part each.
const SMALL: Layout = Layout {
    ring_bytes: 8 * 4096,
    records: 3,
    first_bytes: 4096,
    last_bytes: 4096,
};

fn small_disk(block: u32) -> Recorder {
    Recorder::new(SMALL.region(), block)
}

fn open_small(disk: &mut Recorder, new_boot: bool) -> Writer {
    Writer::with_layout(disk, PENDING_BYTES, new_boot.then_some(""), SMALL).unwrap()
}

/// `count` numbered lines of 12 bytes from `from`.
fn lines(from: usize, count: usize) -> Vec<u8> {
    (from..from + count)
        .flat_map(|line| format!("line {line:06}\n").into_bytes())
        .collect()
}

/// A version 1 log holding `stream`, the newest bytes its ring has room
/// for: a 40-byte header and a ring that fills the region.
fn version_1(region: u64, block: u32, stream: &[u8], boots: u64) -> Recorder {
    let mut disk = Recorder::new(region, block);
    let ring = region - HEADER_BYTES;
    let head = stream.len() as u64;
    let bytes = &mut disk.disk.bytes;
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
    bytes[16..24].copy_from_slice(&head.to_le_bytes());
    bytes[24..32].copy_from_slice(&boots.to_le_bytes());
    bytes[32..40].copy_from_slice(&ring.to_le_bytes());
    let digest = Sha256::digest(&bytes[..40]);
    bytes[40..72].copy_from_slice(&digest);
    for at in head.saturating_sub(ring)..head {
        bytes[(HEADER_BYTES + at % ring) as usize] = stream[at as usize];
    }
    disk
}

#[test]
fn a_version_1_log_is_read_as_it_was_written() {
    let stream = lines(0, 6000);
    let region = SMALL.region();
    let ring = (region - HEADER_BYTES) as usize;
    let disk = version_1(region, 512, &stream, 7);
    let log = disk.log().unwrap();
    assert_eq!((log.boots, log.head), (7, stream.len() as u64));
    assert_eq!(log.ring_bytes, ring as u64);
    assert_eq!(log.stream, stream[stream.len() - ring..]);
    assert!(log.starts.is_empty());
    assert_eq!(log.kept_starts().count(), 0);
}

#[test]
fn a_version_1_log_is_converted_keeping_its_newest_lines_in_order() {
    let region = SMALL.region();
    let old_ring = region - HEADER_BYTES;
    let kept = SMALL.ring_bytes as usize;
    // The head before the record area, inside it, inside it after a wrap,
    // and before it after a wrap.
    for (head, removed) in [
        (20_000, false),
        (40_000, false),
        (old_ring + 40_001, false),
        (old_ring + 10_001, true),
        (old_ring + kept as u64, false),
    ] {
        for block in [512, 4096] {
            let stream = lines(0, head as usize / 12 + 1)[..head as usize].to_vec();
            let mut disk = version_1(region, block, &stream, 4);
            let before = disk.log().unwrap();
            // A conversion that ends before its header leaves a version 1
            // log, and the next one gives what an undisturbed one does.
            drop(open_small(&mut disk, true));
            let interrupted = disk.log().unwrap();
            assert_eq!((interrupted.head, interrupted.boots), (head, 4));
            assert_eq!(interrupted.ring_bytes, old_ring);
            let mut writer = open_small(&mut disk, true);
            assert!(writer.converted() && writer.fresh().is_none());
            assert_eq!(writer.boots(), 5);
            writer.flush(&mut disk).unwrap();
            assert_eq!(writer.verify(&mut disk), Ok(()));
            let log = disk.log().unwrap();
            assert_eq!(log.ring_bytes, kept as u64);
            assert!(log.head >= head, "{head} {block}");
            let marker = "\n=== STELA BOOT 5 ===\n";
            let converted = log.stream.strip_suffix(marker.as_bytes()).unwrap();
            if !removed {
                let newest = &stream[stream.len().saturating_sub(kept)..];
                // The marker took the place of the oldest bytes of a full ring.
                assert!(newest.ends_with(converted), "{head} {block}");
                assert!(newest.len() - converted.len() <= marker.len());
                continue;
            }
            // The older part, the line that says what went, the newest part.
            let line = removed_line(old_ring - kept as u64);
            let place = (head % old_ring) as usize;
            let at = converted
                .windows(line.len())
                .position(|window| window == line)
                .unwrap();
            assert_eq!(
                &converted[at + line.len()..],
                &stream[stream.len() - place..]
            );
            let older = &converted[..at];
            assert!(
                before.stream[..kept - place - line.len()].ends_with(older),
                "{head} {block}"
            );
            // A second open finds a version 2 log.
            assert!(!open_small(&mut disk, false).converted());
        }
    }
}

#[test]
fn a_boot_keeps_its_first_lines_once_the_ring_has_overwritten_them() {
    for block in [512, 4096] {
        let mut disk = small_disk(block);
        let mut writer = open_small(&mut disk, true);
        // Nearly twice the ring, stored a few lines at a time.
        let body = lines(0, 5000);
        for chunk in body.chunks(12 * 50) {
            writer.append(chunk);
            writer.flush(&mut disk).unwrap();
        }
        let mut stream = b"=== STELA BOOT 1 ===\n".to_vec();
        stream.extend_from_slice(&body);
        let log = disk.log().unwrap();
        assert_eq!(
            log.stream,
            stream[stream.len() - SMALL.ring_bytes as usize..]
        );
        let [start] = &log.starts[..] else {
            panic!("{:?}", log.starts.len());
        };
        assert_eq!((start.boot, start.start), (1, 0));
        // Whole lines, as many as fit the record.
        let first = whole_lines(&stream[..4096]);
        assert_eq!(start.first, first);
        assert!(first.len() > 4096 - 12 && first.ends_with(b"\n"));
        assert!(start.last.is_empty());
        let kept: Vec<_> = log.kept_starts().collect();
        assert_eq!(kept, [(start, false)]);

        // The next boot gives it its last lines, from a line's start.
        let mut next = open_small(&mut disk, true);
        next.append(b"short boot\n");
        next.flush(&mut disk).unwrap();
        let log = disk.log().unwrap();
        assert_eq!(log.starts.len(), 2);
        assert_eq!(log.starts[0].first, first);
        let last = &log.starts[0].last;
        assert!(stream.ends_with(last) && last.starts_with(b"line "));
        assert!(last.len() > 4096 - 12 && last.len() <= 4096);
        assert_eq!(
            (log.starts[1].boot, log.starts[1].start),
            (2, stream.len() as u64)
        );
        assert!(log.starts[1].first.is_empty());

        // A boot that ended before its first lines were complete gets all
        // of them at the next open, and no last lines beside them.
        let mut third = open_small(&mut disk, true);
        third.flush(&mut disk).unwrap();
        let log = disk.log().unwrap();
        assert_eq!(log.starts[1].first, b"=== STELA BOOT 2 ===\nshort boot\n");
        assert!(log.starts[1].last.is_empty());
        // The stream holds boots 2 and 3 whole: only boot 1 is told apart.
        assert_eq!(log.kept_starts().count(), 1);

        // The fourth boot takes the oldest record.
        let mut fourth = open_small(&mut disk, true);
        fourth.flush(&mut disk).unwrap();
        let log = disk.log().unwrap();
        let boots: Vec<u64> = log.starts.iter().map(|start| start.boot).collect();
        assert_eq!(boots, [2, 3, 4]);
        assert_eq!(log.kept_starts().count(), 0);
    }
}

#[test]
fn a_resumed_boot_goes_on_with_its_first_lines() {
    let mut disk = small_disk(512);
    let mut loader = open_small(&mut disk, true);
    let before = lines(0, 100);
    loader.append(&before);
    loader.flush(&mut disk).unwrap();
    drop(loader);
    let mut engine = open_small(&mut disk, false);
    let after = lines(100, 600);
    engine.append(&after);
    engine.flush(&mut disk).unwrap();
    let mut stream = b"=== STELA BOOT 1 ===\n".to_vec();
    stream.extend_from_slice(&before);
    stream.extend_from_slice(&after);
    let log = disk.log().unwrap();
    assert_eq!(log.starts[0].first, whole_lines(&stream[..4096]));
    // A third open in the same boot finds the record complete.
    let mut again = open_small(&mut disk, false);
    again.append(b"more\n");
    again.flush(&mut disk).unwrap();
    assert_eq!(
        disk.log().unwrap().starts[0].first,
        whole_lines(&stream[..4096])
    );
}

/// A line costs what it did without records: its tail block and the
/// header. A whole log's boot writes its first lines once more, in the one
/// store during which they are complete.
#[test]
fn a_boot_s_record_costs_one_write_and_no_line_more() {
    let mut disk = Recorder::new(LOG_BYTES, 512);
    let mut writer = Writer::open(&mut disk).unwrap();
    assert_eq!(writer.layout(), Layout::of(LOG_BYTES));
    assert_eq!(writer.ring_bytes(), RING_BYTES);
    writer.flush(&mut disk).unwrap();
    let (mut two, mut three) = (0, 0);
    for line in 0..2400 {
        disk.events.clear();
        let reads = disk.reads;
        writer.append(
            format!("HOST DEVICE PCI 0000:{line:04x}:00.0 start, as a boot says it\n").as_bytes(),
        );
        writer.flush(&mut disk).unwrap();
        assert_eq!(disk.reads, reads);
        match disk.events.iter().filter(|event| event.is_some()).count() {
            2 => two += 1,
            3 => three += 1,
            writes => panic!("line {line}: {writes} writes"),
        }
    }
    assert_eq!((two, three), (2399, 1));
    let log = disk.log().unwrap();
    assert_eq!(log.starts.len(), 1);
    let first = &log.starts[0].first;
    assert!(log.stream.starts_with(first));
    assert!(first.len() > FIRST_BYTES as usize - 80 && first.len() <= FIRST_BYTES as usize);
    // The next boot's first store adds the last boot's last lines.
    let mut next = Writer::open(&mut disk).unwrap();
    disk.events.clear();
    next.flush(&mut disk).unwrap();
    assert_eq!(
        disk.events.iter().filter(|event| event.is_some()).count(),
        3
    );
    let log = disk.log().unwrap();
    assert!(log.starts[0].last.len() > LAST_BYTES as usize - 80);
    assert!(log.stream.ends_with(b"=== STELA BOOT 2 ===\n"));
}

/// The blocks of record `index`'s room in the small layout.
fn record_blocks(index: usize, block: u32) -> core::ops::Range<u64> {
    let at = SMALL.record_at(index) / u64::from(block);
    at..at + SMALL.record_bytes() / u64::from(block)
}

/// A boot that takes the oldest boot's record writes its first lines
/// there only once the header on the device names the record as its own:
/// a store interrupted before its header leaves the older boot's record
/// whole, never the older boot's lengths over the new boot's bytes.
#[test]
fn a_record_s_room_is_written_only_once_the_header_names_its_boot() {
    for block in [512, 4096] {
        let mut disk = small_disk(block);
        let mut first = open_small(&mut disk, true);
        first.append(&lines(0, 400));
        first.flush(&mut disk).unwrap();
        first.append(b"more\n");
        first.flush(&mut disk).unwrap();
        for _ in 2..4 {
            open_small(&mut disk, true).flush(&mut disk).unwrap();
        }
        let kept = disk.log().unwrap().starts[0].clone();
        assert_eq!((kept.boot, kept.first.len() > 4000), (1, true));
        // Boot 4 takes boot 1's record; its first store fills the room
        // and dies before its header.
        let mut fourth = open_small(&mut disk, true);
        assert_eq!(fourth.boots(), 4);
        fourth.append(&lines(1000, 400));
        disk.events.clear();
        let header = disk.disk.bytes[..HEADER_BYTES as usize].to_vec();
        fourth.flush(&mut disk).unwrap();
        let room = record_blocks(1, block);
        assert!(
            disk.events.iter().flatten().all(|lba| !room.contains(lba)),
            "{block}: {:?}",
            disk.events
        );
        // The same store with its header lost: boot 1's record is intact.
        let mut lost = disk.disk.bytes.clone();
        lost[..HEADER_BYTES as usize].copy_from_slice(&header);
        let log = read(&lost).unwrap();
        assert_eq!(log.starts.iter().find(|start| start.boot == 1), Some(&kept));
        // The next store writes the record, before its header.
        fourth.append(b"more\n");
        disk.events.clear();
        fourth.flush(&mut disk).unwrap();
        let writes: Vec<u64> = disk.events.iter().flatten().copied().collect();
        let at = writes.iter().position(|lba| room.contains(lba)).unwrap();
        assert_eq!(writes.last(), Some(&0));
        assert!(at < writes.len() - 1);
        let log = disk.log().unwrap();
        let start = log.starts.iter().find(|start| start.boot == 4).unwrap();
        assert!(
            start
                .first
                .starts_with(b"=== STELA BOOT 4 ===\nline 001000\n")
        );
        assert!(log.starts.iter().all(|start| start.boot != 1));
    }
}

/// A store that fails before or during its record's write writes the
/// record in the next store, before the header that names its length.
#[test]
fn a_failed_store_writes_its_record_in_the_next() {
    for block in [512, 4096] {
        // Fail at the ring's write, then at the record's.
        for allowed in [0, 1] {
            let mut disk = small_disk(block);
            let mut writer = open_small(&mut disk, true);
            writer.flush(&mut disk).unwrap();
            writer.append(&lines(0, 400));
            disk.writes_left = Some(allowed);
            assert_eq!(writer.flush(&mut disk), Err(BlockError::Timeout));
            assert!(disk.log().unwrap().starts[0].first.is_empty());
            disk.writes_left = None;
            writer.flush(&mut disk).unwrap();
            let log = disk.log().unwrap();
            let mut stream = b"=== STELA BOOT 1 ===\n".to_vec();
            stream.extend_from_slice(&lines(0, 400));
            assert_eq!(
                log.starts[0].first,
                whole_lines(&stream[..4096]),
                "{block} {allowed}"
            );
        }
    }
}

/// A record whose room does not start with its boot's marker, as one a
/// write left half done, is not read as that boot's lines.
#[test]
fn a_record_that_does_not_start_with_its_boot_is_not_read() {
    let mut disk = small_disk(512);
    let mut writer = open_small(&mut disk, true);
    writer.append(&lines(0, 400));
    writer.flush(&mut disk).unwrap();
    writer.append(b"more\n");
    writer.flush(&mut disk).unwrap();
    assert_eq!(disk.log().unwrap().starts.len(), 1);
    let at = SMALL.record_at(1) as usize;
    disk.disk.bytes[at..at + 12].copy_from_slice(b"line 000999\n");
    assert!(disk.log().unwrap().starts.is_empty());
}

/// A store that fails part way, as a USB stick that times out once, loses
/// no line: the next store writes the ring blocks it did not, before the
/// header that counts them.
#[test]
fn a_failed_store_s_lines_are_written_by_the_next() {
    for block in [512, 4096] {
        for allowed in 0..4 {
            let mut disk = small_disk(block);
            let mut writer = open_small(&mut disk, true);
            writer.append(&lines(0, 100));
            writer.flush(&mut disk).unwrap();
            // Several blocks of lines, the ring turned past its end.
            writer.append(&lines(100, 3000));
            disk.writes_left = Some(allowed);
            assert_eq!(writer.flush(&mut disk), Err(BlockError::Timeout));
            disk.writes_left = None;
            writer.append(&lines(3100, 10));
            writer.flush(&mut disk).unwrap();
            let mut stream = b"=== STELA BOOT 1 ===\n".to_vec();
            stream.extend_from_slice(&lines(0, 3110));
            let log = disk.log().unwrap();
            let ring = SMALL.ring_bytes as usize;
            assert_eq!(
                text(&log),
                String::from_utf8(stream[stream.len() - ring..].to_vec()).unwrap(),
                "{block} {allowed}"
            );
        }
    }
}

#[test]
fn a_log_file_s_region_is_its_whole_pages_up_to_the_longest() {
    assert_eq!(file_region(LOG_BYTES - 1), None);
    assert_eq!(file_region(LOG_BYTES), Some(LOG_BYTES));
    assert_eq!(file_region(LOG_BYTES + 100), Some(LOG_BYTES));
    assert_eq!(file_region(FILE_BYTES), Some(FILE_BYTES));
    assert_eq!(file_region((33 << 20) + 4095), Some(33 << 20));
    assert_eq!(file_region(1 << 30), Some(MAX_FILE_BYTES));
    // The region a machine-state partition holds keeps its layout, so
    // every log written before stays readable.
    assert_eq!(Layout::of(LOG_BYTES).ring_bytes, 3_141_632);
    assert_eq!(Layout::of(LOG_BYTES).records, RECORDS);
    // A long file: larger records for more boots, the rest ring.
    let large = Layout::of(FILE_BYTES);
    assert_eq!(
        (large.records, large.first_bytes, large.last_bytes),
        (LARGE_RECORDS, 384 << 10, 64 << 10)
    );
    assert_eq!(
        large.ring_bytes,
        FILE_BYTES - HEADER_BYTES - ((16 * 448) << 10)
    );
    for region in [LARGE_BYTES, FILE_BYTES, 20 << 20, MAX_FILE_BYTES] {
        for block in [512, 4096] {
            assert!(Layout::of(region).fits(region, block), "{region} {block}");
        }
    }
    // Its header names every record in its first block.
    assert!(FIELDS + LARGE_RECORDS as usize * RECORD_FIELDS + 32 <= 512);
}

/// A boot on a long log file keeps up to 384 KiB of its first lines, all
/// it prints before macOS starts, past the ring's turn.
#[test]
fn a_long_log_file_keeps_each_boot_s_first_lines_whole() {
    let mut disk = Recorder::new(FILE_BYTES, 512);
    let mut writer = Writer::open(&mut disk).unwrap();
    assert_eq!(writer.layout(), Layout::of(FILE_BYTES));
    // A head of 300 KB, then more than the ring.
    let head = lines(0, 25_000);
    writer.append(&head);
    writer.flush(&mut disk).unwrap();
    let ring = writer.ring_bytes() as usize;
    let mut written = head.len();
    let mut next = 25_000;
    while written < ring + (1 << 20) {
        let chunk = lines(next, 20_000);
        next += 20_000;
        written += chunk.len();
        writer.append(&chunk);
        writer.flush(&mut disk).unwrap();
    }
    let log = disk.log().unwrap();
    assert_eq!(log.ring_bytes, ring as u64);
    assert!(log.overwritten() > 0);
    let [start] = &log.starts[..] else {
        panic!("{}", log.starts.len());
    };
    let mut expected = b"=== STELA BOOT 1 ===\n".to_vec();
    expected.extend_from_slice(&head);
    assert!(start.first.len() > head.len() && start.first.starts_with(&expected));
    assert_eq!(log.kept_starts().count(), 1);
}

/// `stela` makes a 4 MiB log file 32 MiB long: every line it held stays
/// at its position, each kept boot's record goes with it, and the next
/// boot writes on.
#[test]
fn a_log_laid_out_again_in_a_longer_file_keeps_its_lines_and_records() {
    let mut disk = Recorder::new(LOG_BYTES, 512);
    // Ten boots of 600 KB each: the 3 MiB ring has turned, eight records.
    let mut next = 0;
    for _ in 0..10 {
        let mut writer = Writer::open(&mut disk).unwrap();
        for _ in 0..5 {
            writer.append(&lines(next, 10_000));
            next += 10_000;
            writer.flush(&mut disk).unwrap();
        }
    }
    let old = disk.log().unwrap();
    assert_eq!(old.starts.len(), 8);
    assert!(old.overwritten() > 0);
    let grown = relayout(&disk.disk.bytes, FILE_BYTES).unwrap();
    let new = read(&grown).unwrap();
    assert_eq!((new.boots, new.head), (old.boots, old.head));
    assert_eq!(new.ring_bytes, Layout::of(FILE_BYTES).ring_bytes);
    assert_eq!(new.stream, old.stream);
    assert_eq!(new.starts, old.starts);
    // The next boot opens it as it is and fills the last boot's record.
    let mut disk = Recorder::new(FILE_BYTES, 512);
    disk.disk.bytes.copy_from_slice(&grown);
    let mut writer = Writer::open(&mut disk).unwrap();
    assert!(writer.fresh().is_none());
    writer.append(b"after the change\n");
    writer.flush(&mut disk).unwrap();
    let log = disk.log().unwrap();
    assert!(log.stream.starts_with(&old.stream));
    assert!(
        log.stream
            .ends_with(b"=== STELA BOOT 11 ===\nafter the change\n")
    );
    assert_eq!(log.starts.len(), 9);
    assert!(!log.starts[7].last.is_empty());
    // A version 1 log is laid out too, without records.
    let stream = lines(0, 6000);
    let v1 = version_1(LOG_BYTES, 512, &stream, 3);
    let grown = read(&relayout(&v1.disk.bytes, FILE_BYTES).unwrap()).unwrap();
    assert_eq!((grown.boots, grown.stream), (3, stream));
    // A blank file is refused by name.
    assert_eq!(
        relayout(&vec![0; LOG_BYTES as usize], FILE_BYTES),
        Err(HeaderError::Blank)
    );
}

/// A loader older than the longer file opens it as a 4 MiB region and
/// starts a 4 MiB log at its start. The reader reads that log, and the next
/// boot lays it out in the whole file in place with its boots, where it
/// started the file fresh again and they were lost.
#[test]
fn a_smaller_log_an_older_loader_started_in_the_file_is_grown_in_place() {
    for older_boots in [3, 12] {
        let mut disk = Recorder::new(FILE_BYTES, 512);
        let mut next = 0;
        for _ in 0..2 {
            let mut writer = Writer::open(&mut disk).unwrap();
            writer.append(&lines(next, 1000));
            next += 1000;
            writer.flush(&mut disk).unwrap();
        }
        // The older loader sees the file's first 4 MiB, whose header is not
        // its region's, and starts a log there; twelve boots of 600 KB turn
        // its ring.
        let mut older = Recorder::new(LOG_BYTES, 512);
        older
            .disk
            .bytes
            .copy_from_slice(&disk.disk.bytes[..LOG_BYTES as usize]);
        for boot in 0..older_boots {
            let mut writer = Writer::open(&mut older).unwrap();
            assert_eq!(writer.fresh().is_some(), boot == 0);
            for _ in 0..5 {
                writer.append(&lines(next, 10_000));
                next += 10_000;
                writer.flush(&mut older).unwrap();
            }
        }
        disk.disk.bytes[..LOG_BYTES as usize].copy_from_slice(&older.disk.bytes);
        let small = older.log().unwrap();
        assert_eq!(small.overwritten() > 0, older_boots == 12);
        assert_eq!(disk.log().unwrap(), small, "read in the whole file");
        assert_eq!(
            read_newest(&mut disk, 1 << 20, |_| false).unwrap(),
            small.stream,
            "its newest lines read back"
        );
        let mut writer = Writer::open(&mut disk).unwrap();
        assert!(writer.fresh().is_none());
        assert!(!writer.converted());
        assert_eq!(writer.grown(), Some(LOG_BYTES));
        assert_eq!(writer.layout(), Layout::of(FILE_BYTES));
        writer.append(b"after the older loader\n");
        writer.flush(&mut disk).unwrap();
        let log = disk.log().unwrap();
        assert_eq!(log.boots, older_boots + 1);
        assert_eq!(log.overwritten(), small.overwritten());
        assert!(log.stream.starts_with(&small.stream));
        let marker = format!(
            "=== STELA BOOT {} ===\nafter the older loader\n",
            older_boots + 1
        );
        assert!(log.stream.ends_with(marker.as_bytes()));
        assert_eq!(log.starts.len(), small.starts.len() + 1);
        for (grown, kept) in log.starts.iter().zip(&small.starts) {
            assert_eq!(
                (grown.boot, grown.start, &grown.first),
                (kept.boot, kept.start, &kept.first)
            );
        }
    }
}

/// Each boot's marker says what it is and when it started, and its record
/// is read with it.
#[test]
fn a_boot_s_marker_carries_its_note_and_its_record_is_read_with_it() {
    let mut disk = small_disk(512);
    let mut writer = Writer::with_layout(
        &mut disk,
        PENDING_BYTES,
        Some("version=9.9.9 firmware_clock=2026-11-18 01:44:12"),
        SMALL,
    )
    .unwrap();
    writer.append(&lines(0, 400));
    writer.flush(&mut disk).unwrap();
    writer.append(b"more\n");
    writer.flush(&mut disk).unwrap();
    let log = disk.log().unwrap();
    assert!(log.stream.starts_with(
        b"=== STELA BOOT 1 === version=9.9.9 firmware_clock=2026-11-18 01:44:12\nline 000000\n"
    ));
    let [start] = &log.starts[..] else {
        panic!("{}", log.starts.len());
    };
    assert!(
        start
            .first
            .starts_with(b"=== STELA BOOT 1 === version=9.9.9")
    );
    // Another boot's number with a note does not pass for this one's.
    let at = SMALL.record_at(1) as usize;
    disk.disk.bytes[at..at + 22].copy_from_slice(b"=== STELA BOOT 10 === ");
    assert!(disk.log().unwrap().starts.is_empty());
}

/// Every store writes the header's 4 KiB: the header, and its copy in the
/// last 512 bytes. A header that no longer decodes is read from its copy,
/// by the reader and by the next boot, which goes on with the log where it
/// started it fresh before; a newer version's header is not.
#[test]
fn a_header_that_does_not_decode_is_read_from_its_copy() {
    for block in [512, 4096] {
        let mut disk = small_disk(block);
        for boot in 0..3 {
            let mut writer = open_small(&mut disk, true);
            writer.append(&lines(boot * 100, 100));
            writer.flush(&mut disk).unwrap();
        }
        let before = disk.log().unwrap();
        let page = &disk.disk.bytes[..HEADER_BYTES as usize];
        assert_eq!(page[COPY_AT as usize..], page[..512], "{block}");
        // A torn header block.
        disk.disk.bytes[16] ^= 0xff;
        assert_eq!(disk.log().unwrap(), before, "{block}");
        assert_eq!(
            read_newest(&mut disk, 1 << 20, |_| false).unwrap(),
            before.stream
        );
        let mut writer = open_small(&mut disk, true);
        assert!(writer.restored() && writer.fresh().is_none(), "{block}");
        assert!(!writer.converted());
        assert_eq!(writer.boots(), 4);
        writer.append(b"after the torn header\n");
        writer.flush(&mut disk).unwrap();
        let log = disk.log().unwrap();
        assert!(log.stream.starts_with(&before.stream));
        assert!(
            log.stream
                .ends_with(b"=== STELA BOOT 4 ===\nafter the torn header\n")
        );
        // A newer version's header is its own: the log starts fresh.
        disk.disk.bytes[8..12].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(disk.log(), Err(HeaderError::Version(9)));
        let writer = open_small(&mut disk, true);
        assert_eq!(writer.fresh(), Some(&HeaderError::Version(9)));
    }
}

/// What a released loader (0.5.0 and older) leaves in a longer log file:
/// its version 1 log of `stream` and `boots` in the file's first 4 MiB,
/// written through 512-byte blocks, its header's block and the ring blocks
/// its stream reached, and nothing else.
fn as_a_released_loader_writes(disk: &mut Recorder, stream: &[u8], boots: u64) {
    let older = version_1(LOG_BYTES, 512, stream, boots);
    let ring = LOG_BYTES - HEADER_BYTES;
    let reached = (stream.len() as u64).min(ring).next_multiple_of(512);
    let bytes = &mut disk.disk.bytes;
    bytes[..512].copy_from_slice(&older.disk.bytes[..512]);
    let ring_at = HEADER_BYTES as usize..(HEADER_BYTES + reached) as usize;
    bytes[ring_at.clone()].copy_from_slice(&older.disk.bytes[ring_at]);
}

/// The released loader's boots after the ones a newer loader wrote: the
/// stream a released loader starts its log with.
fn released_boots(boots: u64, lines_each: usize) -> Vec<u8> {
    let mut stream = Vec::new();
    for boot in 1..=boots {
        stream.extend_from_slice(format!("=== STELA BOOT {boot} ===\n").as_bytes());
        stream.extend_from_slice(&lines(boot as usize * 1_000_000, lines_each));
    }
    stream
}

/// A loader older than the longer file starts its own 4 MiB log at the
/// file's start, over the newer log's header and the start of its ring. The
/// header's copy, which it leaves alone, still names the newer log: the
/// reader and the next boot keep that log's boots, its records and the
/// lines the older loader did not write over, then the older loader's boots
/// numbered on, where they had only the older loader's before.
#[test]
fn the_boots_before_an_older_loader_s_log_are_kept_with_it() {
    // Unwrapped: 3 short boots; wrapped: 12 boots that turn its ring.
    for (older_boots, older_lines) in [(3u64, 2_000usize), (12, 30_000)] {
        let mut disk = Recorder::new(FILE_BYTES, 512);
        // 20 boots of 1.6 MB turn the 25 MiB ring: 16 records.
        for boot in 0..20 {
            let mut writer = Writer::open(&mut disk).unwrap();
            for part in 0..10 {
                writer.append(&lines(boot * 200_000 + part * 13_000, 13_000));
                writer.flush(&mut disk).unwrap();
            }
        }
        let newer = disk.log().unwrap();
        assert_eq!((newer.boots, newer.starts.len()), (20, 16));
        assert!(newer.overwritten() > 0);
        let stream = released_boots(older_boots, older_lines);
        as_a_released_loader_writes(&mut disk, &stream, older_boots);
        let older = read(&disk.disk.bytes[..LOG_BYTES as usize]).unwrap();
        assert_eq!(older.boots, older_boots);
        assert_eq!(older.overwritten() > 0, older_boots == 12);

        let log = disk.log().unwrap();
        assert_eq!(log.boots, 20 + older_boots);
        // Every record of the newer log, with its first lines.
        assert_eq!(log.starts.len(), 16);
        for (kept, was) in log.starts.iter().zip(&newer.starts) {
            assert_eq!((kept.boot, &kept.first), (was.boot, &was.first));
        }
        // Its last boot's last lines are its own, kept in its record.
        let last = log.starts.last().unwrap();
        assert_eq!(last.boot, 20);
        assert!(!last.last.is_empty());
        assert!(newer.stream.ends_with(&last.last));
        // The newer lines the older loader did not write over, a line in
        // place of those it did, then the older loader's boots numbered on.
        let text = String::from_utf8(log.stream.clone()).unwrap();
        let gap = text
            .find("bytes are missing here: an older Stela wrote its own log over them]\n")
            .expect("the gap is said");
        let newest = String::from_utf8(newer.stream[newer.stream.len() - 1000..].to_vec()).unwrap();
        let newest = &newest[newest.find('\n').unwrap() + 1..];
        assert!(text.contains(newest), "the newer log's newest lines");
        let older_text = String::from_utf8(older.stream.clone()).unwrap();
        let mut numbered = 0;
        for boot in 1..=older_boots {
            if !older_text.contains(&format!("=== STELA BOOT {boot} ===\n")) {
                continue;
            }
            let marker = format!("=== STELA BOOT {} ===\n", 20 + boot);
            let at = text.find(&marker).unwrap_or_else(|| panic!("{marker}"));
            assert!(at > gap);
            numbered += 1;
        }
        // Those the older loader's own ring turned over are gone from both.
        assert!(numbered + 2 >= older_boots, "{numbered}");
        assert!(log.stream.ends_with(&stream[stream.len() - 1000..]));
        assert_eq!(
            text.contains("the older Stela's log overwrote them"),
            older_boots == 12
        );
        // The newest lines read back are the same.
        let newest = read_newest(&mut disk, 1 << 20, |_| false).unwrap();
        assert!(log.stream.ends_with(&newest) && !newest.is_empty());

        // The next boot lays both out in the file and counts on from them.
        let mut writer = Writer::open(&mut disk).unwrap();
        assert_eq!(writer.grown(), Some(LOG_BYTES));
        assert_eq!(writer.kept_earlier(), Some(20));
        assert_eq!(writer.boots(), 21 + older_boots);
        writer.append(b"after the older loader\n");
        writer.flush(&mut disk).unwrap();
        let after = disk.log().unwrap();
        assert_eq!(after.boots, 21 + older_boots);
        let marker = format!(
            "=== STELA BOOT {} ===\nafter the older loader\n",
            21 + older_boots
        );
        let laid_out = after
            .stream
            .strip_suffix(marker.as_bytes())
            .expect("this boot's lines come last");
        // As the reader read both, or their newest that the ring holds.
        assert!(log.stream.ends_with(laid_out));
        assert!(laid_out.len() as u64 >= (log.stream.len() as u64).min(after.ring_bytes - 64));
        // The newer log's records stay, its last boot's last lines its own:
        // none took the older loader's lines.
        for kept in &after.starts {
            if let Some(was) = log.starts.iter().find(|was| was.boot == kept.boot) {
                assert_eq!(kept, was);
            }
        }
        // Read again, the file is one log: no older loader's log is left.
        let mut again = Writer::open(&mut disk).unwrap();
        assert_eq!((again.grown(), again.kept_earlier()), (None, None));
        again.flush(&mut disk).unwrap();
        assert_eq!(disk.log().unwrap().boots, 22 + older_boots);
    }
}

/// An older loader that wrote its header through 4 KiB blocks wrote over
/// the copy too: its log is grown alone, as before the copy.
#[test]
fn an_older_loader_s_log_over_the_copy_is_grown_alone() {
    let mut disk = Recorder::new(FILE_BYTES, 512);
    for boot in 0..2 {
        let mut writer = Writer::open(&mut disk).unwrap();
        writer.append(&lines(boot * 1000, 1000));
        writer.flush(&mut disk).unwrap();
    }
    let stream = released_boots(2, 100);
    as_a_released_loader_writes(&mut disk, &stream, 2);
    disk.disk.bytes[512..HEADER_BYTES as usize].fill(0);
    let log = disk.log().unwrap();
    assert_eq!((log.boots, &log.stream[..]), (2, &stream[..]));
    let writer = Writer::open(&mut disk).unwrap();
    assert_eq!(
        (writer.grown(), writer.kept_earlier()),
        (Some(LOG_BYTES), None)
    );
    assert_eq!(writer.boots(), 3);
}
