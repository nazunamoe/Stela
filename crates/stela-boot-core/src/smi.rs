//! System management interrupts counted over host service turns, so that a
//! slow turn's line (`HOST SERVICE SLOW ... smis=`) confirms or refutes
//! that firmware SMIs, stretched by processors that never answer them,
//! took its time. Intel counts them in `MSR_SMI_COUNT` (SDM Vol. 4, every
//! family 06H processor since Nehalem); the read is probed, and a refused
//! one means no counter. AMD documents no counter register: its SMIs are a
//! core performance event, which the profile's counters own, so AMD
//! processors report none.

use alloc::{format, string::String};
use core::fmt;

/// Intel's `MSR_SMI_COUNT`; bits 31:0 count.
pub const INTEL_SMI_COUNT: u32 = 0x34;

/// The counter register of a processor whose vendor is Intel or not.
pub fn counter(intel: bool) -> Option<u32> {
    intel.then_some(INTEL_SMI_COUNT)
}

/// The SMIs between two reads of the counter, across its wrap.
pub fn taken(before: u64, after: u64) -> u32 {
    (after as u32).wrapping_sub(before as u32)
}

/// The FADT's `SMI_CMD` (ACPI 6.5 table 5.9, offset 48, four bytes): the
/// I/O port whose writes call the firmware's system management code, as
/// its AML and ACPI's enable do. `None` where the FADT has none.
pub fn command_port(fadt: &[u8]) -> Option<u16> {
    let bytes: [u8; 4] = fadt.get(48..52)?.try_into().ok()?;
    u16::try_from(u32::from_le_bytes(bytes))
        .ok()
        .filter(|&port| port != 0)
}

/// Where the processors other than the boot processor are while the
/// firmware's system management code runs: one in wait-for-SIPI takes no
/// SMI, and a handler that waits for every processor waits for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Others {
    /// Parked in an idle loop that answers SMIs.
    pub parked: u32,
    /// In INIT's wait-for-SIPI state.
    pub waiting: u32,
    /// Started as PEs' processors, or parked after them.
    pub started: bool,
}

impl fmt::Display for Others {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.started {
            true => f.write_str("other processors started"),
            false => write!(
                f,
                "other processors parked={} wait_for_sipi={}",
                self.parked, self.waiting
            ),
        }
    }
}

/// The writes to `SMI_CMD` a boot made, for their log lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommandWrites {
    pub count: u32,
    pub slowest_micros: u64,
    pub total_micros: u64,
}

/// The first writes each get a line; later ones only when slow.
pub const LOGGED_WRITES: u32 = 16;
/// A write this slow gets a line whatever its number: a handler that waits
/// for absent processors takes its timeouts, a second or more each.
pub const SLOW_MICROS: u64 = 100_000;

impl CommandWrites {
    /// Count a write of `value` to `port` by `by` that took `micros`, with
    /// `others` as they were; its line, for the first writes and slow ones.
    pub fn record(
        &mut self,
        port: u16,
        value: u32,
        by: &str,
        micros: u64,
        others: Others,
    ) -> Option<String> {
        self.count = self.count.saturating_add(1);
        self.total_micros = self.total_micros.saturating_add(micros);
        self.slowest_micros = self.slowest_micros.max(micros);
        (self.count <= LOGGED_WRITES || micros >= SLOW_MICROS).then(|| {
            format!(
                "ACPI SMI write {} port={port:#x} value={value:#x} by {by} returned in {}.{:03} \
                 ms; {others}; all writes {} ms, slowest {} ms\n",
                self.count,
                micros / 1000,
                micros % 1000,
                self.total_micros / 1000,
                self.slowest_micros / 1000,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smi_command_port_is_read_from_the_fadt() {
        let mut fadt = alloc::vec![0u8; 116];
        assert_eq!(command_port(&fadt), None);
        fadt[48] = 0xb2;
        assert_eq!(command_port(&fadt), Some(0xb2));
        fadt[50] = 1;
        assert_eq!(command_port(&fadt), None, "wider than a port");
        assert_eq!(command_port(&fadt[..50]), None);
    }

    #[test]
    fn the_first_writes_and_every_slow_one_get_a_line() {
        let mut writes = CommandWrites::default();
        let others = Others {
            parked: 7,
            waiting: 0,
            started: false,
        };
        let first = writes
            .record(0xb2, 0xa0, "\\_SB.PCI0.LPCB.EC0._REG", 2_003_100, others)
            .unwrap();
        assert_eq!(
            first,
            "ACPI SMI write 1 port=0xb2 value=0xa0 by \\_SB.PCI0.LPCB.EC0._REG returned in \
             2003.100 ms; other processors parked=7 wait_for_sipi=0; all writes 2003 ms, \
             slowest 2003 ms\n"
        );
        for _ in 1..LOGGED_WRITES {
            assert!(writes.record(0xb2, 1, "x", 20, others).is_some());
        }
        assert_eq!(writes.record(0xb2, 1, "x", 20, others), None);
        assert!(
            writes
                .record(0xb2, 1, "x", SLOW_MICROS, others)
                .unwrap()
                .starts_with("ACPI SMI write 18 ")
        );
        assert_eq!(
            Others {
                started: true,
                ..others
            }
            .to_string(),
            "other processors started"
        );
    }

    #[test]
    fn only_intel_has_the_counter() {
        assert_eq!(counter(true), Some(0x34));
        assert_eq!(counter(false), None);
    }

    #[test]
    fn taken_counts_across_the_wrap_of_the_low_word() {
        assert_eq!(taken(5, 9), 4);
        assert_eq!(taken(0xffff_fffe, 0x1_0000_0001), 3);
        // Bits above 31 are reserved and ignored.
        assert_eq!(taken(0xdead_0000_0000_0007, 7), 0);
    }
}
