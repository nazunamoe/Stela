//! The Metal shader corpus: AIR modules from Apple's `metal -c`, swept by
//! `stela_metal_corpus` through the Metal compiler with NAK's NIR options
//! and with BRW's for each Intel compiler generation from Gen9 (ELK compiles
//! Gen7 and Gen8), each kernel then compiled by that vendor's compiler as a
//! census. Offline compilation, never GPU execution. The census never fails
//! the lane: refusals are its findings, and the receipt counts them.
use super::{meson::Build, tools};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
    process::Stdio,
};

/// Each sweep: its evidence stage and the tool's arguments.
const SWEEPS: [(&str, &[&str]); 2] = [
    ("metal-corpus-nak", &["--nak", "1"]),
    ("metal-corpus-brw", &["--intel", "9"]),
];

/// The outcomes of one platform's sweep.
#[derive(Default, Debug, PartialEq, Eq, serde::Serialize)]
struct Outcomes {
    translated: usize,
    refused: usize,
    compiled: usize,
    compiler_refused: usize,
    aborted: usize,
    skipped: usize,
    held: Option<String>,
}

pub(super) fn verify(stela: &Build, evidence: &Path, modules: &[PathBuf]) -> Result<Option<Value>> {
    if modules.is_empty() {
        return Ok(None);
    }
    let modules = modules
        .iter()
        .map(fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()?;
    let bindings = modules
        .iter()
        .map(|path| tools::binding(path))
        .collect::<Result<Vec<_>>>()?;
    let mut sweeps = Vec::new();
    for (stage, args) in SWEEPS {
        let log = evidence.join(format!("{stage}.log"));
        let stderr = evidence.join(format!("{stage}.stderr.log"));
        println!("GPU compilers {stage}: {}", log.display());
        let status = stela
            .command("stela_metal_corpus")?
            .args(args)
            .args(&modules)
            .stdout(Stdio::from(File::create_new(&log)?))
            .stderr(Stdio::from(File::create_new(&stderr)?))
            .status()
            .context("cannot run stela_metal_corpus")?;
        // 1 says an entry point was refused, which the census counts.
        ensure!(
            matches!(status.code(), Some(0 | 1)),
            "{stage} failed ({status}); see {} and {}",
            log.display(),
            stderr.display()
        );
        let platforms = outcomes(&fs::read_to_string(&log)?);
        for (platform, counts) in &platforms {
            match &counts.held {
                Some(hold) => println!("  {stage} {platform}: held ({hold})"),
                None => println!(
                    "  {stage} {platform}: {} translated, {} refused; kernels {} compiled, \
                     {} refused, {} aborted, {} not compiled",
                    counts.translated,
                    counts.refused,
                    counts.compiled,
                    counts.compiler_refused,
                    counts.aborted,
                    counts.skipped
                ),
            }
        }
        sweeps.push(json!({
            "stage": stage, "args": args, "output": tools::binding(&log)?,
            "platforms": platforms, "hardware_executed": false,
        }));
    }
    Ok(Some(json!({"modules": bindings, "sweeps": sweeps})))
}

/// The outcomes in a sweep's output by platform: the tag that starts an
/// Intel sweep's lines, or `nak`.
fn outcomes(text: &str) -> BTreeMap<String, Outcomes> {
    let mut platforms = BTreeMap::<String, Outcomes>::new();
    for line in text.lines() {
        // Module paths are absolute; a platform's tag has no slash. Totals
        // ("2 refused") are recounted here.
        let (platform, rest) = match line.split_once(' ') {
            _ if line.starts_with('/') => ("nak", line),
            Some((tag, rest)) if rest.starts_with('/') || rest.starts_with("held: ") => (tag, rest),
            _ => continue,
        };
        let counts = platforms.entry(platform.to_owned()).or_default();
        if let Some(hold) = rest.strip_prefix("held: ") {
            counts.held = Some(hold.to_owned());
            continue;
        }
        // "PATH ENTRY: OUTCOME"; a census line ("brw refused: why") has a
        // colon of its own after the first.
        let Some((_, outcome)) = rest.split_once(": ") else {
            continue;
        };
        if outcome == "ok" || outcome == "ok (tile)" {
            counts.translated += 1;
        } else if outcome.starts_with("refused (") || outcome.starts_with("reflection refused (") {
            counts.refused += 1;
        } else if let Some(result) = outcome
            .strip_prefix("brw ")
            .or_else(|| outcome.strip_prefix("nak "))
        {
            match result.split([':', ' ']).next().unwrap_or(result) {
                "skipped" => counts.skipped += 1,
                "refused" => counts.compiler_refused += 1,
                "aborted" | "not" => counts.aborted += 1,
                _ => counts.compiled += 1,
            }
        }
    }
    platforms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_are_counted_by_platform() {
        let text = "\
/c/a.air k: ok
/c/a.air k: nak instrs=3 static_cycles=1
/c/a.air f: ok
/c/a.air f: nak skipped
/c/a.air t: reflection refused (3): air.kernel metadata budget exceeded
/c/a.air u: nak refused
1 refused
tgl /c/a.air k: ok
tgl /c/a.air k: brw simd=32 instrs=9
tgl /c/a.air v: refused (2): visible function linked_shade is not linked
tgl /c/a.air w: ok (tile)
tgl /c/a.air x: brw refused: no code
tgl /c/a.air y: brw aborted (signal 6)
tgl 1 refused
nvl held: intel-gen35-dispatch-qualification
2 refused
";
        let platforms = outcomes(text);
        assert_eq!(
            platforms["nak"],
            Outcomes {
                translated: 2,
                refused: 1,
                compiled: 1,
                compiler_refused: 1,
                skipped: 1,
                ..Outcomes::default()
            }
        );
        assert_eq!(
            platforms["tgl"],
            Outcomes {
                translated: 2,
                refused: 1,
                compiled: 1,
                compiler_refused: 1,
                aborted: 1,
                ..Outcomes::default()
            }
        );
        assert_eq!(
            platforms["nvl"].held.as_deref(),
            Some("intel-gen35-dispatch-qualification")
        );
    }
}
