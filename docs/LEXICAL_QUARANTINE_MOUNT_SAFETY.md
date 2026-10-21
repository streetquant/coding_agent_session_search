# Linux mount boundaries during lexical quarantine retention

A same-filesystem bind mount can expose unrelated data with the same device
number and ordinary link counts. A recursive removal that refuses symlinks is
not, by itself, a refusal to traverse mount points.

The lexical quarantine inventory now checks `/proc/thread-self/mountinfo`
before traversing each bundle on Linux. A mount at the bundle itself, at a
subdirectory, or at a regular file makes that observation incomplete. The
existing incomplete-inventory rule then permits no reclamation. Mounts above
the bundle, and sibling mounts, are not disqualifying: an index may live on its
own filesystem. Mount points are compared by path components, not string prefix.

Both automatic startup retention and approved offline cleanup re-read this
boundary after target/survivor checks, before the rename, and again immediately
before recursive removal. A refusal after the rename leaves the recognized
`.lexical-rebuild-reclaim-*/quarantine` reservation intact. Inspect again and
obtain a new cleanup preview after resolving the mount; do not replay an old
approval after a partially executed cleanup.

The parser preserves non-UTF-8 path bytes and decodes the kernel's space, tab,
newline and backslash escapes. Unknown optional mountinfo fields are ignored.
Missing, unreadable, malformed, empty, truncated or over-budget mount evidence
is a refusal, never evidence that no mounts exist. No mount point is opened by
the parser, and it neither mounts nor unmounts anything.

The mountinfo root field is either an absolute path or, for a pseudo filesystem
that has no path root, a `<name>:[<inode>]` token. A container host publishes
the token form for its network-namespace bind mounts, so both forms are
accepted; every other shape is still a malformed table and is refused as
such. The root field is not part of the boundary decision: only the mount
point is compared against the bundle, so accepting the token form does not
weaken that comparison.

Each snapshot is limited to 1 MiB, and all snapshots in one inventory/recheck
budget share 16 MiB of read work, plus at most one overflow-detection byte. The
existing cooperative deadline also applies. File reads and recursive removal
are synchronous; this is not a hard wall-clock or total-process-memory bound.
A Linux installation without readable proc mountinfo cannot authorize this
cleanup. Existing non-Linux checks are unchanged; this is not a new cross-platform
mount-point guarantee.

Mount namespace and ancestor directories remain operator-controlled. These
checks detect observed mount boundaries and ordinary changes between stages;
they do not atomically exclude a privileged process changing mounts during a
filesystem call. Do not change mounts while inventory or cleanup is running.

Regression tests cover same-device root/directory/file mounts, sibling and
ancestor boundaries, escaped/non-UTF-8 paths, malformed tables, read and work
budgets, pre-walk refusal, real read-only Linux mountinfo inspection, refusals
before/after the rename, interrupted cleanup retry, and preserved newest
failure evidence. Fault-injected tests do not create actual bind mounts. The
pseudo-filesystem root token is covered too: a table carrying it parses, the
same token with a mount point inside the bundle is still refused, and a root
that is neither an absolute path nor a well-formed token stays malformed.

Native qualification commands (not claimed executed successfully here):

```sh
cargo test --locked --lib lexical_quarantine
cargo test --locked --lib staging_reclaim
cargo test --locked --test cli_lexical_quarantine
cargo test --locked --test cli_lexical_quarantine_cleanup
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```
