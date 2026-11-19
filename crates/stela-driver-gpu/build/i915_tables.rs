//! Readers of i915's own tables, shared by its build (`build/i915.rs`) and by
//! `cargo xtask gpu anv-test`, which runs ANV on the devices they describe:
//! the PCI ID table (`i915_pci.c` with Linux's `pciids.h`) and the x86 early
//! quirks' stolen-memory methods (`early-quirks.c`). Each refuses a form it
//! does not know, so a changed table is read here first.
use std::collections::BTreeMap;

/// What i915's table says of each device: the DMA mask bits, whether it
/// binds only by force probe, its graphics IP version, its platform and
/// whether it is a discrete GPU.
#[derive(Clone, Debug, PartialEq)]
pub struct Described {
    pub bits: u8,
    pub forced: bool,
    pub version: u8,
    pub platform: String,
    pub discrete: bool,
}

/// Each device ID of `intel_early_ids` in `quirks` with its method's name,
/// the ID lists expanded from `ids`.
pub fn stolen_methods(quirks: &str, ids: &str) -> Result<BTreeMap<u16, String>, String> {
    let lists = lists(ids)?;
    let entries = quirks
        .split("intel_early_ids[] __initconst = {")
        .nth(1)
        .and_then(|rest| rest.split("};").next())
        .ok_or("no intel_early_ids")?;
    let mut methods = BTreeMap::new();
    for entry in entries.split("),").map(str::trim).filter(|e| !e.is_empty()) {
        let (list, ops) = entry
            .split_once('(')
            .and_then(|(list, rest)| Some((list.trim(), rest.split_once('&')?.1.trim())))
            .ok_or_else(|| format!("entry {entry:?}"))?;
        let method = ops
            .strip_suffix("_early_ops")
            .ok_or_else(|| format!("{ops} is no early ops"))?;
        for id in expand_list(&lists, list)? {
            if methods
                .insert(id, method.to_owned())
                .is_some_and(|other| other != method)
            {
                return Err(format!("{id:#06x} has two methods"));
            }
        }
    }
    if methods.is_empty() {
        return Err("intel_early_ids is empty".into());
    }
    Ok(methods)
}

/// A macro of the table's source: its parameters (none for an object-like
/// one) and its body.
struct Macro {
    parameters: Option<Vec<String>>,
    body: String,
}

/// The table's macros, by name.
fn macros(source: &str) -> BTreeMap<String, Macro> {
    let mut macros = BTreeMap::new();
    for line in source.lines() {
        let Some(rest) = line.trim_start().strip_prefix("#define ") else {
            continue;
        };
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let (name, rest) = rest.split_at(end);
        let (parameters, body) = match rest.strip_prefix('(') {
            Some(rest) => {
                let Some((list, body)) = rest.split_once(')') else {
                    continue;
                };
                (
                    Some(list.split(',').map(|p| p.trim().to_owned()).collect()),
                    body,
                )
            }
            None => (None, rest),
        };
        macros.insert(
            name.to_owned(),
            Macro {
                parameters,
                body: body.trim().to_owned(),
            },
        );
    }
    macros
}

/// The arguments of a call whose text starts after its opening parenthesis,
/// and the length up to and with its closing one.
fn arguments(text: &str) -> Option<(Vec<String>, usize)> {
    let (mut depth, mut start, mut arguments) = (0usize, 0, Vec::new());
    for (index, c) in text.char_indices() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' if depth == 0 => {
                arguments.push(text[start..index].trim().to_owned());
                return Some((arguments, index + 1));
            }
            ')' | '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                arguments.push(text[start..index].trim().to_owned());
                start = index + 1;
            }
            _ => {}
        }
    }
    None
}

/// `text` with every macro of `macros` expanded, as far as `budget` allows.
fn expand(
    text: &str,
    macros: &BTreeMap<String, Macro>,
    budget: &mut usize,
) -> Result<String, String> {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(|c: char| c.is_ascii_alphabetic() || c == '_') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let (word, after) = rest.split_at(end);
        // A field name (`.gt`) or a part of a number is no macro.
        let field = output.ends_with('.')
            || output
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        match macros.get(word).filter(|_| !field) {
            Some(definition) => {
                *budget = budget.checked_sub(1).ok_or("macros nest too deep")?;
                match &definition.parameters {
                    None => {
                        output.push_str(&expand(&definition.body, macros, budget)?);
                        rest = after;
                    }
                    Some(parameters) => {
                        let (values, length) = after
                            .trim_start()
                            .strip_prefix('(')
                            .and_then(arguments)
                            .ok_or_else(|| format!("{word} is called without arguments"))?;
                        let skipped = after.len() - after.trim_start().len() + 1 + length;
                        let mut body = definition.body.clone();
                        for (parameter, value) in parameters.iter().zip(&values) {
                            body = replace_word(&body, parameter, value);
                        }
                        output.push_str(&expand(&body, macros, budget)?);
                        rest = &after[skipped..];
                    }
                }
            }
            None => {
                output.push_str(word);
                rest = after;
            }
        }
    }
    output.push_str(rest);
    Ok(output)
}

/// `text` with each whole word `word` replaced by `value`.
fn replace_word(text: &str, word: &str, value: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(index) = rest.find(word) {
        let before = rest[..index].chars().last();
        let after = rest[index + word.len()..].chars().next();
        let part = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        output.push_str(&rest[..index]);
        output.push_str(if part(before) || part(after) {
            word
        } else {
            value
        });
        rest = &rest[index + word.len()..];
    }
    output.push_str(rest);
    output
}

/// The designated fields of an initializer's body, the last of each name
/// winning as in C.
fn fields(body: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let (mut depth, mut start) = (0usize, 0);
    let mut push = |item: &str| {
        if let Some((name, value)) = item
            .trim()
            .strip_prefix('.')
            .and_then(|f| f.split_once('='))
        {
            fields.insert(name.trim().to_owned(), value.trim().to_owned());
        }
    };
    for (index, c) in body.char_indices() {
        match c {
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    push(&body[start..]);
    fields
}

/// An integer field's value, without the parentheses a macro gave it.
fn number(value: &str) -> Option<u8> {
    value
        .trim_matches(|c: char| c == '(' || c == ')' || c.is_whitespace())
        .parse()
        .ok()
}

/// Each device ID of `pciidlist` in `table` with what its descriptor says,
/// the ID lists expanded from `ids`.
pub fn devices(table: &str, ids: &str) -> Result<BTreeMap<u16, Described>, String> {
    let lists = lists(ids)?;
    let joined = strip_comments(&table.replace("\\\n", " "));
    let macros = macros(&joined);
    let mut descriptors = BTreeMap::new();
    for descriptor in joined
        .split("static const struct intel_device_info ")
        .skip(1)
    {
        let Some((name, body)) = descriptor.split_once(" = {") else {
            continue;
        };
        let name = name.trim();
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let body = body.split("\n};").next().unwrap_or("");
        let mut budget = 4096;
        let fields = fields(&expand(body, &macros, &mut budget)?);
        let get = |field: &str| fields.get(field).map(String::as_str);
        let flag = |field: &str| get(field).is_some_and(|value| value != "0" && value != "false");
        descriptors.insert(
            name.to_owned(),
            Described {
                bits: get("dma_mask_size")
                    .and_then(number)
                    .ok_or_else(|| format!("{name} has no dma_mask_size"))?,
                forced: flag("require_force_probe"),
                version: get("__runtime.graphics.ip.ver")
                    .and_then(number)
                    .ok_or_else(|| format!("{name} has no graphics version"))?,
                platform: get("platform")
                    .map(|value| value.trim_matches(|c| c == '(' || c == ')'))
                    .and_then(|value| value.strip_prefix("INTEL_"))
                    .ok_or_else(|| format!("{name} names no platform"))?
                    .to_owned(),
                discrete: flag("is_dgfx"),
            },
        );
    }
    let entries = joined
        .split("pciidlist[] = {")
        .nth(1)
        .and_then(|rest| rest.split("{}").next())
        .ok_or("no pciidlist")?;
    let mut devices = BTreeMap::new();
    for entry in entries.split("),").map(str::trim).filter(|e| !e.is_empty()) {
        let (list, descriptor) = entry
            .split_once('(')
            .and_then(|(list, rest)| Some((list.trim(), rest.split_once('&')?.1.trim())))
            .ok_or_else(|| format!("entry {entry:?}"))?;
        let described = descriptors
            .get(descriptor)
            .ok_or_else(|| format!("{descriptor} is no descriptor"))?;
        for id in expand_list(&lists, list)? {
            // The first match wins, as Linux matches the table in order (the
            // subsystem-specific Quanta entry adds no device ID of its own).
            devices.entry(id).or_insert_with(|| described.clone());
        }
    }
    if devices.is_empty() {
        return Err("pciidlist is empty".into());
    }
    Ok(devices)
}

/// `text` without its C comments.
fn strip_comments(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    loop {
        let block = rest.find("/*");
        let line = rest.find("//");
        match (block, line) {
            (Some(b), l) if l.is_none_or(|l| b < l) => {
                output.push_str(&rest[..b]);
                rest = rest[b..].split_once("*/").map_or("", |(_, after)| after);
            }
            (_, Some(l)) => {
                output.push_str(&rest[..l]);
                rest = rest[l..].split_once('\n').map_or("", |(_, after)| after);
                output.push('\n');
            }
            _ => break,
        }
    }
    output.push_str(rest);
    output
}

/// Each list macro of Linux's Intel ID header: its own IDs and the lists it
/// includes.
type Lists = BTreeMap<String, (Vec<u16>, Vec<String>)>;
fn lists(ids: &str) -> Result<Lists, String> {
    let mut lists = BTreeMap::new();
    for definition in ids.replace("\\\n", " ").lines() {
        let Some(rest) = definition.strip_prefix("#define ") else {
            continue;
        };
        let Some((name, body)) = rest.split_once("(MACRO__, ...)") else {
            continue;
        };
        let (mut own, mut included) = (Vec::new(), Vec::new());
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
    Ok(lists)
}

/// The device IDs of list `name` and the lists it includes.
fn expand_list(lists: &Lists, name: &str) -> Result<Vec<u16>, String> {
    let mut pending = vec![name.to_owned()];
    let mut ids = Vec::new();
    while let Some(list) = pending.pop() {
        let (own, included) = lists.get(&list).ok_or_else(|| format!("no list {list}"))?;
        pending.extend(included.iter().cloned());
        ids.extend(own);
    }
    Ok(ids)
}
