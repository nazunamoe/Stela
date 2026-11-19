//! xe: the `xe-y` composite of its own Makefile with the display code it
//! shares with i915, compiled unchanged in Stela's Linux environment, and
//! the kernel services it imports that the environment does not yet supply.
use super::kbuild::{Config, Kbuild, Rule};
use super::native::{self, Linux};
use super::xe_tables::{devices, display_firmware, uc_firmware};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
};
use stela_c_build::linux::Module;
use stela_c_build::linux_capture::{self as initcalls, Unit, UnitSpec};

const XE: &str = "../../third_party/linux/drivers/gpu/drm/xe";
const AUTOCONF: &str = "../stela-linux/src/compat/generated/autoconf.h";
const MODULE: Module<'static> = Module {
    name: "xe",
    file: "drivers/gpu/drm/xe/xe",
};
const COMPOSITE: &str = "xe";

/// The Makefile's rule that builds the display code shared with i915 from
/// i915's tree: an object below `i915-display/` is that tree's source.
const SHARED_DISPLAY: (&str, &str) = ("i915-display/", "drivers/gpu/drm/i915/display");
/// The host program the Makefile builds, and each rule file it turns into a
/// table of workarounds below `generated/`.
const GENERATOR: &str = "xe_gen_wa_oob";
const GENERATED: [&str; 2] = ["xe_wa_oob", "xe_device_wa_oob"];

/// The Makefile's rules, each carried out by [`port`] or by nothing: the
/// header self-test runs only with `DRM_XE_WERROR`, and a dependency of the
/// objects on the generated headers is this build's order.
const RULES: [(&str, &str, &[&str]); 5] = [
    (
        "$(obj)/generated/%_wa_oob.c $(obj)/generated/%_wa_oob.h",
        "$(obj)/xe_gen_wa_oob $(src)/xe_wa_oob.rules",
        &["$(call cmd,wa_oob)"],
    ),
    (
        "$(obj)/generated/%_device_wa_oob.c $(obj)/generated/%_device_wa_oob.h",
        "$(obj)/xe_gen_wa_oob $(src)/xe_device_wa_oob.rules",
        &["$(call cmd,device_wa_oob)"],
    ),
    (
        "$(obj)/i915-display/%.o",
        "$(srctree)/drivers/gpu/drm/i915/display/%.c FORCE",
        &[
            "$(call cmd,force_checksrc)",
            "$(call if_changed_rule,cc_o_c)",
        ],
    ),
    (
        "$(obj)/%.hdrtest",
        "$(src)/%.h FORCE",
        &["$(call if_changed_dep,hdrtest)"],
    ),
    (
        "$(uses_generated_oob)",
        "$(obj)/generated/xe_wa_oob.h $(obj)/generated/xe_device_wa_oob.h",
        &[],
    ),
];
/// The Makefile's variables without a selector that this build carries out,
/// as it writes them.
const DEFINITIONS: [(&str, &str); 5] = [
    (
        "cmd_device_wa_oob",
        "mkdir -p $(@D); $(obj)/xe_gen_wa_oob $(src)/xe_device_wa_oob.rules \
         $(generated_device_oob)",
    ),
    (
        "cmd_wa_oob",
        "mkdir -p $(@D); $(obj)/xe_gen_wa_oob $(src)/xe_wa_oob.rules $(generated_oob)",
    ),
    (
        "generated_device_oob",
        "$(obj)/generated/xe_device_wa_oob.c $(obj)/generated/xe_device_wa_oob.h",
    ),
    (
        "generated_oob",
        "$(obj)/generated/xe_wa_oob.c $(obj)/generated/xe_wa_oob.h",
    ),
    ("hostprogs", GENERATOR),
];
/// Its other variables, which name a command in Kbuild's output, belong to
/// the header self-test or list what depends on the generated headers.
const DESCRIPTIONS: [&str; 6] = [
    "cmd_hdrtest",
    "hdrtest_find_args",
    "quiet_cmd_device_wa_oob",
    "quiet_cmd_hdrtest",
    "quiet_cmd_wa_oob",
    "uses_generated_oob",
];

/// The kernel services the composite imports that Stela's Linux environment
/// lacks, or defines only as a trap, by service. Building fails when the
/// imports change, so each supplied service leaves this list with its change.
const OPEN: super::image::Open = &[
    // Nothing tears a pseudo filesystem's superblock down.
    (
        "reopening a file without a VFS path",
        &["stela_linux_reopen_unowned_path"],
    ),
];

/// The unit of xe's own `module_init`. It registers after i915's (33), so
/// xe binds a device both tables hold only when i915's probe fails.
/// Ordinal 37 is xe's display parameters' ([`DISPLAY_PARAMETERS`]).
const INITCALLS: UnitSpec = UnitSpec {
    scope: stela_c_build::linux_capture::Scope {
        slice: "gpu",
        group: "xe",
        group_ordinal: 36,
        unit_ordinal: 1,
        global_ordinal: 36,
    },
    source: "drivers/gpu/drm/xe/xe_module",
    module: MODULE.name,
    file: MODULE.file,
};
fn initcalls_unit() -> Unit {
    INITCALLS.into()
}

/// The unit of the display parameters xe shares with i915
/// (`xe.vbt_firmware` among them), captured so the command line sets them,
/// as in Linux, where they are the module `xe`'s.
const DISPLAY_PARAMETERS: UnitSpec = UnitSpec {
    scope: stela_c_build::linux_capture::Scope {
        slice: "gpu",
        group: "xe",
        group_ordinal: 36,
        unit_ordinal: 2,
        global_ordinal: 37,
    },
    source: "drivers/gpu/drm/i915/display/intel_display_params",
    module: MODULE.name,
    file: MODULE.file,
};

pub fn port(linux: &Linux) -> super::image::Driver {
    let config = Config::parse(&fs::read_to_string(AUTOCONF).expect("read Stela's Kconfig header"))
        .unwrap_or_else(|error| panic!("{AUTOCONF}: {error}"));
    let kbuild = Kbuild::read(Path::new(XE), &config).unwrap_or_else(|error| panic!("{error}"));
    for file in &kbuild.files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    check_makefile(&kbuild);
    let output = stela_c_build::linux_build::out_dir().join("xe");
    generate(&output);
    let sources = kbuild
        .objects(COMPOSITE)
        .unwrap_or_else(|error| panic!("{error}"))
        .iter()
        .map(|object| source(object))
        .filter(|source| {
            *source != initcalls_unit().path() && *source != Unit::from(DISPLAY_PARAMETERS).path()
        })
        .collect::<Vec<_>>();
    assert!(
        kbuild.generated_include && !kbuild.werror,
        "xe's Makefile no longer searches its output directory, or selects -Werror"
    );
    let mut includes = vec![output.clone()];
    includes.extend(kbuild.includes.iter().cloned());
    includes.extend(
        kbuild
            .tree_includes
            .iter()
            .map(|directory| Path::new(native::LINUX).join(directory)),
    );
    let mut build = stela_c_build::native::configure();
    // The output directory is written by every build, so it is not watched.
    linux
        .environment
        .profile(&mut build, None, &includes, Some(&MODULE));
    for directory in [Path::new(XE).to_path_buf(), source("i915-display/x.o")] {
        let directory = directory.parent().unwrap_or(&directory);
        println!("cargo:rerun-if-changed={}", directory.display());
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
    let objects = thread::scope(|scope| {
        sources
            .chunks(sources.len().div_ceil(jobs))
            .map(|chunk| {
                let build = &build;
                scope.spawn(move || build.clone().files(chunk).compile_intermediates())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|compiling| compiling.join().expect("a compiling thread"))
            .collect::<Vec<_>>()
    });
    let compiler = initcalls::compiler(&build);
    let placement = initcalls::Placement::new(
        &initcalls_unit().path(),
        &stela_c_build::linux_build::out_dir(),
        &[
            Path::new("../.."),
            compiler.parent().expect("compiler root"),
        ],
    )
    .expect("xe source placement");
    let captured = initcalls::compile(
        &build,
        Path::new("../stela-linux/src/compat/stela/registration.h"),
        INITCALLS,
        &placement,
    );
    let parameters_placement = initcalls::Placement::new(
        &Unit::from(DISPLAY_PARAMETERS).path(),
        &stela_c_build::linux_build::out_dir(),
        &[
            Path::new("../.."),
            compiler.parent().expect("compiler root"),
        ],
    )
    .expect("xe display parameters placement");
    let parameters = initcalls::compile(
        &build,
        Path::new("../stela-linux/src/compat/stela/registration.h"),
        DISPLAY_PARAMETERS,
        &parameters_placement,
    );
    build
        .objects(objects)
        .object(captured.object())
        .object(parameters.object())
        .compile("stela-xe");
    // Hosted programs test the environment and link no driver.
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "uefi") {
        println!("cargo:rustc-link-lib=static=stela-xe");
    }
    super::image::Driver {
        name: MODULE.name,
        spec: INITCALLS,
        captured,
        more: vec![(DISPLAY_PARAMETERS, parameters)],
        archive: stela_c_build::linux_build::out_dir().join("libstela-xe.a"),
        open: OPEN,
    }
}

/// An object's source: i915's tree for the display code xe shares with it,
/// xe's own otherwise.
fn source(object: &str) -> PathBuf {
    match object.strip_prefix(SHARED_DISPLAY.0) {
        Some(shared) => Path::new(native::LINUX).join(SHARED_DISPLAY.1).join(shared),
        None => Path::new(XE).join(object),
    }
    .with_extension("c")
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
        "xe's Makefile has other rules: {:?}",
        kbuild.rules
    );
    let mut names = DEFINITIONS
        .iter()
        .map(|(name, _)| *name)
        .chain(DESCRIPTIONS)
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert!(
        kbuild.definitions.keys().map(String::as_str).eq(names),
        "xe's Makefile has other variables: {:?}",
        kbuild.definitions.keys()
    );
    for (name, value) in DEFINITIONS {
        assert!(
            kbuild.definitions[name] == value,
            "xe's Makefile changed {name}: {}",
            kbuild.definitions[name]
        );
    }
}

/// Build the Makefile's host program for this machine and run it over each
/// rule file, as `cmd_wa_oob` and `cmd_device_wa_oob` do.
fn generate(output: &Path) {
    let generated = output.join("generated");
    fs::create_dir_all(&generated).expect("create xe's generated directory");
    let program = output.join(GENERATOR);
    let source = Path::new(XE).join(GENERATOR).with_extension("c");
    println!("cargo:rerun-if-changed={}", source.display());
    let host = env::var("HOST").expect("Cargo supplies HOST");
    let mut compiler = cc::Build::new();
    compiler
        .target(&host)
        .host(&host)
        .opt_level(1)
        .cargo_metadata(false);
    stela_c_build::linux_build::run(
        compiler
            .get_compiler()
            .to_command()
            .arg(&source)
            .arg("-o")
            .arg(&program),
    );
    for name in GENERATED {
        let rules = Path::new(XE).join(name).with_extension("rules");
        println!("cargo:rerun-if-changed={}", rules.display());
        stela_c_build::linux_build::run(
            Command::new(&program)
                .arg(&rules)
                .arg(generated.join(name).with_extension("c"))
                .arg(generated.join(name).with_extension("h")),
        );
    }
}

/// Linux's Intel PCI device ID lists, which xe's table is written with.
const PCI_IDS: &str = "../../third_party/linux/include/drm/intel/pciids.h";

/// Write xe's PCI ID table for the Rust side (`src/xe.rs`): each device ID
/// with the DMA mask bits of its platform's descriptor and whether that
/// descriptor binds only by force probe, read from the
/// driver's own `pciidlist` and Linux's ID lists. A clippy build reads it
/// too, so it is written before any C is compiled.
pub fn tables() {
    let table = Path::new(XE).join("xe_pci.c");
    for watched in [table.as_path(), Path::new(PCI_IDS)] {
        println!("cargo:rerun-if-changed={}", watched.display());
    }
    let devices = devices(
        &fs::read_to_string(&table).expect("read xe's PCI table"),
        &fs::read_to_string(PCI_IDS).expect("read Linux's Intel PCI IDs"),
    )
    .unwrap_or_else(|error| panic!("xe's PCI ID table: {error}"));
    let mut text = String::from(
        "/// xe's `pciidlist`: each PCI device ID with its platform's `dma_mask_size`\n\
         /// and `require_force_probe`.\n\
         pub(crate) const DEVICES: &[(u16, u8, bool)] = &[\n",
    );
    for (device, (bits, forced, ..)) in &devices {
        text.push_str(&format!("    ({device:#06x}, {bits}, {forced}),\n"));
    }
    text.push_str(
        "];\n/// Each device ID's platform name (`PLATFORM()`), `has_display`,\n\
         /// `is_dgfx` and whether the descriptor fixes its graphics IP\n\
         /// (`pre_gmdid_graphics_ip`).\n\
         pub(crate) const PLATFORMS: &[(u16, &str, bool, bool, bool)] = &[\n",
    );
    for (device, (_, _, platform, display, discrete, fixed)) in &devices {
        text.push_str(&format!(
            "    ({device:#06x}, {platform:?}, {display}, {discrete}, {fixed}),\n"
        ));
    }
    text.push_str("];\n");
    let uc_table = Path::new(XE).join("xe_uc_fw.c");
    let dmc_table = Path::new(native::LINUX).join(DMC);
    for watched in [uc_table.as_path(), dmc_table.as_path()] {
        println!("cargo:rerun-if-changed={}", watched.display());
    }
    let firmware = uc_firmware(&fs::read_to_string(&uc_table).expect("read xe's firmware table"))
        .unwrap_or_else(|error| panic!("xe's firmware table: {error}"));
    text.push_str(
        "/// `xe_uc_fw.c`: each platform's GuC, HuC and GSC file, as xe names\n\
         /// them in its `MODULE_FIRMWARE` declarations and asks for them.\n\
         pub(crate) const FIRMWARE: &[(&str, [Option<&str>; 3])] = &[\n",
    );
    for (platform, files) in &firmware {
        text.push_str(&format!("    ({platform:?}, {files:?}),\n"));
    }
    let display = display_firmware(&fs::read_to_string(&dmc_table).expect("read intel_dmc.c"))
        .unwrap_or_else(|error| panic!("the display code's firmware table: {error}"));
    text.push_str(
        "];\n/// `intel_dmc.c`: the display microcontroller programs it declares\n\
         /// for display version 12 and later, the displays of xe's platforms.\n\
         pub(crate) const DISPLAY_FIRMWARE: &[&str] = &[\n",
    );
    for name in &display {
        text.push_str(&format!("    {name:?},\n"));
    }
    text.push_str("];\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"));
    fs::write(output.join("xe_devices.rs"), text).expect("write xe's device table");
}

/// The display code's firmware selection, in i915's tree.
const DMC: &str = "drivers/gpu/drm/i915/display/intel_dmc.c";
