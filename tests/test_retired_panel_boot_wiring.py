"""Pin the completed-panel queue to the real bot setup, without starting Discord."""

from pathlib import Path
import re
import tempfile
import unittest

from scripts.check_tui_o_writer_census import _fn_bodies
from scripts.rust_production import production_lines


ROOT = Path(__file__).resolve().parents[1]
SETUP = "src/services/discord/runtime_bootstrap/framework_setup.rs"
CALL = re.compile(
    r"super\s*::\s*footer_retirement\s*::\s*enqueue_completed_singletons\s*"
    r"\(\s*&\s*provider_for_setup\s*,\s*&\s*shared_for_tmux\s*\.\s*token_hash\s*,?\s*\)\s*;"
)
CLONE = re.compile(r"let\s+shared_for_tmux\s*=\s*shared_for_migrate\s*\.\s*clone\s*\(\s*\)\s*;")
SPAWN = "super::spawns::run_bot_spawn_skills_hot_reload"


def assert_boot_wiring(test, path):
    text = "\n".join(code if production else "" for _, code, production in production_lines(path))
    bodies = [(start, end) for name, start, end in _fn_bodies(text)
              if name == "run_bot_framework_setup"]
    test.assertEqual(len(bodies), 1, "real production setup function required")
    start, end = bodies[0]
    body = text[start + 1:end]
    calls = list(CALL.finditer(text))
    test.assertEqual(len(calls), 1, "exactly one provider/token-scoped boot enqueue required")
    call = calls[0]
    test.assertLess(start, call.start(), "enqueue must be inside real setup")
    test.assertLess(call.end(), end, "enqueue must be inside real setup")
    clones = list(CLONE.finditer(body))
    test.assertEqual(len(clones), 1, "setup's shared handle required")
    call_at = call.start() - start - 1
    test.assertEqual(body[clones[0].end():call_at].strip(), "",
                     "enqueue must run directly after the shared clone, outside conditionals/tasks")
    test.assertLess(call_at, body.index(SPAWN), "queue old panels before background recovery")


class RetiredPanelBootWiringTests(unittest.TestCase):
    def fixture(self, text):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / "setup.rs"
        path.write_text(text)
        return path

    def setup_source(self, call):
        return ("async fn run_bot_framework_setup() {\n"
                "let shared_for_tmux = shared_for_migrate.clone();\n"
                + call + "\n" + SPAWN + "();\n}\n")

    def assert_rejected(self, text):
        with self.assertRaises(AssertionError):
            assert_boot_wiring(self, self.fixture(text))

    def test_real_setup_enqueues_completed_singletons_before_background_recovery(self):
        assert_boot_wiring(self, ROOT / SETUP)

    def test_unconditional_provider_and_token_scoped_call_is_accepted(self):
        assert_boot_wiring(self, self.fixture(self.setup_source(
            "super::footer_retirement::enqueue_completed_singletons(\n"
            "&provider_for_setup, &shared_for_tmux.token_hash,\n);")))

    def test_missing_call_is_rejected(self):
        self.assert_rejected(self.setup_source(""))

    def test_comment_or_string_call_does_not_satisfy_wiring(self):
        call = "super::footer_retirement::enqueue_completed_singletons(&provider_for_setup, &shared_for_tmux.token_hash);"
        self.assert_rejected(self.setup_source("// " + call))
        self.assert_rejected(self.setup_source('let decoy = "' + call + '";'))

    def test_test_only_call_does_not_satisfy_wiring(self):
        self.assert_rejected(self.setup_source(
            "#[cfg(test)]\n"
            "super::footer_retirement::enqueue_completed_singletons(&provider_for_setup, &shared_for_tmux.token_hash);"))

    def test_conditional_call_does_not_satisfy_wiring(self):
        self.assert_rejected(self.setup_source(
            "if false { super::footer_retirement::enqueue_completed_singletons(&provider_for_setup, &shared_for_tmux.token_hash); }"))

    def test_wrong_provider_or_token_does_not_satisfy_wiring(self):
        self.assert_rejected(self.setup_source(
            "super::footer_retirement::enqueue_completed_singletons(&other_provider, &shared_for_tmux.token_hash);"))
        self.assert_rejected(self.setup_source(
            "super::footer_retirement::enqueue_completed_singletons(&provider_for_setup, &other_token);"))

    def test_call_in_other_function_does_not_satisfy_wiring(self):
        self.assert_rejected(self.setup_source("") + "\nfn unrelated() {\n"
            "super::footer_retirement::enqueue_completed_singletons(&provider_for_setup, &shared_for_tmux.token_hash);\n}\n")


if __name__ == "__main__":
    unittest.main()
