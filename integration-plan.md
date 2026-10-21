# Integration Plan: Update CASS from pinned upstream main, preserve native-history changes, and finish remaining existing BR implementation tasks

Status: `IN_PROGRESS` (upstream intake; implementation and acceptance incomplete)
Created: `2026-10-20T19:13:08+00:00`
Repository: `/warp/projects/softwares/cass`
Baseline branch: `main`
Baseline commit: `3254b9701105ccd3808bfa2c424cedc10e597a0e`
Reference: `https://github.com/Dicklesworthstone/coding_agent_session_search/tree/main`

## Current execution checkpoint

- User goal remains: update CASS from upstream and finish all remaining implementation; this plan does not narrow that goal.
- BR authority: physical `/warp/projects/softwares/cass/.beads`. Resume parent `coding_agent_session_search-2l1b0`; upstream integration child `coding_agent_session_search-2l1b0.33`, both claimed by `ChartreuseElk`.
- Pinned upstream: `cc7bdf13dceca8995628aecbb552a1067f7b43de`, verified through the GitHub commit API. Compare API reports 1,647 upstream-only and 52 local-only commits, common ancestor `26e24d90788c5836a9d0d18fa6701ccd28766c0b`. Its 300-file list is capped and cannot prove the full change surface.
- Preserve pre-existing work: baseline patch and 29 changed/new source/test path hashes are retained under `.local-state/upstream-integration-01a15ab7/`. Do not stage pre-existing work under this session's commits. Native source archives and running capture/index services remain outside the implementation mutation scope.
- No deletions are authorized. Upstream recovery-file removals must remain present locally; do not run a merge that deletes them from disk.
- The original large upstream fetch ended with early EOF. The complete pinned reference tree was verified separately, and `upstream/main` now resolves to the exact pinned commit through a local native Git fetch. No checkout or destructive merge was performed.
- Native Agent Mail identity, pre-commit guard, and source/control reservations are installed. Conflict-free upstream paths were adopted through a checked native patch; local preservation cohorts were resolved manually.
- Native BR/BV queries succeed. The integration helper incorrectly treats two different hash fields within each BV response as cross-call drift: the exact two-value sets match across plan and triage. Keep this helper report red; use live BR for authority and native BV for advisory ordering.
- Bounded CASS retrieval has same-workspace CloudMCP observations with incomplete transcript coverage. EE resume has zero episodic evidence; no complete capture or reusable-memory claim is made.
- Baseline shell gate parser: `bash scripts/tests/dpfvr_ubs_gate_e2e.sh --batched-only`, 45 passed, zero failed; fixtures retained at `/tmp/cass-gate-parser.twTBZb`. This is parser evidence only.
- RCH is unavailable. The user explicitly approved the local batched gate with bounded build parallelism. The installed `nightly-2026-08-31` toolchain and four jobs are used with a task-local target because `/data/tmp` is unavailable on this host.
- Diagnostic gate r1 compiled production CASS and reached CLI integration tests. Its initial formatting, Clippy, and lib-test compilation failures exposed integration mismatches that have been repaired. It was deliberately terminated when one-session fixtures discovered shared native stores outside HOME. Gate and affected fixture subprocesses now override both CloudMCP and OpenWebUI roots with isolated paths. No source from that accidental discovery is used as acceptance evidence. A fresh gate is required.
- Added actual boundary regressions for structured credential redaction and semantic-budget admission/dispatch. Preserved native snapshot ingestion and checkpoint behavior while adapting upstream APIs. No installer, startup configuration, credentials, or live archive was touched.
- Diagnostic r3 ran the broad library suite: 8,852 passed, one residual procfs assertion failed, 55 ignored. Diagnostic r4 passed all-target Clippy, CLI331, bookmarks9, metamorphic11, search-service173, CloudMCP17, native connector/history suites, FrankenSQLite38, credential E2E68, targeted indexing3 and bin228. All 52 JSON goldens pass by direct execution of the compiled binary; docs had one reviewed budget-description mismatch, now aligned. UBS remains pending.
- R4 exposed stale expectations for procfs refresh counts, Continue parser availability, delayed Devin WAL commits, unknown-agent validation and service-worker cache internals. Those tests now assert the actual safety contracts. The gate PATH had selected Node20 rather than installed Node26; final PATH selects the latter, and the Node SQLite prerequisite is declared in developer docs and Rust CI configuration.
- The pinned UBS package is retained in the ignored task directory. R5 copies its verified release modules into a fresh private scan directory and still verifies every module/helper hash; no scanner checks are disabled. The Rust module receives a 1,800-second limit for the large upstream change surface. The updated gate parser has 61 passing cases.
- UBS r4 was partial: the Rust module timed out after 300 seconds; the other 92 files reported 49 critical and 950 warning instances, including duplicated heuristic findings and intentional test assertions, which still require review. The shell directory-change failure and scalar/array variable-name collision were corrected directly. No security gate or source-stability acceptance is claimed.
- R5 passed fmt, all-target Clippy and every selected functional stage, including credential E2E68, fail-open80, Pages64, search-service173, bin228 and JSON goldens52. UBS completed red: 749 critical and 89,900 warning heuristic instances over653 files. Source-stability failed after an explicitly recorded lift of the freeze to fix the reviewed source. These are diagnostic results, not acceptance or confirmed vulnerability counts.
- R6 passed271 expanded library tests with2 ignored, including the manifest identity and owner admission controls. All-target Clippy found one live integration fixture missing the newly added optional field; corrected it, then upgraded that fixture to FSVIv2 quality-only fresh-process owner search. Stopped only the obsolete r6 gate process tree after useful diagnostics; no final receipt or all-green claim.
- Python collector/query-fuel45 tests and benchmark/IPC47 tests passed. The latter include forced nested-child cleanup and a real nsfs mount-root parser regression. The parser retains the index filesystem allowlist and exact mount identity checks. Pages now aborts stale initialization fetches/body reads and clears accumulated, late and terminal plaintext chunks; final tests remain required.
- Verified the accepted upstream embedding identity, FSVIv2, read-only owner and native ANN landing SHAs as ancestors of the pinned Frankensearch release. The historical external API blocker is resolved. Selected publication now uses the manifest's complete FSVI generation identity directly, without a preliminary ordinary file read; ANN receipts bind that same generation. The witness task is resumed under its existing ID.
- R7 completed source-stable: fmt, all-target Clippy and every selected functional stage passed, including library271, Pages65, fresh-process semantic4, bin228 and goldens68. UBS alone failed with736 critical/89,889 warning/31,514 info instances over653 files. The complete native Rust sink is retained at `.local-state/upstream-integration-01a15ab7/ubs-rust-r7-findings.ndjson` (68,632,600 bytes, SHA2569507e1869a7dd6941200945f313e48c9fe1645c12ffd04791640b328c2a428e6); no rescan is needed to inspect unseen findings.
- Actual pinned single-file canaries refined the suppression diagnosis: JSON-parser scoped annotations work, but `python.lifecycle.popen_handle` remains emitted for exact, unrelated and absent annotations. Bounded Rust critical examples did not demonstrate invalid FFI initialization or SQL/shell injection; unreviewed instances remain unadjudicated and the gate stays blocking.
- Corrected the Python admission harness test to require its bounded operation error and retained decoder cause. The coarse semantic disk resolver now requires an explicit optional caller-pinned source; missing witness reports SourceUnknown/VerifySource after auditing artifacts, a matching witness reports Ready, stale source still errors, and corruption is never hidden as unknown readiness. New real-FSVIv2 fixture controls cover each branch. This does not construct the production corpus witness or admit a serving owner.
- R8 is diagnostic: Clippy caught two new fixture calls missing the pointer timestamp/epoch and expected-prior arguments. Corrected them to the actual CAS API and stopped only the obsolete gate process tree. No final acceptance is inferred from R8.
- R9 completed source-stable: fmt, all-target Clippy, semantic library221, integration5, bin228 and goldens68 passed. UBS alone failed with736 critical/89,911 warning/31,522 info instances. Its complete Rust sink is retained at `ubs-rust-r9-findings.ndjson` (68,649,801 bytes, SHA2565152cf210fbb01ff76d209d213b83a3572b5026d7f682144aa8ffed0fc66aae0). R7 review now classifies all549 panic sites as test-only, all98 comparison sites as tests or public/state/integrity checks, and50 secret/random/shell/SQL sites without a demonstrated production defect. Three bounded FFI examples were also reviewed. This does not waive findings or turn UBS green.
- The full artifact auditor used an ordinary hash read that could change vector access time. On Linux/Android its existing open now requires O_NOATIME/O_NOFOLLOW/O_CLOEXEC and never retries with weaker flags. Added a real FSVIv2 test with an ordinary-read control and metadata checks after sealing, pointer publication, full load and readiness audit. This is a bounded filesystem fix; production corpus-witness construction, retained-owner migration, producer/search/daemon wiring, role-specific ANN descriptors and UBS adjudication remain open.
- R10 was stopped as a diagnostic after pre-flight formatting differences were caught; its partial receipt is not acceptance evidence. The source was formatted before continuing.
- R11 was stopped as a diagnostic after review of Serde's internally tagged unit visitor showed it ignores extra fields. ANN now uses an empty struct variant, whose strict deserializer rejects forbidden WAL keys. Only this run's captured descendants were stopped; its receipt is not acceptance evidence.
- R12 completed source-stable at content SHA `e8395df9d8977c413cd125b6581e622ed05240c3d6cc57114aabdc2771d8c91d`: source/freshness/fmt/all-target Clippy, semantic library222, manifest integration6, native ANN integration16, binary228 and goldens68 all passed. ANN descriptors/evidence use a strict ANN seal without vector WAL fields; vector seals retain explicit zero-WAL evidence. JSON forbidden-field, role/evidence and canonical-tag controls passed. UBS alone failed with4 critical/1209 warning/471 info instances over the five changed files; unchanged full R9 findings remain blocking. The four scoped criticals are three test panic sites and the previously reviewed success-guarded Windows FFI output initialization; no blanket suppression follows.
- The workspace is mounted `noatime`, and the timestamp test explicitly reports its positive control unexercised there. Running the exact R12 integration binary against an isolated self-created tmpfs fixture exercised ordinary-read atime updates and passed all four metadata-preservation paths. Evidence: `gate-local-r12-atime-tmpfs-control.log`. No live archive or mount configuration changed.
- Next: retain UBS as blocking, finish the production corpus witness and selected reader/producer migration under their existing tasks, and complete the strict integration evidence. Scoped R12 is functional acceptance of the bounded filesystem/seal corrections, not a clean full integration gate or all-work-complete result.
- R13 scope: the existing rebuild diagnostic now checks current-generation authority before conventional-file discovery and uses the read-only legacy opener. Focused controls cover absent/compatible/old-revision assets, malformed and dangling current pointers, and preservation of artifact/journal bytes, modification time and directory entries. The batched gate retains all-target Clippy, focused diagnostic/selection tests, manifest/native ANN integrations, binary/goldens and UBS on `model_manager.rs`; prior R12 and full R9 scanner findings remain blocking.
- R13 completed source-stable at SHA `45493977978a5c239db3a5416fd7f45ab5f2462adf6df7454f687f65aa1b68ed`: focused library5, manifest integration6, native ANN16, binary228, goldens68, formatting and all-target Clippy passed. UBS alone failed with6 critical/186 warning/93 info over `model_manager.rs`; earlier complete scanner failures remain blocking. Receipt: `gate-local-r13.receipt`.
- R14 scope: full audits of FSVI v2 now use upstream immutable owned-byte admission and the same descriptor-to-owner checks as the retained reader, rather than accepting a producer's hash-consistent structural claims. Upstream typed reindex/upgrade/snapshot/corruption causes are retained behind bounded public diagnostics. One image is admitted at a time with a 512 MiB declared-image ceiling checked before opening; larger images require sharding, and this is not an RSS guarantee. Real writer fixtures challenge identity, generation, count, docset and invalid-image claims with self-consistent receipts and refreshed hashes across seal/publication/load/readiness. Legacy diagnostic formats, whole-tier sharded membership, production corpus witnesses and producer/serving wiring remain separate unfinished work. R14 verification pending; no task closure or scanner waiver.
- R14 completed source-stable at SHA `e344ca61c70c3dc2ce687881d0c74d792d7ed28a53bf5484e65052122f25d07d`: formatting, all-target Clippy, library189, manifest integration8, native ANN16, binary228 and goldens68 passed. UBS alone failed with4 critical/1204 warning/454 info over4 files; full R9 remains blocking. The exact compiled access-time regression also passed on isolated owned tmpfs with its ordinary-read control exercised. Receipts: `gate-local-r14.receipt` and `gate-local-r14-atime-tmpfs-control.log`.
- R15 scope: disk readiness now requires supported FSVI v2 descriptors even after a successful hash audit. Known v1 vectors remain `LegacyUnidentified` with reindex recovery; unknown formats receive a bounded typed upgrade error. Real-writer downgrade controls cover single and mixed tiers, caller-source presence/absence, robot projections, and preservation of stale-source/corruption precedence. Legacy inspection remains diagnostic; production corpus, producer and serving migration remain unfinished. Verification pending; no scanner waiver or closure.
- R15 completed source-stable at SHA `5a370e0f11b64a1514cf681ec231115018342cd9769cabbe56a8e38bd8905437`: formatting, all-target Clippy, library189, manifest integration9, native ANN16, binary228 and goldens68 passed. UBS alone failed with3 critical/968 warning/405 info over2 files; prior full findings remain blocking. Receipt: `gate-local-r15.receipt`.
- R16 scope: rejected build records now propagate without deleting the destination that may still contain the prior live image; the public writer buffers records until finish. Regressions challenge invalid first/second records (NaN, infinity, wrong/empty dimension and a writer-rejected zero vector) with existing and absent destinations, preserving image/journal bytes, modification times and directory entries. Watch-once validation, inventory and already-covered counting use read-only opens; shared-reader and short/torn-journal controls cover inspection without repair. This does not claim access-time preservation for legacy mappings, complete blue-green publication, or production FSVI v2 migration. Verification pending; prior scanner findings remain blocking.
- R16 completed source-stable at SHA `44323089078dad2b3e306453e4ef74e80dc81eee37ee92f3cde505537528a660`: formatting, all-target Clippy, library15, manifest9, native ANN16, binary228 and goldens68 passed. The real CLI integration had61 passing tests and one setup timeout: its environment clearing removed native-store isolation overrides, so it discovered shared conversations outside HOME. The child was stopped by its180-second timeout; that accidental discovery is excluded from acceptance. UBS also failed. Receipt: `gate-local-r16.receipt`. The current fixture now explicitly sets both CloudMCP/OpenWebUI roots below its own temporary home, including after environment clearing.
- R17 scope: sealing and pointer publication independently reject vector formats that cannot undergo supported FSVI v2 admission, after existing integrity/path checks. Legacy v1 remains diagnostic-only with reindex recovery; unknown formats require an upgrade. Successful filesystem publication tests now use actual public-writer images and owner-derived document-set identities. The single/mixed-tier downgrade matrix challenges both seal and independent pointer publication before injecting historical bytes solely for read-only recovery checks. Verification will also rerun the repaired CLI migration fixture. No serving/corpus migration, scanner waiver or closure claim.
- R17 completed source-stable at SHA `690a80c10538cb8c0d92cdfdad5bdd9d511b81fe3d2625f1d5e37fbbe4150702`: formatting, all-target Clippy, library261, manifest9, native ANN16, real CLI62, binary228 and goldens68 passed. The repaired CLI fixture finished in3.24seconds with exactly its seeded conversation. UBS remains red:3 critical,1034 warnings,420 informational findings over three files; the prior complete R9 findings remain blocking. Receipt: `gate-local-r17.receipt`. No deployment, task closure or scanner waiver.
- R18 scope: strict audits parse the bounded adjacent native ANN receipt through the public engine API and compare every FSVI owner identity/content/generation fingerprint plus selected manifest graph/receipt bindings. Receipt reads require no-atime flags on Linux/Android, reject links and oversized sidecars, retain typed parser causes and sync valid receipts when sealing. The existing public native graph loader uses ordinary file reads, so it cannot be reused for timestamp-preserving structural audits; structural graph parsing remains a serving check and the upstream API limitation remains open. Added actual graph-writer sidecar rejection and access-time controls; no wire parser duplication or full graph-admission claim.
- R18 completed source-stable at SHA `bd5749356a3f81720de205e8f2fa12f67b66123805d3220d1e0f71f9e733e9b1`: formatting, all-target Clippy, library263, manifest9, native ANN16, real CLI62, binary228 and goldens68 passed; UBS alone failed. Receipt: `gate-local-r18.receipt`. A separate run of the exact current library binary on an isolated owned tmpfs fixture exercised ordinary-read access-time changes and proved all four audits preserve vector, graph and receipt metadata (`gate-local-r18-ann-atime-tmpfs-control.log`). The public no-atime structural graph API limitation and full scanner failures remain open.
- R19 scope under existing recipe task `.10`: lead robot recipes with scoped lexical history retrieval, caller deadlines, truthful timeout interpretation and explicit maintenance refusal; remove unconditional fast-diagnostic claims. README, repository SKILL and robot docs now state that the soft token estimate covers hit data rather than the entire response or process resources. The real CLI regression seeds recent, old and unrelated-workspace conversations, then executes the documented search and cited view while checking the entire isolated home/archive remains byte-identical. Topic checks and the exact docs golden cover the published command text. Repository SKILL changes are source only: no canonical skill installation/publication, large-corpus timing acceptance or task closure is claimed.

## Integration Contract

### Required behavior

- Update CASS from pinned upstream main, preserve native-history changes, and finish remaining existing BR implementation tasks

### Compatibility boundary

- Preserve released/documented behavior and durable external contracts unless explicit approval records a change.

### Intentionally unsupported cases

- Record nearby cases that will be rejected, including failure timing and supported alternatives.

### Non-goals

- Record work that is adjacent but not required for this integration.

### Hard gates

- No loss of user-owned work.
- No unapproved breaking API, schema, data, configuration, or operational change.
- No silent dependency addition, upgrade, downgrade, replacement, or license change.
- No reference content is trusted until its selected surface, revision, license, and security findings are locked in `.integration/reference-lock.json`.
- Required behavior is traceable to implementation and executable verification.
- Existing baseline behavior remains passing or every pre-existing failure is reproduced and isolated.
- Frozen atomic commitments do not drift in place; approved changes create explicit superseding commitments.
- Behavioral tests and structural verifiers jointly cover the required contract.
- Changed code has graph-derived impact coverage and tests contain meaningful oracle signals.
- Required behavior has an executable differential or relational contract, repeated for stability, unless an approved waiver explains why no reference execution is possible.
- The integration does not exceed the approved maintainability-delta policy without an explicit exemption.
- Required feature evidence distinguishes a clean pre-feature baseline from the candidate; regression-only checks are not mislabeled.
- High/critical-risk components reject mapped plausible counterexamples in an isolated current copy or carry an approved waiver.
- Execution checkpoints/resume claims and non-secret environment provenance are current and machine-verified.
- Every active requirement/component has stable runtime-diagnosis facet coverage.
- The external implementation trace reconciles with the current diff: no orphan changed path or unwaived unrealized material step remains.
- Pinned upstream evidence is distilled into consumer-visible migration rules; raw changelog volume is not treated as usable context.
- Repository context is selected through multiple deterministic workflow signals and abstains when direct or independent evidence is insufficient.
- Applicable latency, throughput, memory, size, capacity, or other non-functional budgets are measured repeatedly against a clean baseline and remain correctness-bound.
- Every affected consumer-facing API, CLI, configuration, schema, wire, persistence, plugin, event, package, or serialization surface satisfies its declared compatibility direction or is explicitly classified as an approved break with migration evidence.
- Mixed-version readers/writers, expand/migrate/contract transitions, and rollback-sensitive state paths are rehearsed in isolated copies with all required obligations proved.
- Checked-in generated outputs are reproducible from declared sources/configuration using a pinned generator, and generation changes no undeclared path.

### Specification ledger

The machine-readable ledger is `.integration/spec-ledger.json`; its frozen checkpoint is `.integration/spec-baseline.json`.

| Commitment | Source | Components | Acceptance criteria | Final verifiers | Stability/status |
|---|---|---|---|---|---|
| SPEC-001 | https://github.com/Dicklesworthstone/coding_agent_session_search/tree/main | INT-001 | AC-001 | final-focused | provisional/active |

## Baseline

| Item | Evidence | Result |
|---|---|---|
| Repository instructions read | | |
| Git state and user changes recorded | | |
| Baseline revision pinned | `3254b9701105ccd3808bfa2c424cedc10e597a0e` | |
| Baseline build/test commands identified | | |
| Baseline verification executed | | |
| Existing failures reproduced and classified | | |
| Reference revision and license pinned | | |

## Implicit Requirement Recovery

Capture `.integration/requirement-recovery-baseline.json` before editing and verify
`.integration/requirement-recovery-report.json` after implementation. Review every active component
across functional, business, technical, interface/protocol, structural-convention, and data-structure
categories. Every adopted requirement must cite exact repository-local lines and link to the feature
map plus frozen specification. Rejected high/critical findings require explicit approval; deferred
findings block completion.

| Finding | Category | Component | Evidence path/lines | Disposition | Specification | Approval |
|---|---|---|---|---|---|---|
| IMPL-REQ-001 | functional | INT-001 | | adopted | SPEC-001 | |

## Architecture and Context

### Ontology

- Objects: requirements, reference behaviors, modules, symbols, interfaces, dependencies, configuration, schemas, tests, risks, evidence, BR work items.
- Relations: `maps_to`, `depends_on`, `imports`, `calls`, `configures`, `persists`, `verifies`, `rolls_back`, `evidences`.
- Transformations: reference behavior → target-native design → incremental changes → executable evidence.

### Target architecture

Record module responsibilities, source-of-truth interfaces, extension mechanisms, and forbidden bypasses.

Generate `.integration/architecture-map.json` from deterministic repository evidence. Record its map hash, graph coverage, truncation status, import cycles, source/test edges, and any unsupported-language gaps.

### Bounded context evidence

| Context hash | Seeds/queries | Files and line ranges | Why sufficient | Gaps |
|---|---|---|---|---|
| | | | | |

### Impact closure

Record direct targets plus affected callers, callees, tests, configuration, schemas, generated artifacts, and runtime boundaries. Every included path needs evidence; every excluded high-risk neighbor needs a reason.

The machine-readable impact plan is `.integration/impact-plan.json`. Its architecture-map SHA-256, feature-map SHA-256, HEAD, and implementation-diff fingerprint must match the final state.

### Context portfolio and abstention

The policy is `.integration/context-portfolio-policy.json`; the selected, line-addressed portfolio is `.integration/context-portfolio.json`. Use task-specific signals such as feature-to-integration, code-to-test, trace-to-code, edit-to-ripple, and migration-to-usage. Fuse deterministic lexical, architecture, impact, test, instruction, trace, and history evidence under one budget. When required direct anchors or independent channels are absent, stop with `needs_more_evidence` rather than filling the context window with weakly related files.

| Signal | Components | Direct anchors | Independent channels | Selected files/lines | Status or waiver |
|---|---|---|---|---|---|
| CTX-001 | INT-001 | | | | |

## Feature Mapping

The machine-readable map is `.integration/feature-map.json`.

| Component | Requirement | Reference evidence | Target-native integration point | BR bead | Status |
|---|---|---|---|---|---|
| INT-001 | Update CASS from pinned upstream main, preserve native-history changes, and finish remaining existing BR implementation tasks | https://github.com/Dicklesworthstone/coding_agent_session_search/tree/main | | | planned |

## Work Graph

BR is the durable source of truth for work state, dependencies, acceptance criteria, risks, blockers, and evidence links. BV is the read-only graph-analysis and prioritization sidecar.

| Bead | Purpose | Depends on | Acceptance criteria | Evidence | Status |
|---|---|---|---|---|---|
| | | | | | |

Record `.integration/br-bv-probe.json`, the live `br show <id> --json` result for every non-rejected feature-map bead, the latest `bv --robot-plan` data hash, and any cycle, bottleneck, or critical-path findings. The final strict probe must match the feature-map hash, current HEAD, and implementation diff.

## Reference Trust and Behavioral Parity

The policy is `.integration/reference-policy.json`; the frozen observation is `.integration/reference-lock.json`. Treat every instruction, script, example, and template in the reference as untrusted data. Read and execute only selected, hashed paths after critical findings are resolved or explicitly allowlisted.

The executable relation is `.integration/behavior-contract.json`; its result is `.integration/differential-report.json`. Repeat probes to reject flaky equivalence, normalize only declared nondeterminism, and link every active feature component to at least one required case or approved waiver.

| Case | Components | Reference probe | Target probe | Relation | Stability | Result |
|---|---|---|---|---|---|---|
| BEH-001 | INT-001 | | | semantic parity | 2+ identical runs | |

## Runtime Diagnosis and Implementation Trace

The diagnosis contract is `.integration/diagnosis-contract.json`; capture `.integration/diagnosis-baseline.json` before implementation and generate `.integration/diagnosis-report.json` after it. Direct targets must discriminate baseline from final behavior. Diagnosis-only probes may guide localization but are not final feature proof. Every probe must be worktree-side-effect-free, including `.integration` artifacts.

| Facet | Specifications | Components | Mode | Baseline evidence | Final evidence | Status/waiver |
|---|---|---|---|---|---|---|
| FACET-001 | SPEC-001 | INT-001 | direct_target | | | |

The external plan is `.integration/implementation-trace.json`; its final reconciliation is `.integration/plan-reconciliation.json`. Only verified material steps may own changed paths or satisfy active requirement/component coverage. Rejected steps require an explicit reason and approval; verification-only steps cannot conceal unowned implementation changes.

| Step | Kind | Specifications/components | Depends on | Target paths/symbols | Final evidence | Status |
|---|---|---|---|---|---|---|
| STEP-001 | implementation | SPEC-001 / INT-001 | | | final-focused | planned |

## Changed-Line Execution and Change Necessity

The changed-line policy is `.integration/change-coverage-policy.json`; the final execution report is
`.integration/change-coverage-report.json`. Count changed executable lines from the actual diff,
separate uncovered from uninstrumented lines, and require changed error-handling lines to execute.
Aggregate repository coverage or a missing report entry does not satisfy this gate.

The necessity policy is `.integration/change-necessity-policy.json`; the final isolated ablation report
is `.integration/change-necessity-report.json`. Classify each component as `no-change-required`,
`change-required`, `partial-change-required`, or `indeterminate` from diagnosis facets. Group only exact
behavioral implementation paths into cohorts. The candidate must pass required final commands and the
same commands must fail with the declared feature-specific signature after restoring that cohort from
the clean baseline.

| Evidence | Components | Changed paths/facets | Candidate | Ablated/baseline | Result |
|---|---|---|---|---|---|
| CHCOV-001 | INT-001 | | | | |
| NEC-001 | INT-001 | | | | |

## Upstream Change Evidence and Consumer Migration

The policy is `.integration/upstream-evidence-policy.json`. Before implementation, write the hash-bound consumer mapping to `.integration/upstream-evidence-mapping.json`; after implementation, write the transition check to `.integration/upstream-evidence-report.json`. Every source excerpt must come from the selected paths in the pinned reference lock and use bounded line ranges. Classify each change as a direct rename, import migration, compound API migration, paradigm shift, configuration, protocol, behavioral, build/toolchain, or mixed change. Locate exact legacy consumer anchors before editing, then require the declared legacy/new-surface count transition in the final state.

| Migration | Pinned evidence | Classification | Consumer anchors | Coordinated rules | Component coverage |
|---|---|---|---|---|---|
| MIG-001 | UP-001 | | | RULE-001 | INT-001 |

## Consumer Contracts, State Evolution, and Generated Artifacts

| ID | Type | Components | Direction/obligations | Baseline/source | Candidate/output | Native checks/provenance | Status |
|---|---|---|---|---|---|---|---|
| CONTRACT-001 | public API/CLI/config/schema/wire/etc. | INT-001 | backward/forward/full/additive/intentional break | | | | planned |
| STATE-001 | mixed-version/state evolution | INT-001 | expand, old/new coexistence, rollback | | | | planned |
| GEN-001 | generated artifact set | INT-001 | deterministic regeneration | | | generator version + lock/source | planned |

Record contract snapshots and repository-native compatibility checks in
`.integration/contract-policy.json`. Record expand/mixed-version/migrate/contract/rollback
scenarios in `.integration/state-evolution-policy.json`. Record generated source, configuration,
output roots, generator command, version probe, and pinning evidence in
`.integration/generated-artifact-policy.json`.

## Dependency, License, and Supply-Chain Review

| Dependency/reference | Existing version | Proposed change | Alternatives | License/provenance | Security/platform impact | Decision |
|---|---|---|---|---|---|---|
| | | | | | | |

## Verification Matrix

| Requirement or invariant | Test level | Required final command | Fails before change? | Passes after change? | Negative/edge case | Evidence record/hash |
|---|---|---|---|---|---|---|
| Update CASS from pinned upstream main, preserve native-history changes, and finish remaining existing BR implementation tasks | | | | | | |
| Existing behavior preserved | regression | | | | | |

### Validation contrast

Record `.integration/validation-contrast.json`. For every required feature case, record baseline result, candidate result, repetition stability, and role. A case that passes on both states is regression-only.

| Case | Components | Baseline result | Candidate result | Role | Stable | Evidence hash |
|---|---|---|---|---|---|---|
| VC-001 | INT-001 | | | feature_discriminating | | |

### Counterexample rejection

Record `.integration/counterexample-report.json`. Apply plausible non-equivalent faults only in an attested isolated current copy; require assigned commands to kill them and verify exact restoration.

| Counterexample | Components | Risk/fault | Assigned commands | Result | Restoration | Waiver |
|---|---|---|---|---|---|---|
| MUT-001 | INT-001 | | | | | |

### Oracle and fault-model review

Record `.integration/oracle-audit.json`. For every changed or graph-impacted test, identify the observable oracle, representative faults it rejects, negative/boundary cases, mock density, real-boundary evidence, and any approved reason a strong oracle is impossible. Prefer mutation testing when the repository already supports it; otherwise use targeted hand-written mutants or counterexamples without adding dependencies silently.

### Maintainability delta

Record `.integration/quality-report.json` against `.integration/quality-policy.json`. Review added logical lines, duplication, maximum function size, complexity growth, and structural erosion. Passing tests do not waive a failed maintainability gate.

### Non-functional regression budgets

The policy is `.integration/nonfunctional-policy.json`; the measured comparison is `.integration/nonfunctional-report.json`. Run checked-in harnesses repeatedly against a clean pre-feature baseline and the candidate, reject unstable samples and benchmark-side effects, and bind every metric to required final correctness commands. Raw speedup or size reduction is not acceptable when it bypasses the normal public path or weakens correctness.

| Metric | Components | Direction/budget | Baseline statistic | Candidate statistic | Stability | Correctness binding | Result |
|---|---|---|---|---|---|---|---|
| NF-001 | INT-001 | | | | | final-focused / final-full | |

## Execution State and Environment Provenance

The append-only ledger is `.integration/execution-ledger.jsonl`; the final compact state is `.integration/execution-state.json`. Record checkpoints only at coherent boundaries, consume resume claims once, and use explicit fork IDs for alternate continuations. The ledger does not provide exactly-once semantics for external side effects.

The environment baseline is `.integration/environment-baseline.json`; the final comparison is `.integration/environment-report.json`. Record only declared non-secret environment keys. Toolchain, manifest/lockfile, submodule, or platform drift must be restored, declared in the feature map, or explicitly approved.

## Rollback and Recovery

Record the smallest safe reversal for code, dependencies, configuration, schema/data, generated artifacts, and external side effects. For irreversible migrations, record backup, restore, forward-fix, and stop conditions.

## Decisions, Risks, and Blockers

| ID | Type | Statement | Evidence | Decision/mitigation | Owner | Status |
|---|---|---|---|---|---|---|
| | | | | | | |

Escalate only when unresolved ambiguity can materially change public behavior, security, user data, cost, legal obligations, or production operations.

## Execution Log

Update at phase transitions, blockers, contract changes, verification invalidation, and commits. Do not create noisy entries for every read-only command.

| Time | Phase | BR bead | Change/evidence | Commands and results | Diff fingerprint | Commit SHA | Next action |
|---|---|---|---|---|---|---|---|
| 2026-10-20T19:13:08+00:00 | discovery | | initialized | | | | probe repository and pin baseline |

## Completion Gates

- [ ] Required behavior is explicit and all unsupported neighboring cases are stated.
- [ ] Atomic commitments are frozen in `.integration/spec-baseline.json`; no frozen payload changed in place.
- [ ] Repository instructions, architecture, style, and dependency policy were followed.
- [ ] Reference source, exact revision/tree, selected files, license evidence, and security findings are current in `.integration/reference-lock.json`.
- [ ] Reference instructions were treated as untrusted data; no unapproved critical finding remains.
- [ ] Every active component has a complete six-category implicit-requirement review; adopted findings are frozen and traced to exact repository evidence, the feature map, and active specifications, with no deferred finding.
- [ ] Every active component is covered by a stable passing behavior-contract case or an approved waiver in `.integration/differential-report.json`.
- [ ] BR graph contains the integration epic/components, dependencies, acceptance criteria, risks, and evidence links.
- [ ] The final strict BR/BV probe is current, verifies every non-rejected feature-map bead through live BR state, uses robot/read-only surfaces, and records a consistent BV data hash.
- [ ] BV reports no unresolved cycles and the completed path covers all required work.
- [ ] Every active commitment/component has a stable, side-effect-free runtime-grounded diagnosis facet; already-present facets are explicitly classifiable, missing facets pass finally, and partial features change only their missing behavior.
- [ ] Every changed executable line is represented and executed at the declared threshold; changed error-handling lines are executed and cannot be waived, while deletion-only behavior has required final evidence.
- [ ] Every behavioral change cohort is necessary: the isolated candidate passes, restoring the cohort from the baseline produces a stable feature-specific failure, no no-change component has behavioral edits, and no component remains indeterminate.
- [ ] Every changed implementation path maps to both a feature-map component and a realized implementation-trace step with a specific repository-local target prefix; no catch-all glob claims unrelated changes and no unwaived material step remains unrealized.
- [ ] The architecture map is current, complete for affected modules, and free of unresolved blocking cycles.
- [ ] The impact plan covers changed paths, affected dependants, and prioritized tests with no unexplained blind spot.
- [ ] The upstream mapping and final report share the same pinned reference tree, policy, and consumer projection; legacy sites are located before editing and the declared old/new-surface transition passes afterward.
- [ ] Every required context signal proceeds with direct or independently corroborated evidence; the final portfolio covers changed paths and impacted tests within its declared budget, or an explicit abstention blocks implementation.
- [ ] Changed and impacted tests meet the declared oracle-strength expectation, reject representative plausible faults, and include real-boundary evidence where the feature map requires it.
- [ ] The quality-delta report passes or every threshold exemption has a recorded reason and approval.
- [ ] Every applicable non-functional metric is stable, within its baseline-relative budget, side-effect-free, linked to active components, and bound to required final correctness commands.
- [ ] Every declared consumer contract surface is stable, current, linked to its component, and satisfies its compatibility relation; every approved break is classified as `intentional-break` with migration evidence.
- [ ] Every required state-evolution scenario proves its mixed-version and rollback obligations in repeated isolated runs without undeclared side effects.
- [ ] Every declared generated artifact set reproduces the checked-in bytes from pinned sources/configuration and generator provenance, with deterministic repeated output and no changes outside declared output roots.
- [ ] Every required feature validation-contrast case is stable, fails on the clean baseline, passes on the candidate, and is not merely regression-only.
- [ ] Every mapped high/critical-risk counterexample is killed in an isolated current copy and the copy is restored exactly, or a narrow approved waiver records compensating evidence.
- [ ] The execution ledger hash chain verifies, the latest checkpoint is current, resume claims are unique, and `.integration/execution-state.json` binds its current head.
- [ ] The environment report matches the captured baseline except for feature-map-declared or explicitly approved drift.
- [ ] Every requirement maps to a required final command with passing evidence tied to the current diff, verification manifest, feature map, frozen specification, diagnosis baseline/report, implementation trace/reconciliation, architecture map, impact plan, oracle audit, and quality report.
- [ ] The final evidence-chain head is recorded in this plan and the BR verification item or another immutable review artifact.
- [ ] Focused tests, integration tests, regression tests, and applicable full verification pass.
- [ ] Dependency, security, license, migration, performance, and rollback checks are complete where applicable.
- [ ] Three adversarial review lenses have no unresolved blocking finding.
- [ ] `integration-plan.md`, `.integration/feature-map.json`, BR state, and evidence agree.
- [ ] Final diff is minimal, coherent, target-native, and free of unrelated changes.
- [ ] Final status is recorded as `COMPLETE`, `BLOCKED`, or `PARTIAL` with exact evidence.
