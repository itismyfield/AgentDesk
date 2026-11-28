import plistlib
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from scripts.refresh_release_launchd_plist import exec_role_context, refresh


class RefreshLaunchdPlistTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.home = Path(self.scratch.name)
        self.root = self.home / "runtime"
        self.binary = self.root / "bin/agentdesk"
        self.plist = self.home / "Library/LaunchAgents/com.agentdesk.release.plist"
        self.plist.parent.mkdir(parents=True)

    def emit(self, command, *, check):
        self.assertTrue(check)
        candidate = Path(command[command.index("--output") + 1])
        self.assertNotEqual(candidate, self.plist)
        candidate.write_bytes(plistlib.dumps({
            "Label": "com.agentdesk.release", "ProgramArguments": [str(self.binary), "dcserver"],
            "StandardOutPath": str(self.root / "logs/stdout"),
            "StandardErrorPath": str(self.root / "logs/stderr"),
            "SoftResourceLimits": {"NumberOfFiles": 16384},
        }))

    def test_refresh_preserves_both_log_paths_and_uses_new_arguments(self):
        logs = {"StandardOutPath": str(self.home / "Library/Logs/operator.out"),
                "StandardErrorPath": str(self.home / "Library/Logs/operator.err")}
        self.plist.write_bytes(plistlib.dumps({**logs, "ProgramArguments": ["obsolete"]}))
        with patch("scripts.refresh_release_launchd_plist.subprocess.run", side_effect=self.emit):
            refresh(self.binary, self.home, self.root)
        result = plistlib.loads(self.plist.read_bytes())
        self.assertEqual({key: result[key] for key in logs}, logs)
        self.assertEqual(result["ProgramArguments"], [str(self.binary), "dcserver"])
        self.assertEqual(result["SoftResourceLimits"]["NumberOfFiles"], 16384)

    def test_first_install_keeps_generated_defaults(self):
        with patch("scripts.refresh_release_launchd_plist.subprocess.run", side_effect=self.emit):
            refresh(self.binary, self.home, self.root)
        self.assertEqual(plistlib.loads(self.plist.read_bytes())["StandardOutPath"],
                         str(self.root / "logs/stdout"))

    def test_failed_generator_does_not_replace_existing_service(self):
        original = plistlib.dumps({"Label": "com.agentdesk.release"})
        self.plist.write_bytes(original)
        with patch("scripts.refresh_release_launchd_plist.subprocess.run",
                   side_effect=subprocess.CalledProcessError(1, "emit")):
            with self.assertRaises(subprocess.CalledProcessError):
                refresh(self.binary, self.home, self.root)
        self.assertEqual(self.plist.read_bytes(), original)

    def test_malformed_existing_service_is_not_overwritten(self):
        self.plist.write_bytes(b"malformed")
        with patch("scripts.refresh_release_launchd_plist.subprocess.run") as emit:
            with self.assertRaises(plistlib.InvalidFileException):
                refresh(self.binary, self.home, self.root)
            emit.assert_not_called()
        self.assertEqual(self.plist.read_bytes(), b"malformed")

    def test_role_context_validates_plist_and_executes_with_its_environment(self):
        label = "com.agentdesk.release"
        command = ["/bin/bash", "--noprofile", "--norc", "-p", "-s"]
        env = {"HOME": str(self.home), "PATH": "/usr/bin:/bin", "AGENTDESK_CONFIG": "relative.yml"}
        service = dict(Label=label, WorkingDirectory=str(self.home), EnvironmentVariables=env,
                       ProgramArguments=[str(self.binary), "dcserver"])
        with patch("scripts.refresh_release_launchd_plist.os.chdir") as chdir, \
                patch("scripts.refresh_release_launchd_plist.os.execve") as launch:
            for fmt in (plistlib.FMT_XML, plistlib.FMT_BINARY):
                self.plist.write_bytes(plistlib.dumps(service, fmt=fmt))
                exec_role_context(self.plist, label, command)
                chdir.assert_called_with(self.home)
                launch.assert_called_with(command[0], command, env)
            for invalid in ({key: value for key, value in service.items() if key != "WorkingDirectory"},
                            {**service, "Label": "service.other"},
                            {**service, "Program": "/bin/bash"}):
                launch.reset_mock()
                self.plist.write_bytes(plistlib.dumps(invalid))
                with self.assertRaises(SystemExit):
                    exec_role_context(self.plist, label, command)
                launch.assert_not_called()
