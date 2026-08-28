"""Adversarial tests for the bounded Discord publication manifest (#71)."""
from __future__ import annotations

import copy
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_discord_publication_boundaries.py"
_spec = importlib.util.spec_from_file_location("check_discord_publication_boundaries", SCRIPT)
assert _spec and _spec.loader
checker = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = checker
_spec.loader.exec_module(checker)


def row(**updates: object) -> dict[str, object]:
    value: dict[str, object] = {
        "path_id": "fixture",
        "file": "src/fixture.rs",
        "entry_symbol": "publish",
        "transport_symbols": ["gateway.send_message"],
        "contract_paths": [],
        "executor": {"owner": "caller", "mode": "caller", "send_contract": None, "spawn": None},
        "authorities": ["fixture authority"],
        "timeout_retry": {"timeout_ms": None, "policy": "none"},
        "failure_classes": ["AMBIG"],
        "settlement_symbols": [],
        "post_success": None,
        "multi_op": False,
        "success_then_settlement": False,
        "direct_send_count": 1,
        "authority_order_after": [],
    }
    value.update(updates)
    return value


def payload(rows: list[dict[str, object]], scope_files: list[str] | None = None) -> dict[str, object]:
    return {
        "schema_version": 1,
        "scope": "fixture scope; not repository-wide or family-complete",
        "identity": "path_id",
        "closed_scope": True,
        "closure": {
            "kind": "listed_files",
            "claim": "direct_operations_only",
            "scope_files": scope_files or ["src/fixture.rs"],
            "direct_operations": checker.DIRECT_OPERATIONS,
            "excluded_operations": ["delete_message"],
        },
        "excluded": ["unlisted files", "delete_message cleanup"],
        "rows": rows,
    }


class ManifestContract(unittest.TestCase):
    def validate_data(self, data: dict[str, object], files: dict[str, str]) -> dict[str, object]:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for rel, source in files.items():
                target = root / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(source, encoding="utf-8")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps(data), encoding="utf-8")
            return checker.load_and_validate(manifest, root, enforce_canonical=False)

    def validate(self, rows: list[dict[str, object]], source: str = "async fn publish() { gateway.send_message(); }\n", **files: str) -> None:
        all_files = {"src/fixture.rs": source, **files}
        self.validate_data(payload(rows), all_files)

    def reject(self, rows: list[dict[str, object]], message: str, source: str = "async fn publish() { gateway.send_message(); }\n", **files: str) -> None:
        with self.assertRaisesRegex(checker.ManifestError, message):
            self.validate(rows, source, **files)

    def test_checked_in_manifest_is_exact_and_valid(self) -> None:
        checked = checker.load_and_validate(REPO_ROOT / "scripts/discord_publication_boundaries.json", REPO_ROOT)
        self.assertEqual(tuple(checked["closure"]["scope_files"]), checker.CANONICAL_SCOPE_FILES)
        self.assertEqual(tuple(item["path_id"] for item in checked["rows"]), checker.CANONICAL_PATH_IDS)
        self.assertEqual((len(checked["rows"]), len(checked["closure"]["scope_files"])), (30, 29))
        self.assertIn("not repository-wide or family-complete", checked["scope"])

    def test_coordinated_scope_and_row_shrink_is_rejected(self) -> None:
        data = json.loads((REPO_ROOT / "scripts" / "discord_publication_boundaries.json").read_text())
        data["rows"] = [item for item in data["rows"] if item["path_id"] != "restart-report-legacy"]
        data["closure"]["scope_files"].remove("src/services/discord/restart_report.rs")
        with tempfile.NamedTemporaryFile("w", suffix=".json") as manifest:
            json.dump(data, manifest); manifest.flush()
            with self.assertRaisesRegex(checker.ManifestError, "canonical 29-file"):
                checker.load_and_validate(Path(manifest.name), REPO_ROOT)
        data = json.loads((REPO_ROOT / "scripts/discord_publication_boundaries.json").read_text())
        data["rows"][-1] = {**data["rows"][0], "path_id": data["rows"][-1]["path_id"]}
        with tempfile.NamedTemporaryFile("w", suffix=".json") as manifest:
            json.dump(data, manifest); manifest.flush()
            with self.assertRaisesRegex(checker.ManifestError, "canonical 30-row contract"):
                checker.load_and_validate(Path(manifest.name), REPO_ROOT)

    def test_checked_in_ufcs_sites_have_real_owners(self) -> None:
        found = checker.discover_direct_sends(REPO_ROOT, set(checker.CANONICAL_SCOPE_FILES))
        self.assertEqual(found[("src/services/discord/turn_bridge/status_panel.rs", "complete_status_panel_v2")], 1)
        self.assertEqual(found[("src/services/discord/turn_bridge/terminal_outcome_delivery.rs", "run_terminal_outcome_delivery")], 1)

    def test_discovers_method_multiline_macro_and_both_ufcs_forms(self) -> None:
        source = """
async fn method() { gateway\n . send_message ( ); }
async fn macro_call() { wrap!(gateway.edit_message()); }
async fn trait_ufcs() { TurnGateway::edit_message(gateway); }
async fn qualified() { <G as TurnGateway>::send_files(gateway); }
async fn nested_generic() { <Wrapper<u8> as TurnGateway>::create_message(gateway); }
"""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); path = root / "src/fixture.rs"; path.parent.mkdir()
            path.write_text(source)
            found = checker.discover_direct_sends(root, {"src/fixture.rs"})
        self.assertEqual(found, {("src/fixture.rs", name): 1 for name in ("method", "macro_call", "trait_ufcs", "qualified", "nested_generic")})

    def test_comments_strings_raw_strings_and_fake_fn_do_not_count_or_reassign(self) -> None:
        source = r'''
async fn real() {
  // fn fake() { gateway.send_message(); }
  let a = "fn fake2() { gateway.edit_message(); }";
  let b = r#"gateway.create_message(); fn fake3() {"#;
  /* nested /* gateway.send_files(); */ fn fake4() { */
  macro_rules! never_called { () => { gateway.create_message(); } }
  gateway.send_message();
}
'''
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); path = root / "src/fixture.rs"; path.parent.mkdir()
            path.write_text(source)
            found = checker.discover_direct_sends(root, {"src/fixture.rs"})
        self.assertEqual(found, {("src/fixture.rs", "real"): 1})

    def test_unlisted_file_and_delete_message_are_outside_claim(self) -> None:
        self.validate([row()], "async fn publish() { gateway.send_message(); gateway.delete_message(); }\n", **{"src/outside.rs": "fn outside(){ gateway.send_message(); }"})

    def test_orphan_and_missing_direct_operations_are_rejected(self) -> None:
        source = "fn publish(){gateway.send_message();}\nfn orphan(){gateway.edit_message();}"
        self.reject([row()], "orphan direct send .*orphan", source)
        self.reject([row(direct_send_count=2)], "missing direct send")

    def test_line_numbers_are_not_identity(self) -> None:
        self.validate([row()], "\n\n\nasync fn publish() { gateway.send_message(); }\n")

    def test_entry_symbol_must_be_a_real_function_definition(self) -> None:
        self.reject([row(entry_symbol="not_a_function")], "entry function.*missing", "const not_a_function: &str = \"name\"; fn publish(){gateway.send_message();}")

    def test_cross_file_homonym_is_not_contract_evidence(self) -> None:
        self.reject([row(transport_symbols=["helper::transport"])], "row-linked symbol", **{"src/other.rs": "fn unrelated(){ helper::transport(); }"})

    def test_explicit_cross_file_contract_path_is_linked(self) -> None:
        linked = row(
            transport_symbols=["helper::transport"],
            contract_paths=[{"file": "src/other.rs", "entry_symbol": "route"}],
        )
        self.validate([linked], **{"src/other.rs": "fn route(){ helper::transport(); }"})

    def test_contract_path_entry_must_exist(self) -> None:
        linked = row(transport_symbols=["helper::transport"], contract_paths=[{"file": "src/other.rs", "entry_symbol": "missing"}])
        self.reject([linked], "entry function.*missing", **{"src/other.rs": "fn route(){ helper::transport(); }"})

    def test_strict_schema_rejects_unexpected_and_missing_fields(self) -> None:
        unexpected = row(extra="no")
        self.reject([unexpected], "unexpected=.*extra")
        missing = row(); del missing["authorities"]
        self.reject([missing], "missing=.*authorities")

    def test_boolean_and_integer_types_are_strict(self) -> None:
        self.reject([row(multi_op="false")], "must be booleans")
        self.reject([row(direct_send_count=True)], "integer >= 0")
        self.reject([row(timeout_retry={"timeout_ms": False, "policy": "none"})], "invalid timeout_retry")

    def test_absolute_parent_and_non_source_paths_are_rejected(self) -> None:
        for bad in ("/src/fixture.rs", "src/../escape.rs", "tests/fixture.rs"):
            data = payload([row(file=bad)], [bad])
            with self.assertRaisesRegex(checker.ManifestError, r"normalized|src/\*\*/\*.rs"):
                self.validate_data(data, {"src/fixture.rs": "fn publish(){}"})

    def test_resolved_path_must_remain_inside_root(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "repo"; (root / "src").mkdir(parents=True)
            outside = Path(temporary) / "outside.rs"; outside.write_text("fn publish(){}")
            (root / "src/escape.rs").symlink_to(outside)
            manifest = root / "manifest.json"; manifest.write_text(json.dumps(payload([row(file="src/escape.rs")], ["src/escape.rs"])))
            with self.assertRaisesRegex(checker.ManifestError, "escapes repository root"):
                checker.load_and_validate(manifest, root, enforce_canonical=False)

    def test_missing_scope_file_has_manifest_error(self) -> None:
        with self.assertRaisesRegex(checker.ManifestError, "source file missing"):
            self.validate_data(payload([row(file="src/missing.rs")], ["src/missing.rs"]), {})

    def test_dns_rejects_all_uncertain_classes(self) -> None:
        for uncertainty in ("AMBIG", "PARTIAL", "SBU", "POSTCOMMIT_AMBIG"):
            with self.subTest(uncertainty=uncertainty):
                self.reject([row(failure_classes=["DNS", uncertainty], multi_op=uncertainty == "PARTIAL", success_then_settlement=uncertainty == "SBU")], "DNS cannot coexist")

    def test_partial_and_sbu_implications_are_bidirectional(self) -> None:
        self.reject([row(multi_op=True)], "multi_op and PARTIAL")
        self.reject([row(failure_classes=["PARTIAL"])], "multi_op and PARTIAL")
        self.reject([row(success_then_settlement=True)], "success_then_settlement and SBU")
        self.reject([row(failure_classes=["SBU"])], "success_then_settlement and SBU")

    def test_postcommit_ambiguity_requires_sbu(self) -> None:
        self.reject([row(failure_classes=["POSTCOMMIT_AMBIG"])], "requires SBU")

    def test_success_then_settlement_requires_linked_ordered_evidence(self) -> None:
        good = row(
            failure_classes=["SBU"], success_then_settlement=True,
            settlement_symbols=["ledger::settle"],
            post_success={"file": "src/fixture.rs", "entry_symbol": "publish", "transport_symbol": "gateway.send_message", "settlement_symbol": "ledger::settle"},
        )
        self.validate([good], "fn publish(){gateway.send_message(); ledger::settle();}")
        bad = copy.deepcopy(good); bad["post_success"]["settlement_symbol"] = "other::settle"
        self.reject([bad], "reference row claims", "fn publish(){gateway.send_message(); ledger::settle();}")
        self.reject([good], "settlement must appear after", "fn publish(){ledger::settle(); gateway.send_message();}")

    def test_generic_settlement_word_is_rejected(self) -> None:
        self.reject([row(settlement_symbols=["commit"])], "generic settlement", "fn publish(){gateway.send_message(); commit();}")

    def test_spawned_contract_is_fixed_and_linked_to_spawn_body(self) -> None:
        spawn = {"file": "src/spawn.rs", "entry_symbol": "launch", "spawn_api": "tokio::spawn", "target_symbol": "publish"}
        executor = {"owner": "worker", "mode": "spawned", "send_contract": "tokio_spawn_future", "spawn": spawn}
        self.validate([row(executor=executor)], **{"src/spawn.rs": "fn launch(){ tokio::spawn(async move { publish(); }); }"})
        fake = copy.deepcopy(executor); fake["send_contract"] = "future is Send + 'static"
        self.reject([row(executor=fake)], "fixed Send contract", **{"src/spawn.rs": "fn launch(){ tokio::spawn(async move { publish(); }); }"})
        unlinked = copy.deepcopy(executor); unlinked["spawn"]["target_symbol"] = "missing"
        self.reject([row(executor=unlinked)], "inside spawn call", **{"src/spawn.rs": "fn launch(){ tokio::spawn(async move { publish(); }); }"})
        outside = copy.deepcopy(executor); outside["spawn"]["target_symbol"] = "after"
        self.reject([row(executor=outside)], "inside spawn call", **{"src/spawn.rs": "fn launch(){ tokio::spawn(async move {}); after(); }"})
        decoy = "fn launch(){ if false { tokio::spawn(async move { publish(); }); } publish(); }"
        self.reject([row(executor=executor)], "inside spawn call", **{"src/spawn.rs": decoy})
        mismatch = copy.deepcopy(executor); mismatch["spawn"]["spawn_api"] = "task_supervisor::spawn_observed"
        self.reject([row(executor=mismatch)], "does not match", **{"src/spawn.rs": "fn launch(){ task_supervisor::spawn_observed(async move { publish(); }); }"})

    def test_duplicate_path_id_and_authority_cycle_are_rejected(self) -> None:
        duplicate = row(entry_symbol="other", direct_send_count=0)
        self.reject([row(), duplicate], "duplicate path_id", "fn publish(){gateway.send_message();} fn other(){}")
        a = row(path_id="a", authority_order_after=["b"])
        b = row(path_id="b", entry_symbol="other", transport_symbols=["helper::transport"], direct_send_count=0, authority_order_after=["a"])
        self.reject([a, b], "authority order cycle", "fn publish(){gateway.send_message();} fn other(){helper::transport();}")

    def test_materially_false_rows_are_corrected(self) -> None:
        data = json.loads((REPO_ROOT / "scripts/discord_publication_boundaries.json").read_text())
        rows = {item["path_id"]: item for item in data["rows"]}
        self.assertEqual(rows["watcher-pre-emit"]["transport_symbols"], ["http::edit_channel_message", "schedule_discord_retry_with_history_completion_release"])
        self.assertEqual(rows["watcher-abort"]["transport_symbols"], ["http::edit_channel_message", "http::send_channel_message"])
        self.assertEqual(rows["watcher-rollover"]["settlement_symbols"], [])
        self.assertEqual(rows["bridge-recovery-retry"]["settlement_symbols"], ["release_retry_pending"])
        self.assertEqual(rows["plain-bridge-answer"]["direct_send_count"], 1)

    def test_canonical_scope_identity_and_exclusions_are_pinned(self) -> None:
        data = json.loads((REPO_ROOT / "scripts/discord_publication_boundaries.json").read_text())
        for field in ("scope", "identity", "excluded"):
            mutant = copy.deepcopy(data)
            mutant[field] = [] if field == "excluded" else "drifted"
            with tempfile.NamedTemporaryFile("w", suffix=".json") as manifest:
                json.dump(mutant, manifest); manifest.flush()
                with self.assertRaisesRegex(checker.ManifestError, "pinned Phase-A contract|explicit exclusions"):
                    checker.load_and_validate(Path(manifest.name), REPO_ROOT)

    def test_malformed_nested_evidence_is_a_manifest_error(self) -> None:
        malformed = row(contract_paths=[{"file": "src/fixture.rs", "entry_symbol": []}])
        self.reject([malformed], "non-empty strings")
        malformed_spawn = {"owner": "worker", "mode": "spawned", "send_contract": "tokio_spawn_future", "spawn": {"file": "src/fixture.rs", "entry_symbol": "publish", "spawn_api": [], "target_symbol": "publish"}}
        self.reject([row(executor=malformed_spawn)], "non-empty strings")

    def test_ci_lane_runs_checker_and_tests(self) -> None:
        lane = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn('scripts/check_discord_publication_boundaries.py', lane)
        self.assertIn('tests.test_discord_publication_boundaries', lane)


if __name__ == "__main__": unittest.main()
