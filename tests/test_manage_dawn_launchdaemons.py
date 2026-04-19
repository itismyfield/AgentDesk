import argparse
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT_PATH = Path(__file__).resolve().parents[1] / "scripts/manage_dawn_launchdaemons.py"
SPEC = importlib.util.spec_from_file_location("manage_dawn_launchdaemons", SCRIPT_PATH)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class ManageDawnLaunchdaemonsTests(unittest.TestCase):
    def test_sudoers_text_contains_expected_allowlist(self) -> None:
        text = MODULE.sudoers_text(
            user_name="agentdesk",
            python_bin=Path("/opt/homebrew/bin/python3"),
            script_path=SCRIPT_PATH,
        )

        self.assertIn("User_Alias AGENTDESK_RUNTIME = agentdesk", text)
        self.assertIn("/opt/homebrew/bin/python3 " + str(SCRIPT_PATH) + " *", text)
        self.assertIn("NOPASSWD: AGENTDESK_DAWN_MANAGER", text)

    def test_build_self_command_keeps_jobs_and_schedule(self) -> None:
        args = argparse.Namespace(
            action="install",
            job=["memory-dream", "service-monitoring"],
            hour=5,
            minute=30,
            python_bin="/opt/homebrew/bin/python3",
            sudoers_user="agentdesk-runtime",
            skills_root=["/tmp/skills-a", "/tmp/skills-b"],
        )

        command = MODULE.build_self_command(args, as_root=True)

        self.assertEqual(command[:3], ["/opt/homebrew/bin/python3", str(SCRIPT_PATH), "--as-root"])
        self.assertIn("--job", command)
        self.assertIn("memory-dream", command)
        self.assertIn("service-monitoring", command)
        self.assertIn("--sudoers-user", command)
        self.assertIn("agentdesk-runtime", command)
        self.assertEqual(
            command[-12:],
            [
                "--hour",
                "5",
                "--minute",
                "30",
                "--python-bin",
                "/opt/homebrew/bin/python3",
                "--sudoers-user",
                "agentdesk-runtime",
                "--skills-root",
                "/tmp/skills-a",
                "--skills-root",
                "/tmp/skills-b",
            ],
        )

    def test_access_denied_matches_sudo_password_message(self) -> None:
        self.assertTrue(MODULE.access_denied("sudo: a password is required"))
        self.assertFalse(MODULE.access_denied("launchd status requires attention"))

    def test_status_is_not_forced_through_sudo(self) -> None:
        self.assertFalse(MODULE.action_needs_privileged_reexec("status"))
        self.assertTrue(MODULE.action_needs_privileged_reexec("bootstrap"))
        self.assertTrue(MODULE.action_needs_privileged_reexec("install"))
        self.assertTrue(MODULE.action_needs_privileged_reexec("uninstall"))

    def test_preflight_probe_command_reenters_status_as_root(self) -> None:
        args = argparse.Namespace(
            action="preflight",
            job=["memory-dream"],
            hour=None,
            minute=None,
            python_bin="/opt/homebrew/bin/python3",
            sudoers_user="agentdesk-runtime",
            skills_root=["/tmp/skills-a", "/tmp/skills-b"],
            as_root=False,
        )

        with mock.patch.object(MODULE, "trusted_root_python_bin", return_value=Path("/usr/bin/python3")):
            command = MODULE.build_preflight_probe_command(args, "memory-dream")

        self.assertEqual(
            command[:8],
            [
                "sudo",
                "-n",
                "/usr/bin/python3",
                str(SCRIPT_PATH),
                "--as-root",
                "status",
                "--python-bin",
                "/usr/bin/python3",
            ],
        )
        self.assertIn("--sudoers-user", command)
        self.assertIn("agentdesk-runtime", command)
        self.assertEqual(
            command[-6:],
            [
                "--job",
                "memory-dream",
                "--skills-root",
                "/tmp/skills-a",
                "--skills-root",
                "/tmp/skills-b",
            ],
        )

    def test_status_as_root_is_treated_as_privileged_probe(self) -> None:
        args = argparse.Namespace(action="status", as_root=True)

        with mock.patch.object(MODULE.os, "geteuid", return_value=0):
            self.assertTrue(MODULE.privileged_root_requested(args))

    def test_trusted_root_python_bin_prefers_root_owned_fallback(self) -> None:
        with mock.patch.object(
            MODULE, "preferred_python_bin", return_value=Path("/opt/homebrew/bin/python3")
        ):
            with mock.patch.object(
                MODULE.shutil,
                "which",
                side_effect=lambda name: "/opt/homebrew/bin/python3" if name == "python3" else None,
            ):
                with mock.patch.object(
                    MODULE,
                    "path_is_root_owned_and_locked",
                    side_effect=lambda path: Path(path) == Path("/usr/bin/python3"),
                ):
                    trusted = MODULE.trusted_root_python_bin()

        self.assertEqual(trusted, Path("/usr/bin/python3"))

    def test_resolve_job_artifacts_prefers_existing_skills_root(self) -> None:
        spec = MODULE.JOB_SPECS["memory-dream"]
        with tempfile.TemporaryDirectory() as tmpdir:
            skills_root = Path(tmpdir)
            skill_root = skills_root / spec.skill_name
            (skill_root / spec.manager_relpath).parent.mkdir(parents=True, exist_ok=True)
            (skill_root / spec.daemon_plist_relpath).parent.mkdir(parents=True, exist_ok=True)
            (skill_root / spec.manager_relpath).write_text("#!/usr/bin/env python3\n", encoding="utf-8")
            (skill_root / spec.daemon_plist_relpath).write_text(
                "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"></plist>\n",
                encoding="utf-8",
            )

            resolved = MODULE.resolve_job_artifacts("memory-dream", spec, skills_roots=[skills_root])

            self.assertEqual(resolved.skill_root, skill_root)
            self.assertEqual(resolved.manager_script, skill_root / spec.manager_relpath)
            self.assertEqual(resolved.daemon_plist, skill_root / spec.daemon_plist_relpath)

    def test_default_skills_roots_prefers_sudo_user_home(self) -> None:
        with mock.patch.dict(MODULE.os.environ, {"SUDO_USER": "agentdesk"}, clear=False):
            with mock.patch.object(MODULE, "home_for_user", return_value=Path("/Users/agentdesk")):
                roots = MODULE.default_skills_roots()

        self.assertIn(Path("/Users/agentdesk/.codex/skills"), roots)
        self.assertIn(Path("/Users/agentdesk/.adk/release/skills"), roots)


if __name__ == "__main__":
    unittest.main()
