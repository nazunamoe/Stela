//! The graphics memory the firmware reserves for an integrated Intel GPU
//! ("stolen memory"), found as Linux's x86 early quirks find it before any
//! driver runs (`arch/x86/kernel/early-quirks.c`, `intel_graphics_stolen`):
//! the size from the GMCH control register and the base from the GPU's
//! configuration, by the method that file's `intel_early_ids` names for the
//! device. i915, and xe on Tiger Lake to Raptor Lake, read the result as
//! `intel_graphics_stolen_res`; Stela's arch runs no early quirks, so the
//! boot computes it here from the same registers. The build reads
//! `intel_early_ids` (`build.rs`), so the device to method map is Linux's
//! own.

include!(concat!(env!("OUT_DIR"), "/intel_stolen.rs"));

/// A read of 32 bits of the GPU's configuration at a dword-aligned offset.
pub type Read<'a> = &'a mut dyn FnMut(u16) -> Option<u32>;

/// What a read of the registers found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stolen {
    pub base: u64,
    pub size: u64,
}

/// Why no stolen memory is reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Absent {
    /// Linux's early quirks name no method for the device, or one of
    /// graphics the boot hands no driver (before Skylake).
    NoMethod,
    /// A register could not be read.
    Unreadable(u16),
    /// The size or the base read as zero: none is reserved.
    None,
}

const MB: u64 = 1024 * 1024;

/// Configuration registers of the GPU, `include/drm/intel/i915_drm.h`.
const SNB_GMCH_CTRL: u16 = 0x50;
const INTEL_BSM: u16 = 0x5c;
const INTEL_GEN11_BSM_DW0: u16 = 0xc0;
const INTEL_GEN11_BSM_DW1: u16 = 0xc4;
const INTEL_BSM_MASK: u32 = 0xfff0_0000;

/// The stolen memory of the device whose early-quirk method is `method`
/// (the `*_early_ops` name without the suffix), read through `read`: the
/// two methods of the graphics the boot hands a driver, Skylake's (`gen9`)
/// and Ice Lake's (`gen11`, which Tiger Lake to Raptor Lake share). Both take the size from the GMCH control register
/// (`gen9_stolen_size`); the base is the 32-bit BSM register on Skylake
/// (`gen3_stolen_base`) and the 64-bit one from Ice Lake on
/// (`gen11_stolen_base`).
pub fn find(method: &str, read: Read) -> Result<Stolen, Absent> {
    let wide = match method {
        "gen9" => false,
        "gen11" => true,
        _ => return Err(Absent::NoMethod),
    };
    let mut dword = |offset: u16| read(offset).ok_or(Absent::Unreadable(offset));
    // GMS, bits 15:8 of the control word.
    let gms = u64::from((dword(SNB_GMCH_CTRL)? >> 8) & 0xff);
    let size = if gms < 0xf0 {
        gms * 32 * MB
    } else {
        (gms - 0xf0) * 4 * MB + 4 * MB
    };
    let base = if wide {
        u64::from(dword(INTEL_GEN11_BSM_DW0)? & INTEL_BSM_MASK)
            | u64::from(dword(INTEL_GEN11_BSM_DW1)?) << 32
    } else {
        u64::from(dword(INTEL_BSM)? & INTEL_BSM_MASK)
    };
    if size == 0 || base == 0 {
        return Err(Absent::None);
    }
    Ok(Stolen { base, size })
}

/// The early-quirk method Linux names for `device`, from `intel_early_ids`.
pub fn method(device: u16) -> Option<&'static str> {
    STOLEN
        .binary_search_by_key(&device, |(id, _)| *id)
        .ok()
        .map(|index| STOLEN[index].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    /// The GPU's configuration space, by byte.
    fn space(bytes: &[(u16, u8)]) -> impl FnMut(u16) -> Option<u32> {
        let map = bytes.iter().copied().collect::<BTreeMap<_, _>>();
        move |offset| {
            assert_eq!(offset & 3, 0);
            let byte = |at: u16| map.get(&(offset + at)).copied().unwrap_or(0);
            Some(u32::from_le_bytes([byte(0), byte(1), byte(2), byte(3)]))
        }
    }

    #[test]
    fn methods_are_linuxs_by_device() {
        assert!(STOLEN.windows(2).all(|pair| pair[0].0 < pair[1].0));
        for (device, expected) in [
            (0x1916, "gen9"),
            (0x5a85, "gen9"),
            (0x3185, "gen9"),
            (0x3e92, "gen9"),
            (0x9b41, "gen9"),
            (0x8a56, "gen11"),
            (0x4571, "gen11"),
            (0x4e71, "gen11"),
            (0x9a49, "gen11"),
            (0xa7a0, "gen11"),
        ] {
            assert_eq!(method(device), Some(expected), "{device:04x}");
        }
        // Meteor Lake and later find theirs through BAR 2.
        assert_eq!(method(0x7d55), None);
    }

    #[cfg(feature = "i915-port")]
    #[test]
    fn every_device_i915_is_handed_has_a_method_read_here() {
        for &(device, ..) in crate::i915::DEVICES {
            // A discrete GPU's stolen memory is in its own memory, and from
            // Meteor Lake on an integrated one's is behind BAR 2: i915 finds
            // both itself.
            let own = |platform: &crate::i915::Platform| {
                platform.discrete || platform.name == "METEORLAKE"
            };
            if crate::i915::handed(device).is_some_and(|platform| !own(&platform)) {
                assert!(
                    matches!(method(device), Some("gen9" | "gen11")),
                    "{device:04x}"
                );
            }
        }
    }

    #[test]
    fn skylake_takes_gms_in_32_mib_steps_and_the_bsm() {
        // GMS 2 (64 MiB) in bits 15:8, BSM 0x7b80_0001 masked to 1 MiB.
        let mut read = space(&[(0x51, 2), (0x5c, 0x01), (0x5e, 0x80), (0x5f, 0x7b)]);
        assert_eq!(
            find("gen9", &mut read),
            Ok(Stolen {
                base: 0x7b80_0000,
                size: 64 * MB
            })
        );
        // GMS 0xf1: 4 MiB steps from 4 MiB.
        let mut small = space(&[(0x51, 0xf1), (0x5f, 0x7b)]);
        assert_eq!(find("gen9", &mut small).unwrap().size, 8 * MB);
    }

    #[test]
    fn gen11_reads_a_64_bit_base() {
        let mut read = space(&[(0x51, 1), (0xc2, 0x40), (0xc3, 0x8f), (0xc4, 0x01)]);
        assert_eq!(
            find("gen11", &mut read),
            Ok(Stolen {
                base: 0x1_8f40_0000,
                size: 32 * MB
            })
        );
    }

    #[test]
    fn nothing_reserved_unread_or_of_another_method_is_said() {
        let mut read = space(&[]);
        assert_eq!(find("gen9", &mut read), Err(Absent::None));
        let mut none = |_: u16| None;
        assert_eq!(find("gen9", &mut none), Err(Absent::Unreadable(0x50)));
        // Broadwell's and Sandy Bridge's methods, whose graphics the boot
        // does not hand i915, and a name Linux does not have.
        for method in ["gen8", "gen6", "gen12"] {
            assert_eq!(find(method, &mut space(&[])), Err(Absent::NoMethod));
        }
    }
}
