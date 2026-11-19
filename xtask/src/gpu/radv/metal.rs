//! The Metal device over RADV in a process of its own: what `radv-test`
//! runs (as the hidden `gpu radv-metal`) for each device, under Mesa's
//! amdgpu no-op DRM shim, over the hosted RADV library it built (RADV, the
//! AIR compiler, Stela's libdrm stand-in and libdrm_amdgpu, `radv.rs`).
//!
//! It opens the Metal device as the boot does: the stack's Vulkan backend
//! over RADV's loader entry with the AIR compiler service and RADV's NIR
//! options (`open::stack`, so every requirement the device checks, its
//! descriptor heap and pipeline layout, and its internal kernels compiled by
//! ACO), the AMD admission with amdgpu's family as amdgpu's query answers it
//! on the shim's render node, and the Metal device over both
//! (`Device::new`, which derives Metal's capabilities). A device that misses
//! a requirement is reported with every requirement it lacks: the port
//! leaves such a GPU out (`stela_driver_core::amd_metal`).
//!
//! Then each Metal library given is compiled function by function through
//! the device's compiler, as the NVIDIA corpus sweep compiles with NAK
//! (`compiler/tests/metal_corpus.c`): a kernel with 64 threads per
//! threadgroup (fewer when it declares fewer) and every threadgroup buffer
//! its declared bytes, a render function with its own varyings linked,
//! required function constants zero. Each stage's NIR goes to RADV in a
//! pipeline as the device builds its pipelines: a kernel (and a tile
//! function) in a compute pipeline, a vertex or mesh function alone in a
//! graphics pipeline, a fragment function after a vertex stage that writes
//! its inputs (`amd/tests/radv_host.c`). ACO compiles each. Nothing runs on
//! a GPU: the shim accepts and drops submissions.
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, c_void},
    fs,
    os::fd::{FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    ptr::NonNull,
};
use stela_contracts::gate::MemoryRange;
use stela_drivers::gpu::metal::{
    amd,
    compiler::{AirCompiler, NIR_STAGES, Service},
    device::Device,
    error::Error,
    library::Library,
    native::{ExportedMemory, Loader, MemoryExporter, NativeBackend, vk},
    open::{self, Attachment, PcieLink},
    pipeline::MAX_COLOR_ATTACHMENTS,
    shader::{Compiler, NumericClass, Reflection, Specialization, Stage, StorageAttachment},
    vulkan::{
        BlendFactor, BlendOp, ColorComponents, Format, PhysicalDeviceType, PrimitiveTopology,
        SampleCounts, ShaderStages,
        backend::{Backend, Object},
        object::{
            ColorBlendAttachment, ComputePipelineInfo, GraphicsPipelineInfo, LineRasterization,
            Pipeline, PipelineSource, ShaderCode, VertexInput,
        },
        physical::PciBusInfo,
    },
};

/// The PCI function the hosted library's DRM device names (`radv_host.c`).
const PCI: PciBusInfo = PciBusInfo {
    domain: 0,
    bus: 4,
    device: 0,
    function: 0,
};
/// The render node Mesa's DRM shim shows.
const RENDER_NODE: &CStr = c"/dev/dri/renderD128";
/// What the parent finds in the child's output.
pub(super) const OPENED: &str = "Metal device:";
pub(super) const REFUSED: &str = "Metal device refused:";
/// What follows [`REFUSED`]: the requirements the device lacks, by name,
/// between these.
pub(super) const LACKS: &str = "the Vulkan device lacks ";
pub(super) const LEFT_OUT: &str = "; the port leaves such a GPU out";
pub(super) const CORPUS: &str = "Metal corpus:";

fn log(message: &str) {
    println!("  driver: {message}");
}

fn relax() {
    std::hint::spin_loop();
}

/// Guest memory has no host owner here; no path of the open or the corpus
/// imports any.
struct NoGuestMemory;

impl MemoryExporter for NoGuestMemory {
    fn export(&self, _ranges: &[MemoryRange]) -> Result<ExportedMemory, Error> {
        Err(Error::Unsupported("guest memory in a host process"))
    }
    fn close(&self, _exported: ExportedMemory) {}
    fn unmap(&self, _exported: ExportedMemory) {}
}

/// The hosted library, loaded for the process's life.
struct Hosted(*mut c_void);

impl Hosted {
    fn open(path: &Path) -> Result<Self> {
        let name = CString::new(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: a library this test built; it lives until exit.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if handle.is_null() {
            // SAFETY: dlerror's message, read at once.
            let error = unsafe { CStr::from_ptr(libc::dlerror()) };
            bail!(
                "{} did not load: {}",
                path.display(),
                error.to_string_lossy()
            );
        }
        Ok(Self(handle))
    }

    fn symbol(&self, name: &CStr) -> Result<*mut c_void> {
        // SAFETY: the handle is the loaded library's.
        let address = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        ensure!(
            !address.is_null(),
            "the hosted library lacks {}",
            name.to_string_lossy()
        );
        Ok(address)
    }
}

/// amdgpu's family of the shim's device, as the boot's query of the render
/// node answers it (`AMDGPU_INFO_DEV_INFO`, `drm_amdgpu_info_device::family`).
fn amdgpu_family() -> Result<u32> {
    #[repr(C)]
    struct Info {
        return_pointer: u64,
        return_size: u32,
        query: u32,
        arguments: [u32; 4],
    }
    /// `DRM_IOW(DRM_COMMAND_BASE + DRM_AMDGPU_INFO, struct drm_amdgpu_info)`.
    const INFO: libc::c_ulong = 0x4020_6445;
    const DEV_INFO: u32 = 0x16;
    // Linux copies at most `return_size` bytes; Mesa's shim copies its
    // whole record, so the answer has room for every field.
    let mut words = [0u32; 256];
    let mut info = Info {
        return_pointer: words.as_mut_ptr() as u64,
        return_size: size_of_val(&words) as u32,
        query: DEV_INFO,
        arguments: [0; 4],
    };
    // SAFETY: plain file and ioctl calls with buffers that outlive them.
    unsafe {
        let fd = libc::open(RENDER_NODE.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        ensure!(fd >= 0, "the shim shows no render node");
        let result = libc::ioctl(fd, INFO, &mut info);
        libc::close(fd);
        ensure!(result == 0, "amdgpu's device query failed");
    }
    // device_id, chip_rev, external_rev, pci_rev, family.
    Ok(words[4])
}

/// The name amdgpu's interface gives `family` (`AMDGPU_FAMILY_<name>` in
/// `amdgpu_drm.h`), as the kernel stack's table reads it.
fn family_name(header: &str, family: u32) -> Option<String> {
    header.lines().find_map(|line| {
        let mut words = line
            .strip_prefix("#define AMDGPU_FAMILY_")?
            .split_whitespace();
        let name = words.next()?;
        let number = words.next()?.split("/*").next()?.trim();
        (number.parse::<u32>().ok()? == family).then(|| name.to_owned())
    })
}

/// The function at `address` as the pointer type `F`.
///
/// # Safety
///
/// `address` is a function of `F`'s signature.
unsafe fn entry_point<F: Copy>(address: *mut c_void) -> F {
    assert_eq!(
        size_of::<F>(),
        size_of::<*mut c_void>(),
        "a function pointer"
    );
    // SAFETY: as the caller promises, of a function pointer's size.
    unsafe { std::mem::transmute_copy(&address) }
}

/// The compiler service of the hosted library, with RADV's NIR options for
/// `physical`.
fn service(hosted: &Hosted, physical: vk::PhysicalDevice) -> Result<Service> {
    type Options = unsafe extern "C" fn(vk::PhysicalDevice, u32) -> *const c_void;
    // SAFETY: the hook's signature (`compat/mesa/radv_nir_options.c`).
    let options: Options =
        unsafe { std::mem::transmute(hosted.symbol(c"stela_radv_nir_options")?) };
    let mut nir_options = Vec::with_capacity(NIR_STAGES);
    for stage in 0..NIR_STAGES as u32 {
        // SAFETY: RADV's physical device of this instance.
        let pointer = unsafe { options(physical, stage) };
        nir_options.push(
            NonNull::new(pointer.cast_mut())
                .with_context(|| format!("RADV has no NIR options for stage {stage}"))?,
        );
    }
    let nir_options: [NonNull<c_void>; NIR_STAGES] = nir_options.try_into().expect("one per stage");
    macro_rules! entry {
        ($name:literal) => {{
            let address = hosted.symbol(
                CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).expect("a name"),
            )?;
            // SAFETY: the compiler service's own entry point, whose
            // signature `compiler/metal_compiler.h` and `metal_abi.h` give
            // as `compiler::Service` declares it.
            unsafe { entry_point(address) }
        }};
    }
    Ok(Service {
        reflect: entry!("stela_metal_reflect"),
        reflect_bindings: entry!("stela_metal_reflect_bindings"),
        sampler_states: entry!("stela_metal_sampler_states"),
        functions: entry!("stela_metal_functions"),
        exports: entry!("stela_metal_exports"),
        stitch: entry!("stela_metal_stitch"),
        compile: entry!("stela_metal_compile_reflected"),
        tessellation_stages: entry!("stela_metal_tessellation_stages"),
        accumulate_kernel: entry!("stela_metal_accumulate_kernel"),
        select_kernel: entry!("stela_metal_select_kernel"),
        resolve_draws_kernel: entry!("stela_metal_resolve_draws_kernel"),
        resolve_dispatches_kernel: entry!("stela_metal_resolve_dispatches_kernel"),
        store_texels_kernel: entry!("stela_metal_store_texels_kernel"),
        load_texels_kernel: entry!("stela_metal_load_texels_kernel"),
        average_samples_kernel: entry!("stela_metal_average_samples_kernel"),
        decode_etc_kernel: entry!("stela_metal_decode_etc_kernel"),
        decode_pvrtc_kernel: entry!("stela_metal_decode_pvrtc_kernel"),
        reorder_blocks_kernel: entry!("stela_metal_reorder_blocks_kernel"),
        decode_astc_kernel: entry!("stela_metal_decode_astc_kernel"),
        release: entry!("stela_metal_release"),
        buffer_reads: entry!("stela_metal_buffer_reads"),
        nir_options,
    })
}

/// Opens the Metal device over the hosted library at `library` and compiles
/// each Metal library of `corpus` through it; `linux` is the Linux tree
/// whose `amdgpu_drm.h` names amdgpu's families.
pub fn run(library: &Path, linux: &Path, corpus: &[PathBuf]) -> Result<()> {
    let hosted = Hosted::open(library)?;
    // SAFETY: RADV's loader entry (`vk_icdGetInstanceProcAddr`).
    let entry: vk::PFN_vkGetInstanceProcAddr =
        unsafe { std::mem::transmute(hosted.symbol(c"vk_icdGetInstanceProcAddr")?) };
    let family = amdgpu_family()?;
    let header = fs::read_to_string(linux.join("include/uapi/drm/amdgpu_drm.h"))?;
    let name = family_name(&header, family);
    let loader = Loader {
        get_instance_proc_addr: entry,
        log,
        submit_thread: None,
        refusals: None,
        keep_lost: false,
        check_first_submission: false,
        relax,
    };
    // Every requirement RADV lacks, where the boot names only the first.
    // SAFETY: as for the open below.
    let surveyed = unsafe { NativeBackend::survey(loader, PCI) }
        .map_err(|error| anyhow::anyhow!("RADV's instance did not start: {error:?}"))?;
    let (_, missing) = surveyed.context("RADV offers no device at the shim's PCI function")?;
    if !missing.is_empty() {
        println!("{REFUSED} {LACKS}{}{LEFT_OUT}", missing.join(", "));
        return Ok(());
    }
    // A missing entry point stops the run before the backend opens.
    let mut vertex_options = None;
    let probe = |physical| {
        let service = service(&hosted, physical).expect("the hosted compiler service");
        vertex_options = Some(service.nir_options[0]);
        service
    };
    // SAFETY: RADV's entry and the AIR compiler with RADV's options, both
    // in the hosted library, which stays loaded.
    let opened = unsafe { open::stack(loader, PCI, Box::new(NoGuestMemory), probe) };
    let (backend, compiler) = match opened {
        Ok(opened) => opened,
        Err(Error::Unsupported(missing)) => {
            println!("{REFUSED} {LACKS}{missing}{LEFT_OUT}");
            return Ok(());
        }
        Err(error) => bail!("the Metal device's backend did not open: {error:?}"),
    };
    let identity = backend.physical_device().identity.clone();
    let integrated = identity.device_type == PhysicalDeviceType::IntegratedGpu;
    let attachment = Attachment {
        link: if integrated {
            PcieLink::NONE
        } else {
            PcieLink {
                speed: 4,
                width: 16,
            }
        },
        external: false,
        location_number: 0,
    };
    let platform = amd::admit_named(&identity, family, name.as_deref(), attachment)
        .map_err(|refusal| anyhow::anyhow!("the AMD admission refused it: {refusal:?}"))?;
    let mut device = Device::new(backend, compiler, &platform)
        .map_err(|error| anyhow::anyhow!("the Metal device did not open: {error:?}"))?;
    // The queue's timeline as a descriptor the guest's waits take, or
    // completions found by polling where the driver exports none.
    // The no-op shim's syncobjs export as no descriptor (-1).
    let completion = device.take_completion();
    if let Some(fd) = completion.filter(|&fd| fd >= 0) {
        // SAFETY: the device gave the descriptor to this process to own.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    let capabilities = device.capabilities();
    let families: Vec<String> = capabilities
        .families()
        .iter()
        .map(|family| format!("{family:?}"))
        .collect();
    println!(
        "{OPENED} opened on {} ({}), families {}, {} capability gaps, completions {}",
        identity.device_name,
        platform.architecture,
        families.join(" "),
        capabilities.gaps().len(),
        if completion.is_some() {
            "waited on as an exported timeline"
        } else {
            "polled"
        }
    );
    for gap in capabilities.gaps() {
        println!("  gap: {} ({:?})", gap.capability, gap.cause);
    }
    if corpus.is_empty() {
        return Ok(());
    }
    let feeding = (
        feeding_vertex(&hosted)?,
        vertex_options.context("no vertex options")?,
    );
    let mut sweep = Sweep::default();
    for path in corpus {
        sweep.library(&mut device, feeding, path)?;
    }
    let counted = super::Corpus {
        functions: sweep.functions,
        compute: sweep.compute,
        graphics: sweep.graphics,
        refused: sweep.refused.values().sum(),
        skipped: sweep.skipped,
        cached: sweep.cached,
    };
    println!("{}", counted.line());
    for (reason, count) in &sweep.refused {
        println!("  refused {count}: {reason}");
    }
    Ok(())
}

/// `stela_radv_host_feeding_vertex` of the hosted library.
type Feeding = unsafe extern "C" fn(*const c_void, *const c_void) -> *mut c_void;

fn feeding_vertex(hosted: &Hosted) -> Result<Feeding> {
    // SAFETY: its signature in `amd/tests/radv_host.c`.
    Ok(unsafe { entry_point(hosted.symbol(c"stela_radv_host_feeding_vertex")?) })
}

/// What the corpus made: counts, and each refusal's reason with how often.
#[derive(Default)]
struct Sweep {
    functions: usize,
    compute: usize,
    graphics: usize,
    skipped: usize,
    /// Kernels built again from the driver's cache data alone.
    cached: usize,
    refused: BTreeMap<String, usize>,
    /// The first function each reason refused, for the log.
    first: BTreeMap<String, String>,
}

type HostDevice = Device<NativeBackend, AirCompiler>;

impl Sweep {
    fn refuse(&mut self, library: &str, entry: &str, reason: String) {
        println!("  {library} {entry}: refused: {reason}");
        self.first
            .entry(reason.clone())
            .or_insert_with(|| format!("{library} {entry}"));
        *self.refused.entry(reason).or_default() += 1;
    }

    fn library(
        &mut self,
        device: &mut HostDevice,
        feeding: (Feeding, NonNull<c_void>),
        path: &Path,
    ) -> Result<()> {
        let file = fs::read(path)?;
        let bytes = air_slice(&file).to_vec();
        drop(file);
        let label = path
            .file_name()
            .context("a library file")?
            .to_string_lossy()
            .into_owned();
        let names: Vec<String> = Library::parse(&bytes)
            .map_err(|error| anyhow::anyhow!("{}: not a Metal library: {error:?}", path.display()))?
            .functions()
            .iter()
            .map(|function| function.name.to_owned())
            .collect();
        let library = device
            .compiler_mut()
            .add_library(bytes)
            .map_err(|error| anyhow::anyhow!("{label}: {error:?}"))?;
        for entry in &names {
            // Names the function a driver abort leaves the log on.
            eprintln!("  building {label} {entry}");
            self.function(device, feeding, library, &label, entry);
        }
        device
            .compiler_mut()
            .remove_library(library)
            .map_err(|error| anyhow::anyhow!("{label}: {error:?}"))?;
        Ok(())
    }

    fn function(
        &mut self,
        device: &mut HostDevice,
        (feeding, options): (Feeding, NonNull<c_void>),
        library: u64,
        label: &str,
        entry: &str,
    ) {
        // Each constant the function needs takes zero, of its type's size,
        // as the suite's own pipelines give some value.
        let constants = match device.compiler_mut().constants(library, entry) {
            Ok(constants) => constants
                .iter()
                .filter(|constant| constant.required)
                .map(|constant| {
                    let (bytes, lanes) = constant.data_type.lanes().unwrap_or((4, 1));
                    (constant.index, vec![0; bytes * lanes])
                })
                .collect(),
            Err(error) => {
                let detail = refusal(device, error);
                return self.refuse(label, entry, format!("constants: {detail}"));
            }
        };
        let mut specialization = Specialization {
            constants,
            ..Specialization::default()
        };
        let reflected = device
            .compiler_mut()
            .reflect(library, entry, &specialization);
        let interface = match reflected {
            Ok(Reflection::Stage(interface)) => interface,
            // Visible functions link into the stages that call them.
            Ok(Reflection::Visible { .. }) => return,
            Err(error) => {
                let detail = refusal(device, error);
                return self.refuse(label, entry, format!("reflection: {detail}"));
            }
        };
        self.functions += 1;
        match interface.stage {
            Stage::Kernel | Stage::Mesh | Stage::Object => {
                let threads = interface.max_total_threads.unwrap_or(64).clamp(1, 64);
                specialization.workgroup_size = Some([threads, 1, 1]);
                specialization.threadgroup_lengths = interface
                    .threadgroup
                    .iter()
                    .map(|buffer| {
                        (
                            buffer.index,
                            buffer.declared_bytes.max(16).next_multiple_of(16),
                        )
                    })
                    .collect();
            }
            Stage::Vertex | Stage::Fragment => {
                specialization.varyings = interface.varyings.clone();
                specialization.samples = 1;
            }
        }
        if interface.patch.is_some()
            || (interface.stage == Stage::Kernel && !interface.attributes.is_empty())
        {
            // Post-tessellation vertex functions and stage-in kernels need
            // their pipeline's descriptors, which a sweep has none of.
            self.skipped += 1;
            return;
        }
        // Each constexpr sampler takes a sampler of its own, as the device
        // gives it from its heap.
        match device
            .compiler_mut()
            .sampler_states(library, entry, &specialization)
        {
            Ok(states) => {
                specialization.samplers = states.into_iter().zip(1..).collect();
            }
            Err(error) => {
                let detail = refusal(device, error);
                return self.refuse(label, entry, format!("sampler states: {detail}"));
            }
        }
        let compiled = match device
            .compiler_mut()
            .compile(library, entry, &specialization)
        {
            // A kernel of an implicit imageblock is a tile function, which
            // only a tile render pipeline compiles.
            Err(error) if interface.stage == Stage::Kernel => {
                let detail = refusal(device, error);
                if !detail.contains("outside a tile function") {
                    return self.refuse(label, entry, format!("compile: {detail}"));
                }
                as_tile(&mut specialization);
                device
                    .compiler_mut()
                    .compile(library, entry, &specialization)
            }
            compiled => compiled,
        };
        let shader = match compiled {
            Ok(shader) => shader,
            Err(error) => {
                let detail = refusal(device, error);
                return self.refuse(label, entry, format!("compile: {detail}"));
            }
        };
        let code = shader.code;
        let built = match interface.stage {
            Stage::Kernel => self.kernel(device, code).map(|pipeline| (pipeline, None)),
            Stage::Vertex => graphics(
                device,
                vec![(ShaderStages::VERTEX, code)],
                &shader.interface,
            )
            .map(|pipeline| (pipeline, None)),
            Stage::Mesh => graphics(device, vec![(ShaderStages::MESH, code)], &shader.interface)
                .map(|pipeline| (pipeline, None)),
            Stage::Fragment => {
                // SAFETY: the compiler's NIR, and RADV's vertex options.
                let vertex = unsafe { feeding(code.0 as usize as *const c_void, options.as_ptr()) };
                let vertex = ShaderCode(vertex as usize as u64);
                graphics(
                    device,
                    vec![
                        (ShaderStages::VERTEX, vertex),
                        (ShaderStages::FRAGMENT, code),
                    ],
                    &shader.interface,
                )
                .map(|pipeline| (pipeline, Some(vertex)))
            }
            Stage::Object => {
                self.skipped += 1;
                device.compiler_mut().release(code);
                return;
            }
        };
        match built {
            Ok((pipeline, vertex)) => {
                if interface.stage == Stage::Kernel {
                    self.compute += 1;
                } else {
                    self.graphics += 1;
                }
                device.backend().destroy(Object::Pipeline(pipeline));
                if let Some(vertex) = vertex {
                    device.compiler_mut().release(vertex);
                }
            }
            Err(error) => self.refuse(label, entry, format!("pipeline: {error:?}")),
        }
        device.compiler_mut().release(code);
    }
}

/// The Metal library in `file`: the file itself, or the slice of a
/// universal file (as a framework's `default.metallib` ships, its AIR beside
/// each GPU's binaries) that holds one.
fn air_slice(file: &[u8]) -> &[u8] {
    let word = |at: usize| -> Option<usize> {
        let bytes = file.get(at..at + 4)?;
        Some(u32::from_be_bytes(bytes.try_into().ok()?) as usize)
    };
    let (entry, wide) = match word(0) {
        Some(0xcafe_babe) => (20, false),
        Some(0xcafe_babf) => (32, true),
        _ => return file,
    };
    let count = word(4).unwrap_or(0);
    (0..count)
        .filter_map(|index| {
            let at = 8 + index * entry;
            let (offset, size) = if wide {
                let pair = |at| Some(word(at)? << 32 | word(at + 4)?);
                (pair(at + 8)?, pair(at + 16)?)
            } else {
                (word(at + 8)?, word(at + 12)?)
            };
            file.get(offset..offset.checked_add(size)?)
        })
        .find(|slice| slice.starts_with(b"MTLB"))
        .unwrap_or(file)
}

/// `specialization` as a tile render pipeline compiles its tile function
/// (`pipeline.rs`): a 16 x 16 tile's threads in one linear workgroup of
/// dynamic threadgroups, over half4 color attachments of one sample.
fn as_tile(specialization: &mut Specialization) {
    // R16G16B16A16_SFLOAT, written whole.
    let color = StorageAttachment {
        format: 97,
        bytes: 8,
        write_mask: 0xf,
        ..StorageAttachment::default()
    };
    specialization.workgroup_size = Some([256, 1, 1]);
    specialization.dynamic_threadgroups = true;
    specialization.threadgroup_lengths.clear();
    specialization.tile = true;
    specialization.samples = 1;
    specialization.storage_attachments = [Some(color); MAX_COLOR_ATTACHMENTS];
}

impl Sweep {
    /// A kernel's pipeline, built into a pipeline cache whose data a new
    /// cache then takes, from which the driver must build it again without
    /// compiling, as a warm boot's pipeline store does
    /// (`native/pipeline_store.rs`).
    fn kernel(&mut self, device: &mut HostDevice, code: ShaderCode) -> Result<Pipeline, Error> {
        let info = |source| ComputePipelineInfo {
            code,
            required_subgroup_size: 0,
            full_subgroups: false,
            indirect_bindable: false,
            source,
        };
        let backend = device.backend();
        let cache = backend.create_pipeline_cache(&[])?;
        let built = backend.create_compute_pipeline(&info(PipelineSource::Cached(cache)));
        let data = built.is_ok().then(|| backend.pipeline_cache_data(cache));
        backend.destroy(Object::PipelineCache(cache));
        let pipeline = built?;
        let reloaded = backend.create_pipeline_cache(&data.expect("built")?)?;
        match backend.create_compute_pipeline(&info(PipelineSource::Archived(reloaded))) {
            Ok(again) => {
                self.cached += 1;
                backend.destroy(Object::Pipeline(again));
            }
            Err(Error::NotArchived) => {}
            Err(error) => {
                backend.destroy(Object::PipelineCache(reloaded));
                backend.destroy(Object::Pipeline(pipeline));
                return Err(error);
            }
        }
        backend.destroy(Object::PipelineCache(reloaded));
        Ok(pipeline)
    }
}

/// A refusal's detail: the compiler's own account when it kept one.
fn refusal(device: &mut HostDevice, error: Error) -> String {
    match device.compiler_mut().take_refusal() {
        Some(refusal) => format!("{error:?}: {}", refusal.detail),
        None => format!("{error:?}"),
    }
}

/// A graphics pipeline of `stages` over the device's layout, with a color
/// attachment for each of the fragment function's outputs.
fn graphics(
    device: &mut HostDevice,
    stages: Vec<(ShaderStages, ShaderCode)>,
    interface: &stela_drivers::gpu::metal::shader::Interface,
) -> Result<Pipeline, Error> {
    let mut color_formats = vec![None; 8];
    for output in &interface.color_outputs {
        // R16G16B16A16_SFLOAT, R32G32B32A32_UINT, R32G32B32A32_SINT.
        let raw = match output.numeric {
            NumericClass::Float => 97,
            NumericClass::Uint => 107,
            NumericClass::Sint => 108,
        };
        if let Some(slot) = color_formats.get_mut(output.index as usize) {
            *slot = Format::from_raw(raw);
        }
    }
    let info = GraphicsPipelineInfo {
        stages,
        vertex_input: VertexInput::default(),
        topology: PrimitiveTopology::TriangleList,
        patch_control_points: 0,
        samples: SampleCounts::C1,
        alpha_to_coverage: false,
        alpha_to_one: false,
        line_rasterization: LineRasterization::Default,
        color_blend: vec![
            ColorBlendAttachment {
                blend_enable: false,
                src_color: BlendFactor::One,
                dst_color: BlendFactor::Zero,
                color_op: BlendOp::Add,
                src_alpha: BlendFactor::One,
                dst_alpha: BlendFactor::Zero,
                alpha_op: BlendOp::Add,
                write_mask: ColorComponents::R
                    .union(ColorComponents::G)
                    .union(ColorComponents::B)
                    .union(ColorComponents::A),
            };
            color_formats.len()
        ],
        color_formats,
        depth_format: interface
            .writes_depth
            .then(|| Format::from_raw(126))
            .flatten(),
        stencil_format: interface
            .writes_stencil
            .then(|| Format::from_raw(127))
            .flatten(),
        dynamic_states: Vec::new(),
        source: PipelineSource::Compiled,
        indirect_bindable: false,
    };
    device.backend().create_graphics_pipeline(&info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn universal_files_give_their_air_slice() {
        let mut file = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2];
        for (offset, size) in [(48u32, 4u32), (52, 8)] {
            file.extend_from_slice(&[0; 8]);
            file.extend_from_slice(&offset.to_be_bytes());
            file.extend_from_slice(&size.to_be_bytes());
            file.extend_from_slice(&[0; 4]);
        }
        file.extend_from_slice(b"\xcf\xfa\xed\xfeMTLB1234");
        assert_eq!(air_slice(&file), b"MTLB1234");
        assert_eq!(air_slice(b"MTLB0000"), b"MTLB0000");
        // A slice past the file's end is no slice.
        file[23] = 200;
        assert_eq!(air_slice(&file), b"MTLB1234");
        file[43] = 200;
        assert_eq!(air_slice(&file).len(), file.len());
    }

    #[test]
    fn families_are_named_by_amdgpus_header() {
        let header = "#define AMDGPU_FAMILY_NV\t\t\t143 /* Navi10 */\n\
                      #define AMDGPU_FAMILY_GC_10_3_6\t\t149 /* GC 10.3.6 */\n";
        assert_eq!(family_name(header, 143).as_deref(), Some("NV"));
        assert_eq!(family_name(header, 149).as_deref(), Some("GC_10_3_6"));
        assert_eq!(family_name(header, 150), None);
    }
}
