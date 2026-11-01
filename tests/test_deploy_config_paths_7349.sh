#!/usr/bin/env bash
set -euo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$TEST_DIR/.." && pwd)"
DEPLOY_SH="${AGENTDESK_TEST_DEPLOY_SH:-$REPO_ROOT/scripts/deploy-release.sh}"
python3 - "$DEPLOY_SH" "$REPO_ROOT" <<'PY'
import os
import re
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SOURCE = Path(sys.argv[1]).read_text()
REPO_ROOT = Path(sys.argv[2])


def rust_database_default(name):
    source = (REPO_ROOT / "src/config.rs").read_text()
    match = re.search(
        r'fn default_database_' + re.escape(name)
        + r'\(\) -> String\s*\{\s*"([^"]+)"\.into\(\)\s*\}', source,
    )
    if not match:
        raise AssertionError(f"Rust database default is not a string literal: {name}")
    return match.group(1)


def function(name):
    match = re.search(r"^" + re.escape(name) + r"\(\) \{\n.*?^\}(?=\n\n)", SOURCE, re.M | re.S)
    if not match:
        raise AssertionError(f"production function missing: {name}")
    return match.group()


class DeployConfigPaths(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="adk-config-paths-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "config").mkdir()
        self.canonical = self.root / "config/agentdesk.yaml"
        self.selected = self.root / "selected.yaml"
        self.canonical.write_text("server:\n  port: 8111\npolicies:\n  dir: canonical-old\n")
        self.selected.write_text("server:\n  port: 9412\npolicies:\n  dir: selected-old\n")

    def run_shell(self, body, *, resolver_rc=0):
        env = os.environ.copy()
        env.pop("AGENTDESK_CONFIG", None)
        env.pop("DATABASE_URL", None)
        prefix = (
            "set -euo pipefail\n"
            + f"ADK_REL={shlex.quote(str(self.root))}\n"
            + f"SELECTED_CONFIG={shlex.quote(str(self.selected))}\n"
            + "ADK_DEFAULT_PORT=8791\n"
            + f"_release_config_path() {{ printf '%s\\n' \"$SELECTED_CONFIG\"; return {resolver_rc}; }}\n"
        )
        return subprocess.run(
            ["/bin/bash", "-c", prefix + body], env=env,
            capture_output=True, text=True, timeout=15,
        )

    def port(self, *, resolver_rc=0):
        return self.run_shell(
            function("_extract_yaml_server_port_shell") + "\n"
            + function("_resolve_release_server_port")
            + "\n_resolve_release_server_port\n", resolver_rc=resolver_rc,
        )

    def test_port_reads_the_selected_file(self):
        result = self.port()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "9412")

    def test_missing_selected_file_does_not_read_another_candidate(self):
        self.selected.unlink()
        result = self.port()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "8791")

    def test_selection_failure_is_not_a_successful_default(self):
        self.assertNotEqual(self.port(resolver_rc=7).returncode, 0)

    def test_pg_probe_reads_the_selected_file(self):
        database = "database:\n  enabled: true\n  host: selected-host\n  user: test-user\n  dbname: test-db\n"
        self.selected.write_text(database)
        output = self.root / "conninfo"
        output.mkdir()
        result = self.run_shell(
            function("_pg_write_probe_conninfo")
            + f"\n_pg_write_probe_conninfo 25432 {shlex.quote(str(output))} "
            + shlex.quote(str(self.root / "pgpass")) + "\n"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((output / "PGHOST").read_text(), "selected-host")
        self.assertEqual((output / "PGUSER").read_text(), "test-user")
        self.assertEqual((output / "PGDATABASE").read_text(), "test-db")

    def test_pg_probe_defaults_missing_fields_like_rust(self):
        self.selected.write_text("database: { enabled: true }\n")
        output = self.root / "conninfo"
        output.mkdir()
        result = self.run_shell(
            function("_pg_write_probe_conninfo")
            + f"\n_pg_write_probe_conninfo 25432 {shlex.quote(str(output))} "
            + shlex.quote(str(self.root / "pgpass")) + "\n"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((output / "PGHOSTADDR").read_text(), "127.0.0.1")
        self.assertEqual((output / "PGPORT").read_text(), "25432")
        for field, name in [("PGHOST", "host"), ("PGUSER", "user"), ("PGDATABASE", "name")]:
            self.assertEqual((output / field).read_text(), rust_database_default(name))

    def test_policy_rewrite_only_edits_the_selected_file(self):
        start = SOURCE.index("\nAGENTDESK_YAML=")
        end = SOURCE.index('\nif [ -n "${ROUTINES_STAGED:-}" ]', start)
        original = self.canonical.read_bytes()
        result = self.run_shell(SOURCE[start:end])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"  dir: {self.root}/policies", self.selected.read_text())
        self.assertEqual(self.canonical.read_bytes(), original)
        self.assertTrue(Path(str(self.selected) + ".bak-policies-dir").is_file())

    def test_real_shell_bridge_selects_the_same_layout_and_explicit_paths(self):
        cases = (
            ("canonical", ("config/agentdesk.yaml", "agentdesk.yaml"), None, "config/agentdesk.yaml"),
            ("legacy", ("agentdesk.yaml",), None, "agentdesk.yaml"),
            ("explicit", ("chosen.yaml", "config/agentdesk.yaml"), "chosen.yaml", "chosen.yaml"),
            ("missing-explicit", ("config/agentdesk.yaml",), "missing.yaml", "missing.yaml"),
            ("explicit-layout-sibling", ("agentdesk.yaml",), "config/agentdesk.yaml", "agentdesk.yaml"),
            ("empty", (), None, "config/agentdesk.yaml"),
            ("partial", (), None, "config/agentdesk.yaml"),
        )
        for label, files, explicit, expected in cases:
            with self.subTest(case=label):
                root = self.root / label
                root.mkdir()
                if label == "partial":
                    (root / "config").mkdir()
                for name in files:
                    path = root / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text("fixture")
                body = (
                    f"SCRIPT_DIR={shlex.quote(str(REPO_ROOT / 'scripts'))}\n"
                    + f"ADK_REL={shlex.quote(str(root))}\n"
                    + function("_release_config_path") + "\n"
                )
                if explicit:
                    body += f"export AGENTDESK_CONFIG={shlex.quote(str(root / explicit))}\n"
                result = self.run_shell(body + "_release_config_path\n")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), str(root / expected))

    def test_real_bridge_parallel_invocations_use_independent_compiler_outputs(self):
        runner = Path(os.environ.get(
            "ADK_TEST_CONFIG_PATH_RUNNER", REPO_ROOT / "scripts/resolve-config-path.sh"
        ))
        compiler_dir = self.root / "compiler"
        compiler_dir.mkdir()
        output_log = self.root / "compiler-outputs"
        compiler = compiler_dir / "rustc"
        compiler.write_text(
            "#!/usr/bin/env python3\n"
            "import os, pathlib, sys\n"
            "output = pathlib.Path(sys.argv[sys.argv.index('-o') + 1])\n"
            "with open(os.environ['ADK_TEST_COMPILER_OUTPUTS'], 'a') as log:\n"
            "    log.write(str(output) + '\\n')\n"
            "output.write_text(\"#!/bin/sh\\nprintf '%s\\\\n' \\\"$ADK_TEST_SELECTED_CONFIG\\\"\\n\")\n"
            "output.chmod(0o700)\n"
        )
        compiler.chmod(0o700)
        env = os.environ.copy()
        env.pop("AGENTDESK_CONFIG", None)
        env["PATH"] = str(compiler_dir) + os.pathsep + env["PATH"]
        env["ADK_TEST_COMPILER_OUTPUTS"] = str(output_log)
        env["ADK_TEST_SELECTED_CONFIG"] = str(self.canonical)
        children = [subprocess.Popen(
            ["/bin/bash", "-u", str(runner), str(self.root)], env=env,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        ) for _ in range(4)]
        outcomes = [(child, child.communicate(timeout=30)) for child in children]
        for child, (stdout, stderr) in outcomes:
            self.assertEqual(child.returncode, 0, stderr)
            self.assertEqual(stdout.strip(), str(self.canonical))
            self.assertEqual(stderr, "")
        outputs = [Path(line).parent for line in output_log.read_text().splitlines()]
        self.assertEqual(len(outputs), 4)
        self.assertEqual(len(set(outputs)), 4, "parallel compilers share an output directory")

    def test_smoke_consumers_use_the_shared_selection(self):
        for name in (
            "_post_deploy_smoke_check_relay_round_trip",
            "_post_deploy_smoke_run_turn_scenario",
            "_post_deploy_smoke_check_turn_scenarios",
        ):
            with self.subTest(name=name):
                self.assertTrue("_release_config_path" in function(name), name + " bypasses shared selection")
        self.assertTrue('$ADK_REL/config/agentdesk.yaml' not in SOURCE, "deploy still constructs canonical path")


unittest.main(argv=[sys.argv[0]], verbosity=2)
PY
