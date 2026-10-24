//! Runtime lookup of the per-provider ingest contracts baked by `build.rs`.
//!
//! A completed source may only be reused when the code that produced it is
//! unchanged, so the contract stored in the source ledger is compared against
//! the contract of the provider that produced it. The contract is scoped per
//! provider (bead `coding_agent_session_search-ayhvu`): an edit to one
//! connector's code invalidates that connector's completions, not every
//! connector's.

use std::collections::HashMap;
use std::sync::LazyLock;

/// Key of the conservative contract used for a provider slug that has no
/// scope entry of its own. It covers every provider's files, so an unknown
/// provider is never reused on the strength of a narrower scope.
const FALLBACK_KEY: &str = "*";

/// Contract used when the baked map is missing or malformed. It can never
/// equal a computed digest, so a missing map disables reuse instead of
/// blessing it.
const MISSING_CONTRACT: &str = "cass-source-ingest-contract-missing";

/// The baked contracts, keyed by provider slug.
static CONTRACTS: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| parse(env!("CASS_SOURCE_INGEST_CONTRACTS")));

fn contracts() -> &'static HashMap<&'static str, &'static str> {
    &CONTRACTS
}

fn parse(raw: &'static str) -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    for entry in raw.split(';') {
        let Some((slug, digest)) = entry.split_once('=') else {
            continue;
        };
        if slug.is_empty() || digest.is_empty() {
            continue;
        }
        map.insert(slug, digest);
    }
    map
}

/// The ingest contract of the provider that produced a source.
///
/// An unknown provider slug falls back to the conservative contract, which
/// covers every provider's files.
pub(crate) fn provider_contract(provider_slug: &str) -> &'static str {
    let map = contracts();
    map.get(provider_slug)
        .or_else(|| map.get(FALLBACK_KEY))
        .copied()
        .unwrap_or(MISSING_CONTRACT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored completion whose provider contract still matches stays
    /// reusable: the ledger matcher accepts it, so the indexer skips the
    /// source instead of reparsing and rewriting it.
    #[test]
    fn matching_provider_contract_keeps_a_completion_reusable() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"{}").unwrap();
        let contract = provider_contract("cline");
        let observation = serde_json::json!({
            "primary": crate::connectors::source_dependencies::observation::file_observation(&path)
                .unwrap(),
            "producer_contract": contract,
            "dependencies": [],
        })
        .to_string();
        assert!(
            crate::connectors::source_dependencies::observation::ledger_matches(
                &observation,
                &path,
                false,
                contract
            ),
            "a completion produced by the running provider contract must stay reusable"
        );
        let other = provider_contract("codex");
        assert_ne!(
            contract, other,
            "two providers must not share an ingest contract"
        );
        assert!(
            !crate::connectors::source_dependencies::observation::ledger_matches(
                &observation,
                &path,
                false,
                other
            ),
            "a completion must not be reused under another provider's contract"
        );
    }

    #[test]
    fn known_providers_resolve_and_unknown_providers_fall_back() {
        let map = contracts();
        assert!(
            map.contains_key(FALLBACK_KEY),
            "the baked contract map must carry a fallback contract"
        );
        for (slug, digest) in map {
            assert!(
                !digest.is_empty() && digest.chars().all(|c| c.is_ascii_hexdigit()),
                "contract for `{slug}` must be a hex digest"
            );
        }
        assert_eq!(
            provider_contract("codex"),
            map.get("codex").copied().unwrap_or(MISSING_CONTRACT)
        );
        assert_eq!(
            provider_contract("a-provider-that-does-not-exist"),
            map.get(FALLBACK_KEY).copied().unwrap(),
            "an unknown provider must use the conservative fallback contract"
        );
    }
}
