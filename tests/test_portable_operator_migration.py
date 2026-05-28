import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class PortableOperatorMigrationTests(unittest.TestCase):
    def test_dry_run_emits_env_overrides_without_private_defaults(self):
        with tempfile.TemporaryDirectory() as temp:
            legacy_home = Path(temp) / "legacy-home"
            root = legacy_home / ".adk" / "release"
            agents_src = legacy_home / "vault" / "RemoteVault" / "adk-config" / "agents"

            output = subprocess.check_output(
                [
                    sys.executable,
                    str(ROOT / "scripts" / "portable-operator-migration-dry-run.py"),
                    "--legacy-home",
                    str(legacy_home),
                    "--root",
                    str(root),
                    "--obsidian-agents-src",
                    str(agents_src),
                ],
                text=True,
            )

        plan = json.loads(output)
        self.assertTrue(plan["dry_run"])
        self.assertEqual(plan["env_overrides"]["AGENTDESK_ROOT_DIR"], root.as_posix())
        self.assertEqual(
            plan["env_overrides"]["AGENTDESK_OBSIDIAN_AGENTS_SRC"],
            agents_src.as_posix(),
        )
        self.assertIn("migrated-launchd/memory-merge.js", plan["routine_overrides"])
        rendered = json.dumps(plan)
        self.assertNotIn("/Users/itismyfield", rendered)
        self.assertNotIn("mac-mini-release", rendered)

    def test_shell_resolver_preserves_legacy_env_overrides(self):
        bash_path = shutil.which("bash")
        if bash_path is None:
            self.skipTest("bash is unavailable")
        if os.name == "nt" and Path(bash_path).resolve().as_posix().lower().endswith(
            "/windows/system32/bash.exe"
        ):
            self.skipTest("Windows WSL bash launcher does not reliably inherit env overrides")

        with tempfile.TemporaryDirectory() as temp:
            legacy_home = Path(temp) / "legacy-home"
            root = legacy_home / ".adk" / "release"
            vault = legacy_home / "ObsidianVault"
            agents_src = vault / "RemoteVault" / "adk-config" / "agents"
            command = (
                "source scripts/launchd-migrated/_portable-resolver.sh; "
                "agentdesk_source_portable_resolver; "
                'printf "%s\\n%s\\n%s\\n" '
                '"$AGENTDESK_ROOT_DIR" "$OBSIDIAN_VAULT_ROOT" "$AGENTDESK_OBSIDIAN_AGENTS_SRC"'
            )
            env = os.environ.copy()
            env.update(
                {
                    "HOME": str(Path(temp) / "fresh-home"),
                    "AGENTDESK_ROOT_DIR": str(root),
                    "OBSIDIAN_VAULT_ROOT": str(vault),
                    "AGENTDESK_OBSIDIAN_AGENTS_SRC": str(agents_src),
                    "AGENTDESK_SOURCE_ZPROFILE": "0",
                }
            )
            output = subprocess.check_output(
                [bash_path, "-lc", command],
                cwd=ROOT,
                env=env,
                encoding="utf-8",
                text=True,
            ).splitlines()

        self.assertEqual(output, [str(root), str(vault), str(agents_src)])

    def test_shell_resolver_reapplies_launchd_env_after_zprofile(self):
        bash_path = shutil.which("bash")
        if bash_path is None:
            self.skipTest("bash is unavailable")
        if os.name == "nt" and Path(bash_path).resolve().as_posix().lower().endswith(
            "/windows/system32/bash.exe"
        ):
            self.skipTest("Windows WSL bash launcher does not reliably inherit env overrides")

        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp) / "home"
            home.mkdir()
            root = Path(temp) / "release"
            remote = root / "ObsidianVault" / "RemoteVault"
            stale = Path(temp) / "stale"
            (home / ".zprofile").write_text(
                "\n".join(
                    [
                        f"export AGENTDESK_ROOT_DIR={stale / 'release'}",
                        f"export OBSIDIAN_REMOTE_VAULT_ROOT={stale / 'RemoteVault'}",
                        f"export PATH={stale / 'bin'}",
                    ]
                ),
                encoding="utf-8",
            )
            command = (
                "source scripts/launchd-migrated/_portable-resolver.sh; "
                "agentdesk_source_portable_resolver; "
                'printf "%s\\n%s\\n%s\\n" '
                '"$AGENTDESK_ROOT_DIR" "$OBSIDIAN_REMOTE_VAULT_ROOT" "$PATH"'
            )
            env = os.environ.copy()
            env.update(
                {
                    "HOME": str(home),
                    "AGENTDESK_ROOT_DIR": str(root),
                    "OBSIDIAN_REMOTE_VAULT_ROOT": str(remote),
                    "PATH": "/launchd/bin",
                }
            )
            output = subprocess.check_output(
                [bash_path, "-lc", command],
                cwd=ROOT,
                env=env,
                encoding="utf-8",
                text=True,
            ).splitlines()

        self.assertEqual(output[0], str(root))
        self.assertEqual(output[1], str(remote))
        self.assertIn("/launchd/bin", output[2])
        self.assertNotIn(str(stale), output[2])

    def test_shell_resolver_declares_legacy_override_defaults(self):
        resolver = (ROOT / "scripts" / "launchd-migrated" / "_portable-resolver.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            'export AGENTDESK_ROOT_DIR="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}"',
            resolver,
        )
        self.assertIn(
            '$AGENTDESK_ROOT_DIR/ObsidianVault',
            resolver,
        )
        self.assertIn(
            'export AGENTDESK_OBSIDIAN_AGENTS_SRC="${AGENTDESK_OBSIDIAN_AGENTS_SRC:-$OBSIDIAN_REMOTE_VAULT_ROOT/adk-config/agents}"',
            resolver,
        )


if __name__ == "__main__":
    unittest.main()
