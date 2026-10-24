//! Ingest scope: the build inputs that can change what a source's stored
//! result looks like.
//!
//! A completed source may only be reused when the code that produced it is
//! unchanged. `build.rs` resolves the tables below against the crate tree and
//! bakes one contract per provider; the crate looks the contract up by the
//! provider slug recorded in the source ledger. A change to a file outside a
//! provider's scope leaves that provider's contract untouched, so an upgrade
//! that only touches unrelated code no longer forces a full reparse and
//! rewrite of every source (bead `coding_agent_session_search-ayhvu`).
//!
//! `build.rs` includes this file verbatim, so it may only use `std` and
//! `blake3`: no `crate::` paths and no other dependencies.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Digest scheme tag. Bump it whenever the hashing scheme changes so a
/// contract written by an older build can never match a newer one.
pub(crate) const CONTRACT_SCHEME: &str = "cass-source-ingest-contract-v2";

/// Build inputs every provider's producer depends on: the manifest, the
/// resolved dependency graph (the pinned upstream parser crate is pinned
/// there) and the build script itself.
pub(crate) const BUILD_INPUTS: &[&str] = &["Cargo.toml", "Cargo.lock", "build.rs"];

/// The shared ingest surface: code every provider's producer runs through.
/// A change here can change what any provider stores, so it invalidates every
/// provider's completions.
///
/// Directories are listed whole so a new module inside them is covered
/// automatically; the files that are deliberately outside the ingest surface
/// are named in `CORE_SCOPE_EXCLUDED`.
pub(crate) const CORE_SCOPE: &[&str] = &[
    // The connector registry and the source-dependency policy that decides
    // whether a completed source may be reused at all.
    "src/connectors/mod.rs",
    "src/connectors/source_dependencies.rs",
    "src/connectors/source_dependencies",
    "src/connectors/ingest_scope.rs",
    "src/connectors/ingest_contract.rs",
    // These connector modules are called from the indexer's provider-agnostic
    // paths (source path exclusions, session-root ownership, rollout
    // headers), so their behavior is shared, not provider-local.
    "src/connectors/codex.rs",
    "src/connectors/codex",
    "src/connectors/omp.rs",
    "src/connectors/cloudmcp.rs",
    "src/connectors/openwebui.rs",
    // The ingest engine: normalization, redaction, the stored message shape,
    // the writer, and the source identity/provenance rules.
    "src/indexer",
    "src/model",
    "src/storage",
    "src/sources",
    "src/raw_mirror",
    "src/franken_sync.rs",
];

/// Files and directories inside the core scope that are not part of the
/// ingest surface. Each entry is a reviewed judgment: the code reads the
/// stored rows (or the archive around them) instead of deciding what a
/// source stores, or it is test-only code that never reaches the shipped
/// binary. A directory entry excludes everything below it.
pub(crate) const CORE_SCOPE_EXCLUDED: &[&str] = &[
    // Test-only code.
    "src/indexer/checkpoint_deadline.rs",
    "src/indexer/source_ledger_tests.rs",
    "src/indexer/gh473_tests.rs",
    "src/indexer/gh494_tests.rs",
    "src/indexer/fixtures",
    "src/connectors/preflight_tests.rs",
    "src/sources/tailscale_tests.rs",
    // The lexical and semantic indexes read the stored rows; they do not
    // decide what a source stores.
    "src/indexer/lexical_generation.rs",
    "src/indexer/lexical_publish",
    "src/indexer/lexical_publish.rs",
    "src/indexer/lexical_reconcile",
    "src/indexer/lexical_reconcile.rs",
    "src/indexer/semantic",
    "src/indexer/semantic.rs",
    "src/indexer/semantic_progress.rs",
    // Refresh, watchdog and evidence bookkeeping around the ingest run.
    "src/indexer/background_refresh",
    "src/indexer/background_refresh.rs",
    "src/indexer/refresh_ledger.rs",
    "src/indexer/responsiveness",
    "src/indexer/responsiveness.rs",
    "src/indexer/staging_reclaim.rs",
    "src/indexer/archive_lease.rs",
    "src/indexer/source_writer_probe",
    "src/indexer/source_writer_probe.rs",
    // The streaming-OOM quarantine state and the unused memoization cache.
    "src/indexer/quarantine",
    "src/indexer/quarantine.rs",
    "src/indexer/quarantine_retry.rs",
    "src/indexer/memoization.rs",
    // The CLI error taxonomy and the packet equivalence audit hook.
    "src/model/cli_error_kind.rs",
    "src/model/packet_audit.rs",
    // Source installation, setup and discovery tooling: the ingest path reads
    // the configuration, the provenance and the sync helpers, not the
    // installer.
    "src/sources/install.rs",
    "src/sources/setup.rs",
    "src/sources/interactive.rs",
    "src/sources/probe.rs",
    "src/sources/index",
    "src/sources/index.rs",
    "src/sources/config_validation.rs",
];

/// Per-provider scope: the CASS-side sources that can change one provider's
/// parse output, keyed by the provider slug recorded in the source ledger.
///
/// An empty entry list means the provider has no CASS-side parser code: its
/// producer is the pinned upstream crate, so the shared core is its whole
/// scope. A provider slug that is missing from this table is treated as
/// unknown and falls back to the conservative "every provider" contract.
pub(crate) const PROVIDER_SCOPE: &[(&str, &[&str])] = &[
    ("aider", &["src/connectors/aider.rs"]),
    ("amp", &["src/connectors/amp.rs"]),
    ("antigravity", &["src/connectors/antigravity.rs"]),
    ("chatgpt", &["src/connectors/chatgpt.rs"]),
    ("claude_code", &["src/connectors/claude_code.rs"]),
    ("clawdbot", &["src/connectors/clawdbot.rs"]),
    ("cline", &["src/connectors/cline.rs"]),
    ("cloudmcp", &[]),
    (
        "codebuff",
        &["src/connectors/codebuff.rs", "src/connectors/codebuff"],
    ),
    ("codex", &[]),
    (
        "continue",
        &[
            "src/connectors/continue_dev.rs",
            "src/connectors/continue_dev",
        ],
    ),
    ("copilot", &["src/connectors/copilot.rs"]),
    ("copilot_cli", &["src/connectors/copilot_cli.rs"]),
    ("crush", &["src/connectors/crush.rs"]),
    ("cursor", &["src/connectors/cursor.rs"]),
    ("devin", &[]),
    ("factory", &["src/connectors/factory.rs"]),
    ("gemini", &["src/connectors/gemini.rs"]),
    ("goose", &["src/connectors/goose.rs"]),
    ("grok", &["src/connectors/grok.rs"]),
    ("grok_bot", &["src/connectors/grok.rs"]),
    ("hermes", &["src/connectors/hermes.rs"]),
    ("kilo", &["src/connectors/kilo.rs"]),
    ("kimi", &["src/connectors/kimi.rs"]),
    ("kiro", &[]),
    ("muse", &["src/connectors/muse.rs"]),
    ("omp", &[]),
    ("openclaw", &["src/connectors/openclaw.rs"]),
    ("opencode", &["src/connectors/opencode.rs"]),
    ("openhands", &["src/connectors/openhands.rs"]),
    ("openwebui", &[]),
    ("pi_agent", &["src/connectors/pi_agent.rs"]),
    ("pi_durable", &[]),
    ("prime_agent", &[]),
    ("qwen", &["src/connectors/qwen.rs"]),
    ("shelley", &[]),
    ("vibe", &["src/connectors/vibe.rs"]),
];

/// The scope entries of one provider, or `None` when the provider is unknown.
pub(crate) fn provider_entries(provider: &str) -> Option<&'static [&'static str]> {
    PROVIDER_SCOPE
        .iter()
        .find(|(slug, _)| *slug == provider)
        .map(|(_, entries)| *entries)
}

/// Resolve scope entries to the crate-relative files they cover.
///
/// A directory entry contributes every file below it; the exclusions are
/// dropped from every scope. The result is sorted and deduplicated so the
/// digest does not depend on the order the entries were listed in.
pub(crate) fn resolve_scope(entries: &[&str], root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in entries {
        let relative = PathBuf::from(entry);
        let absolute = root.join(&relative); // ubs:ignore[rust.security.path-traversal] -- Entries are reviewed compile-time scope paths.
        let metadata = fs::metadata(&absolute)
            .map_err(|error| format!("ingest scope entry `{entry}` is unreadable: {error}"))?;
        if metadata.is_dir() {
            collect_directory(root, &relative, &mut files)?;
        } else {
            files.insert(scope_key(&relative), relative);
        }
    }
    for excluded in CORE_SCOPE_EXCLUDED {
        files.retain(|name, _| !is_excluded(name, excluded));
    }
    Ok(files.into_values().collect())
}

/// Whether a scope file is covered by an exclusion entry. A directory entry
/// excludes everything below it.
pub(crate) fn is_excluded(name: &str, excluded: &str) -> bool {
    let excluded = scope_key(Path::new(excluded));
    name == excluded || name.starts_with(&format!("{excluded}/"))
}

/// Resolve the shared core scope: the build inputs plus the shared ingest
/// surface.
pub(crate) fn core_scope(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries: Vec<&str> = BUILD_INPUTS.to_vec();
    entries.extend_from_slice(CORE_SCOPE);
    resolve_scope(&entries, root)
}

/// Resolve one provider's own scope (its files beyond the shared core).
pub(crate) fn provider_scope(provider: &str, root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = provider_entries(provider)
        .ok_or_else(|| format!("unknown ingest scope provider `{provider}`"))?;
    resolve_scope(entries, root)
}

/// Resolve every provider's own scope at once, for the conservative fallback
/// contract that unknown providers use.
pub(crate) fn all_provider_scopes(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (provider, _) in PROVIDER_SCOPE {
        for file in provider_scope(provider, root)? {
            files.insert(scope_key(&file), file);
        }
    }
    Ok(files.into_values().collect())
}

/// Digest a set of scope files.
///
/// `overrides` replaces the on-disk bytes of a file. It exists so a test can
/// simulate an edit without touching the working tree; production callers
/// pass an empty map.
pub(crate) fn digest_files(
    files: &[PathBuf],
    root: &Path,
    overrides: &BTreeMap<String, Vec<u8>>,
) -> Result<String, String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CONTRACT_SCHEME.as_bytes());
    hasher.update(b"\0");
    for file in files {
        let name = scope_key(file);
        let absolute = root.join(file); // ubs:ignore[rust.security.path-traversal] -- Files come from the reviewed build-scope resolver.
        let bytes = match overrides.get(&name) {
            Some(bytes) => Cow::Borrowed(bytes.as_slice()),
            None => Cow::Owned(
                fs::read(absolute)
                    .map_err(|error| format!("cannot read ingest scope file `{name}`: {error}"))?,
            ),
        };
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Combine already-computed digests into one contract.
///
/// The provider contract is `core || provider || features`: a change in any
/// part changes the contract, and an unrelated part cannot.
pub(crate) fn compose_digest(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CONTRACT_SCHEME.as_bytes());
    hasher.update(b"\0");
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// The scope digest of one provider: the shared core plus the provider's own
/// files. The enabled feature set is added by `build.rs`, which alone knows
/// it.
pub(crate) fn provider_scope_digest(
    provider: &str,
    root: &Path,
    overrides: &BTreeMap<String, Vec<u8>>,
) -> Result<String, String> {
    let core = digest_files(&core_scope(root)?, root, overrides)?;
    let own = digest_files(&provider_scope(provider, root)?, root, overrides)?;
    Ok(compose_digest(&[&core, &own]))
}

fn scope_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn collect_directory(
    root: &Path,
    relative: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<(), String> {
    let absolute = root.join(relative); // ubs:ignore[rust.security.path-traversal] -- Recursion starts at static scope paths and appends directory entry names.
    let entries = fs::read_dir(&absolute)
        .map_err(|error| format!("cannot read ingest scope directory `{relative:?}`: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read ingest scope entry: {error}"))?;
        let name = entry.file_name();
        let child = relative.join(&name);
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot inspect ingest scope entry: {error}"))?;
        if kind.is_dir() {
            collect_directory(root, &child, files)?;
        } else {
            files.insert(scope_key(&child), child);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn no_overrides() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::new()
    }

    /// An unrelated file under `src/` is not part of any provider's scope, so
    /// editing it cannot invalidate a stored completion.
    #[test]
    fn unrelated_src_change_leaves_every_provider_scope_digest_unchanged() {
        let root = root();
        let unrelated = "src/lib.rs";
        assert!(
            !core_scope(&root)
                .unwrap()
                .iter()
                .any(|file| scope_key(file) == unrelated),
            "{unrelated} must not be part of the shared core scope"
        );
        for (provider, _) in PROVIDER_SCOPE {
            let files = provider_scope(provider, &root).unwrap();
            assert!(
                !files.iter().any(|file| scope_key(file) == unrelated),
                "{unrelated} must not be part of the {provider} scope"
            );
        }

        let before = provider_scope_digest("cline", &root, &no_overrides()).unwrap();
        let mut overrides = no_overrides();
        overrides.insert(unrelated.to_string(), b"// an unrelated edit\n".to_vec());
        let after = provider_scope_digest("cline", &root, &overrides).unwrap();
        assert_eq!(
            before, after,
            "an unrelated src/ edit must not change the cline ingest scope digest"
        );

        // The digest is content sensitive, so the equality above is a
        // property of the scope and not of a digest that ignores content.
        let mut scoped_overrides = no_overrides();
        scoped_overrides.insert("src/indexer/mod.rs".to_string(), b"// an edit\n".to_vec());
        assert_ne!(
            digest_files(&core_scope(&root).unwrap(), &root, &scoped_overrides).unwrap(),
            digest_files(&core_scope(&root).unwrap(), &root, &no_overrides()).unwrap(),
            "the scope digest must react to a changed file that is inside the scope"
        );
    }

    /// A connector edit invalidates that connector's completions only.
    #[test]
    fn provider_scoped_change_invalidates_only_that_provider() {
        let root = root();
        let changed = "src/connectors/cline.rs";
        let mut overrides = no_overrides();
        overrides.insert(changed.to_string(), b"// an unrelated edit\n".to_vec());

        let cline_before = provider_scope_digest("cline", &root, &no_overrides()).unwrap();
        let cline_after = provider_scope_digest("cline", &root, &overrides).unwrap();
        assert_ne!(
            cline_before, cline_after,
            "a connector edit must change that connector's scope digest"
        );

        for provider in ["codex", "claude_code", "omp", "cursor", "opencode"] {
            assert_eq!(
                provider_scope_digest(provider, &root, &no_overrides()).unwrap(),
                provider_scope_digest(provider, &root, &overrides).unwrap(),
                "a connector edit must not change the {provider} scope digest"
            );
        }
    }

    /// The conservative fallback for an unknown provider covers every
    /// provider's files, so no provider edit can hide from it.
    #[test]
    fn unknown_provider_fallback_covers_every_provider_file() {
        let root = root();
        let all = all_provider_scopes(&root).unwrap();
        let mut overrides = no_overrides();
        overrides.insert(
            "src/connectors/cline.rs".to_string(),
            b"// an unrelated edit\n".to_vec(),
        );
        let before = digest_files(&all, &root, &no_overrides()).unwrap();
        let after = digest_files(&all, &root, &overrides).unwrap();
        assert_ne!(
            before, after,
            "the fallback contract must react to any provider's files"
        );
    }

    /// The build script and the crate resolve the same tables, so the scope
    /// digests baked at build time must match the ones recomputed here.
    #[test]
    fn baked_scope_digests_match_the_tables() {
        let root = root();
        let baked = baked_digests();
        for (provider, _) in PROVIDER_SCOPE {
            let expected = provider_scope_digest(provider, &root, &no_overrides()).unwrap();
            assert_eq!(
                baked.get(*provider).map(String::as_str),
                Some(expected.as_str()),
                "the baked scope digest for `{provider}` must match the tables"
            );
        }
        let core = digest_files(&core_scope(&root).unwrap(), &root, &no_overrides()).unwrap();
        let all =
            digest_files(&all_provider_scopes(&root).unwrap(), &root, &no_overrides()).unwrap();
        let fallback = compose_digest(&[&core, &all]);
        assert_eq!(
            baked.get("*").map(String::as_str),
            Some(fallback.as_str()),
            "the baked fallback scope digest must match the tables"
        );
    }

    /// The digests the build script baked into the binary. The runtime
    /// override lets the same check run outside a cargo build; a cargo build
    /// bakes the value in at compile time.
    fn baked_digests() -> BTreeMap<String, String> {
        let raw = std::env::var("CASS_SOURCE_INGEST_SCOPE_DIGESTS")
            .unwrap_or_else(|_| env!("CASS_SOURCE_INGEST_SCOPE_DIGESTS").to_string());
        parse_digests(&raw)
    }

    /// Print the scope digest map in the encoding `build.rs` uses, so the
    /// build script's output can be compared with the tables by hand.
    #[test]
    #[ignore = "prints the scope digest map for verification"]
    fn print_scope_digests() {
        let root = root();
        let core = digest_files(&core_scope(&root).unwrap(), &root, &no_overrides()).unwrap();
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for (provider, _) in PROVIDER_SCOPE {
            let own = digest_files(
                &provider_scope(provider, &root).unwrap(),
                &root,
                &no_overrides(),
            )
            .unwrap();
            map.insert((*provider).to_string(), compose_digest(&[&core, &own]));
        }
        let all =
            digest_files(&all_provider_scopes(&root).unwrap(), &root, &no_overrides()).unwrap();
        map.insert("*".to_string(), compose_digest(&[&core, &all]));
        let mut encoded = String::from("v2");
        for (slug, digest) in &map {
            encoded.push(';');
            encoded.push_str(slug);
            encoded.push('=');
            encoded.push_str(digest);
        }
        println!("SCOPE_DIGESTS={encoded}");
    }

    fn parse_digests(raw: &str) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for entry in raw.split(';').skip(1) {
            let Some((slug, digest)) = entry.split_once('=') else {
                continue;
            };
            map.insert(slug.to_string(), digest.to_string());
        }
        map
    }

    /// Every connector module must be classified: either as part of the
    /// shared core or as the scope of at least one provider. A module that is
    /// in both would silently defeat the provider scoping, so that is
    /// rejected too.
    #[test]
    fn every_connector_module_is_classified() {
        let root = root();
        let core: BTreeMap<String, usize> = core_scope(&root)
            .unwrap()
            .into_iter()
            .map(|file| (scope_key(&file), 0))
            .collect();
        let mut claimed: BTreeMap<String, usize> = BTreeMap::new();
        for (provider, _) in PROVIDER_SCOPE {
            for file in provider_scope(provider, &root).unwrap() {
                *claimed.entry(scope_key(&file)).or_insert(0) += 1;
            }
        }

        let connectors = root.join("src/connectors");
        let mut stack = vec![connectors.clone()];
        let mut visited = Vec::new();
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    visited.push(path);
                }
            }
        }
        assert!(!visited.is_empty(), "the connector tree must not be empty");
        for path in visited {
            let name = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if CORE_SCOPE_EXCLUDED
                .iter()
                .any(|excluded| is_excluded(&name, excluded))
            {
                continue;
            }
            let in_core = core.contains_key(&name);
            let in_provider = claimed.contains_key(&name);
            assert!(
                in_core || in_provider,
                "connector module `{name}` is not classified by any scope"
            );
            assert!(
                !(in_core && in_provider),
                "connector module `{name}` is both shared and provider-local"
            );
        }
    }
}
