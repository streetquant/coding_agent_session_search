#!/usr/bin/env python3
"""Conserve a retained UBS aggregate report without executing a scanner.

Native aggregate counts and sample arrays are different evidence. This collector
keeps every emitted sample, including unparseable ones, and verifies coordinates
only against explicitly pinned source files. It never reconstructs omitted
matches, deduplicates emissions, infers columns, or clears a qualification gate.
The original report bytes remain identified by their SHA-256 and byte length;
the complete native JSON value, including unknown fields, is also retained.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import sys
from pathlib import Path
from typing import Any


SEVERITIES = ("critical", "warning", "info")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
ANNOTATED_3_0_1_RULES = {
    "Security token generated with non-cryptographic randomness":
        re.compile(r"[^\r\n]+ in security-sensitive generation context"),
    "Request-derived URL reaches outbound HTTP client":
        re.compile(r"[^\r\n]+ -> outbound HTTP"),
    "Interpolated SQL reaches execution sink":
        re.compile(r"[^\r\n]+ -> SQL execution"),
}


class ReportError(ValueError):
    """The retained inputs cannot be interpreted without losing evidence."""


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def native_json(data: bytes) -> Any:
    def unique_keys(items: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in items:
            if key in result:
                raise ReportError(f"duplicate native JSON key: {key}")
            result[key] = value
        return result

    def nonfinite(value: str) -> Any:
        raise ReportError(f"nonfinite native JSON number: {value}")

    def finite_float(value: str) -> float:
        number = float(value)
        if not math.isfinite(number):
            nonfinite(value)
        return number

    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=unique_keys,
                          parse_constant=nonfinite, parse_float=finite_float)
    except ReportError:
        raise
    except (UnicodeError, json.JSONDecodeError, ValueError, RecursionError) as error:
        raise ReportError("native report is not complete UTF-8 JSON") from error


def count(value: Any, label: str) -> int:
    if type(value) is not int or value < 0:  # ubs:ignore[py.comparison.type-equality] JSON counts reject bool, an int subclass.
        raise ReportError(f"{label} must be a nonnegative integer")
    return value


def pinned_sources(root: Path, pins: dict[str, str]) -> tuple[dict[str, Any], list[str]]:
    if not isinstance(pins, dict):
        raise ReportError("explicit source pins must be an object")
    sources: dict[str, Any] = {}
    errors: list[str] = []
    for relative, expected in pins.items():
        if not isinstance(relative, str) or not relative:
            raise ReportError("source pins require nonempty relative paths")
        try:
            relative.encode("utf-8")
        except UnicodeError as error:
            raise ReportError("source pin is not a UTF-8 path") from error
        if "\0" in relative:
            raise ReportError("source pin contains a null byte")
        parts = Path(relative).parts
        if not parts or Path(relative).is_absolute() or ".." in parts or relative != Path(relative).as_posix():
            raise ReportError(f"source pin escapes or is not canonical: {relative}")
        if not isinstance(expected, str) or SHA256.fullmatch(expected) is None:
            raise ReportError(f"source pin has no SHA-256: {relative}")
        path = root.joinpath(relative)
        current = root
        for part in parts:
            current /= part
            if current.is_symlink():
                raise ReportError(f"source pin follows a symlink: {relative}")
        try:
            data = path.read_bytes()
        except OSError:
            errors.append(f"pinned source is unavailable: {relative}")
            continue
        actual = digest(data)
        lines = data.split(b"\n")
        if lines[-1:] == [b""]:
            lines.pop()
        sources[relative] = {
            "absolute_path": str(path),
            "expected_sha256": expected,
            "actual_sha256": actual,
            "pin_matches": actual == expected,  # ubs:ignore[python.ctcompare.secret_eq] Public SHA-256 source-integrity pins, not secrets.
            "lines": lines,
        }
        if actual != expected:  # ubs:ignore[python.ctcompare.secret_eq] Public SHA-256 source-integrity pins, not secrets.
            errors.append(f"pinned source bytes changed: {relative}")
    return sources, errors


def sample_record(sample: Any, pointer: str, severity: str, sources: dict[str, Any],
                  annotation_pattern: re.Pattern[str] | None = None) -> dict[str, Any]:
    record: dict[str, Any] = {
        "native_json_pointer": pointer,
        "raw_severity": severity,
        "raw_sample": sample,
        "native_column": None,
        "source_verified": False,
        "address_state": "unsupported-native-sample",
    }
    if not isinstance(sample, str):
        return record
    try:
        record["raw_sample_utf8_sha256"] = digest(sample.encode("utf-8"))
    except UnicodeError:
        record["address_state"] = "non-utf8-native-sample"
        return record
    # Match a known path before parsing the line suffix. A greedy file:line
    # regular expression can consume numeric colon fragments in source code or
    # a filename. Longer pinned names take precedence without probing new paths.
    matches = sorted(sources.items(), key=lambda item: len(item[1]["absolute_path"]), reverse=True)
    for relative, source in matches:
        prefix = source["absolute_path"] + ":"
        if not sample.startswith(prefix):
            continue
        suffix = sample[len(prefix):]
        match = re.fullmatch(r"([0-9]+):(.*)", suffix, re.DOTALL)
        try:
            line_number = int(match.group(1)) if match is not None else 0
        except ValueError:
            line_number = 0
        if line_number < 1:
            record["address_state"] = "malformed-native-address"
            return record
        record.update({
            "relative_path": relative,
            "native_file": source["absolute_path"],
            "native_line": line_number,
            "native_code": match.group(2),
            "source_sha256": source["actual_sha256"],
            "expected_source_sha256": source["expected_sha256"],
            "source_pin_matches": source["pin_matches"],
            "address_state": "parsed-native-sample",
            "line_in_range": line_number <= len(source["lines"]),
        })
        if not record["line_in_range"]:
            record["address_state"] = "native-line-out-of-range"
            return record
        physical_line = source["lines"][line_number - 1]
        record["source_line_sha256"] = digest(physical_line)
        try:
            text = physical_line.decode("utf-8")
        except UnicodeError:
            record["address_state"] = "non-utf8-pinned-source-line"
            return record
        # Native AST sample helpers replace tabs with two spaces; other native
        # helpers trim source lines. Keep raw bytes and report that comparison
        # explicitly. LF alone defines physical lines, so embedded CR/FF bytes
        # never shift the native coordinate or silently disappear from evidence.
        code = record["native_code"]
        record["native_code_matches_line_exactly"] = code == text
        record["native_code_matches_line_after_trim"] = code.strip() in (
            text.strip(), text.replace("\t", "  ").strip(),
        )
        record["native_annotation"] = None
        annotation_matches_source = False
        if annotation_pattern is not None:
            # These three pinned Rust 3.0.1 emitters append diagnostic text to
            # source_line(...). Do not strip arbitrary bracket suffixes: only
            # accept their rule-specific grammar after a complete pinned line.
            variants = {text.strip(), text.strip().replace("\t", " ")}
            for source_code in sorted(variants, key=len, reverse=True):
                prefix = source_code + "  ["
                if not code.startswith(prefix) or not code.endswith("]"):
                    continue
                annotation = code[len(prefix):-1]
                if annotation_pattern.fullmatch(annotation) is not None:
                    record["native_annotation"] = annotation
                    record["native_annotation_format"] = "ubs-rust-3.0.1-rule-specific"
                    record["native_source_code"] = source_code
                    annotation_matches_source = True
                    break
        record["native_annotation_matches_pinned_source_prefix"] = annotation_matches_source
        record["source_verified"] = (
            source["pin_matches"] and (
                record["native_code_matches_line_after_trim"] or annotation_matches_source
            )
        )
        if not record["source_verified"]:
            record["address_state"] = "native-sample-source-mismatch"
        return record
    record["address_state"] = "address-not-in-explicit-source-pins"
    return record


def collect_report(
    report_bytes: bytes,
    source_root: Path,
    pins: dict[str, str],
    native_exit_code: int,
    expected_report_sha256: str,
) -> dict[str, Any]:
    document = native_json(report_bytes)
    if not isinstance(document, dict) or not isinstance(document.get("findings"), list):
        raise ReportError("native report requires a findings array")
    if not isinstance(source_root, Path) or source_root.is_symlink():
        raise ReportError("source root must be a physical directory")
    root = source_root.resolve(strict=True)
    if not root.is_dir() or root != source_root:
        raise ReportError("source root must be a physical directory")
    meta = document.get("meta")
    if not isinstance(meta, dict) or meta.get("project_dir") != str(root):
        raise ReportError("native report and exact source root do not match")
    if type(native_exit_code) is not int:  # ubs:ignore[py.comparison.type-equality] Process-status integers reject bool, an int subclass.
        raise ReportError("native process exit code must be an integer")
    if not isinstance(expected_report_sha256, str) or SHA256.fullmatch(expected_report_sha256) is None:
        raise ReportError("native receipt requires the retained report SHA-256")
    summary = document.get("summary")
    if not isinstance(summary, dict):
        raise ReportError("native report requires a summary object")
    declared = {severity: count(summary.get(severity), f"summary.{severity}") for severity in SEVERITIES}
    files = count(summary.get("files"), "summary.files")
    sources, errors = pinned_sources(root, pins)
    if digest(report_bytes) != expected_report_sha256:  # ubs:ignore[python.ctcompare.secret_eq] Public SHA-256 receipt-integrity digest, not a secret.
        errors.append("native report bytes differ from the retained process receipt")
    if files != len(pins):
        errors.append("native file count differs from the explicit source pin set")
    aggregates = dict.fromkeys(SEVERITIES, 0)
    emitted = dict.fromkeys(SEVERITIES, 0)
    verified = dict.fromkeys(SEVERITIES, 0)
    unrepresented = dict.fromkeys(SEVERITIES, 0)
    records: list[dict[str, Any]] = []
    groups: list[dict[str, Any]] = []
    for index, finding in enumerate(document["findings"]):
        pointer = f"/findings/{index}"
        if not isinstance(finding, dict) or finding.get("severity") not in SEVERITIES:
            raise ReportError(f"{pointer} has an unsupported aggregate shape or severity")
        severity = finding["severity"]
        units = count(finding.get("count"), f"{pointer}/count")
        samples = finding.get("samples")
        if not isinstance(samples, list):
            raise ReportError(f"{pointer}/samples must be an array")
        aggregates[severity] += units
        emitted[severity] += len(samples)
        unrepresented[severity] += max(0, units - len(samples))
        if len(samples) > units:
            errors.append(f"{pointer} emits more samples than declared count units")
        start = len(records)
        annotation_pattern = None
        title = finding.get("title")
        if (meta.get("version") == "3.0.1" and severity == "critical"
                and finding.get("category") == "Security Findings" and isinstance(title, str)):
            annotation_pattern = ANNOTATED_3_0_1_RULES.get(title)
        for number, sample in enumerate(samples):
            record = sample_record(sample, f"{pointer}/samples/{number}", severity, sources,
                                   annotation_pattern)
            records.append(record)
            verified[severity] += int(record["source_verified"])
        groups.append({
            "native_json_pointer": pointer,
            "raw_severity": severity,
            "raw_count_units": units,
            "raw_category": finding.get("category"),
            "raw_title": finding.get("title"),
            "emitted_sample_records": len(samples),
            "unrepresented_count_units": max(0, units - len(samples)),
            "record_start": start,
            "record_end": len(records),
            "count_to_sample_identity_correspondence_proven": False,
        })
    if aggregates != declared:
        errors.append("native group counts do not conserve the native summary")
    if sum(verified.values()) != len(records):
        errors.append("one or more emitted samples have no verified pinned source coordinate")
    nonempty = bool(records) and bool(pins)
    if not nonempty:
        errors.append("empty emitted capture does not qualify source or helper coverage")
    return {
        "schema": "cass.offline-ubs-emission-collection.v1",
        "raw_report_sha256": digest(report_bytes),
        "raw_report_bytes": len(report_bytes),
        "expected_raw_report_sha256": expected_report_sha256,
        "native_document": document,
        "native_exit_code": native_exit_code,
        "source_root": str(root),
        "native_files": files,
        "explicit_source_pins": pins,
        "native_summary_counts": declared,
        "aggregate_group_counts": aggregates,
        "emitted_sample_counts": emitted,
        "verified_emitted_sample_counts": verified,
        "unrepresented_count_units": unrepresented,
        "groups": groups,
        "records": records,
        "all_emitted_samples_retained": len(records) == sum(emitted.values()),
        "emission_collection_integrity": "verified" if not errors else "incomplete",
        "collection_errors": errors,
        "per_helper_native_completion_proven": False,
        "complete_native_count_site_inventory": False,
        "native_membership_credit_for_unemitted_candidates": 0,
        "qualification_gate": "HOLD",
        "qualifies_native_gate": False,
        "limitations": [
            "Aggregate counts are retained native count units, not reconstructed addresses.",
            "Sample records preserve emission order and duplicates; columns are not emitted by this report format.",
            "Capped native samples do not prove per-count-unit identity or helper completion.",
            "Known Rust 3.0.1 annotations are retained separately from exact pinned source prefixes; they prove no dataflow or semantic disposition.",
            "This collector performs no scanner, CASS, Cargo, import, install, or semantic disposition.",
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--run-receipt", required=True, type=Path)
    parser.add_argument("--source-root", required=True, type=Path)
    args = parser.parse_args()
    try:
        receipt = native_json(args.run_receipt.read_bytes())
        if not isinstance(receipt, dict) or not isinstance(receipt.get("source_checks_after"), list):
            raise ReportError("native run receipt requires source_checks_after")
        pins: dict[str, str] = {}
        for entry in receipt["source_checks_after"]:
            if not isinstance(entry, dict) or not isinstance(entry.get("path"), str):
                raise ReportError("native run receipt has a malformed source pin")
            relative = entry["path"]
            if relative in pins:
                raise ReportError("native run receipt has duplicate source pins")
            expected = entry.get("expected_sha256")
            if expected != entry.get("current_sha256") or expected != entry.get("frozen_sha256"):
                raise ReportError("native receipt does not conserve frozen/current source bytes")
            pins[relative] = expected
        outputs = receipt.get("outputs")
        if not isinstance(outputs, dict) or not isinstance(outputs.get("findings.json"), dict):
            raise ReportError("native run receipt requires the retained report output pin")
        report_pin = outputs["findings.json"].get("sha256")
        result = collect_report(
            args.report.read_bytes(), args.source_root, pins, receipt.get("exit_code"), report_pin,
        )
    except (OSError, ReportError) as error:
        print(json.dumps({"schema": "cass.offline-ubs-emission-collection-error.v1", "error": str(error),
                          "qualification_gate": "HOLD", "qualifies_native_gate": False}))
        return 2
    print(json.dumps(result, ensure_ascii=True, sort_keys=True, indent=2))
    # Valid offline collection is not native qualification. In particular a
    # nonempty set of addresses cannot hide raw exit 1 or omitted count units.
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
