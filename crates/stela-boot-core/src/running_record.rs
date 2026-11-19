//! The record of a boot that ended without a stop. A stop leaves its
//! reason in the log, or in the firmware where the log is out of reach
//! ([`crate::stop_record`]). A boot that hangs and is switched off, or that
//! the PC resets, leaves neither, and the next start cannot tell it from
//! one that was shut down: the person sees no text and the log no end.
//!
//! So a boot writes one small firmware variable, `StelaRunning`, just
//! before it asks the firmware to end its boot services: the boot's number
//! in the USB's log, the log's partition, and the time. That is the only
//! write: one per boot, made while the firmware's boot services still run,
//! and none after them, when a variable call can fault in some firmware
//! and would then stop a boot that was going on. The variable is never
//! deleted; the next boot's write replaces it.
//!
//! The next start on the PC reads it ([`Running::decode`]) and decides
//! from the log ([`unfinished`]): the record names the boot before this
//! one in this USB's log, no stop record was carried for it, and its last
//! lines hold none of the lines every stop, restart and power-off writes
//! ([`ENDS`]). Then it says so in the log ([`Unfinished::line`]) and on
//! the screen ([`Unfinished::notice`]), with the last step the log shows
//! running, how long, and whether the step watchdog was on.
//!
//! The step is the last one the log holds. From the firmware's exit until
//! Stela's own driver writes the stick again, lines wait in memory: a boot
//! that ends there shows the step before the exit, and the line says that
//! later lines were not kept.
//!
//! | Offset | Bytes | Field |
//! | --- | --- | --- |
//! | 0 | 8 | magic `STELARUN` |
//! | 8 | 4 | version, 1 |
//! | 12 | 4 | CRC-32 of bytes 16..48 |
//! | 16 | 8 | the boot's number in the USB's log |
//! | 24 | 16 | the GUID of the log's partition |
//! | 40 | 4 | seconds since the PC started, at the write |
//! | 44 | 4 | flags: [`VARIABLE_FAULT`], [`VARIABLE_TRIAL`]; bits 16-31 the faulting version's [`build_tag`] |
//!
//! A boot on a PC whose runtime variable service faulted after the
//! firmware's exit sets [`VARIABLE_FAULT`] with the faulting version's tag
//! ([`variable_service`]): each fault can leave the firmware's variable
//! store half written, so on that PC that version calls the service only
//! while boot services run (`VariableService::Avoided`). Another version,
//! whose mapping of the firmware's runtime regions may differ, tries it
//! after the exit again (`VariableService::Tried`, [`VARIABLE_TRIAL`]): once
//! a try's call after the exit answers, as the try's log says, the flag is
//! cleared; a try that faults keeps the flag, now with its own version's tag;
//! a try that made no such call is tried again. So a PC that only an
//! earlier version's mapping faulted returns to normal after one restart,
//! and one whose firmware faults again keeps the service before the exit
//! for that version, and faults at most once in each later version. Older
//! versions, which want the word zero, read such a record as none.
use alloc::{format, string::String};
use stela_contracts::crc::crc32_ieee;

/// The variable's name, under `stop_record::VENDOR`.
pub const NAME: [u16; 13] = {
    let text = b"StelaRunning";
    let mut name = [0; 13];
    let mut at = 0;
    while at < text.len() {
        name[at] = text[at] as u16;
        at += 1;
    }
    name
};
/// `EFI_VARIABLE_NON_VOLATILE | BOOTSERVICE_ACCESS`: kept across a
/// restart; written and read only while boot services run.
pub const ATTRIBUTES: u32 = 0x1 | 0x2;
pub const BYTES: usize = 48;
/// What the variable takes of the firmware's store, with its name and the
/// firmware's own header, and as much again: firmware writes the new value
/// before it frees the old one.
pub const STORE_BYTES: u64 = 2 * (BYTES as u64 + 128);
/// Firmware variable writes a boot makes for this record.
pub const WRITES_PER_BOOT: u32 = 1;
const MAGIC: &[u8; 8] = b"STELARUN";
const VERSION: u32 = 1;

/// A boot that reached the firmware's exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Running {
    /// Its number in the USB's log.
    pub boot: u64,
    /// The GUID of the partition that holds that log.
    pub log: [u8; 16],
    /// Seconds since the PC started when it was written.
    pub seconds: u32,
    /// [`VARIABLE_FAULT`] and [`VARIABLE_TRIAL`] with a [`build_tag`]
    /// above them, or none.
    pub flags: u32,
}

/// The firmware's runtime variable service faulted on this PC after the
/// firmware's exit, in the boot the record names or an earlier one, under
/// the version whose [`build_tag`] the flags' high half holds.
pub const VARIABLE_FAULT: u32 = 1;
/// The boot the record names called the service after the exit again,
/// though it had faulted under another version (`VariableService::Tried`).
pub const VARIABLE_TRIAL: u32 = 2;
/// The flags' bits for the faulting version's tag.
const TAG_SHIFT: u32 = 16;

/// A version's tag in the record: 16 bits of its string's CRC-32, never 0,
/// which stands for a version the log did not name.
pub fn build_tag(version: &str) -> u32 {
    (crc32_ieee(version.as_bytes()) & 0xffff).max(1)
}

/// How a boot calls the firmware's runtime variable service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VariableService {
    /// It never faulted on this PC: after the exit as a boot needs it.
    Trusted,
    /// It faulted on this PC under this version: only while boot services
    /// run.
    Avoided,
    /// It faulted on this PC under another version: after the exit as a
    /// boot needs it, to see whether this version's calls answer.
    Tried,
}

impl Running {
    pub fn encode(&self) -> [u8; BYTES] {
        let mut bytes = [0; BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.boot.to_le_bytes());
        bytes[24..40].copy_from_slice(&self.log);
        bytes[40..44].copy_from_slice(&self.seconds.to_le_bytes());
        bytes[44..48].copy_from_slice(&self.flags.to_le_bytes());
        let checksum = crc32_ieee(&bytes[16..]);
        bytes[12..16].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }

    /// The record in a variable's value; `None` for any other bytes, which
    /// are never read as a boot.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        if bytes.len() != BYTES
            || &bytes[..8] != MAGIC
            || word(8) != VERSION
            || word(12) != crc32_ieee(&bytes[16..])
            || word(44) & !(VARIABLE_FAULT | VARIABLE_TRIAL | u32::MAX << TAG_SHIFT) != 0
        {
            return None;
        }
        Some(Self {
            boot: u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes")),
            log: bytes[24..40].try_into().expect("16 bytes"),
            seconds: word(40),
            flags: word(44),
        })
    }
}

/// Whether the firmware's store, with `remaining` bytes free, takes the
/// record and keeps `stop_record::STORE_RESERVE` free after it. Some
/// firmware fails once its store fills.
pub const fn store_takes(remaining: u64) -> bool {
    remaining >= STORE_BYTES + crate::stop_record::STORE_RESERVE
}

/// What a boot's last lines hold when it ended by a stop, a restart or a
/// power-off: one of these is in every such end.
pub const ENDS: [&str; 6] = [
    "STELA FATAL",
    "STELA PANIC",
    "STELA STOP",
    "STELA RESTART",
    "STELA POWER-OFF",
    "POWER powering off",
];
/// The line that starts a boot in the log.
const BOOT_MARKER: &str = "=== STELA BOOT ";
/// The line Stela's own driver writes when it has the stick's log again.
const LOG_RESUMED: &str = "through the USB driver";
/// The most of the log's newest bytes the decision reads.
pub const TAIL_BYTES: usize = 256 << 10;

/// Whether `newest`, the log's newest bytes, answer the question: they
/// hold an end's line or reach back to the start of the last boot, or are
/// all the decision reads.
pub fn enough(newest: &[u8]) -> bool {
    let text = String::from_utf8_lossy(newest);
    newest.len() >= TAIL_BYTES
        || text.contains(BOOT_MARKER)
        || ENDS.iter().any(|end| text.contains(end))
}

/// How boot `this_boot` of the log on partition `this_log`, of `version`,
/// calls the firmware's variable service, and the flags its record keeps,
/// from the record `running` the boot before wrote and `newest`, the log's
/// newest lines from before this boot's own:
///
/// - The boot `running` names is the one before this one in this log and
///   its lines show a fault inside the service: flagged with the tag of the
///   version its marker names.
/// - That boot tried the service after the exit ([`VARIABLE_TRIAL`]) and
///   its lines show one of its calls there answered: no longer flagged.
/// - Otherwise the record's flag holds, on any USB, after any boot.
///
/// A flag of this version's tag avoids the service after the exit; one of
/// another's tries it.
pub fn variable_service(
    running: &Running,
    this_boot: u64,
    this_log: &[u8; 16],
    newest: &[u8],
    version: &str,
) -> (VariableService, u32) {
    let mut faulted = (running.flags & VARIABLE_FAULT != 0).then_some(running.flags >> TAG_SHIFT);
    if running.log == *this_log && running.boot.checked_add(1) == Some(this_boot) {
        let text = String::from_utf8_lossy(newest);
        let lines = text.rfind(BOOT_MARKER).map_or(&text[..], |at| &text[at..]);
        let said = |words: &[&str]| {
            lines.lines().any(|line| {
                let line = stamped(line).1;
                words.iter().any(|words| line.starts_with(words))
            })
        };
        let fault = lines.lines().any(|line| {
            stamped(line)
                .1
                .strip_prefix("STELA FATAL ")
                .is_some_and(|reason| reason.starts_with(crate::stop_record::FAULT_WORDS))
        });
        if fault {
            // `=== STELA BOOT 12 === version=0.5.1 ...`
            let tag = lines
                .strip_prefix(BOOT_MARKER)
                .and_then(|marker| marker.lines().next())
                .and_then(|marker| {
                    marker
                        .split(' ')
                        .find_map(|word| word.strip_prefix("version="))
                })
                .map_or(0, build_tag);
            faulted = Some(tag);
        } else if running.flags & VARIABLE_TRIAL != 0 && said(&ANSWERED) {
            faulted = None;
        }
    }
    match faulted {
        None => (VariableService::Trusted, 0),
        Some(tag) if tag == build_tag(version) => {
            (VariableService::Avoided, VARIABLE_FAULT | tag << TAG_SHIFT)
        }
        Some(tag) => (
            VariableService::Tried,
            VARIABLE_FAULT | VARIABLE_TRIAL | tag << TAG_SHIFT,
        ),
    }
}

/// The lines a call of the variable service after the firmware's exit
/// writes when it answered, as it did or refused (`stop_record`'s
/// `return_at_restart`).
const ANSWERED: [&str; 2] = [
    "POWER the firmware starts its entry Boot",
    "POWER the firmware chooses what starts after the restart: it refused BootNext",
];

/// The step watchdog of the boot that ended, as its lines say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watchdog {
    On,
    /// The other processors watched the boot processor's steps.
    Processors,
    Unavailable,
    /// No line of it in the lines read.
    Unknown,
}

impl Watchdog {
    pub const fn name(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Processors => "on (the other processors)",
            Self::Unavailable => "unavailable",
            Self::Unknown => "unknown",
        }
    }
}

/// A boot that ended without a stop, as the log shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unfinished {
    pub boot: u64,
    /// The last step the log shows running; empty where it shows none.
    pub stage: String,
    /// The step had ended and no other had begun.
    pub after: bool,
    /// The last detail of the step that the log shows ended.
    pub detail: String,
    pub watchdog: Watchdog,
    /// Seconds from the step's start to the log's last line.
    pub seconds: Option<u64>,
    /// The log holds no line from after the firmware's exit: the boot went
    /// on from the stage for a time the log does not show.
    pub unkept: bool,
}

/// The milliseconds a log line's stamp gives, `[  12.345] `, and the rest.
fn stamped(line: &str) -> (Option<u64>, &str) {
    let parse = || {
        let inner = line.strip_prefix('[')?;
        let (stamp, rest) = inner.split_once("] ")?;
        let (seconds, millis) = stamp.trim().split_once('.')?;
        let at = seconds.parse::<u64>().ok()? * 1000 + millis.parse::<u64>().ok()?;
        Some((at, rest))
    };
    match parse() {
        Some((at, rest)) => (Some(at), rest),
        None => (None, line),
    }
}

/// Decide whether the boot `running` names ended without a stop, at the
/// start of boot `this_boot` of the log on partition `this_log`:
/// `carried` is the boot of a stop record this start copied from the
/// firmware, and `newest` the log's newest lines from before this boot's
/// own ([`enough`]). `None` where the record is another USB's or an older
/// boot's, which an earlier start told, or where the boot's end is in the
/// log or the firmware.
pub fn unfinished(
    running: &Running,
    this_boot: u64,
    this_log: &[u8; 16],
    carried: Option<u64>,
    newest: &[u8],
) -> Option<Unfinished> {
    if running.log != *this_log
        || running.boot.checked_add(1) != Some(this_boot)
        || carried == Some(running.boot)
    {
        return None;
    }
    let text = String::from_utf8_lossy(newest);
    // The last boot's lines: all that was read where its start is older.
    let (whole, lines) = match text.rfind(BOOT_MARKER) {
        Some(at) => (true, &text[at..]),
        None => (false, &text[..]),
    };
    if ENDS.iter().any(|end| lines.contains(end)) {
        return None;
    }
    let mut found = Unfinished {
        boot: running.boot,
        stage: String::new(),
        after: false,
        detail: String::new(),
        watchdog: Watchdog::Unknown,
        seconds: None,
        unkept: whole && !lines.contains(LOG_RESUMED),
    };
    let (mut began, mut last) = (None, None);
    for line in lines.lines() {
        let (at, line) = stamped(line);
        last = at.or(last);
        let Some(step) = line.strip_prefix("STEP ") else {
            continue;
        };
        if let Some(watchdog) = step.strip_prefix("WATCHDOG ") {
            found.watchdog = if watchdog.starts_with("on") {
                Watchdog::On
            } else if watchdog.starts_with("the other processors") {
                Watchdog::Processors
            } else {
                Watchdog::Unavailable
            };
        } else if let Some(detail) = step.strip_prefix("DETAIL ") {
            let detail = detail
                .rsplit_once(" ms=")
                .map_or(detail, |(detail, _)| detail);
            found.detail = String::from(detail);
        } else if let Some((title, _)) = step.rsplit_once(" t=").filter(|(_, time)| {
            time.bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        }) {
            // `STEP <title> t=<seconds>`: a step begins.
            found.stage = String::from(title);
            (found.after, found.detail) = (false, String::new());
            began = at;
        } else if step.contains(" ms=") && !found.stage.is_empty() {
            // `STEP <mark> <title> … ms=<n>`: the running step ended.
            found.after = true;
        }
    }
    found.seconds = began
        .zip(last)
        .map(|(began, last)| last.saturating_sub(began) / 1000);
    Some(found)
}

impl Unfinished {
    /// The stage as a person reads it.
    fn stage_text(&self) -> String {
        let mut text = match (self.stage.is_empty(), self.after) {
            (true, _) => String::from("not in the log's last lines"),
            (false, false) => format!("\"{}\"", self.stage),
            (false, true) => format!("after \"{}\"", self.stage),
        };
        if !self.detail.is_empty() {
            text.push_str(&format!(", past \"{}\"", self.detail));
        }
        text
    }

    /// `PREVIOUS BOOT ended without a stop (boot N): last stage …,
    /// watchdog=…, N s in the stage`.
    pub fn line(&self) -> String {
        let seconds = match self.seconds {
            Some(seconds) => format!("{seconds} s in the stage by the log's last line"),
            None => String::from("time in the stage unknown"),
        };
        let unkept = match self.unkept {
            true => {
                "; the log holds no line from after the firmware's exit, so the boot may have \
                 gone on to a later stage"
            }
            false => "",
        };
        format!(
            "PREVIOUS BOOT ended without a stop (boot {}): last stage {}, watchdog={}, \
             {seconds}{unkept}\n",
            self.boot,
            self.stage_text(),
            self.watchdog.name()
        )
    }

    /// What the screen says: short, and where the log is.
    pub fn notice(&self) -> String {
        let stage = match (self.stage.is_empty(), self.unkept) {
            (true, _) => String::new(),
            (false, false) => format!(" during \"{}\"", self.stage),
            (false, true) => format!(" during \"{}\" or soon after", self.stage),
        };
        format!(
            "The last start of Stela on this PC did not end by itself: the PC was switched off \
             or reset{stage}. If Stela had stopped working, its log is on this USB: on any \
             computer with stela, plug in this USB and run `stela log`, which saves it to \
             log.txt; include that file in a report."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: [u8; 16] = [7; 16];

    fn running(boot: u64) -> Running {
        Running {
            boot,
            log: LOG,
            seconds: 41,
            flags: 0,
        }
    }

    const FAULTED: &str = "=== STELA BOOT 12 === version=0.5.1 firmware_clock=unread\n\
        [ 2317.000] STEP Restarting the PC t=2317.000\n\
        [ 2317.523] STELA FATAL x86 exception inside the PC firmware's variable service, \
        which Stela called to start Stela again after the restart macOS asked for \
        (vector=14 error=0x0 rip=0x777274ed cr2=0xffd0c04c): the PC does not restart by \
        itself\n";

    #[test]
    fn a_fault_in_the_variable_service_is_found_and_kept_for_the_pc() {
        let old = build_tag("0.5.1") << TAG_SHIFT;
        let service = |running: &Running, log: &[u8; 16], newest: &str, version: &str| {
            variable_service(running, 13, log, newest.as_bytes(), version)
        };
        // Under this version: avoided after the exit.
        assert_eq!(
            service(&running(12), &LOG, FAULTED, "0.5.1"),
            (VariableService::Avoided, VARIABLE_FAULT | old)
        );
        // Another USB's log, an older boot, or another stop: not this PC's.
        for (record, log, newest) in [
            (running(12), [1; 16], FAULTED),
            (running(11), LOG, FAULTED),
            (running(12), LOG, HUNG_AT_EXIT),
        ] {
            assert_eq!(
                service(&record, &log, newest, "0.5.1"),
                (VariableService::Trusted, 0)
            );
        }
        // A fault in an earlier boot of the boot before this one is not it.
        let earlier = alloc::format!("{FAULTED}=== STELA BOOT 12 ===\nSTELA RESTART code=0x15\n");
        assert_eq!(
            service(&running(12), &LOG, &earlier, "0.5.1").0,
            VariableService::Trusted
        );
        // The record's flag holds on any USB, after any boot.
        let flagged = Running {
            flags: VARIABLE_FAULT | old,
            ..running(3)
        };
        assert_eq!(
            variable_service(&flagged, 40, &[1; 16], b"", "0.5.1"),
            (VariableService::Avoided, VARIABLE_FAULT | old)
        );
        assert_eq!(Running::decode(&flagged.encode()), Some(flagged));
        let mut unknown = flagged.encode();
        unknown[44] = 4;
        let checksum = crc32_ieee(&unknown[16..]);
        unknown[12..16].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(Running::decode(&unknown), None);
        // A log that names no version: another's.
        let unnamed = FAULTED.replacen(" version=0.5.1 firmware_clock=unread", "", 1);
        assert_eq!(
            service(&running(12), &LOG, &unnamed, "0.5.1"),
            (VariableService::Tried, VARIABLE_FAULT | VARIABLE_TRIAL)
        );
    }

    /// A PC whose service faulted under an earlier version's mapping returns
    /// to normal once a try after the exit answers, where it stayed flagged
    /// for good; one whose service faults again in the try keeps the flag.
    #[test]
    fn a_fault_under_another_version_is_tried_once_and_cleared_when_it_answers() {
        let old = build_tag("0.5.1") << TAG_SHIFT;
        // The boot after the fault, of a newer version, tries the service.
        let (service, flags) =
            variable_service(&running(12), 13, &LOG, FAULTED.as_bytes(), "0.5.2");
        assert_eq!(
            (service, flags),
            (
                VariableService::Tried,
                VARIABLE_FAULT | VARIABLE_TRIAL | old
            )
        );
        let tried = Running {
            flags,
            ..running(13)
        };
        // Its restart's BootNext answered: the next boot trusts the service.
        let answered = "=== STELA BOOT 13 === version=0.5.2\n\
            [  900.100] POWER the firmware starts its entry Boot0003 again after the restart\n\
            [  900.200] STELA RESTART code=0x15\n";
        assert_eq!(
            variable_service(&tried, 14, &LOG, answered.as_bytes(), "0.5.2"),
            (VariableService::Trusted, 0)
        );
        let refused = answered.replace(
            "starts its entry Boot0003 again after the restart",
            "chooses what starts after the restart: it refused BootNext for its entry Boot0003 \
             (status 0x8000000000000007)",
        );
        assert_eq!(
            variable_service(&tried, 14, &LOG, refused.as_bytes(), "0.5.2").0,
            VariableService::Trusted
        );
        // It powered off without a call after the exit: tried again.
        let powered_off = "=== STELA BOOT 13 === version=0.5.2\n[  900.100] POWER powering off\n";
        assert_eq!(
            variable_service(&tried, 14, &LOG, powered_off.as_bytes(), "0.5.2"),
            (VariableService::Tried, flags)
        );
        // A boot that was no try clears nothing by such a line.
        let avoided = Running {
            flags: VARIABLE_FAULT | build_tag("0.5.2") << TAG_SHIFT,
            ..running(13)
        };
        assert_eq!(
            variable_service(&avoided, 14, &LOG, answered.as_bytes(), "0.5.2").0,
            VariableService::Avoided
        );
        // The try faulted too: this version avoids it from now on.
        let again = FAULTED.replace("BOOT 12 === version=0.5.1", "BOOT 13 === version=0.5.2");
        assert_eq!(
            variable_service(&tried, 14, &LOG, again.as_bytes(), "0.5.2"),
            (
                VariableService::Avoided,
                VARIABLE_FAULT | build_tag("0.5.2") << TAG_SHIFT
            )
        );
        assert_ne!(build_tag("0.5.1"), build_tag("0.5.2"));
    }

    #[test]
    fn the_record_is_read_back_and_other_bytes_are_never_a_boot() {
        let record = running(12);
        let bytes = record.encode();
        assert_eq!(bytes.len(), BYTES);
        assert_eq!(Running::decode(&bytes), Some(record));
        for at in [0, 8, 13, 16, 30, 41, 44] {
            let mut damaged = bytes;
            damaged[at] ^= 1;
            assert_eq!(Running::decode(&damaged), None, "{at}");
        }
        assert_eq!(Running::decode(&bytes[..BYTES - 1]), None);
        assert_eq!(Running::decode(&[0; BYTES]), None);
        assert_eq!(Running::decode(&[]), None);
        assert_eq!(
            NAME.iter()
                .map(|&unit| unit as u8 as char)
                .collect::<String>(),
            "StelaRunning\0"
        );
        // Linux's reserve stays free after both copies of the variable.
        assert!(!store_takes(5120 + 351));
        assert!(store_takes(5120 + 352));
        assert_eq!(WRITES_PER_BOOT, 1);
    }

    /// A boot switched off while it hung in a step after the firmware's
    /// exit: the log's lines end before the exit.
    const HUNG_AT_EXIT: &str = "[   20.100] STEP o Reading Stela's settings ms=12\n\
        === STELA BOOT 12 ===\n\
        [    4.000] HOST version=0.5.0 cpu=\"test\"\n\
        [   30.250] STEP Preparing the Mac's memory t=30.250\n\
        [   30.900] STEP o Preparing the Mac's memory ms=650\n\
        [   31.000] NATIVE MMIO memory map before exit: sealed\n\
        [   31.004] STEP Taking control of the other processors t=31.004\n";

    #[test]
    fn a_boot_that_ended_at_the_firmware_s_exit_is_told_with_its_last_kept_stage() {
        let found = unfinished(&running(12), 13, &LOG, None, HUNG_AT_EXIT.as_bytes()).unwrap();
        assert_eq!(
            found,
            Unfinished {
                boot: 12,
                stage: String::from("Taking control of the other processors"),
                after: false,
                detail: String::new(),
                watchdog: Watchdog::Unknown,
                seconds: Some(0),
                unkept: true,
            }
        );
        assert_eq!(
            found.line(),
            "PREVIOUS BOOT ended without a stop (boot 12): last stage \"Taking control of the \
             other processors\", watchdog=unknown, 0 s in the stage by the log's last line; the \
             log holds no line from after the firmware's exit, so the boot may have gone on to \
             a later stage\n"
        );
        let notice = found.notice();
        assert!(notice.contains(
            "switched off or reset during \"Taking control of the other processors\" or soon \
             after"
        ));
        assert!(notice.contains("on any computer with stela"), "{notice}");
        assert!(notice.contains("`stela log`"), "{notice}");
    }

    #[test]
    fn a_boot_that_hung_with_the_log_open_names_its_step_its_time_and_the_watchdog() {
        let log = format!(
            "{HUNG_AT_EXIT}\
             [   31.900] STEP WATCHDOG on: a Linux step that does not return within 30 s stops \
             the boot (AMD core counter 0)\n\
             [   33.000] BOOT LOG FILE \\EFI\\stela\\boot.log through the USB driver boot=12 \
             head=0x8000\n\
             [   33.500] STEP o Taking control of the other processors ms=2496\n\
             [   33.600] STEP Starting the GPU t=33.600\n\
             [   34.000] STEP DETAIL loading its firmware 1 ms=400\n\
             [   35.000] STEP DETAIL loading its firmware 3 ms=1400 unlogged_like_it=1\n\
             [   95.250] LINUX <6> nouveau 0000:01:00.0: still waiting\n"
        );
        let found = unfinished(&running(12), 13, &LOG, None, log.as_bytes()).unwrap();
        assert_eq!(found.stage, "Starting the GPU");
        // A thinned detail's count of those left out is not part of it.
        assert_eq!(found.detail, "loading its firmware 3");
        assert_eq!(
            (found.seconds, found.unkept, found.after),
            (Some(61), false, false)
        );
        assert_eq!(found.watchdog, Watchdog::On);
        assert_eq!(
            found.line(),
            "PREVIOUS BOOT ended without a stop (boot 12): last stage \"Starting the GPU\", \
             past \"loading its firmware 3\", watchdog=on, 61 s in the stage by the log's last \
             line\n"
        );
        assert!(
            found
                .notice()
                .contains("reset during \"Starting the GPU\". If")
        );

        // Between two steps, and the other watchdog lines.
        let between = format!(
            "{log}[   96.000] STEP WATCHDOG the other processors watch the boot processor's \
             Linux steps\n[   97.000] STEP o Starting the GPU (started) ms=63400\n"
        );
        let found = unfinished(&running(12), 13, &LOG, None, between.as_bytes()).unwrap();
        assert!(found.after);
        assert_eq!(found.watchdog, Watchdog::Processors);
        assert!(
            found
                .line()
                .contains("last stage after \"Starting the GPU\"")
        );
        let unavailable = "=== STELA BOOT 12 ===\nSTEP WATCHDOG unavailable: no counter\n";
        let found = unfinished(&running(12), 13, &LOG, None, unavailable.as_bytes()).unwrap();
        assert_eq!(found.watchdog, Watchdog::Unavailable);
        assert_eq!(found.seconds, None);
        assert!(
            found
                .line()
                .contains("last stage not in the log's last lines")
        );
        assert!(found.line().contains("time in the stage unknown"));
    }

    #[test]
    fn a_boot_whose_end_is_recorded_or_that_is_not_the_last_is_not_told() {
        let tail = HUNG_AT_EXIT.as_bytes();
        assert!(unfinished(&running(12), 13, &LOG, None, tail).is_some());
        // A stop record carried from the firmware for that boot.
        assert_eq!(unfinished(&running(12), 13, &LOG, Some(12), tail), None);
        assert!(unfinished(&running(12), 13, &LOG, Some(9), tail).is_some());
        // Another USB's record, an older boot's, and a log that started over.
        assert_eq!(unfinished(&running(12), 13, &[8; 16], None, tail), None);
        assert_eq!(unfinished(&running(12), 14, &LOG, None, tail), None);
        assert_eq!(unfinished(&running(12), 1, &LOG, None, tail), None);
        assert_eq!(unfinished(&running(u64::MAX), 0, &LOG, None, tail), None);
        // Every end's line in the boot's last lines.
        for end in [
            "[  900.000] STELA FATAL the GPU stopped answering\n",
            "STELA PANIC at src/main.rs\n",
            "[  900.000] STELA STOPPED code=0x7\n",
            "[  900.000] STELA RESTART code=0x5\n",
            "[  900.000] STELA POWER-OFF code=0x6\n",
            "[  900.000] POWER powering off through the firmware\n",
        ] {
            let ended = format!("{HUNG_AT_EXIT}{end}");
            assert_eq!(
                unfinished(&running(12), 13, &LOG, None, ended.as_bytes()),
                None,
                "{end}"
            );
            assert!(enough(end.as_bytes()), "{end}");
        }
        // An end in an earlier boot is not this boot's.
        let earlier = format!("[  900.000] STELA RESTART code=0x5\n{HUNG_AT_EXIT}");
        assert!(unfinished(&running(12), 13, &LOG, None, earlier.as_bytes()).is_some());
        // A long boot: its start is older than the lines read.
        let long = "[ 7200.000] BEAT SHORT blocks=9\n[ 7210.000] BEAT SHORT blocks=10\n";
        let found = unfinished(&running(12), 13, &LOG, None, long.as_bytes()).unwrap();
        assert!(!found.unkept && found.stage.is_empty());
        assert!(!enough(long.as_bytes()));
        assert!(enough(HUNG_AT_EXIT.as_bytes()));
        assert!(enough(&[b'x'; TAIL_BYTES]));
    }
}
