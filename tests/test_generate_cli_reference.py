from pathlib import Path
import re
import sys
import unittest

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT / "scripts"))

import generate_cli_reference as gen  # noqa: E402


class NamingTests(unittest.TestCase):
    def test_kebab_case_matches_clap_derivation(self) -> None:
        cases = {
            "SendToAgent": "send-to-agent",
            "Ismcptool": "ismcptool",
            "DiscordSenddm": "discord-senddm",
            "ReleaseMigratePostgres": "release-migrate-postgres",
            "OpenCode": "open-code",
            "report_channel_id": "report-channel-id",
        }
        for name, expected in cases.items():
            self.assertEqual(gen.kebab_case(name), expected, name)


class AttributeParsingTests(unittest.TestCase):
    def test_attribute_args(self) -> None:
        parsed = gen.parse_attribute_args(
            'long = "from", value_enum, default_value_t = InputModeArg::Fifo, '
            'help = crate::x::CONTRACT, short = \'c\', alias = "starter-message"'
        )
        self.assertEqual(parsed["long"], "from")
        self.assertIs(parsed["value_enum"], True)
        self.assertEqual(parsed["default_value_t"], "InputModeArg::Fifo")
        self.assertEqual(parsed["help"], "crate::x::CONTRACT")
        self.assertEqual(parsed["short"], "c")
        self.assertEqual(parsed["alias"], "starter-message")


class RepositoryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rendered, cls.commands = gen.generate()
        cls.flat = gen.flatten(cls.commands)

    def test_top_level_commands_match_clap_snapshot_test(self) -> None:
        """The clap unit test in src/cli/args.rs pins the runtime command list.

        Reproducing that snapshot proves the Python name derivation matches what
        clap actually builds, without needing a cargo build here.
        """

        source = (REPO_ROOT / "src" / "cli" / "args.rs").read_text(encoding="utf-8")
        match = re.search(
            r"fn top_level_command_name_snapshot_preserves_public_cli_surface.*?let expected = vec!\[(.*?)\];",
            source,
            re.S,
        )
        self.assertIsNotNone(match, "clap snapshot test missing from src/cli/args.rs")
        expected = [name for name in re.findall(r'"([a-z0-9-]+)"', match.group(1)) if name != "help"]
        self.assertEqual([command.path[0] for command in self.commands], expected)

    def test_required_commands_and_flags_are_rendered(self) -> None:
        paths = {" ".join(command.path) for command in self.flat}
        for required in (
            "send-to-agent",
            "query",
            "health",
            "machine-compare",
            "auto-queue activate",
            "provider-cli status",
            "migrate openclaw",
            "discord thread-create",
        ):
            self.assertIn(required, paths, required)
        by_path = {" ".join(command.path): command for command in self.flat}
        flavor = next(arg for arg in by_path["emit-launchd-plist"].args if arg.display.startswith("--flavor"))
        self.assertEqual(flavor.value, "`release`")  # no `dev` variant exists
        expect_reply = next(arg for arg in by_path["send-to-agent"].args if arg.display.startswith("--expect-reply"))
        self.assertTrue(expect_reply.required)
        self.assertEqual(expect_reply.value, "`true`, `false`")
        # `help = crate::...::SEND_TARGET_CONTRACT` resolves to the const text.
        self.assertIn("Target must be channel:<id>", self.rendered)
        self.assertNotIn("SEND_TARGET_CONTRACT", self.rendered)

    def test_rendering_is_deterministic(self) -> None:
        again, _ = gen.generate()
        self.assertEqual(self.rendered, again)


if __name__ == "__main__":
    unittest.main()
