# Quill query-fuel failures: investigation and bounded measurements

Issue: [#518](https://github.com/Dicklesworthstone/coding_agent_session_search/issues/518).

**Status:** the engine accounting correction and optional-retry recovery are
landed on main. Release shipment and the reporter's archive latency remain
unverified. Keep #518 open until its remaining acceptance evidence is available.
The Python driver tests below establish measurement-tool behavior only.

**Update 2026-10-11 (cass bead 7hvlg):** the reporter's engine version is
settled. cass 0.10.0 ships frankensearch-quill 0.3.4, which still charged a
cached term statistic again for every segment it searched (Frankensearch #41),
so the charge grew with the square of the segment count. The test
`search::quill_bridge::tests::gh518_two_word_query_fuel_is_linear_in_segment_count`
builds 120 one-document segments and runs `git fixture` under a 40,000-unit
budget. On v0.10.0 it fails with `fuel exhausted after 40000/40000 units
(segments=42, dictionary_blocks=39794, posting_blocks=164)`. On main, which
includes the fix from Quill 0.4.0 (currently pinned at 0.4.1), it passes.
The fix reaches users with the next cass release.

## Current native coverage and limits

The landed `search_with_fallback` path preserves the exact page when its
optional wildcard retry exhausts fuel, reporting
`_meta.wildcard_fallback_skipped="query_fuel_exhausted"`. A larger optional
deduplication fetch can retain its filtered native page with an unknown total
and `lexical_degrade_reason="dedup_retry_query_fuel_exhausted"`. Unrelated
errors and a required primary fetch with no retained page still fail.

`cass serve --lexical-fallback --db <archive>` separately opts into supervised
FrankenSQLite recovery. `--db` alone does not grant that consent. Its tests in
`tests/quill_fuel_fallback.rs` exercise genuine engine refusals, canonical result
identities, disabled recovery, oversized/corrupt archives, authoritative empty
native results, and a 206-segment service fixture. That service fixture uses an
explicit 80,000-unit budget; it is not ordinary CLI evidence at the default
budget.

The same integration target now contains
`gh518_ordinary_cli_answers_both_queries_and_no_match_on_206_sparse_segments`.
It builds 206 actual live segments containing 3,296 synthetic documents, half
with continuous commits and half with reopened writers. It invokes ordinary
`cass search` at the published default budget for both reported queries, a
no-match control, and a following request. A separate one-unit primary refusal
checks the error's recovery guidance. The child deadline fails the test rather
than accepting a deadline-shaped empty response. Its elapsed values include CLI
startup and index opening; they are fixture observations, not a reporter-archive
benchmark or a measured improvement.

For exact Linux release qualification, that ordinary CLI test accepts
`CASS_TEST_CANDIDATE_BIN` together with `CASS_TEST_CANDIDATE_SHA256`. A missing,
invalid or mismatched supplied identity fails before fixture creation; it never
substitutes Cargo's binary. The test hashes the selected executable before and
after its CLI requests and emits `GH518_BINARY` / `GH518_BINARY_AFTER` receipts.
The other five service tests still use Cargo's binary. This harness extension
awaits its own qualification; it does not turn the earlier R27B result into
release evidence.

**Linux functional qualification, R27B (2026-10-23):** all six tests in
`quill_fuel_fallback` passed on `cass-linux-192-168-68-50`; the complete target
finished in 13.93 seconds. Its terminal receipts are
`TEST_COUNT=test-quill_fuel_fallback PASSED=6 BINARIES=1` and
`STAGE=test-quill_fuel_fallback EXIT=0`. The tested source-content SHA256 is
`1e7c9064bd1ae4ff976bafb2179f83e6bf1f49b86961455bca8f83127ada351a`;
the test file SHA256 is
`c0fd61855b10194beac889d3dc396ddd8ef8860bb0e9b9abbc76c1b7f9bacb9f`.
The retained receipt is
`/cyber-b/cass-finish-01a16542-r27b-parallel-ready.log`. The operator pause
subsequently interrupted the gate; unrelated clippy and library failures had
already made it red, and UBS/goldens did not run. This is a passing integration
target on that source, not a full-gate or release qualification.

Do not infer actual charge from the engine's private work upper bound, or infer
that upper bound from a successful execution. Those are different quantities.
The reporter's 110-second query still has no verified explanation. Fuel counters
are admission accounting, not measured physical reads or a wall-clock deadline.

The ordinary pre-parsed CASS query API meters each call independently, returns
no successful-consumption receipt, and accepts no public request-scoped
checkpoint. Quill's optional `profile-internals` diagnostic feature can record
admitted work for its string-query entry point; CASS does not enable that
feature, and it does not extend the ordinary CASS result or budget contract.
Ordinary search can issue additional count/refinement calls. The existing
changes therefore do not establish a shared request-wide fuel ceiling. A
reporter-archive reproduction, the requested latency/work comparison, and
release qualification remain open; synthetic fixtures cannot discharge those
requirements.

## What the inspected code establishes

The initial CASS revision examined was
`fb51773cac0ae07d1631a01fb78897c71d639713`. Relevant sources:

- [`src/search/quill_bridge.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/src/search/quill_bridge.rs)
  keeps the normal Quill budget at ten million units unless a positive
  `CASS_QUILL_QUERY_FUEL_BUDGET` override is supplied. `search_paginated` wraps
  engine errors as strings; `is_query_fuel_exhausted` recognizes the error chain
  by its message. Work units are not a wall-clock deadline.
- [`src/search/query.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/src/search/query.rs)
  executes `search_tantivy` with a **Quill** reader and native CASS query parser.
  `SearchClient::search` propagates failures from the primary call, its
  overfetch retry, and the federated lane with `?`. The SQLite lane below those
  branches is reached when no lexical reader is available, not when a live
  Quill reader exhausts fuel. A successful empty primary result is deliberately
  authoritative; it must not resurrect stale SQLite matches.
- The SQLite lane is not automatically a cheap indexed substitute.
  `search_sqlite_fts5` falls back to `search_sqlite_message_scan` for unsupported
  grammar, missing/unusable FTS, integrity failures, and several execution or
  hydration failures. The source scan uses keyset pages inside one read
  snapshot and retains a bounded top-k, but its outer loop still traverses the
  eligible archive. Page size is not a request-wide row, byte, or time budget.
- [`src/search/tantivy.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/src/search/tantivy.rs)
  retains compatibility names while using Quill for ingestion and execution.
  [`tests/logging.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/tests/logging.rs)
  still expects `backend="tantivy"`. Neither that label nor a `search_start`
  event proves that an independent Tantivy engine served the result.

There is important upstream history in
[Frankensearch #41](https://github.com/Dicklesworthstone/frankensearch/issues/41).
The [September 20 correction](https://github.com/Dicklesworthstone/frankensearch/issues/41#issuecomment-5752454821)
reports that cached snapshot statistics had been charged again per segment,
although those dictionary reads were not repeated. It also describes sharing
statistics with range/set/glob lowering and changing the matching work ceiling.
Consequently, the #518 dictionary counter alone is **not** evidence of ten
million physical dictionary probes, nor proof that the same old accounting bug
remains in the reporter's binary. Verify the actual engine source/version used
by that binary before choosing an upstream patch or dependency update.

Fragmentation remains relevant independently of the meter: reopened writers can
leave sparse document-ID leases. The
[September 11 upstream experiment](https://github.com/Dicklesworthstone/frankensearch/issues/41)
reported substantial size amplification from forcing concat over sparse leases.
Do not remove merge hole guards, force large query-time merges, or increase the
global fuel budget as a substitute for a measured correction.

## Recovery without deleting the canonical archive

Use the **same database and data directory as the failing command**. Set these
variables to the actual paths; do not paste the placeholders unchanged:

```sh
DB=/absolute/path/to/agent_search.db
DATA_DIR=/absolute/path/to/cass-data
cass --version
cass --db "$DB" index --full --json --data-dir "$DATA_DIR"
cass --db "$DB" search 'git fixture' --limit 1 --json --data-dir "$DATA_DIR"
cass --db "$DB" search 'test isolation' --limit 1 --json --data-dir "$DATA_DIR"
```

`cass index --full --json` with matching `--db`/`--data-dir` is the recovery
command already emitted by
[`inline_repair.rs`](https://github.com/Dicklesworthstone/coding_agent_session_search/blob/fb51773cac0ae07d1631a01fb78897c71d639713/src/search/inline_repair.rs).
It is a manual, potentially expensive rebuild, **not** a verified cure for the
reporter's archive. Preserve a consistent backup first, arrange enough free disk
space, and stop competing indexing/watch activity during the maintenance window.
Do not delete `agent_search.db`, its WAL, source session files, or the last usable
index to work around a query refusal. A filesystem copy of an actively written
SQLite database without its consistent WAL state is not a valid backup.

Preserve the original failure, executable identity, engine versions, and actual
live segment count before rebuilding. A count of segment files on disk is not
necessarily a live segment count: retired files can still be present. Compare
result identities, filters, and live document counts afterward, not just exit
status. Capture any rebuild error separately; repeatedly retrying the original
query with more fuel is not recovery.

## Bounded command-level measurements

`scripts/benchmark_quill_query_fuel.py` requires Python 3.11+ and POSIX process
groups (Linux/macOS). Run it against a **disposable, consistent copy** of the
archive and index. CASS search itself can perform maintenance; this script does
not promise read-only CASS behavior and never invokes a rebuild.

```sh
python3 scripts/benchmark_quill_query_fuel.py \
  --cass /absolute/path/to/cass \
  --db /absolute/path/to/disposable-copy/agent_search.db \
  --data-dir /absolute/path/to/disposable-copy \
  --disposable-copy --reported-segment-count 206 \
  --label 'baseline: record source revision and corpus identifier here' \
  --repetitions 5 --timeout-seconds 10 \
  --output /absolute/path/to/new-gh518-baseline.json
```

The default queries are `git fixture` and `test isolation`, explicitly in lexical
mode with limit one. `--reported-segment-count` is operator-supplied provenance,
not a count the script verifies. `--query` can be repeated for additional cases.
Each invocation gets a fresh process; the driver does **not** flush OS caches or
measure a warm in-process searcher. Latency includes CLI startup, index open,
search, hydration, shutdown, and observation overhead. Query order alternates.

The default external deadline is ten seconds per invocation, with a combined
256 KiB stdout/stderr capture cap. A deadline or output overflow kills/reaps the
private process group. This contains ordinary children in that group; it is not
proof of engine-internal cancellation and does not cancel unrelated daemons or
processes that deliberately detach into another session. No shell is involved.
Limits on repetitions, query count, and timeout prevent an accidentally
unbounded suite. Use a disposable local setup without external query workers.

The receipt includes executable SHA-256, requested budget override, per-query
outcomes and latency percentiles, result counts, distinct fuel-refusal counters,
and observed `search_start` labels. It does **not** persist returned message
content or raw stderr. It does retain the query and explicitly supplied paths,
so review the receipt before sharing it publicly. Output files are never
silently overwritten.

Interpretation rules:

- Failed, timed-out, or malformed-output samples are counted separately, not
  included as artificially fast successful samples. Exit status is nonzero if
  any sample fails. A successful empty result still needs a correctness oracle.
- Missing work counters are `null`, **not zero**. Current CLI success output does
  not supply a successful-query work receipt. Refusal counters measure admitted
  work, not measured physical probes. Repeated error context is deduplicated.
- `serving_backend` remains `null`: attempted backend labels alone are
  insufficient evidence. In particular, this driver does not relabel
  `tantivy` as a demonstrated independent backend or claim a successful fallback.

An explicit `--fuel-budget N` runs every sample at that single positive budget.
The inherited environment override is removed when this option is absent.
There is no adaptive increase and no automatic retry after a refusal. For a
before/after comparison, run separately with each executable on equivalent
snapshots and retain both receipts. A timeout is a censored observation, not an
engine latency percentile. Actual successful-query work measurements require
an engine-facing instrumented benchmark, separate from this CLI driver.

Driver validation:

```sh
python3 -m unittest discover -s scripts/tests \
  -p 'test_benchmark_quill_query_fuel.py' -v
```

The tests use small child processes, a fake CLI, and the reported **log** shape.
They test process-group cleanup (including descendants holding pipes), output
limits, closed-pipe deadlines, spawn failures, JSON validation, percentile
accounting, environment isolation, and non-overwritten receipts. They are not
Rust compilation, Quill correctness tests, or corpus-scale benchmarks.

## Runtime acceptance work still required

A runtime correction must cover the primary, overfetch-retry, and federated
paths under a shared request budget. Keep a legitimate zero-hit result
successful. Catch only an appropriate resource refusal; do not hide corrupt
indexes, parser/invariant errors, or storage failures behind empty results.
Use a supported fallback only when its semantics and **total execution cost**
are bounded. Preserve title/content semantics, metadata filters, provenance,
deduplication, pagination, and coherent archive reads. A row-page cap or SQL
`LIMIT` after ranking is not an instruction budget or a deadline.

Report the actual serving backend and degradation reason per request, including
cache-hit cases, and never store fallback results under a misleading Quill-only
generation. Failed or unavailable fallback must retain the original refusal and
an actionable rebuild command with the effective archive paths. Optional counts
and repeated retrievals must not silently create fresh unlimited retry budgets.

Native regression fixtures must assert their **actual** segment counts (including
206), keep fragmentation from being merged away by the test setup, and exercise
both continuous commits and reopened sparse writer sessions. Include both
reported queries, no-match controls, supported/unsupported fallback queries,
forced low-fuel refusal, unavailable/corrupt fallback, filters, pagination,
deletions, and post-rebuild result identity. Benchmark small and 206-segment
fixtures with the same logical documents, separate opening from warm execution,
and record work counters alongside latency. The native qualification above
covers only the functional cases it lists; the remaining benchmark and recovery
matrix is not established by that receipt.
