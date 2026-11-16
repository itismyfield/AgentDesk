"""Contracts for the base-recomputing module-layer gate (#8131 decision B).

Each fixture is a git repository whose first commit is "main" (refs/remotes/origin/main); the
candidate is a later commit or the working tree. Only increases against that base may block.
"""
from __future__ import annotations

import ast
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT / "scripts") not in sys.path:
    sys.path.insert(0, str(ROOT / "scripts"))

from audit_maintainability.checks import module_layer_gate as gate  # noqa: E402
from audit_maintainability.checks.module_layer_baseline import BASELINE, _id  # noqa: E402

LAYERS = """schema_version = 1
rules = [{prefix="",layer="assembly",reason="fixture assembly"},
         {prefix="lower",layer="foundation",reason="fixture lower"},
         {prefix="upper",layer="assembly",reason="fixture upper"},
         {prefix="spare",layer="assembly",reason="fixture spare"}]
[layers]
foundation = 0
assembly = 1
"""
LOWER = "src/lib.rs::crate::lower"
UPPER = "src/lib.rs::crate::upper"
SPARE = "src/lib.rs::crate::spare"
LOCAL = {}


def source(lower_body, extra=""):
    return (f"mod lower {{ pub fn run() {{ {lower_body} }} }}\n"
            "mod upper { pub fn hit() {} }\nmod spare { pub fn hit() {} }\n" + extra)


class Repo:
    def __init__(self, root: Path):
        self.root = root

    def git(self, *args):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("GIT_", "GITHUB_"))}
        result = subprocess.run(["git", "-C", str(self.root), *args], capture_output=True, text=True, env=env)
        if result.returncode:
            raise AssertionError(result.stderr)
        return result.stdout.strip()

    def put(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def remove(self, relative):
        (self.root / relative).unlink()

    def commit(self, message="fixture"):
        self.git("add", "-A")
        self.git("commit", "-qm", message, "--allow-empty")
        return self.git("rev-parse", "HEAD")

    def main(self, sha=None):
        self.git("update-ref", "refs/remotes/origin/main", sha or self.git("rev-parse", "HEAD"))


class GateFixture(unittest.TestCase):
    def repo(self, lower_body="crate::upper::hit();", layers=LAYERS, extra=""):
        temporary = TemporaryDirectory(prefix="module-layer-gate-")
        self.addCleanup(temporary.cleanup)
        repo = Repo(Path(temporary.name))
        repo.git("init", "-q", "-b", "work")
        repo.git("config", "user.name", "Fixture")
        repo.git("config", "user.email", "fixture@example.invalid")
        repo.git("config", "commit.gpgsign", "false")
        repo.put("Cargo.toml", '[package]\nname="fixture"\nversion="0.1.0"\nedition="2024"\n')
        repo.put("src/lib.rs", source(lower_body, extra))
        repo.put("scripts/audit_maintainability/layers.toml", layers)
        repo.commit("main")
        repo.main()
        return repo

    def run_gate(self, repo, env=None):
        return gate.evaluate(repo.root, LOCAL if env is None else env)

    def codes(self, result):
        return sorted(e["code"] for e in result["errors"])

    def approval(self, repo, name, **tables):
        lines = ["schema_version = 1", f"issue = {int(name.split('-')[0])}", 'summary = "reviewed fixture approval"']
        for kind, rows in tables.items():
            lines.append(f"{kind} = [")
            lines.extend("  { " + ", ".join(f"{k} = {json.dumps(v)}" for k, v in row.items()) + " }," for row in rows)
            lines.append("]")
        repo.put(f"{gate.APPROVALS}/{name}.toml", "\n".join(lines) + "\n")

    def edge(self, result, source, target):
        rows = [r for r in result["draft"]["head"]["reverse_dependencies"] if r["source"] == source and r["target"] == target]
        self.assertEqual(len(rows), 1, rows)
        return rows[0]


class IncreaseAndDecrease(GateFixture):
    def test_new_reverse_debt_blocks(self):
        repo = self.repo()
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::spare::hit();"))
        repo.commit("candidate")
        result = self.run_gate(repo)
        self.assertIn("new-id", self.codes(result))
        self.assertIn("mode=local", result["summary"])

    def test_growth_of_existing_debt_blocks(self):
        repo = self.repo()
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::upper::hit();"))
        self.assertEqual(self.codes(self.run_gate(repo)), ["increase"])

    def test_decrease_and_unchanged_pass_and_report_the_reduction(self):
        repo = self.repo()
        self.assertEqual(self.codes(self.run_gate(repo)), [])
        repo.put("src/lib.rs", source(""))
        repo.commit("remove debt")
        result = self.run_gate(repo)
        self.assertEqual(self.codes(result), [])
        self.assertIn("reverse 1 id/1 occ", result["summary"])

    def test_base_is_main_not_an_earlier_branch_commit(self):
        repo = self.repo()
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::spare::hit();"))
        repo.commit("debt on the branch")
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::spare::hit();", "// later\n"))
        repo.commit("unrelated follow-up")
        self.assertIn("new-id", self.codes(self.run_gate(repo)))


class Approvals(GateFixture):
    def new_spare_debt(self, calls=1):
        repo = self.repo()
        repo.put("src/lib.rs", source("crate::upper::hit();" + " crate::spare::hit();" * calls))
        row = self.edge(self.run_gate(repo), LOWER, SPARE)
        return repo, row

    def increase(self, row, max_count, reason="accepted-debt"):
        return {"section": "reverse_dependencies", "id": row["id"], "source": row["source"],
                "target": row["target"], "kind": row["kind"], "max_count": max_count, "reason": reason}

    def test_approved_increase_passes_and_ceiling_is_exact(self):
        repo, row = self.new_spare_debt()
        self.approval(repo, "8131-spare", increase=[self.increase(row, 1)])
        self.assertEqual(self.codes(self.run_gate(repo)), [])
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::spare::hit(); crate::spare::hit();"))
        self.assertEqual(self.codes(self.run_gate(repo)), ["approval-exceeded", "new-id"])

    def test_approval_for_another_debt_does_not_admit(self):
        repo, row = self.new_spare_debt()
        other = {**row, "target": UPPER, "id": _id("edge", LOWER, UPPER, row["kind"])}
        self.approval(repo, "8131-other", increase=[self.increase(other, 5)])
        self.assertEqual(self.codes(self.run_gate(repo)), ["approval-unused", "new-id"])

    def test_unused_and_placeholder_approvals_fail(self):
        repo = self.repo()
        row = {"source": LOWER, "target": UPPER, "kind": "path", "id": _id("edge", LOWER, UPPER, "path")}
        self.approval(repo, "8131-unused", increase=[self.increase(row, 3)])
        self.assertIn("approval-unused", self.codes(self.run_gate(repo)))
        repo.put(f"{gate.APPROVALS}/8131-todo.toml", 'schema_version = 1\nissue = 8131\nsummary = "TODO: why"\n')
        self.assertIn("approval-invalid", self.codes(self.run_gate(repo)))

    def test_landed_approval_is_inert_and_immutable(self):
        repo, row = self.new_spare_debt()
        self.approval(repo, "8131-landed", increase=[self.increase(row, 2)])
        landed = repo.commit("land debt with approval")
        repo.main(landed)
        self.assertEqual(self.codes(self.run_gate(repo)), [])
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::spare::hit(); crate::spare::hit();"))
        self.assertEqual(self.codes(self.run_gate(repo)), ["increase"])
        repo.put("src/lib.rs", source("crate::upper::hit();"))
        cleared = repo.commit("remove the approved debt")
        repo.main(cleared)
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::spare::hit();"))
        self.assertEqual(self.codes(self.run_gate(repo)), ["new-id"])
        repo.remove(f"{gate.APPROVALS}/8131-landed.toml")
        self.assertEqual(self.codes(self.run_gate(repo)), ["approval-immutable", "new-id"])

    def test_readding_a_deleted_landed_approval_is_refused(self):
        repo, row = self.new_spare_debt()
        self.approval(repo, "8131-landed", increase=[self.increase(row, 1)])
        repo.main(repo.commit("land"))
        text = (repo.root / gate.APPROVALS / "8131-landed.toml").read_text()
        repo.put(f"{gate.APPROVALS}/8131-landed.toml", text + "# edited\n")
        self.assertEqual(self.codes(self.run_gate(repo)), ["approval-immutable"])

    def test_move_needs_approval_and_increase_applies_after_normalization(self):
        repo = self.repo()
        moved = "mod lower {{ pub mod inner {{ pub fn run() {{ {} }} }} }}\nmod upper {{ pub fn hit() {{}} }}\nmod spare {{ pub fn hit() {{}} }}\n"
        repo.put("src/lib.rs", moved.format("crate::upper::hit();"))
        result = self.run_gate(repo)
        self.assertEqual(self.codes(result), ["new-id"])
        old = _id("edge", LOWER, UPPER, "path")
        new = self.edge(result, LOWER + "::inner", UPPER)
        move = {"section": "reverse_dependencies", "old_id": old, "new_id": new["id"], "reason": "split lower"}
        self.approval(repo, "8131-move", move=[move])
        self.assertEqual(self.codes(self.run_gate(repo)), [])
        repo.put("src/lib.rs", moved.format("crate::upper::hit(); crate::upper::hit();"))
        self.assertEqual(self.codes(self.run_gate(repo)), ["increase"])
        self.approval(repo, "8131-move", move=[move], increase=[self.increase(new, 2)])
        self.assertEqual(self.codes(self.run_gate(repo)), [])
        repo.put("src/lib.rs", moved.format("crate::upper::hit(); crate::upper::hit(); crate::upper::hit();"))
        self.assertEqual(self.codes(self.run_gate(repo)), ["approval-exceeded", "increase"])

    def test_cross_section_move_is_refused(self):
        repo = self.repo(layers=LAYERS.replace(']\n[layers]', ']\nunclassified = [{prefix="mixed",reason="fixture"}]\n[layers]'),
                         extra="mod mixed { pub fn hit() {} }\n")
        repo.put("src/lib.rs", source("", "mod mixed { pub fn hit() { crate::upper::hit(); } }\n"))
        head = self.run_gate(repo)["draft"]["head"]
        indeterminate = head["indeterminate_dependencies"][0]["id"]
        self.approval(repo, "8131-cross", move=[{"section": "reverse_dependencies", "old_id": _id("edge", LOWER, UPPER, "path"),
                                                  "new_id": indeterminate, "reason": "moved into mixed"}])
        self.assertIn("approval-move", self.codes(self.run_gate(repo)))

    def test_hidden_reverse_is_not_approvable(self):
        repo = self.repo()
        repo.put("src/lib.rs", source("crate::upper::absent();"))
        result = self.run_gate(repo)
        self.assertIn("hidden-reverse", self.codes(result))
        uncertainty = result["draft"]["head"]["uncertainties"]
        self.approval(repo, "8131-hidden", increase=[{"section": "uncertainties", "id": row["id"], "max_count": row["count"],
                                                      "reason": "accepted-debt"} for row in uncertainty])
        self.assertIn("hidden-reverse", self.codes(self.run_gate(repo)))

    def test_draft_approval_lists_blocking_increases_and_needs_review(self):
        repo, row = self.new_spare_debt()
        result = self.run_gate(repo)
        text = gate.draft_approval(result, "8131-draft")
        repo.put(f"{gate.APPROVALS}/8131-draft.toml", text)
        self.assertIn(row["id"], text)
        self.assertIn("approval-invalid", self.codes(self.run_gate(repo)))
        repo.put(f"{gate.APPROVALS}/8131-draft.toml", text.replace("TODO: 승인 사유를 리뷰받을 문장으로 쓴다", "reviewed"))
        self.assertEqual(self.codes(self.run_gate(repo)), [])


class LayerRules(GateFixture):
    def test_declassification_needs_reclassify_and_unused_reclassify_fails(self):
        repo = self.repo()
        repo.put("scripts/audit_maintainability/layers.toml",
                 LAYERS.replace(']\n[layers]', ']\nunclassified = [{prefix="spare",reason="fixture"}]\n[layers]'))
        self.assertEqual(self.codes(self.run_gate(repo)), ["declassified"])
        self.approval(repo, "8131-spare", reclassify=[{"prefix": "spare", "reason": "spare has no owner yet"}])
        self.assertEqual(self.codes(self.run_gate(repo)), [])
        self.approval(repo, "8131-spare", reclassify=[{"prefix": "spare", "reason": "r"}, {"prefix": "upper", "reason": "r"}])
        self.assertEqual(self.codes(self.run_gate(repo)), ["approval-unused"])

    def test_mapping_only_reduction_needs_reclassify(self):
        repo = self.repo()
        repo.put("scripts/audit_maintainability/layers.toml", LAYERS.replace('prefix="lower",layer="foundation"', 'prefix="lower",layer="assembly"'))
        self.assertEqual(self.codes(self.run_gate(repo)), ["mapping-reduction"])
        self.approval(repo, "8131-lower", reclassify=[{"prefix": "lower", "reason": "lower orchestrates"}])
        self.assertEqual(self.codes(self.run_gate(repo)), [])

    def test_reverse_shaped_unresolved_reference_blocks_until_approved(self):
        repo = self.repo(lower_body="")
        repo.put("src/lib.rs", source("crate::upper::absent();"))
        result = self.run_gate(repo)
        self.assertEqual(self.codes(result), ["unresolved-reverse"])
        row = next(e for e in result["errors"] if e["code"] == "unresolved-reverse")
        self.approval(repo, "8131-unresolved", increase=[{"section": "uncertainties", "id": row["id"], "max_count": row["count"],
                                                          "reason": "accepted-debt"}])
        self.assertEqual(self.codes(self.run_gate(repo)), [])

    def test_tombstone_blocks_resurrection_but_coexists_before_removal(self):
        repo = self.repo()
        repo.put(BASELINE, "{}\n")
        self.assertEqual(self.codes(self.run_gate(repo)), ["tombstone"])
        repo.main(repo.commit("baseline still on main (before B2)"))
        self.assertEqual(self.codes(self.run_gate(repo)), [])


class BaseResolution(GateFixture):
    def pull_request(self):
        repo = self.repo()
        base = repo.git("rev-parse", "HEAD")
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::spare::hit();"))
        head = repo.commit("pr")
        repo.git("checkout", "-q", "--detach", base)
        repo.git("merge", "--no-ff", "-qm", "merge", head)
        return repo, base, head

    def ci(self, repo, event, payload, **extra):
        directory = TemporaryDirectory(prefix="module-layer-gate-event-")
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / "event.json"
        path.write_text(json.dumps(payload))
        return {"GITHUB_ACTIONS": "true", "GITHUB_EVENT_NAME": event, "GITHUB_EVENT_PATH": str(path), **extra}

    def test_pull_request_base_is_first_parent_and_validated(self):
        repo, base, head = self.pull_request()
        env = self.ci(repo, "pull_request", {"pull_request": {"head": {"sha": head}, "base": {"sha": base}}})
        self.assertEqual(gate.resolve_base(repo.root, env)["base"], base)
        result = self.run_gate(repo, env)
        self.assertIn("new-id", self.codes(result))
        self.assertIn("mode=pull_request", result["summary"])
        wrong = self.ci(repo, "pull_request", {"pull_request": {"head": {"sha": base}, "base": {"sha": base}}})
        self.assertEqual(self.codes(self.run_gate(repo, wrong)), ["gate-unavailable"])
        repo.git("checkout", "-q", head)
        self.assertEqual(self.codes(self.run_gate(repo, env)), ["gate-unavailable"])

    def test_push_compares_with_before(self):
        repo, base, _ = self.pull_request()
        merged = repo.git("rev-parse", "HEAD")
        env = self.ci(repo, "push", {"ref": "refs/heads/main", "before": base})
        self.assertEqual(gate.resolve_base(repo.root, env)["base"], base)
        self.assertIn("new-id", self.codes(self.run_gate(repo, env)))
        for payload in ({"ref": "refs/heads/main", "before": merged}, {"ref": "refs/heads/other", "before": base},
                        {"ref": "refs/heads/main", "before": "0" * 40}):
            with self.subTest(payload=payload):
                self.assertEqual(self.codes(self.run_gate(repo, self.ci(repo, "push", payload))), ["gate-unavailable"])

    def test_ci_refuses_base_override(self):
        repo, base, head = self.pull_request()
        env = self.ci(repo, "pull_request", {"pull_request": {"head": {"sha": head}, "base": {"sha": base}}},
                      MODULE_LAYER_BASE_REF="HEAD")
        result = self.run_gate(repo, env)
        self.assertEqual(self.codes(result), ["gate-unavailable"])
        self.assertIn("refused in CI", result["summary"])

    def test_ci_refuses_a_dirty_candidate(self):
        repo, base, head = self.pull_request()
        repo.put("src/lib.rs", source(""))
        env = self.ci(repo, "pull_request", {"pull_request": {"head": {"sha": head}, "base": {"sha": base}}})
        self.assertEqual(self.codes(self.run_gate(repo, env)), ["gate-unavailable"])

    def test_dispatch_is_self_comparison(self):
        repo, _, _ = self.pull_request()
        env = self.ci(repo, "workflow_dispatch", {})
        chosen = gate.resolve_base(repo.root, env)
        self.assertEqual((chosen["mode"], chosen["base"]), ("self", chosen["head"]))

    def test_local_override_must_be_on_main_and_main_is_required(self):
        repo = self.repo()
        main = repo.git("rev-parse", "HEAD")
        repo.put("src/lib.rs", source("crate::upper::hit(); crate::spare::hit();"))
        branch = repo.commit("branch")
        self.assertEqual(gate.resolve_base(repo.root, {})["base"], main)
        self.assertEqual(self.codes(self.run_gate(repo, {"MODULE_LAYER_BASE_REF": branch})), ["gate-unavailable"])
        self.assertEqual(gate.resolve_base(repo.root, {"MODULE_LAYER_BASE_REF": main})["base"], main)
        repo.git("update-ref", "-d", "refs/remotes/origin/main")
        self.assertEqual(self.codes(self.run_gate(repo)), ["gate-unavailable"])


class AnalyzerChange(GateFixture):
    def test_measuring_code_change_that_moves_reverse_debt_needs_approval(self):
        repo = self.repo()
        scripts = repo.root / "scripts"
        for path in (ROOT / "scripts").glob("*.py"):
            shutil.copy2(path, scripts / path.name)
        shutil.copytree(ROOT / "scripts/audit_maintainability/checks", scripts / "audit_maintainability/checks",
                        ignore=shutil.ignore_patterns("__pycache__"))
        for name in ("__init__.py", "common.py", "rust_accessor_scan.py"):
            shutil.copy2(ROOT / "scripts/audit_maintainability" / name, scripts / "audit_maintainability" / name)
        (scripts / "ci").mkdir()
        shutil.copy2(ROOT / "scripts/ci/rust_test_sources.py", scripts / "ci/rust_test_sources.py")
        provider = scripts / "audit_maintainability/checks/module_layer_baseline.py"
        real = provider.read_text()
        provider.write_text(real + "\n_real_baseline = baseline\n\ndef baseline(*args):\n"
                            "    out = _real_baseline(*args)\n    out['reverse_dependencies'] = []\n    return out\n")
        repo.main(repo.commit("main measures no reverse debt"))
        provider.write_text(real)
        result = self.run_gate(repo)
        self.assertEqual(self.codes(result), ["analyzer-change"])
        error = result["errors"][0]
        self.assertEqual((error["reverse_removed"], error["reverse_added"]), (0, 1))
        self.approval(repo, "8131-analyzer", analyzer=[{"digest": error["digest"], "reverse_removed": 0, "reverse_added": 1,
                                                        "reason": "restore the real measurement"}])
        self.assertEqual(self.codes(self.run_gate(repo)), [])
        self.assertIn("analyzer: changed(reverse -0/+1)", self.run_gate(repo)["summary"])

    def test_measuring_code_list_covers_its_local_imports(self):
        listed = set(gate.MEASURING_CODE)
        comparison = {"scripts/audit_maintainability/checks/module_layer_gate.py",
                      "scripts/audit_maintainability/checks/module_layer_ratchet.py"}
        for path in gate.MEASURING_CODE:
            package = Path(path).parent
            for node in ast.walk(ast.parse((ROOT / path).read_text("utf-8"))):
                names = []
                if isinstance(node, ast.ImportFrom):
                    if node.level:
                        base = package
                        for _ in range(node.level - 1):
                            base = base.parent
                        names = [str(base / (node.module or "").replace(".", "/")) if node.module else
                                 str(base / alias.name) for alias in node.names]
                    elif node.module:
                        names = ["scripts/" + node.module.removeprefix("scripts.").replace(".", "/")]
                elif isinstance(node, ast.Import):
                    names = ["scripts/" + alias.name.replace(".", "/") for alias in node.names]
                for name in names:
                    candidate = name + ".py"
                    if (ROOT / candidate).is_file() and candidate not in comparison:
                        self.assertIn(candidate, listed, f"{path} imports {candidate}")


class RequiredPath(unittest.TestCase):
    def test_audit_runs_for_every_pull_request_but_not_in_self_test_only_runs(self):
        from ci import script_check_inputs as inputs
        env = {"GITHUB_EVENT_NAME": "pull_request", "SCRIPT_CHECK_CHANGES_RESULT": "success",
               "SCRIPT_CHECK_CHANGED_FILES": json.dumps(["docs/README.md"])}
        self.assertNotIn("Maintainability audit", inputs.skipped_checks(env))
        self.assertNotIn("Maintainability audit", inputs.SELF_TEST_INPUTS)
        text = (ROOT / "scripts/ci-script-checks.sh").read_text("utf-8")
        self.assertIn('run_check cargo "Maintainability audit"', text)
        self.assertIn('"$PYTHON" scripts/audit_maintainability.py --check', text)
        self.assertIn("tests.test_module_layer_gate", text)

    def test_module_layers_check_stays_a_hard_gate_that_uses_this_gate(self):
        from audit_maintainability.checks import module_layers
        self.assertTrue(module_layers.CHECK.hard_gate)
        self.assertIn("module_layer_gate import evaluate", (ROOT / "scripts/audit_maintainability/checks/module_layers.py").read_text())


if __name__ == "__main__":
    unittest.main()
