//! xe's and i915's firmware: the GuC images and display programs xe, i915
//! and the display code name for each platform of xe's table, the security
//! controller (GSC) images of the platforms whose media GT enters its C6
//! power state only once the GSC runs (Meteor Lake, Arrow Lake and Lunar
//! Lake), and the display programs i915 loads on the older platforms only
//! it drives. The media controller (HuC) images, and the GSC images of the
//! other platforms, are left out: Stela runs no media engine and no content
//! protection, and the boot turns those controllers off on Linux's command
//! line. Built only with the Intel port, so an image or stick without it
//! holds none.
use super::*;

/// The Intel GPU files and their notices: linux-firmware 20261019, whose
/// GuC releases are at or above what the pinned xe asks of each platform.
pub const XE_REVISION: &str = "a2dcb924f0897368b0b4f305cbf9808cdda90ce0";

const I915_LICENSE: Artifact = Artifact {
    source: LINUX_FIRMWARE,
    revision: XE_REVISION,
    member: "LICENSES/LICENSE.i915",
    length: 2080,
    sha256: "8542aeabf2761935122d693561e16766ce1bcc2b0d003204f9040b7d6d929f2e",
};

const XE_LICENSE: Artifact = Artifact {
    source: LINUX_FIRMWARE,
    revision: XE_REVISION,
    member: "LICENSES/LICENSE.xe",
    length: 2041,
    sha256: "26b3c946f1f378ff25bb3bb7c365d108a2bc0bf08619e5e08bc0e4f9aaa6ee1e",
};

const fn xe_file(
    member: &'static str,
    length: usize,
    sha256: &'static str,
    license: &'static Artifact,
) -> Recipe {
    Recipe {
        image: Artifact {
            source: LINUX_FIRMWARE,
            revision: XE_REVISION,
            member,
            length,
            sha256,
        },
        license,
        consumer: Consumer::Xe,
    }
}

/// A file of linux-firmware's `i915` directory, under its notice.
const fn xe_i915(member: &'static str, length: usize, sha256: &'static str) -> Recipe {
    xe_file(member, length, sha256, &I915_LICENSE)
}

/// A file of linux-firmware's `xe` directory, under its notice.
const fn xe_xe(member: &'static str, length: usize, sha256: &'static str) -> Recipe {
    xe_file(member, length, sha256, &XE_LICENSE)
}

/// What xe, i915 and the display code they share load on the platforms xe's
/// PCI ID table admits, Tiger Lake through Panther Lake: the GuC images of
/// `xe_uc_fw.c` and `intel_uc_fw.c` and the display microcontroller programs
/// of `intel_dmc.c`, named as the drivers name them.
pub const XE: &[Recipe] = &[
    xe_i915(
        "i915/tgl_guc_70.bin",
        335360,
        "2f1f57a1b23d186f2592318d1e07a1365968932841ccb3e7177c516ba006e2f6",
    ),
    xe_i915(
        "i915/adlp_guc_70.bin",
        347904,
        "b7a800cbdcdf687e7b1c81f3316d040ab75fa3e5c698ebc356f077af17462ef8",
    ),
    xe_i915(
        "i915/dg1_guc_70.bin",
        321472,
        "967e94b887024351aa363f882d3cbd0bf5fc4f5f47fd113faf8c19fb565c21a9",
    ),
    xe_i915(
        "i915/dg2_guc_70.bin",
        381760,
        "e6e3f8b4480ba976c89c491ac736d6ce41fa82e0cf5726d0763142da3fe8a63b",
    ),
    xe_i915(
        "i915/mtl_guc_70.bin",
        320320,
        "7794f0b6abe5fcd9c6f47035dafe2199f30a6e7d230bd5a53fbf8005a60e5911",
    ),
    xe_xe(
        "xe/lnl_guc_70.bin",
        348992,
        "0583117763d2721b1b3a96c84d610229c18e644dce3a624b4e0ccc32952f5eaa",
    ),
    xe_xe(
        "xe/bmg_guc_70.bin",
        385856,
        "de81c75f46a127c33cd59f604d800e9ffc7ed3495967ba0d8767cd6985ab398b",
    ),
    xe_xe(
        "xe/ptl_guc_70.bin",
        365376,
        "ac42861c62b69f19923d9a69d971fb849921585a01924e14c0e8ac679d7cde81",
    ),
    // The GSC images Meteor Lake (with Arrow Lake) and Lunar Lake need for
    // media C6: without one, the GSC's power well never sleeps and the media
    // GT never idles (`intel_engine_cs.c`, `xe_gsc_init`). From Battlemage
    // on the GSC is no longer needed for it, and Panther Lake's is left out.
    xe_i915(
        "i915/mtl_gsc_1.bin",
        1142784,
        "01e8e2bb0eae90e3b4471703bb04d1bd13adb43373480fb10be6f101b877e0f3",
    ),
    xe_xe(
        "xe/lnl_gsc_1.bin",
        1163264,
        "b9a28846de2df43e8464cb783f965efe3a20df54da26392576583a4e0f4bbef1",
    ),
    xe_i915(
        "i915/tgl_dmc_ver2_12.bin",
        19760,
        "3c013ef0ad96ba73aee8e5bd04a8e27cc9b1c6e9183b1a83ce124485f325afca",
    ),
    xe_i915(
        "i915/rkl_dmc_ver2_03.bin",
        18476,
        "139ce0eb873a13679a4685463cdacf6d247073ce286c8a5bca443ef1d17ee2ac",
    ),
    xe_i915(
        "i915/adls_dmc_ver2_01.bin",
        18704,
        "96e426ed7e80f0cc6644ada1928f4d02b163ff59b14b2339aa28a374d65a5b19",
    ),
    xe_i915(
        "i915/adlp_dmc.bin",
        79088,
        "3516de2e134ddcf3b319c75d2e437779fecbd58cbb77234bd6f297c544e92ccb",
    ),
    // The name the display code asks for when `adlp_dmc.bin` is missing.
    xe_i915(
        "i915/adlp_dmc_ver2_16.bin",
        77084,
        "2da482ea46a40e54c9ca3b54185959177f393eff98ece21acdac7eb6cacb0fcb",
    ),
    xe_i915(
        "i915/dg1_dmc_ver2_02.bin",
        16624,
        "89576b82489737ae1b4cd057f471363a75baeb7590d76a15e17895ad38e6c09d",
    ),
    xe_i915(
        "i915/dg2_dmc_ver2_08.bin",
        22540,
        "cac5204087bba70a81c53778846340e57a4e35e5959b7b42006969f6e5f45466",
    ),
    xe_i915(
        "i915/mtl_dmc.bin",
        52712,
        "1b3b826195d50fae6611d29e0eb348e641a3ada3e0436ee0b987124f47a0e0db",
    ),
    xe_i915(
        "i915/bmg_dmc.bin",
        45964,
        "76e3ec6ea3a53ce727e43b84f5ea14c55400a2d118dac356d4e12a3cfac06b4d",
    ),
    xe_i915(
        "i915/xe2lpd_dmc.bin",
        60448,
        "eb6f1283f714b9a8af3492802d1478e774e7795e09d75b56865ddaca3ae07abc",
    ),
    xe_i915(
        "i915/xe3lpd_dmc.bin",
        95624,
        "be1d24f6262aef0a4b8a69e739d2025df2ba7e924a1697b20f5ab59a7977fc71",
    ),
    xe_i915(
        "i915/xe3lpd_3002_dmc.bin",
        67772,
        "dbc2923b62a6fc9663b928719dd0504df6adba85a0f5b49e1482b7afbc4b36b7",
    ),
];

/// The display program no platform requires but xe's display code names:
/// the one it asks for when ADL-P's own is missing.
#[cfg(test)]
const XE_FALLBACK: &str = "i915/adlp_dmc_ver2_16.bin";

/// A platform's name, PCI device IDs and files: xe's GuC, the display
/// program (none for a GPU without a display engine), the GuC i915 loads
/// where it names another, and the GSC image where the media GT needs it
/// for its C6 power state.
type XePlatform = (&'static str, &'static [u16], [Option<&'static str>; 4]);

/// The platforms of xe's PCI ID table that have pinned files, each with the
/// device IDs Linux's `include/drm/intel/pciids.h` gives it and the files
/// the drivers name for it: xe's GuC, the display program, i915's GuC
/// where it differs, and the GSC image where the media GT's C6 needs it. i915 binds Tiger Lake to Meteor Lake, DG1 and DG2 first
/// and xe is its fallback, so a device holds both drivers' files. i915 runs
/// Tiger Lake and Rocket Lake without a GuC (its default `enable_guc`), and
/// loads the same GuC as xe elsewhere except on Alder Lake N, which its
/// table describes as Alder Lake P. A platform's later re-releases share its
/// files: Raptor Lake S with Alder Lake S, Raptor Lake P and U and Alder
/// Lake N with Alder Lake P (xe loads Tiger Lake's GuC on N), Arrow Lake
/// with Meteor Lake, Wildcat Lake with Panther Lake except for its display.
const XE_PLATFORMS: &[XePlatform] = &[
    (
        "Tiger Lake",
        &[
            0x9a60, 0x9a68, 0x9a70, 0x9a40, 0x9a49, 0x9a59, 0x9a78, 0x9ac0, 0x9ac9, 0x9ad9, 0x9af8,
        ],
        [
            Some("i915/tgl_guc_70.bin"),
            Some("i915/tgl_dmc_ver2_12.bin"),
            None,
            None,
        ],
    ),
    (
        "Rocket Lake",
        // Without 4c80, which Mesa's table leaves out: no Vulkan driver
        // opens it, so xe is not handed it.
        &[0x4c8a, 0x4c8b, 0x4c8c, 0x4c90, 0x4c9a],
        [
            Some("i915/tgl_guc_70.bin"),
            Some("i915/rkl_dmc_ver2_03.bin"),
            None,
            None,
        ],
    ),
    (
        "Alder Lake S or Raptor Lake S",
        &[
            0x4680, 0x4682, 0x4688, 0x468a, 0x468b, 0x4690, 0x4692, 0x4693, 0xa780, 0xa781, 0xa782,
            0xa783, 0xa788, 0xa789, 0xa78a, 0xa78b,
        ],
        [
            Some("i915/tgl_guc_70.bin"),
            Some("i915/adls_dmc_ver2_01.bin"),
            None,
            None,
        ],
    ),
    (
        "Alder Lake P or Raptor Lake P or U",
        &[
            0x46a0, 0x46a1, 0x46a2, 0x46a3, 0x46a6, 0x46a8, 0x46aa, 0x462a, 0x4626, 0x4628, 0x46b0,
            0x46b1, 0x46b2, 0x46b3, 0x46c0, 0x46c1, 0x46c2, 0x46c3, 0xa721, 0xa7a1, 0xa7a9, 0xa7ac,
            0xa7ad, 0xa720, 0xa7a0, 0xa7a8, 0xa7aa, 0xa7ab,
        ],
        [
            Some("i915/adlp_guc_70.bin"),
            Some("i915/adlp_dmc.bin"),
            None,
            None,
        ],
    ),
    (
        "Alder Lake N",
        &[0x46d0, 0x46d1, 0x46d2, 0x46d3, 0x46d4],
        [
            Some("i915/tgl_guc_70.bin"),
            Some("i915/adlp_dmc.bin"),
            Some("i915/adlp_guc_70.bin"),
            None,
        ],
    ),
    (
        "DG1",
        &[0x4905, 0x4906, 0x4907, 0x4908, 0x4909],
        [
            Some("i915/dg1_guc_70.bin"),
            Some("i915/dg1_dmc_ver2_02.bin"),
            None,
            None,
        ],
    ),
    (
        "Arc A-series (DG2)",
        &[
            0x56a0, 0x56a1, 0x56a2, 0x56be, 0x56bf, 0x5690, 0x5691, 0x5692, 0x56a5, 0x56a6, 0x56b0,
            0x56b1, 0x56ba, 0x56bb, 0x56bc, 0x56bd, 0x5693, 0x5694, 0x5695, 0x56a3, 0x56a4, 0x56b2,
            0x56b3, 0x5696, 0x5697,
        ],
        [
            Some("i915/dg2_guc_70.bin"),
            Some("i915/dg2_dmc_ver2_08.bin"),
            None,
            None,
        ],
    ),
    // DG2 without a display engine: no display program is asked for.
    (
        "Data Center GPU Flex (ATS-M)",
        &[0x56c0, 0x56c1, 0x56c2],
        [Some("i915/dg2_guc_70.bin"), None, None, None],
    ),
    (
        "Meteor Lake or Arrow Lake",
        &[
            0x7d40, 0x7d45, 0x7d55, 0x7d60, 0x7dd5, 0x7d51, 0x7dd1, 0x7d41, 0x7d67, 0xb640,
        ],
        [
            Some("i915/mtl_guc_70.bin"),
            Some("i915/mtl_dmc.bin"),
            None,
            Some("i915/mtl_gsc_1.bin"),
        ],
    ),
    (
        "Lunar Lake",
        &[0x6420, 0x64a0, 0x64b0],
        [
            Some("xe/lnl_guc_70.bin"),
            Some("i915/xe2lpd_dmc.bin"),
            None,
            Some("xe/lnl_gsc_1.bin"),
        ],
    ),
    (
        "Arc B-series (Battlemage)",
        &[
            0xe202, 0xe209, 0xe20b, 0xe20c, 0xe20d, 0xe210, 0xe211, 0xe212, 0xe216, 0xe220, 0xe221,
            0xe222, 0xe223,
        ],
        [
            Some("xe/bmg_guc_70.bin"),
            Some("i915/bmg_dmc.bin"),
            None,
            None,
        ],
    ),
    (
        "Panther Lake",
        &[
            0xb080, 0xb081, 0xb082, 0xb083, 0xb084, 0xb085, 0xb086, 0xb087, 0xb08f, 0xb090, 0xb0a0,
            0xb0b0,
        ],
        [
            Some("xe/ptl_guc_70.bin"),
            Some("i915/xe3lpd_dmc.bin"),
            None,
            None,
        ],
    ),
    (
        "Wildcat Lake",
        &[0xfd80, 0xfd81],
        [
            Some("xe/ptl_guc_70.bin"),
            Some("i915/xe3lpd_3002_dmc.bin"),
            None,
            None,
        ],
    ),
];

/// The binding xe's files take: Linux's names, as for nouveau's.
const XE_TARGET: Target<'static> = Target {
    driver: "xe",
    hardware: "intel",
    revision: "linux-firmware",
    board: None,
};

/// xe's request for a file, by the name the driver gives it.
pub(super) fn select_xe(request: &Request) -> Option<&'static Recipe> {
    if request.target.hardware != XE_TARGET.hardware
        || request.target.revision != XE_TARGET.revision
    {
        return None;
    }
    XE.iter().find(|recipe| recipe.image.member == request.name)
}

/// The platform of an Intel GPU with PCI device ID `device`, as people know
/// it, when xe's PCI ID table admits the device and its files are pinned.
/// Ice Lake and older are i915's and have no entry.
pub fn intel_gpu_platform(device: u16) -> Option<&'static str> {
    XE_PLATFORMS
        .iter()
        .find(|(_, devices, _)| devices.contains(&device))
        .map(|(name, ..)| *name)
}

/// The security controller (GSC) image the drivers load on the Intel GPU
/// with PCI device ID `device`, where its media GT enters its C6 power state
/// only once the GSC runs (Meteor Lake, Arrow Lake, Lunar Lake). The boot
/// turns the GSC off on every other platform.
pub fn intel_gsc(device: u16) -> Option<&'static str> {
    XE_PLATFORMS
        .iter()
        .find(|(_, devices, _)| devices.contains(&device))
        .and_then(|(_, _, files)| files[3])
}

/// Whether xe, i915 and the display code start without the file `request`
/// names when it is missing, on every platform that names it: a display
/// microcontroller program (`intel_dmc.c` runs the display without its
/// power states). A GuC image is always needed: `xe_uc_init` fails without
/// it, and i915 without one where it submits through the GuC runs no
/// command (its GT is wedged). i915's own files, the display programs of
/// the older platforms, are all optional.
pub(super) fn optional(request: &Request) -> bool {
    // i915's only files are display microcontroller programs.
    if request.target.driver == I915_TARGET.driver {
        return select_i915(request).is_some();
    }
    if select_xe(request).is_none() {
        return false;
    }
    let mut named = false;
    for (_, _, files) in XE_PLATFORMS {
        for (column, file) in files.iter().enumerate() {
            if *file != Some(request.name.as_str()) {
                continue;
            }
            named = true;
            if column != 1 {
                return false;
            }
        }
    }
    named
}

/// Requires the files xe, i915 and the display code ask for on the Intel GPU
/// with PCI device ID `device`, a device of xe's table: whichever of the two
/// drivers binds it finds its files.
pub fn require_xe(inventory: &mut Inventory, device: u16) -> Result<(), Error> {
    let (_, _, files) = XE_PLATFORMS
        .iter()
        .find(|(_, devices, _)| devices.contains(&device))
        .ok_or_else(|| Error(format!("{device:04x} is no GPU xe has pinned files for")))?;
    for name in files.iter().flatten() {
        inventory.observe(Requirement {
            name,
            target: XE_TARGET,
            minimum_bytes: 1,
        })?;
    }
    Ok(())
}

/// A display microcontroller program of linux-firmware's `i915` directory
/// that only i915 loads, under the directory's notice.
const fn i915_dmc(member: &'static str, length: usize, sha256: &'static str) -> Recipe {
    Recipe {
        image: Artifact {
            source: LINUX_FIRMWARE,
            revision: XE_REVISION,
            member,
            length,
            sha256,
        },
        license: &I915_LICENSE,
        consumer: Consumer::I915,
    }
}

/// What i915's display code loads on the platforms only i915 drives: the
/// display microcontroller programs `intel_dmc.c` names for Skylake to Ice
/// Lake. i915 loads no GuC or HuC there under its default `enable_guc`
/// (`intel_uc.c` uses them from Gen12 on).
pub const I915: &[Recipe] = &[
    i915_dmc(
        "i915/skl_dmc_ver1_27.bin",
        8928,
        "d3b6dc1a39bb2aeb37a1179f2b4e8145c24986da78c533571a5712dc91ec3f61",
    ),
    i915_dmc(
        "i915/bxt_dmc_ver1_07.bin",
        8380,
        "b4036e7698130c7a59637e543f6c614495c7076666104a2079d011f0923455f5",
    ),
    i915_dmc(
        "i915/kbl_dmc_ver1_04.bin",
        8840,
        "2cde41c3e5ad181423bcc3e98ff9c49f743c88f18646af4d0b3c3a9664b831a1",
    ),
    i915_dmc(
        "i915/glk_dmc_ver1_04.bin",
        8800,
        "412d3a3ed68908b983ff16cfe496e969c9fd31f94283e0872d7e5413c7bee504",
    ),
    i915_dmc(
        "i915/icl_dmc_ver1_09.bin",
        25952,
        "aafad1967679baa36e971e5331da73eea31c68239416432df83e71332755dc01",
    ),
];

/// The platforms only i915 drives, Skylake to Ice Lake and their Atom
/// lines, each by the name of its `enum intel_platform` entry without the
/// `INTEL_` prefix, with the PCI device IDs i915's table gives it and the
/// display microcontroller program `dmc_firmware_default` names for it. A
/// device xe's table also holds has its files in [`XE_PLATFORMS`], which
/// serve both drivers, and is not listed; neither is one no Vulkan driver
/// here opens: graphics before Skylake, and an ID
/// Mesa's table leaves out.
type I915Platform = (&'static str, &'static [u16], &'static str);
pub const I915_PLATFORMS: &[I915Platform] = &[
    (
        "SKYLAKE",
        &[
            0x1902, 0x1906, 0x190a, 0x190b, 0x190e, 0x1912, 0x1913, 0x1915, 0x1916, 0x1917, 0x191a,
            0x191b, 0x191d, 0x191e, 0x1921, 0x1923, 0x1926, 0x1927, 0x192a, 0x192b, 0x192d, 0x1932,
            0x193a, 0x193b, 0x193d,
        ],
        "i915/skl_dmc_ver1_27.bin",
    ),
    (
        "BROXTON",
        &[0x0a84, 0x1a84, 0x1a85, 0x5a84, 0x5a85],
        "i915/bxt_dmc_ver1_07.bin",
    ),
    ("GEMINILAKE", &[0x3184, 0x3185], "i915/glk_dmc_ver1_04.bin"),
    (
        "KABYLAKE",
        &[
            0x5902, 0x5906, 0x5908, 0x590a, 0x590b, 0x590e, 0x5912, 0x5913, 0x5915, 0x5916, 0x5917,
            0x591a, 0x591b, 0x591c, 0x591d, 0x591e, 0x5921, 0x5923, 0x5926, 0x5927, 0x593b, 0x87c0,
        ],
        "i915/kbl_dmc_ver1_04.bin",
    ),
    (
        "COFFEELAKE",
        &[
            0x3e90, 0x3e91, 0x3e92, 0x3e93, 0x3e94, 0x3e96, 0x3e98, 0x3e99, 0x3e9a, 0x3e9b, 0x3e9c,
            0x3ea0, 0x3ea1, 0x3ea2, 0x3ea3, 0x3ea4, 0x3ea5, 0x3ea6, 0x3ea7, 0x3ea8, 0x3ea9, 0x87ca,
        ],
        "i915/kbl_dmc_ver1_04.bin",
    ),
    (
        "COMETLAKE",
        &[
            0x9b21, 0x9b41, 0x9ba2, 0x9ba4, 0x9ba5, 0x9ba8, 0x9baa, 0x9bac, 0x9bc2, 0x9bc4, 0x9bc5,
            0x9bc6, 0x9bc8, 0x9bca, 0x9bcc, 0x9be6, 0x9bf6,
        ],
        "i915/kbl_dmc_ver1_04.bin",
    ),
    (
        "ICELAKE",
        &[
            0x8a50, 0x8a51, 0x8a52, 0x8a53, 0x8a54, 0x8a56, 0x8a57, 0x8a58, 0x8a59, 0x8a5a, 0x8a5b,
            0x8a5c, 0x8a5d, 0x8a71,
        ],
        "i915/icl_dmc_ver1_09.bin",
    ),
    (
        "ELKHARTLAKE",
        &[0x4541, 0x4551, 0x4555, 0x4557, 0x4570, 0x4571],
        "i915/icl_dmc_ver1_09.bin",
    ),
    (
        "JASPERLAKE",
        &[0x4e51, 0x4e55, 0x4e57, 0x4e61, 0x4e71],
        "i915/icl_dmc_ver1_09.bin",
    ),
];

/// The binding i915's files take: Linux's names, as for xe's.
pub const I915_TARGET: Target<'static> = Target {
    driver: "i915",
    hardware: "intel",
    revision: "linux-firmware",
    board: None,
};

/// i915's request for a file, by the name the driver gives it.
pub(super) fn select_i915(request: &Request) -> Option<&'static Recipe> {
    if request.target.hardware != I915_TARGET.hardware
        || request.target.revision != I915_TARGET.revision
    {
        return None;
    }
    I915.iter()
        .find(|recipe| recipe.image.member == request.name)
}

/// The platform of [`I915_PLATFORMS`] that holds `device`.
pub fn i915_platform(device: u16) -> Option<&'static I915Platform> {
    I915_PLATFORMS
        .iter()
        .find(|(_, devices, _)| devices.contains(&device))
}

/// Requires the file i915 asks for on `device`, its platform's display
/// microcontroller program, when `device` is one only i915 drives.
pub fn require_i915(inventory: &mut Inventory, device: u16) -> Result<(), Error> {
    let (_, _, dmc) = i915_platform(device)
        .ok_or_else(|| Error(format!("{device:04x} is no Intel GPU only i915 drives")))?;
    inventory.observe(Requirement {
        name: dmc,
        target: I915_TARGET,
        minimum_bytes: 1,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xe_files(device: u16) -> Vec<alloc::string::String> {
        let mut inventory = Inventory::default();
        require_xe(&mut inventory, device).unwrap();
        let requests = inventory.requests().iter();
        requests.map(|request| request.name.clone()).collect()
    }

    fn xe_member(name: &str, hardware: &str) -> Option<&'static Recipe> {
        let request = serde_json::json!({"schema":1,"complete":true,"requirements":[{
            "name":name,"target":{"driver":"xe","hardware":hardware,
            "revision":"linux-firmware","board":null},"minimum_bytes":1}]});
        let inventory = Inventory::parse(&serde_json::to_vec(&request).unwrap()).unwrap();
        Plan::select(&inventory)
            .ok()
            .map(|plan| plan.selections[0].recipe)
    }

    #[test]
    fn an_intel_gpu_requires_the_guc_and_display_program_its_drivers_name() {
        // Tiger Lake, Alder Lake P and Raptor Lake P, U and S, as in laptops.
        assert_eq!(
            xe_files(0x9a49),
            ["i915/tgl_guc_70.bin", "i915/tgl_dmc_ver2_12.bin"]
        );
        for device in [0x46a3, 0xa7a0, 0xa7a1] {
            assert_eq!(
                xe_files(device),
                ["i915/adlp_guc_70.bin", "i915/adlp_dmc.bin"]
            );
        }
        for device in [0x468b, 0xa78b] {
            assert_eq!(
                xe_files(device),
                ["i915/tgl_guc_70.bin", "i915/adls_dmc_ver2_01.bin"]
            );
        }
        // Alder Lake N: xe's GuC is Tiger Lake's, i915's Alder Lake P's.
        assert_eq!(
            xe_files(0x46d1),
            [
                "i915/tgl_guc_70.bin",
                "i915/adlp_dmc.bin",
                "i915/adlp_guc_70.bin"
            ]
        );
        // The GSC image only where the media GT's C6 needs it: Meteor Lake
        // and Arrow Lake, Lunar Lake. Its GuC loads without it everywhere.
        for device in [0x7d55, 0x7d51, 0xb640] {
            assert_eq!(
                xe_files(device),
                [
                    "i915/mtl_guc_70.bin",
                    "i915/mtl_dmc.bin",
                    "i915/mtl_gsc_1.bin"
                ]
            );
        }
        assert_eq!(
            xe_files(0x64a0),
            [
                "xe/lnl_guc_70.bin",
                "i915/xe2lpd_dmc.bin",
                "xe/lnl_gsc_1.bin"
            ]
        );
        assert_eq!(xe_files(0xe20b), ["xe/bmg_guc_70.bin", "i915/bmg_dmc.bin"]);
        assert_eq!(
            xe_files(0xb080),
            ["xe/ptl_guc_70.bin", "i915/xe3lpd_dmc.bin"]
        );
        assert_eq!(intel_gsc(0x7d55), Some("i915/mtl_gsc_1.bin"));
        assert_eq!(intel_gsc(0x64a0), Some("xe/lnl_gsc_1.bin"));
        // No HuC image on any platform; a GSC image in the GSC column only.
        let mut gscs = 0;
        for (_, _, files) in XE_PLATFORMS {
            for (column, file) in files.iter().enumerate() {
                let Some(file) = file else { continue };
                assert!(!file.contains("huc"), "{file}");
                assert_eq!(file.contains("gsc"), column == 3, "{file}");
                gscs += usize::from(column == 3);
            }
        }
        assert_eq!(gscs, 2);
        for device in [0x9a49, 0x56a0, 0xe20b, 0xb080] {
            assert_eq!(intel_gsc(device), None, "{device:04x}");
        }
        assert_eq!(intel_gpu_platform(0x9a49), Some("Tiger Lake"));
        // Ice Lake and Ivy Bridge are not xe's.
        for device in [0x8a56, 0x0166] {
            assert_eq!(intel_gpu_platform(device), None);
            assert!(require_xe(&mut Inventory::default(), device).is_err());
        }
    }

    #[test]
    fn i915_platforms_name_pinned_optional_display_programs_only() {
        let mut named = Vec::new();
        let mut seen = Vec::new();
        for (platform, devices, dmc) in I915_PLATFORMS {
            assert!(!devices.is_empty(), "{platform}");
            for device in *devices {
                assert!(!seen.contains(device), "{platform} {device:04x}");
                seen.push(*device);
                // xe's devices are not i915's.
                assert!(
                    require_xe(&mut Inventory::default(), *device).is_err(),
                    "{device:04x}"
                );
                let mut inventory = Inventory::default();
                require_i915(&mut inventory, *device).unwrap();
                let requests = inventory.requests();
                assert_eq!(requests.len(), 1, "{platform}");
                assert_eq!(requests[0].name, *dmc);
                assert_eq!(requests[0].target.driver, "i915");
                let recipe = recipe(&requests[0]).unwrap();
                assert!(matches!(recipe.consumer, Consumer::I915));
                assert!(optional(&requests[0]), "{dmc}");
                named.push(recipe.image.member);
            }
        }
        // Every pinned file serves a platform.
        for recipe in I915 {
            assert!(
                named.contains(&recipe.image.member),
                "{}",
                recipe.image.member
            );
        }
        // Skylake's, and none for Tiger Lake (xe's), Broadwell or Ironlake.
        assert_eq!(i915_platform(0x1916).unwrap().2, "i915/skl_dmc_ver1_27.bin");
        for device in [0x9a49, 0x1616, 0x0046] {
            assert!(
                require_i915(&mut Inventory::default(), device).is_err(),
                "{device:04x}"
            );
        }
        // An xe request by an i915 file's name is no i915 recipe.
        let request = Request::from(Requirement {
            name: "i915/skl_dmc_ver1_27.bin",
            target: XE_TARGET,
            minimum_bytes: 1,
        });
        assert!(recipe(&request).is_none());
    }

    #[test]
    fn the_drivers_start_without_their_display_programs_only() {
        let request = |name: &str| {
            Request::from(Requirement {
                name,
                target: XE_TARGET,
                minimum_bytes: 1,
            })
        };
        for name in [
            "i915/tgl_dmc_ver2_12.bin",
            "i915/adlp_dmc.bin",
            "i915/bmg_dmc.bin",
        ] {
            assert!(optional(&request(name)), "{name}");
        }
        // GuC images, i915's on Alder Lake N among them, and the GSC images
        // kept for media C6 are needed; HuC images are no recipes at all.
        for name in [
            "i915/tgl_guc_70.bin",
            "i915/adlp_guc_70.bin",
            "xe/ptl_guc_70.bin",
            "i915/tgl_huc.bin",
            "i915/mtl_gsc_1.bin",
            "xe/lnl_gsc_1.bin",
            "xe/ptl_gsc_1.bin",
        ] {
            assert!(!optional(&request(name)), "{name}");
        }
    }

    #[test]
    fn xe_files_are_pinned_once_under_their_directorys_notice() {
        let mut devices = Vec::new();
        for (platform, ids, files) in XE_PLATFORMS {
            for device in *ids {
                assert!(!devices.contains(device), "{platform} {device:04x}");
                devices.push(*device);
            }
            assert!(files[0].is_some(), "{platform}");
            for file in files.iter().flatten() {
                assert!(xe_member(file, "intel").is_some(), "{platform} {file}");
            }
        }
        for (index, recipe) in XE.iter().enumerate() {
            let member = recipe.image.member;
            assert!(
                XE[..index].iter().all(|other| other.image.member != member),
                "{member}"
            );
            assert!(
                XE_PLATFORMS
                    .iter()
                    .any(|(_, _, files)| files.contains(&Some(member)))
                    != (member == XE_FALLBACK),
                "{member} has no platform, or one and is the fallback"
            );
            let notice = if member.starts_with("xe/") {
                "LICENSES/LICENSE.xe"
            } else {
                "LICENSES/LICENSE.i915"
            };
            assert_eq!(recipe.license.member, notice, "{member}");
            assert_eq!(recipe.image.revision, XE_REVISION);
            assert_eq!(recipe.image.sha256.len(), 64);
        }
        assert_eq!(xe_member("i915/tgl_guc_70.bin", "nvidia").map(|_| ()), None);
        assert_eq!(xe_member("i915/skl_guc_70.bin", "intel").map(|_| ()), None);
    }
}
