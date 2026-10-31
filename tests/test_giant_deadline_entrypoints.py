"""Run deadline policy through the subprocess entrypoints used by CI and prepush."""

from __future__ import annotations

import ast
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from datetime import date, datetime, timedelta, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
EVALUATION_DATE = "2026-11-01"
OVERDUE_PREFIX = "WARNING: giant-file overdue:"
GIANT = "src/config.rs"
ISSUE = "#5745"
DEADLINE = "2026-10-01"
PREPUSH_SEQUENCE = (
    "python3 scripts/giant_file_progress.py && "
    "python3 scripts/generate_env_reference.py && "
    "python3 scripts/generate_cli_reference.py && git diff --exit-code --stat"
)


class DeadlineEntrypointsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="giant-deadline-entrypoints-")
        cls.template = Path(cls.temporary.name) / "template"
        cls.template.mkdir()
        scripts = cls.template / "scripts"
        scripts.mkdir()
        for filename in (
            "generate_inventory_docs.py", "giant_file_progress.py", "ratchet_admission.py",
            "generate_env_reference.py", "generate_cli_reference.py", "check_hotfile_ratchet.py",
            "audit_maintainability.py", "rust_lex.py",
        ):
            shutil.copy2(ROOT / "scripts" / filename, scripts / filename)
        shutil.copytree(ROOT / "scripts/audit_maintainability", scripts / "audit_maintainability",
                        ignore=shutil.ignore_patterns("__pycache__"))
        tree = ast.parse((scripts / "generate_inventory_docs.py").read_text())
        purposes = next(ast.literal_eval(node.value) for node in tree.body
                        if isinstance(node, ast.Assign)
                        and any(isinstance(target, ast.Name)
                                and target.id == "TOP_LEVEL_MODULE_PURPOSES"
                                for target in node.targets))
        for name in purposes:
            path = cls.template / "src" / name
            if name.endswith("/"):
                path.mkdir(parents=True, exist_ok=True)
            else:
                cls.put_at(cls.template, "src/" + name, "// fixture\n")
        cls.put_at(cls.template, GIANT, "pub fn fixture() {}\n" * 1200)
        cls.put_at(cls.template, "src/server/routes/mod.rs",
                   "fn compose_api_router() { Router::new().merge(health::router()) }\n")
        cls.put_at(cls.template, "src/server/routes/health.rs",
                   'fn router() { Router::new().route("/health", get(health)) }\n'
                   "fn health() {}\n")
        cls.put_at(cls.template, "src/server/web_surface.rs", "fn web_surface() {}\n")
        cls.put_at(cls.template, "src/server/worker_registry.rs",
                   "pub(crate) const WORKER_SPECS: [WorkerSpec; 0] = [\n];\n")
        cls.put_at(cls.template, "src/cli/args.rs",
                   "#[derive(Parser)]\nstruct Cli {\n"
                   "#[command(subcommand)]\ncommand: Command,\n}\n"
                   "#[derive(Subcommand)]\nenum Command {\nStatus,\n}\n")
        hotfiles = ("src/services/discord/tmux_watcher.rs",
                    "src/services/discord/tui_prompt_relay.rs",
                    "src/services/discord/turn_bridge/mod.rs")
        for path in hotfiles:
            cls.put_at(cls.template, path, "// hotfile fixture\n")
        cls.put_at(cls.template, "scripts/hotfile_ratchet.toml", "[hotfile_ratchet]\n"
                   + f'"{GIANT}" = 1200\n'
                   + "".join(f'"{path}" = 1\n' for path in hotfiles))
        cls.put_at(cls.template, "scripts/audit_maintainability_giant_baseline.toml",
                   f'[giant_file_ratchet]\n"{GIANT}" = 1200\n')
        cls.put_at(cls.template, "scripts/ratchet_admission_history.toml", "schema_version = 1\n")
        cls.put_at(cls.template, ".gitignore", "/target/\n__pycache__/\n*.pyc\n"
                   "/docs/generated/module-inventory.md\n/docs/generated/giant-file-registry.md\n")
        cls.put_at(cls.template, "ARCHITECTURE.md",
                   "<!-- BEGIN GENERATED: SRC TREE -->\n<!-- END GENERATED: SRC TREE -->\n"
                   "<!-- BEGIN GENERATED: TOP LEVEL MODULE MAP -->\n"
                   "<!-- END GENERATED: TOP LEVEL MODULE MAP -->\n")
        # Deliberately exhausted/malformed reset history cannot require a repair commit.
        cls.put_at(cls.template, "scripts/giant_file_registry.toml",
                   "# DEADLINE RESET: 2000-01-01 -> 2026-10-01 (extension > 92 days)\n"
                   "# DEADLINE RESET malformed historical bookkeeping\n"
                   "grandfathered=[]\ngrandfathered_baseline_paths=[]\n"
                   f'[[entry]]\nfile="{GIANT}"\ndecision="shrink"\nowner="team"\n'
                   f'deadline="{DEADLINE}"\ndecompose_issue="{ISSUE}"\n')
        cls.put_at(cls.template, "scripts/giant_file_closed_issue_transition_list.txt",
                   "# Closed issue is deliberately absent from the transition list.\n")
        now = datetime.now(timezone.utc)
        metadata = {
            "schema_version": 2, "refreshed_at": now.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "ratchets": {"closed_deadline_entries": 0, "transition_list_entries": 0},
            "issues": [{"number": 5745, "state": "open", "title": "split fixture",
                        "owners": ["team"], "files": [GIANT]}],
        }
        cls.put_at(cls.template, "scripts/giant_file_issue_metadata.json", json.dumps(metadata))
        # Prepare docs before expiring the plan so hard-failure mutants reach test assertions.
        for script in ("generate_inventory_docs.py", "generate_env_reference.py",
                       "generate_cli_reference.py"):
            result = cls.command_at(cls.template, [sys.executable, "scripts/" + script],
                                    {"GIANT_FILE_EVALUATION_DATE": DEADLINE})
            if result.returncode:
                raise AssertionError(result.stdout + result.stderr)
        metadata["refreshed_at"] = (now - timedelta(days=31)).strftime("%Y-%m-%dT%H:%M:%SZ")
        metadata["issues"][0]["state"] = "closed"
        cls.put_at(cls.template, "scripts/giant_file_issue_metadata.json", json.dumps(metadata))
        cls.git_at(cls.template, "init", "-q", "-b", "main")
        cls.git_at(cls.template, "config", "user.name", "Deadline Fixture")
        cls.git_at(cls.template, "config", "user.email", "fixture@example.invalid")
        cls.git_at(cls.template, "add", "-A")
        cls.git_at(cls.template, "commit", "-qm", "fixture baseline")
        cls.base = cls.git_at(cls.template, "rev-parse", "HEAD")

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    @staticmethod
    def put_at(repo, path, content):
        target = repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

    @staticmethod
    def command_at(repo, command, extra_env=None):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("GFP_", "GITHUB_", "GIT_"))}
        env.update({"GIANT_FILE_EVALUATION_DATE": EVALUATION_DATE,
                    "PYTHONDONTWRITEBYTECODE": "1", "LC_ALL": "C"})
        env.update(extra_env or {})
        return subprocess.run(command, cwd=repo, env=env, capture_output=True, text=True,
                              timeout=90)

    @classmethod
    def git_at(cls, repo, *args):
        result = cls.command_at(repo, ["git", *args])
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)
        return result.stdout.strip()

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="giant-deadline-case-")
        self.addCleanup(self.directory.cleanup)
        self.repo = Path(self.directory.name) / "repo"
        shutil.copytree(self.template, self.repo)

    def candidate(self, changes=None):
        for path, content in (changes or {}).items():
            self.put_at(self.repo, path, content)
        self.git_at(self.repo, "add", "-A")
        self.git_at(self.repo, "commit", "-qm", "fixture candidate", "--allow-empty")
        head = self.git_at(self.repo, "rev-parse", "HEAD")
        self.git_at(self.repo, "checkout", "-q", "--detach", self.base)
        self.git_at(self.repo, "merge", "--no-ff", "-qm", "PR candidate", head)
        candidate = self.git_at(self.repo, "rev-parse", "HEAD")
        self.git_at(self.repo, "update-ref", "refs/remotes/origin/main", candidate)
        return {"GFP_EVENT_NAME": "pull_request", "GFP_REPOSITORY": "itismyfield/AgentDesk",
                "GFP_HEAD_REPOSITORY": "itismyfield/AgentDesk", "GFP_BASE_SHA": self.base,
                "GFP_HEAD_SHA": head, "GFP_CANDIDATE_SHA": candidate, "GFP_REFRESH_DOCS": "1"}

    def assert_warning_pass(self, result, *, count=1, fixture=True):
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, output)
        warnings = [line for line in output.splitlines() if line.startswith(OVERDUE_PREFIX)]
        self.assertEqual(len(warnings), count, output)
        if fixture:
            for detail in (GIANT, DEADLINE, ISSUE):
                self.assertIn(detail, warnings[0])
            stale = next(line for line in output.splitlines() if "older than 30 days" in line)
            for detail in ("giant_file_issue_metadata.json", "expired after", ISSUE):
                self.assertIn(detail, stale)
            closed = next(line for line in output.splitlines() if "has a dead deadline" in line)
            for detail in (GIANT, DEADLINE, ISSUE):
                self.assertIn(detail, closed)
            self.assertNotIn("FATAL:", output)

    def progress(self, event):
        env = self.candidate()
        env.update({"GFP_EVENT_NAME": event, "GFP_REF": "refs/heads/main",
                    "GFP_REF_PROTECTED": "true"})
        return self.command_at(self.repo, [sys.executable, "scripts/giant_file_progress.py"], env)

    def test_pr_refresh_accepts_expired_closed_plan_without_bookkeeping(self):
        self.assert_warning_pass(self.progress("pull_request"))

    def test_raw_giant_growth_warns_in_ci_audit_and_prepush(self):
        env = self.candidate({GIANT: (self.repo / GIANT).read_text() + "// one raw line\n"})
        commands = ([sys.executable, "scripts/giant_file_progress.py"],
                    [sys.executable, "scripts/audit_maintainability.py", "--check"],
                    ["sh", "-c", PREPUSH_SEQUENCE])
        for command in commands:
            with self.subTest(command=command):
                result = self.command_at(self.repo, command, env)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                warnings = [line for line in result.stderr.splitlines() if "WARNING:" in line
                            or "warning" in line]
                self.assertTrue(any(GIANT in line and "+1 raw lines" in line
                                    for line in warnings), result.stderr)

    def test_threshold_crossing_warns_in_ci_audit_and_prepush(self):
        path = "src/cli/near_giant.rs"
        self.put_at(self.repo, path, "pub fn fixture() {}\n" * 999)
        result = self.command_at(self.repo, [sys.executable, "scripts/generate_inventory_docs.py"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.git_at(self.repo, "add", "-A")
        self.git_at(self.repo, "commit", "-qm", "near giant baseline")
        self.base = self.git_at(self.repo, "rev-parse", "HEAD")
        env = self.candidate({path: "pub fn fixture() {}\n" * 1000})
        for command in ([sys.executable, "scripts/giant_file_progress.py"],
                        [sys.executable, "scripts/audit_maintainability.py", "--check"],
                        ["sh", "-c", PREPUSH_SEQUENCE]):
            with self.subTest(command=command):
                result = self.command_at(self.repo, command, env)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertTrue(any(path in line and "+1 raw lines" in line
                                    for line in result.stderr.splitlines()), result.stderr)

    def test_push_refresh_accepts_expired_closed_plan_without_bookkeeping(self):
        self.assert_warning_pass(self.progress("push"))

    def test_dispatch_refresh_accepts_expired_closed_plan_without_bookkeeping(self):
        self.assert_warning_pass(self.progress("workflow_dispatch"))

    def test_inventory_check_accepts_expired_closed_plan_without_bookkeeping(self):
        result = self.command_at(self.repo,
                                 [sys.executable, "scripts/generate_inventory_docs.py", "--check"])
        self.assert_warning_pass(result)

    def test_prepush_giant_docs_sequence_accepts_expired_closed_plan(self):
        self.assert_warning_pass(self.command_at(self.repo, ["sh", "-c", PREPUSH_SEQUENCE],
                                                 self.candidate()))

    def test_unbounded_deadline_extension_needs_no_reset_record(self):
        registry = (self.repo / "scripts/giant_file_registry.toml").read_text()
        registry = "\n".join(line for line in registry.splitlines()
                             if not line.startswith("# DEADLINE RESET")) + "\n"
        registry = registry.replace(DEADLINE, "2030-01-01")
        env = self.candidate({"scripts/giant_file_registry.toml": registry})
        result = self.command_at(self.repo, [sys.executable, "scripts/giant_file_progress.py"], env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("WARNING:", result.stderr)
        self.assertIn("2030-01-01", result.stderr)

    def test_giant_growth_also_warns_in_hotfile_gate(self):
        env = self.candidate({GIANT: (self.repo / GIANT).read_text() + "// one raw line\n"})
        result = self.command_at(self.repo, [sys.executable, "scripts/giant_file_progress.py"], env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"giant growth: {GIANT}: +1 raw lines", result.stderr)
        result = self.command_at(self.repo, [sys.executable, "scripts/check_hotfile_ratchet.py"], env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"WARNING: giant growth: {GIANT}: +1 raw lines", result.stderr)

    def test_missing_deadline_and_issue_warn_without_bookkeeping(self):
        registry = (self.repo / "scripts/giant_file_registry.toml").read_text()
        registry = "\n".join(line for line in registry.splitlines()
                             if not line.startswith(("deadline=", "decompose_issue="))) + "\n"
        env = self.candidate({"scripts/giant_file_registry.toml": registry})
        for command in ([sys.executable, "scripts/giant_file_progress.py"],
                        [sys.executable, "scripts/generate_inventory_docs.py", "--check"],
                        ["sh", "-c", PREPUSH_SEQUENCE]):
            with self.subTest(command=command):
                result = self.command_at(self.repo, command, env)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("WARNING: giant-file registry:", result.stderr)
                self.assertIn("shrink deadline ''", result.stderr)
                self.assertIn("decompose_issue ''", result.stderr)

    def test_same_expired_fixture_still_rejects_generated_document_drift(self):
        path = "docs/generated/route-inventory.md"
        self.put_at(self.repo, path, (self.repo / path).read_text() + "drift\n")
        result = self.command_at(self.repo,
                                 [sys.executable, "scripts/generate_inventory_docs.py", "--check"])
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("stale generated doc: " + path, result.stdout)

    def test_same_expired_fixture_warns_for_unregistered_new_giant(self):
        env = self.candidate({"src/cli/unregistered.rs": "pub fn new_giant() {}\n" * 1000})
        result = self.command_at(self.repo, [sys.executable, "scripts/giant_file_progress.py"], env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("new giant: src/cli/unregistered.rs: +1000 raw lines", result.stderr)
        self.assertIn("src/cli/unregistered.rs", result.stderr)

    def test_current_registry_warnings_match_overdue_entries_in_all_entrypoints(self):
        # Use checked-in planning inputs with small measured files, keeping the test cheap.
        registry = (ROOT / "scripts/giant_file_registry.toml").read_text()
        entries = tomllib.loads(registry)["entry"]
        def overdue(entry):
            try:
                return date.fromisoformat(entry.get("deadline", "")) < date.fromisoformat(EVALUATION_DATE)
            except (TypeError, ValueError):
                return False

        expected = [entry for entry in entries
                    if entry.get("decision", "shrink") == "shrink"
                    and overdue(entry)]
        for entry in entries:
            path = self.repo / entry["file"]
            content = path.read_text() if path.exists() else ""
            self.put_at(self.repo, entry["file"],
                        content + "// measured giant fixture\n" * (1000 - len(content.splitlines())))
        for name in ("giant_file_registry.toml", "giant_file_issue_metadata.json",
                     "giant_file_closed_issue_transition_list.txt"):
            shutil.copy2(ROOT / "scripts" / name, self.repo / "scripts" / name)
        self.put_at(self.repo, "scripts/audit_maintainability_giant_baseline.toml",
                    "[giant_file_ratchet]\n"
                    + "".join(f'"{entry["file"]}" = 1000\n' for entry in entries))
        for script in ("generate_inventory_docs.py", "generate_env_reference.py",
                       "generate_cli_reference.py"):
            result = self.command_at(self.repo, [sys.executable, "scripts/" + script])
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.git_at(self.repo, "add", "-A")
        self.git_at(self.repo, "commit", "-qm", "current registry baseline")
        self.base = self.git_at(self.repo, "rev-parse", "HEAD")
        env = self.candidate()
        commands = [("inventory", [sys.executable, "scripts/generate_inventory_docs.py", "--check"], {})]
        for event in ("pull_request", "push", "workflow_dispatch"):
            commands.append((event, [sys.executable, "scripts/giant_file_progress.py"],
                             {"GFP_EVENT_NAME": event, "GFP_REF": "refs/heads/main",
                              "GFP_REF_PROTECTED": "true"}))
        commands.append(("prepush", ["sh", "-c", PREPUSH_SEQUENCE], {}))
        for name, command, overrides in commands:
            with self.subTest(entrypoint=name):
                result = self.command_at(self.repo, command, {**env, **overrides})
                self.assert_warning_pass(result, count=len(expected), fixture=False)
                warning_lines = [line for line in result.stderr.splitlines()
                                 if line.startswith(OVERDUE_PREFIX)]
                for entry in expected:
                    self.assertTrue(any(all(str(entry.get(key, "")) in line for key in
                                             ("file", "deadline", "decompose_issue"))
                                        for line in warning_lines), result.stderr)


if __name__ == "__main__":
    unittest.main()
