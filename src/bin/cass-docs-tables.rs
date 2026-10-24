//! Regenerate the registry-backed tables embedded in README.md and AGENTS.md.
//!
//! ```text
//! cargo run --bin cass-docs-tables
//! cargo run --bin cass-docs-tables -- --check
//! ```
//!
//! `--check` writes nothing and exits 1 when any embedded block differs from
//! its registry rendering, so CI can run it without dirtying the tree.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use coding_agent_search::docs_tables::{DOCUMENTS, apply_blocks, drifted_markers, missing_markers};

fn main() -> ExitCode {
    let mut check = false;
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--root" => match args.next() {
                Some(path) => root = PathBuf::from(path),
                None => {
                    eprintln!("--root needs a directory");
                    return ExitCode::FAILURE;
                }
            },
            "-h" | "--help" => {
                println!("usage: cass-docs-tables [--check] [--root DIR]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut failures = 0_usize;
    for file in DOCUMENTS {
        if let Err(error) = regenerate(&root.join(file), file, check) {
            eprintln!("{file}: {error}");
            failures += 1;
        }
    }
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn regenerate(path: &Path, name: &str, check: bool) -> Result<(), String> {
    let before = std::fs::read_to_string(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let missing = missing_markers(name, &before);
    if !missing.is_empty() {
        return Err(format!("missing generated blocks: {}", missing.join(", ")));
    }
    let drifted = drifted_markers(&before);
    if check {
        if drifted.is_empty() {
            println!("{name}: up to date");
            return Ok(());
        }
        return Err(format!("drifted blocks: {}", drifted.join(", ")));
    }
    let (after, replaced) =
        apply_blocks(&before).map_err(|error| format!("rewriting blocks: {error}"))?;
    if replaced == 0 {
        return Err("no generated blocks replaced".to_string());
    }
    if after != before {
        std::fs::write(path, after)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
        println!("{name}: rewrote {replaced} blocks");
    } else {
        println!("{name}: unchanged ({replaced} blocks)");
    }
    Ok(())
}
