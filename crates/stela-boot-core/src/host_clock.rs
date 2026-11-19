//! The clock each PE's host core delivered and what held it back, for the
//! heartbeat's `PES CLOCK` line. Guest code runs at 180-440 million guest
//! instructions per second of guest code on reporters' laptops against 760
//! on a desktop whose boot-time clock is only 1.1-1.3 times theirs, and
//! some laptops measure 1.6-3.0 GHz at boot against a 4.1 GHz turbo: a
//! report should say the clock each core ran at under load, and whether a
//! thermal or power limit held it there.
//!
//! - IA32_MPERF (E7H) counts at the TSC's rate and IA32_APERF (E8H) at the
//!   core's actual clock, both only while the core runs (C0), where
//!   CPUID.06H:ECX[0] enumerates them (Intel SDM Vol. 3B 15.2; AMD APM Vol. 2
//!   17.3): their ratio over an interval scales the TSC rate to the clock
//!   delivered, as Linux's `aperfmperf_get_khz` does, and MPERF over the
//!   interval's TSC ticks is the share of it the core ran.
//! - IA32_THERM_STATUS (19CH), where CPUID.06H:EAX[0] enumerates the digital
//!   thermal sensor (Intel SDM Vol. 3B 15.8.5.2): bit 0 the core is over its
//!   thermal limit now, bit 2 PROCHOT# or FORCEPR# slows it now, bit 10 a
//!   power limit holds it below the requested performance now (where
//!   CPUID.06H:EAX[4] enumerates it), bits 22:16 its degrees below the
//!   highest allowed temperature, valid with bit 31. Only the current bits
//!   are read: the sticky log bits keep whatever firmware left.
//! - IA32_PACKAGE_THERM_STATUS (1B1H), where CPUID.06H:EAX[6] enumerates
//!   package thermal management: the same current bits for the whole
//!   package, bit 10 its package power limit (PL1/PL2) holding it now.
//! - The turbo switch firmware may flip while the PC runs (Linux's
//!   intel_pstate notes that platform firmware changes IA32_MISC_ENABLE bit
//!   38 at run time, on a change of power source): Intel IA32_MISC_ENABLE
//!   bit 38, AMD HWCR bit 25 where CPUID reports core performance boost
//!   (`host_performance` switches either on at start and leaves later
//!   changes to firmware).
//! - AMD PStateCurLim (C001_0061H) bits 2:0, where CPUID 8000_0007H:EDX[7]
//!   enumerates hardware P-states: the fastest P-state the hardware allows
//!   now, above P0 while a thermal (HTC), PROCHOT or platform limit holds.
//!
//! A sample reads up to six registers on the core itself, at most once
//! per [`SAMPLE_TICKS`] of its TSC (about 60 times a second at 4 GHz); the
//! check between costs one TSC read where the PE publishes its counts.
use crate::host_performance::{
    AMD_HWCR, AMD_PSTATE_LIMIT, BOOST_DISABLE, MISC_ENABLE, TURBO_DISABLE,
};
use crate::msr::{AMD_VENDOR, Bit, Gate, Msr, Register};
use alloc::{format, string::String};
use core::arch::x86_64::__cpuid;
use stela_engine::machine::engine::HostClock;

const APERF_MPERF: Bit = Bit::new(6, Register::Ecx, 0);
const THERMAL_SENSOR: Bit = Bit::new(6, Register::Eax, 0);
const POWER_LIMIT_NOTICE: Bit = Bit::new(6, Register::Eax, 4);
const PACKAGE_THERMAL: Bit = Bit::new(6, Register::Eax, 6);
const AMD_PSTATES: Bit = Bit::new(0x8000_0007, Register::Edx, 7);
const AMD_BOOST: Bit = Bit::new(0x8000_0007, Register::Edx, 9);
const AMD_RAPL: Bit = Bit::new(0x8000_0007, Register::Edx, 14);

pub const MPERF: Msr = Msr::new(0xe7, Gate::Cpuid(APERF_MPERF));
pub const APERF: Msr = Msr::new(0xe8, Gate::Cpuid(APERF_MPERF));
pub const THERM_STATUS: Msr = Msr::new(0x19c, Gate::Cpuid(THERMAL_SENSOR));
pub const PACKAGE_THERM_STATUS: Msr = Msr::new(0x1b1, Gate::Cpuid(PACKAGE_THERMAL));
/// AMD's energy unit (12:8) and package energy counter (31:0), where CPUID
/// 8000_0007H:EDX[14] enumerates them (AMD PPR, RAPL).
pub const AMD_RAPL_UNIT: Msr = Msr::new(0xc001_0299, Gate::Cpuid(AMD_RAPL));
pub const AMD_PACKAGE_ENERGY: Msr = Msr::new(0xc001_029b, Gate::Cpuid(AMD_RAPL));
/// Intel's model-specific energy unit and package energy counter, the same
/// layout as AMD's; read only through a probe that catches a refusal.
pub const INTEL_RAPL_UNIT: u32 = 0x606;
pub const INTEL_PACKAGE_ENERGY: u32 = 0x611;

const HOT: u64 = 1 << 0;
const PROCHOT: u64 = 1 << 2;
const POWER_LIMITED: u64 = 1 << 10;
const READING_VALID: u64 = 1 << 31;
const READING_SHIFT: u32 = 16;
const READING_MASK: u64 = 0x7f;

/// TSC ticks between two samples of a core: 2^26, 13-34 ms from 5 GHz to
/// 2 GHz, so a 10 s beat holds hundreds of samples and a busy core reads its
/// registers no more than about 60 times a second.
pub const SAMPLE_TICKS: u64 = 1 << 26;

/// Which of the registers this core has, found once: CPUID exits to a
/// hypervisor, so it is never asked per sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    pub counters: bool,
    pub thermal: bool,
    pub power: bool,
    pub package: bool,
    /// The turbo or boost switch: IA32_MISC_ENABLE on Intel, HWCR on AMD.
    pub switch: Option<Switch>,
    /// AMD's current P-state limit.
    pub limit: bool,
}

/// Where a processor's turbo or boost switch is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    Intel,
    Amd,
}

impl Switch {
    fn register(self) -> (Msr, u64) {
        match self {
            Self::Intel => (MISC_ENABLE, TURBO_DISABLE),
            Self::Amd => (AMD_HWCR, BOOST_DISABLE),
        }
    }
}

impl Features {
    /// This processor's enumeration.
    pub fn read() -> Self {
        let identity = __cpuid(0);
        let vendor = [identity.ebx, identity.edx, identity.ecx];
        let amd = vendor == AMD_VENDOR;
        let intel = vendor == [0x756e_6547, 0x4965_6e69, 0x6c65_746e];
        let switch = if intel && identity.eax >= 6 {
            Some(Switch::Intel)
        } else if amd && AMD_BOOST.present() {
            Some(Switch::Amd)
        } else {
            None
        };
        Self {
            counters: APERF.implemented(),
            thermal: THERM_STATUS.implemented(),
            power: POWER_LIMIT_NOTICE.present(),
            package: intel && PACKAGE_THERM_STATUS.implemented(),
            switch,
            limit: amd && AMD_PSTATES.present(),
        }
    }

    fn statuses(self) -> bool {
        self.thermal || self.package || self.switch.is_some() || self.limit
    }
}

/// One reading of the registers [`Features`] names; zero where it has none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub actual: u64,
    pub reference: u64,
    pub therm_status: u64,
    pub package_status: u64,
    /// The switch register's value.
    pub switch: u64,
    /// AMD PStateCurLim.
    pub limit: u64,
}

impl Reading {
    /// The registers `features` names, on this processor.
    ///
    /// # Safety
    /// CPL0 on the processor whose enumeration `features` is.
    pub unsafe fn take(features: Features) -> Self {
        let mut reading = Self::default();
        // SAFETY: the caller's contract; `features` holds only registers
        // CPUID enumerates on this processor, and reading them has no
        // effect.
        unsafe {
            if features.counters {
                reading.reference = MPERF.read();
                reading.actual = APERF.read();
            }
            if features.thermal {
                reading.therm_status = THERM_STATUS.read();
            }
            if features.package {
                reading.package_status = PACKAGE_THERM_STATUS.read();
            }
            if let Some(switch) = features.switch {
                reading.switch = switch.register().0.read();
            }
            if features.limit {
                reading.limit = AMD_PSTATE_LIMIT.read();
            }
        }
        reading
    }
}

/// One core's samples so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    features: Features,
    /// The TSC at the last sample, none before the first.
    last: Option<u64>,
    totals: HostClock,
}

impl Clock {
    pub fn new(features: Features) -> Self {
        Self {
            features,
            last: None,
            totals: HostClock {
                counters: features.counters,
                thermal: features.thermal,
                package: features.package,
                switch: features.switch.is_some(),
                limit: features.limit,
                ..HostClock::default()
            },
        }
    }

    /// The totals at TSC `now`, after a sample through `read` when the last
    /// is [`SAMPLE_TICKS`] old or there is none. A core without either
    /// register set is never read.
    pub fn sample(&mut self, now: u64, read: impl FnOnce(Features) -> Reading) -> HostClock {
        let due = self
            .last
            .is_none_or(|last| now.wrapping_sub(last) >= SAMPLE_TICKS);
        if !due || !(self.features.counters || self.features.statuses()) {
            return self.totals;
        }
        self.last = Some(now);
        let reading = read(self.features);
        let totals = &mut self.totals;
        if self.features.counters {
            totals.actual = reading.actual;
            totals.reference = reading.reference;
        }
        if self.features.statuses() {
            totals.samples += 1;
        }
        if self.features.thermal {
            let status = reading.therm_status;
            totals.hot += u64::from(status & HOT != 0);
            totals.prochot += u64::from(status & PROCHOT != 0);
            totals.power += u64::from(self.features.power && status & POWER_LIMITED != 0);
            totals.headroom = (status & READING_VALID != 0)
                .then_some(((status >> READING_SHIFT) & READING_MASK) as u8);
        }
        if self.features.package {
            let status = reading.package_status;
            totals.package_hot += u64::from(status & HOT != 0);
            totals.package_prochot += u64::from(status & PROCHOT != 0);
            totals.package_power += u64::from(self.features.power && status & POWER_LIMITED != 0);
        }
        if let Some(switch) = self.features.switch {
            totals.turbo_off += u64::from(reading.switch & switch.register().1 != 0);
        }
        if self.features.limit {
            totals.limited += u64::from(reading.limit & 7 != 0);
        }
        *totals
    }
}

/// One PE's interval: its core's counts at the beat before and now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeClock {
    pub before: HostClock,
    pub now: HostClock,
}

impl PeClock {
    /// The clock delivered while running, in MHz, and the running share of
    /// `interval` TSC ticks in percent; none without the counters, or a
    /// clock of none where the core never ran.
    fn delivered(&self, tsc_hz: u64, interval: u64) -> Option<(Option<u64>, u64)> {
        if !self.now.counters {
            return None;
        }
        let actual = self.now.actual.wrapping_sub(self.before.actual);
        let reference = self.now.reference.wrapping_sub(self.before.reference);
        let mhz = (reference != 0).then(|| {
            (u128::from(tsc_hz) * u128::from(actual) / u128::from(reference) / 1_000_000) as u64
        });
        let running = (u128::from(reference) * 100 / u128::from(interval.max(1))).min(100) as u64;
        Some((mhz, running))
    }

    /// Its thermal samples' shares in percent (over the limit, power
    /// limited, PROCHOT) and the last reading's headroom; none without the
    /// sensor or a sample.
    fn limits(&self) -> Option<([u64; 3], Option<u8>)> {
        let samples = self.now.samples.wrapping_sub(self.before.samples);
        if !self.now.thermal || samples == 0 {
            return None;
        }
        let share = |now: u64, before: u64| now.wrapping_sub(before) * 100 / samples;
        Some((
            [
                share(self.now.hot, self.before.hot),
                share(self.now.power, self.before.power),
                share(self.now.prochot, self.before.prochot),
            ],
            self.now.headroom,
        ))
    }
}

/// The share of `part` in `whole` samples in percent, or `-` without
/// samples.
fn share(part: u64, whole: u64) -> String {
    match (part * 100).checked_div(whole) {
        Some(percent) => format!("{percent}"),
        None => String::from("-"),
    }
}

/// `PES CLOCK tsc_mhz=3793 mhz/run%[/hot%/power%/prochot%/below_max_c]
/// 0:4310/100 1:3890/100/0/37/0/22 … all:4012/97
/// package(hot/power/prochot)%=0/37/0 turbo_off%=0 pstate_limited%=- package_w=41.2`:
/// per PE the clock its core delivered while it ran over the `interval` of
/// TSC ticks at `tsc_hz` and the share it ran, then where the core has a
/// thermal sensor the share of samples over its thermal limit, held by a
/// power limit and slowed by PROCHOT, and its last reading's degrees below
/// the highest allowed temperature; `-` for what the core cannot tell. `all`
/// is the clock weighted by each core's running time and the mean running
/// share. Then over all PEs' samples: the package over its thermal limit,
/// held by its package power limit and slowed by PROCHOT (Intel), turbo or
/// boost switched off by firmware, the fastest allowed P-state below P0
/// (AMD), and the package's mean power over the interval in watts
/// (`package_milliwatts`).
pub fn pes_clock(
    tsc_hz: u64,
    interval: u64,
    pes: &[PeClock],
    package_milliwatts: Option<u64>,
) -> String {
    let mut line = format!(
        "PES CLOCK tsc_mhz={} mhz/run%[/hot%/power%/prochot%/below_max_c]",
        tsc_hz / 1_000_000
    );
    let (mut actual, mut reference, mut counted) = (0u128, 0u128, 0u64);
    // Samples and counts over the PEs whose core has each register.
    let mut package = [0u64; 4];
    let mut switch = [0u64; 2];
    let mut limit = [0u64; 2];
    for (pe, clock) in pes.iter().enumerate() {
        line.push_str(&format!(" {pe}:"));
        match clock.delivered(tsc_hz, interval) {
            Some((mhz, running)) => {
                match mhz {
                    Some(mhz) => line.push_str(&format!("{mhz}/{running}")),
                    None => line.push_str(&format!("-/{running}")),
                }
                actual += u128::from(clock.now.actual.wrapping_sub(clock.before.actual));
                reference += u128::from(clock.now.reference.wrapping_sub(clock.before.reference));
                counted += 1;
            }
            None => line.push('-'),
        }
        if let Some(([hot, power, prochot], headroom)) = clock.limits() {
            line.push_str(&format!("/{hot}/{power}/{prochot}/"));
            match headroom {
                Some(degrees) => line.push_str(&format!("{degrees}")),
                None => line.push('-'),
            }
        }
        let (now, before) = (&clock.now, &clock.before);
        let samples = now.samples.wrapping_sub(before.samples);
        if now.package {
            package[0] += samples;
            package[1] += now.package_hot.wrapping_sub(before.package_hot);
            package[2] += now.package_power.wrapping_sub(before.package_power);
            package[3] += now.package_prochot.wrapping_sub(before.package_prochot);
        }
        if now.switch {
            switch[0] += samples;
            switch[1] += now.turbo_off.wrapping_sub(before.turbo_off);
        }
        if now.limit {
            limit[0] += samples;
            limit[1] += now.limited.wrapping_sub(before.limited);
        }
    }
    if counted == 0 || reference == 0 {
        line.push_str(" all:-");
    } else {
        let mhz = u128::from(tsc_hz) * actual / reference / 1_000_000;
        let running = reference * 100 / (u128::from(interval.max(1)) * u128::from(counted));
        line.push_str(&format!(" all:{mhz}/{}", running.min(100)));
    }
    if package[0] == 0 {
        line.push_str(" package(hot/power/prochot)%=-");
    } else {
        line.push_str(&format!(
            " package(hot/power/prochot)%={}/{}/{}",
            share(package[1], package[0]),
            share(package[2], package[0]),
            share(package[3], package[0]),
        ));
    }
    line.push_str(&format!(
        " turbo_off%={} pstate_limited%={}",
        share(switch[1], switch[0]),
        share(limit[1], limit[0]),
    ));
    match package_milliwatts {
        Some(milliwatts) => line.push_str(&format!(
            " package_w={}.{}",
            milliwatts / 1000,
            milliwatts % 1000 / 100
        )),
        None => line.push_str(" package_w=-"),
    }
    line
}

/// A package's energy counter between two beats: RAPL's 32-bit package
/// energy status in units of 1/2^ESU joules, ESU being bits 12:8 of the
/// unit register (Intel MSR_RAPL_POWER_UNIT 606H with MSR_PKG_ENERGY_STATUS
/// 611H; AMD C001_0299H with C001_029BH). The counter wraps after tens of
/// kilojoules, minutes at full power, so one beat sees at most one wrap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackageEnergy {
    /// The TSC and counter at the last reading.
    last: Option<(u64, u32)>,
}

impl PackageEnergy {
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// The mean power since the last reading in milliwatts, from the unit
    /// register `unit`, the counter `counter` at TSC `now` and the TSC's
    /// rate; none at the first reading, with a unit register of 0 (a
    /// hypervisor's answer) or no time between.
    pub fn sample(&mut self, unit: u64, counter: u32, now: u64, tsc_hz: u64) -> Option<u64> {
        let last = self.last.replace((now, counter));
        let (then, before) = last?;
        let ticks = now.wrapping_sub(then);
        if unit == 0 || ticks == 0 || tsc_hz == 0 {
            return None;
        }
        let shift = (unit >> 8) & 0x1f;
        let energy = u128::from(counter.wrapping_sub(before));
        let milliwatts = energy * 1000 * u128::from(tsc_hz) / (u128::from(ticks) << shift);
        Some(u64::try_from(milliwatts).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Features = Features {
        counters: true,
        thermal: true,
        power: true,
        package: false,
        switch: None,
        limit: false,
    };

    fn status(bits: u64, below: u64) -> u64 {
        bits | READING_VALID | below << READING_SHIFT
    }

    #[test]
    fn a_core_is_read_at_most_once_per_sample_interval() {
        let mut clock = Clock::new(ALL);
        let mut reads = 0;
        let read = |actual| {
            move |_: Features| Reading {
                actual,
                reference: actual / 2,
                therm_status: status(0, 40),
                ..Reading::default()
            }
        };
        for now in [1000, 1001, 1000 + SAMPLE_TICKS - 1] {
            let totals = clock.sample(now, |features| {
                reads += 1;
                read(10)(features)
            });
            assert_eq!(totals.actual, 10);
        }
        assert_eq!(reads, 1);
        let totals = clock.sample(1000 + SAMPLE_TICKS, read(30));
        assert_eq!(
            (totals.actual, totals.reference, totals.samples),
            (30, 15, 2)
        );
        // The TSC wraps between two samples.
        let mut clock = Clock::new(ALL);
        clock.sample(u64::MAX - 10, read(1));
        assert_eq!(clock.sample(SAMPLE_TICKS, read(2)).actual, 2);
    }

    #[test]
    fn a_core_without_the_registers_is_never_read() {
        let mut clock = Clock::new(Features::default());
        let totals = clock.sample(0, |_| unreachable!("no register to read"));
        assert_eq!(totals, HostClock::default());
        // Counters without a sensor: no thermal sample counts.
        let mut clock = Clock::new(Features {
            counters: true,
            ..Features::default()
        });
        let totals = clock.sample(0, |_| Reading {
            actual: 5,
            reference: 4,
            therm_status: status(HOT, 0),
            ..Reading::default()
        });
        assert_eq!((totals.actual, totals.samples, totals.hot), (5, 0, 0));
        assert!(totals.counters && !totals.thermal);
    }

    #[test]
    fn thermal_samples_count_each_current_limit() {
        let mut clock = Clock::new(ALL);
        let samples = [
            status(0, 30),
            status(HOT | PROCHOT, 0),
            status(POWER_LIMITED, 12),
            status(POWER_LIMITED, 9),
            // Log bits (1, 3, 11) are history, not the current state.
            status(0b1010 | 1 << 11, 20),
        ];
        let mut totals = HostClock::default();
        for (index, therm_status) in samples.into_iter().enumerate() {
            totals = clock.sample(index as u64 * SAMPLE_TICKS, |_| Reading {
                therm_status,
                ..Reading::default()
            });
        }
        assert_eq!(
            (totals.samples, totals.hot, totals.prochot, totals.power),
            (5, 1, 1, 2)
        );
        assert_eq!(totals.headroom, Some(20));
        // A reading not marked valid gives no headroom; without the power
        // notification the power bit is not read.
        let mut clock = Clock::new(Features {
            power: false,
            ..ALL
        });
        let totals = clock.sample(0, |_| Reading {
            therm_status: POWER_LIMITED | 25 << READING_SHIFT,
            ..Reading::default()
        });
        assert_eq!((totals.power, totals.headroom), (0, None));
    }

    #[test]
    fn the_line_states_each_cores_clock_running_share_and_limits() {
        let tsc_hz = 3_800_000_000;
        let interval = 38_000_000_000;
        let counted = |actual: u64, reference: u64| HostClock {
            actual,
            reference,
            counters: true,
            ..HostClock::default()
        };
        let pes = [
            // Ran the whole interval at 1.25 times the TSC's rate.
            PeClock {
                before: counted(100, 100),
                now: counted(100 + 47_500_000_000, 100 + interval),
            },
            // Ran half of it at half the TSC's rate, held by a power limit
            // in 3 of 4 samples, 15 degrees below its highest.
            PeClock {
                before: HostClock {
                    thermal: true,
                    samples: 10,
                    power: 2,
                    ..counted(0, 0)
                },
                now: HostClock {
                    thermal: true,
                    samples: 14,
                    power: 5,
                    hot: 1,
                    headroom: Some(15),
                    ..counted(interval / 4, interval / 2)
                },
            },
            // Never ran, and a core with no counters.
            PeClock {
                before: counted(7, 7),
                now: counted(7, 7),
            },
            PeClock::default(),
        ];
        assert_eq!(
            pes_clock(tsc_hz, interval, &pes, Some(41_250)),
            "PES CLOCK tsc_mhz=3800 mhz/run%[/hot%/power%/prochot%/below_max_c] 0:4750/100 \
             1:1900/50/25/75/0/15 2:-/0 3:- all:3800/50 package(hot/power/prochot)%=- \
             turbo_off%=- pstate_limited%=- package_w=41.2"
        );
        assert_eq!(
            pes_clock(tsc_hz, interval, &[PeClock::default()], None),
            "PES CLOCK tsc_mhz=3800 mhz/run%[/hot%/power%/prochot%/below_max_c] 0:- all:- \
             package(hot/power/prochot)%=- turbo_off%=- pstate_limited%=- package_w=-"
        );
    }

    #[test]
    fn package_turbo_and_limit_samples_are_shared_over_the_pes_that_have_them() {
        let features = Features {
            package: true,
            power: true,
            switch: Some(Switch::Intel),
            ..Features::default()
        };
        let mut clock = Clock::new(features);
        let readings = [
            (status(POWER_LIMITED, 0), 0),
            (status(POWER_LIMITED | PROCHOT, 0), TURBO_DISABLE),
            (status(0, 0), 0),
            (status(HOT, 0), 0),
        ];
        let before = clock.sample(0, |_| Reading::default());
        let mut now = before;
        for (index, (package_status, switch)) in readings.into_iter().enumerate() {
            now = clock.sample((index as u64 + 1) * SAMPLE_TICKS, |_| Reading {
                package_status,
                switch,
                ..Reading::default()
            });
        }
        assert_eq!(
            (
                now.samples,
                now.package_power,
                now.package_prochot,
                now.package_hot,
                now.turbo_off
            ),
            (5, 2, 1, 1, 1)
        );
        // AMD: boost switch in HWCR and the P-state limit.
        let mut amd = Clock::new(Features {
            switch: Some(Switch::Amd),
            limit: true,
            ..Features::default()
        });
        amd.sample(0, |_| Reading {
            switch: BOOST_DISABLE,
            limit: 0x21,
            ..Reading::default()
        });
        let amd_now = amd.sample(SAMPLE_TICKS, |_| Reading {
            limit: 0x20,
            ..Reading::default()
        });
        assert_eq!(
            (amd_now.samples, amd_now.turbo_off, amd_now.limited),
            (2, 1, 1)
        );
        let line = pes_clock(
            1_000_000_000,
            1,
            &[
                PeClock { before, now },
                PeClock {
                    before: HostClock {
                        switch: true,
                        limit: true,
                        ..HostClock::default()
                    },
                    now: amd_now,
                },
            ],
            None,
        );
        assert!(
            line.ends_with(
                " 0:- 1:- all:- package(hot/power/prochot)%=25/50/25 turbo_off%=33 \
                 pstate_limited%=50 package_w=-"
            ),
            "{line}"
        );
    }

    #[test]
    fn package_power_follows_the_energy_counter() {
        let mut energy = PackageEnergy::new();
        // ESU 14: 1/16384 J per count; 10 s at 1 GHz.
        let unit = 0xa_0e03;
        assert_eq!(
            energy.sample(unit, 1000, 5, 1_000_000_000),
            None,
            "first reading"
        );
        // 4,915,200 counts = 300 J in 10 s = 30 W.
        assert_eq!(
            energy.sample(unit, 1000 + 4_915_200, 5 + 10_000_000_000, 1_000_000_000),
            Some(30_000)
        );
        // The counter wraps.
        let mut energy = PackageEnergy::new();
        energy.sample(unit, u32::MAX - 99, 0, 1_000_000_000);
        assert_eq!(
            energy.sample(unit, 16_284, 1_000_000_000, 1_000_000_000),
            Some(1000)
        );
        // A unit register of 0 is a hypervisor's answer, not a unit.
        let mut energy = PackageEnergy::new();
        energy.sample(0, 0, 0, 1_000_000_000);
        assert_eq!(energy.sample(0, 100, 1_000_000_000, 1_000_000_000), None);
    }
}
