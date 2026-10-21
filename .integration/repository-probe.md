# Repository Probe

Generated: `2026-10-20T19:13:25+00:00`
Root: `/warp/projects/softwares/cass`

## Git State

- HEAD: `3254b9701105ccd3808bfa2c424cedc10e597a0e`
- Branch: `main`
- Changed/untracked paths: `236`
- Diff fingerprint: `ff0853fcad575ab06252a182229d13f54dde2bbaa57dea36033e76dd69f01130`

## Architecture Inputs

### Instructions
- `AGENTS.md` (`0bfe41739182`)

### Dependency and Build Manifests
- `Cargo.lock` (`36f3fa4b978a`)
- `Cargo.toml` (`4018cbdac25c`)
- `fuzz/Cargo.lock` (`da249a009e84`)
- `fuzz/Cargo.toml` (`54168ed4025a`)
- `tests/package-lock.json` (`198a7a6ad347`)
- `tests/package.json` (`7d7adfe2df4f`)
- `tests/performance/package.json` (`090be78dd57d`)

### Languages
- Rust: 553 files
- Shell: 58 files
- JavaScript: 42 files
- TypeScript: 30 files
- Python: 11 files
- PowerShell: 1 files
- Ruby: 1 files

## Candidate Verification Commands

- **format**: `cargo fmt --all -- --check` from `.` (medium confidence; source `Cargo.toml`)
- **format**: `cargo fmt --all -- --check` from `fuzz` (medium confidence; source `fuzz/Cargo.toml`)
- **tests**: `cargo test --all-targets` from `.` (medium confidence; source `Cargo.toml`)
- **tests**: `cargo test --all-targets` from `fuzz` (medium confidence; source `fuzz/Cargo.toml`)

## Tool Availability

- `git`: git version 2.56.0
- `br`: br 0.6.0
- `bv`: bv v0.25.0
- `rg`: ripgrep 15.2.0
- `ast-grep`: ast-grep 0.43.0
- `sg`: ast-grep 0.43.0
- `ctags`: Universal Ctags 6.2.1(v6.2.1), Copyright (C) 2015-2025 Universal Ctags Team
- `jq`: jq-1.8.2
- `python`: Python 3.14.7
- `node`: v26.11.1

## Warnings

- **dirty-worktree**: Repository has 236 changed or untracked paths; preserve user-owned changes.
