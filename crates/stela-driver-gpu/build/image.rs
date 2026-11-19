//! The one registration image of a build: the environment's units (common,
//! then DRM's) and the unit of each kernel driver built, in ordinal order, as
//! one Linux kernel registers every driver built into it. With it, what the
//! image and its drivers import that the environment does not supply is
//! compared with the drivers' lists.
use super::native::{self, Linux};
use std::{collections::BTreeSet, env, path::PathBuf};
use stela_c_build::linux_capture::{self as initcalls, Captured, Projection, UnitSpec};

/// Ordinals of the image's units after the common cohort: DRM's own level
/// (its core, helpers and buses) holds 5 to 31, and each driver one from 32
/// on: nouveau 32, i915 33 and its parameters 34 and 35, xe 36 and its
/// display parameters 37, amdgpu 38, with the ordinals between a driver's
/// and the next's for its parameter units. Linux offers a device to the drivers in this order, so
/// i915 comes before xe.
pub const FIRST_DRIVER_ORDINAL: u32 = 32;

/// The kernel services a driver's objects import that the environment lacks
/// or defines only as a trap, by service.
pub type Open = &'static [(&'static str, &'static [&'static str])];

/// One kernel driver compiled for the image.
pub struct Driver {
    /// Its module name, for the check's message.
    pub name: &'static str,
    /// The unit of its own `module_init`, compiled with its registrations
    /// captured.
    pub spec: UnitSpec,
    pub captured: Captured,
    /// Its other captured units, which declare parameters of its module,
    /// each with the ordinal after the one before.
    pub more: Vec<(UnitSpec, Captured)>,
    /// Its objects, with the captured unit's.
    pub archive: PathBuf,
    pub open: Open,
}

/// Project the image over the environment's units and `drivers`', and check
/// what it reaches.
pub fn link(linux: &Linux, mut drivers: Vec<Driver>) {
    drivers.sort_by_key(|driver| driver.spec.scope.global_ordinal);
    assert!(
        drivers
            .iter()
            .all(|driver| driver.spec.scope.global_ordinal >= FIRST_DRIVER_ORDINAL),
        "a driver's unit lies in DRM's ordinals"
    );
    let mut captured = linux.captured.clone();
    let environment_count = captured.len();
    for driver in &drivers {
        captured.push(driver.captured.clone());
        captured.extend(driver.more.iter().map(|(_, unit)| unit.clone()));
    }
    let mut expected = native::environment_units();
    for driver in &drivers {
        expected.push(driver.spec);
        expected.extend(driver.more.iter().map(|(spec, _)| *spec));
    }
    let image = initcalls::source(
        &captured,
        Projection {
            symbol: "stela_linux_gpu_image",
            expected: &expected,
            imported_units: environment_count,
            descriptors_only: false,
        },
    );
    let mut build = stela_c_build::native::configure();
    linux
        .environment
        .apply(&mut build, None, &native::drm_includes(&[]), None);
    stela_c_build::linux_build::upstream_warnings(&mut build);
    build
        .opt_level(2)
        .cargo_metadata(false)
        .file(image)
        .compile("stela-linux-gpu-image");
    let output = stela_c_build::linux_build::out_dir();
    let archive = output.join("libstela-linux-gpu-image.a");
    let mut ports = vec![archive.as_path()];
    ports.extend(drivers.iter().map(|driver| driver.archive.as_path()));
    let mut open = super::imports::open_all(&ports, &[&linux.archive, &linux.common_archive]);
    let listed = drivers
        .iter()
        .flat_map(|driver| driver.open.iter())
        .flat_map(|(_, symbols)| symbols.iter().copied())
        .collect::<BTreeSet<_>>();
    let stale = listed
        .iter()
        .copied()
        .filter(|symbol| !open.remove(*symbol))
        .collect::<Vec<_>>();
    let names = drivers
        .iter()
        .map(|driver| driver.name)
        .collect::<Vec<_>>()
        .join(" and ");
    assert!(
        open.is_empty() && stale.is_empty(),
        "the imports of {names} changed: newly open {open:?}; now supplied {stale:?}"
    );
    let names = captured
        .iter()
        .flat_map(|unit| parameters(unit.object()))
        .collect::<Vec<_>>();
    assert!(
        names.iter().any(|name| name == "ttm.pages_limit"),
        "the image registers no ttm.pages_limit, which the boot sets: {names:?}"
    );
    write_parameters(&names);
    let digest = initcalls::write(
        &output.join("captured-units.bin"),
        &captured[initcalls::COMMON.len()..],
    );
    println!("cargo:output={}", output.display());
    println!("cargo:cohort_sha256={digest}");
    // Hosted programs test the environment and link no driver.
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "uefi") {
        println!("cargo:rustc-link-lib=static=stela-linux-gpu-image");
    }
}

/// The module parameters `object` registers: each `module.parameter` name
/// its `__param_str_` strings hold, which `registration.h` declares beside
/// each parameter's record, as `moduleparam.h` does.
fn parameters(object: &std::path::Path) -> Vec<String> {
    use object::{Object, ObjectSection, ObjectSymbol};
    let bytes =
        std::fs::read(object).unwrap_or_else(|error| panic!("{}: {error}", object.display()));
    let file = object::File::parse(&*bytes)
        .unwrap_or_else(|error| panic!("{}: {error}", object.display()));
    file.symbols()
        .filter(|symbol| {
            symbol
                .name()
                .is_ok_and(|name| name.starts_with("__param_str_"))
        })
        .map(|symbol| {
            let data = symbol
                .section_index()
                .and_then(|index| file.section_by_index(index).ok())
                .and_then(|section| section.data().ok())
                .unwrap_or_else(|| panic!("{}: a parameter name without data", object.display()));
            let start = usize::try_from(symbol.address()).expect("a section offset");
            let text = &data[start..];
            let end = text
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(text.len());
            String::from_utf8(text[..end].to_vec()).expect("a parameter name is text")
        })
        .collect()
}

/// `parameters.rs` in the build's output: `names` as the Rust array
/// `crate::command_line::REGISTERED` includes.
pub fn write_parameters(names: &[String]) {
    let mut names = names.to_vec();
    names.sort();
    names.dedup();
    let list = names
        .iter()
        .map(|name| format!("{name:?},"))
        .collect::<String>();
    let path = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo's output directory"))
        .join("parameters.rs");
    std::fs::write(path, format!("&[{list}]\n")).expect("write the image's parameters");
}
