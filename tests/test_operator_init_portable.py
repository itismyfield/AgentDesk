import json
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "operator-init-portable.py"


class OperatorInitPortableTests(unittest.TestCase):
    def run_script(self, *args: str) -> dict:
        output = subprocess.check_output(
            [sys.executable, str(SCRIPT), *args],
            text=True,
        )
        return json.loads(output)

    def test_dry_run_reports_plan_without_writing_files(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "release"
            plan = self.run_script("--root", str(root), "--dry-run")

            self.assertTrue(plan["dry_run"])
            self.assertEqual(plan["root"], root.resolve().as_posix())
            self.assertIn((root / "config" / "agentdesk.yaml").resolve().as_posix(), plan["would_create"])
            self.assertFalse(root.exists())

    def test_init_writes_portable_starter_scaffold(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "release"
            resolved_root = root.resolve()
            plan = self.run_script("--root", str(root), "--with-obsidian-stubs")

            config = root / "config" / "agentdesk.yaml"
            launchd_env = root / "config" / "launchd.env.example"
            shared_prompt = root / "config" / "agents" / "_shared.prompt.md"
            agent_prompt = root / "config" / "agents" / "ch-td.prompt.md"

            self.assertFalse(plan["dry_run"])
            for path in (config, launchd_env, shared_prompt, agent_prompt):
                self.assertTrue(path.is_file(), path)
            for path in (
                root / "data",
                root / "logs",
                root / "runtime",
                root / "workspaces" / "ch-td",
                root / "workspaces" / "agentfactory",
                root / "ObsidianVault",
                root / "ObsidianVault" / "RemoteVault" / "adk-config" / "agents",
                root / "ObsidianVault" / "RemoteVault" / "99_Skills",
            ):
                self.assertTrue(path.is_dir(), path)

            rendered = "\n".join(path.read_text(encoding="utf-8") for path in (config, launchd_env))
            self.assertIn("routines:\n  enabled: false", rendered)
            self.assertIn("host: \"127.0.0.1\"", rendered)
            self.assertIn("AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR", rendered)
            self.assertIn(
                "# export OBSIDIAN_REMOTE_VAULT_ROOT="
                f"{shlex.quote(str(resolved_root / 'ObsidianVault' / 'RemoteVault'))}",
                rendered,
            )
            self.assertIn(
                (
                    "# export AGENTDESK_OBSIDIAN_SKILL_ROOT="
                    f"{shlex.quote(str(resolved_root / 'ObsidianVault' / 'RemoteVault' / '99_Skills'))}"
                ),
                rendered,
            )
            self.assertNotIn("/Users/itismyfield", rendered)
            self.assertNotIn("mac-mini-release", rendered)
            self.assertNotIn("YOUR_COMMAND_BOT_TOKEN", rendered)

    def test_existing_files_are_not_overwritten_without_force(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "release"
            config = root / "config" / "agentdesk.yaml"
            config.parent.mkdir(parents=True)
            config.write_text("server: {}\n", encoding="utf-8")

            plan = self.run_script("--root", str(root))

            self.assertEqual(config.read_text(encoding="utf-8"), "server: {}\n")
            self.assertIn(config.resolve().as_posix(), plan["skipped"])

    def test_force_overwrites_existing_starter_files(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "release"
            config = root / "config" / "agentdesk.yaml"
            config.parent.mkdir(parents=True)
            config.write_text("server: {}\n", encoding="utf-8")

            self.run_script("--root", str(root), "--force")

            rendered = config.read_text(encoding="utf-8")
            self.assertIn("Portable AgentDesk starter config", rendered)
            self.assertIn("providers:", rendered)


if __name__ == "__main__":
    unittest.main()
