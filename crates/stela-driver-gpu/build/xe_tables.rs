//! Readers of xe's own tables, shared by its build (`build/xe.rs`) and by
//! `cargo xtask gpu anv-test`, which runs ANV on every device they admit:
//! the PCI ID table (`xe_pci.c` with Linux's `pciids.h`), the firmware table
//! (`xe_uc_fw.c`) and the display code's firmware declarations
//! (`intel_dmc.c`). Each refuses a form it does not know, so a changed table
//! is read here first.
use std::collections::{BTreeMap, BTreeSet};

/// Each device ID of `pciidlist` in `table` with the mask bits, the
/// force-probe requirement, the platform name, whether it has a display
/// engine, whether it is a discrete GPU (`is_dgfx`) and whether the table
/// fixes its graphics IP (`pre_gmdid_graphics_ip`: graphics before 12.70,
/// which report no version of their own), of the descriptor its entry
/// names, the ID lists expanded from `ids`.
pub type Described = (u8, bool, String, bool, bool, bool);
pub fn devices(table: &str, ids: &str) -> Result<BTreeMap<u16, Described>, String> {
    // Each list macro's own IDs and the list macros it includes.
    let mut lists = BTreeMap::new();
    for definition in ids.replace("\\\n", " ").lines() {
        let Some(rest) = definition.strip_prefix("#define ") else {
            continue;
        };
        let Some((name, body)) = rest.split_once("(MACRO__, ...)") else {
            continue;
        };
        let (mut own, mut included) = (Vec::new(), Vec::new());
        // Each call in the body: `MACRO__(0x1234, ...)` is an ID of this
        // list, `OTHER_IDS(MACRO__, ...)` includes another list.
        let pieces = body.split('(').collect::<Vec<_>>();
        for call in pieces.windows(2) {
            let callee = call[0]
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap_or("");
            let argument = call[1].split([',', ')']).next().unwrap_or("").trim();
            if callee == "MACRO__" {
                let id = argument
                    .strip_prefix("0x")
                    .or_else(|| argument.strip_prefix("0X"))
                    .and_then(|digits| u16::from_str_radix(digits, 16).ok())
                    .ok_or_else(|| format!("{name} lists {argument:?}"))?;
                own.push(id);
            } else if argument == "MACRO__" {
                included.push(callee.to_owned());
            }
        }
        lists.insert(name.trim().to_owned(), (own, included));
    }
    // The table's object-like macros, which descriptors include whole
    // (`DG2_FEATURES`).
    let joined = table.replace("\\\n", " ");
    let macros: BTreeMap<&str, &str> = joined
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("#define "))
        .filter_map(|rest| rest.split_once(|c: char| c.is_whitespace()))
        .filter(|(name, _)| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    // Each descriptor's mask.
    let mut masks = BTreeMap::new();
    for descriptor in table.split("struct xe_device_desc ").skip(1) {
        let Some((name, body)) = descriptor.split_once(" = {") else {
            continue;
        };
        // A descriptor's definition, not a pointer to one in a signature.
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let mut body = body.split("};").next().unwrap_or("").to_owned();
        // Included macros may include others (`DG2_FEATURES` names
        // `DGFX_FEATURES`); each is expanded once.
        let mut expanded = BTreeSet::new();
        loop {
            let included: Vec<(&str, &str)> = body
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter_map(|word| macros.get_key_value(word).map(|(n, d)| (*n, *d)))
                .filter(|(name, _)| !expanded.contains(name))
                .collect();
            if included.is_empty() {
                break;
            }
            for (name, definition) in included {
                if expanded.insert(name) {
                    body.push_str(",\n");
                    body.push_str(definition);
                }
            }
        }
        if let Some(bits) = body.split(".dma_mask_size = ").nth(1) {
            let bits = bits.split(',').next().unwrap_or("").trim();
            let platform = body
                .split("PLATFORM(")
                .nth(1)
                .and_then(|rest| rest.split(')').next())
                .map(str::trim)
                .filter(|platform| !platform.is_empty())
                .ok_or_else(|| format!("{name} names no platform"))?;
            masks.insert(
                name.trim().to_owned(),
                (
                    bits.parse::<u8>()
                        .map_err(|_| format!("{name} has mask {bits:?}"))?,
                    body.contains(".require_force_probe = true"),
                    platform.to_owned(),
                    body.contains(".has_display = true"),
                    [".is_dgfx = 1", ".is_dgfx = true"]
                        .iter()
                        .any(|mark| body.contains(mark)),
                    body.contains(".pre_gmdid_graphics_ip = "),
                ),
            );
        }
    }
    let entries = table
        .split("pciidlist[] = {")
        .nth(1)
        .and_then(|rest| rest.split("{ }").next())
        .ok_or("no pciidlist")?;
    let mut devices = BTreeMap::new();
    for entry in entries
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (list, descriptor) = entry
            .split_once('(')
            .and_then(|(list, rest)| Some((list, rest.split_once('&')?.1)))
            .ok_or_else(|| format!("entry {entry:?}"))?;
        let descriptor = descriptor.trim_end_matches("),");
        let described = masks
            .get(descriptor)
            .ok_or_else(|| format!("{descriptor} has no dma_mask_size"))?;
        let mut pending = vec![list.to_owned()];
        while let Some(list) = pending.pop() {
            let (own, included) = lists.get(&list).ok_or_else(|| format!("no list {list}"))?;
            pending.extend(included.iter().cloned());
            for id in own {
                if devices
                    .insert(*id, described.clone())
                    .is_some_and(|other| other != *described)
                {
                    return Err(format!("{id:#06x} has two descriptors"));
                }
            }
        }
    }
    if devices.is_empty() {
        return Err("pciidlist is empty".into());
    }
    Ok(devices)
}

/// The file name of each firmware entry form of `xe_uc_fw.c`, which builds
/// `MAKE_FW_PATH(dir, uc, shortname, version)` as
/// `dir "/" shortname "_" uc version ".bin"`. A table written with another
/// path form or entry form stops the build, so a new form is read here first.
const MAKE_FW_PATH: &str = "__stringify(dir__) \"/\" __stringify(shortname__) \"_\" \
                            __stringify(uc__) version__ \".bin\"";

/// Each platform's GuC, HuC and GSC file in xe's own firmware table: the
/// `XE_*_FIRMWARE_DEFS` lists expanded with the file name forms their
/// `MODULE_FIRMWARE` declarations pass.
pub fn uc_firmware(source: &str) -> Result<BTreeMap<String, [Option<String>; 3]>, String> {
    let joined = source.replace("\\\n", " ");
    let path = joined
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("#define MAKE_FW_PATH(dir__, uc__, shortname__, version__)")
        })
        .ok_or("no MAKE_FW_PATH")?;
    if path.split_whitespace().collect::<Vec<_>>()
        != MAKE_FW_PATH.split_whitespace().collect::<Vec<_>>()
    {
        return Err(format!("MAKE_FW_PATH is {path:?}"));
    }
    // Each file name form: the version suffix it writes from (a, b, c).
    let mut forms = BTreeMap::new();
    for line in joined.lines() {
        let Some(rest) = line.trim().strip_prefix("#define fw_filename_") else {
            continue;
        };
        let (name, body) = rest
            .split_once('(')
            .ok_or_else(|| format!("form {rest:?}"))?;
        let version = body
            .rsplit_once("MAKE_FW_PATH(")
            .ok_or_else(|| format!("form {name}"))?
            .1;
        let version = version
            .splitn(4, ',')
            .nth(3)
            .ok_or_else(|| format!("form {name}"))?;
        let version = version.trim();
        let version = version.strip_suffix(')').unwrap_or(version).trim();
        let suffix = match version {
            "\"\"" => Suffix::None,
            "\"_\" __stringify(a ## . ## b ## . ## c)" => Suffix::Release,
            "\"_\" __stringify(a)" => Suffix::Major,
            "\"_\" __stringify(b)" => Suffix::Minor,
            other => return Err(format!("fw_filename_{name} writes {other:?}")),
        };
        forms.insert(format!("fw_filename_{name}"), suffix);
    }
    let mut files: BTreeMap<String, [Option<String>; 3]> = BTreeMap::new();
    for (column, kind) in ["GUC", "HUC", "GSC"].into_iter().enumerate() {
        let list = format!("XE_{kind}_FIRMWARE_DEFS");
        // The list's parameters, bound to the forms its declarations pass.
        let header = joined
            .lines()
            .find_map(|line| line.trim().strip_prefix(&format!("#define {list}(")))
            .ok_or_else(|| format!("no {list}"))?;
        let (parameters, body) = header
            .split_once(')')
            .ok_or_else(|| format!("{list} header"))?;
        let parameters: Vec<&str> = parameters.split(',').map(str::trim).collect();
        let declared = joined
            .split(&format!("\n{list}(XE_UC_MODULE_FIRMWARE,"))
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .ok_or_else(|| format!("{list} is not declared with MODULE_FIRMWARE"))?;
        let arguments: Vec<&str> = core::iter::once("XE_UC_MODULE_FIRMWARE")
            .chain(declared.split(',').map(str::trim))
            .collect();
        if arguments.len() != parameters.len() {
            return Err(format!("{list} takes {parameters:?}, given {arguments:?}"));
        }
        let binding: BTreeMap<&str, &str> = parameters.iter().copied().zip(arguments).collect();
        for entry in body.split("fw_def(").skip(1) {
            let mut fields = entry.splitn(3, ',');
            let platform = fields.next().unwrap_or("").trim();
            let _gt_type = fields.next();
            let call = fields
                .next()
                .ok_or_else(|| format!("{list} entry {entry:?}"))?;
            let (parameter, arguments) = call
                .trim()
                .split_once('(')
                .ok_or_else(|| format!("{list} entry {entry:?}"))?;
            let arguments: Vec<&str> = arguments
                .split(')')
                .next()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .collect();
            let form = binding
                .get(parameter.trim())
                .and_then(|form| forms.get(*form))
                .ok_or_else(|| format!("{list} entry of {platform} uses {parameter}"))?;
            let [dir, uc, shortname, version @ ..] = arguments.as_slice() else {
                return Err(format!("{list} entry of {platform}: {arguments:?}"));
            };
            let suffix = match (form, version) {
                (Suffix::None, _) => String::new(),
                (Suffix::Release, [a, b, c]) => format!("_{a}.{b}.{c}"),
                (Suffix::Major, [a, ..]) => format!("_{a}"),
                (Suffix::Minor, [_, b, ..]) => format!("_{b}"),
                _ => return Err(format!("{list} entry of {platform}: {arguments:?}")),
            };
            let name = format!("{dir}/{shortname}_{uc}{suffix}.bin");
            let slot = &mut files.entry(platform.to_owned()).or_default()[column];
            // xe takes a platform's first entry of any GT type.
            slot.get_or_insert(name);
        }
    }
    if files.is_empty() {
        return Err("no firmware entries".into());
    }
    Ok(files)
}

/// What a firmware file name form appends to the name for a version.
#[derive(Clone, Copy)]
enum Suffix {
    None,
    Release,
    Major,
    Minor,
}

/// The display microcontroller programs `intel_dmc.c` declares with
/// `MODULE_FIRMWARE`, less those its selection gives display version 11 and
/// older, which only i915 drives.
pub fn display_firmware(source: &str) -> Result<BTreeSet<String>, String> {
    // Each path macro's file name.
    let mut paths = BTreeMap::new();
    for line in source.lines() {
        let Some(rest) = line.trim().strip_prefix("#define ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(|c: char| c.is_whitespace()) else {
            continue;
        };
        let value = value.trim();
        let argument = |prefix: &str| {
            value
                .strip_prefix(prefix)?
                .strip_suffix(')')
                .map(str::to_owned)
        };
        if let Some(platform) = argument("DMC_PATH(") {
            paths.insert(name.to_owned(), format!("i915/{}_dmc.bin", platform.trim()));
        } else if let Some(arguments) = argument("DMC_LEGACY_PATH(") {
            let parts: Vec<&str> = arguments.split(',').map(str::trim).collect();
            let [platform, major, minor] = parts.as_slice() else {
                return Err(format!("{name} is {value:?}"));
            };
            paths.insert(
                name.to_owned(),
                format!("i915/{platform}_dmc_ver{major}_{minor}.bin"),
            );
        }
    }
    let mut declared = BTreeSet::new();
    for line in source.lines() {
        if let Some(name) = line.trim().strip_prefix("MODULE_FIRMWARE(") {
            let name = name.trim_end_matches(';').trim_end_matches(')');
            declared.insert(
                paths
                    .get(name)
                    .ok_or_else(|| format!("MODULE_FIRMWARE({name})"))?
                    .clone(),
            );
        }
    }
    // The selection's branches in order; the first for display version 11
    // starts i915's own platforms.
    let selection = source
        .split("static const char *dmc_firmware_default(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .ok_or("no dmc_firmware_default")?;
    let older = selection
        .split_once("DISPLAY_VER(display) == 11")
        .ok_or("no display version 11 branch")?
        .1;
    for assignment in older.split("fw_path = ").skip(1) {
        let name = assignment.split(';').next().unwrap_or("").trim();
        let file = paths.get(name).ok_or_else(|| format!("fw_path = {name}"))?;
        declared.remove(file);
    }
    if declared.is_empty() {
        return Err("no display firmware".into());
    }
    Ok(declared)
}
