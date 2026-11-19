//! The end of a macOS session whose GPU is lost for good. A lost GPU's
//! service reopens its device for the next client; while that works, macOS
//! goes on. When it does not and no other GPU serves Metal, every new
//! WindowServer is refused and dies, and the screen stays frozen: Stela then
//! stops as a boot without a GPU does, with which GPU was lost and what its
//! kernel driver said of it.
use alloc::{format, string::String, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// The failed reopens in a row after which a lost GPU is taken not to
/// reopen, when its kernel driver did not say that it stopped answering,
/// and the least time from the first of them to the last
/// ([`REOPEN_SPAN_MS`]): a close that found it lost and the clients that
/// follow may try within milliseconds, while its driver may still be
/// resetting it.
pub const REOPEN_ATTEMPTS: u32 = 2;

/// How long reopens of a lost GPU whose driver said nothing must keep
/// failing before it is taken not to reopen. Linux gives a hung job 10 s
/// (amdgpu's `lockup_timeout`, the xe and nouveau schedulers' job timeouts
/// are shorter) before its driver resets the GPU, and the reset takes
/// seconds more; a reopen tried after this has outlived both.
pub const REOPEN_SPAN_MS: u64 = 30_000;

/// What a kernel driver's line says, after its device's name, of a GPU that
/// left the bus (nouveau's `nvkm_device_gone`): after it no reopen can work.
const SILENT: &str = ": stopped answering";

/// What a kernel driver's line says, after its device's name, of a GPU
/// whose MMIO read all ones for a moment and then answered again
/// (nouveau's `nvkm_device_gone`).
const ANSWERS_AGAIN: &str = ": answers again";

/// [`link_event`]'s name for a GPU that stopped answering.
pub const STOPPED_ANSWERING: &str = "stopped answering";

/// What a kernel driver's `line` says of its GPU's link, as the boot log
/// names it beside the port's view: that it stopped answering, or that it
/// answered again after a silence; `None` for any other line.
pub fn link_event(line: &str) -> Option<&'static str> {
    if line.contains(SILENT) {
        Some(STOPPED_ANSWERING)
    } else if line.contains(ANSWERS_AGAIN) {
        Some("answered again after a silence")
    } else {
        None
    }
}

/// Which recovery the port above a GPU that stopped answering still
/// allows, for one boot-log line. Stela tries none of them: the session
/// ends at the GPU's first failed reopen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recovery {
    /// The port could not be read.
    Unknown,
    /// The port above does not answer either.
    PortGone,
    /// The port answers and a secondary bus reset through it is possible.
    BusReset { port: String, link_up: bool },
    /// The port answers, but the fabric would refuse the bus reset.
    NoBusReset { why: &'static str, link_up: bool },
}

impl core::fmt::Display for Recovery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let link = |up: bool| {
            if up {
                "its link is up"
            } else {
                "its link is down"
            }
        };
        match self {
            Self::Unknown => f.write_str("unknown: the port above it is not read"),
            Self::PortGone => f.write_str(
                "none from here: the port above it does not answer either, so the power or the \
                 platform took the whole link, which no reset Stela can make reaches",
            ),
            Self::BusReset { port, link_up } => write!(
                f,
                "possible by a secondary bus reset through port {port} ({}) and a new start of \
                 its driver and its GPU firmware, as Linux's AER recovery resets a bus (nouveau \
                 has no error handler, so Linux does not restart it); Stela does not try it and \
                 the session ends",
                link(*link_up)
            ),
            Self::NoBusReset { why, link_up } => write!(
                f,
                "none bounded: the port answers ({}), but a secondary bus reset through it is \
                 refused: {why}",
                link(*link_up)
            ),
        }
    }
}

/// A GPU that serves Metal, as [`verdict`] reads it.
#[derive(Clone, Copy, Debug)]
pub struct Gpu<'a> {
    /// Its PCI address, as the boot log names it.
    pub name: &'a str,
    /// Whether its device is lost now.
    pub lost: bool,
    /// The reopens that failed in a row since the loss.
    pub failed_reopens: u32,
    /// The milliseconds from the first of them to the last.
    pub failing_ms: u64,
    /// The last such reopen's error.
    pub error: Option<&'a str>,
    /// Its kernel driver's line that it stopped answering, if it gave one.
    pub silent: Option<&'a str>,
}

impl Gpu<'_> {
    /// Whether the GPU is lost and will not reopen: a reopen failed and its
    /// driver said it stopped answering (it then fails every later one), or
    /// [`REOPEN_ATTEMPTS`] failed in a row over at least [`REOPEN_SPAN_MS`].
    /// A lost GPU no reopen was tried on yet, or tried only within that
    /// time, may still come back.
    fn gone(&self) -> bool {
        self.lost
            && match self.silent {
                Some(_) => self.failed_reopens >= 1,
                None => self.failed_reopens >= REOPEN_ATTEMPTS && self.failing_ms >= REOPEN_SPAN_MS,
            }
    }
}

/// Why the boot stops, once no GPU of `gpus` (those that serve Metal) can
/// serve macOS again; `None` while any of them works or may still reopen,
/// and for a boot that has none.
pub fn verdict(gpus: &[Gpu<'_>]) -> Option<String> {
    if gpus.is_empty() || !gpus.iter().all(Gpu::gone) {
        return None;
    }
    let each: Vec<String> = gpus
        .iter()
        .map(|gpu| {
            let tries = match gpu.failed_reopens {
                1 => String::from("1 try"),
                tries => format!("{tries} tries over {} s", gpu.failing_ms / 1000),
            };
            let error = gpu.error.unwrap_or("no error named");
            let said = match gpu.silent {
                Some(line) => format!("Its driver said: {line}"),
                None => String::from("Its driver did not say that it left the bus"),
            };
            format!(
                "the GPU {} was lost while macOS ran and did not reopen ({tries}, last: \
                 {error}). {said}",
                gpu.name
            )
        })
        .collect();
    Some(format!(
        "macOS cannot run without a GPU: {}",
        each.join("; ")
    ))
}

/// The kernel drivers' lines that a GPU stopped answering, the first of each
/// GPU kept for the stop's reason. Written once per slot; read without a
/// lock.
pub struct SilentLines<const SLOTS: usize, const BYTES: usize> {
    lines: [Line<BYTES>; SLOTS],
}

struct Line<const BYTES: usize> {
    /// [`FREE`], [`WRITING`] or [`READY`].
    state: AtomicU8,
    length: AtomicUsize,
    bytes: [AtomicU8; BYTES],
}

const FREE: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

impl<const SLOTS: usize, const BYTES: usize> Default for SilentLines<SLOTS, BYTES> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const SLOTS: usize, const BYTES: usize> SilentLines<SLOTS, BYTES> {
    pub const fn new() -> Self {
        Self {
            lines: [const {
                Line {
                    state: AtomicU8::new(FREE),
                    length: AtomicUsize::new(0),
                    bytes: [const { AtomicU8::new(0) }; BYTES],
                }
            }; SLOTS],
        }
    }

    /// Keeps `line` if it says a device stopped answering and a slot is
    /// free, cut to the slot's size at a character boundary; whether it
    /// says so.
    pub fn observe(&self, line: &str) -> bool {
        if !line.contains(SILENT) {
            return false;
        }
        let mut end = line.len().min(BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        for slot in &self.lines {
            if slot
                .state
                .compare_exchange(FREE, WRITING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                for (byte, value) in slot.bytes.iter().zip(line[..end].bytes()) {
                    byte.store(value, Ordering::Relaxed);
                }
                slot.length.store(end, Ordering::Relaxed);
                slot.state.store(READY, Ordering::Release);
                break;
            }
        }
        true
    }

    /// The kept line that names the GPU at `address`.
    pub fn of(&self, address: &str) -> Option<String> {
        self.lines.iter().find_map(|slot| {
            if slot.state.load(Ordering::Acquire) != READY {
                return None;
            }
            let length = slot.length.load(Ordering::Relaxed);
            let bytes: Vec<u8> = slot.bytes[..length]
                .iter()
                .map(|byte| byte.load(Ordering::Relaxed))
                .collect();
            String::from_utf8(bytes)
                .ok()
                .filter(|line| line.contains(address))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GONE: &str = "nouveau 0000:02:00.0: stopped answering: BOOT_0 read ffffffff for 1004 \
                        ms; config vendor ffff command ffff status ffff pmcsr ffff link status \
                        ffff (vendor ffff link status ffff when it began, 0 silences ended \
                        before); its waits end at once from here";

    fn gpu(lost: bool, failed_reopens: u32, silent: Option<&'static str>) -> Gpu<'static> {
        Gpu {
            name: "0000:02:00.0",
            lost,
            failed_reopens,
            failing_ms: u64::from(failed_reopens.saturating_sub(1)) * REOPEN_SPAN_MS,
            error: (failed_reopens != 0).then_some("DeviceLost"),
            silent,
        }
    }

    /// Both of nouveau's link lines are named for the port's line, and no
    /// other line is.
    #[test]
    fn the_lines_that_go_with_the_ports_view_are_the_silence_and_its_end() {
        assert_eq!(link_event(GONE), Some("stopped answering"));
        assert_eq!(
            link_event(
                "nouveau 0000:02:00.0: answers again: BOOT_0 read ffffffff for 12 to 20 ms \
                 (config vendor 10de link status 1043 then; ...); taken for a transient, said \
                 once"
            ),
            Some("answered again after a silence")
        );
        assert_eq!(
            link_event("nouveau 0000:02:00.0: timer: stalled at ffffffffffffffff"),
            None
        );
    }

    #[test]
    fn a_working_gpu_or_one_that_may_still_reopen_goes_on() {
        assert_eq!(verdict(&[]), None);
        assert_eq!(verdict(&[gpu(false, 0, None)]), None);
        // Lost, its connections still open: no reopen was tried.
        assert_eq!(verdict(&[gpu(true, 0, None)]), None);
        assert_eq!(verdict(&[gpu(true, 0, Some(GONE))]), None);
        // One failed reopen with no word from the driver may be a passing
        // refusal: the next client tries again.
        assert_eq!(verdict(&[gpu(true, 1, None)]), None);
        // Nor do many within the time a driver's reset may take: the closes
        // after a loss and the clients that follow try within moments.
        let soon = Gpu {
            failing_ms: REOPEN_SPAN_MS - 1,
            ..gpu(true, 9, None)
        };
        assert_eq!(verdict(&[soon]), None);
        // A reopen that gave a device counts none, whatever came before.
        assert_eq!(verdict(&[gpu(false, 0, Some(GONE))]), None);
    }

    #[test]
    fn a_gpu_off_the_bus_ends_the_session_at_its_first_failed_reopen() {
        let reason = verdict(&[gpu(true, 1, Some(GONE))]).expect("stops");
        assert!(reason.starts_with(
            "macOS cannot run without a GPU: the GPU 0000:02:00.0 was lost while macOS ran and \
             did not reopen (1 try, last: DeviceLost). Its driver said: nouveau 0000:02:00.0: \
             stopped answering: BOOT_0 read ffffffff for 1004 ms; config vendor ffff"
        ));
    }

    #[test]
    fn a_gpu_its_driver_said_nothing_of_ends_it_once_reopens_failed_long_enough() {
        let reason = verdict(&[gpu(true, REOPEN_ATTEMPTS, None)]).expect("stops");
        assert!(reason.contains("(2 tries over 30 s, last: DeviceLost)"));
        assert!(reason.ends_with("Its driver did not say that it left the bus"));
    }

    #[test]
    fn another_gpu_that_serves_keeps_the_session() {
        let other = Gpu {
            name: "0000:03:00.0",
            ..gpu(false, 0, None)
        };
        assert_eq!(verdict(&[gpu(true, 3, Some(GONE)), other]), None);
        let other = Gpu {
            lost: true,
            failed_reopens: 2,
            failing_ms: 1000,
            error: Some("OutOfMemory"),
            ..other
        };
        // The other's reopens failed only for a moment yet.
        assert_eq!(verdict(&[gpu(true, 3, Some(GONE)), other]), None);
        let other = Gpu {
            failing_ms: 41_500,
            ..other
        };
        let reason = verdict(&[gpu(true, 3, Some(GONE)), other]).expect("stops");
        assert!(reason.contains("the GPU 0000:02:00.0 was lost"));
        assert!(reason.contains("; the GPU 0000:03:00.0 was lost"));
        assert!(reason.contains("(2 tries over 41 s, last: OutOfMemory)"));
    }

    #[test]
    fn the_first_silent_line_of_each_gpu_is_kept_and_found_by_its_address() {
        let lines = SilentLines::<2, 64>::new();
        assert!(
            !lines.observe(
                "nouveau 0000:02:00.0: answers again: BOOT_0 read ffffffff for 30 to 40 ms"
            )
        );
        // A status's description is not the driver's word on its device.
        assert!(!lines.observe("nouveau 0000:02:00.0: rpc: the GPU stopped answering on the bus"));
        assert_eq!(lines.of("0000:02:00.0"), None);
        assert!(lines.observe(GONE));
        // Cut to the slot, whole characters only.
        let kept = lines.of("0000:02:00.0").expect("kept");
        assert_eq!(kept, &GONE[..64]);
        assert_eq!(lines.of("0000:03:00.0"), None);
        assert!(lines.observe("amdgpu 0000:03:00.0: stopped answering: é"));
        assert!(lines.of("0000:03:00.0").expect("kept").ends_with('é'));
        // A third line says so, with no slot left for it.
        assert!(lines.observe("xe 0000:04:00.0: stopped answering"));
        assert_eq!(lines.of("0000:04:00.0"), None);
        let cut = SilentLines::<1, 41>::new();
        assert!(cut.observe("amdgpu 0000:03:00.0: stopped answering: é"));
        assert!(cut.of("0000:03:00.0").expect("kept").ends_with(": "));
    }

    #[test]
    fn a_silent_gpus_recovery_line_says_what_the_port_still_allows() {
        assert!(format!("{}", Recovery::PortGone).starts_with("none from here"));
        let reset = Recovery::BusReset {
            port: String::from("0000:00:01.1"),
            link_up: false,
        };
        let line = format!("{reset}");
        assert!(line.starts_with("possible by a secondary bus reset through port 0000:00:01.1"));
        assert!(line.contains("its link is down"));
        let refused = Recovery::NoBusReset {
            why: "another driver claimed a function on the PCI function's bus",
            link_up: true,
        };
        assert!(
            format!("{refused}")
                .ends_with("refused: another driver claimed a function on the PCI function's bus")
        );
        assert_eq!(
            link_event("nouveau 0000:01:00.0: stopped answering"),
            Some(STOPPED_ANSWERING)
        );
    }
}
