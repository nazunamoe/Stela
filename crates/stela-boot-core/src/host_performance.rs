//! The host processor's own performance request. After ExitBootServices no
//! operating system asks the CPU for speed, so a core runs at whatever its
//! firmware left: with hardware-controlled P-states (Intel HWP, AMD CPPC)
//! not enabled, or enabled with a balanced energy preference, with turbo or
//! boost switched off, or with on-demand clock modulation on, a busy core
//! may stay at or below its base clock. Each processor that runs a PE asks
//! for full performance as Linux's `performance` policy does in active
//! mode, on that processor itself, through the first tier its CPUID
//! reports:
//!
//! 1. Intel HWP (intel_pstate `intel_pstate_hwp_enable`,
//!    `intel_pstate_hwp_set`): HWP notifications off, HWP on, minimum and
//!    maximum both the highest level, desired level 0 (the hardware picks)
//!    and energy preference 0 where the EPP field exists. Like
//!    `intel_pstate_init`, HWP that firmware left off is not turned on
//!    without EPP (Linux also accepts the DEC bit of the model-specific
//!    POWER_CTL, which this does not read); such a processor takes tier 3.
//!    A request that firmware put under the package request (bit 42) is
//!    taken back, so this processor's own fields govern it.
//! 2. AMD CPPC through MSRs (amd-pstate `msr_cppc_enable`,
//!    `amd_pstate_update_min_max_limit`, `amd_pstate_epp_update_limit`):
//!    the published levels checked first, then CPPC on, maximum the core's
//!    highest level, minimum the firmware's minimum where the request holds
//!    nothing else and else the nominal level, desired 0 and energy
//!    preference 0. A processor that publishes no levels takes tier 4.
//! 3. Intel enhanced SpeedStep (intel_pstate without HWP, `performance`):
//!    IA32_PERF_CTL asks for the single-core turbo ratio (the ratio limit
//!    MSR's lowest field, at least the maximum non-turbo ratio of
//!    PLATFORM_INFO), with turbo not disengaged (bit 32 clear). Both ratio
//!    registers are model-specific: they are read only where the caller can
//!    catch a refused read ([`Registers::probe`]).
//! 4. AMD hardware P-states (acpi-cpufreq at its highest state): PStateCtl
//!    asks for P0; the hardware holds it at the current limit
//!    (PStateCurLim), which firmware and the SMU own.
//!
//! Independent of the tier, on each processor:
//!
//! - Turbo or boost that firmware switched off stays off and is reported:
//!   Intel IA32_MISC_ENABLE bit 38 (set only on parts with turbo, SDM Vol. 4
//!   table 2-2), AMD HWCR bit 25 (CpbDis) where CPUID 8000_0007H:EDX[9]
//!   reports core performance boost. The bit is the PC's setup choice or
//!   its maker's thermal design, and Linux's intel_pstate and amd-pstate
//!   leave it as found too. Neither register is written.
//! - On-demand clock modulation (IA32_CLOCK_MODULATION bit 4) left on is
//!   cleared, as Linux's ACPI processor driver clears a throttling state at
//!   start ("we'll crank up the speed", `acpi_processor_get_throttling_info`).
//! - The energy/performance bias (IA32_ENERGY_PERF_BIAS 3:0) is 0
//!   (performance), which on processors without HWP's EPP field steers
//!   turbo and uncore clocks.
//!
//! Nothing here lowers a limit that protects the hardware: power limits
//! (RAPL, the SMU's), the thermal control circuit, its offset and PROCHOT
//! stay as firmware set them, and the processor's own thermal throttling
//! stays on. A core over its thermal limit slows itself; `PES CLOCK` reports
//! the share of samples it did.
//!
//! Every access is gated by the vendor and the CPUID bit that enumerates
//! the register, so a processor (or a hypervisor) without the interface is
//! never touched and only counts as `legacy`. Bits the request does not
//! own (Intel's activity window and valid bits; reserved bits) keep their
//! values. HWP and CPPC enables are one-way until reset. Idle waits are
//! unchanged, and processors parked in their idle loop keep the firmware's
//! request, as Linux leaves an offline AMD CPU's.
//!
//! Registers: Intel SDM Vol. 3B 15.3-15.4 and Vol. 4 (CPUID.01H:ECX[7]
//! EIST, EDX[22] clock modulation; CPUID.06H:EAX[1] turbo, [5] extended
//! modulation, [7] HWP, [8] HWP notification, [10] EPP, ECX[3] EPB;
//! IA32_PERF_CTL 199H, IA32_CLOCK_MODULATION 19AH, IA32_MISC_ENABLE 1A0H,
//! IA32_ENERGY_PERF_BIAS 1B0H, IA32_PM_ENABLE 770H, IA32_HWP_CAPABILITIES
//! 771H, IA32_HWP_INTERRUPT 773H, IA32_HWP_REQUEST 774H with package
//! control in bit 42; PLATFORM_INFO CEH, TURBO_RATIO_LIMIT 1ADH); AMD PPR (CPUID
//! 8000_0007H:EDX[7] hardware P-states, [9] CPB; 8000_0008H:EBX[27] CPPC;
//! HWCR C001_0015H, PStateCurLim C001_0061H, PStateCtl C001_0062H,
//! CPPC_CAP1 C001_02B0H, CPPC_ENABLE C001_02B1H, CPPC_REQ C001_02B3H).
use crate::msr::{AMD_VENDOR, Bit, Gate, Msr, Register};
use alloc::{format, string::String};
use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::sync::atomic::{AtomicU32, Ordering};

const EIST_BIT: Bit = Bit::new(1, Register::Ecx, 7);
const CLOCK_MODULATION_BIT: Bit = Bit::new(1, Register::Edx, 22);
const TURBO_BIT: Bit = Bit::new(6, Register::Eax, 1);
const EXTENDED_MODULATION_BIT: Bit = Bit::new(6, Register::Eax, 5);
const HWP_BIT: Bit = Bit::new(6, Register::Eax, 7);
const HWP_NOTIFY_BIT: Bit = Bit::new(6, Register::Eax, 8);
const HWP_EPP_BIT: Bit = Bit::new(6, Register::Eax, 10);
const ENERGY_BIAS_BIT: Bit = Bit::new(6, Register::Ecx, 3);
const HYBRID_BIT: Bit = Bit::new(7, Register::Edx, 15);
const AMD_PSTATES_BIT: Bit = Bit::new(0x8000_0007, Register::Edx, 7);
const AMD_BOOST_BIT: Bit = Bit::new(0x8000_0007, Register::Edx, 9);
const AMD_CPPC_BIT: Bit = Bit::new(0x8000_0008, Register::Ebx, 27);

pub const PERF_CTL: Msr = Msr::new(0x199, Gate::Cpuid(EIST_BIT));
pub const CLOCK_MODULATION: Msr = Msr::new(0x19a, Gate::Cpuid(CLOCK_MODULATION_BIT));
/// IA32_MISC_ENABLE, architectural on every Intel 64 processor; only bit 38
/// is written, and only where it is set.
pub const MISC_ENABLE: Msr = Msr::new(0x1a0, Gate::LongMode);
pub const ENERGY_PERF_BIAS: Msr = Msr::new(0x1b0, Gate::Cpuid(ENERGY_BIAS_BIT));
pub const PM_ENABLE: Msr = Msr::new(0x770, Gate::Cpuid(HWP_BIT));
pub const HWP_CAPABILITIES: Msr = Msr::new(0x771, Gate::Cpuid(HWP_BIT));
pub const HWP_INTERRUPT: Msr = Msr::new(0x773, Gate::Cpuid(HWP_NOTIFY_BIT));
pub const HWP_REQUEST: Msr = Msr::new(0x774, Gate::Cpuid(HWP_BIT));
pub const AMD_HWCR: Msr = Msr::new(0xc001_0015, Gate::Cpuid(AMD_BOOST_BIT));
pub const AMD_PSTATE_LIMIT: Msr = Msr::new(0xc001_0061, Gate::Cpuid(AMD_PSTATES_BIT));
pub const AMD_PSTATE_CTL: Msr = Msr::new(0xc001_0062, Gate::Cpuid(AMD_PSTATES_BIT));
pub const AMD_CPPC_CAP1: Msr = Msr::new(0xc001_02b0, Gate::Cpuid(AMD_CPPC_BIT));
pub const AMD_CPPC_ENABLE: Msr = Msr::new(0xc001_02b1, Gate::Cpuid(AMD_CPPC_BIT));
pub const AMD_CPPC_REQ: Msr = Msr::new(0xc001_02b3, Gate::Cpuid(AMD_CPPC_BIT));

/// Intel's model-specific ratio registers, read only through
/// [`Registers::probe`].
pub const PLATFORM_INFO: u32 = 0xce;
pub const TURBO_RATIO_LIMIT: u32 = 0x1ad;

const INTEL_VENDOR: [u32; 3] = [
    u32::from_le_bytes(*b"Genu"),
    u32::from_le_bytes(*b"ineI"),
    u32::from_le_bytes(*b"ntel"),
];

/// IA32_HWP_REQUEST bit 42: the package request (IA32_HWP_REQUEST_PKG)
/// governs this processor instead of its own fields.
const HWP_PACKAGE_CONTROL: u64 = 1 << 42;
/// IA32_MISC_ENABLE bit 38: turbo mode disabled.
pub const TURBO_DISABLE: u64 = 1 << 38;
/// HWCR bit 25: core performance boost disabled.
pub const BOOST_DISABLE: u64 = 1 << 25;
/// IA32_CLOCK_MODULATION bit 4: on-demand clock modulation enabled.
const MODULATION_ENABLE: u64 = 1 << 4;
/// IA32_PERF_CTL bit 32: IDA (turbo) disengaged for this processor.
const TURBO_DISENGAGE: u64 = 1 << 32;

/// A processor's core type, as its own CPUID reports it (Intel CPUID.1AH on
/// hybrid parts, AMD CPUID 8000_0026H with the heterogeneous-topology bit).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Class {
    /// One core type, or none reported.
    #[default]
    Uniform,
    Performance,
    Efficiency,
    LowPower,
}

impl Class {
    pub const ALL: [Self; 4] = [
        Self::Uniform,
        Self::Performance,
        Self::Efficiency,
        Self::LowPower,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Uniform => "all",
            Self::Performance => "performance",
            Self::Efficiency => "efficiency",
            Self::LowPower => "low_power",
        }
    }

    /// Intel CPUID.1AH:EAX[31:24]: 20H an Atom (efficiency) core, 40H a Core
    /// (performance) core.
    pub fn intel(core_type: u32) -> Self {
        match core_type {
            0x20 => Self::Efficiency,
            0x40 => Self::Performance,
            _ => Self::Uniform,
        }
    }

    /// AMD CPUID 8000_0026H, decoded as Linux's `parse_topology_amd`: a
    /// heterogeneous level (EAX[30]) with a core type in EBX[31:28].
    pub fn amd(eax: u32, ebx: u32) -> Self {
        if eax & (1 << 30) == 0 {
            return Self::Uniform;
        }
        match ebx >> 28 {
            0 => Self::Performance,
            1 => Self::Efficiency,
            2 => Self::LowPower,
            _ => Self::Uniform,
        }
    }
}

/// What CPUID says about this processor's performance interfaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    pub intel: bool,
    pub amd: bool,
    pub hwp: bool,
    pub hwp_notify: bool,
    pub hwp_epp: bool,
    pub cppc: bool,
    /// Intel: IA32_MISC_ENABLE's turbo switch can be read (CPUID leaf 6
    /// exists).
    pub turbo_switch: bool,
    /// Intel CPUID.06H:EAX[1]: turbo exists and is not switched off.
    pub turbo: bool,
    /// AMD: core performance boost and its HWCR switch.
    pub boost_switch: bool,
    pub clock_modulation: bool,
    pub extended_modulation: bool,
    pub energy_bias: bool,
    /// Intel enhanced SpeedStep (IA32_PERF_CTL).
    pub eist: bool,
    /// AMD hardware P-states (PStateCtl).
    pub amd_pstates: bool,
    pub class: Class,
}

impl Features {
    /// This processor's enumeration. The vendor gates each interface as
    /// Linux's drivers do (intel_pstate loads on Intel only, amd-pstate on
    /// AMD only).
    pub fn read() -> Self {
        let identity = __cpuid(0);
        let vendor = [identity.ebx, identity.edx, identity.ecx];
        let intel = vendor == INTEL_VENDOR;
        let amd = vendor == AMD_VENDOR;
        let class = if intel && identity.eax >= 0x1a && HYBRID_BIT.present() {
            Class::intel(__cpuid_count(0x1a, 0).eax >> 24)
        } else if amd && __cpuid(0x8000_0000).eax >= 0x8000_0026 {
            let level = __cpuid_count(0x8000_0026, 0);
            Class::amd(level.eax, level.ebx)
        } else {
            Class::Uniform
        };
        Self {
            intel,
            amd,
            hwp: intel && HWP_BIT.present(),
            hwp_notify: intel && HWP_NOTIFY_BIT.present(),
            hwp_epp: intel && HWP_EPP_BIT.present(),
            cppc: amd && AMD_CPPC_BIT.present(),
            turbo_switch: intel && identity.eax >= 6,
            turbo: intel && TURBO_BIT.present(),
            boost_switch: amd && AMD_BOOST_BIT.present(),
            clock_modulation: intel && CLOCK_MODULATION_BIT.present(),
            extended_modulation: intel && EXTENDED_MODULATION_BIT.present(),
            energy_bias: ENERGY_BIAS_BIT.present(),
            eist: intel && EIST_BIT.present(),
            amd_pstates: amd && AMD_PSTATES_BIT.present(),
            class,
        }
    }
}

/// The performance levels a processor publishes, on its own 8-bit scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Levels {
    pub highest: u8,
    /// Intel's guaranteed level; AMD's nominal level.
    pub sustained: u8,
    pub lowest: u8,
}

impl Levels {
    /// IA32_HWP_CAPABILITIES: highest 7:0, guaranteed 15:8, most efficient
    /// 23:16, lowest 31:24.
    pub fn intel(capabilities: u64) -> Self {
        Self {
            highest: capabilities as u8,
            sustained: (capabilities >> 8) as u8,
            lowest: (capabilities >> 24) as u8,
        }
    }

    /// CPPC_CAP1: lowest 7:0, lowest nonlinear 15:8, nominal 23:16,
    /// highest 31:24.
    pub fn amd(capabilities: u64) -> Self {
        Self {
            highest: (capabilities >> 24) as u8,
            sustained: (capabilities >> 16) as u8,
            lowest: capabilities as u8,
        }
    }

    /// Published levels that order, or none (an unprogrammed capability
    /// register), so the firmware's request stays.
    fn ordered(self) -> Option<Self> {
        (self.highest != 0 && self.lowest <= self.highest).then_some(self)
    }
}

/// A request's minimum and maximum levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub minimum: u8,
    pub maximum: u8,
}

/// The IA32_HWP_REQUEST value for `capabilities`, or none to keep the
/// firmware's request: minimum 7:0 and maximum 15:8 both the highest level
/// (intel_pstate `intel_pstate_hwp_set`: `min = max` under the performance
/// policy), desired 23:16 = 0, with `epp` the energy preference 31:24 = 0,
/// and package control (bit 42) clear, so these fields govern this
/// processor. Bits 63:43 and 41:32 stay.
pub fn intel_request(capabilities: u64, current: u64, epp: bool) -> Option<(u64, Bounds)> {
    let levels = Levels::intel(capabilities).ordered()?;
    let kept = if epp { !0xffff_ffff } else { !0x00ff_ffff };
    let highest = u64::from(levels.highest);
    let value = (current & kept & !HWP_PACKAGE_CONTROL) | highest | highest << 8;
    Some((
        value,
        Bounds {
            minimum: levels.highest,
            maximum: levels.highest,
        },
    ))
}

/// The CPPC_REQ value for `capabilities`, or none to keep the firmware's
/// request: maximum 7:0 the highest level, minimum 15:8 the firmware's
/// minimum when the request holds nothing else (amd-pstate's
/// `bios_min_perf`, `msr_init_perf`) and else the nominal level, never
/// above the maximum nor below the lowest level; desired 23:16 = 0 and
/// energy preference 31:24 = 0. Bits 63:32 stay.
pub fn amd_request(capabilities: u64, current: u64) -> Option<(u64, Bounds)> {
    const MINIMUM: u64 = 0xff << 8;
    let levels = Levels::amd(capabilities).ordered()?;
    let firmware_minimum = (current & !MINIMUM == 0).then_some((current >> 8) as u8);
    let minimum = firmware_minimum
        .filter(|&minimum| minimum != 0)
        .unwrap_or(levels.sustained)
        .clamp(levels.lowest, levels.highest);
    let value = (current & !0xffff_ffff) | u64::from(levels.highest) | u64::from(minimum) << 8;
    Some((
        value,
        Bounds {
            minimum,
            maximum: levels.highest,
        },
    ))
}

/// The ratio IA32_PERF_CTL asks for, as intel_pstate's
/// `core_get_turbo_pstate`: the single-core turbo ratio (TURBO_RATIO_LIMIT
/// 7:0), at least the maximum non-turbo ratio (PLATFORM_INFO 15:8); none
/// without a non-turbo ratio.
pub fn intel_legacy_ratio(
    platform_info: Option<u64>,
    turbo_ratio_limit: Option<u64>,
) -> Option<u8> {
    let nominal = platform_info
        .map(|value| (value >> 8) as u8)
        .filter(|&ratio| ratio != 0)?;
    Some(
        turbo_ratio_limit
            .map_or(0, |value| value as u8)
            .max(nominal),
    )
}

/// The IA32_PERF_CTL value asking for `ratio` (15:8) with turbo engaged
/// (bit 32 clear); bits 7:0 (voltage on old parts) clear, the rest kept.
pub fn perf_ctl(current: u64, ratio: u8) -> u64 {
    (current & !(0xffff | TURBO_DISENGAGE)) | u64::from(ratio) << 8
}

/// The share of the clock on-demand modulation lets run, in percent, from
/// an IA32_CLOCK_MODULATION value with modulation on: duty 3:1 in 12.5 %
/// steps, or 3:0 in 6.25 % steps with extended modulation.
pub fn modulation_duty(value: u64, extended: bool) -> u8 {
    if extended {
        ((value & 0xf) * 625 / 100) as u8
    } else {
        ((value >> 1 & 7) * 125 / 10) as u8
    }
}

/// What this processor's performance request became.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Intel HWP asks for `bounds`; `package` when firmware had put this
    /// processor under the package request, which the request took back.
    Hwp {
        levels: Levels,
        bounds: Bounds,
        epp: bool,
        package: bool,
    },
    /// AMD CPPC asks for `bounds`.
    Cppc { levels: Levels, bounds: Bounds },
    /// Intel IA32_PERF_CTL asks for `ratio`; `hwp_off` when HWP without EPP
    /// stays off.
    PerfCtl { ratio: u8, hwp_off: bool },
    /// AMD PStateCtl asks for P0; `limit` is the highest-performance
    /// P-state the hardware currently allows (0 is P0).
    PState { limit: u8 },
    /// HWP without EPP that firmware left off, with no IA32_PERF_CTL ratio
    /// to ask for: left as it was.
    HwpWithoutEpp,
    /// The capability register published no usable levels and no legacy
    /// P-state register is reported; the firmware's request stays, and AMD
    /// CPPC is not enabled.
    Unpublished,
    /// No interface is enumerated (a hypervisor that hides them, or
    /// legacy P-states whose ratios cannot be read); nothing was touched.
    Legacy,
}

impl Outcome {
    fn index(self) -> usize {
        match self {
            Self::Hwp { .. } => 0,
            Self::Cppc { .. } => 1,
            Self::PerfCtl { .. } => 2,
            Self::PState { .. } => 3,
            Self::HwpWithoutEpp => 4,
            Self::Unpublished => 5,
            Self::Legacy => 6,
        }
    }

    const NAMES: [&'static str; 7] = [
        "hwp",
        "cppc",
        "perf_ctl",
        "pstate",
        "hwp_without_epp",
        "unpublished",
        "none",
    ];
}

/// Turbo (Intel) or core performance boost (AMD) on this processor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turbo {
    /// Offered and on as firmware left it.
    On,
    /// Firmware switched it off, and it stays off.
    FirmwareOff,
    /// Not offered, or the processor does not say (a hypervisor).
    Absent,
}

impl Turbo {
    fn index(self) -> usize {
        self as usize
    }
}

/// Everything one processor's request did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub class: Class,
    pub outcome: Outcome,
    pub turbo: Turbo,
    /// The running share of the clock firmware's on-demand modulation left,
    /// in percent, where it was on (now cleared).
    pub modulation: Option<u8>,
    /// The energy/performance bias firmware left, where the register exists
    /// (now 0).
    pub energy_bias: Option<u8>,
}

/// The model-specific registers [`request_with`] uses, so that tests can
/// see each access.
pub trait Registers {
    /// # Safety
    /// As [`Msr::read`].
    unsafe fn read(&mut self, msr: Msr) -> u64;
    /// # Safety
    /// As [`Msr::write`].
    unsafe fn write(&mut self, msr: Msr, value: u64);
    /// A model-specific register that no CPUID bit enumerates, or `None`
    /// where the processor refuses the read or the caller cannot catch a
    /// refusal (the default).
    ///
    /// # Safety
    /// CPL0 on the processor the registers belong to, with a #GP of the
    /// read caught by the implementation.
    unsafe fn probe(&mut self, _index: u32) -> Option<u64> {
        None
    }
}

/// This processor's registers, without catching a refused read: tier 3
/// reads no ratio through it.
pub struct Hardware;

impl Registers for Hardware {
    unsafe fn read(&mut self, msr: Msr) -> u64 {
        // SAFETY: the caller's contract.
        unsafe { msr.read() }
    }

    unsafe fn write(&mut self, msr: Msr, value: u64) {
        // SAFETY: the caller's contract.
        unsafe { msr.write(value) }
    }
}

/// Whether turbo or boost is on as firmware left it; nothing is written.
///
/// # Safety
/// As [`request_with`].
unsafe fn turbo_state(features: Features, registers: &mut impl Registers) -> Turbo {
    let (msr, off) = if features.turbo_switch {
        (MISC_ENABLE, TURBO_DISABLE)
    } else if features.boost_switch {
        (AMD_HWCR, BOOST_DISABLE)
    } else {
        return Turbo::Absent;
    };
    // SAFETY: IA32_MISC_ENABLE is architectural on Intel 64; HWCR is gated by
    // CPUID's CPB bit. Bit 38 is set only on parts with turbo.
    let value = unsafe { registers.read(msr) };
    if value & off != 0 {
        Turbo::FirmwareOff
    } else if features.boost_switch || features.turbo {
        // Intel: CPUID.06H:EAX[1] says whether turbo exists at all.
        Turbo::On
    } else {
        Turbo::Absent
    }
}

/// Ask for full performance (module documentation) through `registers`,
/// touching only the registers `features` enumerates.
///
/// # Safety
/// CPL0 on the processor `features` describes, whose registers `registers`
/// reaches, with interrupts disabled and before guest code runs on it;
/// nothing else on this processor owns its P-state registers.
pub unsafe fn request_with(features: Features, registers: &mut impl Registers) -> Report {
    // SAFETY: the caller's contract; each helper touches only registers
    // `features` enumerates.
    let turbo = unsafe { turbo_state(features, registers) };
    let mut modulation = None;
    if features.clock_modulation {
        // SAFETY: CPUID.01H:EDX[22] enumerates IA32_CLOCK_MODULATION; bits
        // 4:0 are its on-demand control, the rest stays.
        unsafe {
            let value = registers.read(CLOCK_MODULATION);
            if value & MODULATION_ENABLE != 0 {
                modulation = Some(modulation_duty(value, features.extended_modulation));
                registers.write(CLOCK_MODULATION, value & !0x1f);
            }
        }
    }
    let mut energy_bias = None;
    if features.energy_bias {
        // SAFETY: CPUID.06H:ECX[3] enumerates IA32_ENERGY_PERF_BIAS; bits
        // 3:0 are the bias, the rest stays.
        unsafe {
            let value = registers.read(ENERGY_PERF_BIAS);
            energy_bias = Some((value & 0xf) as u8);
            if value & 0xf != 0 {
                registers.write(ENERGY_PERF_BIAS, value & !0xf);
            }
        }
    }
    // SAFETY: the caller's contract.
    let outcome = unsafe { request_level(features, registers) };
    Report {
        class: features.class,
        outcome,
        turbo,
        modulation,
        energy_bias,
    }
}

/// The P-state request of the first tier `features` reports.
///
/// # Safety
/// As [`request_with`].
unsafe fn request_level(features: Features, registers: &mut impl Registers) -> Outcome {
    let mut hwp_off = false;
    if features.hwp {
        // SAFETY: CPUID.06H:EAX[7] on an Intel CPU enumerates IA32_PM_ENABLE,
        // IA32_HWP_CAPABILITIES and IA32_HWP_REQUEST, and [8]
        // IA32_HWP_INTERRUPT. The request is written only with HWP on: it
        // faults while HWP is off. Each value keeps the bits it does not own.
        unsafe {
            let enabled = registers.read(PM_ENABLE) & 1 != 0;
            if enabled || features.hwp_epp {
                if features.hwp_notify {
                    registers.write(HWP_INTERRUPT, 0);
                }
                if !enabled {
                    registers.write(PM_ENABLE, 1);
                }
                let capabilities = registers.read(HWP_CAPABILITIES);
                let current = registers.read(HWP_REQUEST);
                let Some((value, bounds)) = intel_request(capabilities, current, features.hwp_epp)
                else {
                    return Outcome::Unpublished;
                };
                registers.write(HWP_REQUEST, value);
                return Outcome::Hwp {
                    levels: Levels::intel(capabilities),
                    bounds,
                    epp: features.hwp_epp,
                    package: current & HWP_PACKAGE_CONTROL != 0,
                };
            }
            hwp_off = true;
        }
    }
    if features.cppc {
        // SAFETY: CPUID 8000_0008H:EBX[27] on an AMD CPU enumerates the CPPC
        // registers. As amd-pstate's per-CPU init, the levels are read and
        // checked first (`amd_pstate_init_perf`, `amd_pstate_init_freq`)
        // and CPPC is enabled only for usable ones (`amd_pstate_cppc_enable`),
        // so an empty capability register leaves CPPC as the firmware left
        // it. The enable precedes the request. The request keeps its upper
        // bits.
        unsafe {
            let capabilities = registers.read(AMD_CPPC_CAP1);
            let current = registers.read(AMD_CPPC_REQ);
            if let Some((value, bounds)) = amd_request(capabilities, current) {
                if registers.read(AMD_CPPC_ENABLE) & 1 == 0 {
                    registers.write(AMD_CPPC_ENABLE, 1);
                }
                registers.write(AMD_CPPC_REQ, value);
                return Outcome::Cppc {
                    levels: Levels::amd(capabilities),
                    bounds,
                };
            }
        }
    }
    if features.eist && (hwp_off || !features.hwp) {
        // SAFETY: CPUID.01H:ECX[7] enumerates IA32_PERF_CTL, which governs
        // while HWP is off; the ratio registers are probed.
        unsafe {
            let ratio = intel_legacy_ratio(
                registers.probe(PLATFORM_INFO),
                registers.probe(TURBO_RATIO_LIMIT),
            );
            if let Some(ratio) = ratio {
                let current = registers.read(PERF_CTL);
                registers.write(PERF_CTL, perf_ctl(current, ratio));
                return Outcome::PerfCtl { ratio, hwp_off };
            }
        }
    }
    if features.amd_pstates {
        // SAFETY: CPUID 8000_0007H:EDX[7] enumerates PStateCurLim and
        // PStateCtl; PStateCtl's command is bits 2:0, the rest stays.
        unsafe {
            let limit = (registers.read(AMD_PSTATE_LIMIT) & 7) as u8;
            let current = registers.read(AMD_PSTATE_CTL);
            if current & 7 != 0 {
                registers.write(AMD_PSTATE_CTL, current & !7);
            }
            return Outcome::PState { limit };
        }
    }
    if hwp_off {
        return Outcome::HwpWithoutEpp;
    }
    if features.cppc {
        return Outcome::Unpublished;
    }
    Outcome::Legacy
}

/// Ask this processor for full performance through `registers` and count
/// what it did.
///
/// # Safety
/// As [`request_with`], on the calling processor, whose registers
/// `registers` reaches.
pub unsafe fn request_full_performance(registers: &mut impl Registers) -> Report {
    // SAFETY: the caller's contract; `Features::read` describes this CPU.
    let report = unsafe { request_with(Features::read(), registers) };
    TALLY.record(report);
    report
}

/// The lowest and highest of a value over processors.
struct Range([AtomicU32; 2]);

impl Range {
    const fn new() -> Self {
        Self([AtomicU32::new(u32::MAX), AtomicU32::new(0)])
    }

    fn add(&self, value: u8) {
        self.0[0].fetch_min(value.into(), Ordering::Relaxed);
        self.0[1].fetch_max(value.into(), Ordering::Relaxed);
    }

    fn text(&self) -> String {
        match (
            self.0[0].load(Ordering::Relaxed),
            self.0[1].load(Ordering::Relaxed),
        ) {
            (u32::MAX, _) => String::from("-"),
            (low, high) if low == high => format!("{low}"),
            (low, high) => format!("{low}-{high}"),
        }
    }
}

/// One core class's processors.
struct ClassTally {
    processors: AtomicU32,
    outcomes: [AtomicU32; 7],
    package: AtomicU32,
    highest: Range,
    sustained: Range,
    lowest: Range,
    minimum: Range,
    maximum: Range,
    /// PERF_CTL ratio, or AMD's current P-state limit.
    legacy: Range,
    turbo: [AtomicU32; 3],
    modulation: AtomicU32,
    duty: Range,
    energy_bias: Range,
}

impl ClassTally {
    const fn new() -> Self {
        Self {
            processors: AtomicU32::new(0),
            outcomes: [const { AtomicU32::new(0) }; 7],
            package: AtomicU32::new(0),
            highest: Range::new(),
            sustained: Range::new(),
            lowest: Range::new(),
            minimum: Range::new(),
            maximum: Range::new(),
            legacy: Range::new(),
            turbo: [const { AtomicU32::new(0) }; 3],
            modulation: AtomicU32::new(0),
            duty: Range::new(),
            energy_bias: Range::new(),
        }
    }

    fn line(&self, class: Class) -> Option<String> {
        let load = |counter: &AtomicU32| counter.load(Ordering::Relaxed);
        let processors = load(&self.processors);
        if processors == 0 {
            return None;
        }
        let mut mechanisms = String::new();
        for (index, name) in Outcome::NAMES.iter().enumerate() {
            let count = load(&self.outcomes[index]);
            if count != 0 {
                if !mechanisms.is_empty() {
                    mechanisms.push(',');
                }
                mechanisms.push_str(&format!("{name}:{count}"));
            }
        }
        let turbo = self.turbo.each_ref().map(load);
        let modulated = load(&self.modulation);
        Some(format!(
            "HOST POWER class={} processors={processors} mechanism={mechanisms} highest={} \
             nominal={} lowest={} requested_min={} requested_max={} legacy_level={} package_request_released={} \
             turbo(on/firmware_off/absent)={}/{}/{} clock_modulation_cleared={modulated}{} \
             energy_bias_firmware={} energy_bias=0\n",
            class.name(),
            self.highest.text(),
            self.sustained.text(),
            self.lowest.text(),
            self.minimum.text(),
            self.maximum.text(),
            self.legacy.text(),
            load(&self.package),
            turbo[0],
            turbo[1],
            turbo[2],
            if modulated != 0 {
                format!("(duty%={})", self.duty.text())
            } else {
                String::new()
            },
            self.energy_bias.text(),
        ))
    }
}

/// Processors per outcome and core class, for one report after the
/// processors start.
pub struct Tally {
    classes: [ClassTally; 4],
}

impl Tally {
    pub const fn new() -> Self {
        Self {
            classes: [const { ClassTally::new() }; 4],
        }
    }

    /// Count one processor's report.
    pub fn record(&self, report: Report) {
        let class = &self.classes[report.class.index()];
        class.processors.fetch_add(1, Ordering::Relaxed);
        class.outcomes[report.outcome.index()].fetch_add(1, Ordering::Relaxed);
        class.turbo[report.turbo.index()].fetch_add(1, Ordering::Relaxed);
        if let Some(duty) = report.modulation {
            class.modulation.fetch_add(1, Ordering::Relaxed);
            class.duty.add(duty);
        }
        if let Some(bias) = report.energy_bias {
            class.energy_bias.add(bias);
        }
        let (levels, bounds) = match report.outcome {
            Outcome::Hwp {
                levels,
                bounds,
                package,
                ..
            } => {
                class
                    .package
                    .fetch_add(u32::from(package), Ordering::Relaxed);
                (levels, bounds)
            }
            Outcome::Cppc { levels, bounds } => (levels, bounds),
            Outcome::PerfCtl { ratio, .. } => {
                class.legacy.add(ratio);
                return;
            }
            Outcome::PState { limit } => {
                class.legacy.add(limit);
                return;
            }
            _ => return,
        };
        class.highest.add(levels.highest);
        class.sustained.add(levels.sustained);
        class.lowest.add(levels.lowest);
        class.minimum.add(bounds.minimum);
        class.maximum.add(bounds.maximum);
    }

    fn count(&self, index: usize) -> u32 {
        self.classes
            .iter()
            .map(|class| class.outcomes[index].load(Ordering::Relaxed))
            .sum()
    }

    fn merged(&self, range: impl Fn(&ClassTally) -> &Range) -> String {
        let merged = Range::new();
        for class in &self.classes {
            let [low, high] = range(class).0.each_ref().map(|v| v.load(Ordering::Relaxed));
            if low != u32::MAX {
                merged.add(low as u8);
                merged.add(high as u8);
            }
        }
        match merged.text().as_str() {
            "-" => String::from("none"),
            text => String::from(text),
        }
    }

    /// The outcomes as the `HOST PERFORMANCE` line, one `HOST POWER` line
    /// per core class, and one `UNSUPPORTED` line for processors that
    /// published no usable levels, and one `HOST POWER turbo` line for
    /// processors whose firmware switched turbo off.
    pub fn summary(&self) -> String {
        let package: u32 = self
            .classes
            .iter()
            .map(|class| class.package.load(Ordering::Relaxed))
            .sum();
        let mut line = format!(
            "HOST PERFORMANCE hwp={} cppc={} hwp_without_epp={} unpublished={} legacy={} \
             package_controlled={package} highest={} requested_minimum={} perf_ctl={} pstate={} \
             preference=performance\n",
            self.count(0),
            self.count(1),
            self.count(4),
            self.count(5),
            self.count(6),
            self.merged(|class| &class.highest),
            self.merged(|class| &class.minimum),
            self.count(2),
            self.count(3),
        );
        for class in Class::ALL {
            if let Some(text) = self.classes[class.index()].line(class) {
                line.push_str(&text);
            }
        }
        let unpublished = self.count(5);
        if unpublished != 0 {
            line.push_str(&format!(
                "UNSUPPORTED host performance request on {unpublished} processors: their HWP or \
                 CPPC capability register publishes no usable levels and no legacy P-state \
                 register is reported, so each keeps the request its firmware left\n"
            ));
        }
        let off: u32 = self
            .classes
            .iter()
            .map(|class| class.turbo[Turbo::FirmwareOff.index()].load(Ordering::Relaxed))
            .sum();
        if off != 0 {
            line.push_str(&format!(
                "HOST POWER turbo is off on {off} processors: the PC's firmware switched it off \
                 and Stela leaves it so, so they run at most at their base clock; the turbo or \
                 boost setting in the firmware's setup switches it on\n"
            ));
        }
        line
    }
}

impl Default for Tally {
    fn default() -> Self {
        Self::new()
    }
}

static TALLY: Tally = Tally::new();

/// The reports of every [`request_full_performance`] as report lines.
pub fn summary() -> String {
    TALLY.summary()
}

#[cfg(test)]
mod tests;
