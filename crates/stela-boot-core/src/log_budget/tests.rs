use super::*;
use alloc::{string::String, vec::Vec};

/// What a gate hands its log, with `|` for each line's stamp.
#[derive(Default)]
struct Log(Vec<u8>);

impl Log {
    fn text(&self) -> String {
        String::from_utf8(self.0.clone()).unwrap()
    }

    fn lines(&self) -> Vec<String> {
        self.text().lines().map(String::from).collect()
    }
}

fn write(gate: &mut Gate, log: &mut Log, text: &str, now: Option<u64>, exempt: bool) {
    let mut out = |piece: Piece<'_>| match piece {
        Piece::LineStart => log.0.push(b'|'),
        Piece::Bytes(bytes) => log.0.extend_from_slice(bytes),
    };
    gate.write(text.as_bytes(), now, exempt, &mut out);
    gate.end(now, &mut out);
}

fn say(gate: &mut Gate, log: &mut Log, now: u64, all: bool) {
    gate.say(Some(now), all, &mut |piece| match piece {
        Piece::LineStart => log.0.push(b'|'),
        Piece::Bytes(bytes) => log.0.extend_from_slice(bytes),
    });
}

fn key(line: &str) -> String {
    String::from_utf8(family(line.as_bytes()).as_bytes().to_vec()).unwrap()
}

#[test]
fn a_line_s_family_is_its_leading_capital_words_or_a_driver_s_first_word() {
    assert_eq!(key("GUEST| AppleARMRTC started\n"), "GUEST|");
    assert_eq!(key("GUEST|text right after\n"), "GUEST|");
    assert_eq!(key("DCP payload A457 +0x0000 [3c, d2]\n"), "DCP payload");
    assert_eq!(key("DCP SWAPS presented=3\n"), "DCP SWAPS");
    assert_eq!(
        key("HOST FIRMWARE RECORD {\"bytes\":1}\n"),
        "HOST FIRMWARE RECORD"
    );
    assert_eq!(
        key("HOST DEVICE PCI 0000:00:04.0 start\n"),
        "HOST DEVICE PCI"
    );
    assert_eq!(key("GPU DRM ioctl AMDGPU_INFO (0x40206445)\n"), "GPU DRM");
    assert_eq!(key("ACPI: RSDP 0x000000007F97E014\n"), "ACPI RSDP");
    assert_eq!(key("STEP o Starting the GPU ms=1\n"), "STEP o");
    assert_eq!(key("STEP Starting the GPU t=1\n"), "STEP");
    assert_eq!(key("nouveau 0000:01:00.0: timer: stalled\n"), "nouveau");
    // A Linux line's level and driver: its errors are not its warnings.
    assert_eq!(
        key("LINUX <4> nouveau 0000:02:00.0: timeout\n"),
        "LINUX <4> nouveau"
    );
    assert_eq!(
        key("LINUX <3> nouveau 0000:02:00.0: error\n"),
        "LINUX <3> nouveau"
    );
    assert_eq!(
        key("LINUX WARN ../../third_party/linux/drivers\n"),
        "LINUX WARN"
    );
    assert_eq!(key("[drm] amdgpu kernel modesetting\n"), "[drm]");
    // No name: indented lines, other text.
    assert_eq!(key(" fills_lower/fills_upper 0:1\n"), "");
    assert_eq!(key("Some text: 12\n"), "");
    // A key never runs past its bytes.
    assert!(
        family(b"ABCDEFGHIJKLM NOPQRSTUVWXYZ ABCDEF\n")
            .as_bytes()
            .len()
            <= KEY_BYTES
    );
}

#[test]
fn a_flood_keeps_its_burst_then_is_counted_while_other_families_are_kept() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let flood = "LINUX <4> nouveau 0000:02:00.0: timeout waiting for the GPU's timer\n";
    // 2000 lines of 70 bytes in 2 s, another family's line every 100.
    for index in 0..2000u64 {
        write(&mut gate, &mut log, flood, Some(index), false);
        if index % 100 == 0 {
            write(
                &mut gate,
                &mut log,
                "HOST DISK IO reads=1\n",
                Some(index),
                false,
            );
        }
    }
    let lines = log.lines();
    let kept = lines
        .iter()
        .filter(|line| line.contains("waiting for the GPU"))
        .count();
    // The burst plus what 2 s refill, never more.
    let room = (BURST_BYTES + 2 * RATE_BYTES_PER_SECOND) as usize;
    assert!(kept * flood.len() <= room + flood.len(), "{kept}");
    assert!(kept * flood.len() >= BURST_BYTES as usize, "{kept}");
    assert_eq!(
        lines.iter().filter(|line| line.contains("DISK IO")).count(),
        20
    );
    // Nothing says it yet: the flood goes on.
    assert!(!log.text().contains("THINNED"));
    // A second after its last line, the next line says what was left out.
    write(&mut gate, &mut log, "USB added 1-2\n", Some(3000), false);
    let lines = log.lines();
    let thinned = &lines[lines.len() - 2];
    assert!(
        thinned.starts_with("|BOOT LOG THINNED \"LINUX <4> nouveau\": "),
        "{thinned}"
    );
    let left = 2000 - kept;
    assert!(
        thinned.contains(&alloc::format!(
            ": {left} lines, {} bytes left out",
            left * flood.len()
        )),
        "{thinned}"
    );
    assert_eq!(lines.last().unwrap(), "|USB added 1-2");
    // Said once.
    write(&mut gate, &mut log, "USB added 1-3\n", Some(5000), false);
    assert_eq!(log.text().matches("THINNED").count(), 1);
}

#[test]
fn a_family_whose_bucket_refills_writes_again_after_saying_its_count() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let line = "DCP payload A457 +0x0000 [00, 00, 00, 00, 00, 00, 00, 00]\n";
    let mut now = 0;
    while !log.text().contains("THINNED") && now < 100_000 {
        write(&mut gate, &mut log, line, Some(now / 1000), false);
        now += 1;
        if gate.next_due != u64::MAX {
            break;
        }
    }
    // 10 s later the bucket holds 2560 bytes: the family's next line is
    // kept, after its count, in that order.
    let at = 20_000;
    write(&mut gate, &mut log, line, Some(at), false);
    let lines = log.lines();
    assert!(lines[lines.len() - 2].starts_with("|BOOT LOG THINNED \"DCP payload\": 1 lines"));
    assert_eq!(
        lines[lines.len() - 1],
        alloc::format!("|{}", line.trim_end())
    );
}

#[test]
fn a_long_flood_is_slowed_to_its_long_rate() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let line = format_line(100);
    // An hour of 10 lines a second, 1 KB/s.
    let mut kept_late = 0;
    for tenth in 0..36_000u64 {
        let before = log.0.len();
        write(&mut gate, &mut log, &line, Some(tenth * 100), false);
        if tenth >= 8_000 && log.0.len() > before && log.0.ends_with(line.as_bytes()) {
            kept_late += 1;
        }
    }
    // Its burst lasts about 88 s; ten minutes of thinning later, from about
    // 690 s, it gets 32 B/s, not 256 B/s: one line in about 3 s.
    let late_bytes = kept_late * line.len();
    let expected = (LONG_RATE_BYTES_PER_SECOND as usize) * (36_000 - 8_000) / 10;
    assert!(
        late_bytes <= expected + 4 * line.len(),
        "{late_bytes} {expected}"
    );
    assert!(
        late_bytes + 4 * line.len() >= expected,
        "{late_bytes} {expected}"
    );
    // Its count is said every minute while it goes on, at line starts.
    let said = log.text().matches("THINNED").count();
    assert!((55..=62).contains(&said), "{said}");
    // A calm family is refilled to its burst and back to the full rate.
    let bucket = gate.buckets.iter().find(|bucket| bucket.used).unwrap();
    assert!(bucket.thinned_since.is_some());
    let mut quiet = Gate::new();
    let mut quiet_log = Log::default();
    write(&mut quiet, &mut quiet_log, &line, Some(0), false);
    let bucket = quiet.buckets.iter().find(|bucket| bucket.used).unwrap();
    assert!(bucket.thinned_since.is_none());
}

fn format_line(bytes: usize) -> String {
    let mut line = String::from("nouveau 0000:01:00.0: WARN ");
    while line.len() < bytes - 1 {
        line.push('x');
    }
    line.push('\n');
    line
}

#[test]
fn exempt_lines_and_lines_before_the_clock_are_kept_and_cost_nothing() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let line = format_line(1000);
    for _ in 0..200 {
        write(&mut gate, &mut log, &line, Some(5), true);
        write(&mut gate, &mut log, &line, None, false);
    }
    assert_eq!(log.lines().len(), 400);
    // The family's bucket was never touched by them.
    assert!(gate.buckets.iter().all(|bucket| !bucket.used));
    write(&mut gate, &mut log, &line, Some(5), false);
    assert_eq!(log.lines().len(), 401);
}

#[test]
fn a_line_written_in_pieces_is_decided_by_its_whole_start() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let mut out = |piece: Piece<'_>| match piece {
        Piece::LineStart => log.0.push(b'|'),
        Piece::Bytes(bytes) => log.0.extend_from_slice(bytes),
    };
    gate.write(b"GPU ", Some(1), false, &mut out);
    gate.write(b"DRM ioctl refused", Some(1), false, &mut out);
    gate.write(b": -22\nHOST", Some(1), false, &mut out);
    // A start shorter than a key waits for the write's end.
    assert!(!gate.at_line_start());
    gate.end(Some(1), &mut out);
    gate.write(b" more\n", Some(1), false, &mut out);
    assert_eq!(log.text(), "|GPU DRM ioctl refused: -22\n|HOST more\n");
    let families: Vec<_> = gate
        .buckets
        .iter()
        .filter(|bucket| bucket.used)
        .map(|bucket| String::from_utf8(bucket.key.as_bytes().to_vec()).unwrap())
        .collect();
    assert_eq!(families, ["GPU DRM", "HOST"]);
}

#[test]
fn every_count_is_said_at_a_stop_and_none_mid_line() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let line = format_line(1024);
    for _ in 0..70 {
        write(&mut gate, &mut log, &line, Some(10), false);
    }
    let piece = |gate: &mut Gate, log: &mut Log, bytes: &[u8]| {
        gate.write(bytes, Some(10), true, &mut |piece| match piece {
            Piece::LineStart => log.0.push(b'|'),
            Piece::Bytes(bytes) => log.0.extend_from_slice(bytes),
        })
    };
    piece(&mut gate, &mut log, b"STELA FATAL half");
    say(&mut gate, &mut log, 10, true);
    assert!(!log.text().contains("THINNED"));
    piece(&mut gate, &mut log, b" a line\n");
    say(&mut gate, &mut log, 10, true);
    let text = log.text();
    assert!(
        text.ends_with(
            "|STELA FATAL half a line\n|BOOT LOG THINNED \"nouveau\": 6 lines, 6144 bytes left \
             out of this log over 0 s, past the family's 64 KiB at once and 256 B/s (the serial \
             port has them)\n"
        ),
        "{text}"
    );
}

#[test]
fn families_past_the_table_share_its_last_bucket() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    // 63 families thinned at once fill the named buckets.
    let big = "x".repeat(70 << 10);
    for index in 0..FAMILIES as u64 {
        let line = alloc::format!("FAMILY{index} {big}\n");
        write(&mut gate, &mut log, &line, Some(0), false);
        write(&mut gate, &mut log, &line, Some(0), false);
    }
    let shared = &gate.buckets[FAMILIES - 1];
    assert!(shared.used && shared.key.as_bytes().is_empty());
    say(&mut gate, &mut log, 0, true);
    assert!(
        log.text()
            .contains("\"lines with no family name, or of families past the 63")
    );
    // Once a family is idle and full again, a new one takes its bucket.
    write(
        &mut gate,
        &mut log,
        "NEWFAMILY line\n",
        Some(10_000_000),
        false,
    );
    assert!(
        gate.buckets[..FAMILIES - 1]
            .iter()
            .any(|bucket| bucket.key == family(b"NEWFAMILY line\n"))
    );
}

/// The guest's console writes a macOS boot's lines whole, and a flood of it
/// is thinned past a megabyte.
#[test]
fn the_guest_console_keeps_a_boot_s_lines_and_thins_a_flood_past_its_burst() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let line = "GUEST| AppleSEPKeyStore: operation failed (pid: 645 sel: 7 ret: e00002c7)\n";
    // 700 KB in the first minute, as a macOS boot writes.
    let count = 700_000 / line.len() as u64;
    for index in 0..count {
        write(
            &mut gate,
            &mut log,
            line,
            Some(index * 60_000 / count),
            false,
        );
    }
    assert_eq!(log.lines().len() as u64, count);
    // A flood after it is thinned once the megabyte is spent.
    for index in 0..20_000 {
        write(&mut gate, &mut log, line, Some(60_000 + index), false);
    }
    say(&mut gate, &mut log, 100_000, true);
    let text = log.text();
    let kept = text.matches("AppleSEPKeyStore").count() * line.len();
    assert!(kept < (GUEST_BURST_BYTES + 100 * RATE_BYTES_PER_SECOND) as usize + 2 * line.len());
    assert!(
        text.contains("past the family's 1024 KiB at once"),
        "{}",
        &text[text.len() - 300..]
    );
}

/// A guest that floods its console and then panics: its panic report, from
/// the `panic(` line on, is kept past the console's spent budget, where it
/// was left out and the log lacked the one report that named the failure.
/// The flood before it stays thinned.
#[test]
fn a_guest_panic_passes_a_spent_console_budget() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let flood = alloc::format!("GUEST| {}\n", "x".repeat(1000));
    for _ in 0..GUEST_BURST_BYTES / 1000 + 10 {
        write(&mut gate, &mut log, &flood, Some(1000), false);
    }
    let before = log.0.len();
    write(&mut gate, &mut log, &flood, Some(1000), false);
    assert_eq!(log.0.len(), before, "the flood is thinned");
    let panic = [
        "GUEST| panic(cpu 2 caller 0xfffffe0012345678): \"Kernel data abort\"\n",
        "GUEST| Debugger message: panic\n",
        "GUEST|   lr: 0xfffffe0012345678  fp: 0xfffffe8001234560\n",
    ];
    for line in panic {
        write(&mut gate, &mut log, line, Some(1000), false);
    }
    let text = log.text();
    for line in panic {
        assert!(text.contains(&alloc::format!("|{line}")), "{line}");
    }
    assert!(begins_guest_panic(b"GUEST|panic(cpu 0"));
    assert!(!begins_guest_panic(b"GUEST| kernel panic"));
}

/// The lines the next start reads for how a boot ended pass a spent
/// budget, where a restart's line could be left out and the next start
/// said the boot hung.
#[test]
fn a_boot_s_end_passes_a_spent_budget() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let line = alloc::format!("STELA RESTART code=0x15 {}\n", "x".repeat(1000));
    for _ in 0..BURST_BYTES / 1000 + 10 {
        write(&mut gate, &mut log, &line, Some(1000), false);
    }
    let kept = log.lines().len();
    write(&mut gate, &mut log, &line, Some(1000), false);
    write(
        &mut gate,
        &mut log,
        "POWER powering off through the firmware\n",
        Some(1000),
        false,
    );
    assert_eq!(log.lines().len(), kept + 2);
    assert!(names_an_end(b"STELA FATAL x"));
    assert!(!names_an_end(b"STEP Restarting"));
}

/// A long install's step lines all pass, where they shared one family's
/// budget and the next start could name an older step as the last.
#[test]
fn every_step_line_passes_a_spent_budget() {
    let (mut gate, mut log) = (Gate::new(), Log::default());
    let detail = alloc::format!("STEP DETAIL Copying {} ms=1\n", "x".repeat(1000));
    let long = "y".repeat(1000);
    for step in 0..BURST_BYTES / 1000 + 10 {
        write(
            &mut gate,
            &mut log,
            &alloc::format!("STEP Installing part {step} {long} t=1.000\n"),
            Some(1000),
            false,
        );
        write(&mut gate, &mut log, &detail, Some(1000), false);
        write(
            &mut gate,
            &mut log,
            &alloc::format!("STEP o Installing part {step} ms=1\n"),
            Some(1000),
            false,
        );
    }
    let last = log.lines().len();
    write(
        &mut gate,
        &mut log,
        "STEP Restarting the PC t=2.000\n",
        Some(1000),
        false,
    );
    assert_eq!(log.lines().len(), last + 1);
    assert_eq!(last, 3 * (BURST_BYTES as usize / 1000 + 10));
    assert!(names_a_step(b"STEP WATCHDOG on"));
    assert!(!names_a_step(b"STEPS x"));
}
