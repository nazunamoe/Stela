//! The host disk under ANS's namespaces.
//!
//! A host transfer takes as long as the host's storage does. Each I/O
//! command's transfer is submitted where the guest rings the doorbell, as
//! many at once as the guest queues, through the block layer
//! ([`Submission`]), which runs them on the disk as it has room. The
//! controller completes each command once the disk has completed its
//! transfer, in whatever order that happens, as a real controller completes
//! commands once its media has. It looks once the disk has posted a
//! completion. A disk that signals its completions wakes the core that
//! looks, so it looks otherwise only for a lost signal; for one that does
//! not, it looks after half the disk's recent latency, then more often
//! (Linux's hybrid polling). Nothing waits for the disk and nothing wakes
//! while no transfer runs. A disk that runs nothing queued executes each
//! command on the calling PE. One poll admits at most 64 commands, so larger
//! transfers keep their partial progress until later polls. The boot stages
//! the guest's disk: there a submission and a look only queue commands and
//! take results, and the host I/O runs outside the devices' lock.
//!
//! A mounting APFS deallocates tens of thousands of ranges, which already
//! read as zeros ([`super::namespace::Medium::deallocate`]), so a Deallocate
//! completes at once and the disk hears of them in the background, as
//! Linux's `fstrim` tells it in batches: [`Discards`] keeps them merged and
//! sends a batch once guest transfers have paused for [`DISCARD_QUIET`]. A
//! batch is one command, which a read that comes while it runs waits for,
//! so it covers at most what the disk deallocates in about
//! [`DISCARD_TARGET`]: the size grows or shrinks with each measured command,
//! between [`DISCARD_LEAST`] and [`DISCARD_MOST`] bytes and the disk's own
//! limits. A disk whose discard commands take longer than [`DISCARD_SLOW`]
//! [`DISCARD_STRIKES`] times in a row hears no more discards: they are
//! hints (NVMe 1.4 section 6.7.1.1), and the ANS line says `shed=`; but not
//! one the guest was told keeps deallocated blocks zeros past a restart
//! ([`HostDisk::deallocates_zeroed`]), whose discards are that promise. Blocks
//! the disk heard of and no write took back since are not sent again, as an
//! APFS format and its first mounts deallocate the whole container more
//! than once. A flush that waits for discards sends them at once and
//! keeps [`DISCARD_COMMANDS`] commands of at least [`DISCARD_URGENT`] bytes,
//! which no measure takes from, on the disk, starting the next as
//! soon as one completes rather than once a whole group has: one slow
//! deallocation does not hold back the rest. A transfer waits for discards
//! at most [`DISCARD_HOLD`]; then it goes, and those not yet sent go on in
//! the background where they keep deallocated blocks zeros, and are given
//! up elsewhere.
//! Discards never reorder past
//! writes. A write takes its blocks back from those waiting and waits out a
//! batch it overlaps; a waiting run a running write overlaps waits out the
//! write; and a flush waits for all of them where the guest was told
//! deallocated blocks stay zeros, as a controller's cache holds them.
use alloc::{boxed::Box, collections::BTreeMap, format, string::String, vec::Vec};
use stela_drivers::block::{
    BlockDevice, BlockError, Extent, Finished, Identity, Request, Submission,
};

/// The latency assumed before any transfer completed: 100 us of the 24 MHz
/// counter.
const FIRST_LATENCY: u64 = 2_400;
/// The shortest wait between two looks: 10 us.
const SHORTEST_POLL: u64 = 240;
/// How long a signalling disk's transfer may go unsignalled before a look
/// finds a lost completion: 1 ms.
const LOST_SIGNAL: u64 = 24_000;

/// The discard commands kept on the disk while a transfer waits for
/// discards or a shutdown hurries them, each one batch.
const DISCARD_COMMANDS: usize = 16;
/// How long guest transfers must have paused before a discard batch goes to
/// the disk: 10 ms.
const DISCARD_QUIET: u64 = 240_000;
/// How long one discard command should take, which a guest read that comes
/// while it runs waits for: 20 ms.
const DISCARD_TARGET: u64 = 480_000;
/// The bytes the first discard command covers, before any was measured.
const DISCARD_FIRST: u64 = 64 << 20;
/// The fewest and most bytes one discard command covers.
const DISCARD_LEAST: u64 = 1 << 20;
const DISCARD_MOST: u64 = 8 << 30;
/// The fewest bytes a command covers while a transfer waits for discards
/// or a shutdown hurries them: the guest already waits for all of them,
/// [`DISCARD_COMMANDS`] run together, and [`DISCARD_HOLD`] bounds the wait.
const DISCARD_URGENT: u64 = 1 << 30;
/// A discard command slower than this, [`DISCARD_STRIKES`] times in a row
/// however small, makes the disk hear no more discards: 200 ms.
const DISCARD_SLOW: u64 = 4_800_000;
const DISCARD_STRIKES: u32 = 3;
/// The longest a guest transfer waits for discards: 5 s, well inside the
/// 30 s macOS gives an NVMe command. Then it goes; the discards not yet
/// sent go on in the background on a disk whose deallocated blocks stay
/// zeros, and are given up on any other.
const DISCARD_HOLD: u64 = 120_000_000;
/// The most runs [`Discards::heard`] keeps; past it, it forgets them all and
/// at worst a run reaches the disk twice.
const HEARD_RUNS: usize = 65_536;

/// Merge `start..end` into `runs`, disjoint runs of blocks keyed by their
/// first block, each with its end.
fn insert_run(runs: &mut BTreeMap<u64, u64>, mut start: u64, mut end: u64) {
    if start >= end {
        return;
    }
    if let Some((&first, &last)) = runs.range(..=start).next_back()
        && last >= start
    {
        start = first;
        end = end.max(last);
    }
    while let Some((&first, &last)) = runs.range(start..=end).next() {
        end = end.max(last);
        runs.remove(&first);
    }
    runs.insert(start, end);
}

/// Take `start..end` out of `runs`.
fn remove_runs(runs: &mut BTreeMap<u64, u64>, start: u64, end: u64) {
    let before = runs
        .range(..start)
        .next_back()
        .filter(|&(_, &last)| last > start)
        .map(|(&first, _)| first);
    if let Some(first) = before {
        let last = runs.insert(first, start).expect("a run");
        if last > end {
            runs.insert(end, last);
        }
    }
    while let Some((&first, &last)) = runs.range(start..end).next() {
        runs.remove(&first);
        if last > end {
            runs.insert(end, last);
        }
    }
}

/// A discard command on the disk: its runs, when it started and whether it
/// started alone, so its time measures the disk's.
struct Batch {
    submission: Submission,
    extents: Vec<Extent>,
    started: u64,
    alone: bool,
}

/// Deallocated blocks the disk has not heard of.
struct Discards {
    /// Merged runs of host blocks: each one's first block and its end.
    waiting: BTreeMap<u64, u64>,
    /// Runs the disk heard of, or is hearing of, that no write took back
    /// since: a deallocation of them again sends nothing.
    heard: BTreeMap<u64, u64>,
    /// The batches on the disk: at most one in the background,
    /// [`DISCARD_COMMANDS`] while urgent.
    running: Vec<Batch>,
    /// Transfers waiting for discards: each one's key, when it was
    /// submitted and its request.
    held: Vec<(u32, u64, Request)>,
    /// Why the disk refused a batch. Discards are hints, so it hears no
    /// more; a flush has nothing to wait for.
    failed: Option<BlockError>,
    /// When a guest transfer last started or completed, if one has.
    active: Option<u64>,
    /// The controller's shutdown waits for these: batches go at once,
    /// [`DISCARD_COMMANDS`] on the disk, as for a waiting transfer.
    hurried: bool,
    /// Runs given up, which the disk never heard of.
    abandoned: u64,
    /// The bytes the next command covers at most.
    piece: u64,
    /// The last command measured alone: its bytes and counter ticks.
    last: Option<(u64, u64)>,
    /// Measured commands slower than [`DISCARD_SLOW`] in a row.
    slow: u32,
    /// The command that made the disk hear no more discards, as `last`.
    shed: Option<(u64, u64)>,
    /// A transfer waited [`DISCARD_HOLD`] for these discards: a flush no
    /// longer waits for them, which go on in the background, until none
    /// is left.
    lapsed: bool,
}

impl Default for Discards {
    fn default() -> Self {
        Self {
            waiting: BTreeMap::new(),
            heard: BTreeMap::new(),
            running: Vec::new(),
            held: Vec::new(),
            failed: None,
            active: None,
            hurried: false,
            abandoned: 0,
            piece: DISCARD_FIRST,
            last: None,
            slow: 0,
            shed: None,
            lapsed: false,
        }
    }
}

impl Discards {
    /// Merge the parts of `start..end` the disk has not heard of into the
    /// waiting runs.
    fn add(&mut self, start: u64, end: u64) {
        let mut heard: Vec<(u64, u64)> = self
            .heard
            .range(..end)
            .rev()
            .take_while(|&(_, &last)| last > start)
            .map(|(&first, &last)| (first.max(start), last.min(end)))
            .collect();
        heard.reverse();
        let mut at = start;
        for (first, last) in heard {
            insert_run(&mut self.waiting, at, first);
            at = at.max(last);
        }
        insert_run(&mut self.waiting, at, end);
    }

    /// A write takes `start..end` back from the waiting runs and from those
    /// the disk heard of. Whether it overlaps a running batch, which it
    /// must wait out.
    fn written(&mut self, start: u64, end: u64) -> bool {
        remove_runs(&mut self.waiting, start, end);
        remove_runs(&mut self.heard, start, end);
        self.running.iter().any(|batch| {
            batch
                .extents
                .iter()
                .any(|extent| extent.lba < end && extent.lba + extent.blocks > start)
        })
    }

    /// The waiting runs for one command, first blocks first: at most
    /// `ranges` of them, each at most `range` blocks (0: no limit), and
    /// `budget` blocks in all. A run cut short keeps its rest waiting, and
    /// one that overlaps a block range of `writing`, the running writes',
    /// waits whole, so that the write lands first. The disk hears of them
    /// from now on.
    fn batch(
        &mut self,
        ranges: usize,
        range: u64,
        budget: u64,
        writing: &[(u64, u64)],
    ) -> Vec<Extent> {
        let mut extents = Vec::new();
        let mut later = Vec::new();
        let mut left = budget.max(1);
        while extents.len() < ranges.max(1)
            && left != 0
            && let Some((lba, end)) = self.waiting.pop_first()
        {
            if writing
                .iter()
                .any(|&(first, last)| first < end && lba < last)
            {
                later.push((lba, end));
                continue;
            }
            let mut blocks = (end - lba).min(left);
            if range != 0 {
                blocks = blocks.min(range);
            }
            if lba + blocks < end {
                self.waiting.insert(lba + blocks, end);
            }
            left -= blocks;
            if self.heard.len() >= HEARD_RUNS {
                self.heard.clear();
            }
            insert_run(&mut self.heard, lba, lba + blocks);
            extents.push(Extent { lba, blocks });
        }
        self.waiting.extend(later);
        extents
    }

    /// A command measured alone covered `bytes` in `took` counter ticks:
    /// fit the next to [`DISCARD_TARGET`], at most twice this one, and,
    /// where `may_shed`, give up discards on the [`DISCARD_STRIKES`]th slow
    /// one in a row. Whether that happened.
    fn measured(&mut self, bytes: u64, took: u64, may_shed: bool) -> bool {
        self.last = Some((bytes, took));
        let fitted = u128::from(bytes) * u128::from(DISCARD_TARGET) / u128::from(took.max(1));
        let fitted = u64::try_from(fitted).unwrap_or(u64::MAX);
        self.piece = fitted
            .min(bytes.saturating_mul(2))
            .clamp(DISCARD_LEAST, DISCARD_MOST);
        if took <= DISCARD_SLOW {
            self.slow = 0;
            return false;
        }
        self.slow = self.slow.saturating_add(1);
        if self.slow < DISCARD_STRIKES || !may_shed {
            return false;
        }
        self.shed = Some((bytes, took));
        true
    }

    fn outstanding(&self) -> bool {
        !self.waiting.is_empty() || !self.running.is_empty()
    }

    /// Whether a transfer waits for discards or a shutdown hurries them.
    fn urgent(&self) -> bool {
        !self.held.is_empty() || self.hurried
    }

    /// The batches the disk may run at once: one in the background, which
    /// a guest read may come to wait behind, and [`DISCARD_COMMANDS`] while
    /// urgent.
    fn batches(&self) -> usize {
        if self.urgent() { DISCARD_COMMANDS } else { 1 }
    }

    /// When the next batch may go at counter `now`, at the earliest: at once
    /// while a transfer waits for discards, else once guest transfers have
    /// paused for [`DISCARD_QUIET`].
    fn due(&self, now: u64) -> u64 {
        match self.active {
            Some(active) if self.held.is_empty() && !self.hurried => {
                active.saturating_add(DISCARD_QUIET)
            }
            _ => now,
        }
    }
}

pub(super) struct HostDisk {
    device: Box<dyn BlockDevice>,
    /// The running transfers: each one's key, when it was submitted in
    /// counter ticks, and its commands.
    running: Vec<(u32, u64, Submission)>,
    /// The running writes' keys and block ranges, which no discard passes.
    writing: Vec<(u32, u64, u64)>,
    /// What a transfer took, a moving average in counter ticks.
    latency: u64,
    discards: Discards,
}

impl HostDisk {
    pub(super) fn new(device: Box<dyn BlockDevice>) -> Self {
        Self {
            device,
            running: Vec::new(),
            writing: Vec::new(),
            latency: FIRST_LATENCY,
            discards: Discards::default(),
        }
    }

    pub(super) fn identity(&self) -> &Identity {
        self.device.identity()
    }

    /// The host disk has a volatile write cache, so a write is durable
    /// only after a flush.
    pub(super) fn write_cache(&self) -> bool {
        self.identity().features.write_cache
    }

    /// Deallocating host blocks leaves them reading as zeros for good: the
    /// disk's Write Zeroes may deallocate.
    pub(super) fn deallocates_zeroed(&self) -> bool {
        self.identity()
            .features
            .write_zeroes
            .is_some_and(|zeroes| zeroes.unmap)
    }

    #[cfg(test)]
    pub(super) fn read(&mut self, address: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.device.read(address, buffer)
    }
    #[cfg(test)]
    pub(super) fn write(&mut self, address: u64, data: &[u8]) -> Result<(), BlockError> {
        self.device.write(address, data)
    }

    /// Tell the disk to deallocate `blocks` blocks from `lba`, in the
    /// background, if it takes that, has not refused it and was not too
    /// slow at it.
    pub(super) fn discard(&mut self, lba: u64, blocks: u64) {
        if self.identity().features.discard.is_some()
            && self.discards.failed.is_none()
            && self.discards.shed.is_none()
        {
            self.discards.add(lba, lba + blocks);
        }
    }

    /// Send the waiting discards without a pause in transfers while
    /// `hurried`: the controller's shutdown waits for them.
    pub(super) fn hurry_discards(&mut self, hurried: bool) {
        self.discards.hurried = hurried;
    }

    /// Forget the discards not yet sent, and say how many runs they were.
    /// Discards are hints (NVMe 1.4 section 6.7.1.1), and the disk keeps
    /// those blocks' old contents. The batches on the disk still complete.
    pub(super) fn abandon_discards(&mut self) -> usize {
        let runs = self.discards.waiting.len();
        self.discards.waiting.clear();
        self.discards.abandoned += runs as u64;
        runs
    }

    /// Whether background discards remain: they need [`Self::poll`] though
    /// no command waits for them.
    pub(super) fn discarding(&self) -> bool {
        self.discards.outstanding()
    }

    /// The blocks `request` writes, if it writes.
    fn written_blocks(&self, request: &Request) -> Option<(u64, u64)> {
        match request {
            Request::Write { lba, data, .. } => {
                let block = u64::from(self.identity().block_size);
                Some((*lba, *lba + data.len() as u64 / block))
            }
            Request::WriteZeroes { lba, blocks, .. } => Some((*lba, *lba + *blocks)),
            _ => None,
        }
    }

    /// Whether `request` must wait for discards. A write takes its blocks
    /// back whatever the answer.
    fn held(&mut self, request: &Request) -> bool {
        match request {
            Request::Write { .. } | Request::WriteZeroes { .. } => {
                let (start, end) = self.written_blocks(request).expect("a write");
                self.discards.written(start, end)
            }
            Request::Flush => {
                self.discards.outstanding() && self.deallocates_zeroed() && !self.discards.lapsed
            }
            _ => false,
        }
    }

    /// Submit `request` under `key` at counter `now`: its result if the
    /// first poll finishes it, else `None` until
    /// [`Self::poll`] gives it. Synchronous providers can also yield after
    /// the block layer's command admission limit.
    pub(super) fn submit(&mut self, key: u32, request: Request, now: u64) -> Option<Finished> {
        if self.held(&request) {
            self.discards.held.push((key, now, request));
            return None;
        }
        self.discards.active = Some(now);
        let written = self.written_blocks(&request);
        let mut submission = match Submission::new(request, self.device.identity()) {
            Ok(submission) => submission,
            Err(error) => return Some(Err(error)),
        };
        if let Some(done) = submission.poll(&mut *self.device) {
            return Some(done);
        }
        self.running.push((key, now, submission));
        if let Some((start, end)) = written {
            self.writing.push((key, start, end));
        }
        None
    }

    /// The transfers the disk has completed by counter `now`, by key; never
    /// waits.
    pub(super) fn poll(&mut self, now: u64) -> Vec<(u32, Finished)> {
        let mut done = Vec::new();
        let mut index = 0;
        while index < self.running.len() {
            let Some(finished) = self.running[index].2.poll(&mut *self.device) else {
                index += 1;
                continue;
            };
            let (key, started, _) = self.running.swap_remove(index);
            self.writing.retain(|&(writer, _, _)| writer != key);
            let took = now.saturating_sub(started);
            self.latency = (self.latency * 7 + took) / 8;
            self.discards.active = Some(now);
            done.push((key, finished));
        }
        self.advance_discards(&mut done, now);
        done
    }

    /// Start discard batches and reap those the disk completed: in the
    /// background one, once guest transfers paused (`Discards::due`) and
    /// none runs; while urgent, a new one in each free slot of
    /// [`DISCARD_COMMANDS`], so a batch waits for no other. A batch that ran
    /// alone sizes the next ([`Discards::measured`]). Then resubmit
    /// the transfers that waited: a write beside the running batches goes,
    /// a flush waits on until none remains or it waited [`DISCARD_HOLD`].
    /// One call starts at most
    /// [`DISCARD_COMMANDS`] batches: a disk that runs each command where it
    /// is asked runs them on the calling PE, under the devices' lock, so the
    /// rest wait for the next poll.
    fn advance_discards(&mut self, done: &mut Vec<(u32, Finished)>, now: u64) {
        let block = u64::from(self.identity().block_size);
        // The guest was told deallocated blocks stay zeros: its discards
        // reach the disk, however slowly.
        let zeroed = self.deallocates_zeroed();
        let mut started = 0;
        loop {
            started += self.start_discards(now, DISCARD_COMMANDS - started);
            let before = self.discards.running.len();
            let mut index = 0;
            while index < self.discards.running.len() {
                let Some(result) = self.discards.running[index]
                    .submission
                    .poll(&mut *self.device)
                else {
                    index += 1;
                    continue;
                };
                let batch = self.discards.running.swap_remove(index);
                if let Err(error) = result {
                    self.refuse_discards(error);
                } else if batch.alone {
                    let blocks: u64 = batch.extents.iter().map(|extent| extent.blocks).sum();
                    let took = now.saturating_sub(batch.started);
                    if self.discards.measured(blocks * block, took, !zeroed) {
                        self.abandon_discards();
                    }
                }
            }
            // A freed slot takes the next batch at once, up to this call's
            // bound; in the background only on a disk that queues commands,
            // as one that runs them where asked runs them under the lock.
            let freed = self.discards.running.len() < before;
            if !(freed
                && started < DISCARD_COMMANDS
                && (self.discards.urgent() || self.device.depth() != 0)
                && !self.discards.waiting.is_empty())
            {
                break;
            }
        }
        // A transfer never waits for discards near the guest's timeout:
        // the running ones complete, and those not yet sent go on in the
        // background where they keep deallocated blocks zeros, and are given
        // up elsewhere.
        if self
            .discards
            .held
            .iter()
            .any(|&(_, submitted, _)| now.saturating_sub(submitted) >= DISCARD_HOLD)
        {
            match zeroed {
                true => self.discards.lapsed = true,
                false => {
                    self.abandon_discards();
                }
            }
        }
        if !self.discards.outstanding() {
            self.discards.lapsed = false;
        }
        for (key, started, request) in core::mem::take(&mut self.discards.held) {
            if let Some(finished) = self.submit(key, request, started) {
                done.push((key, finished));
            }
        }
    }

    /// Start up to `most` discard batches due at counter `now`, each one
    /// command of as many ranges as the disk takes and at most
    /// [`Discards::piece`] bytes; how many started.
    fn start_discards(&mut self, now: u64, most: usize) -> usize {
        let Some(limits) = self.identity().features.discard else {
            return 0;
        };
        if !self.discards.urgent() && (!self.running.is_empty() || now < self.discards.due(now)) {
            return 0;
        }
        let urgent = self.discards.urgent();
        let piece = match urgent {
            true => self.discards.piece.max(DISCARD_URGENT),
            false => self.discards.piece,
        };
        let budget = (piece / u64::from(self.identity().block_size)).max(1);
        let writing: Vec<(u64, u64)> = self
            .writing
            .iter()
            .map(|&(_, start, end)| (start, end))
            .collect();
        let mut started = 0;
        while started < most
            && self.discards.running.len() < self.discards.batches()
            && self.discards.failed.is_none()
        {
            let extents =
                self.discards
                    .batch(limits.ranges.max(1), limits.blocks, budget, &writing);
            if extents.is_empty() {
                break;
            }
            // A disk that runs commands where asked completes them before
            // the counter moves, so its time measures nothing; urgent
            // commands run beside each other, so theirs measures them all.
            let alone = self.device.depth() != 0
                && !urgent
                && self.running.is_empty()
                && self.discards.running.is_empty();
            let request = Request::Discard {
                extents: extents.clone(),
            };
            match Submission::new(request, self.device.identity()) {
                Ok(submission) => {
                    self.discards.running.push(Batch {
                        submission,
                        extents,
                        started: now,
                        alone,
                    });
                    started += 1;
                }
                Err(error) => self.refuse_discards(error),
            }
        }
        started
    }

    /// The disk refused a discard: name why ([`Self::diagnostic`]) and
    /// send no more.
    fn refuse_discards(&mut self, error: BlockError) {
        self.discards.failed = Some(error);
        self.discards.waiting.clear();
    }

    /// Whether a running transfer's completion is posted, ready for
    /// [`Self::poll`]; a look at memory alone.
    pub(super) fn posted(&self) -> bool {
        (!self.running.is_empty() || !self.discards.running.is_empty()) && self.device.posted()
    }

    /// The running transfers at counter `now`, oldest first, each as
    /// key/age in counter ticks/commands the disk runs for it (none while
    /// it waits for room), then the disk's latency and readiness.
    pub(super) fn diagnostic(&self, now: u64) -> String {
        let mut running: Vec<_> = self.running.iter().collect();
        running.sort_unstable_by_key(|(_, started, _)| *started);
        let oldest = running
            .iter()
            .take(super::DIAGNOSTIC_COMMANDS)
            .map(|(key, started, submission)| {
                let age = now.saturating_sub(*started);
                format!("{key:#x}/{age}/{}", submission.started())
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "running={} oldest=[{oldest}] latency={} room={} posted={} signals={} \
             discards={}+{} held={} abandoned={} refused={:?} piece={}KiB last={} shed={}",
            self.running.len(),
            self.latency,
            self.device.room(),
            self.device.posted(),
            self.device.signals(),
            self.discards.waiting.len(),
            self.discards
                .running
                .iter()
                .map(|batch| batch.extents.len())
                .sum::<usize>(),
            self.discards.held.len(),
            self.discards.abandoned,
            self.discards.failed,
            self.discards.piece >> 10,
            measure(self.discards.last),
            measure(self.discards.shed),
        )
    }

    /// When to look for the running transfers' completions next, at counter
    /// `now`: half the latency after the oldest began, then more often; for
    /// a disk that signals them, only once a signal is overdue.
    pub(super) fn next_look(&self, now: u64) -> u64 {
        // Only discards wait for the pause that sends them.
        if self.running.is_empty() && self.discards.running.is_empty() {
            let due = self.discards.due(now);
            if due > now {
                return due;
            }
        }
        let oldest = self.running.iter().map(|(_, started, _)| *started).min();
        if self.device.signals() {
            let overdue = (self.latency * 4).max(LOST_SIGNAL);
            let due = oldest.unwrap_or(now).saturating_add(overdue);
            return if now < due {
                due
            } else {
                now.saturating_add(LOST_SIGNAL)
            };
        }
        let half = oldest.unwrap_or(now).saturating_add(self.latency / 2);
        if now < half {
            half
        } else {
            now.saturating_add((self.latency / 8).max(SHORTEST_POLL))
        }
    }
}

/// A measured discard command as `<KiB>KiB/<us>us`, or `none`.
fn measure(command: Option<(u64, u64)>) -> String {
    match command {
        Some((bytes, ticks)) => format!("{}KiB/{}us", bytes >> 10, ticks / 24),
        None => String::from("none"),
    }
}

#[cfg(test)]
mod tests;
