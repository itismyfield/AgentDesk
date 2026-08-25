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
        # #5504 Stack A moved five channel-only handoff cleanup spellings behind
        # exact cleanup helpers, so 8/3/3/2 became 3/5/4/2. The baseline
        # comment stays at the historical no-growth ceiling; this assertion pins
        # the safer checked-in tree without relaxing that ceiling.
        self.assertEqual(
            self.registry_subcounts,
            {
                "direct_channel_remove": 3,
                "remove_if_current": 5,
                "cancel_and_remove_if_current": 4,
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

    def test_fenced_binders_are_exactly_the_two_production_call_sites(self) -> None:
        # #5071 relay-tail S4: the fence rides on `under_identity_fence`, and the
        # design claims exactly two production binders. Pin the set, not just a
        # total, so moving one file to another still shows up as a diff.
        self.assertEqual(
            self.actual["identity_fence_bind"],
            {
                "src/services/discord/relay_recovery/apply.rs": 1,
                "src/services/discord/tui_direct_pending_start.rs": 1,
            },
        )
        # The owner file both defines and re-spells the binder; counting it would
        # make the category track the implementation instead of its callers.
        self.assertNotIn(ratchet.REGISTRY_OWNER, self.actual["identity_fence_bind"])
        owner_production = ratchet.RUST_LEXER._production_text(ROOT / ratchet.REGISTRY_OWNER)
        self.assertRegex(owner_production, ratchet.IDENTITY_FENCE_PATTERN)
        comment = self.payload["categories"]["identity_fence_bind"]["comment"]
        self.assertIn("does not fence those", comment)

    def test_every_fenced_site_binds_both_s4_conjuncts(self) -> None:
        # #5071 relay-tail S4 r2 (P1-2 ①): the delivery conjunct rides on a
        # SECOND binder, and the growth ratchet cannot see it disappear — losing
        # `.with_terminal_delivery_fence(..)` is a decrease, which this ratchet
        # allows by design. The pairing pass is what makes the two move together.
        self.assertEqual(
            self.actual["delivery_fence_bind"],
            {
                "src/services/discord/relay_recovery/apply.rs": 1,
                "src/services/discord/tui_direct_pending_start.rs": 1,
            },
        )
        self.assertEqual(self.actual["delivery_fence_bind"], self.actual["identity_fence_bind"])
        self.assertNotIn(ratchet.REGISTRY_OWNER, self.actual["delivery_fence_bind"])
        owner_production = ratchet.RUST_LEXER._production_text(ROOT / ratchet.REGISTRY_OWNER)
        self.assertRegex(owner_production, ratchet.DELIVERY_FENCE_PATTERN)
        self.assertEqual(ratchet.pairing_errors(self.actual), [])
        comment = self.payload["categories"]["delivery_fence_bind"]["comment"]
        self.assertIn("the SAME per-file set", comment)

    def test_baseline_states_that_counts_are_not_safety_proof(self) -> None:
        self.assertEqual(self.payload["comment"], ratchet.WARNING)
        self.assertIn("not proof of safety", ratchet.WARNING)

    def test_ci_wiring_runs_scanner_and_tests(self) -> None:
        wiring = (ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("check_destructive_call_site_ratchet.py --check", wiring)
        self.assertIn("tests.test_destructive_call_site_ratchet", wiring)

    def test_warrant_docstring_declares_all_four_limits(self) -> None:
        doc = ratchet.__doc__ or ""
        self.assertIn("two-sided check, not a no-growth check", doc)
        self.assertIn("paired deletion", doc)
        self.assertIn("count-preserving relocation", doc)
        self.assertIn("Return-value discard", doc)
        self.assertIn("Argument identity", doc)
        self.assertIn("Control-flow dominance", doc)
        self.assertIn("Unused diagnostics are not enforcement", doc)
        self.assertIn("ordinary unused-binding warning", doc)
        self.assertIn("behavioral witness", doc)
        self.assertIn("does not\nprove argument identity or control-flow dominance", doc)
        self.assertNotIn("guarantees nothing", doc.lower())

    def test_generated_warrant_comments_match_the_checked_in_baseline(self) -> None:
        generated = ratchet._snapshot(
            self.actual,
            self.registry_subcounts,
            self.payload["measured_at_sha"],
        )
        for category in ("structural_candidate_apply", "destructive_warrant_bind"):
            self.assertEqual(
                generated["categories"][category]["comment"],
                self.payload["categories"][category]["comment"],
            )


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
        "identity_fence_bind": (
            "src/services/discord/t3a4_probe.rs",
            "shared.tmux_watchers.under_identity_fence(fence);\n",
        ),
        "delivery_fence_bind": (
            "src/services/discord/t3a4_probe.rs",
            "view.with_terminal_delivery_fence(delivery);\n",
        ),
        "inflight_row_clear_call": (
            "src/services/discord/t3a4_probe.rs",
            "inflight::clear_inflight_state(&provider, channel_id);\n",
        ),
        "structural_candidate_apply": (
            "src/services/discord/t3a4_probe.rs",
            "structural_candidate_apply(candidate);\n",
        ),
        "destructive_warrant_bind": (
            "src/services/discord/t3a4_probe.rs",
            "destructive_warrant_bind(candidate);\n",
        ),
    }

    def test_fake_callsite_mutations_are_unlisted(self) -> None:
        for category, (rel, body) in self.MUTATIONS.items():
            with self.subTest(category=category), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                write(root, rel, body)
                actual, _subcounts = ratchet.scan(root)
                errors = ratchet.growth_errors(actual, empty_counts())
                self.assertEqual(len(errors), 1)
                self.assertIn(f"{category}: UNLISTED call site", errors[0])

    def test_dropping_a_delivery_fence_is_red_even_though_growth_allows_it(self) -> None:
        """The whole point of the pairing pass, stated as a discrimination test.

        A site that keeps `under_identity_fence(..)` and loses
        `.with_terminal_delivery_fence(..)` is a silently unfenced destructive
        removal. Its `registry_remove` and `identity_fence_bind` counts are
        unchanged and its `delivery_fence_bind` count DECREASED, so the
        no-growth ratchet is green on it. Only `pairing_errors` is red.
        """
        rel = "src/services/discord/t3a4_pairing_probe.rs"
        both = (
            "shared.tmux_watchers\n"
            "    .under_identity_fence(fence)\n"
            "    .with_terminal_delivery_fence(delivery)\n"
            "    .remove_tmux_session_if_current(tmux, &cancel);\n"
        )
        unfenced = both.replace("    .with_terminal_delivery_fence(delivery)\n", "")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root, rel, both)
            paired, _subcounts = ratchet.scan(root)
            self.assertEqual(paired["identity_fence_bind"], {rel: 1})
            self.assertEqual(paired["delivery_fence_bind"], {rel: 1})
            self.assertEqual(ratchet.pairing_errors(paired), [])

            write(root, rel, unfenced)
            dropped, _subcounts = ratchet.scan(root)

        self.assertEqual(dropped["identity_fence_bind"], {rel: 1})
        self.assertEqual(dropped["delivery_fence_bind"], {})
        # The removal is still counted, and nothing grew: the growth ratchet
        # pinned at the PAIRED tree sees no problem at all.
        self.assertEqual(dropped["registry_remove"], paired["registry_remove"])
        self.assertEqual(ratchet.growth_errors(dropped, paired), [])
        errors = ratchet.pairing_errors(dropped)
        self.assertEqual(len(errors), 1)
        self.assertIn("fence_pairing", errors[0])
        self.assertIn(rel, errors[0])
        self.assertIn("must carry both S4 conjuncts", errors[0])

    INFLIGHT_ROW_CLEAR_ENTRY_POINTS = {
        "clear_inflight_state_for_channel": (
            "crate::services::discord::clear_inflight_state_for_channel(&provider, channel);\n"
        ),
        "archive_inflight_state_if_matches_identity_generation": (
            "inflight::archive_inflight_state_if_matches_identity_generation(\n"
            "    &provider, channel, &identity, generation,\n"
            ");\n"
        ),
        "clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence": (
            "inflight::clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence(\n"
            "    &provider, channel, &identity,\n"
            ");\n"
        ),
    }

    def test_inflight_row_clear_catches_the_three_named_entry_points(self) -> None:
        """#5462 S5 r2 (E1.4): the entry points §4.5's enumeration left out.

        Each already has a production consumer, so while they were outside the
        pattern a brand-new file could call them and the ratchet stayed green —
        the opposite of the adoption contract, which is that a new file reaching
        a destruction helper is rejected as UNLISTED.
        """
        rel = "src/services/discord/t3a4_inflight_clear_probe.rs"
        for name, body in self.INFLIGHT_ROW_CLEAR_ENTRY_POINTS.items():
            with self.subTest(entry_point=name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                write(root, rel, body)
                actual, _subcounts = ratchet.scan(root)
                self.assertEqual(actual["inflight_row_clear_call"], {rel: 1})
                errors = ratchet.growth_errors(actual, empty_counts())
                self.assertEqual(len(errors), 1)
                self.assertIn("inflight_row_clear_call: UNLISTED call site", errors[0])
                self.assertIn(rel, errors[0])

    def test_inflight_row_clear_excludes_the_owner_module(self) -> None:
        # The category counts CONSUMERS. Definitions and helper-to-helper
        # composition live under the owner prefix; counting them would pin the
        # implementation instead of the call sites it exists to bound.
        owner = ratchet.INFLIGHT_ROW_CLEAR_OWNER_PREFIX + "probe.rs"
        outside = "src/services/discord/t3a4_inflight_clear_probe.rs"
        body = "inflight::clear_inflight_state(&provider, channel);\n"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root, owner, body)
            write(root, outside, body)
            actual, _subcounts = ratchet.scan(root)
        self.assertEqual(actual["inflight_row_clear_call"], {outside: 1})

    def test_pairing_is_two_sided(self) -> None:
        actual = empty_counts()
        actual["delivery_fence_bind"] = {"src/services/discord/a.rs": 1}
        errors = ratchet.pairing_errors(actual)
        self.assertEqual(len(errors), 1)
        self.assertIn("under_identity_fence 0x", errors[0])
        actual["identity_fence_bind"] = {"src/services/discord/a.rs": 2}
        self.assertEqual(len(ratchet.pairing_errors(actual)), 1)
        actual["identity_fence_bind"]["src/services/discord/a.rs"] = 1
        self.assertEqual(ratchet.pairing_errors(actual), [])

    def test_warrant_pairing_is_two_sided_and_file_local(self) -> None:
        rel = "src/services/discord/s6a_pairing_probe.rs"
        paired_body = (
            "structural_candidate_apply(candidate);\n"
            "destructive_warrant_bind(candidate);\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root, rel, paired_body)
            paired, _subcounts = ratchet.scan(root)
            self.assertEqual(ratchet.pairing_errors(paired), [])

            write(root, rel, "structural_candidate_apply(candidate);\n")
            missing_warrant, _subcounts = ratchet.scan(root)
            self.assertEqual(ratchet.growth_errors(missing_warrant, paired), [])
            errors = ratchet.pairing_errors(missing_warrant)
            self.assertEqual(len(errors), 1)
            self.assertIn("warrant_pairing", errors[0])
            self.assertIn("destructive_warrant_bind 0x", errors[0])

            write(root, rel, "destructive_warrant_bind(candidate);\n")
            missing_structural, _subcounts = ratchet.scan(root)
            self.assertEqual(ratchet.growth_errors(missing_structural, paired), [])
            errors = ratchet.pairing_errors(missing_structural)
            self.assertEqual(len(errors), 1)
            self.assertIn("structural_candidate_apply 0x", errors[0])

    def test_stale_sweep_consumers_are_inside_the_warrant_pairing_gate(self) -> None:
        rel = "src/services/stale_turn_reconciler.rs"
        actual, _subcounts = ratchet.scan(ROOT)
        self.assertEqual(actual["structural_candidate_apply"].get(rel), 1)
        self.assertEqual(actual["destructive_warrant_bind"].get(rel), 1)
        production = ratchet.RUST_LEXER._production_text(ROOT / rel)
        dropped = production.replace(
            "                destructive_warrant_bind(\n",
            "                unpaired_warrant(\n",
            1,
        )
        self.assertEqual(
            len(ratchet.STRUCTURAL_CANDIDATE_PATTERN.findall(dropped)),
            1,
        )
        self.assertEqual(
            len(ratchet.DESTRUCTIVE_WARRANT_PATTERN.findall(dropped)),
            0,
        )
        mutated = empty_counts()
        mutated["structural_candidate_apply"] = {rel: 1}
        errors = ratchet.pairing_errors(mutated)
        self.assertEqual(len(errors), 1)
        self.assertIn("warrant_pairing", errors[0])
        self.assertIn(rel, errors[0])

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
    shared.tmux_watchers.under_identity_fence(fence).with_terminal_delivery_fence(d);
}
""",
            )
            actual, _subcounts = ratchet.scan(root)
            self.assertEqual(sum(actual["tmux_kill"].values()), 1)
            self.assertEqual(sum(actual["watcher_cancel"].values()), 1)
            self.assertEqual(actual["process_kill"], {})
            self.assertEqual(actual["registry_remove"], {})
            self.assertEqual(actual["identity_fence_bind"], {})
            self.assertEqual(actual["delivery_fence_bind"], {})
            self.assertEqual(actual["structural_candidate_apply"], {})
            self.assertEqual(actual["destructive_warrant_bind"], {})

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
