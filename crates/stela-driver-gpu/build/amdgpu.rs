//! amdgpu: the `amdgpu-y` composite of its own makefiles, compiled unchanged
//! in Stela's Linux environment with the include paths and per-object flags
//! those makefiles give, and the kernel services it imports that the
//! environment does not yet supply.
//!
//! The unit of its `module_init` (`amdgpu_drv.c`) is compiled with its
//! registrations captured, for the build's one registration image.
use super::native::Linux;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    thread,
};
use stela_c_build::linux_capture::{self as initcalls, UnitSpec};
use stela_c_build::{kbuild_make::Make, linux::Module};

const AMDGPU: &str = "../../third_party/linux/drivers/gpu/drm/amd/amdgpu";
const AUTOCONF: &str = "../stela-linux/src/compat/generated/autoconf.h";
const MODULE: Module<'static> = Module {
    name: "amdgpu",
    file: "drivers/gpu/drm/amd/amdgpu/amdgpu",
};

/// Whether Stela's Kconfig header sets `symbol`.
fn defined(autoconf: &str, symbol: &str) -> bool {
    autoconf.lines().any(|line| {
        line.strip_prefix("#define ")
            .is_some_and(|rest| rest.split_whitespace().next() == Some(symbol))
    })
}

/// The symbols `config DRM_AMDGPU` selects under the configuration `set`
/// answers for: every `select` of the entry whose condition, a conjunction
/// of symbols, holds. The shared profile must hold them all for the port to
/// be a configuration of the driver that Kconfig allows.
fn selected(kconfig: &str, set: impl Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let entry = kconfig
        .split("\nconfig ")
        .find_map(|entry| entry.strip_prefix("DRM_AMDGPU\n"))
        .ok_or("no config DRM_AMDGPU")?;
    let mut selected = Vec::new();
    for line in entry.lines() {
        let Some(select) = line.trim().strip_prefix("select ") else {
            continue;
        };
        let (symbol, condition) = match select.split_once(" if ") {
            Some((symbol, condition)) => (symbol, condition),
            None => (select, ""),
        };
        let mut holds = true;
        for term in condition.split("&&").map(str::trim) {
            if !term.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("the condition of {line:?} is no conjunction"));
            }
            holds &= term.is_empty() || set(&format!("CONFIG_{term}"));
        }
        if holds {
            selected.push(format!("CONFIG_{}", symbol.trim()));
        }
    }
    if selected.is_empty() {
        return Err("DRM_AMDGPU selects nothing".into());
    }
    Ok(selected)
}

/// The kernel services the composite imports that Stela's Linux environment
/// lacks, or defines only as a trap, by service. Building fails when the
/// imports change, so each supplied service leaves this list with its change.
const OPEN: super::image::Open = &[
    (
        "reopening a file without a VFS path",
        &["stela_linux_reopen_unowned_path"],
    ),
    // Only SMU 14's USB-C controller link makes a device link, and
    // device_link_add refuses it, so none is deleted.
    ("device links", &["device_link_del"]),
    // Only the partitions of a partitioned accelerator (amdxcp) are
    // platform devices.
    ("platform devices", &["platform_device_register_full"]),
    // Only the NUMA placement of an accelerator's memory partitions asks.
    ("the firmware's memory map", &["page_is_ram"]),
];

/// The unit of amdgpu's own `module_init`.
const INITCALLS: UnitSpec = UnitSpec {
    scope: stela_c_build::linux_capture::Scope {
        slice: "gpu",
        group: "amdgpu",
        group_ordinal: 38,
        unit_ordinal: 1,
        global_ordinal: 38,
    },
    source: "drivers/gpu/drm/amd/amdgpu/amdgpu_drv",
    module: MODULE.name,
    file: MODULE.file,
};

/// Variables amdgpu's makefiles read that nothing defines, which Make
/// expands to nothing: a switch only an out-of-tree build sets, and a warning
/// flag only a profile with a frame-size warning limit sets.
const UNDEFINED: [&str; 2] = ["AMD_GPU_RAS_MGR", "frame_warn_flag"];

/// One object of the composite: its source and the flags its makefile adds
/// to and removes from the shared ones.
struct Unit {
    source: PathBuf,
    add: Vec<String>,
    remove: Vec<String>,
}

pub fn port(linux: &Linux) -> super::image::Driver {
    let autoconf = fs::read_to_string(AUTOCONF).expect("read Stela's Kconfig header");
    let mut make = Make::default();
    make.configure(&autoconf)
        .unwrap_or_else(|error| panic!("{AUTOCONF}: {error}"));
    let kconfig = Path::new(AMDGPU).join("Kconfig");
    println!("cargo:rerun-if-changed={}", kconfig.display());
    let set = |symbol: &str| defined(&autoconf, symbol);
    let missing = selected(
        &fs::read_to_string(&kconfig).expect("read amdgpu's Kconfig"),
        set,
    )
    .unwrap_or_else(|error| panic!("{}: {error}", kconfig.display()))
    .into_iter()
    .filter(|symbol| !set(symbol))
    .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "DRM_AMDGPU selects {missing:?}, which Stela's Kconfig does not set"
    );
    make.set("src", AMDGPU);
    // Linux's arch/x86/Makefile: the flags of a unit that may use floating
    // point, and those such a unit drops. Stela's general-register-only
    // build is what keeps every other unit off the FPU.
    make.set("CC_FLAGS_FPU", "-msse -msse2 -D_LINUX_FPU_COMPILATION_UNIT");
    make.set("CC_FLAGS_NO_FPU", "-mgeneral-regs-only");
    make.set("CONFIG_CC_IS_CLANG", "y");
    make.include(&Path::new(AMDGPU).join("Makefile"))
        .unwrap_or_else(|error| panic!("{error}"));
    for file in &make.files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    let words = |make: &mut Make, name: &str| {
        make.words(name)
            .unwrap_or_else(|error| panic!("{name}: {error}"))
    };
    let mut objects = words(&mut make, "amdgpu-y");
    assert!(
        words(&mut make, "obj-y") == ["amdgpu.o"],
        "Stela's Kconfig does not select amdgpu"
    );
    // DRM's makefile builds one more object with amdgpu: the platform
    // devices of a partitioned accelerator (`obj-$(CONFIG_DRM_AMDGPU) +=
    // amd/amdxcp/`), which amdgpu calls into. Its makefile sets no flags.
    let mut partitions = Make::default();
    partitions
        .include(&Path::new(AMDGPU).join("../amdxcp/Makefile"))
        .unwrap_or_else(|error| panic!("{error}"));
    println!("cargo:rerun-if-changed={}", partitions.files[0].display());
    objects.extend(
        words(&mut partitions, "amdxcp-y")
            .into_iter()
            .map(|object| format!("../amdxcp/{object}")),
    );
    let mut flags = words(&mut make, "ccflags-y");
    flags.extend(words(&mut make, "subdir-ccflags-y"));
    let per_object = |make: &mut Make, prefix: &str| {
        make.prefixed(prefix)
            .unwrap_or_else(|error| panic!("{prefix}: {error}"))
            .into_iter()
            .collect::<BTreeMap<_, _>>()
    };
    let mut add = per_object(&mut make, "CFLAGS_");
    let mut remove = BTreeMap::new();
    add.retain(|object, flags| match object.strip_prefix("REMOVE_") {
        Some(object) => {
            remove.insert(object.to_owned(), std::mem::take(flags));
            false
        }
        None => true,
    });
    let undefined = make
        .undefined
        .iter()
        .filter(|name| !name.starts_with("CONFIG_"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert!(
        undefined == UNDEFINED,
        "amdgpu's makefiles read other undefined variables: {undefined:?}"
    );
    // A makefile included twice names its objects twice (the display core
    // lists `dml` in two places); Make links a prerequisite once.
    let mut seen = BTreeSet::new();
    let mut units = objects
        .iter()
        .filter(|object| seen.insert(*object))
        .map(|object| {
            let source = Path::new(AMDGPU).join(object).with_extension("c");
            assert!(source.is_file(), "{} is missing", source.display());
            Unit {
                source,
                add: add.remove(object).unwrap_or_default(),
                remove: remove.remove(object).unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    // Flags a makefile gives an object the configuration does not build go
    // unused, as in Kbuild.
    // Compiled below, with its registrations captured.
    let registering = initcalls::Unit::from(INITCALLS).path();
    let registering = units.remove(
        units
            .iter()
            .position(|unit| unit.source == registering)
            .expect("amdgpu's composite holds amdgpu_drv.c"),
    );

    let mut build = stela_c_build::native::configure();
    let mut includes = Vec::new();
    for flag in &flags {
        match flag.strip_prefix("-I") {
            Some(directory) => includes.push(PathBuf::from(directory)),
            None => {
                assert!(
                    flag.starts_with("-D") || flag.starts_with("-Wno-"),
                    "unsupported amdgpu flag {flag}"
                );
                build.flag(flag);
            }
        }
    }
    linux
        .environment
        .apply(&mut build, None, &includes, Some(&MODULE));
    stela_c_build::linux_build::upstream_warnings(&mut build);
    // As Kbuild builds every profile: Linux relies on the optimizer to drop
    // the calls its configuration disables.
    build.opt_level(2).cargo_metadata(false);
    let jobs = env::var("NUM_JOBS")
        .ok()
        .and_then(|jobs| jobs.parse().ok())
        .unwrap_or(1)
        .max(1);
    // Every unit is compiled before any failure is reported, so one build
    // names all that fail.
    let results = thread::scope(|scope| {
        units
            .chunks(units.len().div_ceil(jobs))
            .map(|chunk| {
                let build = &build;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|unit| {
                            let mut build = build.clone();
                            for flag in &unit.remove {
                                build.remove_flag(flag);
                            }
                            for flag in &unit.add {
                                build.flag(flag);
                            }
                            build
                                .file(&unit.source)
                                .try_compile_intermediates()
                                .map_err(|_| unit.source.clone())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|compiling| compiling.join().expect("a compiling thread"))
            .collect::<Vec<_>>()
    });
    let failed = results
        .iter()
        .filter_map(|result| result.as_ref().err())
        .collect::<Vec<_>>();
    assert!(
        failed.is_empty(),
        "{} of amdgpu's {} units do not compile: {failed:?}",
        failed.len(),
        units.len()
    );
    let mut registering_build = build.clone();
    for flag in &registering.remove {
        registering_build.remove_flag(flag);
    }
    for flag in &registering.add {
        registering_build.flag(flag);
    }
    let compiler = initcalls::compiler(&registering_build);
    let placement = initcalls::Placement::new(
        &registering.source,
        &stela_c_build::linux_build::out_dir(),
        &[
            Path::new("../.."),
            compiler.parent().expect("compiler root"),
        ],
    )
    .expect("amdgpu source placement");
    let captured = initcalls::compile(
        &registering_build,
        Path::new("../stela-linux/src/compat/stela/registration.h"),
        INITCALLS,
        &placement,
    );
    build
        .objects(results.into_iter().flatten().flatten())
        .object(captured.object())
        .compile("stela-amdgpu");
    // Hosted programs test the environment and link no driver.
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "uefi") {
        println!("cargo:rustc-link-lib=static=stela-amdgpu");
    }
    super::image::Driver {
        name: MODULE.name,
        spec: INITCALLS,
        captured,
        more: Vec::new(),
        archive: stela_c_build::linux_build::out_dir().join("libstela-amdgpu.a"),
        open: OPEN,
    }
}

/// Writes what the boot asks of amdgpu before Linux sees a device, read from
/// the driver: its PCI ID table with each device's DMA mask and whether this
/// build's probe takes its family, the classes it takes any AMD function of,
/// and the devices it leaves to older drivers (`src/amdgpu.rs`).
pub fn tables() {
    let read = |path: PathBuf| {
        println!("cargo:rerun-if-changed={}", path.display());
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    };
    let source = |name: &str| read(Path::new(AMDGPU).join(name));
    let table = Table::read(
        &source("amdgpu_drv.c"),
        &read(PathBuf::from(
            "../../third_party/linux/include/linux/pci_ids.h",
        )),
        &read(PathBuf::from(AUTOCONF)),
    )
    .unwrap_or_else(|error| panic!("amdgpu's PCI ID table: {error}"));
    // The DMA mask of each family: that of the memory controllers its IP
    // block list can hold, the narrowest where there are several.
    let lists = block_lists(&source("amdgpu_device.c"))
        .unwrap_or_else(|error| panic!("amdgpu's IP block lists: {error}"));
    let mut masks = BTreeMap::new();
    let mut mask = |list: &str| {
        *masks.entry(list.to_owned()).or_insert_with(|| {
            let text = source(&format!("{list}.c"));
            controllers(&text)
                .into_iter()
                .map(|version| {
                    let file = format!("gmc_v{version}_0.c");
                    mask_bits(&source(&file)).unwrap_or_else(|error| panic!("{file}: {error}"))
                })
                .min()
                .unwrap_or_else(|| panic!("{list}.c adds no memory controller"))
        })
    };
    let discovery = mask(&lists.default);
    let mut family_mask = |family: &str| match lists.families.get(family) {
        Some(list) => mask(list),
        None => discovery,
    };
    // A revision quirk moves a device to another family; the answers here
    // are by device ID, so both families must have the same.
    for (listed, quirked) in &table.quirks {
        assert!(
            family_mask(listed) == family_mask(quirked),
            "a revision quirk moves {listed} to {quirked}, which sets another DMA mask"
        );
    }
    // The register window's BAR (`rmmio_base`), the same for every family
    // Stela drives: asserted below for each.
    let window = RegisterWindow::read(
        &source("amdgpu_device.c"),
        &read(PathBuf::from(
            "../../third_party/linux/include/drm/amd_asic_type.h",
        )),
    )
    .unwrap_or_else(|error| panic!("amdgpu's register window: {error}"));
    // The graphics generation of each family: the oldest graphics block its
    // IP block list can hold.
    let mut generations = BTreeMap::new();
    let mut generation = |list: &str| {
        *generations.entry(list.to_owned()).or_insert_with(|| {
            graphics(&source(&format!("{list}.c")))
                .into_iter()
                .min()
                .unwrap_or_else(|| panic!("{list}.c adds no graphics block"))
        })
    };
    let discovery_generation = generation(&lists.default);
    let mut family_generation = |family: &str| match lists.families.get(family) {
        Some(list) => generation(list),
        None => discovery_generation,
    };
    // A generation RADV's device cannot serve the Metal device on is left
    // out (`stela_driver_core::amd_metal`): the image holds no support for
    // it, and holds it for every family that serves. Every family that
    // serves has its registers in the same BAR, so the table carries
    // neither the probe's answer nor the BAR.
    for (family, _) in table.devices.values() {
        let generation = family_generation(family);
        let serves = generation >= stela_driver_core::amd_metal::FIRST_GENERATION;
        assert!(
            serves || table.unbuilt.contains_key(family),
            "the profile builds amdgpu's support for {family}, of graphics generation \
             {generation}, which cannot serve the Metal device: leave its Kconfig symbol unset"
        );
        assert!(
            !serves || !table.unbuilt.contains_key(family),
            "the profile leaves out amdgpu's support for {family}, of graphics generation \
             {generation}, which serves the Metal device: set its Kconfig symbol"
        );
        assert!(
            !serves || window.drives(family),
            "{family}, which serves the Metal device, has its registers in BAR {}, not {}",
            window.older,
            window.newer
        );
    }
    assert!(
        window.drives("CHIP_IP_DISCOVERY"),
        "a device IP discovery identifies has its registers in BAR {}, not {}",
        window.older,
        window.newer
    );
    for (listed, quirked) in &table.quirks {
        assert!(
            family_generation(listed) == family_generation(quirked),
            "a revision quirk moves {listed} to {quirked}, of another graphics generation"
        );
    }
    // The graphics versions IP discovery gives a block without graphics
    // queues, on which RADV opens no device (`radv_is_gpu_supported`).
    let compute_only = compute_only(&source("amdgpu_discovery.c"), &source)
        .unwrap_or_else(|error| panic!("amdgpu's graphics blocks: {error}"));
    // The graphics versions IP discovery flags as built into the processor.
    let apus = apu_graphics(&source("amdgpu_discovery.c"))
        .unwrap_or_else(|error| panic!("amdgpu's integrated graphics blocks: {error}"));
    let mut text = String::from(
        "/// amdgpu's `pciidlist` entries that name a device: its ID, its family\n\
         /// (`CHIP_*` without the prefix), its graphics generation (the oldest\n\
         /// graphics block the family's list holds), the bits of the DMA mask\n\
         /// its family's memory controller sets, and whether the table flags it\n\
         /// `AMD_IS_APU`.\n\
         pub(crate) const DEVICES: &[(u16, &str, u8, u8, bool)] = &[\n",
    );
    for (device, (family, apu)) in &table.devices {
        let bits = family_mask(family);
        let generation = family_generation(family);
        let name = family.strip_prefix("CHIP_").unwrap_or(family);
        text.push_str(&format!(
            "    ({device:#06x}, {name:?}, {generation}, {bits}, {apu}),\n"
        ));
    }
    text.push_str(
        "];\n/// The classes (base, sub-class, interface) of which the table takes any\n\
         /// AMD function, to identify it by IP discovery.\n\
         pub(crate) const CLASSES: &[u32] = &[\n",
    );
    for class in &table.classes {
        text.push_str(&format!("    {class:#08x},\n"));
    }
    text.push_str(
        "];\n/// `amdgpu_unsupported_pciidlist`: devices of older drivers.\n\
         pub(crate) const LEFT_TO_RADEON: &[u16] = &[\n",
    );
    for device in &table.unsupported {
        text.push_str(&format!("    {device:#06x},\n"));
    }
    text.push_str(&format!(
        "];\n/// The narrowest mask of the memory controllers IP discovery chooses from.\n\
         pub(crate) const DISCOVERY_BITS: u8 = {discovery};\n\
         /// The BAR of the register window of every device Stela drives.\n\
         pub(crate) const REGISTERS: u8 = {};\n\
         /// The oldest graphics generation IP discovery chooses a block of.\n\
         pub(crate) const DISCOVERY_GENERATION: u8 = {discovery_generation};\n\
         /// The graphics versions (major, minor, revision) whose block\n\
         /// IP discovery adds has no graphics queue: compute accelerators.\n\
         pub(crate) const COMPUTE_ONLY: &[(u8, u8, u8)] = &{compute_only:?};\n\
         /// The graphics versions (major, minor, revision) of a GPU IP\n\
         /// discovery identifies that amdgpu flags `AMD_IS_APU`.\n\
         pub(crate) const DISCOVERED_APUS: &[(u8, u8, u8)] = &{apus:?};\n",
        window.newer
    ));
    text.push_str(
        "/// amdgpu's interface's families (`AMDGPU_FAMILY_*`), by number.\n\
         pub(crate) const FAMILIES: &[(u32, &str)] = &[\n",
    );
    for (number, name) in families(&read(PathBuf::from(
        "../../third_party/linux/include/uapi/drm/amdgpu_drm.h",
    ))) {
        text.push_str(&format!("    ({number}, {name:?}),\n"));
    }
    text.push_str("];\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"));
    fs::write(output.join("amdgpu_devices.rs"), text).expect("write amdgpu's device table");
}

/// The families `amdgpu_drm.h` defines, `#define AMDGPU_FAMILY_<name>
/// <number>`, by number, without the unknown family 0.
fn families(header: &str) -> Vec<(u32, String)> {
    let mut families: Vec<(u32, String)> = header
        .lines()
        .filter_map(|line| {
            let mut words = line
                .strip_prefix("#define AMDGPU_FAMILY_")?
                .split_whitespace();
            let name = words.next()?;
            let number = words.next()?.parse().ok()?;
            (number != 0).then(|| (number, name.to_owned()))
        })
        .collect();
    families.sort();
    assert!(!families.is_empty(), "amdgpu_drm.h defines no family");
    families
}

/// `amdgpu_drv.c`'s device table and probe rules.
#[derive(Debug, Default, PartialEq)]
struct Table {
    /// Each device the table names, with its family (`CHIP_*`) and whether
    /// the table flags it `AMD_IS_APU`.
    devices: BTreeMap<u16, (String, bool)>,
    /// The class entries, as `class << 8`.
    classes: Vec<u32>,
    unsupported: BTreeSet<u16>,
    /// The families `amdgpu_support_enabled` refuses in this configuration,
    /// with the generation's name in its message.
    unbuilt: BTreeMap<String, String>,
    /// Each revision quirk: the family the table lists and the one the
    /// quirk gives the device instead.
    quirks: Vec<(String, String)>,
}

fn hex(text: &str) -> Result<u16, String> {
    text.trim()
        .strip_prefix("0x")
        .and_then(|digits| u16::from_str_radix(digits, 16).ok())
        .ok_or_else(|| format!("{text:?} is no 16-bit hexadecimal number"))
}

/// The text between `start` and the next `end` after it.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Result<&'a str, String> {
    text.split_once(start)
        .and_then(|(_, rest)| rest.split_once(end))
        .map(|(body, _)| body)
        .ok_or_else(|| format!("no {start:?} closed by {end:?}"))
}

/// The label of a `case CHIP_X:` line.
fn case(line: &str) -> Option<&str> {
    line.trim().strip_prefix("case ")?.strip_suffix(':')
}

impl Table {
    fn read(driver: &str, ids: &str, autoconf: &str) -> Result<Self, String> {
        let mut table = Self::default();
        for word in between(driver, "amdgpu_unsupported_pciidlist[] = {", "};")?
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|word| word.starts_with("0x"))
        {
            table.unsupported.insert(hex(word)?);
        }
        let entries = between(driver, "struct pci_device_id pciidlist[] = {", "{0, 0, 0}")?;
        for entry in entries
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('{') && !line.starts_with("{ PCI_DEVICE"))
        {
            let fields = entry
                .trim_start_matches('{')
                .trim_end_matches(',')
                .trim_end_matches('}')
                .split(',')
                .map(str::trim)
                .collect::<Vec<_>>();
            let [vendor, device, .., flags] = fields.as_slice() else {
                return Err(format!("entry {entry:?}"));
            };
            if fields.len() != 7 || *vendor != "0x1002" {
                return Err(format!("entry {entry:?}"));
            }
            let mut flags = flags.split('|');
            let family = flags.next().unwrap_or("");
            let flags = flags.collect::<Vec<_>>();
            // Mobility and APU do not change the probe's answer; a flag that
            // does (experimental support) needs its rule here first.
            if !family.starts_with("CHIP_")
                || flags
                    .iter()
                    .any(|flag| !["AMD_IS_MOBILITY", "AMD_IS_APU"].contains(flag))
            {
                return Err(format!(
                    "entry {entry:?} has flags this reader does not know"
                ));
            }
            let apu = flags.contains(&"AMD_IS_APU");
            if table
                .devices
                .insert(hex(device)?, (family.to_owned(), apu))
                .is_some()
            {
                return Err(format!("{device} is listed twice"));
            }
        }
        // The class entries: any device of vendor 1002 with exactly that class.
        for entry in entries.split("{ PCI_DEVICE(").skip(1) {
            let entry = entry.split('}').next().unwrap_or("");
            let class = between(entry, ".class = ", " << 8")?;
            if !entry.starts_with("0x1002, PCI_ANY_ID)")
                || !entry.contains(".class_mask = 0xffffff")
                || !entry.contains(".driver_data = CHIP_IP_DISCOVERY")
            {
                return Err(format!("class entry {entry:?}"));
            }
            let value = ids
                .lines()
                .find_map(|line| {
                    let mut words = line.split_whitespace();
                    (words.next() == Some("#define") && words.next() == Some(class))
                        .then(|| words.next())
                        .flatten()
                })
                .ok_or_else(|| format!("{class} is not in pci_ids.h"))?;
            table.classes.push(u32::from(hex(value)?) << 8);
        }
        if table.devices.is_empty() || table.classes.is_empty() {
            return Err("the table names no device or no class".into());
        }
        // `amdgpu_support_enabled`: each group of families, the generation
        // its message names and the Kconfig symbol that builds its support.
        let rules = between(
            driver,
            "static bool amdgpu_support_enabled(",
            "\n\tdefault:",
        )?;
        let (mut families, mut generation) = (Vec::new(), None);
        for line in rules.lines() {
            if let Some(family) = case(line) {
                families.push(family);
            } else if let Some(name) = line.trim().strip_prefix("gen = \"") {
                generation = name.strip_suffix("\";");
            } else if let Some(symbol) = line
                .trim()
                .strip_prefix("amdgpu_support_built = IS_ENABLED(")
            {
                let symbol = symbol.trim_end_matches(");");
                let generation = generation
                    .take()
                    .ok_or_else(|| format!("{symbol} has no generation name"))?;
                let built = defined(autoconf, symbol);
                for family in families.drain(..) {
                    if !built {
                        table
                            .unbuilt
                            .insert(family.to_owned(), generation.to_owned());
                    }
                }
            }
        }
        if !families.is_empty() {
            return Err(format!("families {families:?} have no support rule"));
        }
        // A revision quirk moves a device to another family; the answers
        // are by device ID, so the probe must take both or neither.
        for quirk in between(driver, "asic_type_quirks[] = {", "};")?
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('{'))
        {
            let fields = quirk
                .trim_matches(|c| c == '{' || c == '}' || c == ',')
                .split(',')
                .map(str::trim)
                .collect::<Vec<_>>();
            let [device, _, family] = fields.as_slice() else {
                return Err(format!("quirk {quirk:?}"));
            };
            let (listed, _) = table
                .devices
                .get(&hex(device)?)
                .ok_or_else(|| format!("quirk {quirk:?} names no listed device"))?;
            if table.unbuilt.get(listed) != table.unbuilt.get(*family) {
                return Err(format!("quirk {quirk:?} changes the probe's answer"));
            }
            table.quirks.push((listed.clone(), (*family).to_owned()));
        }
        Ok(table)
    }
}

/// `amdgpu_device_init`'s choice of the register window's BAR: `newer` for
/// the families from `first` on in `enum amd_asic_type`'s order, `older`
/// for those before it.
#[derive(Debug, PartialEq)]
struct RegisterWindow {
    order: Vec<String>,
    first: String,
    newer: u8,
    older: u8,
}

impl RegisterWindow {
    fn read(device: &str, types: &str) -> Result<Self, String> {
        let order = between(types, "enum amd_asic_type {", "};")?
            .lines()
            .filter_map(|line| {
                let name = line.trim().split([' ', ',', '=', '\t']).next()?;
                name.starts_with("CHIP_").then(|| name.to_owned())
            })
            .collect::<Vec<_>>();
        // The first assignment of the window's base and the family test
        // that guards it, then the assignment of its `else`.
        let at = device
            .find("\t\tadev->rmmio_base = pci_resource_start(")
            .ok_or("no rmmio_base assignment")?;
        let (_, condition) = device[..at]
            .rsplit_once("if (adev->asic_type >= ")
            .ok_or("the rmmio_base assignment has no family test")?;
        let first = condition
            .strip_suffix(") {\n")
            .ok_or_else(|| format!("register window rule {condition:?}"))?;
        let rest = &device[at..];
        let older = between(rest, "} else {\n\t\tadev->rmmio_base = ", ";")?;
        let bar = |text: &str| -> Result<u8, String> {
            between(text, "pci_resource_start(adev->pdev, ", ")")?
                .parse()
                .map_err(|_| format!("register window BAR in {text:?}"))
        };
        if !order.iter().any(|name| name == first) {
            return Err(format!("{first} is no amd_asic_type"));
        }
        Ok(Self {
            first: first.to_owned(),
            newer: bar(rest)?,
            older: bar(older)?,
            order,
        })
    }

    /// Whether `family` (`CHIP_*`) has its registers in the `newer` BAR.
    fn drives(&self, family: &str) -> bool {
        let position = |name: &str| {
            self.order
                .iter()
                .position(|known| known == name)
                .unwrap_or_else(|| panic!("{name} is no amd_asic_type"))
        };
        position(family) >= position(&self.first)
    }
}

/// Which source sets each family's IP blocks (`si`, `cik`, `vi`), and which
/// sets them for every other family, from `amdgpu_device_ip_early_init`.
struct BlockLists {
    families: BTreeMap<String, String>,
    default: String,
}

fn block_lists(device: &str) -> Result<BlockLists, String> {
    let body = between(device, "static int amdgpu_device_ip_early_init(", "\n}\n")?;
    let (mut lists, mut pending, mut default) = (BTreeMap::new(), Vec::new(), None);
    let mut in_default = false;
    for line in body.lines() {
        if let Some(family) = case(line) {
            pending.push(family);
        } else if line.trim() == "default:" {
            in_default = true;
        } else if let Some((call, _)) = line.split_once("_set_ip_blocks(adev)") {
            let list = call
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or("");
            if in_default {
                default = Some(list.to_owned());
                break;
            }
            for family in pending.drain(..) {
                lists.insert(family.to_owned(), list.to_owned());
            }
        }
    }
    Ok(BlockLists {
        families: lists,
        default: default.ok_or("no default IP block list")?,
    })
}

/// The major versions of the memory controller blocks `list` adds.
fn controllers(list: &str) -> BTreeSet<u32> {
    list.split("&gmc_v")
        .skip(1)
        .filter_map(|rest| rest.split('_').next()?.parse().ok())
        .collect()
}

/// The major versions of the graphics blocks `list` adds, which Mesa names
/// the generations by (`gfx_v8_0_ip_block` is GFX8's).
fn graphics(list: &str) -> BTreeSet<u8> {
    list.split("&gfx_v")
        .skip(1)
        .filter_map(|rest| rest.split('_').next()?.parse().ok())
        .collect()
}

/// The bits of the DMA mask a memory controller's source sets: the number
/// in its `DMA_BIT_MASK`, or the least the variable there is assigned.
fn mask_bits(controller: &str) -> Result<u8, String> {
    let argument = between(
        controller,
        "dma_set_mask_and_coherent(adev->dev, DMA_BIT_MASK(",
        "))",
    )?;
    if let Ok(bits) = argument.parse() {
        return Ok(bits);
    }
    let assignment = format!("{argument} =");
    controller
        .split(&assignment)
        .skip(1)
        .filter(|rest| !rest.starts_with('='))
        .flat_map(|rest| {
            rest.split(';')
                .next()
                .unwrap_or("")
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter_map(|word| word.parse::<u8>().ok())
                .filter(|bits| (32..=64).contains(bits))
                .collect::<Vec<_>>()
        })
        .min()
        .ok_or_else(|| format!("{argument} is assigned no mask width"))
}

/// The graphics IP versions `amdgpu_discovery_set_gc_ip_blocks` gives a
/// graphics block that makes no graphics queue: every version of a block
/// whose source never sets `num_gfx_rings` (`gfx_v9_4_3.c`), and those a
/// block's source tests for when it sets the count to zero (Arcturus' and
/// Aldebaran's in `gfx_v9_0.c`). `source` reads a file of amdgpu's.
fn compute_only(
    discovery: &str,
    source: &dyn Fn(&str) -> String,
) -> Result<Vec<(u8, u8, u8)>, String> {
    let body = between(
        discovery,
        "static int amdgpu_discovery_set_gc_ip_blocks(",
        "\n}\n",
    )?;
    let (mut blocks, mut pending) = (BTreeMap::<String, Vec<(u8, u8, u8)>>::new(), Vec::new());
    for line in body.lines() {
        if let Some(version) = line.trim().strip_prefix("case ").and_then(ip_version) {
            pending.push(version);
        } else if let Some(rest) = line.split_once("amdgpu_device_ip_block_add(adev, &gfx_v") {
            let block = rest
                .1
                .split_once("_ip_block")
                .ok_or_else(|| format!("graphics block in {line:?}"))?
                .0;
            blocks
                .entry(format!("gfx_v{block}"))
                .or_default()
                .append(&mut pending);
        } else if line.trim() == "default:" {
            break;
        }
    }
    if blocks.is_empty() {
        return Err(String::from("no graphics block is added"));
    }
    let mut versions = Vec::new();
    for (block, listed) in blocks {
        let text = source(&format!("{block}.c"));
        if !text.contains("num_gfx_rings =") {
            versions.extend(listed);
            continue;
        }
        for (before, _) in text
            .match_indices("num_gfx_rings = 0;")
            .map(|(at, found)| (&text[..at], found))
        {
            let condition = before
                .rsplit_once("if (")
                .map_or("", |(_, condition)| condition);
            versions.extend(
                condition
                    .split("IP_VERSION")
                    .skip(1)
                    .filter_map(ip_version)
                    .filter(|version| listed.contains(version)),
            );
        }
    }
    versions.sort_unstable();
    versions.dedup();
    Ok(versions)
}

/// The graphics IP versions `amdgpu_discovery_set_ip_blocks` flags
/// `AMD_IS_APU`: the cases of its switch on the graphics block's version
/// whose only statement sets the flag.
fn apu_graphics(discovery: &str) -> Result<Vec<(u8, u8, u8)>, String> {
    let marker = "switch (amdgpu_ip_version(adev, GC_HWIP, 0)) {";
    for (at, _) in discovery.match_indices(marker) {
        let mut versions = Vec::new();
        for line in discovery[at + marker.len()..].lines().map(str::trim) {
            if let Some(version) = line.strip_prefix("case ").and_then(ip_version) {
                versions.push(version);
            } else if line.is_empty() {
                continue;
            } else {
                if line == "adev->flags |= AMD_IS_APU;" && !versions.is_empty() {
                    versions.sort_unstable();
                    return Ok(versions);
                }
                break;
            }
        }
    }
    Err(String::from(
        "no switch on the graphics version sets AMD_IS_APU",
    ))
}

/// The version of an `IP_VERSION(major, minor, revision)` that `text`
/// starts with, after its name.
fn ip_version(text: &str) -> Option<(u8, u8, u8)> {
    let arguments = text
        .strip_prefix("IP_VERSION")
        .unwrap_or(text)
        .trim_start()
        .strip_prefix('(')?
        .split_once(')')?
        .0;
    let mut numbers = arguments
        .split(',')
        .map(|number| number.trim().parse().ok());
    let version = (numbers.next()??, numbers.next()??, numbers.next()??);
    numbers.next().is_none().then_some(version)
}
