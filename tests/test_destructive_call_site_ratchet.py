"""Contract and discrimination tests for the #5071 T3-A4 ratchet."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check_destructive_call_site_ratchet.py"
SPEC = importlib.util.spec_from_file_location("destructive_call_site_ratchet", SCRIPT)
ratchet = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ratchet)


def write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


def empty_counts() -> dict[str, dict[str, int]]:
    return {category: {} for category in ratchet.CATEGORIES}


class SourceContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.actual, cls.registry_subcounts = ratchet.scan(ROOT)
        cls.baseline, cls.payload = ratchet.load_baseline(ROOT / ratchet.BASELINE_PATH)

    def test_checked_in_tree_has_no_growth(self) -> None:
        self.assertEqual(ratchet.growth_errors(self.actual, self.baseline), [])

    def test_registry_remeasurement_and_p2_1_classification_are_explicit(self) -> None:
        # #5071 T3-A2 re-pin: post_stream_exit.rs moved its one call site from
        # `tmux_watchers.remove(&channel_id)` to `remove_tmux_session_if_current`,
        # so 9/2 became 8/3 within the same 10-file set. The category total, the
        # per-file counts, and the P2-1 classification below are unchanged.
        self.assertEqual(
            self.registry_subcounts,
            {
                "direct_channel_remove": 8,
                "remove_if_current": 3,
                "cancel_and_remove_if_current": 3,
                "remove_locked_helper": 2,
            },
        )
        comment = self.payload["categories"]["registry_remove"]["comment"]
        self.assertIn("8/3/3/2 (total 16)", comment)
        self.assertIn("10/2/3/2=17", comment)
        self.assertIn("health/recovery.rs remove_locked", comment)
        watcher_backstop = ROOT / "src/services/discord/turn_finalizer/watcher_backstop.rs"
        production = ratchet.RUST_LEXER._production_text(watcher_backstop)
        self.assertNotRegex(production, ratchet.REGISTRY_PATTERNS["direct_channel_remove"])

    def test_baseline_states_that_counts_are_not_safety_proof(self) -> None:
        self.assertEqual(self.payload["comment"], ratchet.WARNING)
        self.assertIn("not proof of safety", ratchet.WARNING)

    def test_ci_wiring_runs_scanner_and_tests(self) -> None:
        wiring = (ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("check_destructive_call_site_ratchet.py --check", wiring)
        self.assertIn("tests.test_destructive_call_site_ratchet", wiring)


class RatchetDiscriminationTests(unittest.TestCase):
    MUTATIONS = {
        "tmux_kill": (
            "src/t3a4_probe.rs",
            'crate::services::platform::tmux::kill_session("probe", "probe");\n',
        ),
        "watcher_cancel": (
            "src/t3a4_probe.rs",
            "cancel.store(true, Ordering::Release);\n",
        ),
        "process_kill": (
            "src/services/discord/t3a4_probe.rs",
            "crate::services::process::kill_pid_tree(42);\n",
        ),
        "registry_remove": (
            "src/services/discord/t3a4_probe.rs",
            "shared.tmux_watchers.remove(&channel);\n",
        ),
    }

    def test_four_fake_callsite_mutations_are_unlisted(self) -> None:
        for category, (rel, body) in self.MUTATIONS.items():
            with self.subTest(category=category), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                write(root, rel, body)
                actual, _subcounts = ratchet.scan(root)
                errors = ratchet.growth_errors(actual, empty_counts())
                self.assertEqual(len(errors), 1)
                self.assertIn(f"{category}: UNLISTED call site", errors[0])

    def test_existing_file_growth_is_red_but_deletion_is_allowed(self) -> None:
        baseline = empty_counts()
        baseline["tmux_kill"] = {"src/a.rs": 2, "src/deleted.rs": 3}
        actual = empty_counts()
        actual["tmux_kill"] = {"src/a.rs": 3}
        errors = ratchet.growth_errors(actual, baseline)
        self.assertEqual(len(errors), 1)
        self.assertIn("GROWTH in src/a.rs", errors[0])
        actual["tmux_kill"]["src/a.rs"] = 1
        self.assertEqual(ratchet.growth_errors(actual, baseline), [])

    def test_comments_strings_and_multiline_spacing_are_normalized(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(
                root,
                "src/services/discord/probe.rs",
                """
// cancel.store(true, Ordering::Release);
const S: &str = "kill_pid_tree(1)";
watcher_cancel
    .store(
        true,
        Ordering::Release,
    );
""",
            )
            actual, _subcounts = ratchet.scan(root)
            self.assertEqual(actual["watcher_cancel"], {"src/services/discord/probe.rs": 1})
            self.assertEqual(actual["process_kill"], {})

    def test_cfg_test_is_included_for_map_wide_categories_only(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(
                root,
                "src/services/discord/probe.rs",
                """
#[cfg(test)]
fn probe() {
    crate::services::platform::tmux::kill_session("x", "y");
    cancel.store(true, Ordering::Release);
    kill_pid_tree(1);
    shared.tmux_watchers.remove(&channel);
}
""",
            )
            actual, _subcounts = ratchet.scan(root)
            self.assertEqual(sum(actual["tmux_kill"].values()), 1)
            self.assertEqual(sum(actual["watcher_cancel"].values()), 1)
            self.assertEqual(actual["process_kill"], {})
            self.assertEqual(actual["registry_remove"], {})

    def test_baseline_round_trip_preserves_per_file_counts_and_warning(self) -> None:
        counts = empty_counts()
        counts["registry_remove"] = {"src/services/discord/a.rs": 1}
        subcounts = {
            "direct_channel_remove": 1,
            "remove_if_current": 0,
            "cancel_and_remove_if_current": 0,
            "remove_locked_helper": 0,
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "baseline.json"
            ratchet.write_baseline(path, counts, subcounts, "a" * 40)
            loaded, payload = ratchet.load_baseline(path)
        self.assertEqual(loaded, counts)
        self.assertEqual(payload["measured_at_sha"], "a" * 40)
        self.assertEqual(payload["comment"], ratchet.WARNING)


if __name__ == "__main__":
    unittest.main()
