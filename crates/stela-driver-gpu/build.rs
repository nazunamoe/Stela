#[cfg(feature = "amdgpu-port")]
#[path = "build/amdgpu.rs"]
mod amdgpu;
#[cfg(any(feature = "xe-port", feature = "i915-port"))]
#[path = "build/anv_tables.rs"]
mod anv_tables;
#[path = "src/drm_version.rs"]
mod drm_version;
#[cfg(feature = "i915-port")]
#[path = "build/i915.rs"]
mod i915;
// Its early-quirk reader serves xe's builds too; its ID table reader is
// i915's.
#[cfg(any(feature = "xe-port", feature = "i915-port"))]
#[cfg_attr(not(feature = "i915-port"), allow(dead_code))]
#[path = "build/i915_tables.rs"]
mod i915_tables;
#[cfg(feature = "linux-drm")]
#[path = "build/image.rs"]
mod image;
#[cfg(feature = "linux-drm")]
#[path = "build/imports.rs"]
mod imports;
#[cfg(any(feature = "nouveau-port", feature = "xe-port", feature = "i915-port"))]
#[path = "build/kbuild.rs"]
mod kbuild;
#[cfg(feature = "linux-kernel")]
#[path = "build/native.rs"]
mod native;
#[cfg(feature = "nouveau-port")]
#[path = "build/nouveau.rs"]
mod nouveau;
#[path = "build/nouveau_firmware.rs"]
mod nouveau_firmware;
#[cfg(feature = "xe-port")]
#[path = "build/xe.rs"]
mod xe;
#[cfg(feature = "xe-port")]
#[path = "build/xe_tables.rs"]
mod xe_tables;

/// The Vulkan drivers `cargo xtask gpu nvk` linked into the image's archives,
/// as it lists them beside them: with `metal-linked`, Intel's sets
/// `stela_vulkan_intel` and AMD's `stela_vulkan_amd`, so the crate names only
/// loader entries the image holds. NVK is always there.
fn vulkan_drivers() {
    const OPTIONAL: [(&str, &str); 2] =
        [("intel", "stela_vulkan_intel"), ("amd", "stela_vulkan_amd")];
    for (_, cfg) in OPTIONAL {
        println!("cargo:rustc-check-cfg=cfg({cfg})");
    }
    if std::env::var_os("CARGO_FEATURE_METAL_LINKED").is_none() {
        return;
    }
    let list = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gpu/nvk/vulkan-drivers");
    println!("cargo:rerun-if-changed={}", list.display());
    let listed = std::fs::read_to_string(&list).unwrap_or_default();
    for (name, cfg) in OPTIONAL {
        if listed.lines().any(|linked| linked == name) {
            println!("cargo:rustc-cfg={cfg}");
        }
    }
}

/// Each driver stack's DRM version, read from its driver's source
/// (`stack::Kernel::version`): `None` for a driver whose source this
/// checkout does not hold.
fn drm_versions() {
    const DRM: &str = "../../third_party/linux/drivers/gpu/drm";
    const DRIVERS: [(&str, &str, &str); 4] = [
        ("NOUVEAU", "nouveau/nouveau_drv.h", "DRIVER_"),
        ("AMDGPU", "amd/amdgpu/amdgpu_drv.c", "KMS_DRIVER_"),
        ("XE", "xe/xe_drv.h", "DRIVER_"),
        ("I915", "i915/i915_driver.c", "DRIVER_"),
    ];
    let mut text = String::new();
    for (name, file, prefix) in DRIVERS {
        let path = std::path::Path::new(DRM).join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let version = std::fs::read_to_string(&path)
            .ok()
            .and_then(|source| drm_version::version(&source, prefix));
        text.push_str(&format!(
            "/// `{file}`'s `{prefix}MAJOR`, `MINOR` and `PATCHLEVEL`.\n\
             pub const {name}: Option<(u32, u32, u32)> = {version:?};\n"
        ));
    }
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"));
    std::fs::write(output.join("drm_versions.rs"), text).expect("write the drivers' DRM versions");
}

/// Write the Intel devices ANV opens for the Rust side (`src/anv.rs`): each
/// PCI device ID of Mesa's Intel table whose family has a graphics version
/// ANV creates a physical device for, without Mesa's force probe. The Intel
/// kernel drivers are handed those GPUs only: on another no Vulkan driver of
/// the image opens the Metal device.
#[cfg(any(feature = "xe-port", feature = "i915-port"))]
fn anv_devices() {
    use std::{env, fs, path::Path};
    const MESA: &str = "../../third_party/mesa";
    let read = |member: &str| {
        let path = Path::new(MESA).join(member);
        println!("cargo:rerun-if-changed={}", path.display());
        fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "read {} (`cargo xtask gpu fetch` checks Mesa out): {error}",
                path.display()
            )
        })
    };
    let listed = anv_tables::devices(
        &read("include/pci_ids/iris_pci_ids.h"),
        &read("src/intel/dev/intel_device_info.c"),
    )
    .unwrap_or_else(|error| panic!("Mesa's Intel device table: {error}"));
    let versions = anv_tables::anv_versions(&read("src/intel/vulkan/anv_physical_device.c"))
        .unwrap_or_else(|error| panic!("ANV's graphics versions: {error}"));
    let mut text = format!(
        "/// The graphics versions `anv_physical_device_try_create` goes on with.\n\
         pub const VERSIONS: core::ops::RangeInclusive<u8> = {}..={};\n\
         /// Mesa's Intel table (`iris_pci_ids.h`): each PCI device ID of those\n\
         /// versions it lists without a force probe, with its graphics version.\n\
         pub(crate) const OPENED: &[(u16, u8)] = &[\n",
        versions.0, versions.1
    );
    for device in anv_tables::opened(&listed, versions) {
        text.push_str(&format!(
            "    ({device:#06x}, {}),\n",
            listed[&device].version
        ));
    }
    text.push_str("];\n");
    let output = env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR");
    fs::write(Path::new(&output).join("anv_devices.rs"), text).expect("write ANV's device table");
}

/// Write the x86 early quirks' stolen memory methods for the Rust side
/// (`src/intel_stolen.rs`), read from `intel_early_ids` in
/// `early-quirks.c` with Linux's ID lists: i915's GPUs and xe's before
/// graphics 12.70 take their stolen memory from what those methods find.
#[cfg(any(feature = "xe-port", feature = "i915-port"))]
fn intel_stolen() {
    use std::{fs, path::Path};
    const EARLY_QUIRKS: &str = "../../third_party/linux/arch/x86/kernel/early-quirks.c";
    const PCI_IDS: &str = "../../third_party/linux/include/drm/intel/pciids.h";
    for watched in [EARLY_QUIRKS, PCI_IDS] {
        println!("cargo:rerun-if-changed={watched}");
    }
    let methods = i915_tables::stolen_methods(
        &fs::read_to_string(EARLY_QUIRKS).expect("read Linux's x86 early quirks"),
        &fs::read_to_string(PCI_IDS).expect("read Linux's Intel PCI IDs"),
    )
    .unwrap_or_else(|error| panic!("the early quirks' intel_early_ids: {error}"));
    let mut text = String::from(
        "/// The x86 early quirks' `intel_early_ids`: each device ID with the\n\
         /// method (`<method>_early_ops`) that finds its stolen memory.\n\
         pub(crate) const STOLEN: &[(u16, &str)] = &[\n",
    );
    for (device, method) in &methods {
        text.push_str(&format!("    ({device:#06x}, {method:?}),\n"));
    }
    text.push_str("];\n");
    let output = std::env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR");
    fs::write(Path::new(&output).join("intel_stolen.rs"), text)
        .expect("write the early quirks' stolen memory methods");
}

fn main() {
    nouveau_firmware::validate();
    vulkan_drivers();
    drm_versions();
    // Active ports declare their native source inputs below. Without this
    // baseline Cargo watches every vendored file even when no C port is used.
    println!("cargo:rerun-if-changed=build.rs");
    // and its modules, which derive the ports' tables.
    println!("cargo:rerun-if-changed=build");
    #[cfg(feature = "amdgpu-port")]
    amdgpu::tables();
    #[cfg(any(feature = "xe-port", feature = "i915-port"))]
    anv_devices();
    #[cfg(any(feature = "xe-port", feature = "i915-port"))]
    intel_stolen();
    #[cfg(feature = "xe-port")]
    xe::tables();
    #[cfg(feature = "i915-port")]
    i915::tables();
    // Clippy reads Rust and links nothing, so it needs no C objects.
    println!("cargo:rerun-if-env-changed=CLIPPY_ARGS");
    #[cfg(feature = "linux-kernel")]
    if std::env::var_os("CLIPPY_ARGS").is_some() {
        let tests = stela_c_build::linux_build::Tests::default();
        tests.write();
        #[cfg(feature = "linux-drm")]
        image::write_parameters(&[]);
        return;
    }
    #[cfg(feature = "linux-kernel")]
    let (linux, mut tests) = (
        native::Linux::new(),
        stela_c_build::linux_build::Tests::default(),
    );
    #[cfg(feature = "linux-kernel")]
    linux.tests(&mut tests);
    #[cfg(feature = "linux-drm")]
    {
        #[allow(unused_mut)]
        let mut drivers: Vec<image::Driver> = vec![
            #[cfg(feature = "nouveau-port")]
            nouveau::port(&linux),
            #[cfg(feature = "xe-port")]
            xe::port(&linux),
            #[cfg(feature = "amdgpu-port")]
            amdgpu::port(&linux),
        ];
        // i915 is built last: it keeps the display code it shares with xe
        // apart from the archives built before it. It registers first of the
        // Intel drivers all the same: the image orders drivers by their
        // global ordinal (`build/image.rs`).
        #[cfg(feature = "i915-port")]
        {
            let others = drivers
                .iter()
                .map(|driver| driver.archive.clone())
                .collect::<Vec<_>>();
            drivers.push(i915::port(&linux, &others));
        }
        image::link(&linux, drivers);
    }
    #[cfg(feature = "linux-kernel")]
    tests.write();
}
