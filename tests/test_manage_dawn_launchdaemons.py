import argparse
import importlib.util
import os
import plistlib
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
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

    def test_sudoers_text_rejects_unsafe_user_name(self) -> None:
        with self.assertRaises(SystemExit):
            MODULE.sudoers_text(
                user_name="agentdesk\nALL ALL=(ALL) NOPASSWD: ALL",
                python_bin=Path("/usr/bin/python3"),
                script_path=SCRIPT_PATH,
            )

    def test_sudoers_text_can_include_bootstrap_notice(self) -> None:
        text = MODULE.sudoers_text(
            user_name="agentdesk",
            python_bin=Path("/usr/bin/python3"),
            script_path=Path("/usr/local/libexec/agentdesk/manage_dawn_launchdaemons.py"),
            bootstrap_required=True,
        )

        self.assertIn("First-time setup still requires", text)
        self.assertIn("managed root-owned entrypoint", text)

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

    def test_build_schedule_override_rejects_non_dict_plist_root(self) -> None:
        with tempfile.NamedTemporaryFile("wb", suffix=".plist", delete=False) as handle:
            plistlib.dump(["not", "a", "dict"], handle)
            source_plist = Path(handle.name)

        try:
            with self.assertRaises(MODULE.ScheduleOverrideError) as ctx:
                MODULE.build_schedule_override(source_plist, hour=5, minute=30)
        finally:
            source_plist.unlink(missing_ok=True)

        self.assertIn("expected plist dictionary root", str(ctx.exception))

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

    def test_run_via_sudo_prefers_managed_entrypoint_when_present(self) -> None:
        args = argparse.Namespace(
            action="install",
            job=["memory-dream"],
            hour=None,
            minute=None,
            python_bin="/opt/homebrew/bin/python3",
            sudoers_user="agentdesk-runtime",
            skills_root=None,
            as_root=False,
        )

        run_result = subprocess.CompletedProcess(["sudo"], 0, stdout="", stderr="")
        with mock.patch.object(MODULE, "trusted_root_python_bin", return_value=Path("/usr/bin/python3")):
            with mock.patch.object(
                MODULE,
                "privileged_reexec_script_path",
                return_value=Path("/usr/local/libexec/agentdesk/manage_dawn_launchdaemons.py"),
            ):
                with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[]):
                    with mock.patch.object(MODULE, "run_command", return_value=run_result) as run_command:
                        MODULE.run_via_sudo(args)

        command = run_command.call_args.args[0]
        self.assertEqual(command[:6], [
            "sudo",
            "-n",
            "/usr/bin/python3",
            "/usr/local/libexec/agentdesk/manage_dawn_launchdaemons.py",
            "--as-root",
            "install",
        ])

    def test_install_managed_entrypoint_copies_script_to_target(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            target = Path(tmpdir) / "libexec/agentdesk/manage_dawn_launchdaemons.py"

            with mock.patch.object(MODULE, "managed_entrypoint_target", return_value=target):
                with mock.patch.object(MODULE, "MANAGED_ENTRYPOINT_DIR", target.parent):
                    installed = MODULE.install_managed_entrypoint(SCRIPT_PATH)
                    self.assertEqual(installed, target)
                    self.assertTrue(target.exists())
                    self.assertEqual(
                        target.read_text(encoding="utf-8"),
                        SCRIPT_PATH.read_text(encoding="utf-8"),
                    )

    def test_ensure_locked_directory_chain_normalizes_existing_managed_dirs(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            anchor = Path(tmpdir) / "agentdesk"
            nested = anchor / "skills/memory-dream/scripts"
            nested.mkdir(parents=True, exist_ok=True)
            for path in [anchor, anchor / "skills", anchor / "skills/memory-dream", nested]:
                os.chmod(path, 0o775)

            MODULE.ensure_locked_directory_chain(nested, anchor=anchor)

            for path in [anchor, anchor / "skills", anchor / "skills/memory-dream", nested]:
                self.assertEqual(path.stat().st_mode & 0o777, 0o755)

    def test_status_as_root_is_treated_as_privileged_probe(self) -> None:
        args = argparse.Namespace(action="status", as_root=False)

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

    def test_validate_privileged_job_artifacts_ignores_source_skill_root(self) -> None:
        job = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/Users/agentdesk/.codex/skills/memory-dream"),
            manager_script=Path("/usr/local/libexec/agentdesk/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/usr/local/libexec/agentdesk/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )

        trusted_paths = {
            Path("/usr/bin/python3"),
            Path("/usr/local/libexec/agentdesk/manage_dawn_launchdaemons.py"),
            job.manager_script,
            job.daemon_plist,
        }
        with mock.patch.object(
            MODULE,
            "path_is_root_owned_and_locked",
            side_effect=lambda path: Path(path) in trusted_paths,
        ):
            MODULE.validate_privileged_job_artifacts(
                job,
                Path("/usr/bin/python3"),
                Path("/usr/local/libexec/agentdesk/manage_dawn_launchdaemons.py"),
            )

    def test_path_is_root_owned_and_locked_rejects_group_writable_parent(self) -> None:
        target = Path("/usr/local/libexec/agentdesk/manage_dawn_launchdaemons.py")
        stats = {
            target: SimpleNamespace(st_uid=0, st_mode=0o100755),
            target.parent: SimpleNamespace(st_uid=0, st_mode=0o040755),
            target.parent.parent: SimpleNamespace(st_uid=0, st_mode=0o040755),
            target.parent.parent.parent: SimpleNamespace(st_uid=0, st_mode=0o040775),
            Path("/usr"): SimpleNamespace(st_uid=0, st_mode=0o040755),
            Path("/"): SimpleNamespace(st_uid=0, st_mode=0o040755),
        }
        path_type = type(Path("/"))

        with mock.patch.object(
            path_type,
            "is_symlink",
            autospec=True,
            side_effect=lambda self: False,
        ):
            with mock.patch.object(
                path_type,
                "stat",
                autospec=True,
                side_effect=lambda self: stats[Path(str(self))],
            ):
                self.assertFalse(MODULE.path_is_root_owned_and_locked(target))

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

    def test_parse_args_rejects_unsafe_sudoers_user(self) -> None:
        with self.assertRaises(SystemExit):
            MODULE.parse_args(["sudoers", "--sudoers-user", "agentdesk\nroot ALL=(ALL) NOPASSWD: ALL"])

    def test_preflight_validates_artifacts_with_trusted_root_python(self) -> None:
        args = argparse.Namespace(
            action="preflight",
            job=None,
            hour=None,
            minute=None,
            python_bin="/opt/homebrew/bin/python3",
            sudoers_user="agentdesk",
            skills_root=None,
            as_root=False,
        )
        resolved = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/tmp/skills/memory-dream"),
            manager_script=Path("/tmp/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/tmp/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )
        seen: dict[str, Path] = {}

        def fake_validate(
            job: MODULE.ResolvedDawnJob,
            python_bin: Path,
            script_path: Path | None = None,
        ) -> None:
            self.assertEqual(job, resolved)
            seen["python_bin"] = python_bin
            seen["script_path"] = script_path

        def fake_run(command: list[str]) -> subprocess.CompletedProcess[str]:
            if command == ["/opt/homebrew/bin/python3", "--version"]:
                return subprocess.CompletedProcess(command, 0, stdout="Python 3.12.0\n", stderr="")
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

        with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[Path("/tmp/skills")]):
            with mock.patch.object(MODULE, "trusted_root_python_bin", return_value=Path("/usr/bin/python3")):
                with mock.patch.object(MODULE, "resolve_job_artifacts", return_value=resolved):
                    with mock.patch.object(MODULE, "plist_valid", return_value=True):
                        with mock.patch.object(MODULE, "validate_privileged_job_artifacts", side_effect=fake_validate):
                            with mock.patch.object(
                                MODULE,
                                "build_preflight_probe_command",
                                return_value=["sudo", "-n", "/usr/bin/python3", str(SCRIPT_PATH), "status"],
                            ):
                                with mock.patch.object(MODULE, "run_command", side_effect=fake_run):
                                    with mock.patch("builtins.print"):
                                        MODULE.render_preflight(
                                            args,
                                            [("memory-dream", MODULE.JOB_SPECS["memory-dream"])],
                                        )

        self.assertEqual(seen["python_bin"], Path("/usr/bin/python3"))
        self.assertIsNone(seen["script_path"])

    def test_preflight_prefers_managed_artifacts_when_managed_entrypoint_exists(self) -> None:
        args = argparse.Namespace(
            action="preflight",
            job=None,
            hour=None,
            minute=None,
            python_bin="/opt/homebrew/bin/python3",
            sudoers_user="agentdesk",
            skills_root=None,
            as_root=False,
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            tmpdir_path = Path(tmpdir)
            managed_script = tmpdir_path / "manage_dawn_launchdaemons.py"
            managed_script.write_text("#!/usr/bin/env python3\n", encoding="utf-8")
            managed_job = MODULE.ResolvedDawnJob(
                name="memory-dream",
                skill_root=tmpdir_path / "skills/memory-dream",
                manager_script=tmpdir_path / "skills/memory-dream/scripts/manage_memory_dream_launchd.py",
                daemon_plist=tmpdir_path / "skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist",
            )
            managed_job.manager_script.parent.mkdir(parents=True, exist_ok=True)
            managed_job.manager_script.write_text("#!/usr/bin/env python3\n", encoding="utf-8")
            managed_job.daemon_plist.parent.mkdir(parents=True, exist_ok=True)
            managed_job.daemon_plist.write_text(
                "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"></plist>\n",
                encoding="utf-8",
            )
            seen: dict[str, Path] = {}

            def fake_validate(
                job: MODULE.ResolvedDawnJob,
                python_bin: Path,
                script_path: Path | None = None,
            ) -> None:
                self.assertEqual(job, managed_job)
                seen["python_bin"] = python_bin
                seen["script_path"] = script_path

            def fake_run(command: list[str]) -> subprocess.CompletedProcess[str]:
                if command == ["/opt/homebrew/bin/python3", "--version"]:
                    return subprocess.CompletedProcess(command, 0, stdout="Python 3.12.0\n", stderr="")
                return subprocess.CompletedProcess(command, 0, stdout="", stderr="")

            with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[Path("/tmp/skills")]):
                with mock.patch.object(MODULE, "managed_entrypoint_target", return_value=managed_script):
                    with mock.patch.object(MODULE, "trusted_root_python_bin", return_value=Path("/usr/bin/python3")):
                        with mock.patch.object(MODULE, "resolve_managed_job_artifacts", return_value=managed_job):
                            with mock.patch.object(
                                MODULE,
                                "resolve_job_artifacts",
                                side_effect=AssertionError("preflight should use managed artifacts once bootstrap installed them"),
                            ):
                                with mock.patch.object(MODULE, "plist_valid", return_value=True):
                                    with mock.patch.object(
                                        MODULE,
                                        "validate_privileged_job_artifacts",
                                        side_effect=fake_validate,
                                    ):
                                        with mock.patch.object(
                                            MODULE,
                                            "build_preflight_probe_command",
                                            return_value=["sudo", "-n", "/usr/bin/python3", str(managed_script), "status"],
                                        ):
                                            with mock.patch.object(MODULE, "run_command", side_effect=fake_run):
                                                with mock.patch("builtins.print"):
                                                    MODULE.render_preflight(
                                                        args,
                                                        [("memory-dream", MODULE.JOB_SPECS["memory-dream"])],
                                                    )

            self.assertEqual(seen["python_bin"], Path("/usr/bin/python3"))
            self.assertEqual(seen["script_path"], managed_script)

    def test_root_preflight_uses_trusted_python_for_version_probe(self) -> None:
        args = argparse.Namespace(
            action="preflight",
            job=None,
            hour=None,
            minute=None,
            python_bin="/opt/homebrew/bin/python3",
            sudoers_user="agentdesk",
            skills_root=None,
            as_root=False,
        )
        resolved = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/tmp/skills/memory-dream"),
            manager_script=Path("/tmp/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/tmp/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )
        seen_commands: list[list[str]] = []

        def fake_run(command: list[str]) -> subprocess.CompletedProcess[str]:
            seen_commands.append(command)
            return subprocess.CompletedProcess(command, 0, stdout="Python 3.12.0\n", stderr="")

        with mock.patch.object(MODULE.os, "geteuid", return_value=0):
            with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[Path("/tmp/skills")]):
                with mock.patch.object(
                    MODULE, "trusted_root_python_bin", return_value=Path("/usr/bin/python3")
                ):
                    with mock.patch.object(MODULE, "resolve_managed_job_artifacts", return_value=resolved):
                        with mock.patch.object(MODULE, "plist_valid", return_value=True):
                            with mock.patch.object(
                                MODULE,
                                "validate_privileged_job_artifacts",
                                return_value=None,
                            ):
                                with mock.patch.object(
                                    MODULE,
                                    "build_preflight_probe_command",
                                    return_value=["sudo", "-n", "/usr/bin/python3", str(SCRIPT_PATH), "status"],
                                ):
                                    with mock.patch.object(MODULE, "run_command", side_effect=fake_run):
                                        with mock.patch("builtins.print"):
                                            MODULE.render_preflight(
                                                args,
                                                [("memory-dream", MODULE.JOB_SPECS["memory-dream"])],
                                            )

        self.assertEqual(seen_commands[0], ["/usr/bin/python3", "--version"])

    def test_preflight_reports_non_executable_python_without_crashing(self) -> None:
        args = argparse.Namespace(
            action="preflight",
            job=None,
            hour=None,
            minute=None,
            python_bin="/tmp/custom-python",
            sudoers_user="agentdesk",
            skills_root=None,
            as_root=False,
        )
        resolved = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/tmp/skills/memory-dream"),
            manager_script=Path("/tmp/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/tmp/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )

        def fake_run(command: list[str]) -> subprocess.CompletedProcess[str]:
            if command and command[0] == "sudo":
                return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
            raise AssertionError(f"unexpected command: {command}")

        with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[Path("/tmp/skills")]):
            with mock.patch.object(MODULE, "resolve_job_artifacts", return_value=resolved):
                with mock.patch.object(MODULE, "plist_valid", return_value=True):
                    with mock.patch.object(
                        MODULE,
                        "validate_privileged_job_artifacts",
                        return_value=None,
                    ):
                        with mock.patch.object(
                            MODULE.Path,
                            "exists",
                            return_value=True,
                        ):
                            with mock.patch.object(
                                MODULE,
                                "path_is_executable",
                                side_effect=lambda path: False,
                            ):
                                with mock.patch.object(
                                    MODULE,
                                    "build_preflight_probe_command",
                                    return_value=["sudo", "-n", "/usr/bin/python3", str(SCRIPT_PATH), "status"],
                                ):
                                    with mock.patch.object(MODULE, "run_command", side_effect=fake_run):
                                        with mock.patch("builtins.print") as printer:
                                            rc = MODULE.render_preflight(
                                                args,
                                                [("memory-dream", MODULE.JOB_SPECS["memory-dream"])],
                                            )

        self.assertEqual(rc, 1)
        rendered = "\n".join(call.args[0] for call in printer.call_args_list)
        self.assertIn("- invocation_python_executable: `False`", rendered)
        self.assertIn("- invocation_python_version: `not executable`", rendered)

    def test_run_bootstrap_validates_installed_entrypoint_not_repo_checkout(self) -> None:
        args = argparse.Namespace(
            action="bootstrap",
            job=["memory-dream"],
            hour=None,
            minute=None,
            python_bin="/opt/homebrew/bin/python3",
            sudoers_user="agentdesk-runtime",
            skills_root=None,
            as_root=True,
        )
        source_job = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/Users/agentdesk/.codex/skills/memory-dream"),
            manager_script=Path("/Users/agentdesk/.codex/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/Users/agentdesk/.codex/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )
        staged_job = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/usr/local/libexec/agentdesk/skills/memory-dream"),
            manager_script=Path("/usr/local/libexec/agentdesk/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/usr/local/libexec/agentdesk/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )

        with mock.patch.object(MODULE, "effective_manager_python", return_value=Path("/usr/bin/python3")):
            with mock.patch.object(
                MODULE,
                "install_managed_entrypoint",
                return_value=Path("/usr/local/libexec/agentdesk/manage_dawn_launchdaemons.py"),
            ):
                with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[Path("/Users/agentdesk/.codex/skills")]):
                    with mock.patch.object(MODULE, "resolve_job_artifacts", return_value=source_job):
                        with mock.patch.object(
                            MODULE,
                            "install_managed_job_artifacts",
                            return_value=staged_job,
                        ) as install_managed_job_artifacts:
                            with mock.patch.object(MODULE, "validate_privileged_entrypoint") as validate_entrypoint:
                                with mock.patch.object(
                                    MODULE,
                                    "install_sudoers_dropin",
                                    return_value=(False, "skip install"),
                                ):
                                    MODULE.run_bootstrap(
                                        args,
                                        [("memory-dream", MODULE.JOB_SPECS["memory-dream"])],
                                    )

        validate_entrypoint.assert_called_once_with(
            Path("/usr/bin/python3"),
            Path("/usr/local/libexec/agentdesk/manage_dawn_launchdaemons.py"),
        )
        install_managed_job_artifacts.assert_called_once_with(
            source_job,
            MODULE.JOB_SPECS["memory-dream"],
        )

    def test_render_batch_summary_privileged_uses_managed_job_artifacts(self) -> None:
        args = argparse.Namespace(
            action="install",
            job=["memory-dream"],
            hour=None,
            minute=None,
            python_bin="/usr/bin/python3",
            sudoers_user="agentdesk-runtime",
            skills_root=None,
            as_root=True,
        )
        managed_job = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/usr/local/libexec/agentdesk/skills/memory-dream"),
            manager_script=Path("/usr/local/libexec/agentdesk/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/usr/local/libexec/agentdesk/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )
        run_result = subprocess.CompletedProcess(["python3"], 0, stdout="ok", stderr="")

        with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[Path("/Users/agentdesk/.codex/skills")]):
            with mock.patch.object(MODULE, "effective_manager_python", return_value=Path("/usr/bin/python3")):
                with mock.patch.object(MODULE, "resolve_managed_job_artifacts", return_value=managed_job):
                    with mock.patch.object(
                        MODULE,
                        "resolve_job_artifacts",
                        side_effect=AssertionError("should not resolve user-owned skill roots in privileged mode"),
                    ):
                        with mock.patch.object(
                            MODULE,
                            "validate_privileged_job_artifacts",
                            return_value=None,
                        ):
                            with mock.patch.object(MODULE, "run_manager", return_value=run_result) as run_manager:
                                with mock.patch("builtins.print"):
                                    rc = MODULE.render_batch_summary(
                                        args,
                                        [("memory-dream", MODULE.JOB_SPECS["memory-dream"])],
                                    )

        self.assertEqual(rc, 0)
        run_manager.assert_called_once()
        self.assertEqual(run_manager.call_args.args[0], managed_job)

    def test_render_batch_summary_reports_schedule_override_error_per_job(self) -> None:
        args = argparse.Namespace(
            action="install",
            job=["memory-dream", "service-monitoring"],
            hour=5,
            minute=30,
            python_bin="/usr/bin/python3",
            sudoers_user="agentdesk-runtime",
            skills_root=None,
            as_root=False,
        )
        first_job = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/Users/agentdesk/.codex/skills/memory-dream"),
            manager_script=Path("/Users/agentdesk/.codex/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/Users/agentdesk/.codex/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )
        second_job = MODULE.ResolvedDawnJob(
            name="service-monitoring",
            skill_root=Path("/Users/agentdesk/.codex/skills/service-monitoring"),
            manager_script=Path("/Users/agentdesk/.codex/skills/service-monitoring/scripts/manage_service_monitoring_launchd.py"),
            daemon_plist=Path("/Users/agentdesk/.codex/skills/service-monitoring/launchd/com.agentdesk.service-monitoring-dawn.plist"),
        )
        run_result = subprocess.CompletedProcess(["python3"], 0, stdout="ok", stderr="")

        with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[Path("/Users/agentdesk/.codex/skills")]):
            with mock.patch.object(MODULE, "effective_manager_python", return_value=Path("/usr/bin/python3")):
                with mock.patch.object(
                    MODULE,
                    "resolve_job_artifacts",
                    side_effect=[first_job, second_job],
                ):
                    with mock.patch("os.geteuid", return_value=501):
                        with mock.patch.object(
                            MODULE,
                            "run_manager",
                            side_effect=[
                                MODULE.ScheduleOverrideError("failed to build schedule override from `/tmp/bad.plist`: malformed"),
                                run_result,
                            ],
                        ) as run_manager:
                            with mock.patch("builtins.print") as print_mock:
                                rc = MODULE.render_batch_summary(
                                    args,
                                    [
                                        ("memory-dream", MODULE.JOB_SPECS["memory-dream"]),
                                        ("service-monitoring", MODULE.JOB_SPECS["service-monitoring"]),
                                    ],
                                )

        self.assertEqual(rc, 1)
        self.assertEqual(run_manager.call_count, 2)
        rendered = print_mock.call_args.args[0]
        self.assertIn("## memory-dream", rendered)
        self.assertIn("failed to build schedule override", rendered)
        self.assertIn("## service-monitoring", rendered)
        self.assertIn("- output: `ok`", rendered)

    def test_render_batch_summary_reports_manager_launch_failure_per_job(self) -> None:
        args = argparse.Namespace(
            action="status",
            job=["memory-dream", "service-monitoring"],
            hour=None,
            minute=None,
            python_bin="/usr/bin/python3",
            sudoers_user="agentdesk-runtime",
            skills_root=None,
            as_root=False,
        )
        first_job = MODULE.ResolvedDawnJob(
            name="memory-dream",
            skill_root=Path("/Users/agentdesk/.codex/skills/memory-dream"),
            manager_script=Path("/Users/agentdesk/.codex/skills/memory-dream/scripts/manage_memory_dream_launchd.py"),
            daemon_plist=Path("/Users/agentdesk/.codex/skills/memory-dream/launchd/com.agentdesk.memory-dream-dawn.plist"),
        )
        second_job = MODULE.ResolvedDawnJob(
            name="service-monitoring",
            skill_root=Path("/Users/agentdesk/.codex/skills/service-monitoring"),
            manager_script=Path("/Users/agentdesk/.codex/skills/service-monitoring/scripts/manage_service_monitoring_launchd.py"),
            daemon_plist=Path("/Users/agentdesk/.codex/skills/service-monitoring/launchd/com.agentdesk.service-monitoring-dawn.plist"),
        )
        run_result = subprocess.CompletedProcess(["python3"], 0, stdout="ok", stderr="")

        with mock.patch.object(MODULE, "candidate_skills_roots", return_value=[Path("/Users/agentdesk/.codex/skills")]):
            with mock.patch.object(MODULE, "effective_manager_python", return_value=Path("/usr/bin/python3")):
                with mock.patch.object(
                    MODULE,
                    "resolve_job_artifacts",
                    side_effect=[first_job, second_job],
                ):
                    with mock.patch("os.geteuid", return_value=501):
                        with mock.patch.object(
                            MODULE,
                            "run_manager",
                            side_effect=[
                                FileNotFoundError(2, "No such file or directory", "/bad/python"),
                                run_result,
                            ],
                        ) as run_manager:
                            with mock.patch("builtins.print") as print_mock:
                                rc = MODULE.render_batch_summary(
                                    args,
                                    [
                                        ("memory-dream", MODULE.JOB_SPECS["memory-dream"]),
                                        ("service-monitoring", MODULE.JOB_SPECS["service-monitoring"]),
                                    ],
                                )

        self.assertEqual(rc, 1)
        self.assertEqual(run_manager.call_count, 2)
        rendered = print_mock.call_args.args[0]
        self.assertIn("## memory-dream", rendered)
        self.assertIn("No such file or directory", rendered)
        self.assertIn("## service-monitoring", rendered)
        self.assertIn("- output: `ok`", rendered)


if __name__ == "__main__":
    unittest.main()
