import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts/ci"))
from pr_scope_core import CoreScopeError, MissingCoreTests, select_core_ids  # noqa: E402


def write_contracts(tmp_path):
    scripts = tmp_path / "scripts"
    (scripts / "ci").mkdir(parents=True)
    (scripts / "ci/targeted-test-selectors.txt").write_text("[core]\nmod::prefix_\n=mod::exact\n")
    (scripts / "ci/pg15_required_ids.txt").write_text("mod::pg15\n")
    (scripts / "ci/pg15_required_runtime_ids.txt").write_text("mod::pg15_runtime\n")
    (scripts / "relay_authority_contract_targets.json").write_text(json.dumps({
        "schema_version": 1,
        "lanes": [
            {"status": "active", "name": "relay", "minimum": 2,
             "command": ["cargo", "test", "--lib", "mod::relay_"]},
            {"status": "gap", "name": "unimplemented"},
        ],
        "condition3_mutations": [{"target": "mod::mutation"}],
        "authority_surface": [{"guards": ["entry_test:mod::entry", "known_gap:#42"]}],
    }))
    (scripts / "tui_o_body_probes.json").write_text(json.dumps({
        "schema_version": 1,
        "probes": [
            {"verified_by": ["mod::recording", "mod::verified"], "recording_test": "recording"},
            {"verified_by": []},
        ],
        "transport_probes": [{"verified_by": ["mod::transport"]}],
        "claim_probes": [{"verified_by": ["mod::claim"]}],
    }))
    core = {("agentdesk", f"mod::{name}") for name in (
        "prefix_hit", "exact", "pg15", "relay_a", "relay_b", "mutation", "entry", "recording",
        "verified", "transport", "claim",
    )} | {("adk-core", "mod::prefix_hit"), ("adk-runtime", "mod::pg15_runtime")}
    return core


class CoreScopeTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.repo = Path(tmp.name)
        self.core = write_contracts(self.repo)

    def test_core_selects_qualified_ids_for_each_contract(self):
        distractors = {("agentdesk", name) for name in (
            "other::recording", "mod::pg15_similar", "other::entry", "mod::exact_similar", "mod::unimplemented",
        )}
        self.assertEqual(select_core_ids(self.repo, self.core | distractors), self.core)

    def test_missing_required_core_is_reported(self):
        for lost in ("relay_b", "pg15", "entry", "exact", "recording"):
            with self.subTest(lost=lost), self.assertRaises(MissingCoreTests):
                select_core_ids(self.repo, self.core - {("agentdesk", f"mod::{lost}")})

    def test_test_in_another_library_cannot_replace_a_root_contract(self):
        replacements = self.core - {("agentdesk", "mod::pg15")} | {("adk-core", "mod::pg15")}
        with self.assertRaisesRegex(MissingCoreTests, "mod::pg15"):
            select_core_ids(self.repo, replacements)

    def test_unreadable_core_contract_is_reported(self):
        (self.repo / "scripts/tui_o_body_probes.json").write_text('{"schema_version": 1, "probes": []}')
        with self.assertRaisesRegex(CoreScopeError, "census"):
            select_core_ids(self.repo, self.core)
