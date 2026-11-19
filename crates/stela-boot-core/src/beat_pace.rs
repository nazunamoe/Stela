//! How often a running macOS writes its heartbeat. The full beat is a
//! hundred lines or so, 14 to 25 KiB on the PCs of the 0.5.0 reports and
//! 36 to 98 KiB (46 at the median) on an 8-PE desktop measured 2026-11-18:
//! every [`BEAT_SECONDS`] it writes about 2.8 MB in ten minutes. A boot's
//! first [`FULL_RATE_SECONDS`] keep that rate, which is when it starts,
//! signs in and is measured. Until [`LONG_RUN_SECONDS`] the full beat comes
//! every [`LATE_BEAT_SECONDS`], about 3.4 MB an hour with the rest of the
//! lines, and after that every [`LONG_BEAT_SECONDS`], about 0.8 MB an
//! hour: so a day's run is about 24 MB, which the USB's log holds whole
//! (`stela_runtime::pc::boot_log::FILE_BYTES`), and its last hour is never
//! overwritten. Each full beat covers the whole interval since the last,
//! and a short line every [`BEAT_SECONDS`] between says the guest still
//! runs and where. Every [`IDENT_SECONDS`] one line repeats what the boot
//! runs on ([`crate::boot_ident`]).
//!
//! `--profile` keeps the full beat every [`BEAT_SECONDS`]: its reports are
//! read by the interval.

/// Seconds between heartbeat lines, full or short.
pub const BEAT_SECONDS: u64 = 10;
/// Seconds of a boot's run during which every beat is a full one.
pub const FULL_RATE_SECONDS: u64 = 600;
/// Seconds between full beats after that.
pub const LATE_BEAT_SECONDS: u64 = 60;
/// Seconds of a boot's run after which full beats are further apart.
pub const LONG_RUN_SECONDS: u64 = 3600;
/// Seconds between full beats after [`LONG_RUN_SECONDS`].
pub const LONG_BEAT_SECONDS: u64 = 600;
/// Seconds between the boot's identity lines.
pub const IDENT_SECONDS: u64 = 600;

/// What the heartbeat writes now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    Nothing,
    /// The one line that says the guest runs and where.
    Short,
    /// Every report.
    Full,
}

/// Seconds between full beats for a boot that has run `ran` seconds.
pub const fn full_seconds(ran: u64, profiling: bool) -> u64 {
    if profiling || ran < FULL_RATE_SECONDS {
        BEAT_SECONDS
    } else if ran < LONG_RUN_SECONDS {
        LATE_BEAT_SECONDS
    } else {
        LONG_BEAT_SECONDS
    }
}

/// What is due for a boot that has run `ran` ticks of a `hz` counter,
/// `since_full` ticks after its last full beat and `since_line` after its
/// last beat line of either kind. A full beat waits for a `quiet` boot
/// processor for one more [`BEAT_SECONDS`] at most, as its reports take
/// the processor a while; the short line does not wait.
pub const fn due(
    ran: u64,
    since_full: u64,
    since_line: u64,
    hz: u64,
    profiling: bool,
    quiet: bool,
) -> Due {
    let beat = BEAT_SECONDS * hz;
    let full = full_seconds(ran / if hz == 0 { 1 } else { hz }, profiling) * hz;
    if since_full >= full && (quiet || since_full >= full + beat) {
        Due::Full
    } else if full > beat && since_line >= beat && since_full + beat / 2 < full {
        // Not just before a full beat, whose first line says the same.
        Due::Short
    } else {
        Due::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ: u64 = 1000;

    /// The lines of a boot that runs `seconds`, as `(second, due)`, with a
    /// boot processor that is quiet when `quiet` says so.
    fn run(seconds: u64, profiling: bool, quiet: impl Fn(u64) -> bool) -> Vec<(u64, Due)> {
        let (mut full, mut line, mut lines) = (0, 0, Vec::new());
        for now in (0..=seconds * HZ).step_by(100) {
            let due = due(now, now - full, now - line, HZ, profiling, quiet(now / HZ));
            match due {
                Due::Full => (full, line) = (now, now),
                Due::Short => line = now,
                Due::Nothing => continue,
            }
            lines.push((now / HZ, due));
        }
        lines
    }

    #[test]
    fn the_first_ten_minutes_beat_in_full_and_later_ones_once_a_minute() {
        let lines = run(900, false, |_| true);
        let early: Vec<_> = lines.iter().filter(|(at, _)| *at < 600).collect();
        assert_eq!(early.len(), 59);
        assert!(
            early
                .iter()
                .all(|(at, due)| *due == Due::Full && at % 10 == 0)
        );
        let late: Vec<_> = lines.iter().filter(|(at, _)| *at >= 600).copied().collect();
        // The last full beat of the first ten minutes was at 590: short
        // lines from 600 to 640, the next full beat at 650.
        assert_eq!(
            late[..8],
            [
                (600, Due::Short),
                (610, Due::Short),
                (620, Due::Short),
                (630, Due::Short),
                (640, Due::Short),
                (650, Due::Full),
                (660, Due::Short),
                (670, Due::Short),
            ]
        );
        let full = late.iter().filter(|(_, due)| *due == Due::Full).count();
        let short = late.iter().filter(|(_, due)| *due == Due::Short).count();
        assert_eq!((full, short), (5, 26));
    }

    #[test]
    fn after_an_hour_the_full_beat_comes_every_ten_minutes_with_short_lines_between() {
        let lines = run(3 * 3600, false, |_| true);
        let late: Vec<_> = lines
            .iter()
            .filter(|(at, _)| *at >= LONG_RUN_SECONDS)
            .copied()
            .collect();
        let full: Vec<_> = late
            .iter()
            .filter(|(_, due)| *due == Due::Full)
            .map(|(at, _)| *at)
            .collect();
        // The last full beat of the hour before was at 3590.
        assert_eq!(full[..3], [4190, 4790, 5390]);
        assert!(
            full.windows(2)
                .all(|pair| pair[1] - pair[0] == LONG_BEAT_SECONDS)
        );
        // A line every 10 s, as before: two hours of them, both ends in.
        assert_eq!(late.len(), 721);
        assert!(
            late.windows(2)
                .all(|pair| pair[1].0 - pair[0].0 == BEAT_SECONDS)
        );
        assert_eq!(full_seconds(3599, false), 60);
        assert_eq!(full_seconds(3600, false), 600);
        assert_eq!(full_seconds(36_000, true), 10);
    }

    #[test]
    fn profiling_keeps_every_beat_full() {
        let lines = run(900, true, |_| true);
        assert_eq!(lines.len(), 90);
        assert!(lines.iter().all(|(_, due)| *due == Due::Full));
    }

    #[test]
    fn a_full_beat_waits_one_interval_for_a_quiet_processor_and_short_lines_do_not() {
        let early = run(60, false, |_| false);
        assert_eq!(early[0], (20, Due::Full));
        let late: Vec<_> = run(800, false, |at| at < 600)
            .into_iter()
            .filter(|(at, _)| *at > 600)
            .collect();
        assert_eq!(late[0], (610, Due::Short));
        // Due at 650, written at 660 without a quiet processor, and no
        // short line in between.
        assert_eq!(late[3..5], [(640, Due::Short), (660, Due::Full)]);
        assert_eq!(full_seconds(599, false), 10);
        assert_eq!(full_seconds(600, false), 60);
        assert_eq!(due(0, 0, 0, 0, false, true), Due::Full);
    }
}
