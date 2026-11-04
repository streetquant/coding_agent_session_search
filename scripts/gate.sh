#!/usr/bin/env bash
# gate.sh — the blocking quality gate, batched into ONE rch fleet admission.
#
# Why this exists (reality check 2026-09-01, WS-A.2): every GitHub workflow is
# disabled, so the only gate is agent-run. Fleet admissions are scarce, and an
# agent that spends one on `cargo check`, then another on clippy, then another
# on tests loses most of an hour to refusals — and the temptation to push
# unverified follows. This script runs exactly what CI will run, inside a
# single remote job, and prints one receipt line per stage:
#
#   STAGE=<name> EXIT=<code>
#
# The receipt is what a bead closure or a push cites. Nothing here weakens a
# gate: stages run with the same flags CI uses, and a stage failure never stops
# the later stages (you want the whole picture from one admission).
#
# Usage:
#   scripts/gate.sh                 # fmt, clippy, lib tests, targeted integration, goldens
#   scripts/gate.sh --lib-filter 'quill_bridge::tests health_watermark'   # narrow the lib run
#   scripts/gate.sh --no-lib        # skip the lib suite (e.g. while bet45 wedge is open)
#   scripts/gate.sh --no-bin        # skip the cass bin's own unit tests (stage bin-cass)
#   scripts/gate.sh --local         # run stages locally (only where rch is not required)
#   GATE_RETRIES=40 GATE_RETRY_SLEEP=90 scripts/gate.sh   # keep retrying fleet refusals
#
# Exit code: 0 when every stage exited 0, 1 otherwise, 103 if the fleet refused
# every attempt (RCH_REQUIRE_REMOTE=1 forbids local fallback).

set -uo pipefail

test_log_counts() {
    local log="$1"
    shift
    local filters_nl=""
    if [ "$#" -gt 0 ]; then
        printf -v filters_nl '%s\n' "$@"
    fi
    GATE_TEST_FILTERS="$filters_nl" awk '
        BEGIN {
            n = split(ENVIRON["GATE_TEST_FILTERS"], raw_filters, "\n")
            for (i = 1; i <= n; i++) {
                if (length(raw_filters[i]) > 0) {
                    filters[++filter_count] = raw_filters[i]
                }
            }
        }
        /^running [0-9]+ tests?$/ {
            announced[++depth] = $2
            next
        }
        depth == 1 && /^test [^ ].* \.\.\. ok$/ {
            line = $0
            sub(/^test /, "", line)
            sub(/ \.\.\. ok$/, "", line)
            sub(/ - should panic$/, "", line)
            for (f = 1; f <= filter_count; f++) {
                if (index(line, filters[f]) > 0) {
                    matched[f]++
                }
            }
            next
        }
        /^test result: ok\. [0-9]+ passed; [0-9]+ failed; [0-9]+ ignored; [0-9]+ measured; [0-9]+ filtered out/ {
            if (depth == 0) { incomplete++; next }
            if ($6 != 0 || $4 + $6 + $8 + $10 != announced[depth]) incomplete++
            if ($4 == 0) empty++
            delete announced[depth--]
            # Inherited subprocess output nests inside its parent test run.
            # Count each top-level binary once; every child must still finish.
            if (depth == 0) { binaries++; passed += $4 }
            next
        }
        /^test result:/ { incomplete++ }
        END {
            for (f = 1; f <= filter_count; f++) {
                if (!matched[f]) {
                    printf "gate: test filter matched 0 passing tests: %s\n", filters[f] > "/dev/stderr"
                    unmatched++
                }
            }
            if (binaries == 0 || empty > 0 || incomplete > 0 || depth != 0 || unmatched > 0) exit 1
            printf "PASSED=%d BINARIES=%d\n", passed, binaries
        }
    ' "$log"
}

verify_receipt() {
    local receipt="$1" remote_rc="$2"
    shift 2
    local failed=0 line name stage last_stage=""
    local -A seen=() counts=()
    if [ "$remote_rc" -ne 0 ]; then
        echo "STAGE=rch EXIT=${remote_rc} (remote job did not complete normally)"
        failed=1
    fi
    while IFS= read -r line; do
        if [[ "$line" =~ ^TEST_COUNT=([a-z0-9_-]+)\ PASSED=([1-9][0-9]*)\ BINARIES=([1-9][0-9]*)$ ]]; then
            counts["${BASH_REMATCH[1]}"]=1
        elif [[ "$line" =~ ^STAGE=([a-z0-9_-]+)\ EXIT=([0-9]+)$ ]]; then
            name="${BASH_REMATCH[1]}"
            [ -n "${seen[$name]:-}" ] && failed=1
            seen["$name"]=1
            last_stage="$name"
            [ "${BASH_REMATCH[2]}" -eq 0 ] || failed=1
            echo "$line"
        fi
    done < "$receipt"
    for stage in "$@"; do
        if [ -z "${seen[$stage]:-}" ]; then
            echo "STAGE=${stage} EXIT=missing (no terminal receipt)"
            failed=1
        fi
        case "$stage" in
            lib-tests|test-*|goldens|goldens-regen)
                if [ -z "${counts[$stage]:-}" ]; then
                    echo "gate: ${stage} has no positive terminal test count" >&2
                    failed=1
                fi ;;
        esac
    done
    if [ "$last_stage" != job-complete ]; then
        echo "gate: missing final job-complete receipt" >&2
        failed=1
    fi
    return "$failed"
}

run_fmt() {
    local formatter wrapper rc
    formatter="${RUSTFMT:-$(command -v rustfmt)}" || return 1
    wrapper="$(mktemp -t cass-gate-rustfmt.XXXXXX)" || return 1
    # cargo-fmt maps a signal-killed rustfmt's missing exit code to success.
    # A waiting shell converts that signal into a normal nonzero exit status.
    # Keep the wrapper for diagnostics and never exec away its waiting shell.
    cat > "$wrapper" <<'SH'
#!/usr/bin/env bash
"${CASS_GATE_RUSTFMT:?}" "$@"
exit "$?"
SH
    chmod +x "$wrapper" || return 1
    CASS_GATE_RUSTFMT="$formatter" RUSTFMT="$wrapper" cargo fmt --check
    rc=$?
    return "$rc"
}

prepare_compile_inputs() (
    set -o pipefail
    local paths="$1" expected="$2" actual identity_rc=0 freshness_rc=0
    actual="$(printf '%s' "$paths" | base64 -d | gzip -d | \
        xargs -0 -r sha256sum | sha256sum | cut -d' ' -f1)" || identity_rc=$?
    [ "$actual" = "$expected" ] || identity_rc=1
    echo "SOURCE_CONTENT_SHA256=${actual} EXPECTED=${expected}"
    echo "STAGE=source-identity EXIT=${identity_rc}"
    [ "$identity_rc" -eq 0 ] || return 1

    # Transfers preserve mtimes, which can predate a warm Cargo fingerprint
    # even when the input bytes changed. Refresh verified inputs before any
    # compiler runs. Keep cache artifacts and source bytes intact; -c never
    # recreates an input removed since verification. The second digest catches
    # missing or changed inputs, including a concurrent edit during refresh.
    printf '%s' "$paths" | base64 -d | gzip -d | \
        xargs -0 -r touch -c -- || freshness_rc=$?
    actual="$(printf '%s' "$paths" | base64 -d | gzip -d | \
        xargs -0 -r sha256sum | sha256sum | cut -d' ' -f1)" || freshness_rc=$?
    [ "$actual" = "$expected" ] || freshness_rc=1
    echo "STAGE=source-freshness EXIT=${freshness_rc}"
    [ "$freshness_rc" -eq 0 ]
)

run_encoded_remote_script() (
    set -o pipefail
    local decoded_script
    # A truncated gzip stream can print a complete shell command before it
    # fails its trailer check. Finish decoding successfully before executing.
    decoded_script="$(printf '%s' "$1" | base64 -d | gzip -d)" || return "$?"
    bash -c "$decoded_script"
)

# A scanner-only invocation has explicit inputs. Bind their names, symlink
# identities and bytes without discovering unrelated untracked scratch trees.
# The ordinary build gate retains its complete source inventory below.
ubs_source_digest() (
    set -o pipefail
    if [ "$#" -eq 0 ]; then
        echo "gate: --verify-ubs requires at least one source file" >&2
        return 2
    fi
    local path
    {
        for path in "$@"; do
            if [[ "$path" = /* || "$path" = .. || "$path" = ../* || "$path" = */../* ]] || [ ! -f "$path" ]; then
                echo "gate: UBS source must be an existing repository-relative file: ${path}" >&2
                exit 2
            fi
            printf '%s\0' "$path"
            if [ -L "$path" ]; then
                printf 'symlink\0'
                readlink --zero -- "$path" || exit 1
            else
                printf 'regular\0'
            fi
            sha256sum --zero -- "$path" || exit 1
        done
    } | sha256sum | cut -d' ' -f1
)

# Exercise the exact receipt parser without admitting a build or substituting
# tools. The shell regression suite feeds it recorded terminal-output shapes.
if [ "${1:-}" = --verify-receipt ]; then
    shift
    verify_receipt "$@"
    exit $?
fi
if [ "${1:-}" = --verify-test-log ]; then
    shift
    test_log_counts "$@"
    exit $?
fi
if [ "${1:-}" = --verify-fmt ]; then
    run_fmt
    exit $?
fi
if [ "${1:-}" = --verify-compile-inputs ]; then
    prepare_compile_inputs "$2" "$3"
    exit $?
fi
if [ "${1:-}" = --verify-remote-payload ]; then
    run_encoded_remote_script "$2"
    exit $?
fi
if [ "${1:-}" = --verify-ubs-source ]; then
    shift
    ubs_source_digest "$@"
    exit $?
fi

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)" || exit 1
cd "$PROJECT_ROOT" || exit 1

LIB_FILTER=""
RUN_LIB=1
RUN_INTEGRATION=1
RUN_BIN=1
RUN_GOLDENS=1
LOCAL=0
REGEN_GOLDENS=0
RUN_DOCS_TRUTH=0
UBS_FILES=()
VERIFY_UBS=0
VERIFY_UBS_FILES=()
# Each entry is `<test-binary>` or `<test-binary>:<filter words>`; the filter
# is passed after `--` so one admission can run only the tests a change touches.
# search_metamorphic is the set-algebra oracle for search semantics (terms,
# Boolean grammar, time windows; bead 2l1b0.68): a wrong-answer regression
# there is invisible to every golden. pages_bundle holds the Pages viewer's
# safety checks (CSP, sanitizer, crypto worker, auth expiry, service worker);
# 20 of them sat red for days because no gate ran the target (2l1b0.77).
# search_service compiles the `cass serve` sources and their unit tests (the
# cass bin itself is `test = false`); 8 of them sat red for 9 days (5188e).
INTEGRATION_TESTS=(cli_robot bookmarks_cli search_metamorphic pages_bundle search_service)
while [ $# -gt 0 ]; do
    case "$1" in
        --lib-filter) LIB_FILTER="$2"; shift 2 ;;
        --no-lib) RUN_LIB=0; shift ;;
        --local) LOCAL=1; shift ;;
        --ubs-files) IFS=',' read -r -a UBS_FILES <<<"$2"; shift 2 ;;
        # Run the strict UBS stage alone over the named files, with the same
        # pinned runner, delta baseline and negative control the admission
        # runs. The stage itself is dispatched after its functions are
        # defined.
        --verify-ubs) VERIFY_UBS=1; shift; VERIFY_UBS_FILES=("$@"); break ;;
        --integration) IFS=',' read -r -a INTEGRATION_TESTS <<<"$2"; shift 2 ;;
        # Regenerate goldens (UPDATE_GOLDENS=1) before verifying them. The
        # verify stage still runs, and every resulting diff under tests/golden
        # must be read before it is committed — regeneration is never a way to
        # make a red golden green.
        --regen-goldens) REGEN_GOLDENS=1; shift ;;
        # The fleet closes an SSH session after 30 minutes (rch E104). A full
        # lib suite plus integration plus goldens does not fit in one job on a
        # busy worker, so split: `--lib-only` (fmt, clippy, lib suite) and a
        # second run with `--no-lib` for integration + goldens.
        --lib-only) RUN_INTEGRATION=0; RUN_GOLDENS=0; shift ;;
        --no-goldens) RUN_GOLDENS=0; shift ;;
        --no-bin) RUN_BIN=0; shift ;;
        # README ↔ code truth (WS-A.9): key bindings, `cass … --flag` usages
        # and env vars, checked with the debug binary the integration stage
        # just built. Opt-in until the README is clean against main.
        --docs-truth) RUN_DOCS_TRUTH=1; shift ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# The fleet's warm target dir. Deliberately NOT the ambient CARGO_TARGET_DIR:
# that variable points at a local build dir in many agent shells, and passing
# it through made a gate run compile clippy from scratch on the worker
# (13 minutes instead of 5). Override with GATE_TARGET_DIR when needed.
TARGET_DIR="${GATE_TARGET_DIR:-/data/tmp/cass-check-target}"
RETRIES="${GATE_RETRIES:-1}"
RETRY_SLEEP="${GATE_RETRY_SLEEP:-90}"
BUILD_JOBS="${GATE_BUILD_JOBS:-2}"
UBS_TIMEOUT="${GATE_UBS_TIMEOUT_SECS:-300}"
if [[ ! "$BUILD_JOBS" =~ ^[1-9][0-9]*$ ]] || [[ ! "$RETRIES" =~ ^[1-9][0-9]*$ ]]; then
    echo "gate: GATE_BUILD_JOBS and GATE_RETRIES must be positive integers" >&2
    exit 2
fi
if [[ ! "$UBS_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
    echo "gate: GATE_UBS_TIMEOUT_SECS must be a positive integer" >&2
    exit 2
fi
prepare_gate_source() {
    SOURCE_HEAD="$(git rev-parse HEAD)" || return 1
    if [ "${#UBS_FILES[@]}" -eq 0 ]; then
        # The branch's own changes are measured against the upstream base this
        # branch merged, not against a remote-tracking ref that is thousands of
        # commits behind. The delta baseline in ubs_delta_gate uses the same
        # commit, so the scanned set and the baseline always agree.
        UBS_BASE="${GATE_BASE:-$(git merge-base "$SOURCE_HEAD" upstream/main 2>/dev/null || git merge-base origin/main "$SOURCE_HEAD")}" || exit 1
        while IFS= read -r -d '' path; do
            [ -f "$path" ] && UBS_FILES+=("$path")
        done < <({
            git diff --name-only -z "$UBS_BASE" -- '*.rs' '*.py' '*.sh' '*.js' '*.ts' '*.tsx' '*.jsx'
            git ls-files --others --exclude-standard -z -- '*.rs' '*.py' '*.sh' '*.js' '*.ts' '*.tsx' '*.jsx'
        } | sort -zu)
    fi
    # Honour .ubsignore. The pinned runner scans every file it is named, even one
    # its own .ubsignore, --ignore-file or --exclude covers, so the config-level
    # suppression AGENTS.md prescribes only takes effect if the gate drops those
    # paths before naming them. Each skipped path is printed in the receipt.
    if [ -f .ubsignore ] && [ "${#UBS_FILES[@]}" -gt 0 ]; then
        declare -A ubs_ignored=()
        while IFS= read -r -d '' path; do
            ubs_ignored["$path"]=1
        done < <(git ls-files -z -c -o -i --exclude-from=.ubsignore -- "${UBS_FILES[@]}")
        ubs_kept=()
        for path in "${UBS_FILES[@]}"; do
            if [ -n "${ubs_ignored[${path#./}]:-}" ]; then
                echo "gate: UBS skips .ubsignore path ${path}"
            else
                ubs_kept+=("$path")
            fi
        done
        UBS_FILES=("${ubs_kept[@]}")
    fi
    UBS_FILES_ARG=""
    if [ "${#UBS_FILES[@]}" -gt 0 ]; then
        for path in "${UBS_FILES[@]}"; do
            if [[ "$path" = /* || "$path" = .. || "$path" = ../* || "$path" = */../* ]] || [ ! -f "$path" ]; then
                echo "gate: UBS source must be an existing repository-relative file: ${path}" >&2
                exit 2
            fi
            printf -v quoted_path ' %q' "./${path}"
            UBS_FILES_ARG+="$quoted_path"
        done
    fi
    source_digest() {
        {
            git diff --binary HEAD
            git ls-files --others --exclude-standard -z | xargs -0 -r sha256sum
        } | sha256sum | cut -d' ' -f1
    }
    SOURCE_DIFF="$(source_digest)" || exit 1
    # RCH 1.0.63 does not combine --job with clean-overlay/content-receipt.
    # Verify the transferred inputs ourselves before running any stage.
    # rch's global transfer excludes drop every `.beads/` directory, so tracked
    # files such as tests/.beads/* never reach the worker. Hashing them made the
    # identity verdict depend on stale leftovers of whichever worker was chosen
    # (green on one worker, red on another for the same tree). No build or test
    # reads them, so they are not inputs this gate can or needs to verify.
    SOURCE_PATHS="$(git ls-files --cached --others --exclude-standard -z -- \
        Cargo.toml Cargo.lock rust-toolchain.toml build.rs src tests benches scripts \
        .cargo .github README.md ':(exclude,glob)**/.beads/**' | \
        sort -zu | gzip -c | base64 -w0)" || exit 1
    SOURCE_CONTENT="$(printf '%s' "$SOURCE_PATHS" | base64 -d | gzip -d | \
        xargs -0 -r sha256sum | sha256sum | cut -d' ' -f1)" || exit 1
}

# Keep complete terminal output and require a positive test count. Cargo exits
# zero when a misspelled filter selects no tests; that is not validation.
run_tests() {
    local stage="$1"
    shift
    local -a filters=()
    while [ "${1:-}" = --filter ]; do
        filters+=("$2")
        shift 2
    done
    local log rc counts
    local -a statuses
    log="$(mktemp -t cass-gate-tests.XXXXXX)"
    "$@" 2>&1 | tee "$log"
    statuses=("${PIPESTATUS[@]}")
    rc=${statuses[0]}
    [ "${statuses[1]}" -eq 0 ] || rc=1
    if [ "$rc" -eq 0 ]; then
        if counts="$(test_log_counts "$log" "${filters[@]}")"; then
            echo "TEST_COUNT=${stage} ${counts}"
        else
            echo "gate: ${stage} has missing or empty test binaries (log=${log})" >&2
            rc=1
        fi
    fi
    echo "STAGE=${stage} EXIT=${rc}"
}

run_ubs() {
    local expected actual scanner installed expected_sha expected_contract_sha tool_dir
    expected="$(tr -d '[:space:]' < .github/workflows/ubs-version.txt)"
    # The runner verifies its language modules against embedded release hashes.
    # Updating the pin also requires reviewing the new runner digest here.
    if [ "$expected" != v5.4.4 ]; then
        echo "gate: no reviewed UBS runner digest for ${expected}" >&2
        return 1
    fi
    expected_sha=bdeccbb35f2056e6177c73b3f0f9966d20d1cbeff22b7f084133c4a6ac625ee2
    expected_contract_sha=7319b2e5e085d0dd2434e5114291b2dee9fbbb96e2cfe02027eb263abcb06ac3
    # Keep both runner and modules private: a different pinned UBS invocation
    # would otherwise replace the shared module cache while this one scans.
    tool_dir="$(mktemp -d -t cass-gate-ubs.XXXXXX)" || return 1
    scanner="$tool_dir/ubs"
    installed="$(command -v ubs || true)"
    if [ -n "$installed" ] && [ "$(sha256sum "$installed" | cut -d' ' -f1)" = "$expected_sha" ]; then
        cp "$installed" "$scanner" || return 1
    else
        echo "gate: acquiring pinned UBS ${expected} in ${tool_dir}"
        curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
            --max-time 60 --user-agent 'cass-gate (+https://github.com/Dicklesworthstone/coding_agent_session_search)' \
            "https://raw.githubusercontent.com/Dicklesworthstone/ultimate_bug_scanner/${expected}/ubs" \
            --output "$scanner" || return 1
    fi
    if [ "$(sha256sum "$scanner" | cut -d' ' -f1)" != "$expected_sha" ]; then
        echo "gate: UBS runner bytes differ from the pinned release" >&2
        return 1
    fi
    # Multi-file language routing runs before module acquisition. A private
    # runner without its contract would classify every explicit target as
    # unsupported and exit without scanning anything.
    mkdir -p "$tool_dir/modules" || return 1
    # A prior admission may seed a fresh private copy of the same pinned
    # release. The runner still verifies every module/helper before execution.
    # Copy bytes rather than retaining links to another invocation's cache.
    if [ -n "${GATE_UBS_MODULE_SEED_DIR:-}" ]; then
        if [ ! -d "$GATE_UBS_MODULE_SEED_DIR" ] || \
           [ ! -f "$GATE_UBS_MODULE_SEED_DIR/contract.json" ] || \
           [ "$(sha256sum "$GATE_UBS_MODULE_SEED_DIR/contract.json" | cut -d' ' -f1)" != "$expected_contract_sha" ]; then
            echo "gate: UBS module seed does not match the pinned release contract" >&2
            return 1
        fi
        cp -RL "$GATE_UBS_MODULE_SEED_DIR/." "$tool_dir/modules/" || return 1
        echo "UBS_MODULE_SEED=private-copy"
    fi
    curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --max-time 60 --user-agent 'cass-gate (+https://github.com/Dicklesworthstone/coding_agent_session_search)' \
        "https://raw.githubusercontent.com/Dicklesworthstone/ultimate_bug_scanner/${expected}/modules/contract.json" \
        --output "$tool_dir/modules/contract.json" || return 1
    if [ "$(sha256sum "$tool_dir/modules/contract.json" | cut -d' ' -f1)" != "$expected_contract_sha" ]; then
        echo "gate: UBS routing contract bytes differ from the pinned release" >&2
        return 1
    fi
    actual="$(bash "$scanner" --version)" || return 1
    echo "UBS_VERSION=${actual} UBS_PIN=${expected} UBS_SHA256=${expected_sha}"
    echo "UBS_MODULE_DIR=${tool_dir}/modules"
    echo "UBS_CONTRACT_SHA256=${expected_contract_sha}"
    if [ "$#" -eq 0 ]; then
        echo "UBS_FILES=0 (no changed scanner-supported source files)"
        return 0
    fi
    printf 'UBS_FILE=%s\n' "$@"
    # A delta gate, not a whole-file gate: see ubs_delta_gate below.
    ubs_delta_gate "$tool_dir" "$@"
}

# Strict UBS delta gate (bead 5v57k).
#
# The pinned runner scans every file it is named and refuses the admission on
# any warning it finds. The files this branch touches already carry the
# upstream base's own findings, so a whole-file gate reports tens of
# thousands of pre-existing warnings on every change: the gate can never
# pass, and no bead can close on its receipt. This gate still scans every
# named file and still refuses findings the branch introduced; it stops
# refusing on findings that already existed at the base commit.
#
# The baseline is derived from git history and never checked in. The base is
# the merge base with upstream/main, so a baseline that is stale or
# regenerated cannot quietly excuse a finding the branch introduced. Three
# runner properties make the delta usable, all measured against the pinned
# v5.4.4 release:
#
#   * `--format=text` never writes the findings sink a delta reads, so a
#     text-format delta reports nothing and exits 0 even with a planted
#     production unwrap in the scanned set. The delta therefore runs in JSON
#     format, and the stage's exit code is that run's exit code.
#   * finding fingerprints exclude the absolute path, so a base tree unpacked
#     in a temporary directory compares against a baseline scanned in the
#     working tree.
#   * a finding's fingerprint covers the statement it points at, so the base
#     tree is normalized with rustfmt before it is scanned. A base tree that
#     cannot be normalized is a loud failure, never a noisier delta.
ubs_delta_gate() {
    local tool_dir="$1"; shift
    local scanner="$tool_dir/ubs"
    local modules="$tool_dir/modules"
    local head_commit base_commit repo_root base_tree
    local cache_dir cache_key baseline_json base_scan_json delta_json
    local base_rc delta_rc clean_rc planted_rc control_rel rel f d
    local control_clean_json="" control_planted_json="" control_probe="" filter_rc
    local fmt_rc rustfmt_bin rustfmt_version
    local ast_grep_bin ast_grep_version="not-required" ast_grep_sha="not-required"
    local scanner_sha contract_sha engine_identity
    local -a base_files=() fmt_roots=()

    head_commit="$(git rev-parse HEAD 2>/dev/null)" || head_commit="${SOURCE_HEAD:-HEAD}"
    repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || repo_root="$PWD"
    base_commit="$(git merge-base "$head_commit" upstream/main 2>/dev/null)" || base_commit=""
    if [ -z "$base_commit" ]; then
        base_commit="$(git merge-base "$head_commit" origin/main 2>/dev/null)" || base_commit=""
    fi
    if [ -z "$base_commit" ]; then
        echo "gate: UBS delta found no base commit (no upstream/main or origin/main merge base)" >&2
        return 1
    fi
    echo "UBS_DELTA_BASE=${base_commit}"
    echo "UBS_DELTA_HEAD=${head_commit}"
    # The pinned runner starts one process per detected language by default.
    # Keep its complete rule set and file set, but admit one language at a time
    # so a mixed Rust/Python/JS/Bash release scan has a bounded process peak.
    # This is the runner's documented JOBS setting, not a finding filter.
    echo "UBS_DELTA_RUNNER_JOBS=1"
    if [ "$base_commit" = "$head_commit" ]; then
        echo "UBS_DELTA_NEW=0 (HEAD is the base commit: this branch introduced no finding)"
        return 0
    fi

    # Without ast-grep the Rust module silently omits AST rules, including
    # the unwrap negative control. Refuse partial coverage before caching it.
    for f in "$@"; do
        case "$f" in
            *.rs)
                ast_grep_bin="$(command -v ast-grep)" || {
                    echo "gate: Rust UBS coverage requires ast-grep" >&2
                    return 1
                }
                ast_grep_version="$("$ast_grep_bin" --version)" || return 1
                ast_grep_sha="$(sha256sum "$ast_grep_bin" | cut -d' ' -f1)" || return 1
                echo "UBS_AST_GREP_VERSION=${ast_grep_version} UBS_AST_GREP_SHA256=${ast_grep_sha}"
                break
                ;;
        esac
    done
    scanner_sha="$(sha256sum "$scanner" | cut -d' ' -f1)" || return 1
    contract_sha="$(sha256sum "$modules/contract.json" | cut -d' ' -f1)" || return 1
    engine_identity="$(printf '%s\n' "$scanner_sha" "$contract_sha" "$ast_grep_version" "$ast_grep_sha" | sha256sum | cut -d' ' -f1)" || return 1
    echo "UBS_DELTA_ENGINE_SHA256=${engine_identity}"

    # The base tree carries every file the named set had at the base commit.
    # It is unpacked on every run because the negative control below needs it
    # even when the baseline scan itself is served from the cache.
    base_tree="$(mktemp -d -t cass-gate-ubs-base.XXXXXX)" || return 1
    if ! ( cd "$repo_root" && git archive "$base_commit" -- . ':(exclude).beads' 2>/dev/null ) | tar -x -C "$base_tree"; then
        echo "gate: UBS delta could not unpack the base tree at ${base_commit}" >&2
        return 1
    fi
    rustfmt_bin="${RUSTFMT:-rustfmt}"
    for f in src/lib.rs src/main.rs; do
        [ -f "$base_tree/$f" ] && fmt_roots+=("$base_tree/$f")
    done
    for d in src/bin tests benches examples; do
        [ -d "$base_tree/$d" ] || continue
        while IFS= read -r f; do fmt_roots+=("$f"); done < <(find "$base_tree/$d" -maxdepth 1 -name '*.rs' | sort)
    done
    fmt_rc=0
    ( cd "$base_tree" && "$rustfmt_bin" --edition 2024 "${fmt_roots[@]}" ) >/dev/null 2>"$base_tree/rustfmt.err" || fmt_rc=$?
    if [ "$fmt_rc" -ne 0 ]; then
        echo "gate: UBS delta base tree could not be rustfmt-normalized" >&2
        sed -n '1,20p' "$base_tree/rustfmt.err" >&2
        return 1
    fi

    # The baseline depends on the base commit, the named file set and the
    # rustfmt that normalized the base, and the actual scanner/AST engine.
    # A baseline from a partial or different engine cannot excuse findings.
    # The base moves far less often
    # than the branch does. Cache it under the gate's target dir so a
    # repeated admission does not rescan the base.
    rustfmt_version="$("$rustfmt_bin" --version 2>/dev/null)"
    cache_dir="${GATE_UBS_BASELINE_CACHE:-${CARGO_TARGET_DIR:-$repo_root/.local-state}/ubs-baseline}"
    cache_key="$( { printf '%s\n' "$base_commit" "$rustfmt_version" "$engine_identity"; printf '%s\n' "$@"; } | sha256sum | cut -d' ' -f1)"
    baseline_json="$cache_dir/$cache_key.json"
    mkdir -p "$cache_dir" 2>/dev/null || true

    if [ -s "$baseline_json" ]; then
        echo "UBS_DELTA_BASELINE=cache"
    else
        for rel in "$@"; do
            rel="${rel#./}"
            [ -f "$base_tree/$rel" ] && base_files+=("$rel")
        done
        echo "UBS_DELTA_BASE_FILES=${#base_files[@]}"
        if [ "${#base_files[@]}" -gt 0 ]; then
            base_scan_json="$base_tree/base.json"
            ( cd "$base_tree" && JOBS=1 UBS_ALLOW_UNVERIFIED_HELPERS=0 bash "$scanner" --module-dir="$modules" --no-auto-update --format=json --ci --save-baseline="$baseline_json" "${base_files[@]}" ) >"$base_scan_json" 2>"$base_tree/base.err"
            base_rc=$?
            # The scanner exits non-zero when it finds anything, so the exit
            # code is not a failure signal here: the baseline file is.
            if [ ! -s "$baseline_json" ]; then
                echo "gate: UBS delta baseline scan produced no baseline (exit ${base_rc})" >&2
                sed -n '1,20p' "$base_tree/base.err" >&2
                return 1
            fi
            echo "UBS_DELTA_BASELINE_SCAN_EXIT=${base_rc} (findings in the base are expected)"
        else
            # Every named file is new in this branch, so every finding in it
            # is new. There is no base content to compare against.
            echo "UBS_DELTA_BASE_FILES=0 (every named file is new in this branch)"
            baseline_json=""
        fi
    fi

    delta_json="$(mktemp -t cass-gate-ubs-delta.XXXXXX)" || return 1
    if [ -n "$baseline_json" ]; then
        # Negative control: a delta gate that cannot fail proves nothing.
        # The base version of one production file is already in the baseline,
        # so scanning it must report no new finding; the same file with one
        # planted production unwrap must report one. The only difference
        # between the two runs is the plant.
        # Pick a production file from the scanned set. The baseline records
        # findings per occurrence (rust.ownership.unwrap-expect alone carries
        # ~20k occurrences across the tree), so a planted unwrap is a new
        # finding rather than a repeat of a file-level finding.
        control_rel=""
        for rel in "$@"; do
            rel="${rel#./}"
            case "$rel" in
                src/*) : ;;
                *) continue ;;
            esac
            case "$rel" in
                */tests/*|*/tests.rs) continue ;;
            esac
            [ -f "$base_tree/$rel" ] || continue
            control_rel="$rel"
            break
        done
        if [ -n "$control_rel" ]; then
            ( cd "$base_tree" && JOBS=1 UBS_ALLOW_UNVERIFIED_HELPERS=0 bash "$scanner" --module-dir="$modules" --no-auto-update --format=json --ci --baseline="$baseline_json" --new-only --fail-on-warning "$control_rel" ) >"$base_tree/control_clean.json" 2>"$base_tree/control_clean.err"
            clean_rc=$?
            cp "$base_tree/$control_rel" "$base_tree/control_rel.orig" || return 1
            # The probe is planted above the file's first #[cfg(test)] or
            # `mod tests` boundary, never appended after it: the boundary
            # heuristic treats everything from that line on as test code, so a
            # probe appended at the end of a file that has one would be
            # excluded as test code and the control would prove nothing. The
            # insertion point is after the file's leading inner attributes
            # and inner doc comments, which is always a top-level position.
            python3 - "$base_tree/$control_rel" "$base_tree/control_probe.txt" <<'PYPLANT'
import json
import sys
from pathlib import Path

target = Path(sys.argv[1])
probe_lines = [
    "// cass-gate 5v57k negative control: a production unwrap the delta must",
    "// report as a new finding. Planted by scripts/gate.sh in a temporary tree.",
    "fn __cass_ubs_gate_control_probe() -> usize {",
    "    let value: Option<usize> = Some(1);",
    # Identifiers are erased by the pinned scanner's fingerprint normalizer.
    # A bare unwrap collides with old calls and shifts their ordinal instead
    # of reporting this plant. Keep a distinct literal on the unwrap's line.
    "    value.unwrap() + 0xCA55_5A17",
    "}",
    "// ubs:ignore[rust.domain.sql-concat] -- Scope control: immutable SQL arithmetic.",
    'const __CASS_UBS_SQL_CONTROL: &str = r#"',
    "    UPDATE gate_counter SET value = value + 0xCA55_5A18;",
    '"#;',
    "fn __cass_ubs_sql_control(untrusted: &str) -> String {",
    '    "SELECT 0xCA55_5A19 " .to_string() + untrusted',
    "}",
]
text = target.read_text(encoding="utf-8")
lines = text.splitlines(keepends=True)
insert_at = 0
for index, line in enumerate(lines):
    stripped = line.lstrip()
    if stripped.startswith("#!") or stripped.startswith("//!"):
        insert_at = index + 1
    else:
        break
planted = [line + "\n" for line in probe_lines]
planted_text = "".join(lines[:insert_at] + planted + lines[insert_at:])
target.write_text(planted_text, encoding="utf-8")
source_copy = Path(sys.argv[2]).with_suffix(".rs")
source_copy.write_text(planted_text, encoding="utf-8")
Path(sys.argv[2]).write_text(json.dumps({
    "path": str(target.resolve()), "source": str(source_copy),
    "unwrap": insert_at + 5, "static_sql": insert_at + 9,
    "dynamic_sql": insert_at + 12,
}) + "\n", encoding="utf-8")
PYPLANT
            ( cd "$base_tree" && JOBS=1 UBS_ALLOW_UNVERIFIED_HELPERS=0 bash "$scanner" --module-dir="$modules" --no-auto-update --format=json --ci --baseline="$baseline_json" --new-only --fail-on-warning "$control_rel" ) >"$base_tree/control_planted.json" 2>"$base_tree/control_planted.err"
            planted_rc=$?
            mv "$base_tree/control_rel.orig" "$base_tree/$control_rel"
            control_clean_json="$base_tree/control_clean.json"
            control_planted_json="$base_tree/control_planted.json"
            control_probe="$base_tree/control_probe.txt"
            if [ "$clean_rc" -ne 0 ] || [ "$planted_rc" -eq 0 ]; then
                echo "gate: UBS delta negative control failed (clean=${clean_rc} planted=${planted_rc})" >&2
                echo "gate: the delta must report no new finding for the base file and one for the planted file" >&2
                return 1
            fi
            echo "UBS_DELTA_CONTROL=ok (base file clean exit=${clean_rc}, planted unwrap detected exit=${planted_rc})"
        else
            echo "UBS_DELTA_CONTROL=skipped (no production file in the scanned set to control)"
        fi
        ( cd "$repo_root" && JOBS=1 UBS_ALLOW_UNVERIFIED_HELPERS=0 bash "$scanner" --module-dir="$modules" --no-auto-update --format=json --ci --baseline="$baseline_json" --new-only --fail-on-warning "$@" ) >"$delta_json" 2>"${delta_json}.err"
    else
        ( cd "$repo_root" && JOBS=1 UBS_ALLOW_UNVERIFIED_HELPERS=0 bash "$scanner" --module-dir="$modules" --no-auto-update --format=json --ci --fail-on-warning "$@" ) >"$delta_json" 2>"${delta_json}.err"
    fi
    delta_rc=$?
    # The JSON delta is the verdict: a text-format delta reports nothing and
    # exits 0, which is exactly the gate that never fails.
    # The verdict is the filtered delta, not the scanner's exit code. The
    # pinned v5.4.4 meta-runner rejects --exclude-tests, so reviewed per-rule
    # exemptions are applied gate-side. A finding is eligible for an exemption
    # only when Rust itself does not compile it without `cfg(test)`:
    #   a tests//benches/ path component, a file that is only reachable
    #   through cfg(test)-gated `mod` declarations
    #   (ubs_core.analyzers.cfg_test_only_rust.compute_test_only), or a line
    #   inside a cfg(test)-gated item or an exact #[test] function.
    # Upstream's own --exclude-tests also drops everything at or after a
    # file's first `#[cfg(test)]` or `mod tests` line. Upstream's docstring
    # warns that this heuristic "can encompass later production code", so
    # that part is reported as the win/lose split instead of being excluded:
    # the receipt names every finding upstream would drop and this gate
    # keeps. The rule is Rust-only, exactly as upstream's flag is. The
    # test-only set is computed over the whole repository rather than the
    # scanned set: a wider reference universe can only remove exclusions,
    # never add one, so it cannot hide production code that a narrower
    # universe would expose.
    filter_rc=0
    python3 - "$delta_json" "$repo_root" "$modules" "$control_clean_json" "$control_planted_json" "$control_probe" <<'PYSUM' || filter_rc=$?
import json
import os
import re
import sys
from bisect import bisect_right
from collections import Counter
from pathlib import Path

delta_path = sys.argv[1]
repo_root = Path(sys.argv[2])
modules_dir = Path(sys.argv[3])
control_clean_path = sys.argv[4] if len(sys.argv) > 4 else ""
control_planted_path = sys.argv[5] if len(sys.argv) > 5 else ""
control_probe_path = sys.argv[6] if len(sys.argv) > 6 else ""

BLOCKING = ("critical", "warning")
# Only these blocking Rust rules are inapplicable to code that the non-test
# binary never compiles. Every other rule, including a future rule, remains
# enforced in tests. Informational findings remain visible as well. The
# per-rule reasons below are part of the receipt, not a blanket test filter.
TEST_ONLY_EXEMPTIONS = {
    "rust.async.lock-unwrap": "A poisoned fixture lock fails the test, not a shipped request.",
    "rust.collections.clone-in-loop": "Fixture allocation does not affect a production hot path.",
    "rust.numeric.instant-subtraction": "A bad test clock bound fails the test.",
    "rust.ownership.panic-macro": "An explicit panic is a test failure or expected failure probe.",
    "rust.ownership.unreachable-macro": "The sentinel fails its test if the fixture branch is reached.",
    "rust.ownership.unwrap-expect": "A missing fixture value fails the test.",
    "rust.panic.assert-macros": "Assertions define the test's expected outcome.",
    "rust.panic.direct-indexing": "An out-of-range fixture access fails the test.",
    "rust.parsing.env-var-unwrap": "A missing test environment variable fails the test.",
    "rust.parsing.parse-unwrap": "An invalid literal fixture fails the test.",
    "rust.parsing.serde-unwrap": "Invalid fixture JSON fails the test.",
    "rust.perf.string-alloc-in-loop": "Test-only rendering and setup are not production hot paths.",
    "rust.security.archive-entry-path": "Test fixture traversal stacks hold adversarial paths, not extraction sinks.",
    "rust.security.constant-time-compare": "Test assertions have no deployed timing oracle; current values are fixture labels or local round-trip bytes.",
    "rust.security.path-traversal": "Test fixture joins and symlinks have no shipped untrusted path sink.",
}
# Pinned UBS 5.4.4's Python lifecycle analyzer is intraprocedural and its
# registry findings bypass the source suppression parser. These three retained
# workers have explicit kill/wait ownership in their callers or close methods.
# Admit only the reviewed assignment and same-line rule-specific marker; all
# other Python lifecycle findings remain blocking.
PYTHON_POPEN_SITES = {
    "scripts/cass_lexical_ipc.py": "self.process",
    "scripts/benchmark_lexical_admission.py": "self.proc",
    "scripts/benchmark_lexical_ipc.py": "process",
}
MOD_TESTS_RE = re.compile(r"^\s*mod tests(\s|\{|;|$)")
INCLUDE_RE = re.compile(r'include!\s*\(\s*"([^"]+)"')
SKIP_DIRS = {".git", "target", ".local-state", "node_modules", "vendor", ".venv"}
ATTR_RE = re.compile(r"^\s*#\[([^\]]*)\]")
CFG_TEST_RE = re.compile(r"^\s*cfg\s*\(\s*(?:test\s*\)|all\s*\(\s*test\s*[,)])")
# The items whose body a cfg(test) attribute gates. A cfg(test) attribute on
# anything else (a statement, a use, a const) never widens the excluded
# region past its own line.
ITEM_SIG_RE = re.compile(
    r"^(?:pub(?:\s*\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?"
    r"(?:unsafe\s+)?(?:extern\s+\"[^\"]*\"\s+)?"
    r"(?:fn|mod|struct|enum|impl|trait|union)\b"
)
TEST_FN_SIG_RE = re.compile(
    r"^(?:pub(?:\s*\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?"
    r"(?:unsafe\s+)?fn\b"
)
# How far a test-only item's signature may run before its body must open.
ITEM_BODY_LOOKAHEAD = 200


def repo_rust_files(root):
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            if name.endswith(".rs"):
                found.append(str(Path(dirpath) / name))
    return found


def test_boundary(path_str):
    """Upstream ubs_core.rust_scan.Scan.test_boundary, unchanged.

    Kept for the win/lose comparison only. Upstream's own docstring warns
    that this heuristic "can encompass later production code", so it is not
    the rule this gate excludes with.
    """
    try:
        lines = Path(path_str).read_text(encoding="utf-8", errors="ignore").splitlines()
    except OSError:
        return 0
    b1 = b2 = 0
    for idx, line in enumerate(lines, start=1):
        if b1 == 0 and "#[cfg(test)]" in line:
            b1 = idx
        if b2 == 0 and MOD_TESTS_RE.match(line):
            b2 = idx
        if b1 and b2:
            break
    if b1 and b2:
        return min(b1, b2)
    return b1 or b2


def strip_rust(text, literal_spans=None):
    """Blank out comments and literals so brace counting sees code only."""
    def blank(chunk):
        return "".join(ch if ch == "\n" else " " for ch in chunk)

    out = []
    index = 0
    size = len(text)
    while index < size:
        ch = text[index]
        if ch == "/" and index + 1 < size and text[index + 1] == "/":
            stop = text.find("\n", index)
            stop = size if stop < 0 else stop
            out.append(blank(text[index:stop]))
            index = stop
            continue
        if ch == "/" and index + 1 < size and text[index + 1] == "*":
            depth = 1
            stop = index + 2
            while stop < size and depth:
                if text.startswith("/*", stop):
                    depth += 1
                    stop += 2
                elif text.startswith("*/", stop):
                    depth -= 1
                    stop += 2
                else:
                    stop += 1
            if depth and literal_spans is not None:
                literal_spans.append((None, None))
            out.append(blank(text[index:stop]))
            index = stop
            continue
        if ch == "r" and index + 1 < size and text[index + 1] in "#\"":
            stop = index + 1
            hashes = 0
            while stop < size and text[stop] == "#":
                hashes += 1
                stop += 1
            if stop < size and text[stop] == "\"":
                stop += 1
                term = "\"" + "#" * hashes
                found = text.find(term, stop)
                stop = size if found < 0 else found + len(term)
                if literal_spans is not None:
                    literal_spans.append((index, stop) if found >= 0 else (None, None))
                out.append(blank(text[index:stop]))
                index = stop
                continue
        if ch == "\"":
            stop = index + 1
            terminated = False
            while stop < size:
                if text[stop] == "\\":
                    stop += 2
                    continue
                if text[stop] == "\"":
                    stop += 1
                    terminated = True
                    break
                stop += 1
            if literal_spans is not None:
                literal_spans.append((index, stop) if terminated else (None, None))
            out.append(blank(text[index:stop]))
            index = stop
            continue
        if ch == "'":
            match = re.match(r"'(\\.|[^\\'])'", text[index:])
            if match:
                stop = index + match.end()
                out.append(blank(text[index:stop]))
                index = stop
                continue
        out.append(ch)
        index += 1
    return "".join(out)


def rust_test_regions(raw):
    """Exact body lines of cfg(test) items and #[test] functions.

    This is the sound part of upstream's test classification: a finding is
    test code only when the file is a test-only file or the line is inside an
    item that Rust itself does not compile without `cfg(test)`. Upstream's
    boundary heuristic also drops everything after a file's first
    `#[cfg(test)]` or `mod tests` line, which can hide production code that
    follows the test module; that part is reported, not excluded.
    """
    source = strip_rust(raw)
    lines = source.splitlines(keepends=True)
    offsets = [0]
    for line in lines:
        offsets.append(offsets[-1] + len(line))
    stack = []
    closing = {}
    pairs = {"(": ")", "[": "]", "{": "}"}
    for offset, char in enumerate(source):
        if char in pairs:
            stack.append((offset, char))
        elif char in ")]}":
            if not stack or pairs[stack[-1][1]] != char:
                return set(), False
            opened, opener = stack.pop()
            if opener == "{":
                closing[opened] = offset
    if stack:
        # Ambiguous or unbalanced source never grants an exemption.
        return set(), False
    marked = set()
    pending = None
    for index, line in enumerate(lines):
        text = line.strip()
        match = ATTR_RE.fullmatch(text)
        if match:
            if CFG_TEST_RE.match(match.group(1)):
                pending = "cfg"
            elif match.group(1).strip() == "test" and pending != "cfg":
                pending = "test"
            continue
        if not text:
            continue
        if pending:
            signature = TEST_FN_SIG_RE if pending == "test" else ITEM_SIG_RE
            if signature.match(text):
                limit = offsets[min(len(lines), index + ITEM_BODY_LOOKAHEAD + 1)]
                # A declaration or scope end cannot borrow a later item's
                # body. Unsupported signatures stay enforced.
                opening = re.search(r"[;{}]", source[offsets[index]:limit])
                if opening and opening.group() == "{":
                    body = offsets[index] + opening.start()
                    end = closing[body]
                    end_line = bisect_right(offsets, end) - 1
                    # Findings have line precision. A production item after
                    # the closing brace shares that line, so retain it all.
                    tail = source[end + 1:offsets[end_line + 1]].strip()
                    stop = end_line if tail else end_line + 1
                    marked.update(range(index + 1, stop + 1))
            pending = None
    return marked, True


def cfg_test_regions(path_str):
    try:
        raw = Path(path_str).read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return set(), False
    return rust_test_regions(raw)


SQL_CONSTANT_RULE = "rust.domain.sql-concat"
SQL_CONSTANT_LINES = {}


def annotated_sql_constant_lines(raw):
    """Honor the exact SQL rule marker on a sole immutable string initializer.

    UBS 5.4.4 loses a const declaration's marker at raw-string body lines.
    Its pinned marker parser still owns comment recognition. Unsupported
    expressions, ambiguous physical lines and malformed source stay enforced.
    """
    spans = []
    code = strip_rust(raw, spans)
    if any(start is None for start, _ in spans) or not rust_test_regions(raw)[1]:
        return set()
    markers = parse_markers(raw, lang="rust")
    admitted = set()
    for start, stop in spans:
        boundary = max(code.rfind(";", 0, start), code.rfind("{", 0, start),
                       code.rfind("}", 0, start)) + 1
        prefix = code[boundary:start]
        if not re.fullmatch(
                r"\s*(?:pub(?:\s*\([^)]*\))?\s+)?const\s+[A-Za-z_][A-Za-z_0-9]*"
                r"\s*:\s*&\s*(?:'static\s+)?str\s*=\s*", prefix):
            continue
        declaration = boundary + len(prefix) - len(prefix.lstrip())
        line_start = code.rfind("\n", 0, declaration) + 1
        if code[line_start:declaration].strip():
            continue
        end = re.match(r"\s*;", code[stop:])
        if end is None:
            continue
        semicolon = stop + end.end()
        line_end = code.find("\n", semicolon)
        if code[semicolon:len(code) if line_end < 0 else line_end].strip():
            continue
        first = raw.count("\n", 0, declaration) + 1
        if not any(marker.rules and SQL_CONSTANT_RULE in marker.rules
                   and (marker.line == first or
                        marker.standalone and marker.line == first - 1)
                   for marker in markers):
            continue
        admitted.update(range(raw.count("\n", 0, start) + 1,
                              raw.count("\n", 0, stop) + 2))
    return admitted


def reviewed_sql_constant(item, source_override=None):
    if finding_rule(item) != SQL_CONSTANT_RULE:
        return False
    path = finding_path(item)
    if not path.endswith(".rs"):
        return False
    if source_override is not None:
        marked = annotated_sql_constant_lines(source_override)
    else:
        if path not in SQL_CONSTANT_LINES:
            try:
                raw = Path(path).read_text(encoding="utf-8")
            except (OSError, UnicodeError):
                return False
            SQL_CONSTANT_LINES[path] = annotated_sql_constant_lines(raw)
        marked = SQL_CONSTANT_LINES[path]
    return finding_line(item) in marked


def verify_sql_constant_controls():
    marker = "// ubs:ignore[rust.domain.sql-concat] -- reviewed immutable SQL\n"
    literal = 'const SQL: &str = r#"\nUPDATE t SET n = n + 1;\n"#;\n'
    dynamic = 'fn query(input: &str) { let q = "SELECT " .to_string() + input; }\n'
    cases = [
        ("marked-raw", marker + literal + dynamic, {3}, {5}),
        ("unmarked", literal + dynamic, set(), {2, 4}),
        ("wrong-rule", marker.replace(SQL_CONSTANT_RULE, "rust.panic.direct-indexing") + literal,
         set(), {3}),
        ("bare-marker", "// ubs:ignore\n" + literal, set(), {3}),
        ("mutable-static", marker + literal.replace("const SQL", "static mut SQL"), set(), {3}),
        ("dynamic-initializer", marker + 'const SQL: String =\n"SELECT " .to_string() + input;\n',
         set(), {3}),
        ("concatenated-initializer", marker + literal.replace('"#;', '"# + input;'), set(), {3}),
        ("same-line-tail", marker + literal.rstrip() + dynamic, set(), {3, 4}),
        ("same-line-prefix", marker + 'fn p() {} ' + literal, set(), {3}),
        ("marker-in-string", 'const DOC: &str = r###"\n' + marker + '"###;\n' + literal,
         set(), {5}),
        ("marker-in-outer-comment", '/*\n' + marker + literal + '*/\n' + literal,
         set(), {8}),
        ("unbalanced", marker + literal + 'fn unfinished() {\n', set(), {3}),
        ("unterminated-literal", marker + literal[:-4], set(), {3}),
        ("unterminated-comment", marker + literal + '/*', set(), {3}),
        ("normal-string", marker + 'const SQL: &str = "UPDATE t SET n = n + 1;";\n',
         {2}, set()),
    ]
    for label, raw, admitted, enforced in cases:
        marked = annotated_sql_constant_lines(raw)
        if not admitted <= marked or enforced & marked:
            raise ValueError("SQL constant scope control failed: %s" % label)
    print("UBS_SQL_CONSTANT_SCOPE_CONTROL=%d passed (only annotated immutable literals admitted)" % len(cases))


def verify_test_region_controls():
    # These exercise the production parser directly; scanner clean/planted
    # controls below independently verify that production findings still block.
    cases = [
        ("included-functions", "fn helper() { assert!(false); }\n#[test]\n"
         "fn oracle() { assert!(true); }\nfn production() { assert!(false); }\n",
         {3}, {1, 4}, True),
        ("cfg-after-body", "#[cfg(test)]\nmod tests {\n"
         "fn oracle() { assert!(true); }\n}\nfn production() { assert!(false); }\n",
         {2, 3, 4}, {5}, True),
        ("same-line-production-tail", "#[cfg(test)]\nmod tests {\n"
         "fn oracle() { assert!(true); }\n} fn production() { assert!(false); }\n",
         {2, 3}, {4}, True),
        ("test-same-line-tail", "#[test]\n"
         "fn oracle() { assert!(true); } fn production() { assert!(false); }\n",
         set(), {2}, True),
        ("raw-string-attribute", 'const RAW: &str = r#"\n#[cfg(test)]\n'
         'fn fake() {\n}\n"#;\nfn production() {\nassert!(false);\n}\n',
         set(), {3, 6, 7, 8}, True),
        ("comment-attribute", "/*\n#[test]\n*/\n"
         "fn production() { assert!(false); }\n", set(), {4}, True),
        ("multiline-test", "#[test]\nfn oracle(\n)\n{\nassert!(true);\n}\n"
         "fn production() { assert!(false); }\n", {2, 3, 4, 5, 6}, {7}, True),
        ("test-on-nonfunction", "#[test]\nmod unsupported {\n"
         "fn production() { assert!(false); }\n}\n", set(), {2, 3, 4}, True),
        ("declaration-no-body", "#[cfg(test)]\nfn declaration();\n"
         "fn production() { assert!(false); }\n", set(), {2, 3}, True),
        ("unbalanced", "#[test]\nfn oracle() {\nassert!(true);\n",
         set(), {2, 3}, False),
    ]
    for label, source, admitted, enforced, expected_sound in cases:
        marked, sound = rust_test_regions(source)
        if (sound != expected_sound or not admitted <= marked or enforced & marked):
            raise ValueError("Rust test region control failed: %s" % label)
    print("UBS_DELTA_TEST_REGION_CONTROL=%d passed (test bodies only; helpers, tails, literals and unsupported source enforced)" % len(cases))


def in_test_path(path_str, test_only):
    norm = path_str.replace("\\", "/")
    parts = norm.split("/")
    for i in range(len(parts) - 1):
        if parts[i] in ("tests", "benches"):
            return True
    if path_str in test_only:
        return True
    try:
        if str(Path(path_str).resolve()) in test_only:
            return True
    except OSError:
        pass
    return False


REGIONS = {}


def is_test_line(path_str, line_no, test_only):
    """Sound classification: test-only file, cfg(test) item or #[test] fn."""
    if in_test_path(path_str, test_only):
        return True
    if path_str not in REGIONS:
        REGIONS[path_str] = cfg_test_regions(path_str)
    marked, sound = REGIONS[path_str]
    if not sound:
        return False
    return line_no in marked


def is_test_line_upstream(path_str, line_no, test_only):
    """Upstream ubs_core.rust_scan.Scan._is_test_line, unchanged."""
    if in_test_path(path_str, test_only):
        return True
    boundary = test_boundary(path_str)
    return boundary > 0 and line_no >= boundary


def finding_line(item):
    raw = item.get("line") or item.get("line_number") or item.get("line_start") or 0
    try:
        return int(raw)
    except (TypeError, ValueError):
        return 0


def finding_path(item):
    return item.get("file") or item.get("path") or ""


def finding_severity(item):
    return (item.get("severity") or item.get("level") or "").lower()


def finding_rule(item):
    return item.get("rule") or item.get("rule_id") or item.get("id") or "?"


def finding_message(item):
    return (item.get("message") or item.get("title") or item.get("description") or "")


def load_findings(path):
    if not path or not os.path.exists(path):
        return None
    try:
        with open(path, encoding="utf-8") as handle:
            doc = json.load(handle)
    except Exception:
        return None
    return doc.get("findings") or []


def reviewed_python_popen(item):
    if (finding_rule(item) != "python.lifecycle.popen_handle"
            or finding_severity(item) not in BLOCKING):
        return False
    path = finding_path(item).replace("\\", "/")
    for relative, binding in PYTHON_POPEN_SITES.items():
        if path != relative and not path.endswith("/" + relative):
            continue
        line_no = finding_line(item)
        if line_no <= 0:
            return False
        try:
            lines = (repo_root / relative).read_text(encoding="utf-8").splitlines()
        except OSError:
            return False
        if line_no > len(lines):
            return False
        line = lines[line_no - 1]
        return (f"{binding} = subprocess.Popen(" in line
                and "ubs:ignore[python.lifecycle.popen_handle]" in line
                and finding_message(item) ==
                f"subprocess handle {binding} never waited/terminated")
    return False


def classify(findings, test_only, source_overrides=None):
    excluded = []
    enforced = []
    upstream_excluded = []
    python_site_excluded = []
    sql_constant_excluded = []
    for item in findings:
        path = finding_path(item)
        source_override = (source_overrides or {}).get(path)
        lang = (item.get("lang") or "").lower()
        rust = lang == "rust" or path.endswith(".rs")
        if rust and is_test_line_upstream(path, finding_line(item), test_only):
            upstream_excluded.append(item)
        if source_override is None:
            test_line = rust and is_test_line(path, finding_line(item), test_only)
        else:
            marked, sound = rust_test_regions(source_override)
            test_line = rust and (in_test_path(path, test_only) or
                                  sound and finding_line(item) in marked)
        if (test_line
                and finding_severity(item) in BLOCKING
                and finding_rule(item) in TEST_ONLY_EXEMPTIONS):
            excluded.append(item)
        elif reviewed_python_popen(item):
            python_site_excluded.append(item)
        elif rust and reviewed_sql_constant(item, source_override):
            sql_constant_excluded.append(item)
        else:
            enforced.append(item)
    return excluded, enforced, upstream_excluded, python_site_excluded, sql_constant_excluded


try:
    verify_test_region_controls()
except Exception as exc:
    print("gate: UBS delta test-region controls failed: %s" % exc)
    sys.exit(3)

universe = repo_rust_files(repo_root)
sys.path.insert(0, str(modules_dir / "helpers"))
try:
    from ubs_core.analyzers.cfg_test_only_rust import compute_test_only
    from ubs_core.suppression import parse_markers
except Exception as exc:
    print("gate: UBS delta could not load the upstream test-only classifier: %s" % exc)
    sys.exit(3)
try:
    verify_sql_constant_controls()
except Exception as exc:
    print("gate: UBS SQL constant scope controls failed: %s" % exc)
    sys.exit(3)
try:
    test_only_list = compute_test_only(universe)
except Exception as exc:
    print("gate: UBS delta test-only classification failed: %s" % exc)
    sys.exit(3)
test_only = set(test_only_list)
for entry in test_only_list:
    try:
        test_only.add(str(Path(entry).resolve()))
    except OSError:
        pass

# An include! target that the exclusion covers would silently drop
# production code from the gate, so the exclusion must not cover one.
included = set()
for entry in universe:
    try:
        text = Path(entry).read_text(encoding="utf-8", errors="ignore")
    except OSError:
        continue
    for match in INCLUDE_RE.finditer(text):
        try:
            included.add(str((Path(entry).parent / match.group(1)).resolve()))
        except OSError:
            continue
clash = sorted(included & test_only)
if clash:
    print("gate: UBS delta test exclusion would hide an include! target: %s" % ", ".join(clash[:5]))
    sys.exit(3)
print("UBS_DELTA_TEST_ONLY_FILES=%d (repo-wide reference universe of %d rust files)" % (
    len(test_only_list), len(universe)))

doc = json.load(open(delta_path, encoding="utf-8"))
findings = doc.get("findings") or []
totals = doc.get("totals") or {}
status = doc.get("status")
failed_modules = doc.get("failed_modules") or []
excluded, enforced, upstream_excluded, python_site_excluded, sql_constant_excluded = classify(findings, test_only)
blocking = [item for item in enforced if finding_severity(item) in BLOCKING]
if len(python_site_excluded) > len(PYTHON_POPEN_SITES):
    print("gate: Python lifecycle waiver matched more findings than reviewed sites")
    sys.exit(3)
for relative, binding in PYTHON_POPEN_SITES.items():
    unmarked_probe = {
        "rule": "python.lifecycle.popen_handle", "severity": "warning",
        "path": relative, "line": 1,
        "message": f"subprocess handle {binding} never waited/terminated",
    }
    if reviewed_python_popen(unmarked_probe):
        print("gate: Python lifecycle waiver admitted an unmarked site in %s" % relative)
        sys.exit(3)
print("UBS_DELTA_NEW=%d critical=%s warning=%s info=%s" % (
    len(findings), totals.get("critical", 0), totals.get("warning", 0), totals.get("info", 0)))
print("UBS_DELTA_TEST_EXCLUDED=%d (reviewed blocking rules in test-only Rust code)" % len(excluded))
print("UBS_DELTA_PYTHON_SITE_EXCLUDED=%d (three reviewed retained-worker ownership sites)" % len(python_site_excluded))
print("UBS_DELTA_PYTHON_SITE_CONTROL=unmarked same-file findings remain blocking")
print("UBS_DELTA_SQL_CONSTANT_EXCLUDED=%d (explicit SQL-rule markers on sole immutable literals)" % len(sql_constant_excluded))
print("UBS_DELTA_UPSTREAM_TEST_EXCLUDED=%d (upstream --exclude-tests boundary rule, for the win/lose split)" % len(upstream_excluded))
print("UBS_DELTA_ENFORCED_BLOCKING=%d" % len(blocking))
for rule, count in sorted(Counter(finding_rule(item) for item in excluded).items()):
    print("UBS_DELTA_TEST_RULE_EXEMPT=%s count=%d reason=%s" % (
        rule, count, TEST_ONLY_EXEMPTIONS[rule]))
upstream_only = [item for item in upstream_excluded if item not in excluded]
print("UBS_DELTA_UPSTREAM_ONLY=%d (findings upstream --exclude-tests would drop that this gate keeps)" % len(upstream_only))
for item in upstream_only[:50]:
    print("UBS_DELTA_UPSTREAM_ONLY_FINDING=%s | %s:%s | %s" % (
        finding_rule(item), finding_path(item), finding_line(item), finding_severity(item)))
for item in excluded[:20]:
    print("UBS_DELTA_TEST_EXCLUDED_FINDING=%s | %s:%s | %s" % (
        finding_rule(item), finding_path(item), finding_line(item), finding_severity(item)))
for item in python_site_excluded:
    print("UBS_DELTA_PYTHON_SITE_EXCLUDED_FINDING=%s | %s:%s | %s | reason=retained worker has explicit kill/wait cleanup" % (
        finding_rule(item), finding_path(item), finding_line(item), finding_severity(item)))
for item in sql_constant_excluded:
    print("UBS_DELTA_SQL_CONSTANT_EXCLUDED_FINDING=%s | %s:%s | %s | reason=annotated immutable SQL literal" % (
        finding_rule(item), finding_path(item), finding_line(item), finding_severity(item)))
for item in blocking[:50]:
    print("UBS_DELTA_FINDING=%s | %s:%s | %s | %s" % (
        finding_rule(item), finding_path(item), finding_line(item), finding_severity(item),
        finding_message(item).replace("\n", " ")[:200]))

verdict = 1 if blocking else 0
print("UBS_DELTA_VERDICT=%d" % verdict)

# The control must survive the filter: the base file stays clean and the
# planted production unwrap must be reported as a production finding, not
# excluded as test code.
if control_clean_path and control_planted_path:
    for control_path in (control_clean_path, control_planted_path):
        try:
            control_document = json.load(open(control_path, encoding="utf-8"))
        except (OSError, ValueError) as exc:
            print("gate: UBS control document could not be read: %s" % exc)
            sys.exit(3)
        if control_document.get("status") != "ok" or control_document.get("failed_modules"):
            print("gate: UBS control scan was partial: %s" % control_path)
            sys.exit(3)
    clean_findings = load_findings(control_clean_path)
    planted_findings = load_findings(control_planted_path)
    if clean_findings is None or planted_findings is None:
        print("gate: UBS delta control findings could not be read")
        sys.exit(3)
    try:
        probe_info = json.load(open(control_probe_path, encoding="utf-8"))
        probe_source = Path(probe_info["source"]).read_text(encoding="utf-8")
        probe_target = Path(probe_info["path"])
        probe_relative = str(probe_target.relative_to(Path(control_probe_path).parent))
        source_overrides = {str(probe_target): probe_source, probe_relative: probe_source,
                            "./" + probe_relative: probe_source}
        probe_lines = [int(probe_info[key]) for key in ("unwrap", "static_sql", "dynamic_sql")]
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print("gate: UBS control source/coordinates could not be read: %s" % exc)
        sys.exit(3)
    clean_excluded, clean_enforced, _, _, _ = classify(clean_findings, test_only)
    planted_excluded, planted_enforced, _, _, planted_sql_excluded = classify(
        planted_findings, test_only, source_overrides)
    clean_blocking = [item for item in clean_enforced if finding_severity(item) in BLOCKING]
    planted_blocking = [item for item in planted_enforced if finding_severity(item) in BLOCKING]
    clean_verdict = 1 if clean_blocking else 0
    planted_verdict = 1 if planted_blocking else 0
    print("UBS_DELTA_CONTROL_VERDICT=clean=%d planted=%d (clean_new=%d planted_new=%d)" % (
        clean_verdict, planted_verdict, len(clean_findings), len(planted_findings)))
    unwrap_probe = [item for item in planted_findings
                    if finding_line(item) == probe_lines[0]
                    and finding_rule(item) == "rust.ownership.unwrap-expect"]
    static_probe = [item for item in planted_findings
                    if finding_line(item) == probe_lines[1] and finding_rule(item) == SQL_CONSTANT_RULE]
    dynamic_probe = [item for item in planted_findings
                     if finding_line(item) == probe_lines[2] and finding_rule(item) == SQL_CONSTANT_RULE]
    if (not unwrap_probe or not static_probe or not dynamic_probe
            or any(item not in planted_enforced for item in unwrap_probe + dynamic_probe)
            or any(item not in planted_sql_excluded for item in static_probe)):
        print("gate: production unwrap/dynamic SQL must stay enforced; only the marked SQL constant may be admitted")
        sys.exit(3)
    print("UBS_DELTA_CONTROL_PROBE=unwrap=%d dynamic_sql=%d enforced; static_sql=%d admitted" % (
        len(unwrap_probe), len(dynamic_probe), len(static_probe)))
    if clean_verdict != 0 or planted_verdict != 1:
        print("gate: the filtered delta must pass on the base file and fail on the planted unwrap")
        sys.exit(3)

if status != "ok" or failed_modules:
    print("gate: UBS delta scan was partial (status=%s failed_modules=%s)" % (status, failed_modules))
    sys.exit(2)
sys.exit(verdict)
PYSUM
    echo "UBS_DELTA_SCANNER_EXIT=${delta_rc}"
    echo "UBS_DELTA_EXIT=${filter_rc}"
    return "$filter_rc"
}

if [ "$VERIFY_UBS" -eq 1 ]; then
    cd "$PROJECT_ROOT" || exit 1
    SOURCE_HEAD="$(git rev-parse HEAD)" || exit 1
    before="$(ubs_source_digest "${VERIFY_UBS_FILES[@]}")" || exit $?
    echo "SOURCE_HEAD=${SOURCE_HEAD} UBS_SOURCE_CONTENT_SHA256=${before}"
    echo "STAGE=source-identity EXIT=0"
    ubs_rc=0
    run_ubs "${VERIFY_UBS_FILES[@]}" || ubs_rc=$?
    echo "STAGE=ubs EXIT=${ubs_rc}"
    after="$(ubs_source_digest "${VERIFY_UBS_FILES[@]}")" || exit 1
    if [ "$(git rev-parse HEAD)" != "$SOURCE_HEAD" ] || [ "$after" != "$before" ]; then
        echo "STAGE=source-stability EXIT=1 (scanner inputs changed during verification)"
        exit 1
    fi
    echo "STAGE=source-stability EXIT=0"
    exit "$ubs_rc"
fi

# Every stage records its own exit code and preserves complete test output.
lib_stage=""
if [ "$RUN_LIB" = 1 ]; then
    # A wedged test (the bet45 class: a producer parked forever at ~0 CPU)
    # would otherwise hold the fleet admission until the job's own ceiling.
    # `timeout` turns that into a loud EXIT=124 on this stage instead. It
    # bounds the test run only: lib-build compiles first, because a loaded
    # worker can spend the whole budget compiling (gate b82: lib-tests EXIT=124
    # before any test ran, after a 72-minute clippy compile).
    LIB_TIMEOUT="${GATE_LIB_TIMEOUT_SECS:-2400}"
    if [[ ! "$LIB_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
        echo "gate: GATE_LIB_TIMEOUT_SECS must be a positive integer" >&2
        exit 2
    fi
    read -r -a filter_words <<< "$LIB_FILTER"
    filter_args=""
    filter_check_args=""
    if [ "${#filter_words[@]}" -gt 0 ]; then
        printf -v filter_args ' %q' "${filter_words[@]}"
        for fw in "${filter_words[@]}"; do
            printf -v fw_q ' --filter %q' "$fw"
            filter_check_args+="$fw_q"
        done
    fi
    lib_stage="cargo test --locked -j ${BUILD_JOBS} --lib --no-run; echo STAGE=lib-build EXIT=\$?; \
run_tests lib-tests${filter_check_args} timeout ${LIB_TIMEOUT} cargo test --locked -j ${BUILD_JOBS} --lib -- --test-threads=${BUILD_JOBS} ${filter_args};"
fi
# Every stage the remote script will run, in order. The receipt check below
# requires each one to report: a job cut short by the fleet's SSH ceiling
# leaves later stages missing, and a missing stage is RED, never green.
EXPECTED_STAGES=(source-identity source-freshness fmt clippy)
[ "$RUN_LIB" = 1 ] && EXPECTED_STAGES+=(lib-build lib-tests)
integration_stage=""
if [ "$RUN_INTEGRATION" = 1 ]; then
    declare -A integration_targets=()
    for entry in "${INTEGRATION_TESTS[@]}"; do
        [ -n "$entry" ] || continue
        t="${entry%%:*}"
        if [[ ! "$t" =~ ^[a-zA-Z0-9_-]+$ ]]; then
            echo "gate: invalid integration target ${t}" >&2
            exit 2
        fi
        if [ -n "${integration_targets[$t]:-}" ]; then
            echo "gate: repeated target ${t}; combine its filters in one target:filter1 filter2 entry" >&2
            exit 2
        fi
        integration_targets["$t"]=1
        filter=""
        if [ "$entry" != "$t" ]; then
            filter="${entry#*:}"
        fi
        read -r -a filter_words <<< "$filter"
        filter_args=""
        filter_check_args=""
        if [ "${#filter_words[@]}" -gt 0 ]; then
            printf -v filter_args ' %q' "${filter_words[@]}"
            for fw in "${filter_words[@]}"; do
                printf -v fw_q ' --filter %q' "$fw"
                filter_check_args+="$fw_q"
            done
        fi
        integration_stage+="run_tests test-${t}${filter_check_args} cargo test --locked -j ${BUILD_JOBS} --test ${t} -- --test-threads=${BUILD_JOBS} ${filter_args};"
        EXPECTED_STAGES+=("test-${t}")
    done
fi
if [ "$RUN_INTEGRATION" = 1 ] && [ "$RUN_BIN" = 1 ]; then
    # The cass bin is `test = false`, so the unit tests of its own modules
    # (logical_archive's backup/restore codec, the CLI modules) ran in no gate;
    # one hung for hours unseen (m5cob). search_service's share runs as
    # --test search_service. Every other skip names the bead whose fix
    # removes it.
    BIN_TIMEOUT="${GATE_BIN_TIMEOUT_SECS:-900}"
    if [[ ! "$BIN_TIMEOUT" =~ ^[1-9][0-9]*$ ]]; then
        echo "gate: GATE_BIN_TIMEOUT_SECS must be a positive integer" >&2
        exit 2
    fi
    BIN_SKIPS=(
        search_service::
    )
    printf -v bin_skip_args ' --skip %q' "${BIN_SKIPS[@]}"
    # As for lib-tests, the timeout bounds the test run, not the compile.
    integration_stage+="cargo test --locked -j ${BUILD_JOBS} --bin cass --no-run; echo STAGE=bin-build EXIT=\$?; \
run_tests bin-cass timeout ${BIN_TIMEOUT} cargo test --locked -j ${BUILD_JOBS} --bin cass -- --test-threads=${BUILD_JOBS}${bin_skip_args};"
    EXPECTED_STAGES+=(bin-build bin-cass)
fi
golden_regen_stage=""
golden_stage=""
if [ "$RUN_GOLDENS" = 1 ]; then
    if [ "$REGEN_GOLDENS" = 1 ]; then
        golden_regen_stage="UPDATE_GOLDENS=1 run_tests goldens-regen cargo test --locked -j ${BUILD_JOBS} --test golden_robot_json --test golden_robot_docs -- --test-threads=${BUILD_JOBS};"
        EXPECTED_STAGES+=(goldens-regen)
    fi
    golden_stage="run_tests goldens cargo test --locked -j ${BUILD_JOBS} --test golden_robot_json --test golden_robot_docs -- --test-threads=${BUILD_JOBS};"
    EXPECTED_STAGES+=(goldens)
fi

docs_truth_stage=""
if [ "$RUN_DOCS_TRUTH" = 1 ]; then
    # Never inspect a pre-existing target binary when the selected tests did
    # not build it. This invocation must establish its source and ELF first.
    printf -v docs_binary '%q' "${TARGET_DIR}/debug/cass"
    docs_truth_stage="cargo build --locked -j ${BUILD_JOBS} --bin cass; build_rc=\$?; echo STAGE=docs-build EXIT=\$build_rc;
if [ \"\$build_rc\" -eq 0 ]; then
    sha256sum ${docs_binary}; echo STAGE=docs-binary-identity EXIT=\$?;
    CASS_BIN=${docs_binary} scripts/validate_docs.sh --truth; echo STAGE=docs-truth EXIT=\$?;
fi;"
    EXPECTED_STAGES+=(docs-build docs-binary-identity docs-truth)
fi

prepare_gate_source || exit 1

REMOTE_SCRIPT="$(declare -f test_log_counts run_tests run_fmt run_ubs ubs_delta_gate prepare_compile_inputs)
set -o pipefail
echo SOURCE_HEAD=${SOURCE_HEAD}
prepare_compile_inputs '${SOURCE_PATHS}' '${SOURCE_CONTENT}' || exit 1
# These native stores live outside HOME. A test that replaces HOME must never
# discover the host's shared transcripts. Individual connector fixtures can
# explicitly override these roots in their own child process.
native_fixture_root=\"\$(mktemp -d -t cass-gate-native.XXXXXX)\" || exit 1
export CASS_CLOUDMCP_DATA_ROOT=\"\${native_fixture_root}/cloudmcp\"
export CASS_OPENWEBUI_DATA_ROOT=\"\${native_fixture_root}/openwebui\"
run_fmt; echo STAGE=fmt EXIT=\$?; \
cargo clippy --locked -j ${BUILD_JOBS} --all-targets -- -D warnings; echo STAGE=clippy EXIT=\$?; \
${lib_stage} ${integration_stage} ${golden_regen_stage} ${golden_stage} ${docs_truth_stage} \
run_ubs ${UBS_FILES_ARG}; echo STAGE=ubs EXIT=\$?; echo STAGE=job-complete EXIT=0"
# Scan after behavioral validation: a slow scanner must not consume the whole
# admission before any product test runs. UBS remains mandatory and blocking.
EXPECTED_STAGES+=(ubs job-complete)

run_once() {
    # Match .cargo/config.toml: storage futures have exceeded a 16 MiB stack.
    # This is virtual address reservation; pages are committed as needed.
    if [ "$LOCAL" = 1 ]; then
        env CARGO_TARGET_DIR="$TARGET_DIR" RUST_MIN_STACK=134217728 UBS_MODULE_TIMEOUT="$UBS_TIMEOUT" bash -c "$REMOTE_SCRIPT"
        return $?
    fi
    # RCH percent-encodes the command into a protocol line capped at 64 KiB.
    # Compress the same script, including its expected source identity, so
    # the transport fits without dropping checks or changing their receipts.
    local payload dispatcher
    payload="$(printf '%s' "$REMOTE_SCRIPT" | gzip -c | base64 -w0)" || return "$?"
    dispatcher="$(declare -f run_encoded_remote_script)
run_encoded_remote_script \"\$1\""
    echo "REMOTE_SCRIPT_BYTES=${#REMOTE_SCRIPT} REMOTE_PAYLOAD_BYTES=${#payload}"
    # Retain cargo in argv[0] so RCH keeps its per-worker Cargo cache isolation.
    RCH_REQUIRE_REMOTE=1 rch exec --job --result-dir tests/golden -- env CARGO_TARGET_DIR="$TARGET_DIR" RUST_MIN_STACK=134217728 UBS_MODULE_TIMEOUT="$UBS_TIMEOUT" bash -c "$dispatcher" cass-cargo-gate "$payload"
}

# The receipt survives a RED run for post-mortem (GATE_RECEIPT_FILE overrides).
receipt_file="${GATE_RECEIPT_FILE:-$(mktemp -t cass-gate.XXXXXX)}"
if [ -s "$receipt_file" ]; then
    echo "gate: refusing to overwrite existing receipt ${receipt_file}" >&2
    exit 2
fi
printf 'SOURCE_HEAD=%s SOURCE_DIFF_SHA256=%s SOURCE_CONTENT_SHA256=%s\n' \
    "$SOURCE_HEAD" "$SOURCE_DIFF" "$SOURCE_CONTENT" >> "$receipt_file"
attempt=1
rc=103
while [ "$attempt" -le "$RETRIES" ]; do
    echo "gate attempt ${attempt}/${RETRIES} $(date +%T)" >&2
    # Both streams: rch relays remote output on whichever stream it chooses,
    # and a receipt that misses the STAGE lines reads as a truncated job.
    attempt_file="$(mktemp "${receipt_file}.attempt-${attempt}.XXXXXX")"
    run_once 2>&1 | tee "$attempt_file"
    pipeline_status=("${PIPESTATUS[@]}")
    rc=${pipeline_status[0]}
    [ "${pipeline_status[1]}" -eq 0 ] || rc=1
    cat "$attempt_file" >> "$receipt_file"
    if [ "$rc" -ne 103 ]; then
        break
    fi
    attempt=$((attempt + 1))
    [ "$attempt" -le "$RETRIES" ] && sleep "$RETRY_SLEEP"
done

if [ "$rc" -eq 103 ]; then
    echo "gate: fleet refused every attempt (exit 103); nothing was verified" >&2
    echo "gate: receipt kept at ${receipt_file}" >&2
    exit 103
fi

echo "---- gate receipt $(date -u +%Y-%m-%dT%H:%M:%SZ) HEAD=${SOURCE_HEAD} DIFF_SHA256=${SOURCE_DIFF} rch_exit=${rc} receipt=${receipt_file} lines=$(wc -l < "$receipt_file") ----"
failed=0
if [ "$(git rev-parse HEAD)" != "$SOURCE_HEAD" ] || \
   [ "$(source_digest)" != "$SOURCE_DIFF" ]; then
    echo "STAGE=source-stability EXIT=1 (checkout changed during validation)"
    failed=1
fi
verify_receipt "$attempt_file" "$rc" "${EXPECTED_STAGES[@]}" || failed=1
if [ "$failed" -ne 0 ]; then
    echo "gate: RED (receipt kept at ${receipt_file})" >&2
    exit 1
fi
echo "gate: GREEN (receipt kept at ${receipt_file})"
