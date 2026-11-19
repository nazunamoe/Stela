//! i915: the `i915-y` composite of its own Makefile, with the display code it
//! shares with xe compiled from the same sources as i915's own build of it,
//! unchanged in Stela's Linux environment, and the kernel services it imports
//! that the environment does not yet supply.
//!
//! The display code is built twice, as Linux builds it once for each driver:
//! xe compiles it against its compatibility headers and i915 with `-DI915`
//! against its own, so the two object sets differ. Kbuild therefore forbids
//! both drivers built in with display, since both would define every display
//! symbol. Here each global symbol i915's archive defines that another
//! driver's archive also defines is renamed in i915's archive, at its
//! definition and at every reference from i915's own objects, so each driver
//! reaches its own copy and a symbol of one never serves the other
//! ([`share`]). Nothing else changes: the import check then reads both
//! archives as they link.
use super::i915_tables;
use super::kbuild::{Config, Kbuild, Rule};
use super::native::{self, Linux};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
};
use stela_c_build::linux::Module;
use stela_c_build::linux_capture::{self as initcalls, Unit, UnitSpec};

const I915: &str = "../../third_party/linux/drivers/gpu/drm/i915";
const AUTOCONF: &str = "../stela-linux/src/compat/generated/autoconf.h";
const MODULE: Module<'static> = Module {
    name: "i915",
    file: "drivers/gpu/drm/i915/i915",
};
const COMPOSITE: &str = "i915";

/// The prefix a shared symbol of i915's archive takes.
pub const SHARED_PREFIX: &str = "stela_i915_";

/// The Makefile's one rule, the header self-test, which runs only with
/// `DRM_I915_WERROR`.
const RULES: [(&str, &str, &[&str]); 1] = [(
    "$(obj)/%.hdrtest",
    "$(src)/%.h FORCE",
    &["$(call if_changed_dep,hdrtest)"],
)];
/// Its variables without a selector, all of the header self-test.
const DESCRIPTIONS: [&str; 3] = ["cmd_hdrtest", "no-header-test", "quiet_cmd_hdrtest"];

/// What `config DRM_I915` selects that Stela's Kconfig leaves out, each
/// because nothing compiled reads it: SHMEM and TMPFS name the file system
/// i915's objects take pages from, which the environment serves as shmem
/// files (`kernel/file.c`) without a mount; RELAY carries the GuC log to
/// debugfs, which is not built.
const UNSELECTED: [&str; 3] = ["CONFIG_SHMEM", "CONFIG_TMPFS", "CONFIG_RELAY"];

/// The kernel services the composite imports that Stela's Linux environment
/// lacks, or defines only as a trap, by service. Building fails when the
/// imports change, so each supplied service leaves this list with its change.
const OPEN: super::image::Open = &[
    (
        "reopening a file without a VFS path",
        &["stela_linux_reopen_unowned_path"],
    ),
    // The GuC log's relay channel, which only debugfs opens.
    (
        "relay channels",
        &[
            "relay_buf_full",
            "relay_close",
            "relay_flush",
            "relay_open",
            "relay_switch_subbuf",
        ],
    ),
    // The GT and microcontroller debugfs files, which are not registered.
    (
        "debugfs file operations",
        &[
            "seq_lseek",
            "seq_read",
            "single_open",
            "single_open_size",
            "single_release",
        ],
    ),
    // As amdgpu's list has it: the PXP TEE component's device link (Meteor
    // Lake's content protection, xe's platform), which device_link_add
    // refuses, so none is deleted.
    ("device links", &["device_link_del"]),
    // Only Bay Trail's and Cherry Trail's LPE audio is a platform device,
    // and no driver is handed their graphics.
    ("platform devices", &["platform_device_register_full"]),
];

/// The unit of i915's own `module_init`. It registers before xe's (36), so
/// Linux offers i915 a device both tables hold first, as upstream binds
/// Tiger Lake to Meteor Lake, DG1 and DG2 to i915 by default, and xe gets
/// the device when i915's probe fails. Ordinals 34 and 35 are i915's
/// parameter units ([`PARAMETERS`], [`DISPLAY_PARAMETERS`]).
const INITCALLS: UnitSpec = UnitSpec {
    scope: stela_c_build::linux_capture::Scope {
        slice: "gpu",
        group: "i915",
        group_ordinal: 33,
        unit_ordinal: 1,
        global_ordinal: 33,
    },
    source: "drivers/gpu/drm/i915/i915_module",
    module: MODULE.name,
    file: MODULE.file,
};
fn initcalls_unit() -> Unit {
    INITCALLS.into()
}

/// The unit of i915's own parameters (`i915.huc_firmware_path` and
/// `i915.gsc_firmware_path` among them), captured so the command line sets
/// them, as in Linux, where they are the module `i915`'s.
const PARAMETERS: UnitSpec = UnitSpec {
    scope: stela_c_build::linux_capture::Scope {
        slice: "gpu",
        group: "i915",
        group_ordinal: 33,
        unit_ordinal: 2,
        global_ordinal: 34,
    },
    source: "drivers/gpu/drm/i915/i915_params",
    module: MODULE.name,
    file: MODULE.file,
};

/// i915's build of the display parameters it shares with xe
/// (`i915.vbt_firmware` among them), captured as xe's build of them is. Its
/// globals take [`SHARED_PREFIX`] with the rest of i915's display objects.
const DISPLAY_PARAMETERS: UnitSpec = UnitSpec {
    scope: stela_c_build::linux_capture::Scope {
        slice: "gpu",
        group: "i915",
        group_ordinal: 33,
        unit_ordinal: 3,
        global_ordinal: 35,
    },
    source: "drivers/gpu/drm/i915/display/intel_display_params",
    module: MODULE.name,
    file: MODULE.file,
};

/// Whether Stela's Kconfig header sets `symbol`.
fn defined(autoconf: &str, symbol: &str) -> bool {
    autoconf.lines().any(|line| {
        line.strip_prefix("#define ")
            .is_some_and(|rest| rest.split_whitespace().next() == Some(symbol))
    })
}

/// The symbols `config <entry>` selects under the configuration `set`
/// answers for: every `select` whose condition, a conjunction of symbols,
/// holds.
fn selected(kconfig: &str, entry: &str, set: impl Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let body = kconfig
        .split("\nconfig ")
        .find_map(|text| text.strip_prefix(&format!("{entry}\n")))
        .ok_or_else(|| format!("no config {entry}"))?;
    let mut chosen = Vec::new();
    for line in body.lines() {
        let Some(select) = line.trim().strip_prefix("select ") else {
            continue;
        };
        let (symbol, condition) = select.split_once(" if ").unwrap_or((select, ""));
        let mut holds = true;
        for term in condition.split("&&").map(str::trim) {
            if !term.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("the condition of {line:?} is no conjunction"));
            }
            holds &= term.is_empty() || set(&format!("CONFIG_{term}"));
        }
        if holds {
            chosen.push(format!("CONFIG_{}", symbol.trim()));
        }
    }
    Ok(chosen)
}

/// `others` are the archives of the drivers built before i915, which
/// [`share`] keeps apart from it.
pub fn port(linux: &Linux, others: &[PathBuf]) -> super::image::Driver {
    let autoconf = fs::read_to_string(AUTOCONF).expect("read Stela's Kconfig header");
    let config = Config::parse(&autoconf).unwrap_or_else(|error| panic!("{AUTOCONF}: {error}"));
    let kconfig = Path::new(I915).join("Kconfig");
    println!("cargo:rerun-if-changed={}", kconfig.display());
    let set = |symbol: &str| defined(&autoconf, symbol);
    let missing = selected(
        &fs::read_to_string(&kconfig).expect("read i915's Kconfig"),
        "DRM_I915",
        set,
    )
    .unwrap_or_else(|error| panic!("{}: {error}", kconfig.display()))
    .into_iter()
    .filter(|symbol| !set(symbol) && !UNSELECTED.contains(&symbol.as_str()))
    .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "DRM_I915 selects {missing:?}, which Stela's Kconfig does not set"
    );
    let kbuild = Kbuild::read(Path::new(I915), &config).unwrap_or_else(|error| panic!("{error}"));
    for file in &kbuild.files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    check_makefile(&kbuild);
    assert!(
        kbuild
            .objects("obj")
            .is_ok_and(|objects| objects == ["i915.o"]),
        "Stela's Kconfig does not select i915 alone"
    );
    assert!(!kbuild.werror, "i915's Makefile selects -Werror");
    let registering = initcalls_unit().path();
    let captured_units = [
        registering.clone(),
        Unit::from(PARAMETERS).path(),
        Unit::from(DISPLAY_PARAMETERS).path(),
    ];
    let sources = kbuild
        .objects(COMPOSITE)
        .unwrap_or_else(|error| panic!("{error}"))
        .iter()
        .map(|object| Path::new(I915).join(object).with_extension("c"))
        .filter(|source| !captured_units.contains(source))
        .collect::<Vec<_>>();
    assert_eq!(
        sources.len() + captured_units.len(),
        kbuild
            .objects(COMPOSITE)
            .unwrap_or_else(|error| panic!("{error}"))
            .len(),
        "i915's composite no longer holds each captured unit"
    );
    let mut includes = kbuild.includes.clone();
    includes.extend(
        kbuild
            .tree_includes
            .iter()
            .map(|directory| Path::new(native::LINUX).join(directory)),
    );
    let mut build = stela_c_build::native::configure();
    linux
        .environment
        .profile(&mut build, None, &includes, Some(&MODULE));
    for name in &kbuild.defines {
        build.define(name, None);
    }
    for directory in ["", "display", "gem", "gt", "gt/uc", "pxp"] {
        println!(
            "cargo:rerun-if-changed={}",
            Path::new(I915).join(directory).display()
        );
    }
    stela_c_build::linux_build::upstream_warnings(&mut build);
    for flag in &kbuild.optional_flags {
        build.flag_if_supported(flag);
    }
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
        sources
            .chunks(sources.len().div_ceil(jobs))
            .map(|chunk| {
                let build = &build;
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|source| {
                            build
                                .clone()
                                .file(source)
                                .try_compile_intermediates()
                                .map_err(|error| (source.clone(), error.to_string()))
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
        "{} of i915's {} units do not compile: {:?}; the first: {}",
        failed.len(),
        sources.len(),
        failed.iter().map(|(source, _)| source).collect::<Vec<_>>(),
        failed[0].1
    );
    let compiler = initcalls::compiler(&build);
    let placement = initcalls::Placement::new(
        &registering,
        &stela_c_build::linux_build::out_dir(),
        &[
            Path::new("../.."),
            compiler.parent().expect("compiler root"),
        ],
    )
    .expect("i915 source placement");
    let captured = initcalls::compile(
        &build,
        Path::new("../stela-linux/src/compat/stela/registration.h"),
        INITCALLS,
        &placement,
    );
    let more = [PARAMETERS, DISPLAY_PARAMETERS]
        .into_iter()
        .map(|spec| {
            let placement = initcalls::Placement::new(
                &Unit::from(spec).path(),
                &stela_c_build::linux_build::out_dir(),
                &[
                    Path::new("../.."),
                    compiler.parent().expect("compiler root"),
                ],
            )
            .expect("i915 parameters placement");
            let unit = initcalls::compile(
                &build,
                Path::new("../stela-linux/src/compat/stela/registration.h"),
                spec,
                &placement,
            );
            (spec, unit)
        })
        .collect::<Vec<_>>();
    build
        .objects(results.into_iter().flatten().flatten())
        .object(captured.object())
        .objects(more.iter().map(|(_, unit)| unit.object()))
        .compile("stela-i915");
    let archive = stela_c_build::linux_build::out_dir().join("libstela-i915.a");
    share(
        &build,
        &archive,
        others,
        &[&linux.archive, &linux.common_archive],
    );
    // Hosted programs test the environment and link no driver.
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "uefi") {
        println!("cargo:rustc-link-lib=static=stela-i915");
    }
    super::image::Driver {
        name: MODULE.name,
        spec: INITCALLS,
        captured,
        more,
        archive,
        open: OPEN,
    }
}

/// Refuse a Makefile whose rules or variables are not the ones this build
/// carries out.
fn check_makefile(kbuild: &Kbuild) {
    let rules = RULES
        .iter()
        .map(|(targets, prerequisites, recipe)| Rule {
            targets: (*targets).to_owned(),
            prerequisites: (*prerequisites).to_owned(),
            recipe: recipe.iter().map(|line| (*line).to_owned()).collect(),
        })
        .collect::<Vec<_>>();
    assert!(
        kbuild.rules == rules,
        "i915's Makefile has other rules: {:?}",
        kbuild.rules
    );
    assert!(
        kbuild
            .definitions
            .keys()
            .map(String::as_str)
            .eq(DESCRIPTIONS),
        "i915's Makefile has other variables: {:?}",
        kbuild.definitions.keys()
    );
    assert!(
        kbuild.defines == ["I915"],
        "i915's Makefile defines other macros: {:?}",
        kbuild.defines
    );
}

/// The global symbols each member of `archive` defines and imports.
fn symbols(archive: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    use object::{Object, ObjectSymbol, read::archive::ArchiveFile};
    let data = fs::read(archive).unwrap_or_else(|error| panic!("{}: {error}", archive.display()));
    let parsed = ArchiveFile::parse(&*data).expect("an archive");
    let (mut defined, mut imported) = (BTreeSet::new(), BTreeSet::new());
    for member in parsed.members() {
        let member = member.expect("an archive member");
        let object =
            object::File::parse(member.data(&*data).expect("member data")).expect("an object file");
        for symbol in object.symbols() {
            let Ok(name) = symbol.name() else {
                continue;
            };
            if symbol.is_undefined() {
                imported.insert(name.to_owned());
            } else if symbol.is_global() {
                defined.insert(name.to_owned());
            }
        }
    }
    let imported = &imported - &defined;
    (defined, imported)
}

/// The renames that keep i915's archive apart from the `others`: each global
/// symbol both define, and the COFF pointer stub (`.refptr.<name>`) a
/// reference to it may carry, gets [`SHARED_PREFIX`].
pub fn renames(i915: &BTreeSet<String>, others: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut renames = BTreeMap::new();
    for name in i915.intersection(others) {
        let (stub, plain) = match name.strip_prefix(".refptr.") {
            Some(plain) => (true, plain),
            None => (false, name.as_str()),
        };
        let renamed = format!("{SHARED_PREFIX}{plain}");
        renames.insert(
            name.clone(),
            if stub {
                format!(".refptr.{renamed}")
            } else {
                renamed.clone()
            },
        );
        if !stub {
            renames.insert(format!(".refptr.{plain}"), format!(".refptr.{renamed}"));
        }
    }
    renames
}

/// Renames in i915's `archive` every global symbol another driver's archive
/// also defines, and checks that afterwards no driver's import is served by
/// another driver and that i915 defines nothing the environment defines.
fn share(build: &cc::Build, archive: &Path, others: &[PathBuf], environment: &[&Path]) {
    let (defined, _) = symbols(archive);
    let mut other_defined = BTreeSet::new();
    let mut other_imported = BTreeSet::new();
    for other in others {
        let (defined, imported) = symbols(other);
        other_defined.extend(defined);
        other_imported.extend(imported);
    }
    let renames = renames(&defined, &other_defined);
    if !renames.is_empty() {
        let list = archive.with_extension("renames");
        fs::write(
            &list,
            renames
                .iter()
                .map(|(from, to)| format!("{from} {to}\n"))
                .collect::<String>(),
        )
        .expect("write i915's renames");
        stela_c_build::linux_build::run(
            Command::new(objcopy(build))
                .arg(format!("--redefine-syms={}", list.display()))
                .arg(archive),
        );
    }
    let (defined, imported) = symbols(archive);
    let shared = defined.intersection(&other_defined).collect::<Vec<_>>();
    assert!(
        shared.is_empty(),
        "i915 still defines symbols another driver defines: {shared:?}"
    );
    let served = imported
        .intersection(&other_defined)
        .chain(other_imported.intersection(&defined))
        .collect::<Vec<_>>();
    assert!(
        served.is_empty(),
        "a driver's import would be served by another driver: {served:?}"
    );
    let mut environment_defined = BTreeSet::new();
    for archive in environment {
        environment_defined.extend(symbols(archive).0);
    }
    // A COFF pointer stub to a symbol i915 only imports is a link-once copy
    // of the stub the environment's own references to it carry.
    let twice = defined
        .intersection(&environment_defined)
        .filter(|name| {
            name.strip_prefix(".refptr.")
                .is_none_or(|plain| defined.contains(plain))
        })
        .collect::<Vec<_>>();
    assert!(
        twice.is_empty(),
        "i915 defines what the environment defines: {twice:?}"
    );
}

/// LLVM's objcopy beside the archiver the build uses, else the one on the
/// path.
fn objcopy(build: &cc::Build) -> PathBuf {
    let archiver = build.get_archiver().get_program().to_owned();
    let archiver = Path::new(&archiver);
    let name = archiver
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("llvm-ar");
    match name.strip_prefix("llvm-ar") {
        Some(suffix) => archiver.with_file_name(format!("llvm-objcopy{suffix}")),
        None => PathBuf::from("llvm-objcopy"),
    }
}

/// Linux's Intel PCI device ID lists, which i915's table is written with.
const PCI_IDS: &str = "../../third_party/linux/include/drm/intel/pciids.h";
/// Write i915's PCI ID table for the Rust side (`src/i915.rs`), read from
/// the driver's own `pciidlist` and Linux's ID lists. A clippy build reads
/// it too, so it is written before any C is compiled.
pub fn tables() {
    let table = Path::new(I915).join("i915_pci.c");
    for watched in [table.as_path(), Path::new(PCI_IDS)] {
        println!("cargo:rerun-if-changed={}", watched.display());
    }
    let ids = fs::read_to_string(PCI_IDS).expect("read Linux's Intel PCI IDs");
    let devices = i915_tables::devices(
        &fs::read_to_string(&table).expect("read i915's PCI table"),
        &ids,
    )
    .unwrap_or_else(|error| panic!("i915's PCI ID table: {error}"));
    let mut text = String::from(
        "/// i915's `pciidlist`: each PCI device ID with its descriptor's\n\
         /// `dma_mask_size`, `require_force_probe`, graphics IP version,\n\
         /// platform and `is_dgfx`.\n\
         pub(crate) const DEVICES: &[(u16, u8, bool, u8, &str, bool)] = &[\n",
    );
    for (device, described) in &devices {
        text.push_str(&format!(
            "    ({device:#06x}, {}, {}, {}, {:?}, {}),\n",
            described.bits,
            described.forced,
            described.version,
            described.platform,
            described.discrete
        ));
    }
    text.push_str("];\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"));
    fs::write(output.join("i915_devices.rs"), text).expect("write i915's device table");
}
