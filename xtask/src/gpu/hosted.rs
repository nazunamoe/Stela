//! What the host tests of the image's Vulkan drivers share: Mesa built for
//! this machine with the image's options and a test's own, a test program
//! linked from the inputs of a driver's link as Mesa links that driver, with
//! Stela's own units first, and the kernel driver's ioctl table read from
//! its unchanged Linux source. Each driver's test (`anv.rs`, `radv.rs`)
//! names its driver, its DRM shim, its program and the devices it runs on.
use super::support::{capture, third_party, tool};
use super::toolchain::GENERATORS;
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// The Meson options of the image's Mesa build a host test changes: Mesa's
/// assertions stay on, and its DRM shim is built.
const REPLACED: [&str; 3] = ["-Dbuildtype=", "-Dtools=", "-Dvulkan-drivers="];
const ADDED: [&str; 2] = ["-Dbuildtype=debugoptimized", "-Dtools=drm-shim"];

/// The image's Meson options with a host test's changes, for the one Vulkan
/// driver `driver` with its own `options`.
pub(super) fn arguments(driver: &str, options: &[&str]) -> Vec<String> {
    super::nvk::OPTIONS
        .iter()
        .filter(|option| !REPLACED.iter().any(|replaced| option.starts_with(replaced)))
        .chain(ADDED.iter())
        .map(|option| (*option).to_owned())
        .chain(std::iter::once(format!("-Dvulkan-drivers={driver}")))
        .chain(options.iter().map(|option| (*option).to_owned()))
        .collect()
}

/// Configures Mesa for this machine in `build` with `arguments`, again
/// whenever they change, as `directory` records them. Only the `.pc` files
/// in `packages` are found: a host's own packages, such as SPIRV-Tools, must
/// not change what the driver is built with.
pub(super) fn configure(
    directory: &Path,
    build: &Path,
    packages: &Path,
    path: &std::ffi::OsStr,
    arguments: &[String],
) -> Result<()> {
    let record = directory.join("configuration");
    let joined = arguments.join("\n") + "\n";
    if build.join("build.ninja").is_file()
        && fs::read_to_string(&record).is_ok_and(|recorded| recorded == joined)
    {
        return Ok(());
    }
    if build.is_dir() {
        fs::remove_dir_all(build)?;
    }
    let _ = fs::remove_file(&record);
    let native = directory.join("native.ini");
    let pkg_config = match crate::tools::which("pkg-config") {
        Some(host) => super::support::slashed(&host),
        None => super::toolchain::pkg_config()?,
    };
    fs::write(
        &native,
        format!("[binaries]\npkg-config = '{pkg_config}'\n"),
    )?;
    crate::tools::exec(
        Command::new(tool("meson", GENERATORS)?)
            .arg("setup")
            .arg(build)
            .arg(third_party("mesa"))
            .arg("--native-file")
            .arg(&native)
            .args(arguments)
            .env("PATH", path)
            .env("PKG_CONFIG_PATH", packages),
    )?;
    fs::write(&record, joined)?;
    Ok(())
}

/// `target`'s link command, as Meson would run it.
pub(super) fn link_command(build: &Path, target: &str) -> Result<Vec<String>> {
    let commands = capture(
        Command::new(tool("ninja", GENERATORS)?)
            .arg("-C")
            .arg(build)
            .args(["-t", "commands", target]),
    )?;
    let line = commands.lines().last().context("no link command")?;
    shlex::split(line).context("the driver's link command")
}

/// The inputs of a link command that the build makes: its relative objects
/// and archives.
pub(super) fn built_inputs(link: &[String]) -> Vec<String> {
    link.iter()
        .filter(|word| word.ends_with(".a") || word.ends_with(".o"))
        .filter(|word| !word.starts_with('/'))
        .cloned()
        .collect()
}

/// `model`'s compile command with its source and output replaced.
pub(super) fn compile_command(
    build: &Path,
    model: &str,
    source: &Path,
    object: &Path,
) -> Result<Command> {
    let entries: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(build.join("compile_commands.json"))?)?;
    let command = entries
        .as_array()
        .context("compile_commands.json")?
        .iter()
        .find(|entry| {
            entry["file"]
                .as_str()
                .is_some_and(|file| file.ends_with(model))
        })
        .and_then(|entry| entry["command"].as_str())
        .with_context(|| format!("no compile command for {model}"))?;
    let words = retargeted(&shlex::split(command).context("compile command")?);
    let mut compile = Command::new(&words[0]);
    compile
        .current_dir(build)
        .args(&words[1..])
        .arg("-c")
        .arg(source)
        .arg("-o")
        .arg(object);
    Ok(compile)
}

/// A compile command without its source, output and dependency file.
fn retargeted(words: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut skip = false;
    for word in words {
        if std::mem::take(&mut skip) {
            continue;
        }
        match word.as_str() {
            "-o" | "-c" | "-MF" | "-MQ" => skip = true,
            "-MD" => {}
            source if !source.starts_with('-') && source.ends_with(".c") => {}
            _ => kept.push(word.clone()),
        }
    }
    kept
}

/// The driver's link with the shared-library words taken out and the test's
/// objects put first, so their definitions are the ones linked, then
/// `extra`: libraries and linker flags of the test's own.
pub(super) fn program_link(
    link: &[String],
    program: &Path,
    objects: &[PathBuf],
    extra: &[String],
) -> Vec<String> {
    let mut words = vec![link[0].clone(), "-rdynamic".into(), "-o".into()];
    words.push(program.display().to_string());
    words.extend(objects.iter().map(|object| object.display().to_string()));
    let mut skip = false;
    for word in &link[1..] {
        if std::mem::take(&mut skip) {
            continue;
        }
        match word.as_str() {
            "-o" | "-Wl,--version-script" => skip = true,
            "-shared" | "-Wl,-Bsymbolic" | "-Wl,--as-needed" | "-Wl,--no-undefined" => {}
            other if other.starts_with("-Wl,-soname") => {}
            _ => words.push(word.clone()),
        }
    }
    words.extend(extra.iter().cloned());
    words
}

/// The driver's link made a shared library of the test's own at `library`:
/// its objects first, then the driver's inputs, without Mesa's export list
/// and name, so every symbol the test's objects export stays visible, then
/// `extra`.
pub(super) fn library_link(
    link: &[String],
    library: &Path,
    objects: &[PathBuf],
    extra: &[String],
) -> Vec<String> {
    let mut words = vec![link[0].clone(), "-shared".into(), "-o".into()];
    words.push(library.display().to_string());
    words.extend(objects.iter().map(|object| object.display().to_string()));
    let mut skip = false;
    for word in &link[1..] {
        if std::mem::take(&mut skip) {
            continue;
        }
        match word.as_str() {
            "-o" | "-Wl,--version-script" => skip = true,
            "-shared" | "-Wl,--as-needed" => {}
            other
                if other.starts_with("-Wl,-soname")
                    || other.starts_with("-Wl,--version-script=") => {}
            _ => words.push(word.clone()),
        }
    }
    words.extend(extra.iter().cloned());
    words
}

/// A preloaded shim library's link, Meson's with its output at `library`
/// and undefined symbols allowed: the shim's libdrm calls resolve in the
/// test program, whose libdrm stand-in is exported.
pub(super) fn preloaded_link(link: &[String], library: &Path) -> Vec<String> {
    let mut words = Vec::new();
    let mut output = false;
    for word in link {
        if std::mem::take(&mut output) {
            words.push(library.display().to_string());
            continue;
        }
        match word.as_str() {
            "-Wl,--no-undefined" => {}
            "-o" => {
                output = true;
                words.push(word.clone());
            }
            _ => words.push(word.clone()),
        }
    }
    words
}

/// The host's `libdrm.so.2`, whose mode calls the display WSI units take;
/// the image leaves those units out. Its other calls are the test's own
/// (`compat/mesa/drm.c`), linked first.
pub(super) fn host_libdrm() -> Result<PathBuf> {
    let libdrm =
        PathBuf::from(capture(Command::new("cc").arg("-print-file-name=libdrm.so.2"))?.trim());
    anyhow::ensure!(
        libdrm.is_absolute() && libdrm.is_file(),
        "this host has no libdrm.so.2, which Mesa's display WSI units link on the host"
    );
    Ok(libdrm)
}

/// The ioctls a kernel driver's table registers
/// (`DRM_IOCTL_DEF_DRV(<prefix>...)`), by the name after `prefix`.
pub(super) fn registered_ioctls(source: &str, prefix: &str) -> Vec<String> {
    source
        .split(&format!("DRM_IOCTL_DEF_DRV({prefix}"))
        .skip(1)
        .filter_map(|rest| rest.split(',').next())
        .map(|name| name.trim().to_owned())
        .collect()
}

/// The ioctls of a kernel driver's table that serve a render node: those
/// registered with `DRM_RENDER_ALLOW` and a handler of the driver's own, not
/// the DRM core's `drm_noop` or `drm_invalid_op`, which answer legacy
/// ioctls without doing anything.
pub(super) fn render_ioctls(source: &str, prefix: &str) -> Vec<String> {
    source
        .split(&format!("DRM_IOCTL_DEF_DRV({prefix}"))
        .skip(1)
        .filter_map(|rest| {
            let entry = rest.split(')').next()?;
            let mut fields = entry.split(',').map(str::trim);
            let (name, handler, flags) = (fields.next()?, fields.next()?, fields.next()?);
            (flags.contains("DRM_RENDER_ALLOW")
                && !matches!(handler, "drm_noop" | "drm_invalid_op"))
            .then(|| name.to_owned())
        })
        .collect()
}

/// What a test's exit line `marker NAME=count ...` says the driver used.
pub(super) fn used_ioctls(output: &str, marker: &str) -> BTreeMap<String, u32> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix(marker))
        .flat_map(str::split_whitespace)
        .filter_map(|pair| {
            let (name, count) = pair.split_once('=')?;
            Some((name.to_owned(), count.parse().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drivers_registered_ioctls_are_read_from_its_table() {
        let source = "const struct drm_ioctl_desc amdgpu_ioctls_kms[] = {\n\
            \tDRM_IOCTL_DEF_DRV(AMDGPU_GEM_CREATE, amdgpu_gem_create_ioctl, DRM_AUTH|DRM_RENDER_ALLOW),\n\
            \tDRM_IOCTL_DEF_DRV(AMDGPU_CTX, amdgpu_ctx_ioctl,\n\
            \t\t\t  DRM_AUTH|DRM_RENDER_ALLOW),\n};\n";
        assert_eq!(registered_ioctls(source, "AMDGPU_"), ["GEM_CREATE", "CTX"]);
        assert!(registered_ioctls(source, "XE_").is_empty());
    }

    #[test]
    fn a_render_node_serves_only_its_drivers_own_render_ioctls() {
        let source = "static const struct drm_ioctl_desc i915_ioctls[] = {\n\
            \tDRM_IOCTL_DEF_DRV(I915_INIT, drm_noop, DRM_AUTH|DRM_MASTER|DRM_ROOT_ONLY),\n\
            \tDRM_IOCTL_DEF_DRV(I915_GETPARAM, i915_getparam_ioctl, DRM_RENDER_ALLOW),\n\
            \tDRM_IOCTL_DEF_DRV(I915_GEM_EXECBUFFER, drm_invalid_op, DRM_AUTH),\n\
            \tDRM_IOCTL_DEF_DRV(I915_GEM_PIN, i915_gem_reject_pin_ioctl, DRM_AUTH|DRM_ROOT_ONLY),\n\
            \tDRM_IOCTL_DEF_DRV(I915_GEM_WAIT, i915_gem_wait_ioctl,\n\
            \t\t\t  DRM_RENDER_ALLOW),\n};\n";
        assert_eq!(render_ioctls(source, "I915_"), ["GETPARAM", "GEM_WAIT"]);
        assert_eq!(registered_ioctls(source, "I915_").len(), 5);
    }

    #[test]
    fn the_exit_line_names_each_ioctl_once() {
        let output = "MESA: warning: x\namdgpu ioctls: INFO=19 GEM_VA=28 CS=4\n";
        let used = used_ioctls(output, "amdgpu ioctls:");
        assert_eq!(used.get("GEM_VA"), Some(&28));
        assert_eq!(used.len(), 3);
        assert!(used_ioctls("nothing\n", "amdgpu ioctls:").is_empty());
    }

    #[test]
    fn a_compile_command_loses_its_source_output_and_dependencies() {
        let words: Vec<String> = "cc -Isrc -MD -MQ a.o -MF a.o.d -o a.o -c ../src/a.c -O2"
            .split(' ')
            .map(str::to_owned)
            .collect();
        assert_eq!(retargeted(&words), ["cc", "-Isrc", "-O2"]);
    }

    #[test]
    fn the_program_links_the_drivers_inputs_after_its_own_objects() {
        let link: Vec<String> =
            "c++ -o libvulkan_radeon.so x.o -Wl,--as-needed -Wl,--no-undefined \
             -shared -Wl,-soname,libvulkan_radeon.so a.a -Wl,--version-script v.sym -lm"
                .split(' ')
                .map(str::to_owned)
                .collect();
        let words = program_link(
            &link,
            Path::new("/t/radv_device"),
            &[PathBuf::from("/t/drm.o")],
            &["/lib/libdrm.so.2".to_owned()],
        );
        assert_eq!(
            words,
            [
                "c++",
                "-rdynamic",
                "-o",
                "/t/radv_device",
                "/t/drm.o",
                "x.o",
                "a.a",
                "-lm",
                "/lib/libdrm.so.2"
            ]
        );
        assert_eq!(
            built_inputs(&link),
            ["x.o", "a.a"],
            "only what the build makes"
        );
    }

    #[test]
    fn a_hosted_library_keeps_its_exports_and_the_drivers_inputs() {
        let link: Vec<String> = [
            "cc",
            "-o",
            "src/amd/vulkan/libvulkan_radeon.so",
            "-shared",
            "-Wl,-soname,libvulkan_radeon.so",
            "-Wl,--version-script",
            "vulkan.sym",
            "-Wl,--as-needed",
            "-Wl,--no-undefined",
            "src/amd/vulkan/libvulkan_radeon.so.p/radv_device.c.o",
            "src/util/libmesa_util.a",
        ]
        .map(str::to_owned)
        .to_vec();
        let words = library_link(
            &link,
            Path::new("/t/libhost.so"),
            &[PathBuf::from("/t/own.o")],
            &["-lllvm".to_owned()],
        );
        assert_eq!(
            words,
            [
                "cc",
                "-shared",
                "-o",
                "/t/libhost.so",
                "/t/own.o",
                "-Wl,--no-undefined",
                "src/amd/vulkan/libvulkan_radeon.so.p/radv_device.c.o",
                "src/util/libmesa_util.a",
                "-lllvm",
            ]
        );
    }

    #[test]
    fn a_preloaded_shim_links_with_its_libdrm_calls_left_to_the_program() {
        let link: Vec<String> =
            "cc -o src/shim.so -Wl,--as-needed -Wl,--no-undefined -shared a.o b.a"
                .split(' ')
                .map(str::to_owned)
                .collect();
        assert_eq!(
            preloaded_link(&link, Path::new("/t/shim.so")),
            [
                "cc",
                "-o",
                "/t/shim.so",
                "-Wl,--as-needed",
                "-shared",
                "a.o",
                "b.a"
            ]
        );
    }

    #[test]
    fn the_test_build_keeps_the_images_options_but_its_own() {
        let arguments = arguments("amd", &["-Damd-use-llvm=false"]);
        assert!(arguments.contains(&"-Dbuildtype=debugoptimized".to_owned()));
        assert!(!arguments.contains(&"-Dbuildtype=release".to_owned()));
        assert!(arguments.contains(&"-Dtools=drm-shim".to_owned()));
        assert!(!arguments.contains(&"-Dtools=".to_owned()));
        assert!(arguments.contains(&"-Dvulkan-drivers=amd".to_owned()));
        assert!(arguments.contains(&"-Damd-use-llvm=false".to_owned()));
        assert!(arguments.contains(&"-Dllvm=disabled".to_owned()));
        assert!(arguments.contains(&"-Dmesa-clc=system".to_owned()));
    }
}
