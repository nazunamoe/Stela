//! Every PCI device ID a GPU stack of this build admits has a market name:
//! the boot screen names the GPU that draws by it, never by a code name or
//! its IDs. The walk takes each driver's own table, so a device a new table
//! admits and the names' sources do not name fails here, and the fix goes
//! to the generator or its sources (`xtask/src/gpu/names.rs`), not to the
//! generated table.
//!
//! - NVIDIA: every display controller the PCI ID database lists in the
//!   device ranges whose firmware the loader's recipes name
//!   ([`nvidia_chip`]): nouveau admits every NVIDIA display function, and a
//!   chip without firmware is refused by name.
//! - AMD: every device amdgpu's table names by ID that the port admits
//!   ([`crate::amdgpu::admits`]), and every device AMD's own name table
//!   lists that amdgpu takes by class, each at every revision AMD's table
//!   lists and at one it does not.
//! - Intel: every device of xe's and i915's tables that the stacks are
//!   handed ([`crate::stack::GpuStack::select`]).
//!
//! The few IDs no source names, engineering samples and devices sold under
//! no name, are listed with the reason.
#[cfg(feature = "amdgpu-port")]
use super::ANY;
use super::{market_name, table};
use crate::Vendor;
use alloc::{format, string::String, vec::Vec};
use stela_loader::host_firmware::recipes::nvidia_chip;

/// Why `name`, a device's market name, is not one a user can read as its
/// maker sells it, if it is not: a chip's or platform's code name, its maker
/// twice, trademark marks, brackets or a company's name.
fn flaw(vendor: Vendor, name: &str) -> Option<&'static str> {
    let maker = match vendor {
        Vendor::Nvidia => "NVIDIA",
        Vendor::Amd => "AMD",
        Vendor::Intel => "Intel",
        Vendor::Other(_) => return Some("no maker"),
    };
    let Some(model) = name
        .strip_prefix(maker)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return Some("not led by its maker");
    };
    if model.trim().is_empty() || model.trim() != model {
        return Some("no model, or stray spaces");
    }
    let words: Vec<&str> = model
        .split(|c: char| c.is_whitespace() || c == '/' || c == ',')
        .filter(|word| !word.is_empty())
        .collect();
    if words
        .iter()
        .any(|word| ["NVIDIA", "AMD", "ATI", "Intel"].contains(word))
    {
        return Some("its maker twice");
    }
    for junk in [
        "(R)",
        "(TM)",
        "(tm)",
        "®",
        "™",
        "[",
        "]",
        "Corporation",
        "Inc.",
        "Engineering Sample",
        "Reserved",
    ] {
        if model.contains(junk) {
            return Some("a trademark mark, bracket, company or sample mark");
        }
    }
    if words.contains(&"Geforce") {
        return Some("its brand misspelled");
    }
    // NVIDIA sells laptops' GeForce RTX from the 30 series on as Laptop GPUs.
    let series = model
        .strip_prefix("GeForce RTX ")
        .and_then(|model| model.split_whitespace().next())
        .and_then(|number| number.parse::<u32>().ok());
    if series.is_some_and(|number| number >= 3000) && model.contains("Max-Q") {
        return Some("a laptop GPU not by NVIDIA's name");
    }
    // NVIDIA's chip codes (`TU104`, `GA102GL`, `nv170`); a model such as
    // `RTX A6000` or `H100` is none.
    let chip = |word: &str| {
        let lower = word.to_ascii_lowercase();
        ["tu", "ga", "ad", "gb", "gh", "gk", "gm", "gp", "gv", "nv"]
            .iter()
            .any(|prefix| {
                lower.strip_prefix(prefix).is_some_and(|rest| {
                    rest.len() >= 3 && rest[..3].bytes().all(|b| b.is_ascii_digit())
                })
            })
    };
    if words.iter().any(|word| chip(word)) {
        return Some("a chip's code name");
    }
    // Code names AMD and Intel give their chips and platforms, which they
    // sell under none of them.
    for code in [
        "Navi",
        "Raphael",
        "Rembrandt",
        "Phoenix",
        "Hawk Point",
        "HawkPoint",
        "Strix",
        "Krackan",
        "Granite Ridge",
        "Renoir",
        "Cezanne",
        "Lucienne",
        "Barcelo",
        "Picasso",
        "Raven",
        "Dragon Range",
        "Mendocino",
        "Van Gogh",
        "VanGogh",
        "Sienna",
        "Navy Flounder",
        "Cavefish",
        "Beige Goby",
        "Yellow Carp",
        "Skillfish",
        "Arcturus",
        "Aldebaran",
        "Polaris",
        "Oberon",
        "Lake",
        "Alchemist",
        "Battlemage",
        "Celestial",
    ] {
        if model.contains(code) {
            return Some("a code name");
        }
    }
    // Intel's graphics tiers and discrete chips (`GT2`, `DG2`).
    if words.iter().any(|word| {
        word.len() == 3
            && (word.starts_with("GT") || word.starts_with("DG"))
            && word.as_bytes()[2].is_ascii_digit()
    }) {
        return Some("a graphics tier or chip code");
    }
    None
}

/// The flaws of each walked device's name, by device and revision, other
/// than those `allowed` lists; how many were walked.
struct Walk {
    vendor: Vendor,
    walked: usize,
    unnamed: Vec<u16>,
    flawed: Vec<String>,
}

impl Walk {
    fn new(vendor: Vendor) -> Self {
        Self {
            vendor,
            walked: 0,
            unnamed: Vec::new(),
            flawed: Vec::new(),
        }
    }

    fn check(&mut self, device: u16, revision: u8) {
        self.walked += 1;
        match market_name(self.vendor, device, revision) {
            None => {
                if !self.unnamed.contains(&device) {
                    self.unnamed.push(device);
                }
            }
            Some(name) => {
                if let Some(flaw) = flaw(self.vendor, &name) {
                    self.flawed
                        .push(format!("{device:04x} rev {revision:02x} {name:?}: {flaw}"));
                }
            }
        }
    }

    /// Every device was named well, but those `unnamed` lists, which have no
    /// name in any source.
    fn assert(mut self, unnamed: &[u16]) -> usize {
        self.unnamed.sort_unstable();
        assert!(
            self.flawed.is_empty(),
            "{:?}: {:#?}",
            self.vendor,
            self.flawed
        );
        let unexpected: Vec<String> = self
            .unnamed
            .iter()
            .filter(|device| !unnamed.contains(device))
            .map(|device| format!("{device:04x}"))
            .collect();
        assert!(
            unexpected.is_empty(),
            "{:?} devices without a market name: {unexpected:?}",
            self.vendor
        );
        // A listed device that has a name now leaves the list.
        let named: Vec<String> = unnamed
            .iter()
            .filter(|device| !self.unnamed.contains(device))
            .map(|device| format!("{device:04x}"))
            .collect();
        assert!(named.is_empty(), "{:?} named now: {named:?}", self.vendor);
        self.walked
    }
}

/// The NVIDIA devices of the firmware's ranges the PCI ID database names by
/// their chip alone (`GA102`, `TU104GL`): no product of theirs is known.
const NVIDIA_CHIP_ONLY: &[u16] = &[
    0x1e3c, 0x1e3d, 0x1e3e, 0x1eab, 0x1eb9, 0x1ebe, 0x1f04, 0x1f2e, 0x1f81, 0x1fa1, 0x1fae, 0x1fbf,
    0x2183, 0x21ae, 0x21bf, 0x21c2, 0x21c3, 0x2200, 0x223f, 0x2483, 0x249f, 0x24a4, 0x24df, 0x2505,
    0x25a3, 0x25a4,
];

/// The NVIDIA devices the database marks as engineering samples, reserved
/// IDs or by the chip's code again (`AC AD104 20GB`, `GB6-256(N22W-ES-A1)`):
/// sold under no name.
const NVIDIA_SAMPLES: &[u16] = &[
    0x1e2d, 0x1e2e, 0x1eae, 0x222b, 0x222f, 0x2480, 0x24ac, 0x24ad, 0x24af, 0x24bf, 0x252f, 0x25af,
    0x2785, 0x2c2c, 0x2d2c,
];

#[test]
fn every_nvidia_gpu_the_firmware_covers_has_a_market_name() {
    let mut walk = Walk::new(Vendor::Nvidia);
    let named = table::ENTRIES
        .iter()
        .filter(|&&(vendor, ..)| vendor == 0x10de)
        .map(|&(_, device, ..)| device);
    let mut devices: Vec<u16> = named
        .chain(table::UNNAMED.iter().map(|&(device, _)| device))
        .filter(|&device| nvidia_chip(device).is_some())
        .collect();
    devices.sort_unstable();
    devices.dedup();
    for device in devices {
        walk.check(device, 0xa1);
    }
    // Each unnamed one is one the database lists without a product name.
    let mut unnamed: Vec<u16> = [NVIDIA_CHIP_ONLY, NVIDIA_SAMPLES].concat();
    unnamed.sort_unstable();
    for device in &unnamed {
        assert!(
            table::UNNAMED.iter().any(|&(listed, _)| listed == *device),
            "{device:04x}"
        );
    }
    // Turing to Blackwell: 333 devices, 41 of them sold under no name.
    assert_eq!((walk.assert(&unnamed), unnamed.len()), (333, 41));
}

/// The AMD devices amdgpu's table names by ID, of the generations the port
/// drives, that neither the PCI ID database nor AMD's own table names, nor
/// flags built into a processor: chips no product of which is known
/// (`Vega 12`, `Navi 22`) and IDs no source lists.
#[cfg(feature = "amdgpu-port")]
const AMD_UNNAMED: &[u16] = &[
    0x66a2, 0x66a4, 0x686a, 0x686d, 0x686e, 0x686f, 0x69a0, 0x69a1, 0x69a2, 0x69a3, 0x7318, 0x731a,
    0x73a0, 0x73a8, 0x73a9, 0x73ac, 0x73ad, 0x73c0, 0x73c1, 0x73c3, 0x73da, 0x73db, 0x73dc, 0x73dd,
    0x73de, 0x73e2, 0x73e8, 0x73e9, 0x73ea, 0x73eb, 0x73ec, 0x73ed, 0x7420,
];

#[cfg(feature = "amdgpu-port")]
#[test]
fn every_amd_gpu_the_port_admits_has_a_market_name_at_every_revision() {
    use crate::{Adapter, bus::pci::Address};
    let admitted = |device: u16| {
        let adapter = Adapter {
            address: Address {
                segment: 0,
                bus: 3,
                device: 0,
                function: 0,
            },
            vendor: Vendor::Amd,
            device,
            class: [3, 0, 0],
            revision: 0,
        };
        crate::amdgpu::admits(&adapter).is_ok()
    };
    // The revisions AMD's own table names a device at, and one it does not.
    let revisions = |device: u16| {
        let mut listed: Vec<u8> = table::ENTRIES
            .iter()
            .filter(|&&(vendor, id, revision, _)| {
                vendor == 0x1002 && id == device && revision != ANY
            })
            .map(|&(.., revision, _)| revision as u8)
            .collect();
        if let Some(other) = (0..=u8::MAX)
            .rev()
            .find(|revision| !listed.contains(revision))
        {
            listed.push(other);
        }
        listed
    };
    let mut by_id = Walk::new(Vendor::Amd);
    let mut ids = 0;
    for &(device, ..) in crate::amdgpu::DEVICES {
        if admitted(device) {
            ids += 1;
            for revision in revisions(device) {
                by_id.check(device, revision);
            }
        }
    }
    let by_id = by_id.assert(AMD_UNNAMED);
    // The devices amdgpu takes by class and identifies by IP discovery, as
    // AMD's own table lists them.
    let mut by_class = Walk::new(Vendor::Amd);
    let mut listed: Vec<u16> = table::ENTRIES
        .iter()
        .filter(|&&(vendor, _, revision, _)| vendor == 0x1002 && revision != ANY)
        .map(|&(_, device, ..)| device)
        .filter(|device| {
            crate::amdgpu::DEVICES
                .binary_search_by_key(device, |&(id, ..)| id)
                .is_err()
        })
        .filter(|&device| admitted(device))
        .collect();
    listed.dedup();
    for &device in &listed {
        for revision in revisions(device) {
            by_class.check(device, revision);
        }
    }
    let by_class = by_class.assert(&[]);
    // Vega to RDNA 2 and the APUs of their time by ID: 96 devices, 269 names
    // with their revisions, 33 of them sold under no name; RDNA 3 on and
    // later APUs by class: 44 devices, 285 names.
    assert_eq!(
        (ids, by_id, AMD_UNNAMED.len(), listed.len(), by_class),
        (96, 269, 33, 44, 285)
    );
}

#[cfg(any(feature = "xe-port", feature = "i915-port"))]
#[test]
fn every_intel_gpu_the_stacks_are_handed_has_a_market_name() {
    use crate::{Adapter, bus::pci::Address, stack::GpuStack};
    let mut devices: Vec<u16> = Vec::new();
    #[cfg(feature = "xe-port")]
    devices.extend(crate::xe::devices().map(|(device, _)| device));
    #[cfg(feature = "i915-port")]
    devices.extend(crate::i915::DEVICES.iter().map(|entry| entry.0));
    devices.sort_unstable();
    devices.dedup();
    let mut walk = Walk::new(Vendor::Intel);
    let mut generic = 0;
    for device in devices {
        let adapter = Adapter {
            address: Address {
                segment: 0,
                bus: 0,
                device: 2,
                function: 0,
            },
            vendor: Vendor::Intel,
            device,
            class: [3, 0, 0],
            revision: 0,
        };
        if GpuStack::select(&adapter).is_ok() {
            walk.check(device, 0);
            // Intel sells some processors' graphics as Intel Graphics alone.
            generic += usize::from(
                market_name(Vendor::Intel, device, 0).as_deref() == Some("Intel Graphics"),
            );
        }
    }
    // Skylake to Panther Lake: 256 devices, 47 of them sold as Intel
    // Graphics.
    assert_eq!((walk.assert(&[]), generic), (256, 47));
}

#[test]
fn a_flawed_name_is_told_apart_from_a_market_name() {
    for (vendor, name) in [
        (Vendor::Nvidia, "NVIDIA GeForce RTX 5080"),
        (Vendor::Nvidia, "NVIDIA RTX A6000"),
        (Vendor::Nvidia, "NVIDIA GeForce RTX 4060 Laptop GPU"),
        (Vendor::Nvidia, "NVIDIA GeForce RTX 2070 Mobile / Max-Q"),
        (Vendor::Amd, "AMD Radeon Vega 10 Graphics"),
        (Vendor::Amd, "AMD Radeon 780M / 760M / 740M Graphics"),
        (Vendor::Intel, "Intel Iris Xe Graphics"),
        (Vendor::Intel, "Intel Arc A770"),
    ] {
        assert_eq!(flaw(vendor, name), None, "{name}");
    }
    for (vendor, name) in [
        (Vendor::Nvidia, "NVIDIA AC AD104 20GB"),
        (Vendor::Nvidia, "NVIDIA NVIDIA GeForce RTX 5080"),
        (Vendor::Nvidia, "NVIDIA GeForce RTX 3090 Engineering Sample"),
        (Vendor::Nvidia, "NVIDIA "),
        (Vendor::Nvidia, "NVIDIA Geforce RTX 3050"),
        (Vendor::Nvidia, "NVIDIA GeForce RTX 4060 Max-Q / Mobile"),
        (Vendor::Nvidia, "GeForce RTX 5080"),
        (Vendor::Amd, "AMD Navi 31"),
        (Vendor::Amd, "AMD Raphael"),
        (Vendor::Intel, "Intel Tiger Lake GT2"),
        (Vendor::Intel, "Intel(R) Arc(TM) A770 Graphics"),
        (Vendor::Intel, "Intel Corporation DG2 [Arc A770]"),
    ] {
        assert!(flaw(vendor, name).is_some(), "{name}");
    }
}
