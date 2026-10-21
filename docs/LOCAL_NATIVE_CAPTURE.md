# Local Max native capture

The OpenCode2 launcher selects the original database at
`/warp/codex/runtime/opencode2-runtime/data/opencode/opencode2.db`.
The CASS default under `~/.local/share/opencode` is a different store. The
reviewed Max profile in
`config/systemd/cass-incremental.service.d/40-opencode2-native-source.conf`
wires the existing `CASS_OPENCODE2_DB` connector option into the native
`cass-incremental.service`. It does not change the launcher or its database.

Before installing the profile, qualify the exact CASS executable against the
original OpenCode2 database in an isolated archive. Require matching native
session IDs, message counts, workspace bindings and content provenance, then
repeat ingestion and verify that no duplicate identities or messages appear.
An executable that recognizes the `opencode` agent can still lack the
`session_v2` parser; configuration alone does not establish capture support.

Install the profile as a private file under
`~/.config/systemd/user/cass-incremental.service.d/`. Preserve any existing file
and its hash before replacement. Reload the user manager only after the native
writer is idle; keep its timer, other drop-ins and executable unchanged.
Qualify bounded ingestion of this original database and inspect the next
scheduled process environment. A matching database checkpoint and exact
session readback establish current capture; a configured unit alone does not.

Rollback restores the prior configuration and retained immutable executable
only when the installed bytes still match the release being rolled back.
Preserve newer native archive rows and source files. Rehearse the old executable
against a disposable copy containing the newly captured records before
switching the shared executable pointer.

Codex provider launchers (`codex-hermes`, `codex-openrouter`, `codex-opencode`,
`codex-agnes` and `codex-kilo`) retain native Codex UUIDs and provider metadata.
CASS stores them as `agent=codex`, `source_id=local`. Standalone OpenCode2,
AGY, Kilo and Cline retain their separate native stores and connector identities.
CloudMCP tool observations remain partial unless actual client messages and
truthful completion evidence are supplied. OpenWebUI snapshots require their
original catalog, instance and conversation provenance. CASS capture and EE
evidence import each need readback; reusable EE memory requires review.

CloudMCP publications contain adjacent `manifest.json` and `transcript.jsonl`
files with the `cloudmcp.conversation-manifest.v1` and
`cloudmcp.conversation-event.v1` schemas. The manifest binds the original
conversation and workspace to the transcript's exact SHA-256 and event count.
Events retain their original source event, client session, agent, timestamp,
role and content hashes. Both original files remain available through the raw
mirror even when a publication is malformed or its schema is unsupported.

Use all four native identity predicates for bounded readback:
`cass sessions --exact-workspace ROOT --exact-source-id SOURCE --exact-agent AGENT --exact-external-id ID --json`.
Repeat `--exact-external-id` to select several original sessions. Preserve the
returned `path` and numeric `conversation_id` together when calling `cass view`;
a source file can contain several conversations. The row's `content_hash`
identifies normalized archive text. For CloudMCP, the exact original transcript
hash is `coverage_provenance.transcript_sha256`; these hashes describe different
bytes. Retain `all_transcript_complete=false` and its coverage source whenever
the original producer provides only partial observations.

Native EE ingestion uses the same exact source, agent and external identity
selectors. A repeated import must skip unchanged evidence; a newer append must
retain the EE session identity and the original source provenance. Unsupported
or uncertain evidence remains explicitly quarantined or unavailable. Review,
validation and application precede retrieval as reusable memory by another
agent; importing raw conversation evidence alone does not satisfy that gate.
