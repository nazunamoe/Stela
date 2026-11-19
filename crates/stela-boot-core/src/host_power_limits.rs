//! The package power and thermal limits firmware left, for one boot line
//! (`HOST POWER LIMITS`). Nothing here writes them: an operating system on
//! mains power runs within the firmware's limits (Linux's intel_rapl only
//! publishes them), and a laptop's firmware may expect a platform driver to
//! move them, which Stela does not model. The line says what a slow PC's
//! cores run under, beside `PES CLOCK`'s clock, power-limit flags and
//! package watts.
//!
//! Intel (SDM Vol. 3B 15.10; all registers model-specific, read only through
//! a probe that catches a refusal):
//!
//! - MSR_RAPL_POWER_UNIT (606H): power unit 1/2^(3:0) W, energy unit
//!   1/2^(12:8) J, time unit 1/2^(19:16) s.
//! - MSR_PKG_POWER_LIMIT (610H): PL1 power 14:0, enable 15, clamp 16, time
//!   window 23:17 (2^Y * (1 + Z/4) time units, Y 21:17, Z 23:22); PL2 the same
//!   at 46:32; lock 63.
//! - The memory-mapped copy of the same register at MCHBAR + 59A0H, which the
//!   host bridge (00:00.0, MCHBAR at config 48H) decodes on client parts;
//!   the package honours the lower of the two copies. Platform drivers on
//!   laptops move this copy.
//! - MSR_PKG_POWER_INFO (614H): thermal design power 14:0.
//! - MSR_TEMPERATURE_TARGET (1A2H): the thermal control circuit's activation
//!   temperature 23:16 and the offset below it 29:24 that firmware may set.
//! - IA32_PACKAGE_THERM_STATUS (1B1H) log bits: the package went over its
//!   thermal limit (1), was slowed by PROCHOT (3), was held by a power limit
//!   (11) since the logs were last cleared, by firmware or at reset.
//!
//! AMD keeps its package limits (STAPM, PPT, thermal) in its SMU, which
//! publishes them only through its mailbox; the line says so, and `PES
//! CLOCK` shows their effect.
use alloc::{format, string::String};

/// One limit of a power-limit register, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub milliwatts: u64,
    pub enabled: bool,
    pub clamped: bool,
    pub window_us: u64,
}

/// MSR_RAPL_POWER_UNIT's power and time units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Units {
    power_shift: u32,
    time_shift: u32,
}

impl Units {
    pub fn decode(value: u64) -> Self {
        Self {
            power_shift: (value & 0xf) as u32,
            time_shift: ((value >> 16) & 0xf) as u32,
        }
    }

    fn milliwatts(self, field: u64) -> u64 {
        ((field & 0x7fff) * 1000) >> self.power_shift
    }

    /// The limit in the low 24 bits of `field`.
    pub fn limit(self, field: u64) -> Limit {
        let y = (field >> 17) & 0x1f;
        let z = (field >> 22) & 3;
        // 2^Y * (1 + Z/4) time units, in microseconds.
        let window = ((u128::from(1_000_000u32) << y) * u128::from(4 + z) / 4) >> self.time_shift;
        Limit {
            milliwatts: self.milliwatts(field),
            enabled: field & (1 << 15) != 0,
            clamped: field & (1 << 16) != 0,
            window_us: u64::try_from(window).unwrap_or(u64::MAX),
        }
    }

    /// PL1 and PL2 of a power-limit register.
    pub fn limits(self, value: u64) -> [Limit; 2] {
        [self.limit(value), self.limit(value >> 32)]
    }
}

fn watts(milliwatts: u64) -> String {
    format!("{}.{}W", milliwatts / 1000, milliwatts % 1000 / 100)
}

fn window(microseconds: u64) -> String {
    if microseconds >= 1_000_000 {
        format!(
            "{}.{}s",
            microseconds / 1_000_000,
            microseconds % 1_000_000 / 100_000
        )
    } else {
        format!("{}.{}ms", microseconds / 1000, microseconds % 1000 / 100)
    }
}

/// `pl1=45.0W/28.0s pl2=115.0W/2.4ms` with `(off)` after a disabled limit
/// and `(clamp)` after one that may go below the requested clock.
fn pair(name: &str, units: Units, value: u64) -> String {
    let text = |limit: Limit| {
        format!(
            "{}/{}{}{}",
            watts(limit.milliwatts),
            window(limit.window_us),
            if limit.enabled { "" } else { "(off)" },
            if limit.clamped { "(clamp)" } else { "" },
        )
    };
    let [pl1, pl2] = units.limits(value);
    format!("{name}_pl1={} {name}_pl2={}", text(pl1), text(pl2))
}

/// What the boot read of an Intel package; `None` where a register was
/// refused or is not decoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntelPackage {
    pub units: Option<u64>,
    pub limit: Option<u64>,
    /// The memory-mapped copy at MCHBAR + 59A0H.
    pub mmio_limit: Option<u64>,
    pub info: Option<u64>,
    pub temperature_target: Option<u64>,
    pub package_status: Option<u64>,
}

/// A memory-mapped limit that reads as all ones or zero is no register.
fn plausible(value: Option<u64>) -> Option<u64> {
    value.filter(|&value| value != u64::MAX && value != 0)
}

/// The `HOST POWER LIMITS` line for an Intel package.
pub fn intel_line(package: IntelPackage) -> String {
    let mut line = String::from("HOST POWER LIMITS vendor=intel");
    match (package.units.map(Units::decode), package.limit) {
        (Some(units), Some(limit)) => {
            line.push(' ');
            line.push_str(&pair("msr", units, limit));
            line.push_str(&format!(" msr_lock={}", limit >> 63));
            match plausible(package.mmio_limit) {
                Some(mmio) => {
                    line.push(' ');
                    line.push_str(&pair("mmio", units, mmio));
                    // The package honours the lower enabled PL1 of the two.
                    let effective = [limit, mmio]
                        .iter()
                        .map(|&value| units.limit(value))
                        .filter(|limit| limit.enabled)
                        .map(|limit| limit.milliwatts)
                        .min();
                    match effective {
                        Some(milliwatts) => {
                            line.push_str(&format!(" effective_pl1={}", watts(milliwatts)))
                        }
                        None => line.push_str(" effective_pl1=-"),
                    }
                }
                None => line.push_str(" mmio=-"),
            }
            match package.info {
                Some(info) => line.push_str(&format!(" tdp={}", watts(units.milliwatts(info)))),
                None => line.push_str(" tdp=-"),
            }
        }
        _ => line.push_str(" msr=- mmio=-"),
    }
    match package.temperature_target {
        Some(target) => line.push_str(&format!(
            " tcc_activation={}C tcc_offset={}C",
            (target >> 16) & 0xff,
            (target >> 24) & 0x3f
        )),
        None => line.push_str(" tcc_activation=- tcc_offset=-"),
    }
    match package.package_status {
        Some(status) => line.push_str(&format!(
            " package_log(hot/prochot/power)={}/{}/{}",
            (status >> 1) & 1,
            (status >> 3) & 1,
            (status >> 11) & 1
        )),
        None => line.push_str(" package_log=-"),
    }
    line.push('\n');
    line
}

/// The `HOST POWER LIMITS` line for an AMD package.
pub fn amd_line() -> String {
    String::from(
        "HOST POWER LIMITS vendor=amd msr=- mmio=- (the SMU keeps the package's power and \
         thermal limits and publishes them only through its mailbox, which is not read; \
         PES CLOCK's package_w and clocks show their effect)\n",
    )
}

/// MCHBAR from the host bridge's configuration dwords at 48H and 4CH: bits
/// 38:15, where bit 0 enables it.
pub fn mchbar(low: u32, high: u32) -> Option<u64> {
    let value = u64::from(high) << 32 | u64::from(low);
    let base = value & 0x7f_ffff_8000;
    (value & 1 != 0 && base != 0).then_some(base)
}

/// The memory-mapped package power limit's offset in MCHBAR.
pub const MMIO_POWER_LIMIT: u64 = 0x59a0;

#[cfg(test)]
mod tests {
    use super::*;

    // Power unit 1/8 W, energy unit 1/16384 J, time unit 1/1024 s.
    const UNITS: u64 = 0x000a_0e03;

    #[test]
    fn limits_decode_power_window_and_flags() {
        let units = Units::decode(UNITS);
        // PL1: 45 W (360), enabled, clamp, Y=10 Z=3: 1024 * 1.75 / 1024 s.
        let pl1 = 360 | 1 << 15 | 1 << 16 | 10 << 17 | 3 << 22;
        assert_eq!(
            units.limit(pl1),
            Limit {
                milliwatts: 45_000,
                enabled: true,
                clamped: true,
                window_us: 1_750_000
            }
        );
        // PL2: 115 W (920), enabled, Y=1 Z=1: 2 * 1.25 / 1024 s.
        let pl2: u64 = 920 | 1 << 15 | 1 << 17 | 1 << 22;
        let [first, second] = units.limits(pl2 << 32 | pl1);
        assert_eq!(first.milliwatts, 45_000);
        assert_eq!((second.milliwatts, second.window_us), (115_000, 2441));
    }

    #[test]
    fn the_intel_line_names_both_copies_and_the_lower_rules() {
        let pl = |watts: u64, enabled: u64| (watts * 8) | enabled << 15 | 0xa << 17 | 3 << 22;
        let msr = 1 << 63 | pl(115, 1) << 32 | pl(45, 1);
        let mmio = pl(115, 1) << 32 | pl(35, 1);
        let line = intel_line(IntelPackage {
            units: Some(UNITS),
            limit: Some(msr),
            mmio_limit: Some(mmio),
            info: Some(45 * 8),
            temperature_target: Some(5 << 24 | 100 << 16),
            package_status: Some(1 << 11 | 1 << 3),
        });
        assert_eq!(
            line,
            "HOST POWER LIMITS vendor=intel msr_pl1=45.0W/1.7s msr_pl2=115.0W/1.7s msr_lock=1 \
             mmio_pl1=35.0W/1.7s mmio_pl2=115.0W/1.7s effective_pl1=35.0W tdp=45.0W \
             tcc_activation=100C tcc_offset=5C package_log(hot/prochot/power)=0/1/1\n"
        );
        // A disabled limit is marked and does not rule; a copy that reads as
        // no register is left out.
        let line = intel_line(IntelPackage {
            units: Some(UNITS),
            limit: Some(pl(45, 1)),
            mmio_limit: Some(pl(15, 0)),
            ..IntelPackage::default()
        });
        assert!(line.contains(" mmio_pl1=15.0W/1.7s(off) "), "{line}");
        assert!(line.contains(" effective_pl1=45.0W "), "{line}");
        let line = intel_line(IntelPackage {
            units: Some(UNITS),
            limit: Some(pl(45, 1)),
            mmio_limit: Some(u64::MAX),
            ..IntelPackage::default()
        });
        assert!(line.contains(" msr_lock=0 mmio=- tdp=- "), "{line}");
        // Nothing readable (a hypervisor): every field says so.
        assert_eq!(
            intel_line(IntelPackage::default()),
            "HOST POWER LIMITS vendor=intel msr=- mmio=- tcc_activation=- tcc_offset=- \
             package_log=-\n"
        );
    }

    #[test]
    fn mchbar_needs_its_enable_bit() {
        assert_eq!(mchbar(0xfedc_0001, 0), Some(0xfedc_0000));
        assert_eq!(mchbar(0xfedc_0000, 0), None);
        assert_eq!(mchbar(0x1, 0), None);
        // Bits above 38 and below 15 are not the base.
        assert_eq!(mchbar(0xfedc_7fff, 0xffff_ff80), Some(0x00_fedc_0000));
    }
}
