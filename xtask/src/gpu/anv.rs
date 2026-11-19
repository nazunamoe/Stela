//! ANV on xe and on i915 without a GPU, on the build host: Mesa built for
//! this machine with the image's ANV options, its objects linked into a test
//! program as Mesa links its driver, with Stela's libdrm stand-in
//! (`compat/mesa/drm.c`) in libdrm's place and Stela's NIR options hook,
//! run over a render node that answers the kernel driver's queries and
//! accepts its binds and submissions without running them: xe's
//! (`intel/hosted/xe_noop_drm_shim.c`) or Mesa's own i915 shim
//! (`intel/hosted/i915_noop_drm_shim.c`), on Mesa's DRM shim. For each
//! device each driver binds, the program creates ANV's device, builds a
//! compute pipeline through BRW from SPIR-V and one from NIR, as the Metal
//! device passes the AIR compiler's NIR, and submits one dispatch
//! (`intel/tests/anv_device.c`); every ioctl ANV made must be one Stela's
//! driver serves on a render node. It also counts the devices where ANV
//! meets the Metal device's Vulkan requirements and reads and writes Metal's
//! tier 2 read-write texture formats.
//!
//! Needs what ANV's image build needs (`clc.rs`: mesa_clc's LLVM, Clang,
//! SPIRV-LLVM-Translator and SPIRV-Tools, and glslang) and the host's
//! `libdrm.so.2`, whose mode calls the display WSI units take; the image
//! leaves those units out.
use super::hosted;
use super::support::{build_directory, gpu, third_party, tool};
use super::toolchain::GENERATORS;
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// xe's own tables, read as its build reads them.
#[allow(dead_code)]
#[path = "../../../crates/stela-driver-gpu/build/xe_tables.rs"]
mod xe_tables;

/// i915's own tables, read as its build reads them.
#[allow(dead_code)]
#[path = "../../../crates/stela-driver-gpu/build/i915_tables.rs"]
mod i915_tables;

/// A kernel driver ANV runs over in the test.
struct Kernel {
    name: &'static str,
    /// Its no-op render node, under `crates/stela-driver-gpu/src`.
    shim: &'static str,
    /// Where it registers its ioctls, under Linux's tree, and their prefix.
    ioctls: (&'static str, &'static str),
}

const XE: Kernel = Kernel {
    name: "xe",
    shim: "intel/hosted/xe_noop_drm_shim.c",
    ioctls: ("drivers/gpu/drm/xe/xe_device.c", "XE_"),
};
const I915: Kernel = Kernel {
    name: "i915",
    shim: "intel/hosted/i915_noop_drm_shim.c",
    ioctls: ("drivers/gpu/drm/i915/i915_driver.c", "I915_"),
};

/// Mesa's own tables for ANV, read as the image's build reads them for the
/// drivers' admission (`crates/stela-driver-gpu/src/anv.rs`).
#[path = "../../../crates/stela-driver-gpu/build/anv_tables.rs"]
mod anv_tables;

/// A device of a kernel driver's table that Mesa's Intel table lists, as
/// the test treats it.
struct Device {
    id: u16,
    /// The kernel driver's platform and Mesa's device family, for the log.
    platform: String,
    family: String,
    /// The kernel driver's `is_dgfx`: Vulkan must report a discrete GPU.
    discrete: bool,
    /// Whether ANV creates a physical device for it (`anv_tables::opened`),
    /// which is the drivers' admission: the test runs ANV on it. One it
    /// does not open is handed to no driver, and the test checks that ANV
    /// gives no Vulkan device for it.
    opened: bool,
}

/// The devices of one kernel driver's table the test covers, and how many
/// of the table Mesa's Intel table does not list at all, by what the kernel
/// driver calls them: those are handed to no driver, and no shim describes
/// them, so they are not run.
#[derive(Default)]
struct Devices {
    listed: Vec<Device>,
    unlisted: std::collections::BTreeMap<String, usize>,
}

/// Mesa's Intel device table with each device's graphics version, and the
/// set ANV opens.
struct Mesa {
    listed: std::collections::BTreeMap<u16, anv_tables::Listed>,
    opened: Vec<u16>,
}

fn read(path: PathBuf) -> Result<String> {
    fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
}

fn mesa() -> Result<Mesa> {
    let mesa = third_party("mesa");
    let listed = anv_tables::devices(
        &read(mesa.join("include/pci_ids/iris_pci_ids.h"))?,
        &read(mesa.join("src/intel/dev/intel_device_info.c"))?,
    )
    .map_err(anyhow::Error::msg)?;
    let versions =
        anv_tables::anv_versions(&read(mesa.join("src/intel/vulkan/anv_physical_device.c"))?)
            .map_err(anyhow::Error::msg)?;
    let opened = anv_tables::opened(&listed, versions);
    Ok(Mesa { listed, opened })
}

impl Mesa {
    /// Files `id` of a kernel driver's table under `devices`; `unlisted`
    /// is what to count it under when Mesa's table does not list it.
    fn file(
        &self,
        devices: &mut Devices,
        id: u16,
        platform: String,
        discrete: bool,
        unlisted: String,
    ) {
        match self.listed.get(&id) {
            None => *devices.unlisted.entry(unlisted).or_default() += 1,
            Some(listed) => devices.listed.push(Device {
                id,
                platform,
                family: listed.family.clone(),
                discrete,
                opened: self.opened.binary_search(&id).is_ok(),
            }),
        }
    }
}

/// Every device of xe's PCI ID table whose platform xe's firmware table
/// gives a GuC image: xe's probe ends without one, so the others are handed
/// to no driver whatever ANV says of them (`crate::xe::admits` in
/// stela-driver-gpu).
fn xe_devices(mesa: &Mesa) -> Result<Devices> {
    let linux = third_party("linux");
    let table = xe_tables::devices(
        &read(linux.join("drivers/gpu/drm/xe/xe_pci.c"))?,
        &read(linux.join("include/drm/intel/pciids.h"))?,
    )
    .map_err(|error| anyhow::anyhow!("xe's PCI ID table: {error}"))?;
    let firmware = xe_tables::uc_firmware(&read(linux.join("drivers/gpu/drm/xe/xe_uc_fw.c"))?)
        .map_err(|error| anyhow::anyhow!("xe's firmware table: {error}"))?;
    let mut devices = Devices::default();
    for (id, (_, _, platform, _, discrete, _)) in table {
        if firmware
            .get(&platform)
            .is_some_and(|files| files[0].is_some())
        {
            mesa.file(&mut devices, id, platform.clone(), discrete, platform);
        }
    }
    ensure!(
        devices.listed.iter().any(|device| device.opened),
        "ANV opens no device of xe's table"
    );
    Ok(devices)
}

/// The first graphics version i915 leaves to xe (`XE_VERSION` in
/// stela-driver-gpu's `i915.rs`).
const XE_VERSION: u8 = 12;

/// The devices of i915's PCI ID table that i915 binds without force probe
/// and xe's table does not hold (those are xe's).
fn i915_devices(mesa: &Mesa) -> Result<Devices> {
    let linux = third_party("linux");
    let ids = read(linux.join("include/drm/intel/pciids.h"))?;
    let xe = xe_tables::devices(&read(linux.join("drivers/gpu/drm/xe/xe_pci.c"))?, &ids)
        .map_err(|error| anyhow::anyhow!("xe's PCI ID table: {error}"))?;
    let table = i915_tables::devices(&read(linux.join("drivers/gpu/drm/i915/i915_pci.c"))?, &ids)
        .map_err(|error| anyhow::anyhow!("i915's PCI ID table: {error}"))?;
    let mut devices = Devices::default();
    for (id, described) in table {
        if described.forced || xe.contains_key(&id) {
            continue;
        }
        ensure!(
            described.version < XE_VERSION,
            "i915 binds {id:04x}, of graphics version {}, which xe's table does not hold",
            described.version
        );
        mesa.file(
            &mut devices,
            id,
            described.platform,
            described.discrete,
            format!("graphics version {}", described.version),
        );
    }
    ensure!(
        devices.listed.iter().any(|device| device.opened),
        "ANV opens no device of i915's table"
    );
    Ok(devices)
}

/// The driver's link target, whose inputs the test program links.
const DRIVER: &str = "src/intel/vulkan/libvulkan_intel.so";
/// One of ANV's own units and one of the DRM shim's, whose flags the test's
/// sources compile with.
const ANV_MODEL: &str = "src/intel/vulkan/anv_util.c";
const SHIM_MODEL: &str = "src/drm-shim/device.c";
/// The shim's objects as Meson builds them, and the archives it links.
const SHIM_OBJECTS: [&str; 2] = [
    "src/drm-shim/libdrm_shim.a.p/drm_shim.c.o",
    "src/drm-shim/libdrm_shim.a.p/device.c.o",
];
const SHIM_ARCHIVES: [&str; 5] = [
    "src/intel/dev/libintel_dev.a",
    "src/util/libmesa_util.a",
    "src/c11/impl/libmesa_util_c11.a",
    "src/util/blake3/libblake3.a",
    "src/util/libmesa_util_simd.a",
];

pub fn test(jobs: u16) -> Result<()> {
    let directory = build_directory("anv-host");
    let build = directory.join("build");
    let output = directory.join("test");
    let bin = directory.join("bin");
    let packages = directory.join("packages");
    let local = crate::tools::root().join(".tools/gpu");
    let host = std::env::join_paths(
        [local.join(GENERATORS), local.join("rust/bin")]
            .into_iter()
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            )),
    )?;
    let tools = match super::clc::tools(Path::new(&host), jobs)? {
        Ok(tools) => tools,
        Err(reason) => bail!("ANV cannot be built on this host: {reason}"),
    };
    for path in [&output, &bin, &packages] {
        fs::create_dir_all(path)?;
    }
    for program in &tools.programs {
        fs::copy(program, bin.join(program.file_name().context("host tool")?))?;
    }
    let path =
        std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(&host)))?;
    // libdrm's headers as the image has them; its calls are Stela's.
    fs::write(
        packages.join("libdrm.pc"),
        format!(
            "Name: libdrm\nDescription: vendored headers\nVersion: {}\n\
             Cflags: -I{1} -I{1}/include/drm\n",
            super::nvk::LIBDRM_VERSION,
            super::support::slashed(&third_party("libdrm"))
        ),
    )?;
    ensure!(
        third_party("libdrm").join("xf86drm.h").is_file(),
        "third_party/libdrm is missing; run `cargo xtask gpu fetch`"
    );
    let arguments = hosted::arguments("intel", &super::nvk::ANV_OPTIONS);
    hosted::configure(&directory, &build, &packages, &path, &arguments)?;
    let link = hosted::link_command(&build, DRIVER)?;
    let mut targets = hosted::built_inputs(&link);
    targets.extend(SHIM_OBJECTS.map(str::to_owned));
    targets.extend(SHIM_ARCHIVES.map(str::to_owned));
    crate::tools::exec(
        Command::new(tool("ninja", GENERATORS)?)
            .arg("-C")
            .arg(&build)
            .arg(format!("-j{jobs}"))
            .args(&targets)
            .env("PATH", &path),
    )?;
    let (shims, program) = link_test(&build, &output, &link, &tools.glslang.0)?;
    let mut failures = Vec::new();
    let mesa = mesa()?;
    let xe = xe_devices(&mesa)?;
    run(&XE, &xe, &program, &shims[0], &output, &mut failures)?;
    let i915 = i915_devices(&mesa)?;
    run(&I915, &i915, &program, &shims[1], &output, &mut failures)?;
    ensure!(
        failures.is_empty(),
        "ANV failed on the host (logs in {}):\n{}",
        output.display(),
        failures.join("\n")
    );
    Ok(())
}

/// Runs the test program over `kernel`'s shim for each of `devices`, logging
/// each run's output beside the program, and adds each failure to
/// `failures`.
fn run(
    kernel: &Kernel,
    devices: &Devices,
    program: &Path,
    shim: &Path,
    output: &Path,
    failures: &mut Vec<String>,
) -> Result<()> {
    let (registry, prefix) = kernel.ioctls;
    let served = hosted::render_ioctls(
        &fs::read_to_string(third_party("linux").join(registry))?,
        prefix,
    );
    ensure!(
        !served.is_empty(),
        "{registry} serves no ioctl on a render node"
    );
    let marker = format!("{} shim: ioctls", kernel.name);
    let (mut run, mut metal, mut read_write, mut refused) = (0, 0, 0, 0);
    // Each Mesa family's devices, and how many met the Metal device's
    // requirements and the tier 2 formats.
    let mut families = std::collections::BTreeMap::<String, (usize, usize, usize)>::new();
    for device in &devices.listed {
        let id = format!("{:04x}", device.id);
        let name = format!("{id} {} ({})", device.platform, device.family);
        let result = Command::new(program)
            .args([
                id.as_str(),
                match (device.opened, device.discrete) {
                    (false, _) => "none",
                    (true, true) => "discrete",
                    (true, false) => "integrated",
                },
                kernel.name,
            ])
            .env("LD_PRELOAD", shim)
            .env("INTEL_STUB_GPU_DEVICE_ID", &id)
            .stdin(Stdio::null())
            .output()?;
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        fs::write(
            output.join(format!("{}-{id}.log", kernel.name)),
            format!("{stdout}{stderr}"),
        )?;
        if !device.opened {
            // Handed to no driver: ANV must give it no Vulkan device.
            if result.status.success() && stdout.contains(NO_DEVICE) {
                refused += 1;
            } else {
                failures.push(format!(
                    "{name} is handed to no driver, but ANV on {} did not refuse it ({}): {}",
                    kernel.name,
                    result.status,
                    stderr.trim()
                ));
            }
            continue;
        }
        let used = hosted::used_ioctls(&stderr, &marker);
        let unserved: Vec<_> = used
            .keys()
            .filter(|ioctl| !served.contains(ioctl))
            .cloned()
            .collect();
        match (
            result.status.success(),
            used.is_empty(),
            unserved.is_empty(),
        ) {
            (true, false, true) => {
                run += 1;
                let met = stdout.contains(METAL_MET);
                let formats = stdout.contains(READ_WRITE_MET);
                metal += usize::from(met);
                read_write += usize::from(formats);
                let family = families.entry(device.family.clone()).or_default();
                family.0 += 1;
                family.1 += usize::from(met);
                family.2 += usize::from(formats);
                println!("{name}: {}", stdout.trim().replace('\n', "; "));
                println!(
                    "  {} ioctls: {}",
                    kernel.name,
                    used.iter()
                        .map(|(ioctl, count)| format!("{ioctl} {count}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            (true, _, false) => failures.push(format!(
                "{name}: ANV used ioctls Stela's {} does not serve on a render node: {}",
                kernel.name,
                unserved.join(", ")
            )),
            _ => failures.push(format!(
                "{name} on {} failed with {}: {}",
                kernel.name,
                result.status,
                stderr.trim()
            )),
        }
    }
    for (family, (count, met, formats)) in &families {
        println!(
            "Mesa family {family} on {}: {count} devices, Metal requirements met on {met}, \
             tier 2 read-write formats on {formats}",
            kernel.name
        );
    }
    let opened = devices.listed.iter().filter(|device| device.opened).count();
    println!(
        "ANV ran on {0}'s ioctls for {run} of the {opened} devices {0} is handed, with no GPU; \
         the Metal device's Vulkan requirements are met on {metal} of them, and Metal's tier 2 \
         read-write texture formats on {read_write}",
        kernel.name
    );
    // A device handed to a driver must open the Metal device: the drivers'
    // admission claims no other.
    if metal != opened || read_write != opened {
        failures.push(format!(
            "{} is handed {opened} devices, of which {metal} meet the Metal device's \
             requirements and {read_write} its tier 2 read-write formats",
            kernel.name
        ));
    }
    let left_out = devices.listed.len() - opened;
    println!(
        "{0} is not handed {left_out} more devices of its table that Mesa lists: ANV gave no \
         Vulkan device for {refused} of them",
        kernel.name
    );
    for (what, count) in &devices.unlisted {
        println!(
            "{} is not handed {count} devices of {what}: Mesa's Intel table does not list them",
            kernel.name
        );
    }
    Ok(())
}

/// What the test program prints when ANV gives no Vulkan device.
const NO_DEVICE: &str = "no Vulkan device from ANV";
/// What the test program prints for a device the Metal device admits.
const METAL_MET: &str = "Metal device: every requirement met";
/// What it prints for a device that reads and writes every format of
/// Metal's read-write texture tier 2.
const READ_WRITE_MET: &str = "Metal read-write textures: every tier 2 format read and written";

/// Builds the xe and i915 shims and the test program; their paths.
fn link_test(
    build: &Path,
    output: &Path,
    link: &[String],
    glslang: &Path,
) -> Result<(Vec<PathBuf>, PathBuf)> {
    let sources = gpu();
    crate::tools::exec(
        Command::new(glslang)
            .args([
                "-V",
                "--target-env",
                "vulkan1.1",
                "--vn",
                "stela_anv_device_spirv",
            ])
            .arg("-o")
            .arg(output.join("anv_device_shader.h"))
            .arg(sources.join("intel/tests/anv_device.comp"))
            .stdout(Stdio::null()),
    )?;
    // The DRM shim's stat and stat64 come first (see the xe shim's source).
    let renamed = output.join("drm_shim.o");
    crate::tools::exec(
        Command::new("objcopy")
            .args([
                "--redefine-sym",
                "stat=stela_drm_shim_stat",
                "--redefine-sym",
                "stat64=stela_drm_shim_stat64",
            ])
            .arg(build.join(SHIM_OBJECTS[0]))
            .arg(&renamed),
    )?;
    let mut shims = Vec::new();
    for kernel in [&XE, &I915] {
        let stem = Path::new(kernel.shim).file_stem().context("shim name")?;
        let shim_object = output.join(stem).with_extension("o");
        crate::tools::exec(
            hosted::compile_command(build, SHIM_MODEL, &sources.join(kernel.shim), &shim_object)?
                .args(["-fPIC", "-Isrc/intel", "-Isrc/intel/dev"])
                .arg(format!(
                    "-I{}",
                    third_party("mesa").join("src/intel").display()
                )),
        )?;
        let shim = output.join(stem).with_extension("so");
        crate::tools::exec(
            Command::new("cc")
                .current_dir(build)
                .arg("-shared")
                .arg("-o")
                .arg(&shim)
                .arg(&shim_object)
                .arg(&renamed)
                .arg(SHIM_OBJECTS[1])
                .args(SHIM_ARCHIVES)
                .args(["-ldl", "-lpthread", "-lm"]),
        )?;
        shims.push(shim);
    }
    let libc = format!("-idirafter{}", sources.join("compat/libc").display());
    let units = [
        (
            "compat/mesa/anv_nir_options.c",
            vec!["-Wno-missing-prototypes".to_owned()],
        ),
        // Exported, so the shim's own libdrm calls find them.
        (
            "compat/mesa/drm.c",
            vec![libc.clone(), "-fvisibility=default".to_owned()],
        ),
        (
            "intel/tests/anv_nir_shader.c",
            vec!["-Wno-missing-prototypes".to_owned()],
        ),
        (
            "intel/tests/anv_device.c",
            vec![libc, format!("-I{}", output.display())],
        ),
    ];
    let mut objects = Vec::new();
    for (source, flags) in units {
        let object = output.join(
            Path::new(source)
                .with_extension("o")
                .file_name()
                .context("unit name")?,
        );
        crate::tools::exec(
            hosted::compile_command(build, ANV_MODEL, &sources.join(source), &object)?.args(flags),
        )?;
        objects.push(object);
    }
    let libdrm = [hosted::host_libdrm()?.display().to_string()];
    let program = output.join("anv_device");
    let words = hosted::program_link(link, &program, &objects, &libdrm);
    crate::tools::exec(Command::new(&words[0]).current_dir(build).args(&words[1..]))?;
    Ok((shims, program))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_build_keeps_the_images_options_but_its_own() {
        let arguments = hosted::arguments("intel", &super::super::nvk::ANV_OPTIONS);
        assert!(arguments.contains(&"-Dvulkan-drivers=intel".to_owned()));
        assert!(arguments.contains(&"-Dintel-rt=disabled".to_owned()));
        assert!(arguments.contains(&"-Dtools=drm-shim".to_owned()));
    }

    /// The test program checks exactly the Metal device's Vulkan
    /// requirements, and the dma-buf import its native backend asks for, so
    /// a device it passes is one the Metal device opens.
    #[test]
    fn the_programs_requirements_are_the_metal_devices() {
        let read = |path: &str| {
            fs::read_to_string(crate::tools::root().join(path))
                .unwrap_or_else(|error| panic!("{path}: {error}"))
        };
        // The first string after each `marker` in the part of `text` from
        // `start` to `end`.
        let names = |text: &str, start: &str, end: &str, marker: &str| -> Vec<String> {
            let body = &text[text.find(start).expect(start) + start.len()..];
            let body = &body[..body.find(end).expect(end)];
            body.split(marker)
                .skip(1)
                .map(|part| {
                    let part = &part[part.find('"').expect("a name") + 1..];
                    part[..part.find('"').expect("a name")].to_owned()
                })
                .collect()
        };
        let metal = names(
            &read("crates/stela-driver-gpu/src/metal/vulkan/backend.rs"),
            "pub fn requirements(",
            "\n    ]\n}",
            "requirement(",
        );
        assert_eq!(metal.len(), 18, "{metal:?}");
        let mut program = names(
            &read("crates/stela-driver-gpu/src/intel/tests/anv_device.c"),
            "} requirements[] = {",
            "};",
            "{",
        );
        assert_eq!(
            program.pop().as_deref(),
            Some("VK_EXT_external_memory_dma_buf")
        );
        assert_eq!(program, metal);
    }

    /// The readers over forms of Mesa's tables: a family's graphics
    /// version is the last its initializer states once its macros are
    /// written out, a force-probed device is not opened, and ANV's bounds
    /// are its own two checks.
    #[test]
    fn mesas_tables_say_which_devices_anv_opens() {
        let ids = "CHIPSET(0x1616, bdw_gt2, \"BDW GT2\", \"Intel(R) HD Graphics 5500\")\n\
                   CHIPSET(0x1916, skl_gt2, \"SKL GT2\", \"Intel(R) HD Graphics 520\")\n\
                   /* a comment */\n\
                   CHIPSET(0x9a49, tgl_gt2, \"TGL GT2\", \"Intel(R) Iris(R) Xe Graphics\")\n\
                   CHIPSET(0xd750, nvl_p, \"NVL\", \"Intel(R) Graphics\", FORCE_PROBE)\n";
        let info = "#define GFX8_FEATURES \\\n   .ver = 8, \\\n   .has_64bit = true\n\
                    #define GFX9_FEATURES \\\n   GFX8_FEATURES, \\\n   .ver = 9\n\
                    #define XE3_FEATURES(gt) \\\n   .ver = 30, .gt = gt\n\
                    static const struct intel_device_info intel_device_info_bdw_gt2 = {\n   \
                    GFX8_FEATURES, .gt = 2,\n};\n\
                    static const struct intel_device_info intel_device_info_skl_gt2 = {\n   \
                    GFX9_FEATURES, .gt = 2,\n};\n\
                    static const struct intel_device_info intel_device_info_tgl_gt2 = {\n   \
                    .ver = 12,\n};\n\
                    static const struct intel_device_info intel_device_info_nvl_p = {\n   \
                    XE3_FEATURES(2),\n};\n";
        let listed = anv_tables::devices(ids, info).unwrap();
        let version = |id: u16| listed[&id].version;
        assert_eq!(
            [0x1616, 0x1916, 0x9a49, 0xd750].map(version),
            [8, 9, 12, 30]
        );
        assert!(listed[&0xd750].forced && !listed[&0x9a49].forced);
        let anv = "result = intel_get_device_info_from_fd(fd, &devinfo, 9, -1);\n\
                   if (devinfo.ver < 9) {\n} else if (devinfo.ver > 30) {\n";
        let versions = anv_tables::anv_versions(anv).unwrap();
        assert_eq!(versions, (9, 30));
        // Broadwell is below ANV's versions and Nova Lake force-probed.
        assert_eq!(anv_tables::opened(&listed, versions), [0x1916, 0x9a49]);
        // A table in a form the readers do not know is refused, not
        // misread.
        assert!(anv_tables::devices("CHIPSET(0x1916, kbl_gt2, \"\")", info).is_err());
        assert!(anv_tables::devices("", info).is_err());
        assert!(anv_tables::anv_versions("if (devinfo.ver < 9)").is_err());
    }
}
