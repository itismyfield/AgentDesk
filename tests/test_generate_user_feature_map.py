"""Feature declarations cannot manufacture live coverage or erase inventory."""

from copy import deepcopy
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import unquote

import yaml

from scripts import check_doc_path_refs, generate_user_feature_map as gen
from scripts.ci import user_feature_map_report


REPO = Path(__file__).resolve().parents[1]
GENERATOR = REPO / "scripts/generate_user_feature_map.py"
FEATURES = (
    "message-delivery", "busy-input", "reaction-display", "cancel-stop",
    "reset-clear", "threads", "dm", "routines", "herdr-handoff",
    "effort-model", "campaigns",
)
ROWS = "docs/user-feature-map.rows.yaml"
SCENARIOS = "tests/e2e/tui_relay/scenarios"
OUTPUTS = ("docs/user-feature-map.md", "docs/generated/user-feature-coverage.json")
PYTHON_TEST_REFERENCE = "tests/ref_cases.py::ReferenceCase.test_existing"
RUST_TEST_REFERENCE = "src/ref_tests.rs::existing_test"


def applicable():
    return {
        "host": "tmux", "provider": "claude", "surface": "channel", "state": "idle",
        "qualifiers": {
            "turn_origin": "managed", "output_path": "legacy",
            "input_source": "legacy_automation",
        },
        "required_assertion_ids": ["provider_text_present"],
    }


class FeatureMapTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="feature-map-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q")
        (self.root / ".keep").write_text("bootstrap\n")
        self.git("add", ".keep")
        self.git("commit", "-qm", "bootstrap")
        self.rows = {
            "schema": 1,
            "features": [
                {
                    "feature_id": feature_id, "title": feature_id,
                    "visible_contract": "사용자에게 응답이 한 번 보인다.",
                    "verification": "python3 scripts/generate_user_feature_map.py --check",
                    "owner": "src/services/discord", "chart": "해당 없음: 정적 명세",
                    "applicable": [applicable()], "pending_assertions": [],
                    "assertion_sources": {},
                }
                for feature_id in FEATURES
            ],
            "removal_reasons": [],
        }
        app = applicable()
        app.pop("required_assertion_ids")
        self.scenario = {
            "id": "E-900", "agent_mode": "real_live", "coverage_class": "live",
            "cells": ["claude-tui"], "steps": [{"send_discord_prompt": "marker"}],
            "assertions": [{"provider_text_present": "marker"}],
            "feature_coverage": {
                "schema": 1,
                "claims": [{"feature_id": "message-delivery", **app,
                            "assertion_ids": ["provider_text_present"]}],
            },
        }
        self.save()

    def git(self, *args):
        result = subprocess.run(
            ["git", "-c", "user.name=Feature map test", "-c",
             "user.email=feature-map@example.invalid", *args],
            cwd=self.root, text=True, capture_output=True, check=True,
        )
        return result.stdout.strip()

    def dump(self, relative, value):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(yaml.safe_dump(value, allow_unicode=True, sort_keys=False))

    def save(self):
        self.dump(ROWS, self.rows)
        self.dump(f"{SCENARIOS}/E-900.yaml", self.scenario)

    def baseline(self):
        self.save()
        self.git("add", ROWS)
        self.git("commit", "-qm", "inventory baseline")

    def generate(self):
        self.save()
        return gen.generate(self.root, "HEAD")

    def cli(self, *mode):
        return subprocess.run(
            [sys.executable, str(GENERATOR), *mode, "--root", str(self.root),
             "--base-ref", "HEAD"], text=True, capture_output=True,
        )

    def rejected(self, pattern):
        with self.assertRaisesRegex(gen.MapError, pattern):
            self.generate()

    def reference_sources(self):
        python = self.root / "tests/ref_cases.py"
        python.parent.mkdir(parents=True, exist_ok=True)
        python.write_text(
            "import unittest\n\n"
            "class ReferenceCase(unittest.TestCase):\n"
            "    def test_existing(self):\n"
            "        self.assertTrue(True)\n"
            "    def helper(self):\n"
            "        return True\n"
        )
        rust = self.root / "src/ref_tests.rs"
        rust.parent.mkdir(parents=True, exist_ok=True)
        rust.write_text("#[test]\nfn existing_test() {}\nfn helper() {}\n")

    def reference(self, **changes):
        return {"feature_id": "dm", "scope": "provider routing의 부분 명세", "needs_live": True,
                **changes}

    def test_valid_bootstrap_counts_declarations_without_execution_claim(self):
        markdown, report = self.generate()
        self.assertEqual(report["schema_version"], 1)
        self.assertEqual((report["applicable"], report["covered"], report["missing"]),
                         (11, 1, 10))
        self.assertTrue(report["source_hash"])
        self.assertIn("E-900", markdown)

    def test_schema_rejects_empty_or_duplicate_feature_inventory(self):
        original = deepcopy(self.rows)
        for features in ([], original["features"] + [deepcopy(original["features"][0])]):
            with self.subTest(features=len(features)):
                self.rows = deepcopy(original)
                self.rows["features"] = features
                self.rejected("feature|schema|inventory")

    def test_schema_rejects_empty_applicable_inventory(self):
        self.rows["features"][0]["applicable"] = []
        self.rejected("applicable|inventory|schema")

    def test_schema_rejects_invalid_axis_qualifier_and_source(self):
        original = deepcopy(self.rows)
        for name, value in (("host", "wildcard"), ("provider", "unknown"),
                            ("surface", "any"), ("state", "unknown")):
            with self.subTest(axis=name):
                self.rows = deepcopy(original)
                self.rows["features"][0]["applicable"][0][name] = value
                self.rejected("axis|schema|host|provider|surface|state")
        for name, value in (("input_source", "unknown"), ("turn_origin", "any"),
                            ("output_path", "any"), ("unknown", "value")):
            with self.subTest(qualifier=name):
                self.rows = deepcopy(original)
                self.rows["features"][0]["applicable"][0]["qualifiers"][name] = value
                self.rejected("qualifier|source|schema")

    def test_cells_subset_rejects_host_provider_outside_scenario(self):
        self.rows["features"][0]["applicable"][0]["host"] = "herdr"
        self.scenario["feature_coverage"]["claims"][0]["host"] = "herdr"
        self.rejected("cells")

    def test_pipe_and_empty_cells_do_not_imply_tui_coverage(self):
        for cells in (["claude-pipe"], []):
            with self.subTest(cells=cells):
                self.scenario["cells"] = cells
                self.rejected("cells")

    def test_registry_operation_missing_from_yaml_is_rejected(self):
        self.scenario["feature_coverage"]["claims"][0]["assertion_ids"].append(
            "no_duplicate_content")
        self.rejected("assertion")

    def test_yaml_operation_missing_from_registry_is_rejected(self):
        operation = "not_implemented_observer"
        self.rows["features"][0]["applicable"][0]["required_assertion_ids"] = [operation]
        self.rows["features"][0]["pending_assertions"] = [{
            "assertion_id": operation, "issue": "#8120", "reason": "미구현",
            "input_sources": ["legacy_automation"],
        }]
        self.scenario["assertions"] = [{operation: True}]
        self.scenario["feature_coverage"]["claims"][0]["assertion_ids"] = [operation]
        self.rejected("assertion|registry")

    def test_assertion_entry_with_two_operations_is_not_executable(self):
        self.scenario["assertions"] = [{
            "provider_text_present": "marker", "no_duplicate_content": True,
        }]
        self.rejected("assertion|schema")

    def test_required_assertion_cannot_be_omitted_from_claim(self):
        self.rows["features"][0]["applicable"][0]["required_assertion_ids"].append(
            "no_duplicate_content")
        self.scenario["assertions"].append({"no_duplicate_content": True})
        self.rejected("required|assertion")

    def test_thread_and_busy_witnesses_are_automatic_requirements(self):
        for axis, value in (("surface", "thread"), ("state", "busy")):
            with self.subTest(axis=axis):
                self.rows["features"][0]["applicable"][0] = applicable()
                self.rows["features"][0]["applicable"][0][axis] = value
                claim = self.scenario["feature_coverage"]["claims"][0]
                claim["surface"], claim["state"] = "channel", "idle"
                claim[axis] = value
                self.rejected("witness|assertion|required")

    def test_metadata_only_witness_is_not_a_registered_execution_assertion(self):
        operation = "surface_thread_observed"
        app = self.rows["features"][0]["applicable"][0]
        app["surface"] = "thread"
        self.rows["features"][0]["pending_assertions"] = [{
            "assertion_id": operation, "issue": "#8120", "reason": "실제 관측 미구현",
            "input_sources": ["legacy_automation"],
        }]
        claim = self.scenario["feature_coverage"]["claims"][0]
        claim["surface"] = "thread"
        claim["assertion_ids"].append(operation)
        self.rejected("assertion|witness|registry")

    def test_automation_cannot_cover_a_human_only_assertion(self):
        for source in ("allowed_bot", "legacy_automation"):
            with self.subTest(source=source):
                self.rows["features"][0]["assertion_sources"] = {
                    "provider_text_present": ["human"],
                }
                self.rows["features"][0]["applicable"][0]["qualifiers"]["input_source"] = source
                self.scenario["feature_coverage"]["claims"][0]["qualifiers"]["input_source"] = source
                self.rejected("source")

    def test_pending_source_declaration_cannot_widen_human_only_contract(self):
        feature = self.rows["features"][0]
        feature["assertion_sources"] = {"provider_text_present": ["human"]}
        feature["pending_assertions"] = [{
            "assertion_id": "provider_text_present", "issue": "#8120",
            "reason": "관측 메타데이터가 source 계약을 완화할 수 없다.",
            "input_sources": ["human", "allowed_bot"],
        }]
        feature["applicable"][0]["qualifiers"]["input_source"] = "allowed_bot"
        self.scenario["feature_coverage"]["claims"][0]["qualifiers"]["input_source"] = "allowed_bot"
        self.rejected("source")

    def test_nonlive_classes_never_increase_live_covered_count(self):
        for coverage_class, mode in (("fixture", "none"),
                                     ("unsupported-known-gap", "real_live"),
                                     ("retired", "real_live")):
            with self.subTest(coverage_class=coverage_class):
                self.scenario["coverage_class"], self.scenario["agent_mode"] = coverage_class, mode
                _, report = self.generate()
                self.assertEqual(report["covered"], 0)
                self.assertEqual(report["missing"], 11)

    def test_live_label_without_live_mode_is_not_live_coverage(self):
        self.scenario["agent_mode"] = "none"
        try:
            _, report = self.generate()
        except gen.MapError:
            return
        self.assertEqual(report["covered"], 0)

    def test_fixture_operation_cannot_be_promoted_to_live_coverage(self):
        operation = "fixture_followup_ready"
        self.rows["features"][0]["applicable"][0]["required_assertion_ids"] = [operation]
        self.scenario["assertions"] = [{operation: True}]
        self.scenario["feature_coverage"]["claims"][0]["assertion_ids"] = [operation]
        self.rejected("fixture|live|class")

    def test_fixture_execution_cannot_be_promoted_by_live_metadata(self):
        self.scenario["steps"] = [{
            "replay_fixture": {"kind": "codex-modern", "provider": "codex", "frames": []},
        }]
        self.rejected("fixture|live|class")

    def test_live_claim_needs_executable_input_steps(self):
        for steps in ([], [{"wait_idle_s": 1}]):
            with self.subTest(steps=steps):
                self.scenario["steps"] = steps
                self.rejected("class|input|live|executable")

    def test_skipped_known_gap_cannot_claim_live_coverage(self):
        self.scenario["skip_reason"] = "Known-gap: 실제 provider 실행을 아직 지원하지 않는다."
        self.rejected("skip|gap|class")

    def test_qualifier_and_input_source_keys_cannot_substitute_for_each_other(self):
        row = self.rows["features"][0]
        human = deepcopy(row["applicable"][0])
        human["qualifiers"]["input_source"] = "human"
        native = deepcopy(row["applicable"][0])
        native["qualifiers"]["turn_origin"] = "native"
        row["applicable"].extend([human, native])
        _, report = self.generate()
        self.assertEqual((report["applicable"], report["covered"], report["missing"]),
                         (13, 1, 12))
        self.assertNotEqual(gen.applicable_key("message-delivery", human),
                            gen.applicable_key("message-delivery", native))

    def test_reduction_of_feature_applicable_or_required_needs_exact_new_reason(self):
        extra = deepcopy(self.rows["features"][0])
        extra["feature_id"] = "additional-feature"
        self.rows["features"].append(extra)
        second = deepcopy(self.rows["features"][0]["applicable"][0])
        second["qualifiers"]["turn_origin"] = "native"
        self.rows["features"][0]["applicable"].append(second)
        self.rows["features"][0]["applicable"][0]["required_assertion_ids"].append(
            "no_duplicate_content")
        self.scenario["assertions"].append({"no_duplicate_content": True})
        self.scenario["feature_coverage"]["claims"][0]["assertion_ids"].append(
            "no_duplicate_content")
        self.baseline()
        original = deepcopy(self.rows)
        for kind in ("feature", "applicable", "required"):
            with self.subTest(kind=kind):
                self.rows = deepcopy(original)
                if kind == "feature":
                    self.rows["features"].pop()
                elif kind == "applicable":
                    self.rows["features"][0]["applicable"].pop()
                else:
                    self.rows["features"][0]["applicable"][0]["required_assertion_ids"].pop()
                self.rejected("ratchet|removal")

    def test_exact_new_removal_reason_allows_required_reduction(self):
        app = self.rows["features"][0]["applicable"][0]
        app["required_assertion_ids"].append("no_duplicate_content")
        self.baseline()
        app["required_assertion_ids"].remove("no_duplicate_content")
        self.rows["removal_reasons"] = [{
            "key": gen.required_key("message-delivery", app, "no_duplicate_content"),
            "reason": "현재 사용자 계약에서 중복 검사는 별도 행으로 이관한다.",
        }]
        _, report = self.generate()
        self.assertEqual(report["covered"], 1)

    def test_old_generic_reason_cannot_be_reused_for_new_reduction(self):
        app = self.rows["features"][0]["applicable"][0]
        app["required_assertion_ids"].append("no_duplicate_content")
        self.rows["removal_reasons"] = [{"key": "feature:old-feature", "reason": "옛 일반 사유"}]
        self.baseline()
        app["required_assertion_ids"].remove("no_duplicate_content")
        self.rows["removal_reasons"].append({
            "key": gen.required_key("message-delivery", app, "no_duplicate_content"),
            "reason": "옛 일반 사유",
        })
        self.rejected("ratchet|removal|reason")

    def test_old_exact_key_reason_is_not_new_authorization(self):
        app = self.rows["features"][0]["applicable"][0]
        app["required_assertion_ids"].append("no_duplicate_content")
        self.rows["removal_reasons"] = [{
            "key": gen.required_key("message-delivery", app, "no_duplicate_content"),
            "reason": "이전 base에 이미 존재하는 설명",
        }]
        self.baseline()
        app["required_assertion_ids"].remove("no_duplicate_content")
        self.rejected("ratchet|removal|reason")

    def test_missing_or_unresolvable_base_is_not_bootstrap(self):
        for base in ("", "refs/heads/no-such-base"):
            with self.subTest(base=base), self.assertRaisesRegex(gen.MapError, "base|ref"):
                gen.generate(self.root, base)

    def test_base_selection_does_not_change_generated_bytes(self):
        bootstrap_markdown, bootstrap_report = self.generate()
        self.baseline()
        baseline_markdown, baseline_report = self.generate()
        self.assertEqual(bootstrap_markdown, baseline_markdown)
        self.assertEqual(json.dumps(bootstrap_report, sort_keys=True),
                         json.dumps(baseline_report, sort_keys=True))

    def test_source_hash_changes_when_only_rows_text_changes(self):
        _, original = self.generate()
        path = self.root / ROWS
        path.write_text(path.read_text() + "\n# 수기 정본 주석만 변경\n")
        changed_markdown, changed = gen.generate(self.root, "HEAD")
        self.assertNotEqual(original["source_hash"], changed["source_hash"])
        self.assertEqual((original["applicable"], original["covered"]),
                         (changed["applicable"], changed["covered"]))
        self.assertEqual((changed_markdown, changed), gen.generate(self.root, "HEAD"))

    def test_source_hash_includes_notes_of_scenarios_without_claims(self):
        unclaimed = deepcopy(self.scenario)
        unclaimed["id"] = "E-901"
        unclaimed.pop("feature_coverage")
        unclaimed["notes"] = "claim이 없는 시나리오"
        relative = f"{SCENARIOS}/E-901.yaml"
        self.dump(relative, unclaimed)
        _, original = self.generate()
        unclaimed["notes"] += "의 설명만 수정"
        self.dump(relative, unclaimed)
        changed_markdown, changed = gen.generate(self.root, "HEAD")
        self.assertNotEqual(original["source_hash"], changed["source_hash"])
        self.assertEqual((original["applicable"], original["covered"]),
                         (changed["applicable"], changed["covered"]))
        self.assertEqual((changed_markdown, changed), gen.generate(self.root, "HEAD"))

    def test_feature_needs_live_rejects_non_boolean_values(self):
        for value in (1, 0, "true", None, []):
            with self.subTest(value=value):
                self.rows["features"][0]["needs_live"] = value
                self.rejected("needs_live|test")

    def test_test_names_require_a_list_of_real_test_coordinates(self):
        for value in ("tests/ref_cases.py::ReferenceCase.test_existing", None, 1, [False], [""]):
            with self.subTest(value=value):
                self.rows["features"][0]["test_names"] = value
                self.rejected("test")

    def test_test_names_reject_invalid_paths_and_missing_definitions(self):
        self.reference_sources()
        invalid = (
            "missing-coordinate", "../tests/ref_cases.py::ReferenceCase.test_existing",
            "tests/../tests/ref_cases.py::ReferenceCase.test_existing",
            "/tmp/ref_cases.py::ReferenceCase.test_existing", "tests/missing.py::ReferenceCase.test_existing",
            "tests/ref_cases.py::ReferenceCase.test_missing", "tests/ref_cases.py::OtherCase.test_existing",
            "tests/ref_cases.py::ReferenceCase.helper", "src/ref_tests.rs::missing_test",
            "src/ref_tests.rs::helper",
        )
        for value in invalid:
            with self.subTest(value=value):
                self.rows["features"][0]["test_names"] = [value]
                self.rejected("test")

    def test_reference_metadata_rejects_unknown_feature_or_empty_scope(self):
        for entry in (self.reference(feature_id="not-in-inventory"),
                      self.reference(scope=""), self.reference(scope="  "),
                      self.reference(scope=False)):
            with self.subTest(entry=entry):
                self.scenario["feature_coverage"]["references"] = [entry]
                self.rejected("reference")

    def test_references_require_mappings_and_explicit_true_needs_live(self):
        for entries in (False, {}, [None], [self.reference(needs_live=False)],
                        [self.reference(needs_live=1)], [self.reference(needs_live="true")]):
            with self.subTest(entries=entries):
                self.scenario["feature_coverage"]["references"] = entries
                self.rejected("reference|needs_live")

    def test_references_only_scenario_preserves_all_live_gaps(self):
        metadata = self.scenario["feature_coverage"]
        metadata.pop("claims")
        entry = self.reference()
        metadata["references"] = [entry]
        for explicit_empty_claims in (False, True):
            with self.subTest(explicit_empty_claims=explicit_empty_claims):
                if explicit_empty_claims:
                    metadata["claims"] = []
                markdown, report = self.generate()
                self.assertEqual((report["applicable"], report["covered"], report["missing"]), (11, 0, 11))
                self.assertEqual(report["by_feature"]["dm"]["scenario_references"],
                                 [{"scenario_id": "E-900", "scope": entry["scope"], "needs_live": True}])
                self.assertIn(entry["scope"], markdown)

    def test_empty_claims_and_references_are_not_a_feature_mapping(self):
        self.scenario["feature_coverage"].update(claims=[], references=[])
        self.rejected("claim|reference")

    def test_references_and_existing_tests_are_reported_without_implying_execution(self):
        self.reference_sources()
        feature = next(row for row in self.rows["features"] if row["feature_id"] == "dm")
        feature["needs_live"] = True
        feature["test_names"] = [PYTHON_TEST_REFERENCE, RUST_TEST_REFERENCE]
        self.scenario["feature_coverage"]["references"] = [self.reference()]
        markdown, report = self.generate()
        summary = report["by_feature"]["dm"]
        self.assertTrue(summary["needs_live"])
        self.assertCountEqual(summary["test_names"], [PYTHON_TEST_REFERENCE, RUST_TEST_REFERENCE])
        self.assertEqual((summary["covered"], summary["missing"]), (0, 1))
        self.assertEqual(report["covered"], 1)
        self.assertIn(PYTHON_TEST_REFERENCE, markdown)
        self.assertIn(RUST_TEST_REFERENCE, markdown)

    def test_false_needs_live_does_not_fill_a_missing_live_contract(self):
        self.reference_sources()
        feature = next(row for row in self.rows["features"] if row["feature_id"] == "dm")
        feature["needs_live"] = False
        feature["test_names"] = [PYTHON_TEST_REFERENCE]
        _, report = self.generate()
        summary = report["by_feature"]["dm"]
        self.assertIs(summary["needs_live"], False)
        self.assertEqual((summary["covered"], summary["missing"]), (0, 1))
        self.assertEqual((report["covered"], report["missing"]), (1, 10))

    def test_existing_rows_default_to_needs_live_and_empty_test_references(self):
        _, report = self.generate()
        for feature in report["by_feature"].values():
            self.assertIs(feature["needs_live"], True)
            self.assertEqual(feature["test_names"], [])
            self.assertEqual(feature["scenario_references"], [])

    def test_referenced_test_source_bytes_participate_in_source_hash(self):
        self.reference_sources()
        self.rows["features"][0]["test_names"] = [PYTHON_TEST_REFERENCE, RUST_TEST_REFERENCE]
        _, original = self.generate()
        for relative in ("tests/ref_cases.py", "src/ref_tests.rs"):
            with self.subTest(source=relative):
                path = self.root / relative
                before = path.read_text()
                comment = "\n# source metadata only\n" if relative.endswith(".py") else "\n// source metadata only\n"
                path.write_text(before + comment)
                _, changed = gen.generate(self.root, "HEAD")
                self.assertNotEqual(original["source_hash"], changed["source_hash"])
                self.assertEqual((changed["applicable"], changed["covered"], changed["missing"]), (11, 1, 10))
                self.assertEqual(changed, gen.generate(self.root, "HEAD")[1])
                path.write_text(before)

    def test_raw_source_only_change_keeps_markdown_identical(self):
        original_markdown, original = self.generate()
        path = self.root / ROWS
        path.write_text(path.read_text() + "\n# commentary only\n")
        markdown, report = gen.generate(self.root, "HEAD")
        self.assertEqual(original_markdown, markdown)
        self.assertNotEqual(original["source_hash"], report["source_hash"])

    def test_validation_and_separate_output_do_not_need_committed_coverage(self):
        directory = self.root / "review artifacts" / "feature-map"
        valid = self.cli("--validate")
        self.assertEqual(valid.returncode, 0, valid.stdout + valid.stderr)
        rendered = self.cli("--output-dir", str(directory))
        self.assertEqual(rendered.returncode, 0, rendered.stdout + rendered.stderr)
        report = json.loads((directory / "user-feature-coverage.json").read_text())
        self.assertEqual(report["covered"], 1)
        self.assertFalse((self.root / OUTPUTS[1]).exists())
        self.assertFalse((self.root / OUTPUTS[0]).exists())
        markdown = directory / "user-feature-map.md"
        line = next(line for line in markdown.read_text().splitlines() if line.startswith("수기 정본:"))
        link = re.search(r"\[user-feature-map\.rows\.yaml\]\(([^)]+)\)", line)
        self.assertIsNotNone(link)
        self.assertEqual((directory / unquote(link[1])).resolve(strict=True), self.root / ROWS)
        self.assertEqual(check_doc_path_refs.scan_doc(
            markdown.relative_to(self.root).as_posix(), line,
            check_doc_path_refs.RepoIndex([ROWS], self.root),
        ), [])

    def commit_snapshot(self, message):
        registry = self.root / gen.REGISTRY
        registry.parent.mkdir(parents=True, exist_ok=True)
        registry.write_bytes((REPO / gen.REGISTRY).read_bytes())
        self.git("add", "scripts", "docs", "tests")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def push_event(self, base):
        event = self.root / "event.json"
        event.write_text(json.dumps({"before": base, "ref": "refs/heads/main"}))
        return {"GITHUB_EVENT_NAME": "push", "GITHUB_EVENT_PATH": str(event),
                "TEST_LANE_BASELINE_REF": "HEAD", "GFP_BASE_SHA": "HEAD"}

    def test_main_report_uses_previous_push_sha_instead_of_validation_baseline(self):
        base = self.commit_snapshot("covered")
        self.scenario["coverage_class"] = "fixture"
        self.save()
        head = self.commit_snapshot("coverage gap")
        output = self.root / "push"
        user_feature_map_report.report(self.root, output, head, self.push_event(base))
        summary = (output / "summary.md").read_text()
        self.assertIn("covered → gap", summary)
        self.assertIn(f"Source validation baseline: `{head}`", summary)
        self.assertIn("feature-map.diff", summary)
        manifest = json.loads((output / "manifest.json").read_text())
        self.assertEqual((manifest["base"], manifest["head"]), (base, head))
        for name in ("head.md", "head.json", "base.md", "base.json", "summary.md", "feature-map.diff"):
            self.assertEqual(manifest["sha256"][name], hashlib.sha256((output / name).read_bytes()).hexdigest())

    def test_report_for_source_only_change_keeps_markdown_identical(self):
        base = self.commit_snapshot("before")
        rows = self.root / ROWS
        rows.write_text(rows.read_text() + "\n# commentary only\n")
        head = self.commit_snapshot("comment")
        output = self.root / "push"
        server, repository = "https://git.example.invalid", "owner/repo"
        env = {**self.push_event(base), "GITHUB_SERVER_URL": server, "GITHUB_REPOSITORY": repository}
        user_feature_map_report.report(self.root, output, head, env)
        self.assertEqual((output / "base.md").read_bytes(), (output / "head.md").read_bytes())
        self.assertNotEqual((output / "base.json").read_bytes(), (output / "head.json").read_bytes())
        summary = (output / "summary.md").read_text()
        self.assertIn("Markdown unchanged.", summary)
        self.assertIn("Source hash changed:", summary)
        evidence = summary.split("### Source inputs\n", 1)
        self.assertEqual(len(evidence), 2)
        for role, sha in (("Head", head), ("Base", base)):
            self.assertIn(f"[{role} rows YAML]({server}/{repository}/blob/{sha}/{ROWS})", evidence[1])
        docs = check_doc_path_refs.RepoIndex(["summary.md"], output)
        for name in ("base.md", "head.md"):
            line = next(line for line in (output / name).read_text().splitlines() if line.startswith("수기 정본:"))
            self.assertIn("](summary.md#source-inputs)", line)
            self.assertEqual(check_doc_path_refs.scan_doc(name, line, docs), [])
        self.assertFalse((output / "feature-map.diff").exists())

    def test_report_validates_both_snapshots_and_rejects_hidden_contract_changes(self):
        extra = applicable()
        extra["qualifiers"]["turn_origin"] = "native"
        self.rows["features"][0]["applicable"].append(extra)
        self.save()
        original_rows, original_scenario = deepcopy(self.rows), deepcopy(self.scenario)
        good = self.commit_snapshot("valid coverage")
        errors = (
            ("missing assertion", "E-900.yaml.*required witness"),
            ("unsupported witness", "user-feature-map.rows.yaml.*unsupported required assertion"),
            ("hidden tuple", "user-feature-map.rows.yaml.*new removal_reason"),
        )
        for kind, error in errors:
            with self.subTest(kind=kind):
                self.git("switch", "--detach", good)
                self.rows, self.scenario = deepcopy(original_rows), deepcopy(original_scenario)
                apps = self.rows["features"][0]["applicable"]
                if kind == "missing assertion":
                    self.scenario["assertions"].append({"no_duplicate_marker": "marker"})
                    self.scenario["feature_coverage"]["claims"][0]["assertion_ids"] = ["no_duplicate_marker"]
                elif kind == "unsupported witness":
                    apps[0]["required_assertion_ids"].append("unsupported_witness")
                else:
                    apps.pop()
                self.save()
                broken = self.commit_snapshot(kind)
                output = self.root / "invalid"
                with self.assertRaisesRegex(gen.MapError, error):
                    user_feature_map_report.report(self.root, output, good, self.push_event(good))
                self.assertFalse(output.exists())
                self.rows, self.scenario = deepcopy(original_rows), deepcopy(original_scenario)
                self.save()
                restored = self.commit_snapshot("restore valid coverage")
                with self.assertRaisesRegex(gen.MapError, error):
                    user_feature_map_report.report(self.root, output, restored, self.push_event(broken))
                self.assertFalse(output.exists())

    def test_generated_bytes_are_repeatable_and_drift_is_rejected(self):
        self.save()
        first = self.cli("--write")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        target = self.root / OUTPUTS[0]
        original = target.read_bytes()
        second = self.cli("--write")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual(target.read_bytes(), original)
        self.assertFalse((self.root / OUTPUTS[1]).exists())
        target.write_bytes(original + b" ")
        result = self.cli("--check")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(f"drift: regenerate {OUTPUTS[0]}", result.stderr)
        self.assertIn("--write --base-ref HEAD", result.stderr)
        self.assertEqual(self.cli("--validate").returncode, 0)
        self.assertEqual(self.cli("--write").returncode, 0)
        self.assertEqual(self.cli("--check").returncode, 0)


if __name__ == "__main__":
    unittest.main()
