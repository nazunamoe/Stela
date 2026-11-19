//! ANV as the Intel stacks' Vulkan driver: which Intel GPUs it opens, from
//! Mesa's own Intel device table and ANV's own version checks as the build
//! read them (`build/anv_tables.rs`).
//!
//! The Metal device runs on a Vulkan device, and ANV is the only Intel
//! Vulkan driver the image holds. So an Intel GPU ANV creates no physical
//! device for cannot serve the Metal device, and the Intel kernel drivers
//! are not handed it ([`opens`]): graphics before Skylake, which ANV leaves
//! to Mesa's older Vulkan driver, and any device Mesa's table leaves out or
//! lists only for its force probe. Such a GPU is reported as any GPU without
//! a driver is ([`crate::Adapter::unavailable_reason`]). `cargo xtask gpu anv-test` runs ANV on
//! every device handed to xe and i915 and checks each of the Metal device's
//! requirements on it.

include!(concat!(env!("OUT_DIR"), "/anv_devices.rs"));

/// The graphics version Mesa gives `device`, if ANV creates a physical
/// device for it.
pub fn version(device: u16) -> Option<u8> {
    OPENED
        .binary_search_by_key(&device, |(id, _)| *id)
        .ok()
        .map(|index| OPENED[index].1)
}

/// Whether ANV creates a physical device for `device`.
pub fn opens(device: u16) -> bool {
    version(device).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anv_opens_skylake_and_later_graphics_of_mesas_table() {
        assert!(OPENED.windows(2).all(|pair| pair[0].0 < pair[1].0));
        // ANV's own bounds: Gen9 to Xe3.
        assert_eq!(VERSIONS, 9..=30);
        assert!(OPENED.iter().all(|(_, version)| VERSIONS.contains(version)));
        // Skylake, Broxton, Gemini Lake, Kaby Lake, Ice Lake, Tiger Lake,
        // DG2, Meteor Lake, Lunar Lake, Battlemage and Panther Lake.
        for (device, expected) in [
            (0x1916, 9),
            (0x5a85, 9),
            (0x3185, 9),
            (0x5916, 9),
            (0x8a56, 11),
            (0x9a49, 12),
            (0x56a0, 12),
            (0x7d55, 12),
            (0x64a0, 20),
            (0xe20b, 20),
            (0xb080, 30),
        ] {
            assert_eq!(version(device), Some(expected), "{device:04x}");
        }
        // Broadwell and Cherry Trail are in Mesa's table at version 8, which
        // ANV leaves to hasvk; Haswell, Sandy Bridge, Ironlake and i830 are
        // in none ANV reads; Nova Lake is listed for its force probe only;
        // one Rocket Lake and one Ice Lake ID are not listed.
        for device in [
            0x1616, 0x22b0, 0x0416, 0x0126, 0x0046, 0x3577, 0xd750, 0x4c80, 0x8a70,
        ] {
            assert!(!opens(device), "{device:04x}");
        }
    }
}
