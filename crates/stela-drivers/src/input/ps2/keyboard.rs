// SPDX-License-Identifier: GPL-2.0-only
//! Scan-set-2 framing, the set 1 an i8042 translates it to, and the Linux
//! atkbd set-2 keycode and unxlate tables.
//! Copyright (c) 1999–2002 Vojtech Pavlik. See NOTICE.
use super::{ErrorKind, Input, Port};
use alloc::vec::Vec;

const EXTENDED: u8 = 0xe0;
const PAUSE: u8 = 0xe1;
const BREAK: u8 = 0xf0;
const PAUSE_TAIL: [u8; 7] = [0x14, 0x77, 0xe1, 0xf0, 0x14, 0xf0, 0x77];
const BAT: u8 = 0xaa;
const ACK: u8 = 0xfa;
const NAK: u8 = 0xfe;
const ERR: u8 = 0xff;
const HANJA: u8 = 0xf1;
const HANGEUL: u8 = 0xf2;
/// Linux atkbd's `xl_table`: translated bytes that are a reply or a lone key
/// code as they arrive and also a key's release with bit 7 set.
const XL_TABLE: [u8; 6] = [BAT, ERR, ACK, NAK, HANJA, HANGEUL];
/// Linux `KEY_HANGEUL` and `KEY_HANJA`, keys with no release code.
const KEY_HANGEUL: u16 = 122;
const KEY_HANJA: u16 = 123;
/// Linux `ATKBD_KEY_NULL`: a code that is framing, as Print Screen's fake
/// shift, and gives no key.
const KEY_NULL: u16 = 255;

#[derive(Default)]
pub(super) struct Keyboard {
    /// The controller translates the keyboard's set 2 to set 1, as the
    /// firmware left it; the bytes are then decoded as Linux atkbd decodes
    /// a translated port.
    translated: bool,
    /// Translated: which [`XL_TABLE`] entries last arrived as a press, so
    /// that the same byte with bit 7 is that key's release.
    xl_bit: u8,
    /// Translated: the E0 (1) or E1 (2) bytes still to come in a sequence.
    emul: u8,
    extended: bool,
    released: bool,
    pause: Option<usize>,
    prefix_at: u64,
    held: [u64; 4],
    observed: Vec<u8>,
}
impl Keyboard {
    pub(super) fn begin_observation(&mut self) {
        self.observed.clear();
    }
    pub(super) fn drain_observed(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.observed)
    }
    /// Decode the bytes as set 1 from a translating controller, or as
    /// set 2. Any sequence in progress is dropped.
    pub(super) fn set_translated(&mut self, translated: bool) {
        self.translated = translated;
        self.xl_bit = 0;
        self.reset_prefix();
    }
    pub(super) fn reset_prefix(&mut self) {
        self.extended = false;
        self.released = false;
        self.pause = None;
        self.emul = 0;
    }
    pub(super) fn prefix(&self) -> bool {
        self.extended || self.released || self.pause.is_some() || self.emul != 0
    }
    fn key(&mut self, usage: u8, pressed: bool, events: &mut Vec<Input>) {
        let word = usize::from(usage / 64);
        let bit = 1 << (usage % 64);
        if (self.held[word] & bit != 0) == pressed {
            return;
        }
        if pressed {
            self.held[word] |= bit;
        } else {
            self.held[word] &= !bit;
        }
        events.push(Input::Key { usage, pressed });
    }
    pub(super) fn expire(&mut self, now: u64, timeout: u64) -> Result<(), ErrorKind> {
        if self.prefix() && now.wrapping_sub(self.prefix_at) >= timeout {
            self.reset_prefix();
            return Err(ErrorKind::PacketTimeout(Port::Keyboard));
        }
        Ok(())
    }
    pub(super) fn feed(
        &mut self,
        data: u8,
        now: u64,
        timeout: u64,
        events: &mut Vec<Input>,
        observe: bool,
    ) -> Result<(), ErrorKind> {
        // The actual owning Producer admission controls this byte tap only.
        // Guest parsing still runs after observation closes; no observer
        // buffer grows and no Linux Pause/repeat policy is manufactured.
        if observe {
            self.observed.push(data);
        }
        // A prefix that expired before this byte is dropped, and the byte
        // starts a sequence of its own.
        if self.prefix() && now.wrapping_sub(self.prefix_at) >= timeout {
            self.reset_prefix();
        }
        if self.translated {
            return self.feed_translated(data, now, events);
        }
        if let Some(position) = self.pause {
            if data != PAUSE_TAIL[position] {
                self.reset_prefix();
                return Err(ErrorKind::KeyboardSequence);
            }
            if position + 1 == PAUSE_TAIL.len() {
                self.reset_prefix();
                // Pause has no break sequence; publish the complete pulse.
                self.key(0x48, true, events);
                self.key(0x48, false, events);
            } else {
                self.pause = Some(position + 1);
            }
            return Ok(());
        }
        match data {
            EXTENDED if !self.extended && !self.released => {
                self.extended = true;
                self.prefix_at = now;
                return Ok(());
            }
            PAUSE if !self.prefix() => {
                self.pause = Some(0);
                self.prefix_at = now;
                return Ok(());
            }
            BREAK if !self.released => {
                if !self.prefix() {
                    self.prefix_at = now;
                }
                self.released = true;
                return Ok(());
            }
            0xaa => return Err(ErrorKind::DeviceReset(Port::Keyboard)),
            0 | 0xff | 0xfa | 0xfe | EXTENDED | PAUSE | BREAK => {
                self.reset_prefix();
                return Err(ErrorKind::KeyboardSequence);
            }
            _ => {}
        }
        let extended = self.extended;
        let released = self.released;
        self.reset_prefix();
        // Print Screen's fake shift is framing, not a real left shift.
        if extended && data == 0x12 {
            return Ok(());
        }
        let index = usize::from(data & 0x7f)
            | (usize::from(data & 0x80) << 1)
            | if extended { 0x80 } else { 0 };
        let code = SET2.get(index).copied().unwrap_or(0);
        let Some(usage) = super::super::virtio::usage(code) else {
            return Err(ErrorKind::UnsupportedScanCode(index as u16));
        };
        self.key(usage, !released, events);
        Ok(())
    }
    /// One byte from a translating controller, as Linux
    /// `atkbd_receive_byte` takes it on a translated port: bit 7 is the
    /// release, except for a byte of [`XL_TABLE`] whose press did not come
    /// last, which is a reply or a key without a release; E0 and E1 begin a
    /// sequence of one and two more bytes.
    fn feed_translated(
        &mut self,
        data: u8,
        now: u64,
        events: &mut Vec<Input>,
    ) -> Result<(), ErrorKind> {
        let mut code = data;
        if self.emul != 0 || self.need_xlate(data) {
            self.released = data & 0x80 != 0;
            code &= 0x7f;
        }
        if self.emul == 0 {
            self.calculate_xl_bit(data);
        }
        match code {
            BAT => {
                self.reset_prefix();
                return Err(ErrorKind::DeviceReset(Port::Keyboard));
            }
            EXTENDED | PAUSE => {
                self.emul = if code == EXTENDED { 1 } else { 2 };
                self.prefix_at = now;
                return Ok(());
            }
            BREAK => {
                self.released = true;
                self.prefix_at = now;
                return Ok(());
            }
            // A reply nothing waits for, and the keyboard's overrun code:
            // Linux drops them and keeps the sequence in progress.
            ACK | NAK | ERR => return Ok(()),
            _ => {}
        }
        let mut index = usize::from(code & 0x7f) | (usize::from(code & 0x80) << 1);
        if self.emul == 1 {
            index |= 0x80;
        }
        if self.emul != 0 {
            self.emul -= 1;
            if self.emul != 0 {
                return Ok(());
            }
        }
        let released = core::mem::take(&mut self.released);
        let keycode = match index {
            0..=0x7f => SET2[usize::from(UNXLATE[index])],
            0x80..=0xff => SET2[usize::from(UNXLATE[index & 0x7f]) | 0x80],
            _ if code == HANJA => KEY_HANJA,
            _ if code == HANGEUL => KEY_HANGEUL,
            _ => 0,
        };
        if keycode == KEY_NULL {
            return Ok(());
        }
        let Some(usage) = super::super::virtio::usage(keycode) else {
            return Err(ErrorKind::UnsupportedScanCode(index as u16));
        };
        self.key(usage, !released, events);
        // Hanja and Hangeul send no release; Linux releases them at once.
        if !released && matches!(keycode, KEY_HANJA | KEY_HANGEUL) {
            self.key(usage, false, events);
        }
        Ok(())
    }
    /// Linux `atkbd_need_xlate`: whether bit 7 of `code` is a release.
    fn need_xlate(&self, code: u8) -> bool {
        if code == EXTENDED || code == PAUSE {
            return false;
        }
        match XL_TABLE.iter().position(|entry| *entry == code) {
            Some(bit) => self.xl_bit & (1 << bit) != 0,
            None => true,
        }
    }
    /// Linux `atkbd_calculate_xl_bit`: a press of an [`XL_TABLE`] code's key
    /// marks that its release may follow; that release, or the reply,
    /// clears the mark.
    fn calculate_xl_bit(&mut self, code: u8) {
        if let Some(bit) = XL_TABLE.iter().position(|entry| (code ^ entry) & 0x7f == 0) {
            if code & 0x80 != 0 {
                self.xl_bit &= !(1 << bit);
            } else {
                self.xl_bit |= 1 << bit;
            }
        }
    }
    pub(super) fn release(&mut self, events: &mut Vec<Input>) {
        self.reset_prefix();
        for usage in 0..=u8::MAX {
            self.key(usage, false, events);
        }
    }
}

/// Linux `atkbd_unxlate_table`: the set-2 code an i8042 translated to each
/// set-1 code below 0x80.
const UNXLATE: [u8; 128] = [
    0, 118, 22, 30, 38, 37, 46, 54, 61, 62, 70, 69, 78, 85, 102, 13, 21, 29, 36, 45, 44, 53, 60,
    67, 68, 77, 84, 91, 90, 20, 28, 27, 35, 43, 52, 51, 59, 66, 75, 76, 82, 14, 18, 93, 26, 34, 33,
    42, 50, 49, 58, 65, 73, 74, 89, 124, 17, 41, 88, 5, 6, 4, 12, 3, 11, 2, 10, 1, 9, 119, 126,
    108, 117, 125, 123, 107, 115, 116, 121, 105, 114, 122, 112, 113, 127, 96, 97, 120, 7, 15, 23,
    31, 39, 47, 55, 63, 71, 79, 86, 94, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 87, 111, 19, 25, 57,
    81, 83, 92, 95, 98, 99, 100, 101, 103, 104, 106, 109, 110,
];

// The remaining zero-initialized upstream entries are handled by get().
const SET2: [u16; 261] = [
    0, 67, 65, 63, 61, 59, 60, 88, 183, 68, 66, 64, 62, 15, 41, 117, 184, 56, 42, 93, 29, 16, 2, 0,
    185, 0, 44, 31, 30, 17, 3, 0, 186, 46, 45, 32, 18, 5, 4, 95, 187, 57, 47, 33, 20, 19, 6, 183,
    188, 49, 48, 35, 34, 21, 7, 184, 189, 0, 50, 36, 22, 8, 9, 185, 190, 51, 37, 23, 24, 11, 10, 0,
    191, 52, 53, 38, 39, 25, 12, 0, 192, 89, 40, 0, 26, 13, 0, 193, 58, 54, 28, 27, 0, 43, 0, 194,
    0, 86, 91, 90, 92, 0, 14, 94, 0, 79, 124, 75, 71, 121, 0, 0, 82, 83, 80, 76, 77, 72, 1, 69, 87,
    78, 81, 74, 55, 73, 70, 99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 217, 100, 255, 0,
    97, 165, 0, 0, 156, 0, 0, 0, 0, 0, 0, 125, 173, 114, 0, 113, 0, 0, 0, 126, 128, 0, 0, 140, 0,
    0, 0, 127, 159, 0, 115, 0, 164, 0, 0, 116, 158, 0, 172, 166, 0, 0, 0, 142, 157, 0, 0, 0, 0, 0,
    0, 0, 155, 0, 98, 0, 0, 163, 0, 0, 226, 0, 0, 0, 0, 0, 0, 0, 0, 255, 96, 0, 0, 0, 143, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 107, 0, 105, 102, 0, 0, 112, 110, 111, 108, 112, 106, 103, 0, 119, 0, 118,
    109, 0, 99, 104, 119, 0, 0, 0, 0, 65, 99,
];
