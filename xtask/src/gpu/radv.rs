//! RADV on amdgpu without a GPU, on the build host: Mesa built for this
//! machine with the image's RADV options (`nvk::RADV`), its objects linked
//! into a test program as Mesa links its driver, with Stela's libdrm
//! stand-in (`compat/mesa/drm.c`) in libdrm's place, libdrm_amdgpu built from
//! the same unchanged sources as in the image, and Stela's NIR options hook,
//! run over Mesa's own amdgpu no-op DRM shim (`src/amd/drm-shim`), which
//! answers amdgpu's queries from real devices' records and accepts buffers,
//! binds and submissions without running them. For each device the test
//! names, the program creates RADV's device, builds a compute pipeline
//! through ACO and submits one dispatch, then builds one from NIR given as
//! the Metal device gives it (`amd/tests/radv_device.c`); every amdgpu ioctl
//! RADV made must be one Stela's amdgpu registers.
//!
//! The same objects, with the AIR compiler built as the image builds it and
//! linked to the Rust toolchain's LLVM 23, also make a hosted RADV library.
//! For each device a process of its own (`metal.rs`) opens the Metal device
//! over it as the boot does, which checks every Vulkan requirement the
//! device has and compiles its internal kernels through ACO, and reports
//! the Metal families it derives, or the requirements RADV lacks there,
//! which must be those the port's rule leaves the GPU out for
//! (`stela_driver_core::amd_metal`). With `--corpus`, one device of each
//! graphics generation also compiles every function of the Metal
//! libraries given through the device's compiler and ACO.
//!
//! Needs what RADV's image build needs (`clc.rs`: glslang, and mesa_clc
//! for Mesa's configuration) and the host's `libdrm.so.2`.
use super::hosted;
use super::nvk::{AIR, LIBDRM_VERSION, RADV};
use super::support::{build_directory, gpu, slashed, third_party, tool};
use super::toolchain::GENERATORS;
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// The owner's PC's graphics core (GC 10.3.6), which the test runs first;
/// the others are every chip RADV creates a device for (`devices.rs`).
const FIRST: &str = "raphael_mendocino";
/// The program's exit line naming the amdgpu ioctls RADV made.
const USED: &str = "amdgpu ioctls:";
/// Mesa's amdgpu no-op shim library, whose link the test runs itself: the
/// shim's libdrm calls are the program's, which Meson's link refuses as
/// undefined.
const SHIM: &str = "src/amd/drm-shim/libamdgpu_noop_drm_shim.so";

mod devices;
// The Metal device's child process loads RADV with `dlopen` and asks
// amdgpu's render node, so it runs on Linux, under Mesa's DRM shim.
#[cfg(unix)]
mod metal;
#[cfg(unix)]
pub use metal::run as metal;

/// `gpu radv-metal` off Linux: the child has no RADV to load.
#[cfg(not(unix))]
pub fn metal(_library: &Path, _linux: &Path, _corpus: &[PathBuf]) -> Result<()> {
    anyhow::bail!("gpu radv-metal runs RADV under Mesa's DRM shim, on Linux only")
}

/// What the parent finds in the child's output.
pub(super) const OPENED: &str = "Metal device:";
pub(super) const REFUSED: &str = "Metal device refused:";
/// What follows [`REFUSED`]: the requirements the device lacks, by name,
/// between these.
pub(super) const LACKS: &str = "the Vulkan device lacks ";
pub(super) const LEFT_OUT: &str = "; the port leaves such a GPU out";
pub(super) const CORPUS: &str = "Metal corpus:";

pub fn test(jobs: u16, corpus: &[PathBuf]) -> Result<()> {
    let corpus = libraries(corpus)?;
    let directory = build_directory("radv-host");
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
        Err(reason) => bail!("RADV cannot be built on this host: {reason}"),
    };
    for path in [&output, &bin, &packages] {
        fs::create_dir_all(path)?;
    }
    for program in &tools.programs {
        fs::copy(program, bin.join(program.file_name().context("host tool")?))?;
    }
    let path =
        std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(&host)))?;
    let libdrm = third_party("libdrm");
    ensure!(
        libdrm.join("xf86drm.h").is_file() && libdrm.join("amdgpu/amdgpu_device.c").is_file(),
        "third_party/libdrm is missing or lacks libdrm_amdgpu; run `cargo xtask gpu fetch`"
    );
    // libdrm's headers as the image has them; its calls are Stela's, and
    // libdrm_amdgpu is built here from its sources, as for the image.
    fs::write(
        packages.join("libdrm.pc"),
        format!(
            "Name: libdrm\nDescription: vendored headers\nVersion: {LIBDRM_VERSION}\n\
             Cflags: -I{0} -I{0}/include/drm\n",
            slashed(&libdrm)
        ),
    )?;
    fs::write(
        packages.join("libdrm_amdgpu.pc"),
        format!(
            "Name: libdrm_amdgpu\nDescription: built from its sources\nVersion: {LIBDRM_VERSION}\n\
             Cflags: -I{0} -I{0}/amdgpu -I{0}/include/drm\n",
            slashed(&libdrm)
        ),
    )?;
    let arguments = hosted::arguments(RADV.name, RADV.options);
    hosted::configure(&directory, &build, &packages, &path, &arguments)?;
    let link = hosted::link_command(&build, RADV.target)?;
    let shim_link = hosted::link_command(&build, SHIM)?;
    let mut targets = hosted::built_inputs(&link);
    targets.extend(hosted::built_inputs(&shim_link));
    crate::tools::exec(
        Command::new(tool("ninja", GENERATORS)?)
            .arg("-C")
            .arg(&build)
            .arg(format!("-j{jobs}"))
            .args(&targets)
            .env("PATH", &path),
    )?;
    let program = link_test(&build, &output, &link, &tools.glslang.0)?;
    let registered = hosted::registered_ioctls(
        &fs::read_to_string(third_party("linux").join("drivers/gpu/drm/amd/amdgpu/amdgpu_drv.c"))?,
        "AMDGPU_",
    );
    ensure!(!registered.is_empty(), "amdgpu_drv.c registers no ioctl");
    let devices = devices::devices(&shim_sources()?, FIRST)?;
    let shim = output.join("libamdgpu_noop_drm_shim.so");
    let [table, start] = shim_table(&build, &output, &devices)?;
    let mut words = hosted::preloaded_link(&shim_link, &shim);
    for (unit, object) in [
        ("_common_amdgpu_devices.c.o", table),
        ("/amdgpu_noop_drm_shim.c.o", start),
    ] {
        let replaced = words
            .iter_mut()
            .find(|word| word.ends_with(unit))
            .with_context(|| format!("the shim's link has no {unit}"))?;
        *replaced = object.display().to_string();
    }
    crate::tools::exec(
        Command::new(&words[0])
            .current_dir(&build)
            .args(&words[1..]),
    )?;
    let llvm = air_llvm()?;
    let library = link_library(&build, &output, &link, &llvm)?;
    let mut failures = Vec::new();
    let (mut opened, mut left_out) = (0, 0);
    let mut classes = Vec::new();
    let sweeps = sweep_devices(&devices);
    for shim_device in &devices {
        let (device, discrete) = (shim_device.name.as_str(), shim_device.discrete);
        let name = shim_device.description();
        let run = Command::new("timeout")
            .arg("120")
            .arg(&program)
            .arg(if discrete { "discrete" } else { "integrated" })
            .env("LD_PRELOAD", &shim)
            .env("AMDGPU_GPU_ID", device)
            .stdin(Stdio::null())
            .output()?;
        let stdout = String::from_utf8_lossy(&run.stdout);
        let stderr = String::from_utf8_lossy(&run.stderr);
        fs::write(
            output.join(format!("{device}.log")),
            format!("{stdout}{stderr}"),
        )?;
        let used = hosted::used_ioctls(&stdout, USED);
        let unregistered: Vec<_> = used
            .keys()
            .filter(|ioctl| !registered.contains(ioctl))
            .cloned()
            .collect();
        let summary = stdout
            .lines()
            .find(|line| line.starts_with("RADV on amdgpu:"));
        match (run.status.success(), summary, unregistered.is_empty()) {
            (true, Some(summary), true) if !used.is_empty() => {
                println!("{device} ({name}): {summary}");
                println!(
                    "  amdgpu ioctls: {}",
                    used.iter()
                        .map(|(ioctl, count)| format!("{ioctl} {count}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                // What the port's rule says RADV lacks on this generation:
                // a GPU it names anything for is not claimed.
                let expected: Vec<&str> =
                    stela_driver_core::amd_metal::lacking(generation(&shim_device.level)?)
                        .collect();
                // The corpus runs on one device of each generation.
                let sweep = !corpus.is_empty() && sweeps.contains(&device);
                match open_metal(
                    &library,
                    &shim,
                    device,
                    if sweep { &corpus } else { &[] },
                    &output,
                ) {
                    Ok(Opened::Metal(_)) if !expected.is_empty() => failures.push(format!(
                        "{device} ({name}): the Metal device opened, yet the port leaves its \
                         generation out as lacking {} (stela-driver-core's amd_metal)",
                        expected.join(", ")
                    )),
                    Ok(Opened::Metal(lines)) => {
                        opened += 1;
                        // A generation left out compiles nothing.
                        if sweep {
                            classes.push(shim_device.level.as_str());
                        }
                        lines.iter().for_each(|line| println!("  {line}"));
                    }
                    Ok(Opened::LeftOut(line, lacks)) if lacks == expected => {
                        left_out += 1;
                        println!("  {line}");
                    }
                    Ok(Opened::LeftOut(_, lacks)) => failures.push(format!(
                        "{device} ({name}): RADV lacks {}, where the port's rule for its \
                         generation (stela-driver-core's amd_metal) names {}",
                        lacks.join(", "),
                        if expected.is_empty() {
                            String::from("nothing, so the port would claim it")
                        } else {
                            expected.join(", ")
                        }
                    )),
                    Err(error) => failures.push(format!("{device} ({name}): {error:#}")),
                }
            }
            (true, _, false) => failures.push(format!(
                "{device} ({name}): RADV used ioctls Stela's amdgpu does not register: {}",
                unregistered.join(", ")
            )),
            _ => failures.push(format!(
                "{device} ({name}) failed with {}: {}",
                run.status,
                stderr.trim()
            )),
        }
    }
    ensure!(
        failures.is_empty(),
        "RADV on amdgpu failed on the host (logs in {}):\n{}",
        output.display(),
        failures.join("\n")
    );
    println!(
        "RADV ran on amdgpu's ioctls for {} devices, with no GPU; the Metal device opened on {opened} \
         of them and the port leaves out the other {left_out}, as its rule says{}",
        devices.len(),
        if corpus.is_empty() {
            String::from(", no Metal libraries given to compile")
        } else {
            format!(
                ", and compiled {} Metal libraries on {} generations",
                corpus.len(),
                classes.len()
            )
        }
    );
    Ok(())
}

/// The graphics generation of one of Mesa's levels: `GFX10_3` is 10.
fn generation(level: &str) -> Result<u8> {
    level
        .strip_prefix("GFX")
        .and_then(|rest| rest.split('_').next()?.parse().ok())
        .with_context(|| format!("{level} names no graphics generation"))
}

/// What the Metal device did on a device.
enum Opened {
    /// It opened: its report, the corpus's included.
    Metal(Vec<String>),
    /// RADV lacks requirements, named in the report's line: the port leaves
    /// the GPU out.
    LeftOut(String, Vec<String>),
}

/// Runs `metal.rs` in a process of its own over the shim showing `device`,
/// then once for each of the Metal libraries `corpus`, so that a library
/// that stops its process is reported and the others still compile; the
/// logs go beside the program's.
fn open_metal(
    library: &Path,
    shim: &Path,
    device: &str,
    corpus: &[PathBuf],
    output: &Path,
) -> Result<Opened> {
    let child = |corpus: Option<&Path>, log: &Path| -> Result<String> {
        let run = Command::new("timeout")
            .arg(if corpus.is_none() { "300" } else { "1800" })
            .arg(std::env::current_exe()?)
            .args(["gpu", "radv-metal"])
            .arg(library)
            .args(corpus)
            .env("LD_PRELOAD", shim)
            .env("AMDGPU_GPU_ID", device)
            .stdin(Stdio::null())
            .output()?;
        let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&run.stderr);
        fs::write(log, format!("{stdout}{stderr}"))?;
        let building = stderr
            .lines()
            .rev()
            .find_map(|line| line.trim_start().strip_prefix("building "))
            .map(|function| format!(" building {function}"))
            .unwrap_or_default();
        ensure!(
            run.status.success(),
            "the Metal device's process failed with {}{building} (log {})",
            run.status,
            log.display()
        );
        Ok(stdout)
    };
    let stdout = child(None, &output.join(format!("{device}.metal.log")))?;
    if let Some(line) = stdout.lines().find(|line| line.starts_with(REFUSED)) {
        let lacks = line
            .split_once(LACKS)
            .and_then(|(_, rest)| rest.strip_suffix(LEFT_OUT))
            .with_context(|| format!("the Metal device's refusal names no requirement: {line}"))?;
        return Ok(Opened::LeftOut(
            line.to_owned(),
            lacks.split(", ").map(str::to_owned).collect(),
        ));
    }
    let mut lines: Vec<String> = stdout
        .lines()
        .filter(|line| line.starts_with(OPENED))
        .map(str::to_owned)
        .collect();
    ensure!(
        !lines.is_empty(),
        "the Metal device's process reported nothing"
    );
    let mut total = Corpus::default();
    let mut failed = Vec::new();
    for path in corpus {
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let log = output.join(format!("{device}.{name}.metal.log"));
        match child(Some(path), &log) {
            Ok(stdout) => {
                let counted = stdout.lines().find_map(Corpus::parse);
                total.add(&counted.with_context(|| format!("{name} reported no corpus line"))?);
                lines.extend(
                    stdout
                        .lines()
                        .filter(|line| line.starts_with("  refused "))
                        .map(|line| format!("{name}:{}", line.trim_start_matches(' '))),
                );
            }
            Err(error) => failed.push(format!("{name}: {error:#}")),
        }
    }
    if !corpus.is_empty() {
        lines.push(total.line());
    }
    ensure!(
        failed.is_empty(),
        "the corpus stopped compiling: {}",
        failed.join("; ")
    );
    Ok(Opened::Metal(lines))
}

/// A corpus line's counts (`CORPUS`).
#[derive(Debug, Default, PartialEq)]
struct Corpus {
    functions: usize,
    compute: usize,
    graphics: usize,
    refused: usize,
    skipped: usize,
    /// Kernels the driver built again from its cache's data alone.
    cached: usize,
}

impl Corpus {
    fn parse(line: &str) -> Option<Self> {
        let numbers: Vec<usize> = line
            .strip_prefix(CORPUS)?
            .split(|c: char| !c.is_ascii_digit())
            .filter(|word| !word.is_empty())
            .map(|word| word.parse().ok())
            .collect::<Option<_>>()?;
        let [
            functions,
            _,
            compute,
            graphics,
            refused,
            skipped,
            cached,
            kernels,
        ] = numbers[..]
        else {
            return None;
        };
        (kernels == compute).then_some(Self {
            functions,
            compute,
            graphics,
            refused,
            skipped,
            cached,
        })
    }

    fn add(&mut self, other: &Self) {
        self.functions += other.functions;
        self.compute += other.compute;
        self.graphics += other.graphics;
        self.refused += other.refused;
        self.skipped += other.skipped;
        self.cached += other.cached;
    }

    fn line(&self) -> String {
        format!(
            "{} {} functions, {} pipelines built by ACO ({} compute, {} graphics), {} refused, \
             {} skipped, {} of {} kernels built again from the driver's cache data alone",
            CORPUS,
            self.functions,
            self.compute + self.graphics,
            self.compute,
            self.graphics,
            self.refused,
            self.skipped,
            self.cached,
            self.compute
        )
    }
}

/// The device of each graphics generation the corpus compiles on: its
/// first recorded one, else its first.
fn sweep_devices(devices: &[devices::ShimDevice]) -> Vec<&str> {
    let mut levels: Vec<&str> = Vec::new();
    for device in devices {
        if !levels.contains(&device.level.as_str()) {
            levels.push(&device.level);
        }
    }
    levels
        .into_iter()
        .filter_map(|level| {
            let mut of_level = devices.iter().filter(|device| device.level == level);
            let first = of_level.clone().next()?;
            Some(
                of_level
                    .find(|device| device.derived.is_none())
                    .unwrap_or(first)
                    .name
                    .as_str(),
            )
        })
        .collect()
}

/// The sources `devices.rs` reads.
fn shim_sources() -> Result<devices::Sources> {
    let mesa = third_party("mesa").join("src/amd");
    let read = |path: PathBuf| {
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    };
    Ok(devices::Sources {
        families: read(mesa.join("common/amd_family.h"))?,
        levels: read(mesa.join("common/amd_family.c"))?,
        identify: read(mesa.join("common/ac_gpu_info.c"))?,
        ranges: read(mesa.join("addrlib/src/amdgpu_asic_addr.h"))?,
        records: read(mesa.join("common/amdgpu_devices.c"))?,
        ids: read(third_party("linux").join("drivers/gpu/drm/amd/amdgpu/amdgpu_drv.c"))?,
    })
}

/// Compiles the shim's device table with the devices derived from
/// recorded siblings (`amd/tests/radv_shim_devices.c`), which fills it as
/// the shim starts its device, and the shim's own start under the name the
/// table calls; their objects.
fn shim_table(
    build: &Path,
    output: &Path,
    devices: &[devices::ShimDevice],
) -> Result<[PathBuf; 2]> {
    let generated = output.join("shim");
    fs::create_dir_all(&generated)?;
    fs::write(
        generated.join("radv_shim_derived.h"),
        devices::derived_header(devices)?,
    )?;
    let object = generated.join("radv_shim_devices.o");
    crate::tools::exec(
        hosted::compile_command(
            build,
            "amd/common/amdgpu_devices.c",
            &gpu().join("amd/tests/radv_shim_devices.c"),
            &object,
        )?
        .arg(format!("-I{}", generated.display())),
    )?;
    let start = generated.join("amdgpu_noop_drm_shim.o");
    crate::tools::exec(
        hosted::compile_command(
            build,
            "amd/drm-shim/amdgpu_noop_drm_shim.c",
            &third_party("mesa").join("src/amd/drm-shim/amdgpu_noop_drm_shim.c"),
            &start,
        )?
        .arg("-Ddrm_shim_driver_init=stela_mesa_drm_shim_driver_init"),
    )?;
    Ok([object, start])
}

/// The Metal libraries of `paths`: each file, and each `.metallib` under a
/// directory, in order.
fn libraries(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut entries: Vec<PathBuf> = fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<_>>()?;
            entries.sort();
            found.extend(entries.into_iter().filter(|entry| {
                entry
                    .extension()
                    .is_some_and(|extension| extension == "metallib")
            }));
        } else {
            ensure!(path.is_file(), "{} is no Metal library", path.display());
            found.push(path.clone());
        }
    }
    Ok(found)
}

/// The LLVM 23 shared library of the Rust toolchain building this task,
/// whose C interface the AIR compiler links on the host as it links the
/// image's LLVM.
fn air_llvm() -> Result<PathBuf> {
    let sysroot = PathBuf::from(
        super::support::capture(Command::new("rustc").args(["--print", "sysroot"]))?.trim(),
    );
    let directory = sysroot.join("lib");
    fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libLLVM.so.23"))
        })
        .with_context(|| format!("the Rust toolchain at {} has no LLVM 23", sysroot.display()))
}

/// Builds the hosted RADV library: the driver's objects as Mesa links them,
/// with the image's stand-ins, libdrm_amdgpu from its sources, the AIR
/// compiler built as the image builds it, and `radv_host.c`; its path.
fn link_library(build: &Path, output: &Path, link: &[String], llvm: &Path) -> Result<PathBuf> {
    let sources = gpu();
    let objects_directory = output.join("library");
    fs::create_dir_all(&objects_directory)?;
    let libc = format!("-idirafter{}", sources.join("compat/libc").display());
    let exported = "-fvisibility=default".to_owned();
    let mut units = vec![
        (
            sources.join("compat/mesa/radv_nir_options.c"),
            vec!["-Wno-missing-prototypes".to_owned(), exported.clone()],
        ),
        (
            sources.join("compat/mesa/drm.c"),
            vec![libc.clone(), exported.clone(), "-Wno-vla".to_owned()],
        ),
        (
            sources.join("amd/tests/radv_host.c"),
            vec![libc, exported.clone()],
        ),
    ];
    for library in RADV.libraries {
        let defines: Vec<String> = library
            .defines
            .iter()
            .map(|define| format!("-D{define}"))
            .collect();
        units.extend(
            library
                .sources
                .iter()
                .map(|source| (third_party(source), defines.clone())),
        );
    }
    let compiler = sources.join("compiler");
    let includes: Vec<String> = [
        compiler.clone(),
        compiler.join("llvm/include"),
        third_party("llvm").join("llvm/include"),
        sources.clone(),
    ]
    .iter()
    .map(|include| format!("-I{}", include.display()))
    .chain([
        exported,
        "-Wno-missing-prototypes".to_owned(),
        "-Wno-vla".to_owned(),
    ])
    .collect();
    units.extend(
        AIR.iter()
            .map(|name| (compiler.join(format!("{name}.c")), includes.clone())),
    );
    let mut objects = Vec::new();
    for (source, flags) in units {
        let object = objects_directory.join(
            source
                .with_extension("o")
                .file_name()
                .context("unit name")?,
        );
        crate::tools::exec(
            hosted::compile_command(build, RADV.model, &source, &object)?.args(flags),
        )?;
        objects.push(object);
    }
    let library = output.join("libstela_radv_host.so");
    let directory = llvm.parent().context("LLVM's directory")?;
    let extra = [
        llvm.display().to_string(),
        format!("-Wl,-rpath,{}", directory.display()),
        hosted::host_libdrm()?.display().to_string(),
    ];
    let words = hosted::library_link(link, &library, &objects, &extra);
    crate::tools::exec(Command::new(&words[0]).current_dir(build).args(&words[1..]))?;
    Ok(library)
}

/// Builds the test program; its path.
fn link_test(build: &Path, output: &Path, link: &[String], glslang: &Path) -> Result<PathBuf> {
    let sources = gpu();
    crate::tools::exec(
        Command::new(glslang)
            .args([
                "-V",
                "--target-env",
                "vulkan1.1",
                "--vn",
                "stela_radv_device_spirv",
            ])
            .arg("-o")
            .arg(output.join("radv_device_shader.h"))
            .arg(sources.join("amd/tests/radv_device.comp"))
            .stdout(Stdio::null()),
    )?;
    let libc = format!("-idirafter{}", sources.join("compat/libc").display());
    let mut units = vec![
        (
            sources.join("compat/mesa/radv_nir_options.c"),
            vec!["-Wno-missing-prototypes".to_owned()],
        ),
        // Exported, as in Intel's test, so the shim's libdrm calls reach
        // them. RADV's units refuse variable-length arrays, which libdrm's
        // command macros take the size of; NVK's flags, which the image
        // compiles this unit with, allow them.
        (
            sources.join("compat/mesa/drm.c"),
            vec![
                libc.clone(),
                "-fvisibility=default".to_owned(),
                "-Wno-vla".to_owned(),
            ],
        ),
        (
            sources.join("amd/tests/radv_device.c"),
            vec![libc, format!("-I{}", output.display())],
        ),
    ];
    for library in RADV.libraries {
        let defines: Vec<String> = library
            .defines
            .iter()
            .map(|define| format!("-D{define}"))
            .collect();
        units.extend(
            library
                .sources
                .iter()
                .map(|source| (third_party(source), defines.clone())),
        );
    }
    let mut objects = Vec::new();
    for (source, flags) in units {
        let object = output.join(
            source
                .with_extension("o")
                .file_name()
                .context("unit name")?,
        );
        crate::tools::exec(
            hosted::compile_command(build, RADV.model, &source, &object)?.args(flags),
        )?;
        objects.push(object);
    }
    let program = output.join("radv_device");
    // Every ioctl the linked code makes passes the program's count first.
    let extra = [
        "-Wl,--wrap=ioctl".to_owned(),
        hosted::host_libdrm()?.display().to_string(),
    ];
    let words = hosted::program_link(link, &program, &objects, &extra);
    crate::tools::exec(Command::new(&words[0]).current_dir(build).args(&words[1..]))?;
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_lines_read_back_and_add_up() {
        let one = Corpus {
            functions: 9,
            compute: 3,
            graphics: 4,
            refused: 1,
            skipped: 1,
            cached: 3,
        };
        assert_eq!(Corpus::parse(&one.line()), Some(one));
        let mut total = Corpus::parse(
            &Corpus {
                functions: 2,
                ..Corpus::default()
            }
            .line(),
        )
        .unwrap();
        total.add(&Corpus {
            functions: 9,
            compute: 3,
            graphics: 4,
            refused: 1,
            skipped: 1,
            cached: 2,
        });
        assert_eq!((total.functions, total.cached), (11, 2));
        assert_eq!(Corpus::parse("Metal corpus: 3 functions"), None);
        assert_eq!(Corpus::parse("something else 1 2 3 4 5 6"), None);
    }

    #[test]
    fn the_corpus_compiles_on_a_recorded_device_of_each_generation() {
        let device = |name: &str, level: &str, derived: bool| devices::ShimDevice {
            name: name.to_owned(),
            chip: name.to_uppercase(),
            level: level.to_owned(),
            note: String::new(),
            discrete: true,
            derived: derived.then(|| devices::Derived {
                base: String::from("base"),
                family: 0,
                external_rev: 0,
                device_id: 0,
                version: None,
            }),
        };
        let devices = [
            device("tahiti", "GFX6", true),
            device("pitcairn", "GFX6", false),
            device("gfx1170", "GFX11_7", true),
            device("navi21", "GFX10_3", false),
            device("navi22", "GFX10_3", true),
        ];
        assert_eq!(sweep_devices(&devices), ["pitcairn", "gfx1170", "navi21"]);
    }

    #[test]
    fn radvs_test_build_is_the_images_radv() {
        let arguments = hosted::arguments(RADV.name, RADV.options);
        assert!(arguments.contains(&"-Dvulkan-drivers=amd".to_owned()));
        assert!(arguments.contains(&"-Damd-use-llvm=false".to_owned()));
        // The image has no `dladdr`, so RADV's cache UUID needs a named ID.
        assert!(
            arguments
                .iter()
                .any(|argument| argument.starts_with("-Dradv-build-id="))
        );
        assert_eq!(RADV.libraries[0].name, "libdrm_amdgpu");
    }
}
