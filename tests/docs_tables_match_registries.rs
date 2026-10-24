//! README.md and AGENTS.md embed generated tables; this is the drift gate.
//!
//! Every block between `<!-- BEGIN GENERATED <NAME> -->` and
//! `<!-- END GENERATED <NAME> -->` must equal the registry rendering from
//! `coding_agent_search::docs_tables`. Editing a row by hand fails here and
//! `cargo run --bin cass-docs-tables` repairs it.
//!
//! A companion check proves each documented environment variable still has a
//! read site in `src/`, so the registry cannot document a variable that no
//! code path consumes.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use coding_agent_search::docs_tables::{
    DOCUMENTS, apply_blocks, drifted_markers, missing_markers, render, required_markers,
};

/// Connector roots that `franken_agent_detection` consumes directly; their
/// only in-repo mention may be the registry row itself.
const CONNECTOR_OWNED_ENV_VARS: &[&str] = &[
    "CASS_AIDER_DATA_ROOT",
    "GEMINI_HOME",
    "OPENCODE_STORAGE_ROOT",
    "CHATGPT_ENCRYPTION_KEY",
];

fn doc(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn source_files(dir: &Path, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            source_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(
                fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
            );
        }
    }
}

fn source_corpus() -> String {
    let mut files = Vec::new();
    source_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    files.join("\n")
}

#[test]
fn readme_and_agents_tables_match_the_registries() {
    for name in DOCUMENTS {
        let contents = doc(name);
        let missing = missing_markers(name, &contents);
        assert!(
            missing.is_empty(),
            "{name} is missing generated blocks: {missing:?}"
        );
        let drifted = drifted_markers(&contents);
        assert!(
            drifted.is_empty(),
            "{name} has drifted generated blocks: {drifted:?}; run `cargo run --bin cass-docs-tables`"
        );
        let (rewritten, replaced) = apply_blocks(&contents).expect("document must rewrite");
        assert_eq!(rewritten, contents, "{name} must be byte-stable");
        assert_eq!(
            replaced,
            required_markers(name).len(),
            "{name} should contain every required block"
        );
    }
}

#[test]
fn a_hand_edited_row_is_reported_as_drift() {
    let contents = doc("README.md");
    let stale = contents.replacen(
        "| `0` | success | no | Read stdout as the command result. |",
        "| `999` | success | no | Read stdout as the command result. |",
        1,
    );
    assert_ne!(stale, contents, "negative control must edit a real row");
    assert_eq!(drifted_markers(&stale), vec!["EXIT CODES"]);
    let (rewritten, _) = apply_blocks(&stale).expect("rewrite");
    assert!(rewritten.contains(&render("EXIT CODES")));
}

#[test]
fn every_documented_env_var_has_a_read_site() {
    let registry = coding_agent_search::build_env_var_capabilities();
    let names: BTreeSet<&str> = registry.iter().map(|var| var.name.as_str()).collect();
    assert_eq!(names.len(), registry.len(), "registry names must be unique");

    let corpus = source_corpus();
    let mut unread = Vec::new();
    for name in &names {
        let quoted = format!("\"{name}\"");
        let occurrences = corpus.matches(&quoted).count();
        // One occurrence is the registry row; a documented variable needs at
        // least one more mention in a read path. Connector-owned roots are
        // resolved inside franken_agent_detection, so the registry row is
        // their in-repo declaration.
        let required = if CONNECTOR_OWNED_ENV_VARS.contains(name) {
            1
        } else {
            2
        };
        if occurrences < required {
            unread.push((*name).to_string());
        }
    }
    assert!(
        unread.is_empty(),
        "documented variables with no read site in src/: {unread:?}"
    );
}

#[test]
fn dependency_pin_table_contains_every_exact_or_git_pin() {
    let mut corpus = doc("README.md");
    corpus.push('\n');
    corpus.push_str(&doc("AGENTS.md"));
    let manifest: toml::Value = toml::from_str(
        &fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap(),
    )
    .expect("Cargo.toml parses");
    let dependencies = manifest["dependencies"].as_table().expect("[dependencies]");
    let pinned: Vec<&String> = dependencies
        .iter()
        .filter(|(_, spec)| {
            spec.get("git").is_some()
                || spec
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|version| version.starts_with('='))
        })
        .map(|(name, _)| name)
        .collect();
    assert!(pinned.len() >= 6, "expected the contract-critical pins");
    for name in pinned {
        assert!(
            corpus.contains(&format!("`{name}`")),
            "generated dependency tables omit pinned crate {name}"
        );
    }
}
