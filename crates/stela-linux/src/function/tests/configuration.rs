//! Exact callback widths, successful all-ones and retained typed PCI failures.
use super::*;
use core::cell::{Cell, RefCell};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Access {
    write: bool,
    offset: u16,
    width: usize,
}
pub(super) struct FaultConfig {
    bytes: Config,
    accesses: RefCell<Vec<Access>>,
    writes: Cell<usize>,
    read_error: Option<ConfigError>,
    pub(super) write_error: Option<(usize, ConfigError)>,
}
impl FaultConfig {
    pub(super) fn new(byte: u8) -> Self {
        Self {
            bytes: Config([byte; 256]),
            accesses: RefCell::new(Vec::new()),
            writes: Cell::new(0),
            read_error: None,
            write_error: None,
        }
    }
    fn access(&self, write: bool, offset: u16, width: usize) -> Result<(), ConfigError> {
        self.accesses.borrow_mut().push(Access {
            write,
            offset,
            width,
        });
        if write {
            let index = self.writes.get() + 1;
            self.writes.set(index);
            if let Some((at, error)) = self.write_error
                && at == index
            {
                return Err(error);
            }
        } else if let Some(error) = self.read_error {
            return Err(error);
        }
        Ok(())
    }
}
macro_rules! read {
    ($name:ident, $ty:ty, $width:literal) => {
        fn $name(&self, offset: u16) -> Result<$ty, ConfigError> {
            self.access(false, offset, $width)?;
            self.bytes.$name(offset)
        }
    };
}
macro_rules! write {
    ($name:ident, $ty:ty, $width:literal) => {
        fn $name(&mut self, offset: u16, value: $ty) -> Result<(), ConfigError> {
            self.access(true, offset, $width)?;
            self.bytes.$name(offset, value)
        }
    };
}
impl ConfigSpace for FaultConfig {
    read!(read8, u8, 1);
    read!(read16, u16, 2);
    read!(read32, u32, 4);
    write!(write8, u8, 1);
    write!(write16, u16, 2);
    write!(write32, u32, 4);
}
pub(super) struct TestHost {
    pub(super) config: FaultConfig,
    pub(super) msi: Result<(), MsiError>,
    pub(super) drops: Arc<AtomicUsize>,
}
impl Drop for TestHost {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}
impl Host for TestHost {
    type Config = FaultConfig;
    fn config(&mut self) -> &mut FaultConfig {
        &mut self.config
    }
    fn enable_msi(&mut self) -> Result<(), MsiError> {
        self.msi
    }
    fn disable_msi(&mut self) -> Result<(), MsiError> {
        self.msi
    }
}
fn state(config: FaultConfig) -> State<TestHost> {
    State {
        host: TestHost {
            config,
            msi: Ok(()),
            drops: Arc::new(AtomicUsize::new(0)),
        },
        mappings: mappings(&Arc::new(Identity::default())),
        enrollment: None,
        configuration: ConfigurationStatus::default(),
    }
}
#[test]
fn callbacks_keep_all_ones_success_and_exact_width() {
    let mut state = state(FaultConfig::new(0xff));
    let context = (&raw mut state).cast();
    for (width, expected) in [(1, 0xff), (2, 0xffff), (4, u32::MAX)] {
        let mut out = 0;
        assert_eq!(
            // SAFETY: context points to this live, uniquely accessed State<TestHost>;
            // out is a distinct aligned writable u32. Model callbacks are sequential.
            unsafe { read_config::<TestHost>(context, 0, width, &raw mut out) },
            0
        );
        assert_eq!(out, expected);
        assert_eq!(
            // SAFETY: the same live State<TestHost> is uniquely accessed by this
            // sequential model callback; no state reference overlaps the call.
            unsafe { write_config::<TestHost>(context, 8, width, expected) },
            0
        );
    }
    assert_eq!(
        state.host.config.accesses.borrow().as_slice(),
        &[
            Access {
                write: false,
                offset: 0,
                width: 1
            },
            Access {
                write: true,
                offset: 8,
                width: 1
            },
            Access {
                write: false,
                offset: 0,
                width: 2
            },
            Access {
                write: true,
                offset: 8,
                width: 2
            },
            Access {
                write: false,
                offset: 0,
                width: 4
            },
            Access {
                write: true,
                offset: 8,
                width: 4
            },
        ]
    );
    assert_eq!(state.configuration, ConfigurationStatus::default());
}
#[test]
fn malformed_c_offsets_refuse_before_touching_transport() {
    let mut state = state(FaultConfig::new(0));
    let context = (&raw mut state).cast();
    for (offset, width) in [(-1, 1), (2, 4), (4094, 4), (i32::MAX, 4), (0, 3), (0, 0)] {
        let mut out = 0x1122_3344;
        assert_eq!(
            // SAFETY: context is the live, uniquely accessed State<TestHost> and
            // out is a distinct writable u32; the callback validates numeric inputs.
            unsafe { read_config::<TestHost>(context, offset, width, &raw mut out) },
            PCIBIOS_BAD_REGISTER_NUMBER
        );
        assert_eq!(out, 0x1122_3344);
        assert_eq!(
            // SAFETY: this sequential callback uniquely accesses the same live
            // State<TestHost>; malformed numeric inputs are checked by the callback.
            unsafe { write_config::<TestHost>(context, offset, width, 0) },
            PCIBIOS_BAD_REGISTER_NUMBER
        );
    }
    assert!(state.host.config.accesses.borrow().is_empty());
    assert_eq!(state.configuration, ConfigurationStatus::default());
}
#[test]
fn pcibios_refusal_keeps_exact_rust_cause_and_unchanged_output() {
    for (reason, expected) in [
        (
            ConfigFailure::Access {
                offset: 0,
                width: 4,
            },
            PCIBIOS_BAD_REGISTER_NUMBER,
        ),
        (ConfigFailure::ForeignGrant, PCIBIOS_SET_FAILED),
        (ConfigFailure::RoutingSealed, PCIBIOS_SET_FAILED),
    ] {
        let error = ConfigError::before_effect(reason);
        let mut config = FaultConfig::new(0);
        config.read_error = Some(error);
        let mut state = state(config);
        let context = (&raw mut state).cast();
        let mut out = 0x55aa_3311;
        assert_eq!(
            // SAFETY: context points to this live, uniquely accessed State<TestHost>;
            // out is a distinct writable u32 and no callback overlaps this call.
            unsafe { read_config::<TestHost>(context, 0, 4, &raw mut out) },
            expected
        );
        assert_eq!(out, 0x55aa_3311);
        assert_eq!(state.configuration.first, Some(error));
        assert_eq!(state.configuration.uncertain, None);
    }
}
#[test]
fn msi_stops_at_first_failed_access_without_rollback_or_later_writes() {
    let error = ConfigError::before_effect(ConfigFailure::ForeignGrant);
    let mut config = FaultConfig::new(0);
    config.bytes.write16(0x62, Msi::WIDE | Msi::ENABLE).unwrap();
    config.write_error = Some((2, error));
    assert_eq!(
        Msi::at(0x60).program(&mut config, 0xfee0_1000, 0x51),
        Err(error)
    );
    assert_eq!(
        config.accesses.borrow().as_slice(),
        &[
            Access {
                write: false,
                offset: 0x62,
                width: 2
            },
            Access {
                write: true,
                offset: 0x62,
                width: 2
            },
            Access {
                write: false,
                offset: 0x62,
                width: 2
            },
            Access {
                write: true,
                offset: 0x64,
                width: 4
            },
        ]
    );
    assert_eq!(config.bytes.read16(0x62).unwrap(), Msi::WIDE);
    assert_eq!(config.bytes.read32(0x64).unwrap(), 0);
    let mut invalid = FaultConfig::new(0);
    assert!(matches!(
        Msi::at(u16::MAX).disable(&mut invalid),
        Err(ConfigError {
            reason: ConfigFailure::Access { .. },
            effect: ConfigEffect::None
        })
    ));
    assert!(invalid.accesses.borrow().is_empty());
}
#[test]
fn host_msi_failure_keeps_typed_cause_before_c_errno_mapping() {
    let error = ConfigError::before_effect(ConfigFailure::BusMasterSealed);
    for (failure, expected) in [
        (MsiError::Config(error), EIO),
        (MsiError::Native(-16), -16),
        (MsiError::Native(0), EIO),
    ] {
        let mut state = state(FaultConfig::new(0));
        state.host.msi = Err(failure);
        assert_eq!(
            // SAFETY: the pointer targets this live State<TestHost>, exclusively
            // accessed for this callback; the host returns only its model result.
            unsafe { enable_msi::<TestHost>((&raw mut state).cast()) },
            expected
        );
        assert_eq!(state.configuration.msi, Some(failure));
        assert_eq!(
            state.configuration.first,
            if matches!(failure, MsiError::Config(_)) {
                Some(error)
            } else {
                None
            }
        );
    }
}
struct MsixHost {
    config: FaultConfig,
    grant: Result<(u32, u32), MsiError>,
    asked: Vec<u32>,
    disabled: usize,
}
impl Host for MsixHost {
    type Config = FaultConfig;
    fn config(&mut self) -> &mut FaultConfig {
        &mut self.config
    }
    fn enable_msi(&mut self) -> Result<(), MsiError> {
        Ok(())
    }
    fn disable_msi(&mut self) -> Result<(), MsiError> {
        Ok(())
    }
    fn routes_msix(&self) -> bool {
        true
    }
    fn enable_msix(&mut self, vectors: u32) -> Result<(u32, u32), MsiError> {
        self.asked.push(vectors);
        self.grant
    }
    fn disable_msix(&mut self) -> Result<(), MsiError> {
        self.disabled += 1;
        Ok(())
    }
}
fn msix_state(grant: Result<(u32, u32), MsiError>) -> State<MsixHost> {
    State {
        host: MsixHost {
            config: FaultConfig::new(0),
            grant,
            asked: Vec::new(),
            disabled: 0,
        },
        mappings: mappings(&Arc::new(Identity::default())),
        enrollment: None,
        configuration: ConfigurationStatus::default(),
    }
}
#[test]
fn msix_grants_return_count_and_first_line_or_typed_refusal() {
    let mut first = 0;
    let mut state = msix_state(Ok((40, 3)));
    assert_eq!(
        // SAFETY: live, exclusively accessed State<MsixHost>; first is a
        // distinct aligned writable word.
        unsafe { enable_msix::<MsixHost>((&raw mut state).cast(), 8, &raw mut first) },
        3
    );
    assert_eq!((first, state.host.asked.as_slice()), (40, &[8][..]));
    // Asked how many it could set up, the host answers with any count.
    assert_eq!(
        // SAFETY: as above.
        unsafe { enable_msix::<MsixHost>((&raw mut state).cast(), 0, &raw mut first) },
        3
    );
    // SAFETY: the same live State<MsixHost>, sequential model callback.
    unsafe { disable_msix::<MsixHost>((&raw mut state).cast()) };
    assert_eq!(state.host.disabled, 1);
    assert_eq!(state.configuration.msi, None);

    // A host that grants more lines than asked is refused.
    let mut state = msix_state(Ok((40, 9)));
    first = 7;
    assert_eq!(
        // SAFETY: as above.
        unsafe { enable_msix::<MsixHost>((&raw mut state).cast(), 8, &raw mut first) },
        EIO
    );
    assert_eq!(first, 7);

    for (failure, expected) in [(MsiError::Native(-28), -28), (MsiError::Native(1), EIO)] {
        let mut state = msix_state(Err(failure));
        assert_eq!(
            // SAFETY: as above.
            unsafe { enable_msix::<MsixHost>((&raw mut state).cast(), 4, &raw mut first) },
            expected
        );
        assert_eq!(state.configuration.msi, Some(failure));
    }
}
#[test]
fn hosts_refuse_msix_unless_they_route_it() {
    let host = TestHost {
        config: FaultConfig::new(0),
        msi: Ok(()),
        drops: Arc::new(AtomicUsize::new(0)),
    };
    assert!(!host.routes_msix());
    let mut host = host;
    assert_eq!(host.enable_msix(4), Err(MsiError::Native(EOPNOTSUPP)));
}
