use super::*;
use alloc::{rc::Rc, vec::Vec};
use core::cell::RefCell;

/// Every write to either end, in order: which end, offset, value.
type Log = Rc<RefCell<Vec<(&'static str, u16, u32)>>>;

struct End {
    name: &'static str,
    words: [u32; 1024],
    log: Log,
}

impl End {
    fn word(&self, offset: u16) -> u32 {
        self.words[usize::from(offset / 4)]
    }
}

impl ConfigSpace for End {
    fn read32(&self, offset: u16) -> Result<u32, ConfigError> {
        Ok(self.word(offset))
    }
    fn read16(&self, offset: u16) -> Result<u16, ConfigError> {
        Ok((self.word(offset) >> ((offset & 2) * 8)) as u16)
    }
    fn read8(&self, offset: u16) -> Result<u8, ConfigError> {
        Ok((self.word(offset) >> ((offset & 3) * 8)) as u8)
    }
    fn write8(&mut self, _: u16, _: u8) -> Result<(), ConfigError> {
        unreachable!("no byte writes")
    }
    fn write16(&mut self, offset: u16, value: u16) -> Result<(), ConfigError> {
        let shift = (offset & 2) * 8;
        let word = &mut self.words[usize::from(offset / 4)];
        *word = *word & !(0xffff << shift) | u32::from(value) << shift;
        self.log
            .borrow_mut()
            .push((self.name, offset, u32::from(value)));
        Ok(())
    }
    fn write32(&mut self, offset: u16, value: u32) -> Result<(), ConfigError> {
        self.words[usize::from(offset / 4)] = value;
        self.log.borrow_mut().push((self.name, offset, value));
        Ok(())
    }
}

/// Firmware's L1 PM Substates Control 1 on both ends: ASPM L1.1 and L1.2
/// on, Common_Mode_Restore_Time 40 us and an LTR_L1.2_THRESHOLD of
/// 96 x 1024 ns; Control 2: T_POWER_ON 5 x 2 us.
const FIRMWARE_CONTROL_1: u32 = 0x4060_280c;
const FIRMWARE_CONTROL_2: u32 = 0x0000_0028;

/// Root port: PCI Express capability at 0x58 (a root port), ASPM L1 on with
/// common clock and Clock PM, LTR supported and enabled; L1 PM Substates at
/// 0x150 with every substate, Common_Mode_Restore_Time 0 and T_POWER_ON
/// 15 x 10 us.
const PORT_LINK: u16 = 0x58 + 0x10;
const PORT_L1SS: u16 = 0x150;
fn root_port(log: &Log) -> End {
    let mut words = [0; 1024];
    words[0] = 0x1633_1022;
    words[1] = 0x0010_0407;
    words[0x34 / 4] = 0x50;
    words[0x50 / 4] = 0x0003_5801;
    words[0x58 / 4] = 0x0142_0010;
    words[0x64 / 4] = 0x0007_cc83;
    words[0x68 / 4] = 0x2083_0142;
    words[0x7c / 4] = 0x0000_0800;
    words[0x80 / 4] = 0x0000_0400;
    words[0x100 / 4] = 0x1401_0001;
    words[0x140 / 4] = 0x1501_0018;
    words[0x150 / 4] = 0x0001_001e;
    words[0x154 / 4] = 0x0079_001f;
    words[0x158 / 4] = FIRMWARE_CONTROL_1;
    words[0x15c / 4] = FIRMWARE_CONTROL_2;
    End {
        name: "port",
        words,
        log: log.clone(),
    }
}

/// A GA107-class GPU function: PCI Express capability at 0x68 (an
/// endpoint), ASPM L1 on with common clock and Clock PM, LTR supported and
/// enabled; LTR at 0x100, L1 PM Substates at 0x120 with every substate,
/// Common_Mode_Restore_Time 255 us and T_POWER_ON 1 x 10 us.
const GPU_LINK: u16 = 0x68 + 0x10;
const GPU_L1SS: u16 = 0x120;
fn gpu(log: &Log) -> End {
    let mut words = [0; 1024];
    words[0] = 0x25a2_10de;
    words[1] = 0x0010_0406;
    words[0x34 / 4] = 0x60;
    words[0x60 / 4] = 0x0003_6801;
    words[0x68 / 4] = 0x0002_0010;
    words[0x74 / 4] = 0x0047_cc83;
    words[0x78 / 4] = 0x1083_0142;
    words[0x8c / 4] = 0x0000_0800;
    words[0x90 / 4] = 0x0000_0400;
    words[0x100 / 4] = 0x1201_0018;
    words[0x104 / 4] = 0x1003_1003;
    words[0x120 / 4] = 0x0001_001e;
    words[0x124 / 4] = 0x0009_ff1f;
    words[0x128 / 4] = FIRMWARE_CONTROL_1;
    words[0x12c / 4] = FIRMWARE_CONTROL_2;
    End {
        name: "gpu",
        words,
        log: log.clone(),
    }
}

/// Linux's numbers for these two ends: Common_Mode_Restore_Time 255 us,
/// T_POWER_ON the port's 150 us (scale 1, value 15), and a threshold of
/// 2 + 4 + 255 + 150 = 411 us, 402 x 1024 ns rounded up.
const LINUX_TIMING: u32 = 0x4192_ff00;
const LINUX_POWER_ON: u32 = 0x79;

#[test]
fn linux_encodes_the_l1_2_threshold_rounded_up() {
    assert_eq!(threshold(1), (0, 1000));
    assert_eq!(threshold(411), (2, 402));
    assert_eq!(threshold(40), (2, 40));
    assert_eq!(threshold(u32::MAX / 1000), (5, 128));
    assert_eq!(
        l12_timing(0x0079_001f, 0x0009_ff1f),
        (LINUX_TIMING, LINUX_POWER_ON)
    );
}

#[test]
fn the_l1_2_timing_is_linuxs_from_both_ends_with_l1_2_and_aspm_off_while_it_is_written() {
    let log = Log::default();
    let (mut port, mut gpu) = (root_port(&log), gpu(&log));
    let configured = configure(&mut port, &mut gpu, true).unwrap();
    assert_eq!(
        configured,
        Configured::Set {
            before: 0xc,
            after: 0xc,
            programmed: true,
            timing: LINUX_TIMING,
            power_on: LINUX_POWER_ON,
            ltr_path: true,
            l1: true,
        }
    );
    // The port takes Common_Mode_Restore_Time; both take the threshold and
    // T_POWER_ON; the enables come back as the firmware left them.
    assert_eq!(port.word(PORT_L1SS + 8), LINUX_TIMING | 0xc);
    assert_eq!(
        gpu.word(GPU_L1SS + 8),
        LINUX_TIMING & !0xff00 | 0x2800 | 0xc
    );
    assert_eq!(port.word(PORT_L1SS + 0xc), LINUX_POWER_ON);
    assert_eq!(gpu.word(GPU_L1SS + 0xc), LINUX_POWER_ON);
    assert_eq!(port.read16(PORT_LINK).unwrap() & 3, 2);
    assert_eq!(gpu.read16(GPU_LINK).unwrap() & 3, 2);

    let writes = log.borrow().clone();
    let link = |end, value: u32| (end, if end == "gpu" { GPU_LINK } else { PORT_LINK }, value);
    // ASPM off, the GPU's first, before any substate register is written.
    assert_eq!(writes[0], link("gpu", 0x0140));
    assert_eq!(writes[1], link("port", 0x0140));
    // L1.2 off at both ends, the GPU's first.
    assert_eq!(writes[2], ("gpu", GPU_L1SS + 8, FIRMWARE_CONTROL_1 & !5));
    assert_eq!(writes[3], ("port", PORT_L1SS + 8, FIRMWARE_CONTROL_1 & !5));
    // Every timing write lands with L1.2 off at both ends.
    let enables_back = writes
        .iter()
        .position(|&(end, offset, value)| {
            end == "port" && offset == PORT_L1SS + 8 && value & 4 != 0
        })
        .unwrap();
    assert!(enables_back > 3);
    for &(_, offset, value) in &writes[4..enables_back] {
        if offset == PORT_L1SS + 8 || offset == GPU_L1SS + 8 {
            assert_eq!(value & 5, 0, "{offset:#x} {value:#x}");
        }
    }
    // Enables back, the port's first, then ASPM back, the port's first.
    assert_eq!(writes[enables_back + 1].0, "gpu");
    assert_eq!(writes[enables_back + 1].1, GPU_L1SS + 8);
    assert_eq!(
        &writes[enables_back + 2..],
        &[link("port", 0x0142), link("gpu", 0x0142)]
    );
}

#[test]
fn without_ltr_from_the_root_port_aspm_l1_2_is_turned_off_and_l1_1_kept() {
    let log = Log::default();
    let (mut port, mut gpu) = (root_port(&log), gpu(&log));
    let configured = configure(&mut port, &mut gpu, false).unwrap();
    let Configured::Set { before, after, .. } = configured else {
        panic!("{configured:?}");
    };
    assert_eq!((before, after), (0xc, 0x8));
    assert_eq!(port.word(PORT_L1SS + 8) & 0xf, 0x8);
    assert_eq!(gpu.word(GPU_L1SS + 8) & 0xf, 0x8);
    assert_eq!(gpu.read16(GPU_LINK).unwrap() & 3, 2);
    // Substates off at the GPU before the port, on at the port before the
    // GPU, all while ASPM is off at both ends.
    let writes = log.borrow().clone();
    let substates: Vec<_> = writes
        .iter()
        .filter(|&&(_, offset, _)| offset == PORT_L1SS + 8 || offset == GPU_L1SS + 8)
        .rev()
        .take(4)
        .map(|&(end, _, value)| (end, value & 0xf))
        .collect();
    assert_eq!(
        substates,
        [("gpu", 0x8), ("port", 0x8), ("port", 0), ("gpu", 0)]
    );
    let last_aspm_off = writes
        .iter()
        .rposition(|&(_, offset, value)| {
            (offset == PORT_LINK || offset == GPU_LINK) && value & 3 == 0
        })
        .unwrap();
    let first_aspm_on = writes
        .iter()
        .position(|&(_, offset, value)| {
            (offset == PORT_LINK || offset == GPU_LINK) && value & 3 != 0
        })
        .unwrap();
    assert!(last_aspm_off < first_aspm_on);
    assert!(
        alloc::format!("{configured}").contains("LTR does not reach the device"),
        "{configured}"
    );
}

#[test]
fn a_link_the_firmware_left_without_substates_is_not_touched() {
    let log = Log::default();
    let (mut port, mut gpu) = (root_port(&log), gpu(&log));
    port.words[usize::from(PORT_L1SS + 8) / 4] &= !0xf;
    gpu.words[usize::from(GPU_L1SS + 8) / 4] &= !0xf;
    assert_eq!(
        configure(&mut port, &mut gpu, true).unwrap(),
        Configured::FirmwareOff
    );
    assert!(log.borrow().is_empty());
}

#[test]
fn without_aspm_l1_at_both_ends_no_substate_stays_on() {
    let log = Log::default();
    let (mut port, mut gpu) = (root_port(&log), gpu(&log));
    gpu.words[usize::from(GPU_LINK) / 4] &= !2;
    let Configured::Set { after, l1, .. } = configure(&mut port, &mut gpu, true).unwrap() else {
        panic!("not set");
    };
    assert_eq!((after, l1), (0, false));
    assert_eq!(port.word(PORT_L1SS + 8) & 0xf, 0);
    assert_eq!(gpu.word(GPU_L1SS + 8) & 0xf, 0);
    // The port's lone L1 goes off with the substates, as Linux writes it.
    assert_eq!(port.read16(PORT_LINK).unwrap() & 3, 0);
}

#[test]
fn ltr_reaches_a_device_only_through_enabled_components_up_to_a_root_port() {
    let on = |root_port| Ltr {
        root_port,
        enabled: true,
    };
    assert!(ltr_path(&[on(false), on(true)]));
    assert!(ltr_path(&[on(false), on(false), on(false), on(true)]));
    assert!(!ltr_path(&[on(false), on(false)]));
    assert!(!ltr_path(&[Ltr::default(), on(true)]));
    assert!(!ltr_path(&[
        on(false),
        Ltr {
            root_port: true,
            enabled: false
        }
    ]));
    assert!(!ltr_path(&[]));
    let log = Log::default();
    assert_eq!(ltr(&gpu(&log)).unwrap(), Some(on(false)));
    assert_eq!(ltr(&root_port(&log)).unwrap(), Some(on(true)));
}
