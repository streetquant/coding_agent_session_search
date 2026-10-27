//! Deterministic Markdown tables rendered from the code registries.
//!
//! README.md and AGENTS.md embed the output of this module between
//! `<!-- BEGIN GENERATED <NAME> -->` / `<!-- END GENERATED <NAME> -->`
//! markers. The drift test in `tests/docs_tables_match_registries.rs`
//! compares the embedded bytes with [`render`], and
//! `src/bin/cass-docs-tables.rs` regenerates both documents:
//!
//! ```text
//! cargo run --bin cass-docs-tables            # rewrite README.md and AGENTS.md
//! cargo run --bin cass-docs-tables -- --check # exit 1 on drift
//! ```
//!
//! Every table reads one registry, never a second hand-kept copy:
//!
//! * exit codes and retry guidance — [`build_exit_code_capabilities`]
//! * `err.kind` taxonomy — the `kind_str` arms of `ErrorKind`
//! * environment variables — [`build_env_var_capabilities`]
//! * connectors — the runtime connector registry
//! * dependency pins — `[dependencies]` in `Cargo.toml`
//! * TUI keymaps — `ui::shortcuts`' dispatch tables

use crate::model::cli_error_kind::ErrorKind;
use crate::{build_env_var_capabilities, build_exit_code_capabilities};

/// Markers embedded in README.md, in rewrite order.
pub const README_MARKERS: &[&str] = &[
    "EXIT CODES",
    "ERROR KINDS",
    "ENV VARS",
    "CONNECTORS",
    "DEPENDENCY PINS",
    "TUI KEYMAP",
    "TUI DETAIL KEYMAP",
    "TUI EXPORT KEYMAP",
];

/// Markers embedded in AGENTS.md, in rewrite order.
pub const AGENTS_MARKERS: &[&str] = &[
    "EXIT CODES",
    "ERROR KINDS",
    "ENV VAR INDEX",
    "CONNECTORS",
    "DEPENDENCY PINS",
];

/// Every marker owned by this module, once. The per-document lists overlap
/// (exit codes, kinds, connectors and pins appear in both files), so rewrite
/// loops must iterate this deduplicated union.
const ALL_MARKERS: &[&str] = &[
    "EXIT CODES",
    "ERROR KINDS",
    "ENV VARS",
    "ENV VAR INDEX",
    "CONNECTORS",
    "DEPENDENCY PINS",
    "TUI KEYMAP",
    "TUI DETAIL KEYMAP",
    "TUI EXPORT KEYMAP",
];

const README: &str = "README.md";
const AGENTS: &str = "AGENTS.md";

/// Files whose generated blocks this module owns.
pub const DOCUMENTS: &[&str] = &[README, AGENTS];

/// Markers that must be present in `file`.
#[must_use]
pub fn required_markers(file: &str) -> &'static [&'static str] {
    match file {
        README => README_MARKERS,
        AGENTS => AGENTS_MARKERS,
        _ => &[],
    }
}

/// Render one marker block. The returned text ends in a newline so the
/// closing marker starts on its own line.
#[must_use]
pub fn render(marker: &str) -> String {
    match marker {
        "EXIT CODES" => exit_codes_table(),
        "ERROR KINDS" => error_kinds_table(),
        "ENV VARS" => env_vars_table(),
        "ENV VAR INDEX" => env_var_index_table(),
        "CONNECTORS" => connectors_table(),
        "DEPENDENCY PINS" => dependency_pins_table(),
        "TUI KEYMAP" => crate::ui::shortcuts::markdown_reference(),
        "TUI DETAIL KEYMAP" => crate::ui::shortcuts::markdown_detail_reference(),
        "TUI EXPORT KEYMAP" => crate::ui::shortcuts::markdown_export_reference(),
        // ubs:ignore[rust.ownership.panic-macro] -- This internal dispatcher is called with the fixed generated-block registries; an unknown marker is a programmer error, not document input.
        other => panic!("unknown generated-docs marker: {other}"),
    }
}

/// Replace every generated block present in `doc`. Returns the rewritten
/// document and the number of blocks replaced.
pub fn apply_blocks(doc: &str) -> Result<(String, usize), String> {
    let mut out = doc.to_string();
    let mut replaced = 0;
    for marker in ALL_MARKERS.iter().copied() {
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- Each of the nine fixed registry markers needs its own exact opening delimiter for document replacement.
        let begin = format!("<!-- BEGIN GENERATED {marker} -->");
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- Pair this registry marker with its distinct closing delimiter; no per-message or unbounded input loop is involved.
        let end = format!("<!-- END GENERATED {marker} -->");
        let Some(start) = out.find(&begin) else {
            continue;
        };
        let Some(relative_end) = out
            .get(start + begin.len()..)
            .and_then(|tail| tail.find(&end))
        else {
            // ubs:ignore[rust.perf.string-alloc-in-loop] -- A malformed generated block returns one marker-specific diagnostic and stops rewriting immediately.
            return Err(format!(
                "{marker}: BEGIN marker without a matching END marker"
            ));
        };
        let content_end = start + begin.len() + relative_end;
        let mut replacement = String::with_capacity(begin.len() + end.len() + 256);
        replacement.push_str(&begin);
        replacement.push('\n');
        replacement.push_str(&render(marker));
        replacement.push_str(&end);
        out.replace_range(start..content_end + end.len(), &replacement);
        replaced += 1;
    }
    Ok((out, replaced))
}

/// Markers whose embedded block differs from [`render`].
#[must_use]
pub fn drifted_markers(doc: &str) -> Vec<&'static str> {
    let mut drifted = Vec::new();
    for marker in ALL_MARKERS.iter().copied() {
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- Drift detection checks the nine fixed generated-block opening delimiters, each once per document.
        let begin = format!("<!-- BEGIN GENERATED {marker} -->");
        // ubs:ignore[rust.perf.string-alloc-in-loop] -- The corresponding fixed closing delimiter is required to distinguish incomplete blocks from valid output.
        let end = format!("<!-- END GENERATED {marker} -->");
        let Some(start) = doc.find(&begin) else {
            continue;
        };
        let Some(relative_end) = doc
            .get(start + begin.len()..)
            .and_then(|tail| tail.find(&end))
        else {
            drifted.push(marker);
            continue;
        };
        let body_start = start + begin.len() + 1; // skip the newline after BEGIN
        let body_end = start + begin.len() + relative_end;
        let embedded = doc.get(body_start..body_end).unwrap_or_default();
        if embedded != render(marker) {
            drifted.push(marker);
        }
    }
    drifted
}

/// Markers missing from `doc`, restricted to the document's required set.
#[must_use]
pub fn missing_markers(file: &str, doc: &str) -> Vec<&'static str> {
    required_markers(file)
        .iter()
        .copied()
        .filter(|marker| {
            !doc.contains(&format!("<!-- BEGIN GENERATED {marker} -->"))
                || !doc.contains(&format!("<!-- END GENERATED {marker} -->"))
        })
        .collect()
}

fn cell(value: &str) -> String {
    value.replace('\n', " ").replace('|', "\\|")
}

fn exit_codes_table() -> String {
    use std::fmt::Write as _;

    let mut out = String::from(
        "| Code | Meaning | Retryable | Agent action |\n\
         |------|---------|-----------|--------------|\n",
    );
    for code in build_exit_code_capabilities() {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            cell(&code.code),
            cell(&code.meaning),
            cell(&code.retryable),
            cell(&code.agent_action)
        );
    }
    out
}

fn error_kinds_table() -> String {
    use std::fmt::Write as _;

    let kinds = error_kind_names();
    let mut out = format!(
        "The registry defines {} `err.kind` values; branch on the kind, not the numeric code, \
         for codes >= 10.\n\n| Kind |\n|------|\n",
        kinds.len()
    );
    for kind in kinds {
        let _ = writeln!(out, "| `{}` |", cell(&kind));
    }
    out
}

/// Wire names declared by `ErrorKind::kind_str`, in enum order.
fn error_kind_names() -> Vec<String> {
    const SOURCE: &str = include_str!("model/cli_error_kind.rs");
    SOURCE
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("Self::")?;
            let (_, wire) = rest.split_once(" => \"")?;
            let (wire, _) = wire.split_once('"')?;
            Some(wire.to_string())
        })
        .collect()
}

fn env_vars_table() -> String {
    use std::fmt::Write as _;

    let mut vars = build_env_var_capabilities();
    vars.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = format!(
        "Generated from the env-var registry (`cass capabilities --json | jq .env_vars`). \
         {} variables:\n\n| Variable | Default | Description |\n|----------|---------|-------------|\n",
        vars.len()
    );
    for var in vars {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} |",
            cell(&var.name),
            cell(var.default.as_deref().unwrap_or("unset")),
            cell(&var.description)
        );
    }
    out
}

fn env_var_index_table() -> String {
    use std::fmt::Write as _;

    let mut names: Vec<String> = build_env_var_capabilities()
        .into_iter()
        .map(|var| var.name)
        .collect();
    names.sort();
    names.dedup();
    let mut out = format!(
        "{} documented variables (`cass capabilities --json | jq .env_vars`):\n\n\
         | Variable | Variable | Variable | Variable |\n\
         |----------|----------|----------|----------|\n",
        names.len()
    );
    for chunk in names.chunks(4) {
        out.push('|');
        for index in 0..4 {
            match chunk.get(index) {
                Some(name) => {
                    let _ = write!(out, " `{}` |", cell(name));
                }
                None => out.push_str("  |"),
            }
        }
        out.push('\n');
    }
    out
}

fn connectors_table() -> String {
    use std::fmt::Write as _;

    let connectors = crate::capabilities_connector_names();
    let mut out = format!(
        "The runtime registry registers {} connectors \
         (`cass capabilities --json | jq .connectors`):\n\n\
         | Connector | Connector | Connector |\n\
         |-----------|-----------|-----------|\n",
        connectors.len()
    );
    for chunk in connectors.chunks(3) {
        out.push('|');
        for index in 0..3 {
            match chunk.get(index) {
                Some(name) => {
                    let _ = write!(out, " `{}` |", cell(name));
                }
                None => out.push_str("  |"),
            }
        }
        out.push('\n');
    }
    out
}

fn dependency_pins_table() -> String {
    use std::fmt::Write as _;

    // ubs:ignore[rust.ownership.unwrap-expect,rust.parsing.serde-unwrap] -- Cargo already parsed this immutable compile-time manifest before building the registry renderer; no runtime file is deserialized here.
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml"))
        .expect("Cargo.toml must parse for the generated dependency table");
    // ubs:ignore[rust.ownership.unwrap-expect] -- This crate's compile-time Cargo.toml necessarily has its required dependency table; document contents never select it.
    let dependencies = manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .expect("Cargo.toml must have a [dependencies] table");

    let mut pinned: Vec<(&String, &toml::Value)> = dependencies
        .iter()
        .filter(|(_, spec)| {
            spec.get("git").is_some()
                || spec
                    .get("version")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|version| version.starts_with('='))
        })
        .collect();
    pinned.sort_by_key(|(left, _)| *left);

    let mut out = format!(
        "{} exact or git-pinned direct dependencies (generated from `[dependencies]` in \
         `Cargo.toml`):\n\n| Dependency | Requirement | Features |\n\
         |------------|-------------|----------|\n",
        pinned.len()
    );
    for (name, spec) in pinned {
        let package = spec.get("package").and_then(toml::Value::as_str);
        let display = match package {
            Some(package) if package != name => format!("`{name}` (`{package}`)"),
            _ => format!("`{name}`"),
        };
        let requirement = if let Some(url) = spec.get("git").and_then(toml::Value::as_str) {
            let rev = spec.get("rev").and_then(toml::Value::as_str);
            let version = spec
                .get("version")
                .and_then(toml::Value::as_str)
                .map(|version| version.trim_start_matches('='));
            match (rev, version) {
                (Some(rev), Some(version)) => {
                    format!("git `{url}` rev `{rev}` (crate {version})")
                }
                (Some(rev), None) => format!("git `{url}` rev `{rev}`"),
                (None, Some(version)) => format!("git `{url}` (crate {version})"),
                (None, None) => format!("git `{url}`"),
            }
        } else {
            let version = spec
                .get("version")
                .and_then(toml::Value::as_str)
                .unwrap_or("unspecified");
            format!("crates.io `{version}`")
        };
        let features = spec
            .get("features")
            .and_then(toml::Value::as_array)
            .map(|features| {
                features
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(|feature| format!("`{feature}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let _ = writeln!(out, "| {display} | {} | {features} |", cell(&requirement));
    }
    out
}

/// Every parsed error-kind wire name resolves through the typed registry.
#[must_use]
pub fn error_kinds_round_trip() -> bool {
    error_kind_names()
        .iter()
        .all(|kind| ErrorKind::from_kind_str(kind).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_str_arm_round_trips() {
        let names = error_kind_names();
        assert!(
            names.len() > 50,
            "expected the full taxonomy, got {}",
            names.len()
        );
        for name in names {
            assert!(
                ErrorKind::from_kind_str(&name).is_some(),
                "{name} has a kind_str arm but no from_kind_str arm"
            );
        }
    }

    #[test]
    fn marker_rewrite_is_idempotent() {
        let original = "intro\n<!-- BEGIN GENERATED EXIT CODES -->\nstale\n<!-- END GENERATED EXIT CODES -->\noutro\n";
        let (first, replaced) = apply_blocks(original).unwrap();
        assert_eq!(replaced, 1);
        assert!(first.contains(&render("EXIT CODES")));
        let (second, replaced) = apply_blocks(&first).unwrap();
        assert_eq!(replaced, 1);
        assert_eq!(first, second, "a second rewrite must be byte-identical");
    }

    #[test]
    fn stale_row_is_reported_as_drift() {
        let original = "<!-- BEGIN GENERATED EXIT CODES -->\n| Code | Meaning | Retryable | Agent action |\n|------|---------|-----------|--------------|\n| `999` | stale | no | stale |\n<!-- END GENERATED EXIT CODES -->\n";
        assert_eq!(drifted_markers(original), vec!["EXIT CODES"]);
        assert!(missing_markers(README, original).contains(&"ENV VARS"));
    }
}
