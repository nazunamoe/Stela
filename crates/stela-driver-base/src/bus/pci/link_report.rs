//! A PCI Express link's power management as one end of it reports it, for
//! the boot log: ASPM and Clock Power Management as supported and as set,
//! the link's state, LTR, the L1 PM Substates, and the error status a link
//! that went down leaves. Read at a GPU's start, from both ends, and from
//! the port above it when the GPU stops answering, so a log tells a link
//! that dropped out of a low power state from power that went away. Reads
//! only; a function that does not answer reads all ones and says so.
use super::{
    CAPABILITY_EXPRESS, CAPABILITY_POWER, ConfigError, ConfigSpace,
    capability::{capabilities, extended_capabilities},
};
use core::fmt;

/// Extended capability IDs: Advanced Error Reporting, LTR, L1 PM Substates.
const EXTENDED_AER: u16 = 0x01;
const EXTENDED_LTR: u16 = 0x18;
const EXTENDED_L1SS: u16 = 0x1e;

/// PCI Express capability registers (PCIe r6.0 7.5.3).
const FLAGS: u16 = 0x02;
const DEVICE_CONTROL: u16 = 0x08;
const LINK_CAPABILITIES: u16 = 0x0c;
const LINK_CONTROL: u16 = 0x10;
const SLOT_STATUS: u16 = 0x1a;
const DEVICE_CAPABILITIES_2: u16 = 0x24;
const DEVICE_CONTROL_2: u16 = 0x28;
const SLOT_IMPLEMENTED: u16 = 1 << 8;
const ROOT_PORT: u16 = 4;

/// The registers of the PCI Express capability this report reads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Express {
    /// Device/Port Type, from the capability's flags.
    pub port_type: u16,
    /// Device Status (upper half of the Device Control DWORD).
    pub device_status: u16,
    pub link_capabilities: u32,
    pub link_control: u16,
    pub link_status: u16,
    /// Slot Status, when the port implements a slot.
    pub slot_status: Option<u16>,
    pub device_capabilities_2: u32,
    pub device_control_2: u16,
}

/// What one end of a link reports ([`read`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkEnd {
    pub vendor: u16,
    pub command: u16,
    pub status: u16,
    /// PM Control/Status, when the function has the capability.
    pub power: Option<u16>,
    pub express: Option<Express>,
    /// LTR's Max Snoop and Max No-Snoop Latency DWORD.
    pub ltr: Option<u32>,
    /// L1 PM Substates Capabilities, Control 1 and Control 2.
    pub l1ss: Option<[u32; 3]>,
    /// AER Uncorrectable and Correctable Error Status, and Root Error Status
    /// on a root port.
    pub aer: Option<[u32; 3]>,
}

impl LinkEnd {
    /// Whether the function answered: a Vendor ID that is not all ones.
    pub fn answers(&self) -> bool {
        self.vendor != u16::MAX
    }
}

/// Read what [`LinkEnd`] holds of the function `config` reaches. A function
/// that does not answer gives its all-ones Vendor ID and nothing else; a
/// capability list that does not read whole ends the walk where it fails.
pub fn read<C: ConfigSpace + ?Sized>(config: &C) -> Result<LinkEnd, ConfigError> {
    let identity = config.read32(0)?;
    let mut end = LinkEnd {
        vendor: identity as u16,
        ..LinkEnd::default()
    };
    if !end.answers() {
        return Ok(end);
    }
    let control = config.read32(4)?;
    end.command = control as u16;
    end.status = (control >> 16) as u16;
    let mut express_at = None;
    for entry in capabilities(config) {
        let Ok(entry) = entry else { break };
        match entry.id {
            CAPABILITY_POWER if end.power.is_none() => {
                end.power = Some(config.read16(entry.offset + 4)?);
            }
            CAPABILITY_EXPRESS if express_at.is_none() => express_at = Some(entry.offset),
            _ => {}
        }
    }
    let Some(at) = express_at else {
        return Ok(end);
    };
    let flags = config.read16(at + FLAGS)?;
    let link = config.read32(at + LINK_CONTROL)?;
    let port_type = flags >> 4 & 0xf;
    end.express = Some(Express {
        port_type,
        device_status: (config.read32(at + DEVICE_CONTROL)? >> 16) as u16,
        link_capabilities: config.read32(at + LINK_CAPABILITIES)?,
        link_control: link as u16,
        link_status: (link >> 16) as u16,
        slot_status: if flags & SLOT_IMPLEMENTED != 0 {
            Some(config.read16(at + SLOT_STATUS)?)
        } else {
            None
        },
        device_capabilities_2: config.read32(at + DEVICE_CAPABILITIES_2)?,
        device_control_2: config.read16(at + DEVICE_CONTROL_2)?,
    });
    for entry in extended_capabilities(config) {
        let Ok(entry) = entry else { break };
        let at = entry.offset;
        match entry.id {
            EXTENDED_LTR if end.ltr.is_none() => end.ltr = Some(config.read32(at + 4)?),
            EXTENDED_L1SS if end.l1ss.is_none() => {
                end.l1ss = Some([
                    config.read32(at + 4)?,
                    config.read32(at + 8)?,
                    config.read32(at + 0xc)?,
                ]);
            }
            EXTENDED_AER if end.aer.is_none() => {
                let root = if port_type == ROOT_PORT {
                    config.read32(at + 0x30)?
                } else {
                    0
                };
                end.aer = Some([config.read32(at + 4)?, config.read32(at + 0x10)?, root]);
            }
            _ => {}
        }
    }
    Ok(end)
}

/// `L0s+L1` for an ASPM field, as Link Capabilities and Link Control lay it.
struct Aspm(u32);

impl fmt::Display for Aspm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(["off", "L0s", "L1", "L0s+L1"][(self.0 & 3) as usize])
    }
}

/// `L1.1+L1.2` for L1 PM Substates bits laid out as Control 1's enables
/// (bit 0 PCI-PM L1.2, 1 PCI-PM L1.1, 2 ASPM L1.2, 3 ASPM L1.1).
struct Substates(u32);

impl fmt::Display for Substates {
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

/// One line's worth, without the function's name:
/// `vendor 10de command 0406 status 0010 D0; aspm supported L0s+L1 on L1, clock PM supported 1 on 1, common clock 1; link gen3 x8 active 1 training 0; device status 0000; LTR supported 1 on 1 limits 10031003; L1SS supported ASPM-L1.1+ASPM-L1.2+PM-L1.1+PM-L1.2 on ASPM-L1.2 control 4060280c/00000028; AER uncorrectable 00000000 correctable 00000000`.
impl fmt::Display for LinkEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.answers() {
            return f.write_str("does not answer (vendor ffff)");
        }
        write!(
            f,
            "vendor {:04x} command {:04x} status {:04x}",
            self.vendor, self.command, self.status
        )?;
        if let Some(power) = self.power {
            write!(f, " D{}", power & 3)?;
        }
        let Some(express) = self.express else {
            return f.write_str("; no PCI Express capability");
        };
        let capabilities = express.link_capabilities;
        let control = express.link_control;
        let status = express.link_status;
        write!(
            f,
            "; aspm supported {} on {}, clock PM supported {} on {}, common clock {}",
            Aspm(capabilities >> 10),
            Aspm(u32::from(control)),
            capabilities >> 18 & 1,
            control >> 8 & 1,
            control >> 6 & 1,
        )?;
        write!(
            f,
            "; link gen{} x{} active {} training {}; device status {:04x}",
            status & 0xf,
            status >> 4 & 0x3f,
            status >> 13 & 1,
            status >> 11 & 1,
            express.device_status,
        )?;
        if let Some(slot) = express.slot_status {
            write!(f, "; slot status {slot:04x}")?;
        }
        write!(
            f,
            "; LTR supported {} on {}",
            express.device_capabilities_2 >> 11 & 1,
            express.device_control_2 >> 10 & 1,
        )?;
        if let Some(limits) = self.ltr {
            write!(f, " limits {limits:08x}")?;
        }
        match self.l1ss {
            Some([capability, control_1, control_2]) => write!(
                f,
                "; L1SS supported {} on {} control {control_1:08x}/{control_2:08x}",
                // Capabilities lay the four out as Control 1 does.
                Substates(capability & 0xf),
                Substates(control_1 & 0xf),
            )?,
            None => f.write_str("; no L1SS")?,
        }
        if let Some([uncorrectable, correctable, root]) = self.aer {
            write!(
                f,
                "; AER uncorrectable {uncorrectable:08x} correctable {correctable:08x}"
            )?;
            if express.port_type == ROOT_PORT {
                write!(f, " root {root:08x}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Words([u32; 1024]);

    impl ConfigSpace for Words {
        fn read32(&self, offset: u16) -> Result<u32, ConfigError> {
            Ok(self.0[usize::from(offset / 4)])
        }
        fn read16(&self, offset: u16) -> Result<u16, ConfigError> {
            Ok((self.0[usize::from(offset / 4)] >> ((offset & 2) * 8)) as u16)
        }
        fn read8(&self, offset: u16) -> Result<u8, ConfigError> {
            Ok((self.0[usize::from(offset / 4)] >> ((offset & 3) * 8)) as u8)
        }
        fn write8(&mut self, _: u16, _: u8) -> Result<(), ConfigError> {
            unreachable!("the report only reads")
        }
        fn write16(&mut self, _: u16, _: u16) -> Result<(), ConfigError> {
            unreachable!("the report only reads")
        }
        fn write32(&mut self, _: u16, _: u32) -> Result<(), ConfigError> {
            unreachable!("the report only reads")
        }
    }

    /// A root port with L1 and L1.2 on, a slot, AER, LTR and L1SS.
    fn root_port() -> Words {
        let mut words = [0; 1024];
        words[0] = 0x1633_1022;
        words[1] = 0x0010_0407;
        words[0x34 / 4] = 0x50;
        words[0x50 / 4] = 0x0003_5801;
        words[0x54 / 4] = 0x0000_0008;
        words[0x58 / 4] = 0x0142_0010;
        words[0x5c / 4] = 0x0000_0000;
        words[0x60 / 4] = 0x0000_0000;
        // Link Capabilities: L0s+L1 supported, clock PM; control L1, common
        // clock, clock PM on; status gen3 x8, DLLLA.
        words[0x64 / 4] = 0x0007_cc83;
        words[0x68 / 4] = 0x2083_0142;
        words[0x6c / 4] = 0;
        words[0x70 / 4] = 0x0040_0000;
        words[0x7c / 4] = 0x0000_0800;
        words[0x80 / 4] = 0x0000_0400;
        words[0x100 / 4] = 0x1401_0001;
        words[0x104 / 4] = 0x0000_4000;
        words[0x110 / 4] = 0x0000_2000;
        words[0x130 / 4] = 0x0000_0004;
        words[0x140 / 4] = 0x1501_0018;
        words[0x144 / 4] = 0x1003_1003;
        words[0x150 / 4] = 0x0001_001e;
        words[0x154 / 4] = 0x0028_281f;
        words[0x158 / 4] = 0x4060_2804;
        words[0x15c / 4] = 0x0000_0028;
        Words(words)
    }

    #[test]
    fn a_root_port_reports_its_link_power_and_error_status() {
        let end = read(&root_port()).unwrap();
        assert!(end.answers());
        assert_eq!(
            alloc::format!("{end}"),
            "vendor 1022 command 0407 status 0010 D0; aspm supported L0s+L1 on L1, clock PM \
             supported 1 on 1, common clock 1; link gen3 x8 active 1 training 0; device status \
             0000; slot status 0040; LTR supported 1 on 1 limits 10031003; L1SS supported \
             ASPM-L1.1+ASPM-L1.2+PM-L1.1+PM-L1.2 on ASPM-L1.2 control 40602804/00000028; AER \
             uncorrectable 00004000 correctable 00002000 root 00000004"
        );
    }

    #[test]
    fn a_function_off_the_bus_says_only_that() {
        let end = read(&Words([u32::MAX; 1024])).unwrap();
        assert!(!end.answers());
        assert_eq!(alloc::format!("{end}"), "does not answer (vendor ffff)");
    }
}
