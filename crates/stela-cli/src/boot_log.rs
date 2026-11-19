//! `stela log`: the boot logs `stela.efi` keeps. Every boot writes the
//! USB's own log, [`crate::efi::BOOT_LOG`] on the stick's EFI partition, from
//! its first line; an install keeps another on its machine-state partition
//! (`stela_runtime::pc::boot_log`), at the partition's start, or in the tail
//! of an Apple region that also holds the machine state
//! (`pc::machine_state`), as a setup with `--target` alone lays it out.
//!
//! Without FILE the command reads this computer's disks through raw device
//! access, never through a mounted folder, which Windows refuses for an EFI
//! partition: the disks that do not run this computer first and, when they
//! hold no log, the system disks too. FILE is a whole-disk image or device,
//! a raw partition, or a copy of the `boot.log` file itself. The hidden
//! `--partition`, `--offset` and `--in-target` name one machine-state place
//! instead. `stela log STICK` reads the same places of a stick.
use crate::setup::host::{self, Disk, Host};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    fmt::Write as _,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use stela_contracts::masked::{mask_line, mask_stored_line};
use stela_drivers::block::{BlockDevice, BlockError, Identity, Transport};
use stela_formats::payload::ReadAt;
use stela_runtime::pc::{
    boot_log::{self, Log},
    fat32::Fat32,
    gpt::{self, Guid},
    machine_state,
};

/// The logs found, each with where it is, and the notes on what they lack.
pub(crate) type Found = (Vec<(String, Log)>, Vec<String>);

/// One machine-state place in a file, which the hidden `log` options name
/// instead of finding every log.
#[derive(Default, Clone)]
pub(crate) struct Place {
    /// Only the machine-state partition with this unique GUID in the file's
    /// GPT.
    pub(crate) partition: Option<String>,
    /// Only the machine-state partition at this byte offset in the file (0
    /// for a raw partition).
    pub(crate) offset: Option<u64>,
    /// The partition is a Mac's SSD partition whose last bytes hold the
    /// machine state; with an offset, the file ends where the partition
    /// does.
    pub(crate) in_target: bool,
}

/// `stela log`: the logs of a whole-disk image or device, a raw partition
/// or a copied boot.log (`file`), or of every disk of this computer.
pub(crate) struct BootLog {
    pub(crate) file: Option<PathBuf>,
    /// Where the logs go instead of standard output.
    pub(crate) output: Option<PathBuf>,
    pub(crate) partition: Option<String>,
    pub(crate) offset: Option<u64>,
    pub(crate) in_target: bool,
}

impl BootLog {
    pub(crate) fn run(&self) -> Result<()> {
        let (logs, headings, notes) = match &self.file {
            None => {
                let (logs, notes) = scan_noted(&host::System)?;
                (logs, true, notes)
            }
            Some(file) if self.partition.is_none() && self.offset.is_none() => {
                let (logs, notes) = find(file)?;
                let several = logs.len() > 1;
                (logs, several, notes)
            }
            Some(file) => (vec![(String::new(), self.load(file)?)], false, Vec::new()),
        };
        write_logs(&logs, headings, &notes, self.output.as_deref())
    }

    fn load(&self, path: &Path) -> Result<Log> {
        let mut file =
            File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let (offset, end) = match (&self.partition, self.offset) {
            (Some(guid), _) => {
                let guid: Guid = guid.parse().map_err(anyhow::Error::msg)?;
                let (table, block) = table(path)?;
                let partition = table
                    .partitions
                    .iter()
                    .find(|partition| partition.unique_guid == guid)
                    .with_context(|| {
                        format!(
                            "{} has no partition {guid}; it has:\n{}",
                            path.display(),
                            listing(&table, block)
                        )
                    })?;
                let bytes = partition.blocks() * block;
                ensure!(
                    self.in_target || bytes >= boot_log::PARTITION_BYTES,
                    "partition {guid} holds {bytes:#x} bytes; the boot log needs {:#x}, so \
                     stela.efi boots it without one",
                    boot_log::PARTITION_BYTES
                );
                let start = partition.first_lba * block;
                (start, start + bytes)
            }
            (None, Some(offset)) => (offset, file.seek(SeekFrom::End(0))?),
            (None, None) => unreachable!("`run` finds the logs without a place"),
        };
        // The machine state starts at the partition's start, or in its tail.
        let offset = if self.in_target {
            let layout = machine_state::split(end.saturating_sub(offset)).map_err(|error| {
                anyhow!(
                    "{} at {offset:#x} holds no machine state in its last bytes: {error}",
                    path.display()
                )
            })?;
            offset + layout.state_offset()
        } else {
            offset
        };
        read(&mut file, offset).with_context(|| format!("{}", path.display()))
    }
}

/// Print `logs` in order, each after a heading naming its place when
/// `headings`, or write them to `output` and say where they went. Each
/// log's one-line summary goes to standard error and before its lines,
/// which are written as the boot wrote them, oldest boot first
/// ([`write_log`]). Each of `notes` (what the logs lack, [`scan_noted`]) is
/// the file's first lines, so a reader of an attached file sees it, and is
/// said again after the summary.
pub(crate) fn write_logs(
    logs: &[(String, Log)],
    headings: bool,
    notes: &[String],
    output: Option<&Path>,
) -> Result<()> {
    let replaced = output.is_some_and(Path::exists);
    let mut sink: Box<dyn Write> = match output {
        Some(path) => Box::new(std::io::BufWriter::new(
            File::create(path).with_context(|| format!("cannot create {}", path.display()))?,
        )),
        None => Box::new(std::io::stdout().lock()),
    };
    for note in notes {
        // Headings name mount folders, which can hold the account's name:
        // the written log, which people post, has a tag in its place.
        let note = note.replace('\n', " ");
        match writeln!(sink, "=== NOTE: {} ===", mask_line(&note)) {
            Err(error) if output.is_none() && error.kind() == std::io::ErrorKind::BrokenPipe => {
                return Ok(());
            }
            result => result.context("cannot write the boot log")?,
        }
    }
    for (place, log) in logs {
        let mut written = Ok(());
        if headings {
            eprint!("{place}: ");
            written = writeln!(sink, "=== {} ===", mask_line(place));
        }
        let summary = summary(log);
        eprint!("{summary}");
        match written
            .and_then(|()| writeln!(sink, "=== {} ===", summary.trim_end()))
            .and_then(|()| write_log(&mut sink, log))
        {
            // A reader that closed the pipe early (`| head`) has what it
            // asked for.
            Err(error) if output.is_none() && error.kind() == std::io::ErrorKind::BrokenPipe => {
                return Ok(());
            }
            result => result.context("cannot write the boot log")?,
        }
    }
    match sink.flush() {
        Err(error) if output.is_none() && error.kind() == std::io::ErrorKind::BrokenPipe => {}
        result => result.context("cannot write the boot log")?,
    }
    if let Some(path) = output {
        #[cfg(unix)]
        crate::setup::hand_to_invoker(path);
        eprintln!("{}", saved(logs.len(), path, replaced));
    }
    for note in notes {
        eprintln!("Note: {note}");
    }
    Ok(())
}

/// `bytes`, each line with the identifiers that lines older Stela versions
/// wrote in full in a tag's place ([`mask_stored_line`]), as a boot now
/// writes them: a stick keeps lines from before. A line that is not UTF-8
/// is written as it is unless its readable text holds one.
fn write_masked(sink: &mut dyn Write, bytes: &[u8]) -> std::io::Result<()> {
    for line in bytes.split_inclusive(|&byte| byte == b'\n') {
        let (text, end) = match line.strip_suffix(b"\n") {
            Some(text) => (text, &b"\n"[..]),
            None => (line, &b""[..]),
        };
        let masked = match std::str::from_utf8(text) {
            Ok(text) => mask_stored_line(text).into_owned().into_bytes(),
            Err(_) => match mask_stored_line(&String::from_utf8_lossy(text)) {
                std::borrow::Cow::Owned(masked) => masked.into_bytes(),
                std::borrow::Cow::Borrowed(_) => text.to_vec(),
            },
        };
        sink.write_all(&masked)?;
        sink.write_all(end)?;
    }
    Ok(())
}

/// A log's lines: first what its boot records keep of the boots whose
/// first lines the log has since overwritten (`Log::kept_starts`), each
/// boot under a line naming it, then the stream as it was written, with
/// identifiers masked ([`write_masked`]).
fn write_log(sink: &mut dyn Write, log: &Log) -> std::io::Result<()> {
    let ended_line = |sink: &mut dyn Write, bytes: &[u8]| -> std::io::Result<()> {
        write_masked(sink, bytes)?;
        match bytes.last() {
            Some(b'\n') | None => Ok(()),
            Some(_) => sink.write_all(b"\n"),
        }
    };
    let mut kept = false;
    for (start, ended) in log.kept_starts() {
        kept = true;
        writeln!(
            sink,
            "=== BOOT {}: its first lines, kept apart; the log has since overwritten them ===",
            start.boot
        )?;
        ended_line(sink, &start.first)?;
        match (ended, start.last.is_empty()) {
            (true, false) => {
                writeln!(
                    sink,
                    "=== BOOT {}: its last lines; those between were overwritten ===",
                    start.boot
                )?;
                ended_line(sink, &start.last)?;
            }
            (true, true) => writeln!(
                sink,
                "=== BOOT {}: its later lines were overwritten ===",
                start.boot
            )?,
            (false, _) => writeln!(
                sink,
                "=== BOOT {}: lines were overwritten here; the log below goes on in this boot ===",
                start.boot
            )?,
        }
    }
    if kept {
        writeln!(
            sink,
            "=== THE LOG, every line it still holds, oldest first ==="
        )?;
    }
    write_masked(sink, &log.stream)
}

/// The milliseconds since the PC started that a log line begins with,
/// `[  12.345] ` (`stela.efi`'s `output::stamp`).
fn stamp(line: &[u8]) -> Option<u64> {
    let inner = line.strip_prefix(b"[")?;
    let end = inner.iter().take(16).position(|&byte| byte == b']')?;
    let (seconds, millis) = std::str::from_utf8(&inner[..end])
        .ok()?
        .trim()
        .split_once('.')?;
    Some(seconds.parse::<u64>().ok()? * 1000 + millis.parse::<u64>().ok()?)
}

/// The seconds of running the lines of `stream` cover: in each boot, from
/// its first stamped line to its last.
pub(crate) fn covered_seconds(stream: &[u8]) -> u64 {
    let (mut total, mut span) = (0, None::<(u64, u64)>);
    for line in stream.split(|&byte| byte == b'\n') {
        if line.starts_with(b"=== STELA BOOT ") {
            total += span
                .take()
                .map_or(0, |(first, last)| last.saturating_sub(first));
        } else if let Some(at) = stamp(line) {
            span = Some((span.map_or(at, |(first, _)| first), at));
        }
    }
    (total + span.map_or(0, |(first, last)| last.saturating_sub(first))) / 1000
}

/// `seconds` as a person says a duration: `2 h 40 min`, `49 min`, `35 s`.
fn duration_text(seconds: u64) -> String {
    match (seconds / 3600, seconds % 3600 / 60) {
        (0, 0) => format!("{seconds} s"),
        (0, minutes) => format!("{minutes} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

/// Where `count` logs were written, as the full path to attach.
fn saved(count: usize, path: &Path, replaced: bool) -> String {
    let path = shown(path);
    let over = match replaced {
        true => ", replacing the file that was there",
        false => "",
    };
    match count {
        1 => format!("Saved the boot log to {}{over}", path.display()),
        _ => format!("Saved {count} boot logs to {}{over}", path.display()),
    }
}

/// Where `stela log` saves the logs on a terminal: `log.txt` in this
/// folder, the one file every hint asks to attach.
pub(crate) fn default_output() -> PathBuf {
    PathBuf::from("log.txt")
}

/// `path` in full, as a file to attach is found again.
pub(crate) fn shown(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Every boot log in FILE, each with where it is: the file itself when it
/// is a copied `boot.log`; else in each GPT partition, or in FILE itself
/// without a GPT, the stick's log file on a FAT32 volume, then the machine
/// state's at the start or in the tail.
fn find(path: &Path) -> Result<Found> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let end = file.seek(SeekFrom::End(0))?;
    let mut magic = [0u8; 8];
    file.seek(SeekFrom::Start(0))?;
    if file.read_exact(&mut magic).is_ok() && &magic == boot_log::MAGIC {
        let mut region = vec![0; usize::try_from(end.min(boot_log::MAX_FILE_BYTES))?];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut region)?;
        let log = boot_log::read(&region).with_context(|| format!("{}", path.display()))?;
        return Ok((vec![(String::from("the file"), log)], Vec::new()));
    }
    let (spans, listed) = match table(path) {
        Ok((table, block)) => (
            table
                .partitions
                .iter()
                .map(|partition| {
                    let place = format!("partition {}", partition.unique_guid);
                    (
                        place,
                        partition.first_lba * block,
                        partition.blocks() * block,
                    )
                })
                .collect::<Vec<_>>(),
            format!("; its GPT has:\n{}", listing(&table, block)),
        ),
        Err(_) => (vec![(String::from("the file"), 0, end)], String::new()),
    };
    let mut logs = Logs::default();
    for (place, start, bytes) in spans {
        let mut read = |offset: u64, buffer: &mut [u8]| -> Result<()> {
            ensure!(
                offset + buffer.len() as u64 <= bytes,
                "read past the partition"
            );
            file.seek(SeekFrom::Start(start + offset))?;
            Ok(file.read_exact(buffer)?)
        };
        logs.extend(partition_logs(&mut read, bytes, &place));
    }
    ensure!(
        !logs.is_empty(),
        "{} holds no Stela boot log: no {} on a FAT32 volume, and nothing at the start or in \
         the last 16 MiB of any partition{listed}",
        path.display(),
        crate::efi::BOOT_LOG
    );
    let notes = std::mem::take(&mut logs.stick_notes);
    Ok((logs.all(), notes))
}

/// The boot logs found on one disk or partition: the stick's own log file,
/// then the machine state's.
#[derive(Default)]
pub(crate) struct Logs {
    pub(crate) usb: Vec<(String, Log)>,
    pub(crate) machine: Vec<(String, Log)>,
    /// Why a Stela stick's own log was not read, or was read without its
    /// header: always worth saying.
    pub(crate) stick_notes: Vec<String>,
    /// The partitions that could not be read at all: worth saying only when
    /// no stick's log was found, since one of them may be the stick.
    pub(crate) unread: Vec<String>,
}

impl Logs {
    fn is_empty(&self) -> bool {
        self.usb.is_empty() && self.machine.is_empty()
    }

    fn extend(&mut self, other: Self) {
        self.usb.extend(other.usb);
        self.machine.extend(other.machine);
        self.stick_notes.extend(other.stick_notes);
        self.unread.extend(other.unread);
    }

    /// The USB's logs first.
    pub(crate) fn all(self) -> Vec<(String, Log)> {
        let mut logs = self.usb;
        logs.extend(self.machine);
        logs
    }
}

/// Fills a buffer from one partition, or from a file standing for one, at a
/// byte offset.
type Reader<'a> = dyn FnMut(u64, &mut [u8]) -> Result<()> + 'a;

/// A partition as the FAT32 reader reads it.
struct Span<'a, 'b> {
    read: &'a mut Reader<'b>,
    bytes: u64,
}

impl ReadAt for Span<'_, '_> {
    type Error = anyhow::Error;

    fn len(&self) -> u64 {
        self.bytes
    }

    fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        (self.read)(offset, buffer)
    }
}

/// Every boot log of a `bytes`-long partition named `place`: the stick's
/// own file when it holds a FAT32 volume, as `stela.efi`'s own FAT reader
/// finds it, and the machine state's at its start and in its last bytes.
fn partition_logs(read: &mut Reader<'_>, bytes: u64, place: &str) -> Logs {
    let mut logs = Logs::default();
    match file_log(read, bytes) {
        Ok(StickLog::Whole(log)) => logs
            .usb
            .push((format!("{place}, {}", crate::efi::BOOT_LOG), log)),
        Ok(StickLog::Raw(log, error)) => {
            logs.stick_notes.push(format!(
                "{place}: {} has no usable header ({error}); its contents follow as they lie \
                 in the file, not in the order they were written",
                crate::efi::BOOT_LOG
            ));
            logs.usb.push((
                format!("{place}, {}, raw contents", crate::efi::BOOT_LOG),
                log,
            ));
        }
        // A volume that is no Stela stick has no log to miss.
        Err(error) if stick(read, bytes) => logs.stick_notes.push(format!(
            "{place} is a Stela stick whose {} could not be read: {error:#}",
            crate::efi::BOOT_LOG
        )),
        Err(_) => {}
    }
    if bytes >= boot_log::PARTITION_BYTES
        && let Ok(log) = state_log(read, 0)
    {
        logs.machine.push((place.to_owned(), log));
    }
    if let Ok(layout) = machine_state::split(bytes)
        && let Ok(log) = state_log(read, layout.state_offset())
    {
        logs.machine
            .push((format!("{place}, machine state in its last bytes"), log));
    }
    logs
}

/// A stick's own log file as it was read.
enum StickLog {
    Whole(Log),
    /// The file's bytes past a header that does not decode, zeros left out:
    /// the lines are there even when their order and boot count are not.
    Raw(Log, boot_log::HeaderError),
}

/// Whether a `bytes`-long partition is a Stela stick's FAT32 volume: it
/// holds the loader's options or the log file.
fn stick(read: &mut Reader<'_>, bytes: u64) -> bool {
    Fat32::open(Span { read, bytes }).is_ok_and(|mut volume| {
        volume.file(crate::efi::OPTIONS).is_ok() || volume.file(crate::efi::BOOT_LOG).is_ok()
    })
}

/// The USB's own log, [`crate::efi::BOOT_LOG`] on the FAT32 volume of a
/// `bytes`-long partition: the region the file's length gives
/// ([`boot_log::file_region`]), which `stela.efi` writes through its
/// clusters. A file no boot has written is refused; one whose header does
/// not decode is read raw.
fn file_log(read: &mut Reader<'_>, bytes: u64) -> Result<StickLog> {
    let mut volume = Fat32::open(Span { read, bytes }).map_err(|error| anyhow!("{error}"))?;
    let file = volume
        .file(crate::efi::BOOT_LOG)
        .map_err(|error| anyhow!("{error}"))?;
    let region_bytes = boot_log::file_region(file.bytes).with_context(|| {
        format!(
            "{} holds {} bytes, fewer than the log's {}",
            crate::efi::BOOT_LOG,
            file.bytes,
            boot_log::LOG_BYTES
        )
    })?;
    let mut span = volume.into_inner();
    let mut region = vec![0; usize::try_from(region_bytes)?];
    let mut done = 0;
    for extent in &file.extents {
        let take = usize::try_from(extent.bytes)?.min(region.len() - done);
        span.read_at(extent.offset, &mut region[done..done + take])?;
        done += take;
        if done == region.len() {
            break;
        }
    }
    ensure!(done == region.len(), "{} ends early", crate::efi::BOOT_LOG);
    match boot_log::read(&region) {
        Ok(log) => Ok(StickLog::Whole(log)),
        Err(error) => {
            let stream: Vec<u8> = region[boot_log::HEADER_BYTES as usize..]
                .iter()
                .copied()
                .filter(|&byte| byte != 0)
                .collect();
            if stream.is_empty() {
                bail!("{} has no boot log: {error}", crate::efi::BOOT_LOG);
            }
            Ok(StickLog::Raw(
                Log {
                    boots: 0,
                    head: stream.len() as u64,
                    ring_bytes: boot_log::RING_BYTES,
                    stream,
                    starts: Vec::new(),
                },
                error,
            ))
        }
    }
}

/// log header is there: one block is read before the whole region.
fn state_log(read: &mut Reader<'_>, state: u64) -> Result<Log> {
    let start = state + boot_log::OFFSET;
    let mut header = vec![0; boot_log::HEADER_BYTES as usize];
    read(start, &mut header)?;
    ensure!(
        header.starts_with(boot_log::MAGIC),
        "no boot log at {start:#x}"
    );
    let mut region = vec![0; boot_log::LOG_BYTES as usize];
    read(start, &mut region)?;
    boot_log::read(&region).with_context(|| format!("no boot log at {start:#x}"))
}

/// A whole disk as the person running the command knows it: its model,
/// size, device, and the folders or drive letters its volumes have.
pub(crate) fn describe(disk: &Disk) -> String {
    let device = if cfg!(windows) {
        format!(
            "PhysicalDrive{}",
            disk.identifier.trim_start_matches("disk")
        )
    } else {
        format!("/dev/{}", disk.identifier)
    };
    let mut text = match disk.name.trim() {
        "" => String::from("disk"),
        name => name.to_owned(),
    };
    let _ = write!(text, " ({}, {device}", host::size_text(disk.bytes));
    for mount in disk.partitions.iter().filter_map(|p| p.mount.as_ref()) {
        let _ = write!(text, ", {}", mount.display());
    }
    text.push(')');
    text
}

/// The boot logs on whole disk `disk`, read through `host`'s raw access to
/// its partitions, each placed by its partition. A disk none of whose
/// partitions can be read is refused with the first reason.
pub(crate) fn disk_logs(host: &dyn Host, disk: &Disk) -> Result<Logs> {
    let mut logs = Logs::default();
    let (mut refused, mut readable) = (None, false);
    for partition in &disk.partitions {
        let identifier = partition.identifier.as_str();
        let info = match host.partition(identifier) {
            Ok(info) => info,
            Err(error) => {
                logs.unread
                    .push(format!("could not read partition {identifier}: {error:#}"));
                continue;
            }
        };
        let mut read = |offset: u64, buffer: &mut [u8]| -> Result<()> {
            let bytes = host.read(identifier, offset, buffer.len())?;
            ensure!(bytes.len() == buffer.len(), "{identifier} read short");
            buffer.copy_from_slice(&bytes);
            Ok(())
        };
        if let Err(error) = read(0, &mut [0; 512]) {
            logs.unread
                .push(format!("could not read partition {identifier}: {error:#}"));
            refused.get_or_insert(error);
            continue;
        }
        readable = true;
        logs.extend(partition_logs(
            &mut read,
            info.bytes,
            &format!("partition {identifier}"),
        ));
    }
    match refused {
        Some(error) if !readable => Err(error),
        _ => Ok(logs),
    }
}

/// What [`scan_noted`] says when the logs it found lack the stick's own.
pub(crate) const NO_STICK_LOG: &str = "no log from a Stela stick was found. \
    These logs come only from the disk macOS is on, which an install writes and later boots do \
    not: they do not show a boot that failed after the install. Plug in the stick Stela booted \
    from and run this again.";

/// Every boot log on `host`'s disks ([`scan_noted`]), without its notes.
#[cfg(test)]
pub(crate) fn scan(host: &dyn Host) -> Result<Vec<(String, Log)>> {
    scan_noted(host).map(|(logs, _)| logs)
}

/// Every boot log on `host`'s disks, each placed by its disk and partition:
/// the disks that do not run this computer first and, when they hold none,
/// the system disks too. No log anywhere is refused with the disks looked
/// at and what to try. The notes say what the logs lack: the stick's own
/// log, which alone holds every boot, and each disk that could not be read.
pub(crate) fn scan_noted(host: &dyn Host) -> Result<Found> {
    let disks = host.disks()?;
    let (mut looked, mut unread) = (String::new(), String::new());
    let (mut notes, mut unread_parts) = (Vec::new(), Vec::new());
    for system in [false, true] {
        let mut logs = Vec::new();
        let mut stick = false;
        for disk in disks.iter().filter(|disk| disk.system == system) {
            let name = describe(disk);
            let _ = writeln!(looked, "  {name}");
            match disk_logs(host, disk) {
                Ok(mut found) => {
                    stick |= !found.usb.is_empty();
                    notes.extend(
                        found
                            .stick_notes
                            .drain(..)
                            .map(|note| format!("{name}: {note}")),
                    );
                    unread_parts
                        .extend(found.unread.drain(..).map(|note| format!("{name}: {note}")));
                    logs.extend(
                        found
                            .all()
                            .into_iter()
                            .map(|(place, log)| (format!("{name}: {place}"), log)),
                    );
                }
                Err(error) => {
                    let _ = writeln!(unread, "  {name}: {error:#}");
                    notes.push(format!("could not read {name}: {error:#}"));
                }
            }
        }
        if !logs.is_empty() {
            if !stick {
                notes.insert(0, NO_STICK_LOG.to_owned());
                notes.append(&mut unread_parts);
            }
            return Ok((logs, notes));
        }
    }
    let rights = if cfg!(windows) {
        format!("from {}", host::ADMIN)
    } else {
        format!("with {}", host::ADMIN)
    };
    let looked = if looked.is_empty() {
        String::from("This computer lists no disks.\n")
    } else {
        format!("Looked at:\n{looked}")
    };
    let unread = if unread.is_empty() {
        unread
    } else {
        format!("Could not read:\n{unread}")
    };
    bail!(
        "no Stela boot log on this computer's disks.\n{looked}{unread}Plug in the stick Stela \
         booted from and run this again {rights}."
    )
}

/// The boot log of the machine state that starts at byte `state` of `file`.
fn read(file: &mut File, state: u64) -> Result<Log> {
    let start = state + boot_log::OFFSET;
    let mut region = vec![0; boot_log::LOG_BYTES as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut region).with_context(|| {
        format!(
            "no boot log region at {start:#x}: the machine-state partition needs {:#x} bytes",
            boot_log::PARTITION_BYTES
        )
    })?;
    boot_log::read(&region).with_context(|| format!("no boot log at {start:#x}"))
}

/// What a log holds, one line: its boots, how much running time its lines
/// cover where they are stamped, and, once it has overwritten older lines,
/// the boots whose first lines it kept apart.
pub(crate) fn summary(log: &Log) -> String {
    let mut text = format!("Stela boot log: {} boots", log.boots);
    let seconds = covered_seconds(&log.stream);
    if seconds != 0 {
        let _ = write!(
            text,
            "; its lines cover {} of running",
            duration_text(seconds)
        );
    }
    let left_out = left_out_notes(&log.stream);
    if left_out != 0 {
        let _ = write!(
            text,
            "; {left_out} lines say where lines were left out of it (BOOT LOG THINNED, \
             [boot log dropped, BOOT LOG busy)"
        );
    }
    if log.overwritten() != 0 {
        text.push_str("; older lines were overwritten");
        let kept: Vec<u64> = log.kept_starts().map(|(start, _)| start.boot).collect();
        match kept[..] {
            [] => {}
            [boot] => {
                let _ = write!(text, ", the first lines of boot {boot} are kept");
            }
            [first, .., last] => {
                let _ = write!(
                    text,
                    ", the first lines of boots {first} to {last} are kept"
                );
            }
        }
    }
    text.push('\n');
    text
}

/// The lines of `stream` that say a boot left lines out of its log: a
/// family past its budget (`stela_boot_core::log_budget`), a buffer that
/// overflowed between stores (`boot_log::Pending`), or writes that found
/// the log held elsewhere.
fn left_out_notes(stream: &[u8]) -> usize {
    const NOTES: [&[u8]; 3] = [
        b"BOOT LOG THINNED ",
        b"[boot log dropped ",
        b"BOOT LOG busy: ",
    ];
    stream
        .split(|&byte| byte == b'\n')
        .filter(|line| {
            // After the line's stamp, if any.
            let text = match line.strip_prefix(b"[") {
                Some(rest) if stamp(line).is_some() => rest
                    .iter()
                    .position(|&byte| byte == b']')
                    .map_or(*line, |end| rest[end + 1..].trim_ascii_start()),
                _ => line,
            };
            NOTES.iter().any(|note| text.starts_with(note))
        })
        .count()
}

/// End every device skip the stick's boot log at `path` holds (`pc::device_starts`):
/// the functions an earlier boot did not finish starting, which later boots
/// skip until `by` (`stela repair` or `stela update`) clears them.
/// The functions it named, for the report; none when the log names none or
/// cannot be read, which leaves nothing to clear. A dry run changes nothing.
pub(crate) fn clear_device_skips(path: &Path, by: &str, dry_run: bool) -> Result<Vec<String>> {
    use stela_runtime::pc::device_starts;
    let Ok(mut bytes) = std::fs::read(path) else {
        return Ok(Vec::new());
    };
    let Ok(log) = boot_log::read(&bytes) else {
        return Ok(Vec::new());
    };
    let named: Vec<String> = device_starts::read(&log.stream)
        .iter()
        .map(|function| {
            format!(
                "PCI {} {:04x}:{:04x}",
                function.address, function.vendor, function.device
            )
        })
        .collect();
    if named.is_empty() || dry_run {
        return Ok(named);
    }
    let mut region = Region {
        identity: Identity {
            transport: Transport::VirtioBlk,
            location: path.display().to_string(),
            model: String::from("boot log"),
            serial: String::new(),
            blocks: bytes.len() as u64 / 512,
            block_size: 512,
            read_only: false,
            features: Default::default(),
        },
        bytes: &mut bytes,
    };
    let mut writer = boot_log::Writer::resume(&mut region)
        .map_err(|error| anyhow!("{}: {error}", path.display()))?;
    writer.append(format!("{} by {by}\n", device_starts::CLEARED).as_bytes());
    writer
        .flush(&mut region)
        .map_err(|error| anyhow!("{}: {error}", path.display()))?;
    // In place: the file keeps the clusters `stela.efi` writes on to.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to write", path.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(named)
}

/// A boot log file's bytes as 512-byte blocks.
struct Region<'a> {
    identity: Identity,
    bytes: &'a mut [u8],
}

impl BlockDevice for Region<'_> {
    fn identity(&self) -> &Identity {
        &self.identity
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.identity.check(lba, buffer.len())?;
        let at = (lba * 512) as usize;
        buffer.copy_from_slice(&self.bytes[at..at + buffer.len()]);
        Ok(())
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        self.identity.check(lba, data.len())?;
        let at = (lba * 512) as usize;
        self.bytes[at..at + data.len()].copy_from_slice(data);
        Ok(())
    }
}

/// A disk image read through 512- or 4096-byte blocks.
struct Image {
    file: File,
    identity: Identity,
}

impl BlockDevice for Image {
    fn identity(&self) -> &Identity {
        &self.identity
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.identity.check(lba, buffer.len())?;
        self.file
            .seek(SeekFrom::Start(lba * u64::from(self.identity.block_size)))
            .and_then(|_| self.file.read_exact(buffer))
            .map_err(|_| BlockError::Device { status: 0 })
    }

    fn write(&mut self, _: u64, _: &[u8]) -> Result<(), BlockError> {
        Err(BlockError::ReadOnly)
    }
}

/// The GPT of a whole-disk image or device and its block size. A device's
/// length is where it ends, as its metadata says none.
fn table(path: &Path) -> Result<(gpt::Table, u64)> {
    let bytes = File::open(path)?.seek(SeekFrom::End(0))?;
    let mut last = None;
    for block in [512u32, 4096] {
        let mut image = Image {
            file: File::open(path)?,
            identity: Identity {
                transport: Transport::VirtioBlk,
                location: path.display().to_string(),
                model: String::from("image"),
                serial: String::new(),
                blocks: bytes / u64::from(block),
                block_size: block,
                read_only: true,
                features: Default::default(),
            },
        };
        match gpt::read(&mut image) {
            Ok(table) => return Ok((table, u64::from(block))),
            Err(error) => last = Some(error),
        }
    }
    bail!(
        "{} holds no GPT ({}); pass --offset for a raw partition",
        path.display(),
        last.expect("two block sizes were tried")
    )
}

fn listing(table: &gpt::Table, block: u64) -> String {
    let mut text = String::new();
    for partition in &table.partitions {
        let _ = writeln!(
            text,
            "  {}  {:#x} bytes at {:#x}  {}",
            partition.unique_guid,
            partition.blocks() * block,
            partition.first_lba * block,
            partition.name
        );
    }
    text
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use stela_formats::payload::gpt::{Disk, Partition, Table, encode};
    use stela_runtime::pc::boot_log::Writer;

    /// A partition of a byte vector.
    struct Bytes<'a> {
        bytes: &'a mut [u8],
        identity: Identity,
    }

    impl BlockDevice for Bytes<'_> {
        fn identity(&self) -> &Identity {
            &self.identity
        }

        fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
            self.identity.check(lba, buffer.len())?;
            let at = (lba * 512) as usize;
            buffer.copy_from_slice(&self.bytes[at..at + buffer.len()]);
            Ok(())
        }

        fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
            self.identity.check(lba, data.len())?;
            let at = (lba * 512) as usize;
            self.bytes[at..at + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    /// A log region with one boot per entry of `boots`, each printing it.
    pub(crate) fn region(boots: &[&str]) -> Vec<u8> {
        let boots: Vec<Vec<u8>> = boots.iter().map(|line| line.as_bytes().to_vec()).collect();
        written(&boots)
    }

    /// A 4 MiB log region as this tree's writer leaves it, with one boot per
    /// entry of `boots`, each printing it and storing every 64 KiB.
    pub(crate) fn written(boots: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = vec![0u8; boot_log::LOG_BYTES as usize];
        let mut region = Bytes {
            bytes: &mut bytes,
            identity: Identity {
                transport: Transport::VirtioBlk,
                location: "test".into(),
                model: "RAM".into(),
                serial: String::new(),
                blocks: boot_log::LOG_BYTES / 512,
                block_size: 512,
                read_only: false,
                features: Default::default(),
            },
        };
        for lines in boots {
            let mut writer = Writer::open(&mut region).unwrap();
            for part in lines.chunks(64 << 10) {
                writer.append(part);
                writer.flush(&mut region).unwrap();
            }
        }
        bytes
    }

    /// `boots` boots of `lines` numbered lines each (`boot B line L`), from
    /// `first` on: a boot's own lines, without its marker.
    pub(crate) fn boot_lines(first: usize, boots: usize, lines: usize) -> Vec<Vec<u8>> {
        (first..first + boots)
            .map(|boot| {
                (0..lines)
                    .flat_map(|line| {
                        format!("[{line:5}.000] boot {boot} line {line:06}\n").into_bytes()
                    })
                    .collect()
            })
            .collect()
    }

    /// A 4 MiB log region as every released `stela.efi` (0.5.0 and older)
    /// left it: a version 1 header of 40 bytes and its SHA-256, and a ring
    /// that fills the region, holding the newest bytes of `boots`, each
    /// after its `=== STELA BOOT n ===` marker.
    pub(crate) fn released_region(boots: &[Vec<u8>]) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let mut stream = Vec::new();
        for (boot, lines) in boots.iter().enumerate() {
            stream.extend_from_slice(format!("=== STELA BOOT {} ===\n", boot + 1).as_bytes());
            stream.extend_from_slice(lines);
        }
        let mut bytes = vec![0u8; boot_log::LOG_BYTES as usize];
        let ring = boot_log::LOG_BYTES - boot_log::HEADER_BYTES;
        let head = stream.len() as u64;
        bytes[..8].copy_from_slice(boot_log::MAGIC);
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[16..24].copy_from_slice(&head.to_le_bytes());
        bytes[24..32].copy_from_slice(&(boots.len() as u64).to_le_bytes());
        bytes[32..40].copy_from_slice(&ring.to_le_bytes());
        let digest = Sha256::digest(&bytes[..40]);
        bytes[40..72].copy_from_slice(&digest);
        for at in head.saturating_sub(ring)..head {
            bytes[(boot_log::HEADER_BYTES + at % ring) as usize] = stream[at as usize];
        }
        bytes
    }

    /// What `stela log IMAGE` saves for the image of a stick whose EFI
    /// partition holds `log` as its log file: the command's own path, from
    /// the disk's GPT through the FAT32 volume to the file's clusters.
    pub(crate) fn stick_log_text(log: &[u8]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("stick.img");
        disk_image(&image, &[(ESP, &fat_volume(Some(log)))]);
        let output = dir.path().join("log.txt");
        BootLog {
            file: Some(image),
            output: Some(output.clone()),
            partition: None,
            offset: None,
            in_target: false,
        }
        .run()
        .unwrap();
        String::from_utf8(std::fs::read(output).unwrap()).unwrap()
    }

    const GUID: &str = "5E1A0000-0000-4000-8000-0000000000C0";

    /// A GPT image with a machine-state partition at 1 MiB, whose log has two
    /// boots when it holds one.
    fn image(path: &Path, partition_bytes: u64) {
        let mib = 1024 * 1024;
        let disk_bytes = (partition_bytes + 2 * mib).max(16 * mib);
        let blocks = disk_bytes / 512;
        let mut bytes = vec![0u8; disk_bytes as usize];
        let table = Table {
            disk_guid: Guid([7; 16]),
            slots: Default::default(),
            partitions: vec![Partition {
                type_guid: "0FC63DAF-8483-4772-8E79-3D69D8477DE4".parse().unwrap(),
                unique_guid: GUID.parse().unwrap(),
                first_lba: 2048,
                last_lba: 2048 + partition_bytes / 512 - 1,
                attributes: 0,
                name: "machine state".into(),
            }],
        };
        let disk = Disk {
            block_bytes: 512,
            blocks,
        };
        for write in encode(disk, &table).unwrap() {
            let at = (write.lba * 512) as usize;
            bytes[at..at + write.bytes.len()].copy_from_slice(&write.bytes);
        }
        if partition_bytes >= boot_log::PARTITION_BYTES {
            let start = (2048 * 512 + boot_log::OFFSET) as usize;
            bytes[start..start + boot_log::LOG_BYTES as usize]
                .copy_from_slice(&region(&["first boot\n", "second boot\n"]));
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn command(file: &Path, partition: Option<&str>, offset: Option<u64>) -> BootLog {
        BootLog {
            file: Some(file.into()),
            output: None,
            partition: partition.map(Into::into),
            offset,
            in_target: false,
        }
    }

    /// `stela repair` and `stela update` end every skip the stick's log
    /// holds, in place, and a dry run changes nothing.
    #[test]
    fn clearing_device_skips_ends_them_in_place() {
        use stela_runtime::pc::device_starts;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.bin");
        // A boot that stopped inside a network adapter's start.
        std::fs::write(
            &path,
            region(&["HOST DEVICE START pci=0000:05:00.0 id=144d:a808\n\
                 HOST DEVICE START pci=0000:07:00.0 id=10ec:8125\n"]),
        )
        .unwrap();
        let skips = |path: &Path| {
            let log = boot_log::read(&std::fs::read(path).unwrap()).unwrap();
            device_starts::read(&log.stream)
        };
        assert_eq!(skips(&path).iter().count(), 1);
        let named = clear_device_skips(&path, "stela repair", true).unwrap();
        assert_eq!(named, ["PCI 0000:07:00.0 10ec:8125"]);
        assert_eq!(skips(&path).iter().count(), 1);
        let named = clear_device_skips(&path, "stela repair", false).unwrap();
        assert_eq!(named, ["PCI 0000:07:00.0 10ec:8125"]);
        assert!(skips(&path).is_empty());
        let log = boot_log::read(&std::fs::read(&path).unwrap()).unwrap();
        let text = String::from_utf8(log.stream).unwrap();
        assert!(
            text.ends_with("HOST DEVICE SKIPS CLEARED by stela repair\n"),
            "{text}"
        );
        // Nothing left to clear, and no log at all, write nothing.
        assert!(
            clear_device_skips(&path, "stela update", false)
                .unwrap()
                .is_empty()
        );
        assert!(
            clear_device_skips(&dir.path().join("none"), "x", false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn gpt_log_selection_accepts_guid_or_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("machine-state.img");
        image(&path, boot_log::PARTITION_BYTES);
        let expected = "=== STELA BOOT 1 ===\nfirst boot\n=== STELA BOOT 2 ===\nsecond boot\n";
        let log = command(&path, Some(&GUID.to_lowercase()), None)
            .load(&path)
            .unwrap();
        assert_eq!(log.boots, 2);
        assert_eq!(String::from_utf8(log.stream.clone()).unwrap(), expected);
        assert_eq!(summary(&log), "Stela boot log: 2 boots\n");
        let raw = command(&path, None, Some(2048 * 512)).load(&path).unwrap();
        assert_eq!(raw, log);

        // Without a place the command finds the log itself.
        let (found, _) = find(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], (format!("partition {GUID}"), log.clone()));
        let error = format!(
            "{:#}",
            command(&path, Some("5E1A0000-0000-4000-8000-0000000000C1"), None)
                .load(&path)
                .unwrap_err()
        );
        assert!(error.contains("has no partition"), "{error}");
        // A region that holds no log is refused by name.
        let error = format!(
            "{:#}",
            command(&path, None, Some(0)).load(&path).unwrap_err()
        );
        assert!(error.contains("the log region is blank"), "{error}");
    }

    #[test]
    fn boot_log_reads_machine_state_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("region.img");
        // A 20 MiB partition keeps its machine state from 4 MiB.
        let partition_bytes = 20 * 1024 * 1024;
        image(&path, partition_bytes);
        let mut bytes = std::fs::read(&path).unwrap();
        let tail = machine_state::split(partition_bytes)
            .unwrap()
            .state_offset();
        assert_eq!(tail, 4 * 1024 * 1024);
        let at = (2048 * 512 + tail + boot_log::OFFSET) as usize;
        bytes[at..at + boot_log::LOG_BYTES as usize].copy_from_slice(&region(&["in the tail\n"]));
        std::fs::write(&path, &bytes).unwrap();
        let mut tail_log = command(&path, Some(GUID), None);
        tail_log.in_target = true;
        let log = tail_log.load(&path).unwrap();
        assert_eq!(
            String::from_utf8(log.stream).unwrap(),
            "=== STELA BOOT 1 ===\nin the tail\n"
        );
        // A raw partition ends with the file.
        let raw = dir.path().join("raw.img");
        std::fs::write(
            &raw,
            &bytes[2048 * 512..2048 * 512 + partition_bytes as usize],
        )
        .unwrap();
        let mut raw_log = command(&raw, None, Some(0));
        raw_log.in_target = true;
        assert_eq!(raw_log.load(&raw).unwrap().boots, 1);
        // A partition too small for a tail is named.
        let small = dir.path().join("small.img");
        image(&small, 2 * 1024 * 1024);
        let mut small_log = command(&small, Some(GUID), None);
        small_log.in_target = true;
        let error = format!("{:#}", small_log.load(&small).unwrap_err());
        assert!(
            error.contains("no machine state in its last bytes"),
            "{error}"
        );
    }

    #[test]
    fn boot_log_reports_undersized_partition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.img");
        image(&path, 2 * 1024 * 1024);
        let error = format!(
            "{:#}",
            command(&path, Some(GUID), None).load(&path).unwrap_err()
        );
        assert!(error.contains("boots it without one"), "{error}");
        // Without a place, no log anywhere is named with the GPT's partitions.
        let error = format!("{:#}", find(&path).unwrap_err());
        assert!(error.contains("holds no Stela boot log"), "{error}");
        assert!(error.contains(GUID), "{error}");
    }

    #[test]
    fn boot_log_finds_logs_at_a_partition_start_and_in_its_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("both.img");
        let partition_bytes = 20 * 1024 * 1024;
        image(&path, partition_bytes);
        let mut bytes = std::fs::read(&path).unwrap();
        let tail = machine_state::split(partition_bytes)
            .unwrap()
            .state_offset();
        let at = (2048 * 512 + tail + boot_log::OFFSET) as usize;
        bytes[at..at + boot_log::LOG_BYTES as usize].copy_from_slice(&region(&["in the tail\n"]));
        std::fs::write(&path, &bytes).unwrap();
        let (found, _) = find(&path).unwrap();
        let places: Vec<_> = found
            .iter()
            .map(|(place, log)| (place.as_str(), log.boots))
            .collect();
        assert_eq!(
            places,
            [
                (format!("partition {GUID}").as_str(), 2),
                (
                    format!("partition {GUID}, machine state in its last bytes").as_str(),
                    1
                ),
            ]
        );
        // A raw partition without a GPT is searched as one.
        let raw = dir.path().join("raw.img");
        std::fs::write(
            &raw,
            &bytes[2048 * 512..2048 * 512 + partition_bytes as usize],
        )
        .unwrap();
        let (found, _) = find(&raw).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "the file");
    }

    /// A 40 MiB FAT32 volume as setup's EFI partition holds it:
    /// `EFI/STELA/BOOT.LOG` holds `log` when given, in a contiguous chain.
    pub(crate) fn fat_volume(log: Option<&[u8]>) -> Vec<u8> {
        use stela_runtime::pc::fat32::format::{
            END_OF_CHAIN, FATS, Geometry, MEDIA_ENTRY, volume_label,
        };
        const DIRECTORY: u8 = 0x10;
        const ARCHIVE: u8 = 0x20;
        let sectors = 40 * 2048;
        let geometry = Geometry::new(sectors, 512).unwrap();
        let mut bytes = vec![0u8; (sectors * 512) as usize];
        let boot = geometry
            .boot_sector(0, &volume_label("EFI").unwrap(), 1)
            .unwrap();
        bytes[..512].copy_from_slice(&boot);
        let set = |bytes: &mut [u8], cluster: u32, value: u32| {
            for copy in 0..FATS {
                let at = (geometry.fat_sector(copy) * 512 + u64::from(cluster) * 4) as usize;
                bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
        };
        set(&mut bytes, 0, MEDIA_ENTRY);
        for cluster in 1..=4 {
            set(&mut bytes, cluster, END_OF_CHAIN);
        }
        let entry = |bytes: &mut [u8],
                     directory: u32,
                     name: &[u8; 11],
                     attributes: u8,
                     first: u32,
                     size: u32| {
            let at = geometry.cluster_offset(directory) as usize;
            let entry = &mut bytes[at..at + 32];
            entry[..11].copy_from_slice(name);
            entry[11] = attributes;
            entry[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
            entry[26..28].copy_from_slice(&(first as u16).to_le_bytes());
            entry[28..32].copy_from_slice(&size.to_le_bytes());
        };
        entry(&mut bytes, 2, b"EFI        ", DIRECTORY, 3, 0);
        entry(&mut bytes, 3, b"STELA      ", DIRECTORY, 4, 0);
        if let Some(log) = log {
            let clusters = log.len().div_ceil(geometry.cluster_bytes() as usize) as u32;
            for cluster in 5..5 + clusters {
                let next = if cluster + 1 == 5 + clusters {
                    END_OF_CHAIN
                } else {
                    cluster + 1
                };
                set(&mut bytes, cluster, next);
            }
            entry(&mut bytes, 4, b"BOOT    LOG", ARCHIVE, 5, log.len() as u32);
            let at = geometry.cluster_offset(5) as usize;
            bytes[at..at + log.len()].copy_from_slice(log);
        }
        bytes
    }

    /// A whole-disk image of `partitions`, each a type and its bytes, laid
    /// out from 1 MiB.
    pub(crate) fn disk_image(path: &Path, partitions: &[(&str, &[u8])]) {
        let mut first = 2048u64;
        let mut table = Table {
            disk_guid: Guid([9; 16]),
            slots: Default::default(),
            partitions: Vec::new(),
        };
        let mut writes = Vec::new();
        for (index, (kind, bytes)) in partitions.iter().enumerate() {
            let blocks = bytes.len() as u64 / 512;
            table.partitions.push(Partition {
                type_guid: kind.parse().unwrap(),
                unique_guid: format!("5E1A0000-0000-4000-8000-00000000000{index}")
                    .parse()
                    .unwrap(),
                first_lba: first,
                last_lba: first + blocks - 1,
                attributes: 0,
                name: String::new(),
            });
            writes.push((first * 512, *bytes));
            first += blocks;
        }
        let disk = Disk {
            block_bytes: 512,
            blocks: first + 2048,
        };
        let mut image = vec![0u8; (disk.blocks * 512) as usize];
        for write in encode(disk, &table).unwrap() {
            let at = (write.lba * 512) as usize;
            image[at..at + write.bytes.len()].copy_from_slice(&write.bytes);
        }
        for (at, bytes) in writes {
            image[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
        }
        std::fs::write(path, image).unwrap();
    }

    pub(crate) const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    const LINUX: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

    #[test]
    fn boot_log_reads_the_usb_s_own_file_before_the_machine_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usb.img");
        let esp = fat_volume(Some(&region(&["CHOICE 3 (Install macOS)\n"])));
        let mut state = vec![0u8; boot_log::PARTITION_BYTES as usize];
        state[boot_log::OFFSET as usize..].copy_from_slice(&region(&["NOR persistent\n"]));
        disk_image(&path, &[(LINUX, &state), (ESP, &esp)]);
        let (found, _) = find(&path).unwrap();
        let places: Vec<_> = found
            .iter()
            .map(|(place, log)| (place.as_str(), log.boots))
            .collect();
        assert_eq!(
            places,
            [
                (
                    "partition 5E1A0000-0000-4000-8000-000000000001, EFI/stela/boot.log",
                    1
                ),
                ("partition 5E1A0000-0000-4000-8000-000000000000", 1),
            ]
        );
        assert_eq!(
            found[0].1.stream,
            b"=== STELA BOOT 1 ===\nCHOICE 3 (Install macOS)\n"
        );
        // A raw EFI partition, and a stick whose log file was never made.
        let raw = dir.path().join("esp.img");
        std::fs::write(&raw, &esp).unwrap();
        assert_eq!(find(&raw).unwrap().0[0].0, "the file, EFI/stela/boot.log");
        let bare = dir.path().join("bare.img");
        disk_image(&bare, &[(ESP, &fat_volume(None))]);
        let error = format!("{:#}", find(&bare).unwrap_err());
        assert!(
            error.contains("no EFI/stela/boot.log on a FAT32 volume"),
            "{error}"
        );
    }

    /// A stick whose log file `stela` made 32 MiB long is read whole, and
    /// a copy of that file too.
    #[test]
    fn boot_log_reads_a_long_log_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let old = region(&["boot one\n", "boot two\n"]);
        let long = boot_log::relayout(&old, boot_log::FILE_BYTES).unwrap();
        let path = dir.path().join("usb.img");
        disk_image(&path, &[(ESP, &fat_volume(Some(&long)))]);
        let (found, notes) = find(&path).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        let [(_, log)] = &found[..] else {
            panic!("{}", found.len());
        };
        assert_eq!(
            log.ring_bytes,
            boot_log::Layout::of(boot_log::FILE_BYTES).ring_bytes
        );
        assert_eq!(
            log.stream,
            b"=== STELA BOOT 1 ===\nboot one\n=== STELA BOOT 2 ===\nboot two\n"
        );
        let copy = dir.path().join("boot.log");
        std::fs::write(&copy, &long).unwrap();
        let (found, _) = find(&copy).unwrap();
        assert_eq!(found[0].1.stream, log.stream);
    }

    /// A stick put back to a released Stela after `stela` made its log
    /// 32 MiB, and booted by it: that loader starts its own 4 MiB log at
    /// the file's start. `stela log` lists the boots from before it, which
    /// the header's copy still names, then its boots, numbered on.
    #[test]
    fn stela_log_lists_the_boots_from_before_an_older_loader_s_log() {
        let newer = written(&boot_lines(1, 5, 2_000));
        let long = boot_log::relayout(&newer, boot_log::FILE_BYTES).unwrap();
        let before = boot_log::read(&long).unwrap();
        let older = released_region(&boot_lines(100, 2, 500));
        let mut file = long.clone();
        // The released loader's header block and the ring it reached.
        let reached = boot_log::read(&older).unwrap().head.next_multiple_of(512) as usize;
        let ring = boot_log::HEADER_BYTES as usize;
        file[..512].copy_from_slice(&older[..512]);
        file[ring..ring + reached].copy_from_slice(&older[ring..ring + reached]);
        let text = stick_log_text(&file);
        assert!(
            text.starts_with("=== Stela boot log: 7 boots"),
            "{}",
            &text[..200]
        );
        for boot in 1..=7 {
            let marker = format!("=== STELA BOOT {boot} ===\n");
            assert!(text.contains(&marker), "{marker}");
        }
        // The boots from before keep their first lines; the older loader's
        // lines follow them.
        for start in &before.starts {
            assert!(text.contains(std::str::from_utf8(&start.first).unwrap()));
        }
        let six = text
            .find("=== STELA BOOT 6 ===\n[    0.000] boot 100 line 000000\n")
            .unwrap();
        assert!(text.find("boot 5 line 001999\n").unwrap() < six);
        assert!(text.ends_with("boot 101 line 000499\n"));
    }

    #[test]
    fn a_stick_log_that_cannot_be_read_whole_is_said_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        // A header that no longer decodes is read from its copy.
        let mut torn = region(&["CHOICE 1 (macOS)\n"]);
        torn[16] ^= 0xff;
        let raw = dir.path().join("torn.img");
        std::fs::write(&raw, fat_volume(Some(&torn))).unwrap();
        let (found, notes) = find(&raw).unwrap();
        assert_eq!(found[0].0, "the file, EFI/stela/boot.log");
        assert!(notes.is_empty(), "{notes:?}");
        // Without its copy, the lines are read as they lie.
        torn[boot_log::COPY_AT as usize + 16] ^= 0xff;
        std::fs::write(&raw, fat_volume(Some(&torn))).unwrap();
        let (found, notes) = find(&raw).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "the file, EFI/stela/boot.log, raw contents");
        assert_eq!(
            found[0].1.stream,
            b"=== STELA BOOT 1 ===\nCHOICE 1 (macOS)\n"
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("has no usable header (the log header's checksum does not match)"),
            "{notes:?}"
        );
        // A log file no boot wrote: nothing to show, and the stick is named.
        let blank = fat_volume(Some(&vec![0u8; boot_log::LOG_BYTES as usize]));
        let mut read = |offset: u64, buffer: &mut [u8]| -> Result<()> {
            buffer.copy_from_slice(&blank[offset as usize..offset as usize + buffer.len()]);
            Ok(())
        };
        let logs = partition_logs(&mut read, blank.len() as u64, "partition sdb1");
        assert!(logs.usb.is_empty());
        assert_eq!(logs.stick_notes.len(), 1, "{:?}", logs.stick_notes);
        assert!(
            logs.stick_notes[0].starts_with(
                "partition sdb1 is a Stela stick whose EFI/stela/boot.log could not be read: "
            ) && logs.stick_notes[0].ends_with("the log region is blank"),
            "{:?}",
            logs.stick_notes
        );
        // A FAT32 volume that is no stick is passed over without a word.
        let mut other = fat_volume(None);
        let at = other.windows(11).position(|w| w == b"STELA      ").unwrap();
        other[at..at + 11].copy_from_slice(b"OTHER      ");
        let mut read = |offset: u64, buffer: &mut [u8]| -> Result<()> {
            buffer.copy_from_slice(&other[offset as usize..offset as usize + buffer.len()]);
            Ok(())
        };
        let logs = partition_logs(&mut read, other.len() as u64, "partition sdc1");
        assert!(logs.is_empty() && logs.stick_notes.is_empty());
    }

    #[test]
    fn boot_log_reads_a_copied_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot.log");
        std::fs::write(&path, region(&["first\n", "second\n"])).unwrap();
        let (found, _) = find(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "the file");
        assert_eq!(found[0].1.boots, 2);
        // A blank copy holds nothing to print.
        std::fs::write(&path, vec![0u8; boot_log::LOG_BYTES as usize]).unwrap();
        assert!(find(&path).is_err());
    }

    #[test]
    fn boot_log_output_holds_the_streams_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("log.txt");
        let mut log = boot_log::read(&region(&["caf\u{e9} \x01\n"])).unwrap();
        log.stream.push(0xff);
        let logs = [
            (String::from("USB"), log.clone()),
            (String::from("disk"), log),
        ];
        write_logs(&logs, true, &[String::from("first\nnote")], Some(&output)).unwrap();
        let mut expected =
            b"=== NOTE: first note ===\n=== USB ===\n=== Stela boot log: 1 boots ===\n".to_vec();
        expected.extend_from_slice(&logs[0].1.stream);
        expected.extend_from_slice(b"=== disk ===\n=== Stela boot log: 1 boots ===\n");
        expected.extend_from_slice(&logs[1].1.stream);
        assert_eq!(std::fs::read(&output).unwrap(), expected);
        let said = saved(2, &output, false);
        assert!(said.starts_with("Saved 2 boot logs to "), "{said}");
        assert!(said.ends_with("log.txt"), "{said}");
        // A log.txt that was there is said to be replaced.
        let said = saved(1, &output, true);
        assert!(
            said.ends_with("log.txt, replacing the file that was there"),
            "{said}"
        );
        let error = write_logs(&logs, false, &[], Some(&dir.path().join("no/such/dir")));
        assert!(format!("{:#}", error.unwrap_err()).contains("cannot create"));

        // Mount folders name the account; the written log has a tag there.
        let place = "SanDisk (32 GB, /dev/sdb, /media/someone/STELA): USB";
        let logs = [(String::from(place), logs[0].1.clone())];
        let note = String::from("could not read Disk (1 TB, /dev/sda, /home/someone)");
        write_logs(&logs, true, &[note], Some(&output)).unwrap();
        let text = String::from_utf8_lossy(&std::fs::read(&output).unwrap()).into_owned();
        assert!(!text.contains("someone"), "{text}");
        let tag = stela_contracts::masked::Name("someone").to_string();
        assert!(
            text.contains(&format!("/media/{tag}/STELA): USB ===")),
            "{text}"
        );
        assert!(text.contains(&format!("/home/{tag}) ===")), "{text}");

        // Lines an older boot wrote with whole serials and MAC addresses
        // are written masked, as a boot now writes them; the rest as they
        // were.
        let old = "STORAGE DEVICE Nvme at 0000:01:00.0 serial=\"S4EWNX0R123456Z\" model=\"SSD\"\n\
                   NETWORK candidate driver=r8169 adapter=0000:03:00.0 mac=00:e0:4c:12:34:5e\n\
                   HOST DISK IO serial=..456Z reads=3\n";
        let logs = [(String::new(), boot_log::read(&region(&[old])).unwrap())];
        write_logs(&logs, false, &[], Some(&output)).unwrap();
        let text = String::from_utf8(std::fs::read(&output).unwrap()).unwrap();
        assert!(!text.contains("S4EWNX0R123456Z"), "{text}");
        assert!(!text.contains("12:34:5e"), "{text}");
        assert!(
            text.ends_with(
                "STORAGE DEVICE Nvme at 0000:01:00.0 serial=\"..456Z\" model=\"SSD\"\n\
                 NETWORK candidate driver=r8169 adapter=0000:03:00.0 mac=00:e0:4c:..:..:5e\n\
                 HOST DISK IO serial=..456Z reads=3\n"
            ),
            "{text}"
        );
    }

    /// A boot that ran until the log overwrote its own first lines: they
    /// are written first, from the boot's record, and the summary says how
    /// much running the lines cover.
    #[test]
    fn a_long_boot_s_first_lines_are_written_before_the_log() {
        let mut bytes = vec![0u8; boot_log::LOG_BYTES as usize];
        let mut region = Bytes {
            bytes: &mut bytes,
            identity: Identity {
                transport: Transport::VirtioBlk,
                location: "test".into(),
                model: "RAM".into(),
                serial: String::new(),
                blocks: boot_log::LOG_BYTES / 512,
                block_size: 512,
                read_only: false,
                features: Default::default(),
            },
        };
        let mut writer = Writer::open(&mut region).unwrap();
        writer.append(b"[    1.000] HOST version=0.5.0 cpu=test\n");
        // A line every 100 ms: 36 bytes each, more than the ring holds.
        let lines = boot_log::RING_BYTES / 36 + 5000;
        for line in 0..lines {
            let millis = 2000 + line * 100;
            writer.append(
                format!(
                    "[{:5}.{:03}] BEAT blocks={line:010}\n",
                    millis / 1000,
                    millis % 1000
                )
                .as_bytes(),
            );
            if line % 2000 == 0 {
                writer.flush(&mut region).unwrap();
            }
        }
        writer.flush(&mut region).unwrap();
        drop(writer);
        let mut next = Writer::open(&mut region).unwrap();
        next.append(b"[    0.500] HOST version=0.5.0 cpu=test\n");
        next.flush(&mut region).unwrap();
        let log = boot_log::read(&bytes).unwrap();
        assert!(log.overwritten() > 0);
        assert!(!String::from_utf8_lossy(&log.stream[..40_000]).contains("HOST version"));
        // The ring holds some 87,000 lines of boot 1, 100 ms apart.
        let seconds = covered_seconds(&log.stream);
        assert!((8000..9500).contains(&seconds), "{seconds}");
        let said = summary(&log);
        let expected = format!(
            "Stela boot log: 2 boots; its lines cover {} of running; older lines were \
             overwritten, the first lines of boot 1 are kept\n",
            duration_text(seconds)
        );
        assert_eq!(said, expected);
        // Lines that say others were left out are counted.
        let notes = b"[   1.000] BOOT LOG THINNED \"GUEST|\": 3 lines\n[boot log dropped 12 bytes]\nGUEST| BOOT LOG THINNED\n";
        assert_eq!(left_out_notes(notes), 2);
        let mut written = Vec::new();
        write_log(&mut written, &log).unwrap();
        let text = String::from_utf8(written).unwrap();
        let first = "=== BOOT 1: its first lines, kept apart; the log has since overwritten \
                     them ===\n=== STELA BOOT 1 ===\n[    1.000] HOST version=0.5.0 cpu=test\n";
        assert!(text.starts_with(first), "{}", &text[..200]);
        let goes_on = "=== BOOT 1: lines were overwritten here; the log below goes on in this \
                       boot ===\n=== THE LOG, every line it still holds, oldest first ===\n";
        let at = text.find(goes_on).unwrap();
        assert!(at >= boot_log::FIRST_BYTES as usize - 100);
        assert_eq!(&text.as_bytes()[at + goes_on.len()..], &log.stream[..]);
        assert!(text.ends_with("=== STELA BOOT 2 ===\n[    0.500] HOST version=0.5.0 cpu=test\n"));

        assert_eq!(stamp(b"[12345.678] x"), Some(12_345_678));
        assert_eq!(stamp(b"[    0.007] x"), Some(7));
        assert_eq!(stamp(b"=== STELA BOOT 3 ==="), None);
        assert_eq!(stamp(b"[boot log dropped 4 bytes]"), None);
        assert_eq!(
            covered_seconds(b"[   10.000] a\n[   70.000] b\n=== STELA BOOT 2 ===\nx\n[    5.000] c\n[   35.500] d\n"),
            90
        );
        assert_eq!(
            [duration_text(35), duration_text(2940), duration_text(7200)],
            ["35 s", "49 min", "2 h"]
        );
    }
}
