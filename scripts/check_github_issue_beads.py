#!/usr/bin/env python3
"""Audit that every upstream GitHub issue is represented in the local bead ledger.

The ledger rule is one bead per GitHub issue (AGENTS.md: a GitHub issue gets a
`gh<n>-<topic>` bead within 24 h and the first triage comment cites it).  The
upstream repository closes issues independently, so this audit answers two
questions from the committed JSONL, which is the ledger source of truth:

* is every OPEN upstream issue tracked by at least one non-closed bead?
* is every CLOSED upstream issue referenced by at least one bead?

A closed upstream issue that still has an open bead is reported as
informational: the local record can deliberately cover follow-up work that the
upstream issue's closure does not finish.

Usage:
    scripts/check_github_issue_beads.py
    scripts/check_github_issue_beads.py --repo owner/name --json
    scripts/check_github_issue_beads.py --gh-json /tmp/issues.json

Exit codes: 0 clean, 1 coverage violations, 2 input/tool failure.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

SCHEMA = "cass.gh-issue-bead-coverage.v1"
DEFAULT_REPO = "Dicklesworthstone/coding_agent_session_search"
DEFAULT_LEDGER = Path(".beads/issues.jsonl")
ISSUE_REF_RES = (
    re.compile(r"(?i)\bgh\s*#?\s*(\d{1,6})\b"),
    re.compile(r"(?i)\bgithub\s*#?\s*(\d{1,6})\b"),
    re.compile(r"github\.com/[^/\s]+/[^/\s]+/issues/(\d{1,6})"),
)


def references(record: dict) -> set[int]:
    text = " ".join(
        str(record.get(key) or "") for key in ("id", "title", "description", "external_ref")
    )
    return {int(match.group(1)) for pattern in ISSUE_REF_RES for match in pattern.finditer(text)}


def load_ledger(path: Path) -> list[dict]:
    records = []
    with path.open(encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, 1):
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise SystemExit(f"{path}:{line_number}: invalid JSON: {error}")
            if not isinstance(record, dict) or "id" not in record:
                raise SystemExit(f"{path}:{line_number}: not a bead record")
            records.append(record)
    return records


def fetch_issues(repo: str, state: str, limit: int) -> list[dict]:
    command = [
        "gh",
        "issue",
        "list",
        "--repo",
        repo,
        "--state",
        state,
        "--limit",
        str(limit),
        "--json",
        "number,title,state,closedAt,url",
    ]
    try:
        result = subprocess.run(command, capture_output=True, text=True, check=True)
    except FileNotFoundError:
        raise SystemExit("gh is not installed")
    except subprocess.CalledProcessError as error:
        raise SystemExit(f"gh issue list failed ({error.returncode}): {error.stderr.strip()}")
    issues = json.loads(result.stdout or "[]")
    if not isinstance(issues, list):
        raise SystemExit("gh issue list returned a non-list payload")
    return issues


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=DEFAULT_REPO, help=f"GitHub repository (default: {DEFAULT_REPO})")
    parser.add_argument("--ledger", type=Path, default=DEFAULT_LEDGER, help="ledger JSONL path")
    parser.add_argument("--gh-json", type=Path, help="read `gh issue list --state all --json ...` output instead of calling gh")
    parser.add_argument("--limit", type=int, default=5000, help="maximum issues to fetch from gh")
    parser.add_argument("--json", action="store_true", help="emit machine-readable JSON")
    args = parser.parse_args()

    records = load_ledger(args.ledger)
    by_number: dict[int, list[dict]] = {}
    for record in records:
        for number in references(record):
            by_number.setdefault(number, []).append(record)

    if args.gh_json:
        issues = json.loads(args.gh_json.read_text(encoding="utf-8"))
    else:
        issues = fetch_issues(args.repo, "all", args.limit)

    open_unbeaded, closed_unbeaded, closed_with_open_beads = [], [], []
    upstream_open = upstream_closed = 0
    for issue in issues:
        number = int(issue["number"])
        state = str(issue.get("state", "")).upper()
        if state not in ("OPEN", "CLOSED"):
            continue
        tracking = by_number.get(number, [])
        live = [record for record in tracking if record.get("status") != "closed"]
        if state == "OPEN":
            upstream_open += 1
            if not live:
                open_unbeaded.append({"number": number, "title": issue.get("title", ""), "url": issue.get("url", ""), "beads": [record["id"] for record in tracking]})
        else:
            upstream_closed += 1
            if not tracking:
                closed_unbeaded.append({"number": number, "title": issue.get("title", ""), "url": issue.get("url", ""), "closed_at": issue.get("closedAt")})
            elif live:
                closed_with_open_beads.append({"number": number, "title": issue.get("title", ""), "open_beads": [{"id": record["id"], "status": record.get("status")} for record in live]})

    violations = len(open_unbeaded) + len(closed_unbeaded)
    payload = {
        "schema": SCHEMA,
        "repo": args.repo,
        "ledger": str(args.ledger),
        "upstream": {"open": upstream_open, "closed": upstream_closed},
        "beads": len(records),
        "violations": {
            "open_unbeaded": open_unbeaded,
            "closed_unbeaded": closed_unbeaded,
        },
        "informational": {"closed_with_open_beads": closed_with_open_beads},
    }

    if args.json:
        print(json.dumps(payload, indent=2, sort_keys=False))
    else:
        print(f"repo={args.repo} ledger={args.ledger} beads={len(records)}")
        print(f"upstream open={upstream_open} closed={upstream_closed}")
        print(f"open upstream issues with no non-closed bead: {len(open_unbeaded)}")
        for item in open_unbeaded:
            print(f"  #{item['number']} {item['title']}")
        print(f"closed upstream issues with no bead: {len(closed_unbeaded)}")
        for item in closed_unbeaded[:50]:
            print(f"  #{item['number']} {item['title']}")
        if len(closed_unbeaded) > 50:
            print(f"  ... {len(closed_unbeaded) - 50} more (use --json)")
        print(f"closed upstream issues still carrying an open bead: {len(closed_with_open_beads)}")
        for item in closed_with_open_beads:
            beads = ", ".join(f"{bead['id']}[{bead['status']}]" for bead in item["open_beads"])
            print(f"  #{item['number']} {beads}")
        print("VERDICT:", "clean" if violations == 0 else f"{violations} coverage violation(s)")
    return 0 if violations == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
