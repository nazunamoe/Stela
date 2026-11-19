//! A function's MSI-X table as its owner keeps it for Linux (PCI Local Bus
//! 3.0 6.8.2; Linux's `drivers/pci/msi/msi.c`): the layout from the
//! capability's BIR and offset words, every vector masked until its line is
//! enabled, and each vector's message and mask kept so a reset that cleared
//! the table can be written back.
use super::{BARS, Region};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use stela_driver_base::bus::pci::{ConfigError, ConfigSpace};

/// The capability's identifier.
pub const CAPABILITY: u8 = 0x11;
const CONTROL: u16 = 2;
const TABLE: u16 = 4;
const PENDING: u16 = 8;
const ENABLE: u16 = 1 << 15;
const FUNCTION_MASK: u16 = 1 << 14;
const SIZE: u16 = 0x7ff;
const COMMAND: u16 = 4;
const INTX_DISABLE: u16 = 1 << 10;
/// Bytes of one table entry: address low, address high, data, vector control.
pub const ENTRY_BYTES: u64 = 16;
const VECTOR_MASK: u32 = 1;
/// A memory BAR in Linux's resource flags (`IORESOURCE_MEM`).
const IORESOURCE_MEM: u64 = 0x200;

/// Where the capability puts the table and its pending bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub capability: u16,
    pub entries: u16,
    pub table_bar: u8,
    pub table_offset: u32,
    pub pending_bar: u8,
    pub pending_offset: u32,
}

impl Layout {
    /// The capability at `capability`.
    pub fn read<C: ConfigSpace>(config: &C, capability: u16) -> Result<Self, ConfigError> {
        let control = config.read16(capability + CONTROL)?;
        let table = config.read32(capability + TABLE)?;
        let pending = config.read32(capability + PENDING)?;
        Ok(Self {
            capability,
            entries: (control & SIZE) + 1,
            table_bar: (table & 7) as u8,
            table_offset: table & !7,
            pending_bar: (pending & 7) as u8,
            pending_offset: pending & !7,
        })
    }

    fn span(regions: &[Region; BARS], bar: u8, offset: u32, bytes: u64) -> Option<u64> {
        let region = regions.get(usize::from(bar))?;
        let end = u64::from(offset).checked_add(bytes)?;
        (region.flags & IORESOURCE_MEM != 0 && region.start != 0 && end <= region.size)
            .then(|| region.start + u64::from(offset))
    }

    /// The table's bus address, when it and the pending bits lie inside
    /// memory BARs of `regions`; a capability pointing elsewhere has none.
    pub fn table_address(&self, regions: &[Region; BARS]) -> Option<u64> {
        let pending = u64::from(self.entries).div_ceil(64) * 8;
        Self::span(regions, self.pending_bar, self.pending_offset, pending)?;
        Self::span(
            regions,
            self.table_bar,
            self.table_offset,
            u64::from(self.entries) * ENTRY_BYTES,
        )
    }

    /// The table's length in bytes.
    pub fn table_bytes(&self) -> u64 {
        u64::from(self.entries) * ENTRY_BYTES
    }
}

/// The owner's mapping of the table, uncached, from entry 0.
///
/// # Safety
/// Offsets below the layout's table length reach that table's dwords for as
/// long as the value lives, with each write reaching the device in order.
pub unsafe trait TableAccess {
    fn read32(&self, offset: u64) -> u32;
    fn write32(&self, offset: u64, value: u32);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Config(ConfigError),
    /// A table write that did not read back.
    Readback {
        index: u16,
        word: u8,
    },
    /// A configuration word that did not read back, or read as all ones.
    Control {
        offset: u16,
    },
    Index,
    /// The function's MSI-X is off.
    Disabled,
}

impl From<ConfigError> for Error {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

#[derive(Clone, Copy, Default)]
struct Vector {
    message: Option<(u64, u32)>,
    open: bool,
}

/// One function's table, shared by its owner and its vectors' sources.
pub struct Table<T> {
    layout: Layout,
    access: T,
    vectors: RefCell<Vec<Vector>>,
    enabled: Cell<bool>,
}

impl<T: TableAccess> Table<T> {
    pub fn new(layout: Layout, access: T) -> Self {
        let mut vectors = Vec::new();
        vectors.resize(usize::from(layout.entries), Vector::default());
        Self {
            layout,
            access,
            vectors: RefCell::new(vectors),
            enabled: Cell::new(false),
        }
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn enabled(&self) -> bool {
        self.enabled.get()
    }

    fn write(&self, index: u16, word: u8, value: u32) -> Result<(), Error> {
        if index >= self.layout.entries {
            return Err(Error::Index);
        }
        let offset = u64::from(index) * ENTRY_BYTES + u64::from(word) * 4;
        self.access.write32(offset, value);
        // The read also flushes the posted write.
        if self.access.read32(offset) != value {
            return Err(Error::Readback { index, word });
        }
        Ok(())
    }

    fn control<C: ConfigSpace>(&self, config: &mut C, value: u16) -> Result<(), Error> {
        let offset = self.layout.capability + CONTROL;
        config.write16(offset, value)?;
        if config.read16(offset)? & (ENABLE | FUNCTION_MASK) != value & (ENABLE | FUNCTION_MASK) {
            return Err(Error::Control { offset });
        }
        Ok(())
    }

    /// Turn MSI-X on with every vector masked. While it is on already, as
    /// for a write-back after a reset, each vector's message and mask are
    /// written back as Linux last left them; turned on anew, no earlier
    /// message is kept. As Linux does, under Function Mask, since some
    /// functions decode the table only with MSI-X enabled, and with INTx off.
    pub fn enable<C: ConfigSpace>(&self, config: &mut C) -> Result<(), Error> {
        if !self.enabled.get() {
            self.vectors.borrow_mut().fill(Vector::default());
        }
        let command = config.read16(COMMAND)?;
        let control = config.read16(self.layout.capability + CONTROL)?;
        if command == u16::MAX || control == u16::MAX {
            return Err(Error::Control { offset: COMMAND });
        }
        config.write16(COMMAND, command | INTX_DISABLE)?;
        self.control(config, control | ENABLE | FUNCTION_MASK)?;
        let vectors = self.vectors.borrow();
        for (index, vector) in vectors.iter().enumerate() {
            let index = index as u16;
            self.write(index, 3, VECTOR_MASK)?;
            if let Some((address, data)) = vector.message {
                self.write(index, 0, address as u32)?;
                self.write(index, 1, (address >> 32) as u32)?;
                self.write(index, 2, data)?;
            }
        }
        for (index, vector) in vectors.iter().enumerate() {
            if vector.open && vector.message.is_some() {
                self.write(index as u16, 3, 0)?;
            }
        }
        self.control(config, (control | ENABLE) & !FUNCTION_MASK)?;
        self.enabled.set(true);
        Ok(())
    }

    /// Mask every vector and turn MSI-X off; the messages stay for the next
    /// enable. No interrupt in flight is drained by it.
    pub fn disable<C: ConfigSpace>(&self, config: &mut C) -> Result<(), Error> {
        self.enabled.set(false);
        for index in 0..self.layout.entries {
            self.write(index, 3, VECTOR_MASK)?;
        }
        let control = config.read16(self.layout.capability + CONTROL)?;
        if control == u16::MAX {
            return Err(Error::Control {
                offset: self.layout.capability + CONTROL,
            });
        }
        self.control(config, (control | FUNCTION_MASK) & !ENABLE)
    }

    /// Mask one vector.
    pub fn mask(&self, index: u16) -> Result<(), Error> {
        let mut vectors = self.vectors.borrow_mut();
        let vector = vectors.get_mut(usize::from(index)).ok_or(Error::Index)?;
        vector.open = false;
        self.write(index, 3, VECTOR_MASK)
    }

    /// Give one vector its message, masked.
    pub fn program(&self, index: u16, address: u64, data: u32) -> Result<(), Error> {
        self.mask(index)?;
        let mut vectors = self.vectors.borrow_mut();
        vectors[usize::from(index)].message = Some((address, data));
        self.write(index, 0, address as u32)?;
        self.write(index, 1, (address >> 32) as u32)?;
        self.write(index, 2, data)
    }

    /// Let one programmed vector signal, while MSI-X is on.
    pub fn unmask(&self, index: u16) -> Result<(), Error> {
        if !self.enabled.get() {
            return Err(Error::Disabled);
        }
        let mut vectors = self.vectors.borrow_mut();
        let vector = vectors.get_mut(usize::from(index)).ok_or(Error::Index)?;
        if vector.message.is_none() {
            return Err(Error::Disabled);
        }
        vector.open = true;
        self.write(index, 3, 0)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use stela_driver_base::bus::pci::ConfigFailure;

    struct Config([u8; 256]);
    impl Config {
        fn get<const N: usize>(&self, offset: u16) -> Result<[u8; N], ConfigError> {
            let at = usize::from(offset);
            self.0
                .get(at..at + N)
                .map(|bytes| bytes.try_into().unwrap())
                .ok_or(ConfigError::before_effect(ConfigFailure::Access {
                    offset,
                    width: N,
                }))
        }
        fn set(&mut self, offset: u16, bytes: &[u8]) -> Result<(), ConfigError> {
            let at = usize::from(offset);
            self.0[at..at + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
    }
    impl ConfigSpace for Config {
        fn read8(&self, offset: u16) -> Result<u8, ConfigError> {
            self.get(offset).map(u8::from_le_bytes)
        }
        fn read16(&self, offset: u16) -> Result<u16, ConfigError> {
            self.get(offset).map(u16::from_le_bytes)
        }
        fn read32(&self, offset: u16) -> Result<u32, ConfigError> {
            self.get(offset).map(u32::from_le_bytes)
        }
        fn write8(&mut self, offset: u16, value: u8) -> Result<(), ConfigError> {
            self.set(offset, &value.to_le_bytes())
        }
        fn write16(&mut self, offset: u16, value: u16) -> Result<(), ConfigError> {
            self.set(offset, &value.to_le_bytes())
        }
        fn write32(&mut self, offset: u16, value: u32) -> Result<(), ConfigError> {
            self.set(offset, &value.to_le_bytes())
        }
    }

    /// Table RAM, which a reset clears, and the writes it saw.
    #[derive(Default)]
    struct Ram {
        words: RefCell<[u32; 32]>,
        writes: Cell<usize>,
        stuck: Cell<Option<u64>>,
    }
    // SAFETY: plain memory standing for eight entries.
    unsafe impl TableAccess for &Ram {
        fn read32(&self, offset: u64) -> u32 {
            self.words.borrow()[offset as usize / 4]
        }
        fn write32(&self, offset: u64, value: u32) {
            self.writes.set(self.writes.get() + 1);
            if self.stuck.get() != Some(offset) {
                self.words.borrow_mut()[offset as usize / 4] = value;
            }
        }
    }

    const CAP: u16 = 0x60;

    fn config() -> Config {
        let mut config = Config([0; 256]);
        config.0[usize::from(CAP)] = CAPABILITY;
        // Eight entries; table in BAR 0 at 0x2000, pending bits in BAR 2.
        config.write16(CAP + CONTROL, 7).unwrap();
        config.write32(CAP + TABLE, 0x2000).unwrap();
        config.write32(CAP + PENDING, 0x3000 | 2).unwrap();
        config
    }

    fn word(ram: &Ram, index: usize, word: usize) -> u32 {
        ram.words.borrow()[index * 4 + word]
    }

    #[test]
    fn layout_comes_from_the_bir_and_offset_words() {
        let config = config();
        let layout = Layout::read(&config, CAP).unwrap();
        assert_eq!(
            layout,
            Layout {
                capability: CAP,
                entries: 8,
                table_bar: 0,
                table_offset: 0x2000,
                pending_bar: 2,
                pending_offset: 0x3000,
            }
        );
        assert_eq!(layout.table_bytes(), 128);
        let mut regions = [Region::default(); BARS];
        let memory = |start, size| Region {
            start,
            size,
            flags: IORESOURCE_MEM,
        };
        regions[0] = memory(0x8000_0000, 0x4000);
        // No BAR 2 yet: the pending bits lie nowhere, so no table either.
        assert_eq!(layout.table_address(&regions), None);
        regions[2] = memory(0x9000_0000, 0x3008);
        assert_eq!(layout.table_address(&regions), Some(0x8000_2000));
        // A table past its BAR's end, or in an I/O BAR.
        regions[0].size = 0x2040;
        assert_eq!(layout.table_address(&regions), None);
        regions[0] = Region {
            flags: 0x100,
            ..memory(0x8000_0000, 0x4000)
        };
        assert_eq!(layout.table_address(&regions), None);
    }

    #[test]
    fn vectors_stay_masked_until_unmasked_and_come_back_after_a_reset() {
        let mut config = config();
        let ram = Ram::default();
        let table = Table::new(Layout::read(&config, CAP).unwrap(), &ram);
        assert_eq!(table.unmask(0), Err(Error::Disabled));
        table.enable(&mut config).unwrap();
        let control = config.read16(CAP + CONTROL).unwrap();
        assert_eq!(control & (ENABLE | FUNCTION_MASK), ENABLE);
        assert_ne!(config.read16(COMMAND).unwrap() & INTX_DISABLE, 0);
        assert!((0..8).all(|index| word(&ram, index, 3) == VECTOR_MASK));
        // A vector without a message cannot open.
        assert_eq!(table.unmask(2), Err(Error::Disabled));
        table.program(2, 0xfee0_0000, 0x41).unwrap();
        table.program(5, 0x1_fee0_1000, 0x42).unwrap();
        assert_eq!(
            (word(&ram, 2, 0), word(&ram, 2, 2), word(&ram, 2, 3)),
            (0xfee0_0000, 0x41, VECTOR_MASK)
        );
        assert_eq!((word(&ram, 5, 0), word(&ram, 5, 1)), (0xfee0_1000, 1));
        table.unmask(2).unwrap();
        assert_eq!(word(&ram, 2, 3), 0);
        assert_eq!(word(&ram, 5, 3), VECTOR_MASK);
        assert_eq!(table.program(8, 0, 0), Err(Error::Index));

        // A reset clears the table and the capability; enable writes back
        // both messages, opening only the vector Linux had open.
        *ram.words.borrow_mut() = [0; 32];
        config = self::config();
        table.enable(&mut config).unwrap();
        assert_eq!(
            (word(&ram, 2, 0), word(&ram, 2, 2), word(&ram, 2, 3)),
            (0xfee0_0000, 0x41, 0)
        );
        assert_eq!((word(&ram, 5, 2), word(&ram, 5, 3)), (0x42, VECTOR_MASK));

        table.mask(2).unwrap();
        assert_eq!(word(&ram, 2, 3), VECTOR_MASK);
        table.disable(&mut config).unwrap();
        assert!(!table.enabled());
        let control = config.read16(CAP + CONTROL).unwrap();
        assert_eq!(control & (ENABLE | FUNCTION_MASK), FUNCTION_MASK);
        assert_eq!(table.unmask(5), Err(Error::Disabled));
        // Turned on anew, no message from before is written or opened.
        *ram.words.borrow_mut() = [0; 32];
        table.enable(&mut config).unwrap();
        assert_eq!((word(&ram, 5, 2), word(&ram, 5, 3)), (0, VECTOR_MASK));
        assert_eq!(table.unmask(5), Err(Error::Disabled));
    }

    #[test]
    fn a_write_that_does_not_stick_is_refused() {
        let mut config = config();
        let ram = Ram::default();
        let table = Table::new(Layout::read(&config, CAP).unwrap(), &ram);
        ram.stuck.set(Some(3 * ENTRY_BYTES + 12));
        assert_eq!(
            table.enable(&mut config),
            Err(Error::Readback { index: 3, word: 3 })
        );
        assert!(!table.enabled());
        ram.stuck.set(None);
        table.enable(&mut config).unwrap();
        ram.stuck.set(Some(4 * ENTRY_BYTES + 8));
        assert_eq!(
            table.program(4, 0xfee0_0000, 7),
            Err(Error::Readback { index: 4, word: 2 })
        );
    }
}
