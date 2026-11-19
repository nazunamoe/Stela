//! The import check of a driver compiled for Stela's Linux environment: what
//! its archive reaches when linked, directly or through the environment's
//! objects, that nothing supplies or that the environment defines only as a
//! trap.
use object::{Object, ObjectSymbol, read::archive::ArchiveFile};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
};

const KERNEL: &str = "../stela-linux/src/compat/kernel";
/// What the compiler itself calls, which every platform's runtime supplies.
const RUNTIME: [&str; 4] = ["memcpy", "memmove", "memset", "memcmp"];
/// The prefix of what Stela's Rust side exports to the environment.
const PLATFORM: &str = "stela_linux_";
/// The prefix of the hosted test harness (`stela/test.h`). Only a hosted
/// environment built with its source fixtures reaches it, from fixture code
/// sharing archive members with the services a driver imports.
const HARNESS: &str = "stela_test_";

/// The services `port`'s members reach that the `environment` archives lack
/// or define only as a trap.
pub fn open_all(ports: &[&Path], environment: &[&Path]) -> BTreeSet<String> {
    let port = ports
        .iter()
        .flat_map(|archive| members(archive))
        .collect::<Vec<_>>();
    let environment = environment
        .iter()
        .flat_map(|archive| members(archive))
        .collect::<Vec<_>>();
    let definers = environment
        .iter()
        .flat_map(|member| member.defined.iter().map(move |symbol| (symbol, member)))
        .collect::<BTreeMap<_, _>>();
    let defined = port
        .iter()
        .flat_map(|member| &member.defined)
        .collect::<BTreeSet<_>>();
    let traps = traps();
    let harness = env::var_os("DEP_STELA_LINUX_HOSTED_FIXTURES").is_some();
    let mut reached = port
        .iter()
        .flat_map(|member| &member.imported)
        .filter(|symbol| !defined.contains(symbol))
        .collect::<Vec<_>>();
    let mut seen = reached.iter().copied().collect::<BTreeSet<_>>();
    let mut open = BTreeSet::new();
    while let Some(symbol) = reached.pop() {
        if traps.contains(symbol) {
            open.insert(symbol.clone());
        }
        match definers.get(symbol) {
            Some(member) => reached.extend(
                member
                    .imported
                    .iter()
                    .filter(|&import| !defined.contains(import) && seen.insert(import)),
            ),
            None if RUNTIME.contains(&symbol.as_str())
                || symbol.starts_with(PLATFORM)
                || (harness && symbol.starts_with(HARNESS)) => {}
            None => {
                open.insert(symbol.clone());
            }
        }
    }
    open
}

/// What one archive member defines and imports.
struct Member {
    defined: BTreeSet<String>,
    imported: BTreeSet<String>,
}

fn members(archive: &Path) -> Vec<Member> {
    let data = fs::read(archive).unwrap_or_else(|error| panic!("{}: {error}", archive.display()));
    let archive = ArchiveFile::parse(&*data).expect("an archive");
    archive
        .members()
        .map(|member| {
            let member = member.expect("an archive member");
            let object = object::File::parse(member.data(&*data).expect("member data"))
                .expect("an object file");
            let (mut defined, mut imported) = (BTreeSet::new(), BTreeSet::new());
            for symbol in object.symbols() {
                let Ok(name) = symbol.name() else {
                    continue;
                };
                // A weak reference left undefined resolves to null, as Linux's
                // symbol_get of a module that is not loaded (i915's link to
                // intel_ips), so it reaches no service.
                if symbol.is_undefined() && symbol.is_weak() {
                    continue;
                }
                if symbol.is_undefined() {
                    imported.insert(name.to_owned());
                } else if symbol.is_global() {
                    defined.insert(name.to_owned());
                }
            }
            Member { defined, imported }
        })
        .collect()
}

/// What the environment defines without serving: functions that trap
/// (`STELA_UNSUPPORTED`), placeholder objects (`STELA_UNSUPPORTED_OBJECT`)
/// and functions that report themselves unsupported and fail.
fn traps() -> BTreeSet<String> {
    let mut traps = BTreeSet::new();
    for source in stela_c_build::linux_build::sources(KERNEL)
        .into_iter()
        .chain(stela_c_build::linux_build::sources(
            "src/compat/linux/kernel",
        ))
    {
        let text = fs::read_to_string(&source).expect("read a kernel-API source");
        for (marker, field) in [
            ("STELA_UNSUPPORTED(", 1),
            ("STELA_UNSUPPORTED_OBJECT(", 1),
            ("stela_linux_unsupported(\"", 0),
        ] {
            for definition in text.split(marker).skip(1) {
                let end = definition.find([')', '"']).unwrap_or(definition.len());
                if let Some(name) = definition[..end].split(',').nth(field) {
                    traps.insert(name.trim().to_owned());
                }
            }
        }
    }
    traps
}
