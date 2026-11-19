//! The market names of NVIDIA, AMD and Intel display controllers, by PCI
//! IDs, for the boot screen and log (`stela_drivers::gpu::names`). They are
//! generated from pinned sources, never typed by hand:
//!
//! - the PCI ID database's `pci.ids` ([`PCI_IDS_VERSION`], GPL-2.0-or-later
//!   or BSD-3-Clause), a release checked by its size and SHA-256: NVIDIA and
//!   AMD devices, and Intel's where it names them;
//! - libdrm's `data/amdgpu.ids` (MIT), AMD's own table by device and
//!   revision, at the `third_party/libdrm` pin;
//! - Mesa's `include/pci_ids/iris_pci_ids.h` and `crocus_pci_ids.h` (MIT) at
//!   the `third_party/mesa` pin: every Intel display controller Mesa drives,
//!   and its name where `pci.ids` has none;
//! - amdgpu's own PCI ID table (`amdgpu_drv.c`, GPL-2.0 or MIT) at the
//!   `third_party/linux` pin, for the processors' GPUs it flags `AMD_IS_APU`
//!   that neither of the others names: AMD sells those as Radeon Graphics.
//!
//! Only display controllers are kept: `pci.ids` entries whose own name marks
//! another function (audio, USB and the like) are left out, and Intel IDs
//! are Mesa's. Only market names are kept: a `pci.ids` name that is the
//! chip's code again (`AD104 [AC AD104 20GB]`), an engineering sample or a
//! reserved ID names no product, and its device is listed without one, for
//! the test that walks every device the GPU stacks admit. A laptop's GeForce
//! RTX of the 30 series or later takes NVIDIA's name, `Laptop GPU`, where the
//! database says `Max-Q / Mobile`.
use super::fetch::pinned_file;
use crate::tools::root;
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, fmt::Write as _, fs, path::Path, process::Command};

/// The generated table, beside the lookup that reads it.
pub const GENERATED: &str = "crates/stela-driver-gpu/src/names/table.rs";

/// The `pci.ids` release the table is made from (its `Version:` line), as
/// the PCI ID Project publishes it (https://pci-ids.ucw.cz/, history at
/// https://github.com/pciutils/pciids) and hwdata installs it as
/// `/usr/share/hwdata/pci.ids`.
pub const PCI_IDS_VERSION: &str = "2026.02.12";
const PCI_IDS_BYTES: usize = 1_584_801;
const PCI_IDS_SHA256: &str = "d57b3524531fcc31701042f09f6dba44f85c951c7cbec5ba74a8724f85b59fd3";

const NVIDIA: u16 = 0x10de;
const AMD: u16 = 0x1002;
const INTEL: u16 = 0x8086;
/// A name that holds for every revision of its device.
const ANY: u16 = 0x100;

/// Words in a `pci.ids` device's own name that mark a function other than
/// a display controller.
const NOT_DISPLAY: &[&str] = &[
    "Audio",
    "USB",
    "UCSI",
    "SMBus",
    "Ethernet",
    "SATA",
    "IDE",
    "LPC",
    "Bridge",
    "Memory Controller",
];

/// Words of a `pci.ids` market name that mark a device sold under no name.
const NOT_MARKET: &[&str] = &["Engineering Sample", "Reserved"];

/// AMD's name for the graphics of a processor it gives no model.
const AMD_PROCESSOR_GRAPHICS: &str = "Radeon Graphics";

/// Writes [`GENERATED`] from `pci_ids`, the pinned release, and the
/// submodules' pinned files.
pub fn generate(pci_ids: &Path) -> Result<()> {
    let data = fs::read(pci_ids).with_context(|| pci_ids.display().to_string())?;
    ensure!(
        data.len() == PCI_IDS_BYTES && stela_formats::sha256(&data) == PCI_IDS_SHA256,
        "{} is not pci.ids {PCI_IDS_VERSION} ({PCI_IDS_BYTES} bytes, SHA-256 {PCI_IDS_SHA256})",
        pci_ids.display()
    );
    let pci_ids = String::from_utf8_lossy(&data);
    let (libdrm, amdgpu_ids) = pinned_file("libdrm", "data/amdgpu.ids")?;
    let (mesa, iris) = pinned_file("mesa", "include/pci_ids/iris_pci_ids.h")?;
    let (_, crocus) = pinned_file("mesa", "include/pci_ids/crocus_pci_ids.h")?;
    let (linux, amdgpu_drv) = pinned_file("linux", "drivers/gpu/drm/amd/amdgpu/amdgpu_drv.c")?;
    let sources = format!(
        "pci.ids {PCI_IDS_VERSION}, libdrm {} data/amdgpu.ids, Mesa {} include/pci_ids, \
         Linux {} amdgpu_drv.c",
        &libdrm[..12],
        &mesa[..12],
        &linux[..12]
    );
    let names = table(&pci_ids, &amdgpu_ids, &[&iris, &crocus], &amdgpu_drv)?;
    let unnamed = unnamed(&pci_ids, &names)?;
    let generated = root().join(GENERATED);
    fs::create_dir_all(generated.parent().context("no parent")?)?;
    fs::write(&generated, source(&sources, &names, &unnamed))?;
    let status = Command::new("rustfmt")
        .args(["--edition", "2024"])
        .arg(&generated)
        .status()?;
    ensure!(status.success(), "rustfmt failed on {GENERATED}");
    println!("wrote {GENERATED}: {} names from {sources}", names.len());
    Ok(())
}

/// Each name by (vendor, device, revision or [`ANY`]), without its maker.
fn table(
    pci_ids: &str,
    amdgpu_ids: &str,
    mesa: &[&str],
    amdgpu_drv: &str,
) -> Result<BTreeMap<(u16, u16, u16), String>> {
    let mut names = BTreeMap::new();
    let devices = pci_devices(pci_ids)?;
    let intel: BTreeMap<u16, String> = mesa
        .iter()
        .map(|text| mesa_names(text))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    ensure!(!intel.is_empty(), "Mesa's Intel tables hold no device");
    for ((vendor, device), name) in &devices {
        let Some(name) = display_name(name) else {
            continue;
        };
        match *vendor {
            NVIDIA | AMD => {
                names.insert((*vendor, *device, ANY), name);
            }
            INTEL if intel.contains_key(device) => {
                names.insert((INTEL, *device, ANY), name);
            }
            _ => {}
        }
    }
    for (device, name) in &intel {
        names
            .entry((INTEL, *device, ANY))
            .or_insert_with(|| plain(name));
    }
    let amd = amdgpu_names(amdgpu_ids)?;
    ensure!(!amd.is_empty(), "amdgpu.ids holds no device");
    // A device `pci.ids` does not name, at a revision `amdgpu.ids` does not
    // list: the names AMD gives its listed revisions, as AMD's own table
    // joins them ("Radeon 840M / 860M Graphics").
    let mut by_device: BTreeMap<u16, Vec<String>> = BTreeMap::new();
    for (&(device, revision), name) in &amd {
        let name = plain(name);
        names.insert((AMD, device, u16::from(revision)), name.clone());
        let listed = by_device.entry(device).or_default();
        if !listed.contains(&name) {
            listed.push(name);
        }
    }
    for (device, listed) in by_device {
        names
            .entry((AMD, device, ANY))
            .or_insert_with(|| joined(&listed));
    }
    let processors = amd_processors(amdgpu_drv)?;
    ensure!(
        !processors.is_empty(),
        "amdgpu's table flags no device AMD_IS_APU"
    );
    for device in processors {
        names
            .entry((AMD, device, ANY))
            .or_insert_with(|| AMD_PROCESSOR_GRAPHICS.to_owned());
    }
    Ok(names)
}

/// The NVIDIA display controllers `pci.ids` lists that `names` holds no name
/// for, each with the database's own words: chip codes, engineering samples
/// and reserved IDs.
fn unnamed(
    pci_ids: &str,
    names: &BTreeMap<(u16, u16, u16), String>,
) -> Result<BTreeMap<u16, String>> {
    Ok(pci_devices(pci_ids)?
        .into_iter()
        .filter(|((vendor, device), name)| {
            *vendor == NVIDIA
                && !NOT_DISPLAY
                    .iter()
                    .any(|word| name.split('[').next().unwrap_or(name).contains(word))
                && !names.contains_key(&(NVIDIA, *device, ANY))
        })
        .map(|((_, device), name)| (device, name))
        .collect())
}

/// `names`, one device's names in the order first listed, as one name: the
/// words they share before and after said once ("Radeon 780M / 760M
/// Graphics").
fn joined(names: &[String]) -> String {
    let words: Vec<Vec<&str>> = names.iter().map(|name| name.split(' ').collect()).collect();
    let Some(first) = words.first() else {
        return String::new();
    };
    let shortest = words.iter().map(Vec::len).min().unwrap_or(0);
    let before = (0..shortest)
        .take_while(|&index| words.iter().all(|name| name[index] == first[index]))
        .count();
    let after = (0..shortest)
        .take_while(|&index| {
            words
                .iter()
                .all(|name| name[name.len() - 1 - index] == first[first.len() - 1 - index])
        })
        .count();
    // Each name keeps a word of its own.
    let before = before.min(shortest.saturating_sub(1));
    let after = after.min(shortest.saturating_sub(1 + before));
    let own: Vec<String> = words
        .iter()
        .map(|name| name[before..name.len() - after].join(" "))
        .collect();
    [
        first[..before].join(" "),
        own.join(" / "),
        first[first.len() - after..].join(" "),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

/// The devices amdgpu's `pciidlist` flags `AMD_IS_APU`: built into the
/// processor.
fn amd_processors(amdgpu_drv: &str) -> Result<Vec<u16>> {
    let list = amdgpu_drv
        .split_once("struct pci_device_id pciidlist[] = {")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(list, _)| list)
        .context("amdgpu_drv.c has no pciidlist")?;
    let mut devices = Vec::new();
    for line in list.lines() {
        let fields: Vec<&str> = line
            .trim()
            .trim_start_matches('{')
            .split(',')
            .map(str::trim)
            .collect();
        if fields.len() < 7 || fields[0] != "0x1002" || !fields[6].contains("AMD_IS_APU") {
            continue;
        }
        let device = fields[1]
            .strip_prefix("0x")
            .and_then(|id| u16::from_str_radix(id, 16).ok())
            .with_context(|| format!("amdgpu_drv.c line {line:?}"))?;
        devices.push(device);
    }
    Ok(devices)
}

/// Every NVIDIA, AMD and Intel device line of `pci.ids`: its IDs and name.
fn pci_devices(text: &str) -> Result<BTreeMap<(u16, u16), String>> {
    let mut devices = BTreeMap::new();
    let mut vendor = None;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        // The device classes follow the vendors.
        if line.starts_with("C ") {
            break;
        }
        if line.starts_with("\t\t") {
            continue;
        }
        let hex = |field: &str| u16::from_str_radix(field, 16);
        if let Some(entry) = line.strip_prefix('\t') {
            let (Some(vendor), Some((device, name))) = (vendor, entry.split_once("  ")) else {
                continue;
            };
            if [NVIDIA, AMD, INTEL].contains(&vendor) {
                let device = hex(device).with_context(|| format!("pci.ids line {line:?}"))?;
                devices.insert((vendor, device), name.to_owned());
            }
            continue;
        }
        vendor = line.split_once("  ").and_then(|(id, _)| hex(id).ok());
    }
    ensure!(
        devices.keys().any(|&(vendor, _)| vendor == NVIDIA),
        "pci.ids holds no NVIDIA device"
    );
    Ok(devices)
}

/// The market name in a `pci.ids` device name, the part in brackets after
/// the chip (`GB203 [GeForce RTX 5080]`), when the device is a display
/// controller sold under it: not the chip's code again, an engineering
/// sample or a reserved ID ([`NOT_MARKET`]).
fn display_name(name: &str) -> Option<String> {
    let (chip, rest) = name.split_once('[')?;
    let (market, _) = rest.rsplit_once(']')?;
    if NOT_DISPLAY.iter().any(|word| chip.contains(word))
        || NOT_MARKET.iter().any(|word| market.contains(word))
    {
        return None;
    }
    // A code of letters and digits, as `AD104`; a family's name such as
    // `Vega` is also the product's.
    let code = chip.split_whitespace().next().unwrap_or_default();
    let is_code =
        code.chars().any(|c| c.is_ascii_digit()) && code.chars().any(|c| c.is_ascii_alphabetic());
    if is_code && market.split_whitespace().any(|word| word == code) {
        return None;
    }
    let market = laptop(plain(market));
    (!market.is_empty()).then_some(market)
}

/// A GeForce RTX of the 30 series or later that the database marks for
/// laptops (`GeForce RTX 4060 Max-Q / Mobile`) by the name NVIDIA sells it
/// under since that series: `GeForce RTX 4060 Laptop GPU`.
fn laptop(name: String) -> String {
    for mark in ["Max-Q / Mobile", "Mobile / Max-Q"] {
        let Some((model, rest)) = name.split_once(mark) else {
            continue;
        };
        let number = model
            .strip_prefix("GeForce RTX ")
            .and_then(|model| model.split_whitespace().next())
            .and_then(|number| number.parse::<u32>().ok());
        if number.is_some_and(|number| number >= 3000) {
            return format!("{model}Laptop GPU{rest}");
        }
    }
    name
}

/// `amdgpu.ids`' lines `device,\trevision,\tname`, both IDs in hex.
fn amdgpu_names(text: &str) -> Result<BTreeMap<(u16, u8), String>> {
    let mut names = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.splitn(3, ',').map(str::trim).collect();
        let [device, revision, name] = fields[..] else {
            // Comments and the format's version line.
            continue;
        };
        let (Ok(device), Ok(revision)) = (
            u16::from_str_radix(device, 16),
            u8::from_str_radix(revision, 16),
        ) else {
            continue;
        };
        names.insert((device, revision), name.to_owned());
    }
    Ok(names)
}

/// Mesa's `CHIPSET(0x56a0, dg2_g10, "DG2", "Intel(R) Arc(tm) A770 Graphics")`
/// lines: each device and its name.
fn mesa_names(text: &str) -> Result<Vec<(u16, String)>> {
    let mut names = Vec::new();
    for line in text.lines() {
        // A comment may follow the closing parenthesis.
        let Some((arguments, _)) = line
            .trim()
            .strip_prefix("CHIPSET(")
            .and_then(|line| line.rsplit_once(')'))
        else {
            continue;
        };
        let device = arguments
            .split(',')
            .next()
            .and_then(|id| id.trim().strip_prefix("0x"))
            .and_then(|id| u16::from_str_radix(id, 16).ok())
            .with_context(|| format!("Mesa line {line:?}"))?;
        // The last quoted argument; flags such as `FORCE_PROBE` may follow.
        let Some(name) = arguments.split('"').skip(1).step_by(2).last() else {
            anyhow::bail!("Mesa line {line:?} names no device");
        };
        // A placeholder Mesa replaces at run time names nothing.
        if !name.contains("XXX") {
            names.push((device, name.to_owned()));
        }
    }
    Ok(names)
}

/// `name` without trademark marks and its maker, which the lookup puts back
/// in one spelling.
fn plain(name: &str) -> String {
    let mut name = name.to_owned();
    for mark in ["(R)", "(r)", "(TM)", "(tm)", "®", "™"] {
        name = name.replace(mark, "");
    }
    // The maker is said once, before the name: not again inside it
    // ("Mobile Intel GM45 Express Chipset"); its brand as it spells it.
    name.split_whitespace()
        .filter(|word| !["NVIDIA", "AMD", "ATI", "Intel"].contains(word))
        .map(|word| match word {
            "Geforce" => "GeForce",
            word => word,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The generated Rust source of `names`, and of `unnamed` for the tests.
fn source(
    sources: &str,
    names: &BTreeMap<(u16, u16, u16), String>,
    unnamed: &BTreeMap<u16, String>,
) -> String {
    // Each distinct name once, in the order first used.
    let mut text = String::new();
    let mut offsets: BTreeMap<&str, usize> = BTreeMap::new();
    let mut lines = String::new();
    let mut entries = String::new();
    for ((vendor, device, revision), name) in names {
        let offset = *offsets.entry(name).or_insert_with(|| {
            let offset = text.len();
            text.push_str(name);
            text.push('\n');
            let _ = writeln!(lines, "    {:?},", format!("{name}\n"));
            offset
        });
        let _ = writeln!(
            entries,
            "    ({vendor:#06x}, {device:#06x}, {revision:#05x}, {offset}),"
        );
    }
    let mut without = String::new();
    for (device, name) in unnamed {
        let _ = writeln!(without, "    ({device:#06x}, {name:?}),");
    }
    format!(
        "//! Generated by `cargo xtask gpu names` from {sources}; do not edit.\n\
         //! `xtask/src/gpu/names.rs` says what is kept.\n\n\
         /// Where the names come from.\n\
         pub(super) const SOURCES: &str = {sources:?};\n\n\
         /// Every name, without its maker, each ended by a newline.\n\
         pub(super) const TEXT: &str = concat!(\n{lines});\n\n\
         /// Each name's PCI vendor, device, revision or `0x100` for any, and \
         where it starts in [`TEXT`], sorted.\n\
         pub(super) static ENTRIES: [(u16, u16, u16, u32); {}] = [\n{entries}];\n\n\
         /// The NVIDIA display controllers the database lists under no market name, with \
         its words.\n\
         #[cfg(test)]\n\
         pub(super) static UNNAMED: [(u16, &str); {}] = [\n{without}];\n",
        names.len(),
        unnamed.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PCI_IDS: &str = "\
# comment
10de  NVIDIA Corporation
\t2c02  GB203 [GeForce RTX 5080]
\t\t1043 89f2  some board
\t2c22  GB203 High Definition Audio Controller
\t2c23  GB203 USB Type-C [UCSI]
\t2200  GA102
\t222b  GA102 [GeForce RTX 3090 Engineering Sample]
\t2480  GA104 [Reserved Dev ID A]
\t2785  AD104 [AC AD104 20GB]
\t2786  AD104 [GeForce RTX 4070]
1002  Advanced Micro Devices, Inc. [AMD/ATI]
\t13c0  Granite Ridge [Radeon Graphics]
\t15e7  Barcelo
\t73bf  Navi 21
\t744c  Navi 31 [Radeon RX 7900 XT/7900 XTX/7900 GRE/7900M]
\tab30  Navi 31 HDMI/DP Audio [Radeon RX 7900]
8086  Intel Corporation
\t56a0  DG2 [Arc A770]
\ta780  Raptor Lake-S GT1 [UHD Graphics 770]
\t7d55  Meteor Lake-P [Intel Arc Graphics]
\t1602  Broadwell-U Integrated Graphics
\t2723  Wi-Fi 6 AX200 [Wi-Fi 6 AX200 160MHz]
C 00  Unclassified device
\t00  Non-VGA unclassified device
";

    const AMDGPU_IDS: &str = "\
# List of AMDGPU IDs
#
# Syntax:
# device_id,\trevision_id,\tproduct_name        <-- single tab after comma

1.0.0
744C,\tC8,\tAMD Radeon RX 7900 XTX
744C,\tCC,\tAMD Radeon RX 7900 XT
15BF,\tC1,\tAMD Radeon 780M Graphics
15BF,\tC2,\tAMD Radeon 760M Graphics
15BF,\tC3,\tAMD Radeon 780M Graphics
15BF,\tC5,\tAMD Radeon 740M Graphics
";

    const IRIS: &str = "\
CHIPSET(0x1602, bdw_gt1, \"BDW GT1\", \"Intel(R) HD Graphics\")
CHIPSET(0x56a0, dg2_g10, \"DG2\", \"Intel(R) Arc(tm) A770 Graphics\")
CHIPSET(0xa780, rpl, \"RPL-S\", \"Intel(R) Graphics\")
CHIPSET(0xd750, nvl_p, \"NVL-P\", \"Intel(R) Graphics\", FORCE_PROBE)
CHIPSET(0x7d55, mtl_h, \"MTL\", \"Intel(R) Arc(tm) Graphics\")
CHIPSET(0x22b1, chv, \"BSW\", \"Intel(R) HD Graphics XXX\") /* Overridden at run time */
";

    const AMDGPU_DRV: &str = "\
static const struct pci_device_id pciidlist[] = {
\t{0x1002, 0x15e7, PCI_ANY_ID, PCI_ANY_ID, 0, 0, CHIP_RENOIR|AMD_IS_APU},
\t{0x1002, 0x13c0, PCI_ANY_ID, PCI_ANY_ID, 0, 0, CHIP_IP_DISCOVERY|AMD_IS_APU},
\t{0x1002, 0x73bf, PCI_ANY_ID, PCI_ANY_ID, 0, 0, CHIP_SIENNA_CICHLID},
\t{0, 0, 0}
};
";

    #[test]
    fn names_are_market_names_of_display_controllers_only() {
        let names = table(PCI_IDS, AMDGPU_IDS, &[IRIS], AMDGPU_DRV).unwrap();
        let name = |vendor, device, revision| names.get(&(vendor, device, revision)).cloned();
        assert_eq!(
            name(NVIDIA, 0x2c02, ANY).as_deref(),
            Some("GeForce RTX 5080")
        );
        // Audio and USB functions, and the class section, are left out.
        assert_eq!(name(NVIDIA, 0x2c22, ANY), None);
        assert_eq!(name(NVIDIA, 0x2c23, ANY), None);
        assert_eq!(name(AMD, 0xab30, ANY), None);
        // AMD: its own table by revision, the database for the device.
        assert_eq!(
            name(AMD, 0x744c, 0xc8).as_deref(),
            Some("Radeon RX 7900 XTX")
        );
        assert_eq!(
            name(AMD, 0x744c, 0xcc).as_deref(),
            Some("Radeon RX 7900 XT")
        );
        assert_eq!(
            name(AMD, 0x744c, ANY).as_deref(),
            Some("Radeon RX 7900 XT/7900 XTX/7900 GRE/7900M")
        );
        assert_eq!(name(AMD, 0x13c0, ANY).as_deref(), Some("Radeon Graphics"));
        // Intel: Mesa's devices, the database's name first, the maker once.
        assert_eq!(name(INTEL, 0x56a0, ANY).as_deref(), Some("Arc A770"));
        assert_eq!(
            name(INTEL, 0xa780, ANY).as_deref(),
            Some("UHD Graphics 770")
        );
        assert_eq!(name(INTEL, 0x7d55, ANY).as_deref(), Some("Arc Graphics"));
        assert_eq!(name(INTEL, 0x1602, ANY).as_deref(), Some("HD Graphics"));
        assert_eq!(name(INTEL, 0xd750, ANY).as_deref(), Some("Graphics"));
        // An Intel device Mesa does not drive is no display controller here.
        assert_eq!(name(INTEL, 0x2723, ANY), None);
        // A processor's GPU amdgpu flags and nothing names is AMD's Radeon
        // Graphics; one the database names keeps its name, and a discrete
        // GPU nothing names has none.
        assert_eq!(name(AMD, 0x15e7, ANY).as_deref(), Some("Radeon Graphics"));
        assert_eq!(name(AMD, 0x73bf, ANY), None);
        // A device the database names only by its chip, or under the chip's
        // code, an engineering sample or a reserved ID, has no name, and the
        // tests' list holds it.
        assert_eq!(name(NVIDIA, 0x2785, ANY), None);
        assert_eq!(name(NVIDIA, 0x2480, ANY), None);
        assert_eq!(name(NVIDIA, 0x2200, ANY), None);
        let unnamed = unnamed(PCI_IDS, &names).unwrap();
        assert_eq!(
            unnamed.keys().copied().collect::<Vec<_>>(),
            [0x2200, 0x222b, 0x2480, 0x2785]
        );
        assert_eq!(unnamed[&0x2785], "AD104 [AC AD104 20GB]");
        assert_eq!(names.len(), 17);
    }

    #[test]
    fn a_device_amds_table_names_by_revision_has_their_names_for_any_other() {
        let names = table(PCI_IDS, AMDGPU_IDS, &[IRIS], AMDGPU_DRV).unwrap();
        // The database names Navi 31 itself; Phoenix's revisions are named
        // only by AMD's table.
        assert_eq!(
            names.get(&(AMD, 0x15bf, ANY)).map(String::as_str),
            Some("Radeon 780M / 760M / 740M Graphics")
        );
        assert_eq!(
            names.get(&(AMD, 0x15bf, 0xc1)).map(String::as_str),
            Some("Radeon 780M Graphics")
        );
        let join = |names: &[&str]| {
            joined(
                &names
                    .iter()
                    .map(|&name| name.to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(join(&["Radeon 610M"]), "Radeon 610M");
        assert_eq!(
            join(&["Radeon RX 9060 XT", "Radeon RX 9060 XT LP"]),
            "Radeon RX 9060 XT / XT LP"
        );
        assert_eq!(
            join(&["Radeon RX 9070 XT", "Radeon RX 9070 GRE", "Radeon RX 9070"]),
            "Radeon RX 9070 XT / 9070 GRE / 9070"
        );
        assert_eq!(
            join(&["Radeon Pro W7600", "Radeon RX 7600 XT"]),
            "Radeon Pro W7600 / RX 7600 XT"
        );
    }

    #[test]
    fn the_source_holds_each_name_once_at_its_offset() {
        let names = table(PCI_IDS, AMDGPU_IDS, &[IRIS], AMDGPU_DRV).unwrap();
        let unnamed = unnamed(PCI_IDS, &names).unwrap();
        let source = source("test", &names, &unnamed);
        assert!(source.contains("pub(super) static ENTRIES: [(u16, u16, u16, u32); 17]"));
        assert!(source.contains("    (0x2785, \"AD104 [AC AD104 20GB]\"),"));
        assert!(source.contains("    (0x10de, 0x2c02, 0x100, "));
        assert!(source.contains("    (0x1002, 0x744c, 0x0c8, "));
        assert_eq!(source.matches("\"Radeon Graphics\\n\"").count(), 1);
    }

    #[test]
    fn marks_and_makers_are_taken_out() {
        assert_eq!(
            plain("Intel(R) Arc(tm) A770  Graphics"),
            "Arc A770 Graphics"
        );
        assert_eq!(plain("AMD Radeon 860M Graphics"), "Radeon 860M Graphics");
        assert_eq!(plain("NVIDIA RTX A6000"), "RTX A6000");
        assert_eq!(
            plain("Mobile Intel(R) GM45 Express Chipset"),
            "Mobile GM45 Express Chipset"
        );
        assert_eq!(
            display_name("Vega 10 XL/XT [Radeon RX Vega 56/64]").as_deref(),
            Some("Radeon RX Vega 56/64")
        );
        assert_eq!(
            display_name("GP100GL [Quadro GP100]").as_deref(),
            Some("Quadro GP100")
        );
        assert_eq!(plain("Geforce RTX 3050"), "GeForce RTX 3050");
        // Laptops' GeForce RTX by NVIDIA's name from the 30 series on.
        assert_eq!(
            display_name("AD107M [GeForce RTX 4060 Max-Q / Mobile]").as_deref(),
            Some("GeForce RTX 4060 Laptop GPU")
        );
        assert_eq!(
            display_name("GA104M [GeForce RTX 3080 Mobile / Max-Q 8GB/16GB]").as_deref(),
            Some("GeForce RTX 3080 Laptop GPU 8GB/16GB")
        );
        assert_eq!(
            display_name("TU106M [GeForce RTX 2070 Mobile / Max-Q]").as_deref(),
            Some("GeForce RTX 2070 Mobile / Max-Q")
        );
        assert_eq!(
            display_name("GA102GL [RTX A6000]").as_deref(),
            Some("RTX A6000")
        );
        assert_eq!(display_name("TU104"), None);
    }
}
