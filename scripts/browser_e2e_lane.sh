#!/usr/bin/env bash
# Browser acceptance lane adapted from upstream640ac09cf (browser-e2e-lane-bnueb).
# Run on a dedicated dsr build host, never Max, ts1, or a shared development host.
# Usage: LANE_DSR_HOST=<actual-hostname> scripts/browser_e2e_lane.sh
#        [--project chromium|firefox|webkit] [--skip-build] [SPEC ...]
# Use a clean checkout of the commit under test. Skipped/flaky tests are recorded
# separately, and missing required positive/negative cases fail qualification.
set -euo pipefail

repo="${LANE_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
projects=()
specs=()
build=1
while [ $# -gt 0 ]; do
    case "$1" in
        --project)
            [ $# -ge 2 ] || { echo "browser lane: --project needs a value" >&2; exit 2; }
            case "$2" in chromium|firefox|webkit) projects+=("$2") ;; *) echo "browser lane: unsupported project $2" >&2; exit 2 ;; esac
            shift 2 ;;
        --skip-build) build=0; shift ;;
        -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
        --*) echo "browser lane: unknown option $1" >&2; exit 2 ;;
        *) specs+=("$1"); shift ;;
    esac
done
host="$(hostname)"
case "$host" in max|max.*|ts1|ts1.*) echo "browser lane: shared development host refused: $host" >&2; exit 2 ;; esac
[ -n "${LANE_DSR_HOST:-}" ] && [ "$LANE_DSR_HOST" = "$host" ] || {
    echo "browser lane: set LANE_DSR_HOST to the verified dedicated dsr host's hostname" >&2
    exit 2
}
git -C "$repo" diff --quiet HEAD -- || { echo "browser lane: use a clean checkout of the candidate commit" >&2; exit 2; }
commit="$(git -C "$repo" rev-parse HEAD)"
[ ${#projects[@]} -gt 0 ] || projects=(chromium)
[ ${#specs[@]} -gt 0 ] || specs=(
    e2e/encryption/password-flow.spec.ts
    e2e/export/html-export-e2e.spec.ts
    e2e/export/pages-pipeline-e2e.spec.ts
    e2e/offline/cdn-fallback.spec.ts
)
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}"
receipt="${LANE_RECEIPT:-$repo/tests/e2e-lane-receipt.json}"
[ ! -e "$receipt" ] || { echo "browser lane: preserve the existing receipt; choose a fresh LANE_RECEIPT" >&2; exit 2; }
exe=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) exe=".exe" ;; esac
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }
if [ "$build" = 1 ]; then
    (cd "$repo" && cargo build --locked --release --bin cass --bin cass-pages-perf-bundle)
fi
cass="$CARGO_TARGET_DIR/release/cass$exe"
bundle="$CARGO_TARGET_DIR/release/cass-pages-perf-bundle$exe"
[ -x "$cass" ] && [ -x "$bundle" ] || { echo "browser lane: both candidate binaries are required" >&2; exit 2; }
cass_sha="$(sha256 "$cass")"
bundle_sha="$(sha256 "$bundle")"
build_commit="$("$cass" capabilities --json | node --input-type=module -e 'let s=""; for await (const c of process.stdin) s+=c; console.log(JSON.parse(s).build_commit);')"
[ "$build_commit" = "${commit:0:12}" ] || { echo "browser lane: candidate build commit differs from checkout" >&2; exit 2; }

cd "$repo/tests" || exit 2
[ -d node_modules/@playwright/test ] || npm ci --no-audit --no-fund
project_args=()
for project in "${projects[@]}"; do project_args+=("--project=$project"); done
npx playwright install "${projects[@]}"
set +e
E2E_SKIP_REGENERATE=0 npx playwright test "${project_args[@]}" "${specs[@]}"
status=$?
set -e
browser_versions="$(node --input-type=module - "${projects[@]}" <<'BROWSERS'
import { chromium, firefox, webkit } from "playwright-core";
const out = {};
for (const name of process.argv.slice(2)) {
  try {
    const browser = await ({ chromium, firefox, webkit })[name].launch();
    out[name] = browser.version();
    await browser.close();
  } catch { out[name] = null; }
}
console.log(JSON.stringify(out));
BROWSERS
)"
LANE_STATUS="$status" LANE_HOST="$host" LANE_OS="$(uname -srm)" \
LANE_COMMIT="$commit" LANE_COMMIT_AFTER="$(git -C "$repo" rev-parse HEAD)" \
LANE_DIRTY_AFTER="$(git -C "$repo" status --porcelain --untracked-files=no | wc -l | tr -d ' ')" \
LANE_CASS_SHA="$cass_sha" LANE_CASS_SHA_AFTER="$(sha256 "$cass")" \
LANE_BUNDLE_SHA="$bundle_sha" LANE_BUNDLE_SHA_AFTER="$(sha256 "$bundle")" \
LANE_BUILD_COMMIT="$build_commit" LANE_PLAYWRIGHT="$(npx playwright --version)" \
LANE_BROWSERS="$browser_versions" LANE_PROJECTS="${projects[*]}" \
LANE_SPECS="${specs[*]}" LANE_RECEIPT_PATH="$receipt" node --input-type=module <<'RECEIPT'
import { readFileSync, writeFileSync } from "fs";
import path from "path";
const report = JSON.parse(readFileSync("e2e-results.json", "utf8"));
const specs = {};
const tests = [];
function walk(suite) {
  for (const spec of suite.specs ?? []) {
    // Playwright reports locations relative to config.rootDir (tests/e2e).
    const file = path.relative(process.cwd(), path.resolve(report.config.rootDir, spec.file)).split(path.sep).join("/");
    const row = (specs[file] ??= { passed: 0, failed: 0, skipped: 0, flaky: 0, not_run: 0 });
    for (const test of spec.tests ?? []) {
      const last = test.results?.[test.results.length - 1];
      const status = !last ? "not_run"
        : test.status === "skipped" || last.status === "skipped" ? "skipped"
        : test.status === "flaky" ? "flaky"
        : test.status === "expected" && last.status === "passed" ? "passed" : "failed";
      row[status]++;
      tests.push({ file, title: spec.title, project: test.projectName, status,
        annotations: test.annotations ?? [], errors: last?.errors ?? [] });
    }
  }
  for (const child of suite.suites ?? []) walk(child);
}
for (const suite of report.suites ?? []) walk(suite);
const env = process.env;
const projects = env.LANE_PROJECTS.split(" ");
const requested = env.LANE_SPECS.split(" ");
const required = {
  "e2e/encryption/password-flow.spec.ts": ["no plaintext title, metadata or messages before unlock", "shows error with wrong password"],
  "e2e/export/pages-pipeline-e2e.spec.ts": ["unlocks with the archive password", "unlocks with the typed recovery key", "refuses a revoked password"],
  "e2e/offline/cdn-fallback.spec.ts": ["fallback classes and legible content survive CDN failure"],
};
const failures = [];
for (const file of requested) {
  for (const project of projects) {
    const rows = tests.filter((t) => t.file === file && t.project === project);
    if (!rows.some((t) => t.status === "passed")) failures.push(`${project}: ${file} has no passing tests`);
    for (const title of required[file] ?? []) {
      if (!rows.some((t) => t.title === title && t.status === "passed")) failures.push(`${project}: required case did not pass: ${title}`);
    }
  }
}
if (env.LANE_COMMIT !== env.LANE_COMMIT_AFTER || Number(env.LANE_DIRTY_AFTER) !== 0) failures.push("candidate source changed during the run");
if (env.LANE_CASS_SHA !== env.LANE_CASS_SHA_AFTER || env.LANE_BUNDLE_SHA !== env.LANE_BUNDLE_SHA_AFTER) failures.push("candidate executable changed during the run");
if (Object.values(specs).some((r) => r.failed || r.flaky || r.not_run)) failures.push("failed, flaky or unexecuted tests remain");
if (projects.some((name) => !JSON.parse(env.LANE_BROWSERS)[name])) failures.push("selected browser version is unavailable");
if (Number(env.LANE_STATUS) !== 0) failures.push(`Playwright exited ${env.LANE_STATUS}`);
const receipt = {
  lane: "browser-e2e-lane-bnueb", generated_at: new Date().toISOString(),
  host: env.LANE_HOST, os: env.LANE_OS, cass_commit: env.LANE_COMMIT,
  binary_build_commit: env.LANE_BUILD_COMMIT, cass_sha256: env.LANE_CASS_SHA,
  bundle_helper_sha256: env.LANE_BUNDLE_SHA, playwright: env.LANE_PLAYWRIGHT,
  browsers: JSON.parse(env.LANE_BROWSERS), projects, requested_specs: requested,
  playwright_exit: Number(env.LANE_STATUS), specs, tests, failures,
  status: failures.length ? "failed" : "passed",
};
writeFileSync(env.LANE_RECEIPT_PATH, JSON.stringify(receipt, null, 2) + "\n", { flag: "wx" });
console.log(`LANE receipt=${env.LANE_RECEIPT_PATH} status=${receipt.status} cass_sha256=${env.LANE_CASS_SHA}`);
process.exitCode = failures.length ? 1 : 0;
RECEIPT
