//! Native GPU build tasks. Compiling a core is distinct from linking its
//! platform port into Stela or executing work on a physical GPU.
use anyhow::Result;
use clap::{Subcommand, ValueEnum};
use std::path::PathBuf;

mod anv;
mod clc;
pub mod fetch;
mod hosted;
mod image;
mod llvm;
mod mesa;
mod metal_suite;
mod names;
mod nvk;
mod radv;
mod support;
mod sysroot;
pub(crate) mod toolchain;

/// Floating-point semantics required by the supplied AIR input.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum AirFpProfile {
    #[default]
    Ieee,
    #[value(name = "apple-32023.922-air2.9-msl4.1")]
    Apple32023Air29Msl41,
}

impl AirFpProfile {
    fn name(self) -> &'static str {
        match self {
            Self::Ieee => "ieee",
            Self::Apple32023Air29Msl41 => "apple-32023.922-air2.9-msl4.1",
        }
    }

    fn kind(self) -> u32 {
        match self {
            Self::Ieee => 0,
            Self::Apple32023Air29Msl41 => 1,
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Install the pinned build tools under `.tools/gpu`, then check out each
    /// upstream submodule in `third_party/` at its pinned commit, sparsely
    /// where it has a pattern file, with Stela's patch series.
    Fetch {
        /// Discard commits and edits in a submodule that were never exported.
        #[arg(long)]
        force: bool,
    },
    /// Write each submodule's commits since its pin to its patch series.
    Export,
    /// Build NVIDIA, AMD and Intel shader compiler libraries for this host
    /// without Vulkan/OpenGL frontends, with Stela's compiler targets, and run
    /// their Meson tests.
    Compilers {
        /// Maximum parallel native compiler processes.
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..))]
        jobs: u16,
        /// Fail if any fixture target reports an unsupported compiler path.
        /// This still does not establish physical GPU or Metal support.
        #[arg(long)]
        require_all_targets: bool,
        /// Also compile the shared AIR frontend and its tests against this
        /// native LLVM 23 shared library. No GPU work is submitted.
        #[arg(long)]
        llvm_library: Option<PathBuf>,
        /// Enable the AMD LLVM backend using matching LLVM 23
        /// development files located by this llvm-config executable.
        #[arg(long, requires = "llvm_library")]
        llvm_config: Option<PathBuf>,
        /// Also compile these unchanged raw AIR bitcode inputs through each
        /// vendor compiler test. Repeat for multiple inputs; no GPU work is
        /// submitted.
        #[arg(long, requires_all = ["llvm_library", "entry"])]
        air_input: Vec<PathBuf>,
        /// Exact entry point in every supplied raw AIR input.
        #[arg(long, requires = "air_input")]
        entry: Option<String>,
        /// Floating-point profile for the unchanged raw AIR inputs.
        /// Selection requires independent compiler provenance; metadata alone
        /// does not authenticate the selected private compiler profile.
        #[arg(long, value_enum, requires = "air_input")]
        air_fp_profile: Option<AirFpProfile>,
        /// Check the bounded threadgroup fixtures: original Apple module,
        /// LLVM static WG64 module, then LLVM dynamic WG64 module. This is
        /// offline shader/packet checking, never GPU execution.
        #[arg(long, num_args = 3, requires = "llvm_library", value_names = ["APPLE", "STATIC", "DYNAMIC"])]
        threadgroup_inputs: Vec<PathBuf>,
        /// Check unchanged triangle VS/FS and sampled-texture AIR inputs.
        /// Textures currently must refuse explicitly; no GPU work is submitted.
        #[arg(long, num_args = 2, requires = "llvm_library", value_names = ["TRIANGLE", "TEXTURE"])]
        render_inputs: Vec<PathBuf>,
        /// Sweep these AIR modules from Apple's `metal -c` through the Metal
        /// compiler, with NAK's options and with BRW's for each Intel
        /// compiler generation, and compile each kernel by that compiler: a
        /// census of the shader corpus. No GPU work is submitted.
        #[arg(long, num_args = 1.., requires = "llvm_library", value_name = "MODULE")]
        metal_corpus: Vec<PathBuf>,
    },
    /// Compile NVK, NIR and NAK's C for `stela.efi` against Stela's C library,
    /// with ANV and BRW when this host has the tools their build runs, and
    /// generate the sources of Mesa's Rust crates.
    Nvk {
        /// Maximum parallel native compiler processes.
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..))]
        jobs: u16,
    },
    /// Build ANV for this machine with the image's options and run it on an
    /// xe render node with no GPU behind it: device creation, a BRW compute
    /// pipeline and one submission for each device xe admits that Mesa's
    /// Intel table lists, through Stela's libdrm stand-in, each xe ioctl
    /// checked against Stela's xe.
    AnvTest {
        /// Maximum parallel native compiler processes.
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..))]
        jobs: u16,
    },
    /// RADV and ANV as the image compiles them, with its C library and
    /// defines, built for this machine instead of the image's target and
    /// run over Mesa's DRM shim: each must make its devices and one
    /// submission. Run after `nvk`.
    ImageTest {
        /// Maximum parallel native compiler processes.
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..))]
        jobs: u16,
    },
    /// Build RADV for this machine with the image's options and run it on an
    /// amdgpu render node with no GPU behind it: device creation, an ACO
    /// compute pipeline and one submission per device of the test, through
    /// Stela's libdrm stand-in and libdrm_amdgpu, each amdgpu ioctl checked
    /// against Stela's amdgpu.
    ///
    /// Each device then opens the Metal device over the same RADV in a
    /// process of its own, which checks every Vulkan requirement the device
    /// has; the first device of each graphics generation also compiles the
    /// Metal libraries given through ACO.
    RadvTest {
        /// Maximum parallel native compiler processes.
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..))]
        jobs: u16,
        /// Metal libraries (`.metallib` files, or directories of them) to
        /// compile function by function through the device's compiler and
        /// ACO. They are Apple compiler output and never committed.
        #[arg(long)]
        corpus: Vec<PathBuf>,
    },
    /// The Metal device over the hosted RADV library LIBRARY, in the process
    /// `radv-test` starts for one device under the amdgpu DRM shim.
    #[command(hide = true)]
    RadvMetal {
        library: PathBuf,
        corpus: Vec<PathBuf>,
    },
    /// Differential testing of the Metal stack: plan the suite's assertions,
    /// prove them on a Mac, carry them to a guest and check its results.
    #[command(subcommand)]
    MetalSuite(metal_suite::Command),
    /// Write the market names of NVIDIA, AMD and Intel display controllers
    /// that the boot screen shows (`crates/stela-driver-gpu/src/names`) from
    /// the pinned `pci.ids` release and the libdrm and Mesa pins.
    Names {
        /// The PCI ID database's `pci.ids` of the pinned release.
        #[arg(long, default_value = "/usr/share/hwdata/pci.ids")]
        pci_ids: PathBuf,
    },
    /// Compile LLVM and libc++ for `stela.efi`, which `nvk` also does.
    Llvm {
        /// Maximum parallel native compiler processes.
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..))]
        jobs: u16,
        /// Also compile it for this machine and run what AIR does on it.
        #[arg(long)]
        test: bool,
    },
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Fetch { force } => fetch::fetch(force),
        Command::Export => fetch::export(),
        Command::Compilers {
            jobs,
            require_all_targets,
            llvm_library,
            llvm_config,
            air_input,
            entry,
            air_fp_profile,
            threadgroup_inputs,
            render_inputs,
            metal_corpus,
        } => mesa::build(mesa::BuildOptions {
            jobs,
            require_all_targets,
            llvm_library: llvm_library.as_deref(),
            llvm_config: llvm_config.as_deref(),
            air_inputs: &air_input,
            entry: entry.as_deref(),
            fp_profile: air_fp_profile.unwrap_or_default(),
            threadgroup_inputs: &threadgroup_inputs,
            render_inputs: &render_inputs,
            metal_corpus: &metal_corpus,
        }),
        // `stela.efi` carries the kext it links against each boot's kernel.
        Command::Nvk { jobs } => build_nvk(jobs),
        Command::AnvTest { jobs } => anv::test(jobs),
        Command::ImageTest { jobs } => image::test(jobs),
        Command::RadvTest { jobs, corpus } => radv::test(jobs, &corpus),
        Command::RadvMetal { library, corpus } => {
            radv::metal(&library, &support::third_party("linux"), &corpus)
        }
        Command::MetalSuite(command) => metal_suite::run(command),
        Command::Names { pci_ids } => names::generate(&pci_ids),
        Command::Llvm { jobs, test } => llvm::build(&toolchain::discover()?, jobs, test),
    }
}

/// Parallel compiler processes for a build that names none: one per CPU, at
/// most 8, so a small build machine does not run out of memory.
pub fn default_jobs() -> u16 {
    std::thread::available_parallelism().map_or(1, |cpus| cpus.get().min(8) as u16)
}

/// Prepare the complete native rendering archive and its guest components.
pub fn build_nvk(jobs: u16) -> Result<()> {
    nvk::build(jobs)?;
    crate::guest::run()
}
