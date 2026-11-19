//! The object lists a driver's Kbuild makefiles compose, read with the subset
//! of GNU Make they use. Anything outside that subset is refused, even in an
//! inactive conditional, rather than guessed at.
//!
//! A line ending in a backslash continues on the next. Accepted lines:
//! comments; `include $(src)/<path>`; `ifdef CONFIG_<X>`,
//! `ifeq ($(CONFIG_<X>),y)`, `ifneq ($(CONFIG_<X>),y)` and `endif`;
//! `ccflags-y` and `subdir-ccflags-y` (or `-$(CONFIG_<X>)`) appended with
//! include directories below `$(src)`, `$(srctree)` or at `$(obj)`, with
//! `-Werror`, with `-D<NAME>` or with one `$(call cc-option, -W<name>)`; and
//! `<name>-y` or `<name>-$(CONFIG_<X>)` assigned with `:=` or `+=` from object
//! paths and `$(<name>-y)` references. A `:=` variable expands its references
//! when assigned, as Make's simply expanded variables do. A recursively
//! expanded variable expands them when it is read, so its reference is
//! expanded at the assignment only on the condition that what it names does
//! not change afterwards; a later change of it is refused.
//!
//! A variable without a selector (or with a suffix that is no selector, as
//! `no-header-test`) and a rule with its recipe are not evaluated: they are
//! kept as written in [`Kbuild::definitions`] and [`Kbuild::rules`], and the
//! driver's build refuses any it does not carry out.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Bounds on what one driver's makefiles may hold.
const MAX_FILES: usize = 128;
const MAX_DEPTH: usize = 8;
const MAX_BYTES: usize = 1 << 20;

/// The `CONFIG_` symbols of a Kconfig header: `#define CONFIG_<X> <value>`.
pub struct Config(BTreeMap<String, String>);

impl Config {
    pub fn parse(header: &str) -> Result<Self, String> {
        let mut symbols = BTreeMap::new();
        for line in header.lines() {
            let Some(definition) = line.strip_prefix("#define ") else {
                continue;
            };
            let (name, value) = definition.split_once(' ').unwrap_or((definition, ""));
            if name.starts_with("CONFIG_")
                && symbols
                    .insert(name.to_owned(), value.trim().to_owned())
                    .is_some()
            {
                return Err(format!("{name} is defined twice"));
            }
        }
        Ok(Self(symbols))
    }

    /// Whether a `bool` or `tristate` symbol is `y`. A module (`m`) selection
    /// and a non-boolean value are refused: Stela builds no modules.
    fn enabled(&self, name: &str) -> Result<bool, String> {
        if self.0.contains_key(&format!("{name}_MODULE")) {
            return Err(format!("{name} selects a module"));
        }
        match self.0.get(name).map(String::as_str) {
            None => Ok(false),
            Some("1") => Ok(true),
            Some(value) => Err(format!("{name} is {value}, not a boolean")),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    Simple,
    Recursive,
}

/// A rule as its makefile writes it. Nothing here runs a recipe.
#[derive(Debug, PartialEq)]
pub struct Rule {
    pub targets: String,
    pub prerequisites: String,
    pub recipe: Vec<String>,
}

/// What a driver's makefiles define: its object lists and include paths.
#[derive(Default)]
pub struct Kbuild {
    variables: BTreeMap<String, (Flavor, Vec<String>)>,
    /// `ccflags-y` include directories in the driver's tree, in order.
    pub includes: Vec<PathBuf>,
    /// Include directories below the kernel's source tree (`$(srctree)`).
    pub tree_includes: Vec<PathBuf>,
    /// Whether the driver's output directory (`$(obj)`) is searched.
    pub generated_include: bool,
    /// Warning flags the compiler takes when it knows them (`cc-option`).
    pub optional_flags: Vec<String>,
    /// Macros defined for every object (`-D<NAME>`), in order.
    pub defines: Vec<String>,
    /// Variables a recursively expanded variable refers to, which may then
    /// not change.
    read_late: std::collections::BTreeSet<String>,
    /// Whether an enabled option turns warnings into errors.
    pub werror: bool,
    /// Variables without a selector, as written, each `+=` joined by a space.
    pub definitions: BTreeMap<String, String>,
    /// Rules, in order.
    pub rules: Vec<Rule>,
    /// Every makefile read, in order.
    pub files: Vec<PathBuf>,
}

impl Kbuild {
    /// Reads the directory's `Kbuild`, or its `Makefile` when it has none, as
    /// Kbuild itself chooses, and the makefiles it includes, which name paths
    /// relative to `source` as `$(src)`.
    pub fn read(source: &Path, config: &Config) -> Result<Self, String> {
        let mut kbuild = Self::default();
        let file = if source.join("Kbuild").exists() {
            "Kbuild"
        } else {
            "Makefile"
        };
        kbuild.include(source, Path::new(file), config, 0)?;
        Ok(kbuild)
    }

    /// The objects of `<name>-y`, as paths below the source directory.
    pub fn objects(&self, name: &str) -> Result<&[String], String> {
        self.variables
            .get(&format!("{name}-y"))
            .map(|(_, objects)| objects.as_slice())
            .ok_or_else(|| format!("Kbuild defines no {name}-y"))
    }

    fn include(
        &mut self,
        source: &Path,
        file: &Path,
        config: &Config,
        depth: usize,
    ) -> Result<(), String> {
        if depth > MAX_DEPTH || self.files.len() == MAX_FILES {
            return Err(format!("{} nests past Kbuild's bounds", file.display()));
        }
        let path = source.join(file);
        let text =
            fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if text.len() > MAX_BYTES {
            return Err(format!("{} is too large", path.display()));
        }
        self.files.push(path.clone());
        let mut active = vec![true];
        // Whether tab-indented lines belong to the last rule as its recipe.
        let mut recipe = false;
        for (number, line) in logical_lines(&text) {
            let at = || format!("{}:{number}", path.display());
            if recipe && line.starts_with('\t') {
                if active[active.len() - 1] {
                    let rule = self.rules.last_mut().expect("a recipe follows its rule");
                    rule.recipe.push(line.trim().to_owned());
                }
                continue;
            }
            recipe = false;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(enabled) =
                condition(line, config).map_err(|error| format!("{}: {error}", at()))?
            {
                active.push(active[active.len() - 1] && enabled);
                continue;
            }
            if line == "endif" {
                if active.len() == 1 {
                    return Err(format!("{}: endif without ifdef", at()));
                }
                active.pop();
                continue;
            }
            let statement = Statement::parse(line).map_err(|error| format!("{}: {error}", at()))?;
            recipe = matches!(statement, Statement::Rule { .. });
            if !active[active.len() - 1] {
                continue;
            }
            match statement {
                Statement::Include(relative) => {
                    self.include(source, Path::new(relative), config, depth + 1)?;
                }
                Statement::Flags { condition, value } => {
                    if let Some(symbol) = condition
                        && !config
                            .enabled(symbol)
                            .map_err(|error| format!("{}: {error}", at()))?
                    {
                        continue;
                    }
                    self.flags(source, value)
                        .map_err(|error| format!("{}: {error}", at()))?;
                }
                Statement::Assign {
                    name,
                    condition,
                    simple,
                    words,
                } => {
                    if let Some(symbol) = condition
                        && !config
                            .enabled(symbol)
                            .map_err(|error| format!("{}: {error}", at()))?
                    {
                        continue;
                    }
                    self.assign(name, simple, &words)
                        .map_err(|error| format!("{}: {error}", at()))?;
                }
                Statement::Define {
                    name,
                    append,
                    value,
                } => {
                    let definition = self.definitions.entry(name.to_owned()).or_default();
                    if append && !definition.is_empty() {
                        definition.push(' ');
                    } else if !append {
                        definition.clear();
                    }
                    definition.push_str(value);
                }
                Statement::Rule {
                    targets,
                    prerequisites,
                } => self.rules.push(Rule {
                    targets: targets.to_owned(),
                    prerequisites: prerequisites.to_owned(),
                    recipe: Vec::new(),
                }),
            }
        }
        if active.len() != 1 {
            return Err(format!("{}: ifdef without endif", path.display()));
        }
        Ok(())
    }

    /// The compiler flags of one `ccflags` line.
    fn flags(&mut self, source: &Path, value: &str) -> Result<(), String> {
        if let Some(option) = value
            .strip_prefix("$(call cc-option,")
            .and_then(|option| option.strip_suffix(')'))
        {
            let option = option.trim();
            return if option.starts_with("-W")
                && option[2..]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                self.optional_flags.push(option.to_owned());
                Ok(())
            } else {
                Err(format!("unsupported flag {value}"))
            };
        }
        let mut words = value.split_whitespace();
        while let Some(word) = words.next() {
            if word == "-Werror" {
                self.werror = true;
                continue;
            }
            if let Some(name) = word.strip_prefix("-D") {
                if name.is_empty()
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return Err(format!("unsupported flag {word}"));
                }
                self.defines.push(name.to_owned());
                continue;
            }
            let directory = match word.strip_prefix("-I") {
                Some("") => words.next().ok_or("-I names no directory")?,
                Some(directory) => directory,
                None => return Err(format!("unsupported flag {word}")),
            };
            let below = |root: &str| {
                directory.strip_prefix(root).and_then(|path| {
                    path.is_empty()
                        .then_some("")
                        .or_else(|| path.strip_prefix('/'))
                })
            };
            if directory == "$(obj)" {
                self.generated_include = true;
            } else if let Some(path) = below("$(src)") {
                self.includes.push(source.join(if path.is_empty() {
                    "."
                } else {
                    relative(path)?
                }));
            } else if let Some(path) = below("$(srctree)") {
                self.tree_includes
                    .push(relative(path.strip_suffix('/').unwrap_or(path))?.into());
            } else {
                return Err(format!("unsupported flag {word}"));
            }
        }
        Ok(())
    }

    fn assign(&mut self, name: &str, simple: bool, words: &[&str]) -> Result<(), String> {
        let key = format!("{name}-y");
        if self.read_late.contains(&key) {
            return Err(format!(
                "{key} changes after a recursively expanded variable refers to it"
            ));
        }
        let flavor = match (simple, self.variables.get(&key)) {
            (true, _) => Flavor::Simple,
            (false, Some((flavor, _))) => *flavor,
            (false, None) => Flavor::Recursive,
        };
        let mut values = Vec::new();
        for word in words {
            if let Some(reference) = word
                .strip_prefix("$(")
                .and_then(|word| word.strip_suffix(')'))
            {
                if flavor == Flavor::Recursive {
                    self.read_late.insert(reference.to_owned());
                }
                let (_, referenced) = self
                    .variables
                    .get(reference)
                    .ok_or_else(|| format!("{key} refers to undefined {reference}"))?;
                values.extend(referenced.iter().cloned());
            } else if word.ends_with(".o") && !word.contains('$') && !word.contains("..") {
                values.push((*word).to_owned());
            } else {
                return Err(format!("{key} holds {word}, which is no object"));
            }
        }
        let entry = self.variables.entry(key).or_insert((flavor, Vec::new()));
        if simple {
            entry.1.clear();
        }
        entry.0 = flavor;
        entry.1.extend(values);
        Ok(())
    }
}

/// The makefile's lines with each backslash continuation joined to its first
/// line by one space, numbered from 1 by that first line.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut continued: Option<(usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        let (number, mut joined) = match continued.take() {
            Some((number, mut joined)) => {
                joined.push(' ');
                joined.push_str(line.trim_start());
                (number, joined)
            }
            None => (index + 1, line.to_owned()),
        };
        if joined.ends_with('\\') {
            joined.pop();
            joined.truncate(joined.trim_end().len());
            continued = Some((number, joined));
        } else {
            lines.push((number, joined));
        }
    }
    lines.extend(continued);
    lines
}

/// Whether `line` opens a conditional, and whether its branch is taken.
fn condition(line: &str, config: &Config) -> Result<Option<bool>, String> {
    if let Some(symbol) = line.strip_prefix("ifdef ") {
        return config.enabled(symbol.trim()).map(Some);
    }
    for (keyword, equal) in [("ifeq ", true), ("ifneq ", false)] {
        if let Some(operands) = line.strip_prefix(keyword) {
            let symbol = operands
                .trim()
                .strip_prefix("($(")
                .and_then(|operands| operands.strip_suffix("),y)"))
                .filter(|symbol| symbol.starts_with("CONFIG_"))
                .ok_or_else(|| format!("unsupported conditional {line}"))?;
            return Ok(Some(config.enabled(symbol)? == equal));
        }
    }
    Ok(None)
}

/// One line of the accepted subset.
enum Statement<'a> {
    Include(&'a str),
    Flags {
        condition: Option<&'a str>,
        value: &'a str,
    },
    Assign {
        name: &'a str,
        condition: Option<&'a str>,
        simple: bool,
        words: Vec<&'a str>,
    },
    Define {
        name: &'a str,
        append: bool,
        value: &'a str,
    },
    Rule {
        targets: &'a str,
        prerequisites: &'a str,
    },
}

/// What separates a line's two sides.
enum Operator {
    Simple,
    Append,
    Recursive,
    Rule,
}

impl<'a> Statement<'a> {
    fn parse(line: &'a str) -> Result<Self, String> {
        if let Some(path) = line.strip_prefix("include $(src)/") {
            return relative(path).map(Statement::Include);
        }
        let (target, operator, value) =
            split(line).ok_or_else(|| format!("unsupported statement {line}"))?;
        if let Operator::Rule = operator {
            return Ok(Statement::Rule {
                targets: target,
                prerequisites: value,
            });
        }
        let identifier = |name: &str| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        };
        let Some((name, selector)) = target.rsplit_once('-') else {
            return if identifier(target) {
                Ok(Statement::Define {
                    name: target,
                    append: matches!(operator, Operator::Append),
                    value,
                })
            } else {
                Err(format!("unsupported target {target}"))
            };
        };
        let condition = match selector {
            "y" => None,
            // A name with a dash and no selector (`no-header-test`); a
            // composite's other lists (`-objs`, `-m`, `-n`) are not that.
            selector
                if !selector.starts_with("$(")
                    && !matches!(selector, "objs" | "m" | "n")
                    && target
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)) =>
            {
                return Ok(Statement::Define {
                    name: target,
                    append: matches!(operator, Operator::Append),
                    value,
                });
            }
            selector => Some(
                selector
                    .strip_prefix("$(")
                    .and_then(|selector| selector.strip_suffix(')'))
                    .filter(|symbol| symbol.starts_with("CONFIG_"))
                    .ok_or_else(|| format!("unsupported selector {selector}"))?,
            ),
        };
        if matches!(operator, Operator::Recursive) {
            return Err(format!("{target} is assigned with ="));
        }
        if name == "ccflags" || name == "subdir-ccflags" {
            return if matches!(operator, Operator::Append) {
                Ok(Statement::Flags { condition, value })
            } else {
                Err(format!("{target} is not appended to"))
            };
        }
        if !identifier(name) {
            return Err(format!("unsupported variable {name}"));
        }
        Ok(Statement::Assign {
            name,
            condition,
            simple: matches!(operator, Operator::Simple),
            words: value.split_whitespace().collect(),
        })
    }
}

/// A line's sides around its first `:=`, `+=`, `=` or rule colon outside a
/// `$(...)` reference.
fn split(line: &str) -> Option<(&str, Operator, &str)> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        let next = bytes.get(index + 1);
        let (operator, width) = match byte {
            b'(' => {
                depth += 1;
                continue;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                continue;
            }
            _ if depth != 0 => continue,
            b':' if next == Some(&b'=') => (Operator::Simple, 2),
            b'+' if next == Some(&b'=') => (Operator::Append, 2),
            b'=' => (Operator::Recursive, 1),
            b':' => (Operator::Rule, 1),
            _ => continue,
        };
        return Some((line[..index].trim(), operator, line[index + width..].trim()));
    }
    None
}

/// A path below `$(src)`.
fn relative(path: &str) -> Result<&str, String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|part| part == ".." || part.is_empty())
    {
        return Err(format!("{path} leaves the driver's tree"));
    }
    Ok(path)
}
