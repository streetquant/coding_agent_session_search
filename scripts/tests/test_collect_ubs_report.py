#!/usr/bin/env python3
"""Offline collector controls; fixtures stay in an explicitly owned directory.

Run with CASS_COLLECTOR_TEST_ROOT pointing to a private qualification directory.
The tests neither invoke UBS/CASS/Cargo nor remove their retained inputs.
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import unittest
import uuid
from pathlib import Path
from unittest import mock


MODULE_PATH = Path(__file__).resolve().parents[1] / "collect_ubs_report.py"
SPEC = importlib.util.spec_from_file_location("cass_offline_ubs_collector", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("collector module cannot be loaded")
COLLECTOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(COLLECTOR)


class CollectorControls(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        owned = os.environ.get("CASS_COLLECTOR_TEST_ROOT")
        if not owned:
            raise RuntimeError("CASS_COLLECTOR_TEST_ROOT must name an owned physical directory")
        cls.owned_root = Path(owned)
        if not cls.owned_root.is_dir() or cls.owned_root.resolve() != cls.owned_root:
            raise RuntimeError("fixture root must already exist and be physical")

    def setUp(self) -> None:
        self.root = self.owned_root / f"{self._testMethodName}-{uuid.uuid4().hex}"
        self.root.mkdir()
        self.relative = "src/adapter:42.rs"
        self.source = self.root / self.relative
        self.source.parent.mkdir()
        self.source.write_bytes(b"  visible call(42:7)\nsecond\n")
        self.pins = {self.relative: COLLECTOR.digest(self.source.read_bytes())}
        self.sample = f"{self.source}:1:visible call(42:7)"

    def report(self, samples=None, units: int = 1) -> dict:
        if samples is None:
            samples = [self.sample]
        return {
            "meta": {"project_dir": str(self.root), "module": "fixture-only"},
            "summary": {"files": len(self.pins), "critical": units, "warning": 0, "info": 0},
            "findings": [{"severity": "critical", "count": units, "samples": samples,
                          "category": "public-fixture", "title": "synthetic native group"}],
        }

    def encoded(self, report: dict) -> bytes:
        return json.dumps(report, ensure_ascii=True, separators=(",", ":")).encode("utf-8")

    def collect(self, report=None, native_exit_code: int = 1, expected_hash=None) -> dict:
        payload = self.encoded(self.report() if report is None else report)
        return COLLECTOR.collect_report(payload, self.root, self.pins, native_exit_code,
                                        expected_hash or COLLECTOR.digest(payload))

    def receipt(self, payload: bytes) -> dict:
        return {
            "exit_code": 1,
            "source_checks_after": [
                {"path": path, "expected_sha256": pin, "current_sha256": pin, "frozen_sha256": pin}
                for path, pin in self.pins.items()
            ],
            "outputs": {"findings.json": {"sha256": COLLECTOR.digest(payload)}},
        }

    def run_cli(self, report: dict, receipt=None) -> subprocess.CompletedProcess:
        payload = self.encoded(report)
        cli_inputs = self.root / f"cli-inputs-{uuid.uuid4().hex}"
        cli_inputs.mkdir()
        report_path = cli_inputs / "retained-report.json"
        receipt_path = cli_inputs / "retained-run-receipt.json"
        report_path.write_bytes(payload)
        receipt_path.write_bytes(self.encoded(self.receipt(payload) if receipt is None else receipt))
        return subprocess.run(
            [sys.executable, "-B", str(MODULE_PATH), "--report", str(report_path),
             "--run-receipt", str(receipt_path), "--source-root", str(self.root)],
            capture_output=True, text=True, check=False, timeout=30,
        )

    def test_group_order_duplicate_emissions_and_unknown_fields_are_retained(self) -> None:
        report = self.report([self.sample, self.sample], units=4)
        report["unknown_native_metadata"] = {"nested": ["kept", 7, False]}
        report["findings"][0]["future_rule_field"] = ["preserved"]
        result = self.collect(report)
        self.assertEqual(result["native_document"], report)
        self.assertEqual([record["raw_sample"] for record in result["records"]], [self.sample] * 2)
        self.assertEqual([record["native_json_pointer"] for record in result["records"]],
                         ["/findings/0/samples/0", "/findings/0/samples/1"])
        self.assertEqual(result["groups"][0]["record_start"], 0)
        self.assertEqual(result["groups"][0]["record_end"], 2)
        self.assertEqual(result["unrepresented_count_units"]["critical"], 2)
        self.assertTrue(result["all_emitted_samples_retained"])

    def test_capped_group_preserves_missing_units_without_invented_sites(self) -> None:
        result = self.collect(self.report(units=890))
        self.assertEqual(result["native_summary_counts"]["critical"], 890)
        self.assertEqual(result["unrepresented_count_units"]["critical"], 889)
        self.assertEqual(len(result["records"]), 1)
        self.assertEqual(result["native_membership_credit_for_unemitted_candidates"], 0)
        self.assertFalse(result["complete_native_count_site_inventory"])
        self.assertFalse(result["per_helper_native_completion_proven"])
        self.assertFalse(result["groups"][0]["count_to_sample_identity_correspondence_proven"])
        self.assertEqual(result["qualification_gate"], "HOLD")

    def test_all_samples_and_zero_native_exit_do_not_clear_qualification(self) -> None:
        result = self.collect(native_exit_code=0)
        self.assertEqual(result["native_exit_code"], 0)
        self.assertEqual(result["emission_collection_integrity"], "verified")
        self.assertEqual(result["unrepresented_count_units"]["critical"], 0)
        self.assertFalse(result["qualifies_native_gate"])
        self.assertEqual(result["qualification_gate"], "HOLD")

    def test_native_exit_one_and_raw_byte_identity_remain_visible(self) -> None:
        report = self.report()
        payload = self.encoded(report)
        result = self.collect(report)
        self.assertEqual(result["native_exit_code"], 1)
        self.assertEqual(result["raw_report_sha256"], COLLECTOR.digest(payload))
        self.assertEqual(result["raw_report_bytes"], len(payload))
        self.assertEqual(result["raw_report_sha256"], result["expected_raw_report_sha256"])

    def test_empty_capture_is_incomplete_even_with_zero_counts(self) -> None:
        result = self.collect(self.report([], units=0))
        self.assertEqual(result["emission_collection_integrity"], "incomplete")
        self.assertIn("empty emitted capture does not qualify source or helper coverage",
                      result["collection_errors"])
        self.assertFalse(result["qualifies_native_gate"])

    def test_nonstring_and_unpaired_unicode_samples_are_preserved_unverified(self) -> None:
        samples = [None, {"opaque": "sample"}, ["array"], 8, "\ud800"]
        result = self.collect(self.report(samples, units=len(samples)))
        self.assertEqual([record["raw_sample"] for record in result["records"]], samples)
        self.assertEqual(result["emitted_sample_counts"]["critical"], len(samples))
        self.assertEqual(result["verified_emitted_sample_counts"]["critical"], 0)
        self.assertEqual(result["records"][-1]["address_state"], "non-utf8-native-sample")
        self.assertEqual(result["emission_collection_integrity"], "incomplete")

    def test_unknown_sample_path_is_not_opened_or_promoted(self) -> None:
        report = self.report(["/unowned/native/history:1:private-data"])
        original_read = Path.read_bytes
        observed = []

        def read_pinned(path):
            observed.append(path)
            return original_read(path)

        with mock.patch.object(Path, "read_bytes", read_pinned):
            result = self.collect(report)
        self.assertEqual(observed, [self.source])
        self.assertEqual(result["records"][0]["address_state"], "address-not-in-explicit-source-pins")
        self.assertFalse(result["records"][0]["source_verified"])

    def test_colons_in_filename_and_code_do_not_move_the_line(self) -> None:
        result = self.collect()
        record = result["records"][0]
        self.assertEqual(record["relative_path"], "src/adapter:42.rs")
        self.assertEqual(record["native_line"], 1)
        self.assertEqual(record["native_code"], "visible call(42:7)")
        self.assertIsNone(record["native_column"])
        self.assertTrue(record["source_verified"])
        self.assertFalse(record["native_code_matches_line_exactly"])
        self.assertTrue(record["native_code_matches_line_after_trim"])

    def test_known_annotations_preserve_raw_text_and_verify_only_source_prefix(self) -> None:
        cases = [
            ("Security token generated with non-cryptographic randomness",
             "Instant::now( in security-sensitive generation context"),
            ("Request-derived URL reaches outbound HTTP client", "data_dir -> build -> outbound HTTP"),
            ("Interpolated SQL reaches execution sink", "path( -> SQL execution"),
        ]
        for title, annotation in cases:
            with self.subTest(title=title):
                sample = self.sample + f"  [{annotation}]"
                report = self.report([sample])
                report["meta"]["version"] = "3.0.1"
                report["findings"][0].update({"title": title, "category": "Security Findings"})
                result = self.collect(report)
                record = result["records"][0]
                self.assertEqual(record["raw_sample"], sample)
                self.assertEqual(record["native_code"], f"visible call(42:7)  [{annotation}]")
                self.assertEqual(record["native_annotation"], annotation)
                self.assertEqual(record["native_source_code"], "visible call(42:7)")
                self.assertFalse(record["native_code_matches_line_after_trim"])
                self.assertTrue(record["native_annotation_matches_pinned_source_prefix"])
                self.assertTrue(record["source_verified"])
                self.assertEqual(result["qualification_gate"], "HOLD")

    def test_arbitrary_or_mislabeled_annotations_do_not_hide_code_mismatches(self) -> None:
        report = self.report([self.sample + "  [path( -> SQL execution]"])
        for version, title, category in [("3.0.2", "Interpolated SQL reaches execution sink", "Security Findings"),
                                         ("3.0.1", "Unknown rule", "Security Findings"),
                                         ("3.0.1", "Interpolated SQL reaches execution sink", "Unknown category")]:
            with self.subTest(version=version, title=title, category=category):
                report["meta"]["version"] = version
                report["findings"][0].update({"title": title, "category": category})
                self.assertFalse(self.collect(report)["records"][0]["source_verified"])
        report["meta"]["version"] = "3.0.1"
        report["findings"][0].update({"title": "Interpolated SQL reaches execution sink",
                                    "category": "Security Findings"})
        for sample in [self.sample + "  [unrecognized annotation]",
                       f"{self.source}:1:wrong code  [path( -> SQL execution]",
                       self.sample + "  [path( -> SQL execution] extra"]:
            with self.subTest(sample=sample):
                report["findings"][0]["samples"] = [sample]
                self.assertFalse(self.collect(report)["records"][0]["source_verified"])

    def test_physical_lf_lines_keep_embedded_cr_and_form_feed(self) -> None:
        source = self.root / "src/physical.rs"
        source.write_bytes(b"embedded\rbyte\x0ckept\n\t visible\n")
        self.pins = {"src/physical.rs": COLLECTOR.digest(source.read_bytes())}
        samples = [f"{source}:1:embedded\rbyte\x0ckept", f"{source}:2:   visible"]
        result = self.collect(self.report(samples, units=2))
        self.assertEqual([record["native_line"] for record in result["records"]], [1, 2])
        self.assertTrue(all(record["source_verified"] for record in result["records"]))
        self.assertEqual(result["records"][0]["source_line_sha256"],
                         COLLECTOR.digest(b"embedded\rbyte\x0ckept"))
        self.assertFalse(result["records"][1]["native_code_matches_line_exactly"])

    def test_missing_hash_mismatch_range_and_code_mismatch_stay_incomplete(self) -> None:
        self.pins[self.relative] = "0" * 64
        result = self.collect()
        self.assertFalse(result["records"][0]["source_pin_matches"])
        self.assertFalse(result["records"][0]["source_verified"])
        self.assertIn(f"pinned source bytes changed: {self.relative}", result["collection_errors"])
        self.pins[self.relative] = COLLECTOR.digest(self.source.read_bytes())
        for suffix, state in [("99:second", "native-line-out-of-range"),
                              ("1:wrong", "native-sample-source-mismatch"),
                              ("zero:code", "malformed-native-address"),
                              ("0:code", "malformed-native-address"),
                              ("9" * 5000 + ":code", "malformed-native-address")]:
            with self.subTest(suffix=suffix[:30]):
                result = self.collect(self.report([f"{self.source}:{suffix}"]))
                self.assertEqual(result["records"][0]["address_state"], state)
                self.assertFalse(result["records"][0]["source_verified"])
        self.pins = {"src/unavailable.rs": "0" * 64}
        result = self.collect(self.report([f"{self.root}/src/unavailable.rs:1:missing"]))
        self.assertIn("pinned source is unavailable: src/unavailable.rs", result["collection_errors"])

    def test_non_utf8_source_line_keeps_sample_without_fabricated_text(self) -> None:
        source = self.root / "src/non-utf8.rs"
        source.write_bytes(b"bad\xff\n")
        self.pins = {"src/non-utf8.rs": COLLECTOR.digest(source.read_bytes())}
        result = self.collect(self.report([f"{source}:1:bad"]))
        self.assertEqual(result["records"][0]["address_state"], "non-utf8-pinned-source-line")
        self.assertFalse(result["records"][0]["source_verified"])

    def test_receipt_report_hash_and_file_count_mismatches_are_reported(self) -> None:
        report = self.report()
        report["summary"]["files"] = 2
        result = self.collect(report, expected_hash="0" * 64)
        self.assertIn("native report bytes differ from the retained process receipt", result["collection_errors"])
        self.assertIn("native file count differs from the explicit source pin set", result["collection_errors"])

    def test_group_counts_and_excess_samples_do_not_pass_integrity(self) -> None:
        report = self.report([self.sample, self.sample], units=1)
        report["summary"]["critical"] = 2
        result = self.collect(report)
        self.assertEqual(len(result["records"]), 2)
        self.assertIn("/findings/0 emits more samples than declared count units", result["collection_errors"])
        self.assertIn("native group counts do not conserve the native summary", result["collection_errors"])
        self.assertEqual(result["unrepresented_count_units"]["critical"], 0)
        self.assertEqual(result["emission_collection_integrity"], "incomplete")

    def test_native_claims_cannot_override_derived_hold(self) -> None:
        report = self.report()
        report.update({"qualifies_native_gate": True, "qualification_gate": "PASS",
                       "per_helper_native_completion_proven": True})
        result = self.collect(report)
        self.assertTrue(result["native_document"]["qualifies_native_gate"])
        self.assertEqual(result["qualification_gate"], "HOLD")
        self.assertFalse(result["qualifies_native_gate"])
        self.assertFalse(result["per_helper_native_completion_proven"])

    def test_large_sample_array_has_no_collector_cap(self) -> None:
        samples = [self.sample] * 5001
        result = self.collect(self.report(samples, units=len(samples)))
        self.assertEqual(len(result["records"]), 5001)
        self.assertEqual(result["records"][-1]["native_json_pointer"], "/findings/0/samples/5000")
        self.assertEqual(result["verified_emitted_sample_counts"]["critical"], 5001)
        self.assertEqual(result["emission_collection_integrity"], "verified")

    def test_summary_and_count_shapes_are_strict(self) -> None:
        for bad in [True, -1, 1.5, "1", None]:
            with self.subTest(bad=bad):
                report = self.report()
                report["summary"]["critical"] = bad
                with self.assertRaises(COLLECTOR.ReportError):
                    self.collect(report)
                report = self.report()
                report["findings"][0]["count"] = bad
                with self.assertRaises(COLLECTOR.ReportError):
                    self.collect(report)
        with self.assertRaises(COLLECTOR.ReportError):
            self.collect(native_exit_code=True)

    def test_malformed_native_shapes_do_not_become_empty_success(self) -> None:
        for mutate in [lambda report: report.update({"meta": []}),
                       lambda report: report.update({"summary": []}),
                       lambda report: report.update({"findings": {}}),
                       lambda report: report["findings"][0].update({"severity": "unknown"}),
                       lambda report: report["findings"][0].update({"samples": "not-an-array"})]:
            report = self.report()
            mutate(report)
            with self.assertRaises(COLLECTOR.ReportError):
                self.collect(report)

    def test_duplicate_truncated_nonfinite_and_non_utf8_json_are_rejected(self) -> None:
        for payload in [b'{"count":1,"count":2}', b'{"unfinished":', b'{"number":NaN}',
                        b'{"number":Infinity}', b'{"number":1e999}', b'{"text":"\xff"}',
                        '{"count":1}'.encode("utf-16"), '{"count":1}'.encode("utf-32")]:
            with self.subTest(payload=payload[:32]):
                with self.assertRaises(COLLECTOR.ReportError):
                    COLLECTOR.native_json(payload)

    def test_noncanonical_paths_and_source_symlinks_are_rejected(self) -> None:
        for relative in ["../outside", "/absolute", "src//adapter.rs", "src/../adapter.rs", ".",
                         "src/null\0.rs", "src/\ud800.rs"]:
            with self.subTest(relative=relative):
                self.pins = {relative: "0" * 64}
                with self.assertRaises(COLLECTOR.ReportError):
                    self.collect()
        link = self.root / "src/link.rs"
        link.symlink_to(self.source)
        self.pins = {"src/link.rs": COLLECTOR.digest(self.source.read_bytes())}
        with self.assertRaises(COLLECTOR.ReportError):
            self.collect(self.report([f"{link}:1:visible call(42:7)"]))

    def test_unknown_title_shape_is_retained_without_annotation_promotion(self) -> None:
        report = self.report([self.sample + "  [path( -> SQL execution]"])
        report["meta"]["version"] = "3.0.1"
        report["findings"][0].update({"title": {"unknown": "shape"}, "category": "Security Findings"})
        result = self.collect(report)
        self.assertEqual(result["groups"][0]["raw_title"], {"unknown": "shape"})
        self.assertFalse(result["records"][0]["source_verified"])

    def test_root_symlink_and_wrong_native_root_are_rejected(self) -> None:
        link = self.owned_root / f"root-alias-{uuid.uuid4().hex}"
        link.symlink_to(self.root, target_is_directory=True)
        report = self.report()
        payload = self.encoded(report)
        with self.assertRaises(COLLECTOR.ReportError):
            COLLECTOR.collect_report(payload, link, self.pins, 1, COLLECTOR.digest(payload))
        report["meta"]["project_dir"] = str(self.owned_root)
        with self.assertRaises(COLLECTOR.ReportError):
            self.collect(report)

    def test_cli_valid_collection_has_exit_one_and_preserves_native_exit(self) -> None:
        result = self.run_cli(self.report())
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stderr, "")
        # ubs:ignore[py.parsing.json-loads-no-try,py.json.loads-no-try] -- The test asserts on the collector's JSON document; a parse failure must fail the test loudly.
        document = json.loads(result.stdout)
        self.assertEqual(document["native_exit_code"], 1)
        self.assertEqual(document["qualification_gate"], "HOLD")
        self.assertFalse(document["qualifies_native_gate"])

    def test_cli_malformed_receipt_and_duplicate_pins_have_error_exit(self) -> None:
        for receipt in [[], {"source_checks_after": [], "outputs": []},
                        {"source_checks_after": [], "outputs": {"findings.json": []}}]:
            with self.subTest(receipt=receipt):
                result = self.run_cli(self.report(), receipt)
                self.assertEqual(result.returncode, 2, result.stderr)
                # ubs:ignore[py.parsing.json-loads-no-try,py.json.loads-no-try] -- The test asserts on the collector's JSON document; a parse failure must fail the test loudly.
                document = json.loads(result.stdout)
                self.assertEqual(document["qualification_gate"], "HOLD")
                self.assertFalse(document["qualifies_native_gate"])
        report = self.report()
        receipt = self.receipt(self.encoded(report))
        receipt["source_checks_after"].append(dict(receipt["source_checks_after"][0]))
        result = self.run_cli(report, receipt)
        self.assertEqual(result.returncode, 2, result.stderr)
        # ubs:ignore[py.parsing.json-loads-no-try,py.json.loads-no-try] -- The test asserts on the collector's JSON document; a parse failure must fail the test loudly.
        self.assertIn("duplicate source pins", json.loads(result.stdout)["error"])

    def test_cli_unconserved_receipt_pins_are_not_accepted(self) -> None:
        report = self.report()
        receipt = self.receipt(self.encoded(report))
        receipt["source_checks_after"][0]["current_sha256"] = "0" * 64
        result = self.run_cli(report, receipt)
        self.assertEqual(result.returncode, 2, result.stderr)
        # ubs:ignore[py.parsing.json-loads-no-try,py.json.loads-no-try] -- The test asserts on the collector's JSON document; a parse failure must fail the test loudly.
        self.assertIn("does not conserve", json.loads(result.stdout)["error"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
