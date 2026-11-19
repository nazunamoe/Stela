//! Byte budgets per line family for the boot log, so that one source that
//! floods it cannot push every other source's lines out of its ring.
//!
//! A line's family is its leading words ([`family`]): `GUEST|` for the
//! guest's own output, `DCP payload`, `GPU DRM`, `nouveau`, `HOST FIRMWARE
//! RECORD`. Each family has a bucket of [`BURST_BYTES`] ([`GUEST_BURST_BYTES`]
//! for the guest's console), which it may write at once, refilled at
//! [`RATE_BYTES_PER_SECOND`]; a family thinned without
//! a pause for [`LONG_SECONDS`] is refilled at [`LONG_RATE_BYTES_PER_SECOND`]
//! only, until its bucket is full again. A line is kept when its family's
//! bucket holds any bytes as it starts, and costs its length; a line that
//! starts on an empty bucket is left out of the log and counted. A
//! `BOOT LOG THINNED` line names each family that was thinned, with the
//! lines and bytes left out and the seconds they cover, at the first line
//! start a second after its last line left out, and every
//! [`SUMMARY_SECONDS`] while it goes on. The serial port has every line.
//!
//! Measured (2026-11-18): a booted desktop's lines outside the heartbeat are
//! about 140 B/s at the median, the largest family a few hundred; one guest
//! error repeated 1114 times wrote 93 KB in 10 s; the bundled firmware's
//! records wrote 241 KB in the first second of a boot with an integrated
//! AMD GPU; one display swap's payload dump wrote 59 to 250 KB at once; a
//! Linux GPU driver's warning repeated 1362 times wrote 93 KB in 10 s.
//! Replayed over eight reporters' logs (2.5 to 18 minutes of macOS each),
//! these budgets leave out the payload dumps and the repeated warnings,
//! 1 % of one log, and nothing of the other seven. A flooding family costs
//! its burst, 0.15 MB more in its first ten minutes and 0.12 MB in each
//! later hour, so a day of one flood is about 3 MB.
//!
//! The heartbeat's reports, whose size the beat's pace bounds
//! (`beat_pace`), and every line once a stop began, are `exempt`: they are
//! kept and cost nothing. So are lines before the counter's rate is known,
//! the first fraction of a second, and the lines that say how the boot or
//! the guest went down ([`names_an_end`]): the lines the next start reads
//! for how this boot ended (`running_record::ENDS`), and the guest's panic,
//! from its `panic(` line, [`PANIC_LINES`] lines, which a guest that
//! flooded its console before it panicked would otherwise lose. The step
//! lines ([`names_a_step`]) are too: the next start names the last step
//! begun from them (`running_record::unfinished`), and they are few, a
//! step's begin and end and its details, which the status thins itself.
//!
//! [`Gate`] applies the budgets to output that arrives in pieces: it holds
//! a line's first [`KEY_BYTES`] until they name its family, and its user
//! ends each write with [`Gate::end`], which decides a line held that long.
use core::fmt::{self, Write as _};

/// The bytes of a line's start that name its family.
pub const KEY_BYTES: usize = 24;
/// The families the gate tells apart at once; the last slot is shared by
/// every family that finds the others busy.
pub const FAMILIES: usize = 64;
/// What a family may write at once: a PCI census, one display swap's
/// payload dump.
pub const BURST_BYTES: i64 = 64 << 10;
/// What the guest's own console (`GUEST|`) may write at once: a macOS
/// boot's kernel and launchd lines, 0.2 to 0.7 MB in its first minutes on
/// the reporters' PCs, and a panic after them.
pub const GUEST_BURST_BYTES: i64 = 1 << 20;
/// The guest console's family.
const GUEST: &[u8] = b"GUEST|";
/// What a family may write each second after its burst: above any one
/// family's steady rate outside the heartbeat.
pub const RATE_BYTES_PER_SECOND: i64 = 256;
/// A family thinned this long without its bucket filling again ...
pub const LONG_SECONDS: u64 = 600;
/// ... gets this rate until it does: about 0.12 MB an hour.
pub const LONG_RATE_BYTES_PER_SECOND: i64 = 32;
/// A family that goes on being thinned has its count said this often.
pub const SUMMARY_SECONDS: u64 = 60;
/// A family's count is said once its last left-out line is this old.
const SETTLED_MILLIS: u64 = 1000;
/// The guest's lines kept from its panic line on, whatever its budget: a
/// panic's report, its backtrace and its loaded extensions, with room.
pub const PANIC_LINES: u32 = 2048;

/// Whether a line that starts with `start` (its text after the stamp) says
/// how this boot ended, as the next start reads it (`running_record::ENDS`).
pub fn names_an_end(start: &[u8]) -> bool {
    crate::running_record::ENDS
        .iter()
        .any(|end| start.starts_with(end.as_bytes()))
}

/// Whether a line that starts with `start` is a step's (`STEP <title>
/// t=`, `STEP <mark> <title> ms=`, `STEP DETAIL`, `STEP WATCHDOG`), which
/// the next start reads for the step a boot stopped in.
pub fn names_a_step(start: &[u8]) -> bool {
    start.starts_with(b"STEP ")
}

/// Whether a line that starts with `start` begins the guest's panic: the
/// guest console's `panic(cpu ...)` line.
pub fn begins_guest_panic(start: &[u8]) -> bool {
    start.strip_prefix(GUEST).is_some_and(|text| {
        let text = &text[text.iter().take_while(|&&byte| byte == b' ').count()..];
        text.starts_with(b"panic(")
    })
}

/// A family's name: up to [`KEY_BYTES`] of a line's start.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Key {
    bytes: [u8; KEY_BYTES],
    len: u8,
}

impl Key {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    fn push(&mut self, word: &[u8]) -> bool {
        let at = self.len as usize;
        let space = usize::from(at != 0);
        if at + space + word.len() > KEY_BYTES {
            return false;
        }
        if space != 0 {
            self.bytes[at] = b' ';
        }
        self.bytes[at + space..at + space + word.len()].copy_from_slice(word);
        self.len = (at + space + word.len()) as u8;
        true
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}",
            core::str::from_utf8(self.as_bytes()).unwrap_or("?")
        )
    }
}

/// A word of capitals, digits and `_|-/` that starts with a capital, as
/// the log's line names are (`HOST`, `GUEST|`, `PES`, `X86_64`).
fn capitals(word: &[u8]) -> bool {
    word.len() >= 2
        && word[0].is_ascii_uppercase()
        && word
            .iter()
            .all(|&byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'|' | b'-' | b'/'))
}

/// A word a driver's lines start with (`nouveau`, `amdgpu`, `[drm]`,
/// `xhci_hcd`), or the lower-case word after one capital word
/// (`DCP payload`, `STEP o`) or after a kernel log level.
fn plain(word: &[u8]) -> bool {
    !word.is_empty()
        && word.len() <= 16
        && word
            .iter()
            .all(|&byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'[' | b']'))
}

/// A Linux kernel log level, as `LINUX <3> nouveau ...` lines give it.
fn level(word: &[u8]) -> bool {
    matches!(word, [b'<', b'0'..=b'7', b'>'])
}

/// The family of a line that starts with `start` (its text after the
/// stamp): its first three capital words, or one capital word and the
/// lower-case word after it, with a kernel log level and the driver after
/// it (`LINUX <3> nouveau`), or a driver's first word; the empty family
/// for anything else, such as an indented line.
pub fn family(start: &[u8]) -> Key {
    let mut key = Key::default();
    let line = start.split(|&byte| byte == b'\n').next().unwrap_or(start);
    let (mut capital, mut leveled) = (0, false);
    for word in line.split(|&byte| byte == b' ') {
        let word = word.strip_suffix(b":").unwrap_or(word);
        // `GUEST|text` names the guest's lines whatever follows.
        if let Some(at) = word.iter().position(|&byte| byte == b'|')
            && capital == 0
            && capitals(&word[..=at])
        {
            key.push(&word[..=at]);
            return key;
        }
        if !leveled && capitals(word) && capital < 3 && key.push(word) {
            capital += 1;
            continue;
        }
        if !leveled && capital != 0 && level(word) && key.push(word) {
            leveled = true;
            continue;
        }
        let lower = word.iter().all(u8::is_ascii_lowercase);
        if (capital == 1 && lower || leveled || capital == 0) && plain(word) {
            key.push(word);
        }
        break;
    }
    key
}

/// What `key`'s family may write at once.
fn burst(key: &Key) -> i64 {
    match key.as_bytes() == GUEST {
        true => GUEST_BURST_BYTES,
        false => BURST_BYTES,
    }
}

/// One family's bucket and what it left out since its count was last said.
#[derive(Clone, Copy, Default)]
struct Bucket {
    key: Key,
    used: bool,
    /// Bytes it may write now; below zero after a long kept line.
    tokens: i64,
    /// What it may write at once.
    burst: i64,
    /// Thousandths of a byte the last refill did not give.
    remainder: i64,
    at: u64,
    /// When it was first left out without its bucket filling since.
    thinned_since: Option<u64>,
    /// Left out since its count was last said.
    lines: u64,
    bytes: u64,
    first_left_out: u64,
    last_left_out: u64,
    said_at: u64,
}

impl Bucket {
    fn fresh(key: Key, now: u64) -> Self {
        Self {
            key,
            used: true,
            tokens: burst(&key),
            burst: burst(&key),
            at: now,
            said_at: now,
            ..Self::default()
        }
    }

    fn refill(&mut self, now: u64) {
        let rate = match self.thinned_since {
            Some(since) if now.saturating_sub(since) >= LONG_SECONDS * 1000 => {
                LONG_RATE_BYTES_PER_SECOND
            }
            _ => RATE_BYTES_PER_SECOND,
        };
        let elapsed = now.saturating_sub(self.at).min(1 << 32) as i64;
        let gained = elapsed * rate + self.remainder;
        self.tokens = (self.tokens + gained / 1000).min(self.burst);
        self.remainder = gained % 1000;
        self.at = now;
        if self.tokens == self.burst {
            self.thinned_since = None;
            self.remainder = 0;
        }
    }

    /// Whether its count waits to be said, and is due at `now`.
    fn due(&self, now: u64) -> bool {
        self.lines != 0
            && (now.saturating_sub(self.last_left_out) >= SETTLED_MILLIS
                || now.saturating_sub(self.said_at) >= SUMMARY_SECONDS * 1000)
    }

    /// Its count as a line, which empties it.
    fn summary(&mut self, now: u64) -> Summary {
        let mut line = Summary::default();
        let name = match self.key.as_bytes() {
            [] => "lines with no family name, or of families past the 63 told apart",
            key => core::str::from_utf8(key).unwrap_or("?"),
        };
        let _ = writeln!(
            line,
            "BOOT LOG THINNED \"{name}\": {} lines, {} bytes left out of this log over {} s, past \
             the family's {} KiB at once and {} B/s (the serial port has them)",
            self.lines,
            self.bytes,
            self.last_left_out.saturating_sub(self.first_left_out) / 1000,
            self.burst >> 10,
            match self.thinned_since {
                Some(since) if now.saturating_sub(since) >= LONG_SECONDS * 1000 => {
                    LONG_RATE_BYTES_PER_SECOND
                }
                _ => RATE_BYTES_PER_SECOND,
            },
        );
        self.lines = 0;
        self.bytes = 0;
        self.said_at = now;
        line
    }
}

/// A summary line, formatted without the heap.
pub struct Summary {
    bytes: [u8; 320],
    len: usize,
}

impl Default for Summary {
    fn default() -> Self {
        Self {
            bytes: [0; 320],
            len: 0,
        }
    }
}

impl Summary {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Write for Summary {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        // A line cut short still ends with its line feed.
        let room = self.bytes.len() - 1 - self.len;
        let take = text.len().min(room);
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        if take < text.len() && self.bytes[self.len - 1] != b'\n' {
            self.bytes[self.len] = b'\n';
            self.len += 1;
        }
        Ok(())
    }
}

/// What the gate hands its log: the start of a line, which takes its
/// stamp, and bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    LineStart,
    Bytes(&'a [u8]),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Line {
    /// The next byte starts a line.
    Start,
    /// The line's first bytes wait in `held` for its family.
    Holding,
    /// The line goes to the log; its bytes cost the bucket, if any.
    Keep(Option<usize>),
    /// The line is left out and counted in the bucket.
    Drop(usize),
}

/// The budgets, and the line being written.
pub struct Gate {
    buckets: [Bucket; FAMILIES],
    held: [u8; KEY_BYTES],
    held_len: usize,
    line: Line,
    /// The held line is exempt.
    held_exempt: bool,
    /// The guest's lines still kept for its panic ([`PANIC_LINES`]).
    panic_lines: u32,
    /// The earliest time a count may be due; `u64::MAX` with none waiting.
    next_due: u64,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub const fn new() -> Self {
        const EMPTY: Bucket = Bucket {
            key: Key {
                bytes: [0; KEY_BYTES],
                len: 0,
            },
            used: false,
            tokens: 0,
            burst: 0,
            remainder: 0,
            at: 0,
            thinned_since: None,
            lines: 0,
            bytes: 0,
            first_left_out: 0,
            last_left_out: 0,
            said_at: 0,
        };
        Self {
            buckets: [EMPTY; FAMILIES],
            held: [0; KEY_BYTES],
            held_len: 0,
            line: Line::Start,
            held_exempt: false,
            panic_lines: 0,
            next_due: u64::MAX,
        }
    }

    /// Whether the next byte starts a line.
    pub fn at_line_start(&self) -> bool {
        self.line == Line::Start
    }

    /// Bytes reached the log past the gate (lines carried from the loader):
    /// the next byte starts a line when `line_start`.
    pub fn resume(&mut self, line_start: bool) {
        self.line = match line_start {
            true => Line::Start,
            false => Line::Keep(None),
        };
        self.held_len = 0;
    }

    /// The bucket of `key`, made for it if need be: an unused slot, else
    /// the least recently used full one with nothing to say, else the last.
    fn bucket(&mut self, key: Key, now: u64) -> usize {
        let named = &self.buckets[..FAMILIES - 1];
        if let Some(index) = named
            .iter()
            .position(|bucket| bucket.used && bucket.key == key)
        {
            return index;
        }
        let mut free = None;
        let mut idle: Option<(usize, u64)> = None;
        for (index, bucket) in self.buckets[..FAMILIES - 1].iter_mut().enumerate() {
            if !bucket.used {
                free = free.or(Some(index));
                continue;
            }
            bucket.refill(now);
            if bucket.tokens == bucket.burst
                && bucket.lines == 0
                && idle.is_none_or(|(_, at)| bucket.at < at)
            {
                idle = Some((index, bucket.at));
            }
        }
        let shared = FAMILIES - 1;
        match free.or(idle.map(|(index, _)| index)) {
            Some(index) => {
                self.buckets[index] = Bucket::fresh(key, now);
                index
            }
            None => {
                if !self.buckets[shared].used {
                    self.buckets[shared] = Bucket::fresh(Key::default(), now);
                }
                shared
            }
        }
    }

    /// Decide the line whose first bytes are held, and hand them on.
    fn decide(&mut self, now: Option<u64>, out: &mut dyn FnMut(Piece<'_>)) {
        let held_len = self.held_len;
        let start = &self.held[..held_len];
        if begins_guest_panic(start) {
            self.panic_lines = PANIC_LINES;
        }
        let panic = self.panic_lines != 0 && start.starts_with(GUEST);
        if panic {
            self.panic_lines -= 1;
        }
        let exempt = self.held_exempt || panic || names_an_end(start) || names_a_step(start);
        self.held_len = 0;
        let Some(now) = now.filter(|_| !exempt) else {
            self.line = Line::Keep(None);
            out(Piece::LineStart);
            out(Piece::Bytes(&self.held[..held_len]));
            return;
        };
        let index = self.bucket(family(&self.held[..held_len]), now);
        let bucket = &mut self.buckets[index];
        bucket.refill(now);
        if bucket.tokens > 0 {
            bucket.tokens -= held_len as i64;
            self.line = Line::Keep(Some(index));
            out(Piece::LineStart);
            out(Piece::Bytes(&self.held[..held_len]));
        } else {
            if bucket.lines == 0 {
                bucket.first_left_out = now;
            }
            bucket.thinned_since.get_or_insert(now);
            bucket.lines += 1;
            bucket.bytes += held_len as u64;
            bucket.last_left_out = now;
            self.next_due = self.next_due.min(now + SETTLED_MILLIS);
            self.line = Line::Drop(index);
        }
    }

    /// Write `text`, a piece of output, at `now` (milliseconds; `None`
    /// before the clock runs, which keeps every line). `exempt` lines are
    /// kept and cost nothing. A line's start whose family is not known
    /// yet waits for the next piece or [`Gate::end`].
    pub fn write(
        &mut self,
        text: &[u8],
        now: Option<u64>,
        exempt: bool,
        out: &mut dyn FnMut(Piece<'_>),
    ) {
        for segment in text.split_inclusive(|&byte| byte == b'\n') {
            let mut rest = segment;
            if matches!(self.line, Line::Start | Line::Holding) {
                if self.line == Line::Start {
                    self.say_due(now, out);
                    self.held_exempt = exempt;
                    self.line = Line::Holding;
                }
                let take = rest.len().min(KEY_BYTES - self.held_len);
                self.held[self.held_len..self.held_len + take].copy_from_slice(&rest[..take]);
                self.held_len += take;
                rest = &rest[take..];
                let ended = self.held[self.held_len - 1] == b'\n';
                if self.held_len < KEY_BYTES && !ended {
                    continue;
                }
                self.decide(now, out);
            }
            match self.line {
                Line::Keep(bucket) => {
                    if let Some(index) = bucket {
                        self.buckets[index].tokens -= rest.len() as i64;
                    }
                    if !rest.is_empty() {
                        out(Piece::Bytes(rest));
                    }
                }
                Line::Drop(index) => self.buckets[index].bytes += rest.len() as u64,
                Line::Start | Line::Holding => {}
            }
            if segment.ends_with(b"\n") {
                self.line = Line::Start;
            }
        }
    }

    /// End a write: a line whose start is still held is decided now, so no
    /// byte waits for later output.
    pub fn end(&mut self, now: Option<u64>, out: &mut dyn FnMut(Piece<'_>)) {
        if self.line == Line::Holding {
            self.decide(now, out);
        }
    }

    /// Say the counts that are due at `now`, or every count when `all`, as
    /// lines; only between lines.
    pub fn say(&mut self, now: Option<u64>, all: bool, out: &mut dyn FnMut(Piece<'_>)) {
        if self.line != Line::Start {
            return;
        }
        let Some(now) = now else {
            return;
        };
        if !all && now < self.next_due {
            return;
        }
        let mut next = u64::MAX;
        for bucket in self.buckets.iter_mut().filter(|bucket| bucket.lines != 0) {
            if all || bucket.due(now) {
                let summary = bucket.summary(now);
                out(Piece::LineStart);
                out(Piece::Bytes(summary.as_bytes()));
            } else {
                next = next
                    .min(bucket.last_left_out + SETTLED_MILLIS)
                    .min(bucket.said_at + SUMMARY_SECONDS * 1000);
            }
        }
        self.next_due = next;
    }

    fn say_due(&mut self, now: Option<u64>, out: &mut dyn FnMut(Piece<'_>)) {
        if now.is_some_and(|now| now >= self.next_due) {
            self.say(now, false, out);
        }
    }
}

#[cfg(test)]
mod tests;
