"""Selection contract for the `high_risk_recovery` path filter (#5232).

`dorny/paths-filter@v3` compiles every pattern into its own
`picomatch(pattern, {dot: true})` matcher and ORs them (`matchers.some(...)`),
so a leading `!` is not a subtraction but one more POSITIVE matcher for
"anything that is not this" -- which is what made this lane always true.
`test_every_pattern_stays_inside_the_reimplemented_dialect` keeps the matcher
honest: it fails if a pattern uses a picomatch feature it cannot reproduce.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_DIR = REPO_ROOT / ".github/workflows"
WORKFLOW_PATH = WORKFLOW_DIR / "ci-pr.yml"
LANE = "high_risk_recovery"
GATES_DOC = REPO_ROOT / "docs/ci/release-gates.md"
ALWAYS_ON = "### Always-on (필터 없음)"
# One file per module imported by the three SUTs this lane runs:
# `src/high_risk_recovery.rs`, `services::hang_forensics` and
# `services::health_diagnostics`.
ONE_HOP_IMPORTS = """defaults.json policies/default-pipeline.yaml src/config.rs
    src/pipeline.rs src/services/dispatches/discord_delivery/mod.rs
    src/services/dispatches/outbox_queue.rs src/services/observability/mod.rs
    src/services/git/commit_resolver.rs src/services/platform/mod.rs
    src/config_live_reload.rs src/services/dispatch_gate.rs
    src/services/health_active_session_audit.rs
    src/services/session_activity.rs""".split()
SUPPORTED_SEGMENT = re.compile(r"^[A-Za-z0-9._-]*\*?[A-Za-z0-9._-]*$")


def workflow_files() -> list[Path]:
    """GitHub honours both extensions; walking one leaves a blind spot."""
    return sorted(q for ext in ("*.yml", "*.yaml") for q in WORKFLOW_DIR.glob(ext))


def filter_blocks(workflow: Path):
    """Each `dorny/paths-filter` step's parsed `filters:` block, any job."""
    jobs = (yaml.safe_load(workflow.read_text(encoding="utf-8")) or {}).get("jobs") or {}
    for job in jobs.values():
        for step in job.get("steps") or []:
            if str(step.get("uses", "")).startswith("dorny/paths-filter"):
                assert "predicate-quantifier" not in step["with"], "quantifier moved"
                yield yaml.safe_load(step["with"]["filters"])


def load_filters(workflow: Path = WORKFLOW_PATH) -> dict[str, tuple[str, ...]]:
    """The block defining this lane -- not merely the first block declared,
    which any new paths-filter job ahead of `changes` would displace."""
    for block in filter_blocks(workflow):
        if LANE in block:
            return {n: tuple(p) for n, p in block.items()}
    raise AssertionError(f"{workflow.name}: no paths-filter block defines {LANE}")


def _atom(part: str) -> str:
    return "[^/]*" if part == "*" else re.escape(part)


def pattern_to_regex(pattern: str) -> re.Pattern[str]:
    """Trailing `/**` matches the prefix and all beneath it; `*` matches a run
    of non-`/` characters (possibly empty, dots not special); rest is literal."""
    body, suffix = pattern, ""
    if pattern.endswith("/**"):
        body, suffix = pattern[: -len("/**")], "(?:/.*)?"
    segments = [
        "".join(map(_atom, re.split(r"(\*)", seg))) for seg in body.split("/")
    ]
    return re.compile("^" + "/".join(segments) + suffix + "$")


def select(filters: dict[str, tuple[str, ...]], changed: list[str]) -> set[str]:
    """The filter names dorny would set to `true` for this changed-file list."""
    return {
        name
        for name, patterns in filters.items()
        if any(pattern_to_regex(q).match(p) for q in patterns for p in changed)
    }


class HighRiskRecoveryPathFilterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.filters = load_filters()

    def test_no_pattern_is_negated_in_any_workflow(self) -> None:
        """#5232 is a defect class, not one file -- `ci-main.yml` carried the
        same `!` line. Walk the directory so new workflows are covered too."""
        blocks = [
            (wf.name, b)
            for wf in workflow_files()
            for b in filter_blocks(wf)
        ]
        negated = [
            (wf, n, q) for wf, b in blocks for n, ps in b.items() for q in ps
            if q.startswith("!")
        ]
        self.assertEqual(negated, [], "a leading `!` re-opens the #5232 defect")
        self.assertIn("ci-pr.yml", {wf for wf, _ in blocks})

    def test_the_lane_selects_for_every_one_hop_import_of_its_suts(self) -> None:
        """`ci-pr.yml` gates this lane on a path list, so the list has to reach
        every module its SUTs import; a miss skips the lane silently."""
        for path in ONE_HOP_IMPORTS:
            with self.subTest(path=path):
                self.assertIn(LANE, select(self.filters, [path]))

    def test_ci_main_runs_the_lane_unconditionally(self) -> None:
        """#5232 R3: ci-main has no library sweep behind this lane, so a filter
        miss there is unrecoverable. It pays the lane once per merge instead of
        gating it, and keeps no unread filter to drift out of date."""
        workflow = WORKFLOW_DIR / "ci-main.yml"
        job = yaml.safe_load(workflow.read_text(encoding="utf-8"))["jobs"]
        self.assertNotIn("if", job["high-risk-recovery"])
        self.assertNotIn("needs", job["high-risk-recovery"])
        self.assertEqual(list(filter_blocks(workflow)), [])

    def test_release_gates_doc_agrees_with_ci_main_gating(self) -> None:
        """Nothing read `docs/ci/release-gates.md`, so the commit above left
        it still describing a `needs: changes` gate on main. A reader who
        trusts the doc restores that `if:` -- which is #5232 re-opening.
        Anchor both directions on the job's own `name`, not on prose."""
        job = yaml.safe_load(
            (WORKFLOW_DIR / "ci-main.yml").read_text(encoding="utf-8")
        )["jobs"]["high-risk-recovery"]
        _, heading, rest = GATES_DOC.read_text(encoding="utf-8").partition(ALWAYS_ON)
        self.assertTrue(heading, f"release-gates.md: `{ALWAYS_ON}` section is gone")
        self.assertEqual(
            not {"if", "needs"} & job.keys(),
            job["name"] in rest.split("\n###", 1)[0],
            "ci-main gating and the release-gates.md always-on list disagree",
        )

    def test_every_pattern_stays_inside_the_reimplemented_dialect(self) -> None:
        """Same scope as the `!` guard: a pattern `pattern_to_regex` cannot
        reproduce would make every `select()` assertion above meaningless."""
        for wf in workflow_files():
            for block in filter_blocks(wf):
                for name, patterns in block.items():
                    for q in patterns:
                        with self.subTest(workflow=wf.name, filter=name, pattern=q):
                            body = q[: -len("/**")] if q.endswith("/**") else q
                            self.assertNotIn("**", body, "`**` only trails")
                            for segment in body.split("/"):
                                self.assertRegex(segment, SUPPORTED_SEGMENT)

    def test_unrelated_documentation_change_does_not_select_the_lane(self) -> None:
        changed = ["docs/architecture/relay.md", "README.md", "AGENTS.md"]
        self.assertNotIn(LANE, select(self.filters, changed))

    def test_recovery_code_and_shared_dependencies_select_the_lane(self) -> None:
        for path in (
            "src/high_risk_recovery.rs",
            "src/services/hang_forensics.rs",
            "src/services/discord/relay_recovery.rs",
            "src/services/discord/placeholder_live_events/mod.rs",
            "Cargo.toml",
            "migrations/postgres/0101_canonical_discord_session_identity.sql",
            "policies/ci-recovery.js",
            "policies/default-pipeline.yaml",
        ):
            with self.subTest(path=path):
                self.assertIn(LANE, select(self.filters, [path]))

    def test_workflow_and_check_configuration_changes_select_the_lane(self) -> None:
        for path in (
            ".github/workflows/ci-pr.yml",
            "scripts/check_test_target_integrity.py",
            "scripts/ci/postgres-service.sh",
        ):
            with self.subTest(path=path):
                self.assertIn(LANE, select(self.filters, [path]))

    def test_multi_area_pull_request_selects_the_union_of_its_lanes(self) -> None:
        areas = [
            ["dashboard/src/app.tsx"],
            ["src/services/discord/relay_recovery.rs"],
            ["migrations/postgres/0102_example.sql"],
            ["docs/architecture/relay.md"],
        ]
        union = set().union(*(select(self.filters, area) for area in areas))
        self.assertEqual(select(self.filters, [p for a in areas for p in a]), union)
        self.assertLessEqual({"dashboard", LANE, "pg_db"}, union)

    def test_ci_script_checks_runs_this_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn("unittest tests.test_high_risk_recovery_path_filter", script)


if __name__ == "__main__":
    unittest.main()
