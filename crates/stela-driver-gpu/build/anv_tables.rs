//! Readers of Mesa's own tables for ANV, the Intel stacks' Vulkan driver,
//! shared by the build (`build.rs`) and by `cargo xtask gpu anv-test`, which
//! runs ANV on every device they give: the Intel PCI ID table
//! (`include/pci_ids/iris_pci_ids.h`), each family's graphics version
//! (`src/intel/dev/intel_device_info.c`) and the versions ANV creates a
//! device for (`src/intel/vulkan/anv_physical_device.c`). Each refuses a form
//! it does not know, so a changed table is read here first.
use std::collections::BTreeMap;

/// What Mesa's table says of a PCI device ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Listed {
    /// The device information the entry names (`intel_device_info_<family>`).
    pub family: String,
    /// That family's graphics version (`.ver`).
    pub version: u8,
    /// Whether Mesa drives it only by its force probe (`FORCE_PROBE`).
    pub forced: bool,
}

/// Every `CHIPSET` entry of `ids` with its family's graphics version from
/// `info`.
pub fn devices(ids: &str, info: &str) -> Result<BTreeMap<u16, Listed>, String> {
    let versions = versions(info)?;
    let mut devices = BTreeMap::new();
    for line in ids.lines() {
        let Some(entry) = line.trim().strip_prefix("CHIPSET(") else {
            continue;
        };
        let mut fields = entry.split(',').map(str::trim);
        let (Some(id), Some(family)) = (fields.next(), fields.next()) else {
            return Err(format!("Mesa's Intel table has the entry {line:?}"));
        };
        let id = id
            .strip_prefix("0x")
            .and_then(|digits| u16::from_str_radix(digits, 16).ok())
            .ok_or_else(|| format!("Mesa's Intel table lists the device {id:?}"))?;
        let version = *versions
            .get(family)
            .ok_or_else(|| format!("Mesa's device information has no {family} for {id:04x}"))?;
        devices.insert(
            id,
            Listed {
                family: family.to_owned(),
                version,
                forced: entry.contains("FORCE_PROBE"),
            },
        );
    }
    if devices.is_empty() {
        return Err("Mesa's Intel table lists no device".to_owned());
    }
    Ok(devices)
}

/// `text` with each macro of `macros` it names replaced by the macro's body,
/// in place and to any depth below 32, as the preprocessor writes an
/// initializer out. A function-like macro's arguments stay behind its body,
/// which no reader here minds.
fn expand(text: &str, macros: &BTreeMap<&str, &str>, depth: u8) -> Option<String> {
    if depth == 32 {
        return None;
    }
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest.find(|c| !word(c)).unwrap_or(rest.len());
        let (name, after) = rest.split_at(end);
        match macros.get(name) {
            Some(body) => out.push_str(&expand(body, macros, depth + 1)?),
            None => out.push_str(name),
        }
        let gap = after.find(word).unwrap_or(after.len());
        out.push_str(&after[..gap]);
        rest = &after[gap..];
    }
    Some(out)
}

/// The graphics version of each `intel_device_info_<family>` of `info`: the
/// last `.ver` of its initializer with the macros it names expanded, as C
/// takes the last designator (Gen9's features start from Gen8's).
fn versions(info: &str) -> Result<BTreeMap<String, u8>, String> {
    let joined = info.replace("\\\n", " ");
    // Each macro's body by name, object-like or function-like.
    let macros: BTreeMap<&str, &str> = joined
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("#define "))
        .filter_map(|rest| {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let (name, body) = rest.split_at(end);
            // A function-like macro's parameters are no part of its body.
            let body = match body.strip_prefix('(') {
                Some(parameters) => parameters.split_once(')').map_or("", |(_, body)| body),
                None => body,
            };
            (!name.is_empty()).then_some((name, body))
        })
        .collect();
    let mut versions = BTreeMap::new();
    for definition in joined
        .split("struct intel_device_info intel_device_info_")
        .skip(1)
    {
        let Some((family, body)) = definition.split_once(" = {") else {
            continue;
        };
        if !family
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let body = expand(body.split("};").next().unwrap_or(""), &macros, 0)
            .ok_or_else(|| format!("intel_device_info_{family} nests its macros too deep"))?;
        let version = body
            .rsplit(".ver = ")
            .next()
            .filter(|_| body.contains(".ver = "))
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|digits| digits.parse::<u8>().ok())
            .ok_or_else(|| format!("intel_device_info_{family} states no graphics version"))?;
        versions.insert(family.to_owned(), version);
    }
    Ok(versions)
}

/// The graphics versions ANV creates a physical device for, first and last,
/// from `anv_physical_device_try_create` in `source`: the least version it
/// asks the device information for and the one past which it says Vulkan is
/// not yet supported.
pub fn anv_versions(source: &str) -> Result<(u8, u8), String> {
    let number = |after: &str| {
        source.split(after).nth(1).and_then(|rest| {
            rest.trim_start()
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|digits| digits.parse::<u8>().ok())
        })
    };
    let first = number("intel_get_device_info_from_fd(fd, &devinfo,")
        .ok_or("ANV asks for its device information in a form not known here")?;
    // The same floor, stated again for hasvk's devices.
    if number("if (devinfo.ver <") != Some(first) {
        return Err("ANV's two checks of its least graphics version differ".to_owned());
    }
    let last = number("} else if (devinfo.ver >")
        .ok_or("ANV states its greatest graphics version in a form not known here")?;
    if first > last {
        return Err(format!("ANV drives graphics versions {first} to {last}"));
    }
    Ok((first, last))
}

/// The devices of `listed` ANV creates a physical device for without a
/// force probe: those of its versions (`anv_versions`), sorted.
pub fn opened(listed: &BTreeMap<u16, Listed>, versions: (u8, u8)) -> Vec<u16> {
    listed
        .iter()
        .filter(|(_, device)| !device.forced && (versions.0..=versions.1).contains(&device.version))
        .map(|(id, _)| *id)
        .collect()
}
