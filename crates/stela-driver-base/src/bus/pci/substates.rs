//! A link's L1 PM Substates as Linux's ASPM core leaves them when it finds
//! a link the firmware set up (`drivers/pci/pcie/aspm.c`,
//! `pcie_aspm_cap_init`, `aspm_l1ss_init`, `aspm_calc_l12_info` and
//! `pcie_config_aspm_link` under the default policy, which keeps what the
//! firmware enabled): the L1.2 timing both ends must share, computed from
//! both ends' capabilities, and no ASPM L1.2 on a link without LTR from the
//! root port down to the device, as L1.2 entry relies on the
//! LTR_L1.2_THRESHOLD (PCIe r6.0 5.5.4, 6.18).
//!
//! A link the firmware left with no substate on is left as it is: Stela
//! never enables a substate the firmware did not. Linux's exit latency
//! check (`pcie_aspm_check_latency`) and common clock setup are not part
//! of this.
use super::{
    CAPABILITY_EXPRESS, CapabilityError, ConfigError, ConfigSpace,
    capability::{capabilities, extended_capabilities},
};
use core::fmt;

const EXTENDED_L1SS: u16 = 0x1e;
/// PCI Express capability registers (PCIe r6.0 7.5.3).
const FLAGS: u16 = 0x02;
const LINK_CONTROL: u16 = 0x10;
const DEVICE_CAPABILITIES_2: u16 = 0x24;
const DEVICE_CONTROL_2: u16 = 0x28;
const ROOT_PORT: u16 = 4;
/// Link Control's ASPM Control field: L0s, L1.
const ASPM: u16 = 3;
const ASPM_L0S: u16 = 1;
const ASPM_L1: u16 = 2;
/// LTR Mechanism Supported, in Device Capabilities 2, and its enable, in
/// Device Control 2.
const LTR_SUPPORTED: u32 = 1 << 11;
const LTR_ENABLE: u16 = 1 << 10;

/// L1 PM Substates registers (PCIe r6.0 7.8.3), at their capability.
const CAPABILITIES: u16 = 4;
const CONTROL_1: u16 = 8;
const CONTROL_2: u16 = 0xc;
/// Capabilities: the four substates, laid out as Control 1's enables, and
/// L1 PM Substates Supported.
const SUBSTATES: u32 = 0xf;
const PCI_PM_L1_2: u32 = 1 << 0;
const ASPM_L1_2: u32 = 1 << 2;
const L1_2: u32 = PCI_PM_L1_2 | ASPM_L1_2;
const SUPPORTED: u32 = 1 << 4;
/// Port Common_Mode_Restore_Time, T_POWER_ON Scale and Value.
const CAP_COMMON_MODE: u32 = 0xff00;
const CAP_POWER_ON_SCALE: u32 = 0x3_0000;
const CAP_POWER_ON_VALUE: u32 = 0xf8_0000;
/// Control 1: Common_Mode_Restore_Time, LTR_L1.2_THRESHOLD Value and Scale.
const COMMON_MODE: u32 = 0xff00;
const THRESHOLD_VALUE: u32 = 0x03ff_0000;
const THRESHOLD_SCALE: u32 = 0xe000_0000;
/// Control 2: T_POWER_ON Scale and Value.
const POWER_ON_SCALE: u32 = 0x3;
const POWER_ON_VALUE: u32 = 0xf8;

/// Why a link's substates could not be read or set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Config(ConfigError),
    Capability(CapabilityError),
}

impl From<ConfigError> for Error {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<CapabilityError> for Error {
    fn from(error: CapabilityError) -> Self {
        Self::Capability(error)
    }
}

/// What [`configure`] found and did, for one boot-log line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Configured {
    /// One end has no PCI Express or L1 PM Substates capability.
    Absent,
    /// The firmware left no substate on at either end: nothing changed.
    FirmwareOff,
    /// Linux's settings: the substates on at both ends before and now
    /// (Control 1's enable bits), whether the L1.2 timing differed and was
    /// written, the shared Control 1 timing and Control 2, whether LTR
    /// reaches the device from the root port, and whether both ends had
    /// ASPM L1 on.
    Set {
        before: u32,
        after: u32,
        programmed: bool,
        timing: u32,
        power_on: u32,
        ltr_path: bool,
        l1: bool,
    },
}

/// `ASPM-L1.1+ASPM-L1.2` for Control 1's enable bits.
struct Names(u32);

impl fmt::Display for Names {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = [
            (3, "ASPM-L1.1"),
            (2, "ASPM-L1.2"),
            (1, "PM-L1.1"),
            (0, "PM-L1.2"),
        ];
        let mut first = true;
        for (bit, name) in names {
            if self.0 & 1 << bit != 0 {
                f.write_str(if first { "" } else { "+" })?;
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

impl fmt::Display for Configured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Absent => f.write_str("not on this link, left as it is"),
            Self::FirmwareOff => {
                f.write_str("off at both ends as the firmware left them, left off")
            }
            Self::Set {
                before,
                after,
                programmed,
                timing,
                power_on,
                ltr_path,
                l1,
            } => {
                write!(
                    f,
                    "{} as the firmware left them, now {}",
                    Names(before),
                    Names(after)
                )?;
                if before & !after != 0 {
                    f.write_str(if !l1 {
                        "; off as Linux keeps no substate where both ends do not have ASPM L1 on"
                    } else if before & !after & ASPM_L1_2 != 0 && !ltr_path {
                        "; ASPM L1.2 off as Linux turns it off where LTR does not reach the \
                         device from the root port"
                    } else {
                        "; off as Linux keeps only what both ends' capabilities list"
                    })?;
                }
                if programmed {
                    write!(
                        f,
                        "; L1.2 timing written as Linux computes it from both ends: control \
                         {timing:08x}/{power_on:08x}"
                    )?;
                } else {
                    f.write_str("; L1.2 timing already as Linux computes it")?;
                }
                Ok(())
            }
        }
    }
}

/// The PCI Express capability's offset of `config`, if any.
fn express<C: ConfigSpace + ?Sized>(config: &C) -> Result<Option<u16>, Error> {
    for entry in capabilities(config) {
        let entry = entry?;
        if entry.id == CAPABILITY_EXPRESS {
            return Ok(Some(entry.offset));
        }
    }
    Ok(None)
}

/// The L1 PM Substates capability's offset of `config`, if any.
fn l1ss<C: ConfigSpace + ?Sized>(config: &C) -> Result<Option<u16>, Error> {
    for entry in extended_capabilities(config) {
        let entry = entry?;
        if entry.id == EXTENDED_L1SS {
            return Ok(Some(entry.offset));
        }
    }
    Ok(None)
}

/// One component's LTR, as Linux's `pci_configure_ltr` reads it: whether
/// it is a root port, and whether it supports LTR and has it enabled.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Ltr {
    pub root_port: bool,
    pub enabled: bool,
}

/// [`Ltr`] of `config`; `None` for a function without a PCI Express
/// capability.
pub fn ltr<C: ConfigSpace + ?Sized>(config: &C) -> Result<Option<Ltr>, Error> {
    let Some(at) = express(config)? else {
        return Ok(None);
    };
    let port_type = config.read16(at + FLAGS)? >> 4 & 0xf;
    let supported = config.read32(at + DEVICE_CAPABILITIES_2)? & LTR_SUPPORTED != 0;
    let enabled = config.read16(at + DEVICE_CONTROL_2)? & LTR_ENABLE != 0;
    Ok(Some(Ltr {
        root_port: port_type == ROOT_PORT,
        enabled: supported && enabled,
    }))
}

/// Linux's `ltr_path` of a device whose components from it up to the root
/// complex are `chain`, the device first: each has LTR enabled and the last
/// is a root port.
pub fn ltr_path(chain: &[Ltr]) -> bool {
    chain.last().is_some_and(|top| top.root_port) && chain.iter().all(|end| end.enabled)
}

/// Linux's `calc_l12_pwron`: a T_POWER_ON in microseconds, or `None` for
/// the reserved scale, which Linux reports and takes as zero.
fn power_on_us(scale: u32, value: u32) -> u32 {
    match scale {
        0 => value * 2,
        1 => value * 10,
        2 => value * 100,
        _ => 0,
    }
}

/// Linux's `encode_l12_threshold`: the LTR_L1.2_THRESHOLD Scale and Value
/// for `us` microseconds, rounded up.
fn threshold(us: u32) -> (u32, u32) {
    let ns = u64::from(us) * 1000;
    let most = u64::from(THRESHOLD_VALUE >> 16);
    for (scale, unit) in [1u64, 32, 1024, 32768, 1_048_576, 33_554_432]
        .into_iter()
        .enumerate()
    {
        if ns <= unit * most {
            return (scale as u32, ns.div_ceil(unit) as u32);
        }
    }
    (5, most as u32)
}

/// Linux's `aspm_calc_l12_info`: the Control 1 timing and Control 2 both
/// ends share, from their capabilities: the greater Common_Mode_Restore_Time
/// and T_POWER_ON, and the threshold of an L0 to L1.2 and back transition.
fn l12_timing(parent: u32, child: u32) -> (u32, u32) {
    let common_mode = ((parent & CAP_COMMON_MODE) >> 8).max((child & CAP_COMMON_MODE) >> 8);
    let power_on = |cap: u32| {
        let scale = (cap & CAP_POWER_ON_SCALE) >> 16;
        let value = (cap & CAP_POWER_ON_VALUE) >> 19;
        (power_on_us(scale, value), scale, value)
    };
    let (parent_us, parent_scale, parent_value) = power_on(parent);
    let (child_us, child_scale, child_value) = power_on(child);
    let (us, scale, value) = if parent_us > child_us {
        (parent_us, parent_scale, parent_value)
    } else {
        (child_us, child_scale, child_value)
    };
    let control_2 = scale | value << 3;
    // T(POWER_OFF) is at most 2 us and T(L1.2) at least 4 us.
    let (threshold_scale, threshold_value) = threshold(2 + 4 + common_mode + us);
    let control_1 = common_mode << 8 | threshold_value << 16 | threshold_scale << 29;
    (control_1, control_2)
}

/// Clear `clear` and set `set` in the DWORD at `offset`, as Linux's
/// `pci_clear_and_set_config_dword`.
fn update<C: ConfigSpace + ?Sized>(
    config: &mut C,
    offset: u16,
    clear: u32,
    set: u32,
) -> Result<(), ConfigError> {
    let value = config.read32(offset)?;
    config.write32(offset, value & !clear | set)
}

/// Clear `clear` and set `set` in Link Control.
fn link<C: ConfigSpace + ?Sized>(
    config: &mut C,
    at: u16,
    clear: u16,
    set: u16,
) -> Result<(), ConfigError> {
    let value = config.read16(at + LINK_CONTROL)?;
    config.write16(at + LINK_CONTROL, value & !clear | set)
}

/// Set the L1 PM Substates of the link between the port `parent` and the
/// device function `child`, in D0 at both ends, as Linux leaves a link the
/// firmware set up ([module](self)), with `ltr_path` from [`ltr_path`].
/// Writes in Linux's order: ASPM off at both ends, the child's first; L1.2
/// off at both ends, the child's first, while the timing is written; the
/// substates kept set again, the parent's first; ASPM back as it was, the
/// parent's first.
pub fn configure<P: ConfigSpace + ?Sized, C: ConfigSpace + ?Sized>(
    parent: &mut P,
    child: &mut C,
    ltr_path: bool,
) -> Result<Configured, Error> {
    let (Some(parent_express), Some(child_express)) = (express(parent)?, express(child)?) else {
        return Ok(Configured::Absent);
    };
    let (Some(parent_l1ss), Some(child_l1ss)) = (l1ss(parent)?, l1ss(child)?) else {
        return Ok(Configured::Absent);
    };
    let supported = |cap: u32| if cap & SUPPORTED != 0 { cap } else { 0 };
    let parent_cap = supported(parent.read32(parent_l1ss + CAPABILITIES)?);
    let mut child_cap = supported(child.read32(child_l1ss + CAPABILITIES)?);
    let parent_control = parent.read32(parent_l1ss + CONTROL_1)?;
    let child_control = child.read32(child_l1ss + CONTROL_1)?;
    if (parent_control | child_control) & SUBSTATES == 0 {
        return Ok(Configured::FirmwareOff);
    }
    if !ltr_path {
        child_cap &= !ASPM_L1_2;
    }
    let support = parent_cap & child_cap & SUBSTATES;
    let before = parent_control & child_control & SUBSTATES;
    let parent_link = parent.read16(parent_express + LINK_CONTROL)?;
    let child_link = child.read16(child_express + LINK_CONTROL)?;

    // pcie_aspm_cap_init: L0s and L1 off at both ends while the substates'
    // configuration changes, the child's first.
    let aspm_on = (parent_link | child_link) & ASPM != 0;
    if aspm_on {
        link(child, child_express, ASPM, 0)?;
        link(parent, parent_express, ASPM, 0)?;
    }
    let (timing, power_on) = l12_timing(parent_cap, child_cap);
    let mut programmed = false;
    if support & L1_2 != 0 {
        let parent_now = parent.read32(parent_l1ss + CONTROL_1)?;
        let parent_2 = parent.read32(parent_l1ss + CONTROL_2)?;
        let child_now = child.read32(child_l1ss + CONTROL_1)?;
        let child_2 = child.read32(child_l1ss + CONTROL_2)?;
        let held = COMMON_MODE | THRESHOLD_VALUE | THRESHOLD_SCALE;
        // Linux programs Common_Mode_Restore_Time in the parent only.
        let threshold = THRESHOLD_VALUE | THRESHOLD_SCALE;
        programmed = timing != parent_now & held
            || timing & threshold != child_now & threshold
            || power_on != parent_2 & (POWER_ON_SCALE | POWER_ON_VALUE)
            || power_on != child_2 & (POWER_ON_SCALE | POWER_ON_VALUE);
        // Linux compares whole registers, so it writes whenever an end has
        // an enable or another bit set, if only the same timing again.
        let writes = programmed
            || timing != parent_now
            || timing != child_now
            || power_on != parent_2
            || power_on != child_2;
        if writes {
            let parent_l1_2 = parent_now & L1_2;
            let child_l1_2 = child_now & L1_2;
            if parent_l1_2 | child_l1_2 != 0 {
                update(child, child_l1ss + CONTROL_1, L1_2, 0)?;
                update(parent, parent_l1ss + CONTROL_1, L1_2, 0)?;
            }
            let power = POWER_ON_SCALE | POWER_ON_VALUE;
            update(parent, parent_l1ss + CONTROL_2, power, power_on)?;
            update(child, child_l1ss + CONTROL_2, power, power_on)?;
            update(
                parent,
                parent_l1ss + CONTROL_1,
                COMMON_MODE,
                timing & COMMON_MODE,
            )?;
            update(
                parent,
                parent_l1ss + CONTROL_1,
                threshold,
                timing & threshold,
            )?;
            update(child, child_l1ss + CONTROL_1, threshold, timing & threshold)?;
            if parent_l1_2 | child_l1_2 != 0 {
                update(parent, parent_l1ss + CONTROL_1, 0, parent_l1_2)?;
                update(child, child_l1ss + CONTROL_1, 0, child_l1_2)?;
            }
        }
    }
    // pcie_config_aspm_link under the default policy: what the firmware
    // enabled that the link can do, and no substate without ASPM L1.
    let l1 = parent_link & child_link & ASPM_L1 != 0;
    let after = if l1 { before & support } else { 0 };
    if after != before {
        if support != 0 {
            update(child, child_l1ss + CONTROL_1, SUBSTATES, 0)?;
            update(parent, parent_l1ss + CONTROL_1, SUBSTATES, 0)?;
            update(parent, parent_l1ss + CONTROL_1, SUBSTATES, after)?;
            update(child, child_l1ss + CONTROL_1, SUBSTATES, after)?;
        }
        // Linux writes each end's L0s as it was and L1 only where both
        // ends had it, the parent's first.
        let both_l1 = if l1 { ASPM_L1 } else { 0 };
        link(
            parent,
            parent_express,
            ASPM,
            parent_link & ASPM_L0S | both_l1,
        )?;
        link(child, child_express, ASPM, child_link & ASPM_L0S | both_l1)?;
    } else if aspm_on {
        link(parent, parent_express, ASPM, parent_link & ASPM)?;
        link(child, child_express, ASPM, child_link & ASPM)?;
    }
    Ok(Configured::Set {
        before,
        // Without a substate both ends list, Linux leaves the registers.
        after: if support != 0 { after } else { before },
        programmed,
        timing,
        power_on,
        ltr_path,
        l1,
    })
}

#[cfg(test)]
mod tests;
