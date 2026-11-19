use super::*;
use alloc::{vec, vec::Vec};

/// Registers with fixed values that log each access in order.
#[derive(Default)]
struct Logged {
    values: Vec<(u32, u64)>,
    accesses: Vec<(char, u32, u64)>,
    /// Model-specific registers a probe finds; others are refused.
    probed: Vec<(u32, u64)>,
}

impl Logged {
    fn with(values: &[(Msr, u64)]) -> Self {
        Self {
            values: values
                .iter()
                .map(|(msr, value)| (msr.index, *value))
                .collect(),
            ..Self::default()
        }
    }

    fn writes(&self) -> Vec<(u32, u64)> {
        self.accesses
            .iter()
            .filter(|(kind, ..)| *kind == 'w')
            .map(|&(_, index, value)| (index, value))
            .collect()
    }
}

impl Registers for Logged {
    unsafe fn read(&mut self, msr: Msr) -> u64 {
        let value = self
            .values
            .iter()
            .find(|(index, _)| *index == msr.index)
            .map_or(0, |(_, value)| *value);
        self.accesses.push(('r', msr.index, value));
        value
    }

    unsafe fn write(&mut self, msr: Msr, value: u64) {
        self.accesses.push(('w', msr.index, value));
        if let Some(entry) = self
            .values
            .iter_mut()
            .find(|(index, _)| *index == msr.index)
        {
            entry.1 = value;
        } else {
            self.values.push((msr.index, value));
        }
    }

    unsafe fn probe(&mut self, index: u32) -> Option<u64> {
        let found = self
            .probed
            .iter()
            .find(|(probed, _)| *probed == index)
            .map(|(_, value)| *value);
        self.accesses.push(('p', index, found.unwrap_or(u64::MAX)));
        found
    }
}

fn intel(hwp: bool, notify: bool, epp: bool) -> Features {
    Features {
        intel: true,
        hwp,
        hwp_notify: notify,
        hwp_epp: epp,
        ..Features::default()
    }
}

fn amd(cppc: bool) -> Features {
    Features {
        amd: true,
        cppc,
        ..Features::default()
    }
}

fn request(features: Features, registers: &mut Logged) -> Outcome {
    run(features, registers).outcome
}

fn run(features: Features, registers: &mut Logged) -> Report {
    // SAFETY: the registers are a log, not this processor's.
    unsafe { request_with(features, registers) }
}

#[test]
fn a_processor_without_either_interface_is_not_touched() {
    for features in [
        Features::default(),
        intel(false, false, false),
        amd(false),
        // A hypervisor that reports neither bit, on either vendor.
        Features {
            intel: true,
            amd: false,
            ..Features::default()
        },
    ] {
        let mut registers = Logged::default();
        assert_eq!(request(features, &mut registers), Outcome::Legacy);
        assert!(registers.accesses.is_empty(), "{features:?}");
    }
}

#[test]
fn intel_turns_notifications_off_then_hwp_on_before_the_request() {
    // highest 0x30, guaranteed 0x24, most efficient 0x10, lowest 0x08.
    let mut registers = Logged::with(&[
        (HWP_CAPABILITIES, 0x0810_2430),
        // Firmware: balanced EPP 0x80, desired 0x11, activity window.
        (HWP_REQUEST, 0x0000_0201_8011_3008),
    ]);
    let outcome = request(intel(true, true, true), &mut registers);
    assert_eq!(
        registers.writes(),
        vec![(0x773, 0), (0x770, 1), (0x774, 0x0000_0201_0000_3030),],
        "IA32_HWP_INTERRUPT, then IA32_PM_ENABLE, then IA32_HWP_REQUEST"
    );
    assert_eq!(
        outcome,
        Outcome::Hwp {
            levels: Levels {
                highest: 0x30,
                sustained: 0x24,
                lowest: 0x08
            },
            bounds: Bounds {
                minimum: 0x30,
                maximum: 0x30
            },
            epp: true,
            package: false,
        }
    );
}

#[test]
fn intel_with_hwp_already_on_writes_only_the_request() {
    let mut registers = Logged::with(&[
        (PM_ENABLE, 1),
        (HWP_CAPABILITIES, 0x0810_2430),
        // Package control (bit 42) set by firmware: taken back, so this
        // processor's own fields govern it.
        (HWP_REQUEST, 0x0000_0400_8000_3008),
    ]);
    let outcome = request(intel(true, false, true), &mut registers);
    assert_eq!(registers.writes(), vec![(0x774, 0x0000_0000_0000_3030)]);
    assert!(matches!(outcome, Outcome::Hwp { package: true, .. }));
}

#[test]
fn intel_hwp_without_epp_stays_off_and_keeps_the_preference_field() {
    // Off: left off, as intel_pstate does without EPP (or DEC).
    let mut registers = Logged::with(&[(HWP_CAPABILITIES, 0x0810_2430)]);
    assert_eq!(
        request(intel(true, true, false), &mut registers),
        Outcome::HwpWithoutEpp
    );
    assert!(registers.writes().is_empty());
    assert_eq!(registers.accesses, vec![('r', 0x770, 0)]);
    // On already (firmware's choice): bits 31:24 are not an EPP field.
    let mut registers = Logged::with(&[
        (PM_ENABLE, 1),
        (HWP_CAPABILITIES, 0x0810_2430),
        (HWP_REQUEST, 0x5a00_3008),
    ]);
    let outcome = request(intel(true, false, false), &mut registers);
    assert_eq!(registers.writes(), vec![(0x774, 0x5a00_3030)]);
    assert!(matches!(outcome, Outcome::Hwp { epp: false, .. }));
}

#[test]
fn unpublished_levels_keep_the_firmware_request() {
    let mut registers = Logged::with(&[(PM_ENABLE, 1), (HWP_REQUEST, 0x8000_ff01)]);
    assert_eq!(
        request(intel(true, false, true), &mut registers),
        Outcome::Unpublished
    );
    assert!(registers.writes().is_empty());
    // Lowest above highest.
    assert_eq!(intel_request(0x4000_0020, 0, true), None);
    let mut registers = Logged::with(&[(AMD_CPPC_ENABLE, 1), (AMD_CPPC_REQ, 0x8000_ff01)]);
    assert_eq!(request(amd(true), &mut registers), Outcome::Unpublished);
    assert!(registers.writes().is_empty());
    // As amd-pstate checks the levels before it enables CPPC, an empty
    // capability register leaves CPPC off where the firmware left it off.
    let mut registers = Logged::with(&[(AMD_CPPC_REQ, 0x8000_ff01)]);
    assert_eq!(request(amd(true), &mut registers), Outcome::Unpublished);
    assert!(registers.writes().is_empty(), "{:x?}", registers.writes());
}

#[test]
fn amd_enables_cppc_then_asks_nominal_to_highest() {
    // CAP1: highest 0xa6, nominal 0x7c, lowest nonlinear 0x38, lowest 0x12.
    let mut registers = Logged::with(&[
        (AMD_CPPC_CAP1, 0xa67c_3812),
        // Firmware: EPP 0x80, desired 0, minimum 0x12, maximum 0xa6, and
        // upper bits that stay.
        (AMD_CPPC_REQ, 0xdead_0000_8000_12a6),
    ]);
    let outcome = request(amd(true), &mut registers);
    assert_eq!(
        registers.writes(),
        vec![(0xc001_02b1, 1), (0xc001_02b3, 0xdead_0000_0000_7ca6)],
        "CPPC_ENABLE, then CPPC_REQ: maximum 7:0, minimum 15:8"
    );
    assert_eq!(
        outcome,
        Outcome::Cppc {
            levels: Levels {
                highest: 0xa6,
                sustained: 0x7c,
                lowest: 0x12
            },
            bounds: Bounds {
                minimum: 0x7c,
                maximum: 0xa6
            },
        }
    );
    // Already on: only the request.
    let mut registers = Logged::with(&[
        (AMD_CPPC_ENABLE, 1),
        (AMD_CPPC_CAP1, 0xa67c_3812),
        (AMD_CPPC_REQ, 0x8000_12a6),
    ]);
    request(amd(true), &mut registers);
    assert_eq!(registers.writes(), vec![(0xc001_02b3, 0x7ca6)]);
}

#[test]
fn amd_keeps_a_firmware_minimum_from_an_otherwise_empty_request() {
    // amd-pstate's bios_min_perf: only the minimum field is set.
    let (value, bounds) = amd_request(0xa67c_3812, 0x5000).unwrap();
    assert_eq!(bounds.minimum, 0x50);
    assert_eq!(value, 0x50a6);
    // A request with other fields set holds no firmware minimum.
    let (_, bounds) = amd_request(0xa67c_3812, 0x8000_50a6).unwrap();
    assert_eq!(bounds.minimum, 0x7c);
    // An empty request: the nominal level.
    let (_, bounds) = amd_request(0xa67c_3812, 0).unwrap();
    assert_eq!(bounds.minimum, 0x7c);
}

#[test]
fn a_minimum_outside_the_published_range_is_clamped() {
    // Nominal 0xc0 above highest 0xa6.
    let (_, bounds) = amd_request(0xa6c0_3812, 0).unwrap();
    assert_eq!(bounds.minimum, 0xa6);
    // Nominal 0 below lowest 0x12.
    let (_, bounds) = amd_request(0xa600_3812, 0).unwrap();
    assert_eq!(bounds.minimum, 0x12);
}

fn plain(outcome: Outcome) -> Report {
    Report {
        class: Class::Uniform,
        outcome,
        turbo: Turbo::On,
        modulation: None,
        energy_bias: None,
    }
}

#[test]
fn the_summary_counts_outcomes_and_the_range_of_levels() {
    let tally = Tally::new();
    assert!(
        tally
            .summary()
            .contains("highest=none requested_minimum=none")
    );
    for (highest, nominal) in [(0xa6u64, 0x7cu64), (0xb0, 0x7c)] {
        let capabilities = highest << 24 | nominal << 16 | 0x3812;
        let (_, bounds) = amd_request(capabilities, 0).unwrap();
        tally.record(plain(Outcome::Cppc {
            levels: Levels::amd(capabilities),
            bounds,
        }));
    }
    tally.record(plain(Outcome::Legacy));
    let line = tally.summary();
    assert!(line.starts_with("HOST PERFORMANCE hwp=0 cppc=2 "), "{line}");
    assert!(line.contains(" legacy=1 "), "{line}");
    assert!(
        line.contains("highest=166-176 requested_minimum=124 "),
        "{line}"
    );
    // The performance line and one power line for the one class, and no
    // UNSUPPORTED line while every processor published levels.
    let lines: Vec<_> = line.lines().collect();
    assert_eq!(lines.len(), 2, "{line}");
    assert_eq!(
        lines[1],
        "HOST POWER class=all processors=3 mechanism=cppc:2,none:1 highest=166-176 \
         nominal=124 lowest=18 requested_min=124 requested_max=166-176 legacy_level=- \
         package_request_released=0 turbo(on/firmware_off/absent)=3/0/0 \
         clock_modulation_cleared=0 energy_bias_firmware=- energy_bias=0"
    );
    // Processors with an empty capability register are said once, together.
    tally.record(plain(Outcome::Unpublished));
    tally.record(plain(Outcome::Unpublished));
    let line = tally.summary();
    assert!(line.contains(" unpublished=2 "), "{line}");
    let lines: Vec<_> = line.lines().collect();
    assert_eq!(lines.len(), 3, "{line}");
    assert!(
        lines[2].starts_with("UNSUPPORTED host performance request on 2 processors: "),
        "{line}"
    );
}

#[test]
fn each_core_class_has_its_own_power_line() {
    let tally = Tally::new();
    let hwp = |highest: u64, class| Report {
        class,
        outcome: request(
            intel(true, false, true),
            &mut Logged::with(&[(PM_ENABLE, 1), (HWP_CAPABILITIES, 0x0100_1c00 | highest)]),
        ),
        turbo: Turbo::On,
        modulation: None,
        energy_bias: Some(6),
    };
    for _ in 0..2 {
        tally.record(hwp(63, Class::Performance));
    }
    tally.record(Report {
        modulation: Some(75),
        ..hwp(36, Class::Efficiency)
    });
    tally.record(Report {
        turbo: Turbo::FirmwareOff,
        ..hwp(36, Class::Efficiency)
    });
    let line = tally.summary();
    let lines: Vec<_> = line.lines().collect();
    assert_eq!(lines.len(), 4, "{line}");
    assert!(lines[0].contains(" hwp=4 "), "{line}");
    assert!(lines[0].contains(" highest=36-63 "), "{line}");
    assert_eq!(
        lines[1],
        "HOST POWER class=performance processors=2 mechanism=hwp:2 highest=63 nominal=28 \
         lowest=1 requested_min=63 requested_max=63 legacy_level=- package_request_released=0 \
         turbo(on/firmware_off/absent)=2/0/0 clock_modulation_cleared=0 \
         energy_bias_firmware=6 energy_bias=0"
    );
    assert!(
        lines[2]
            .starts_with("HOST POWER class=efficiency processors=2 mechanism=hwp:2 highest=36 "),
        "{line}"
    );
    assert!(
        lines[2]
            .contains(" turbo(on/firmware_off/absent)=1/1/0 clock_modulation_cleared=1(duty%=75) "),
        "{line}"
    );
    assert!(
        lines[3].starts_with("HOST POWER turbo is off on 1 processors: "),
        "{line}"
    );
}

#[test]
fn turbo_that_firmware_switched_off_stays_off() {
    // Intel: IA32_MISC_ENABLE bit 38 set: reported, and the register is not
    // written.
    let features = Features {
        turbo_switch: true,
        ..intel(true, false, true)
    };
    let mut registers = Logged::with(&[
        (MISC_ENABLE, TURBO_DISABLE | 0x850089),
        (PM_ENABLE, 1),
        (HWP_CAPABILITIES, 0x0810_2430),
    ]);
    let report = run(features, &mut registers);
    assert_eq!(report.turbo, Turbo::FirmwareOff);
    assert!(registers.writes().iter().all(|(index, _)| *index != 0x1a0));
    assert!(matches!(report.outcome, Outcome::Hwp { .. }));
    // Left on: no write; on or absent as CPUID.06H:EAX[1] says.
    for (turbo, expected) in [(true, Turbo::On), (false, Turbo::Absent)] {
        let mut registers = Logged::with(&[(MISC_ENABLE, 0x850089)]);
        let features = Features {
            turbo_switch: true,
            turbo,
            ..Features::default()
        };
        assert_eq!(run(features, &mut registers).turbo, expected);
        assert!(registers.writes().is_empty());
    }
    // AMD: HWCR bit 25 (CpbDis) where CPUID reports boost.
    let features = Features {
        boost_switch: true,
        ..amd(false)
    };
    let mut registers = Logged::with(&[(AMD_HWCR, BOOST_DISABLE | 0x0100_0010)]);
    assert_eq!(run(features, &mut registers).turbo, Turbo::FirmwareOff);
    assert!(registers.writes().is_empty());
    let mut registers = Logged::with(&[(AMD_HWCR, 0x0100_0010)]);
    assert_eq!(run(features, &mut registers).turbo, Turbo::On);
    assert!(registers.writes().is_empty());
    // Without the switch nothing is read.
    let mut registers = Logged::default();
    assert_eq!(run(amd(false), &mut registers).turbo, Turbo::Absent);
    assert!(registers.accesses.is_empty());
}

#[test]
fn clock_modulation_left_on_is_cleared_and_its_duty_reported() {
    // Extended modulation: duty 3:0 in 6.25 % steps (12 = 75 %); bits above
    // 4 stay.
    let features = Features {
        clock_modulation: true,
        extended_modulation: true,
        ..Features::default()
    };
    let mut registers = Logged::with(&[(CLOCK_MODULATION, 0x100 | 0x10 | 12)]);
    let report = run(features, &mut registers);
    assert_eq!(report.modulation, Some(75));
    assert_eq!(registers.writes(), vec![(0x19a, 0x100)]);
    // Without it: duty 3:1 in 12.5 % steps (0b100 = 50 %).
    assert_eq!(modulation_duty(0x10 | 0b1000, false), 50);
    // Off: read, not written.
    let mut registers = Logged::with(&[(CLOCK_MODULATION, 12)]);
    assert_eq!(super::tests::run(features, &mut registers).modulation, None);
    assert!(registers.writes().is_empty());
}

#[test]
fn the_energy_bias_becomes_performance() {
    let features = Features {
        energy_bias: true,
        ..Features::default()
    };
    let mut registers = Logged::with(&[(ENERGY_PERF_BIAS, 0x30 | 6)]);
    assert_eq!(run(features, &mut registers).energy_bias, Some(6));
    assert_eq!(registers.writes(), vec![(0x1b0, 0x30)]);
    let mut registers = Logged::with(&[(ENERGY_PERF_BIAS, 0)]);
    assert_eq!(run(features, &mut registers).energy_bias, Some(0));
    assert!(registers.writes().is_empty());
}

#[test]
fn intel_without_hwp_asks_perf_ctl_for_the_turbo_ratio() {
    let features = Features {
        eist: true,
        ..intel(false, false, false)
    };
    // Maximum non-turbo 0x24 (PLATFORM_INFO 15:8), one-core turbo 0x2a;
    // firmware asked for 0x10 with turbo disengaged (bit 32).
    let mut registers = Logged::with(&[(PERF_CTL, 1 << 32 | 1 << 40 | 0x1000)]);
    registers.probed = vec![
        (PLATFORM_INFO, 0x8000_2400),
        (TURBO_RATIO_LIMIT, 0x2628_2a2a),
    ];
    assert_eq!(
        request(features, &mut registers),
        Outcome::PerfCtl {
            ratio: 0x2a,
            hwp_off: false
        }
    );
    assert_eq!(registers.writes(), vec![(0x199, 1 << 40 | 0x2a00)]);
    // Turbo ratios below the nominal one (or none) ask for the nominal.
    assert_eq!(intel_legacy_ratio(Some(0x2400), Some(0x20)), Some(0x24));
    assert_eq!(intel_legacy_ratio(Some(0x2400), None), Some(0x24));
    // No ratio to ask for: nothing written.
    let mut registers = Logged::default();
    assert_eq!(request(features, &mut registers), Outcome::Legacy);
    assert!(registers.writes().is_empty());
    assert_eq!(intel_legacy_ratio(Some(0xff), Some(0x30)), None);
}

#[test]
fn intel_hwp_left_off_without_epp_takes_perf_ctl() {
    let features = Features {
        eist: true,
        ..intel(true, true, false)
    };
    let mut registers = Logged {
        probed: vec![(PLATFORM_INFO, 0x2400)],
        ..Logged::default()
    };
    assert_eq!(
        request(features, &mut registers),
        Outcome::PerfCtl {
            ratio: 0x24,
            hwp_off: true
        }
    );
    // HWP stays off: neither its enable nor its interrupt mask is written.
    assert_eq!(registers.writes(), vec![(0x199, 0x2400)]);
}

#[test]
fn amd_without_usable_cppc_asks_for_p0() {
    let features = Features {
        amd_pstates: true,
        ..amd(true)
    };
    // CPPC enumerated with an empty capability register, as on some
    // Zen 2 laptops: CPPC stays off, PStateCtl asks for P0 under a limit
    // of P1 that the hardware keeps.
    let mut registers = Logged::with(&[(AMD_PSTATE_LIMIT, 0x21), (AMD_PSTATE_CTL, 0xf000_0002)]);
    assert_eq!(
        request(features, &mut registers),
        Outcome::PState { limit: 1 }
    );
    assert_eq!(registers.writes(), vec![(0xc001_0062, 0xf000_0000)]);
    // Already P0: read, not written.
    let mut registers = Logged::default();
    assert_eq!(
        request(features, &mut registers),
        Outcome::PState { limit: 0 }
    );
    assert!(registers.writes().is_empty());
}

#[test]
fn the_order_is_modulation_bias_then_the_level() {
    let features = Features {
        boost_switch: true,
        energy_bias: true,
        ..amd(true)
    };
    let mut registers = Logged::with(&[
        (AMD_HWCR, BOOST_DISABLE),
        (ENERGY_PERF_BIAS, 6),
        (AMD_CPPC_CAP1, 0xa67c_3812),
    ]);
    run(features, &mut registers);
    let order: Vec<u32> = registers.writes().iter().map(|(index, _)| *index).collect();
    assert_eq!(order, vec![0x1b0, 0xc001_02b1, 0xc001_02b3]);
}

#[test]
fn core_classes_follow_cpuid() {
    assert_eq!(Class::intel(0x40), Class::Performance);
    assert_eq!(Class::intel(0x20), Class::Efficiency);
    assert_eq!(Class::intel(0), Class::Uniform);
    assert_eq!(Class::amd(1 << 30, 1 << 28), Class::Efficiency);
    assert_eq!(Class::amd(1 << 30, 0), Class::Performance);
    assert_eq!(Class::amd(0, 1 << 28), Class::Uniform, "not heterogeneous");
}
