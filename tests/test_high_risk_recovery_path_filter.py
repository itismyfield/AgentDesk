"""Selection contract for the `high_risk_recovery` path filter (#5185).

`dorny/paths-filter@v3` is not GitHub's native `paths:`. Its `Filter.load`
parses the `filters` YAML, compiles EVERY pattern into its own
`picomatch(pattern, {dot: true})` matcher, and a file matches a rule when
`patterns.some(...)` holds. A leading `!` is therefore not a subtraction: it
is one more POSITIVE matcher for "anything that is not this", which used to
make this lane fire on nearly every changed file.

These tests reimplement that semantics for the pattern dialect this workflow
actually uses, and `test_every_pattern_stays_inside_the_reimplemented_dialect`
is what keeps the reimplementation honest: it fails the moment a pattern uses
a picomatch feature (`!`, braces, extglobs, character classes, a non-trailing
`**`) that the matcher below does not reproduce.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_PATH = REPO_ROOT / ".github/workflows/ci-pr.yml"
LANE = "high_risk_recovery"

# Literal path characters plus `*`. Anything else is outside the dialect the
# matcher below reproduces, so the guard test rejects it.
SUPPORTED_SEGMENT = re.compile(r"^[A-Za-z0-9._-]*\*?[A-Za-z0-9._-]*$")


def load_filters() -> dict[str, tuple[str, ...]]:
    """Load the filters exactly as the action does: the step's YAML string."""
    workflow = yaml.safe_load(WORKFLOW_PATH.read_text(encoding="utf-8"))
    steps = workflow["jobs"]["changes"]["steps"]
    step = next(s for s in steps if s.get("uses", "").startswith("dorny/paths-filter"))
    # `predicate-quantifier` defaults to `some`; assert nobody set `every`.
    assert "predicate-quantifier" not in step["with"], "quantifier is no longer `some`"
    return {
        name: tuple(patterns)
        for name, patterns in yaml.safe_load(step["with"]["filters"]).items()
    }


def pattern_to_regex(pattern: str) -> re.Pattern[str]:
    """Reproduce `picomatch(pattern, {dot: true})` for the supported dialect.

    * a trailing `/**` matches the prefix itself and anything beneath it;
    * `*` matches any run of non-`/` characters, including a leading dot and
      including the empty string (`rust-toolchain*` matches `rust-toolchain`);
    * every other character is literal, and matching is anchored full-path.
    """
    body, suffix = pattern, ""
    if pattern.endswith("/**"):
        body, suffix = pattern[: -len("/**")], "(?:/.*)?"
    segments = [
        "".join("[^/]*" if part == "*" else re.escape(part) for part in re.split(r"(\*)", seg))
        for seg in body.split("/")
    ]
    return re.compile("^" + "/".join(segments) + suffix + "$")


def select(filters: dict[str, tuple[str, ...]], changed: list[str]) -> set[str]:
    """Return the filter names dorny would set to `true` for `changed`."""
    return {
        name
        for name, patterns in filters.items()
        if any(
            pattern_to_regex(pattern).match(path) for pattern in patterns for path in changed
        )
    }


class HighRiskRecoveryPathFilterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.filters = load_filters()

    def test_no_pattern_is_negated(self) -> None:
        """The #5185 regression fence: a leading `!` is a positive matcher."""
        negated = [
            (name, pattern)
            for name, patterns in self.filters.items()
            for pattern in patterns
            if pattern.startswith("!")
        ]
        self.assertEqual(negated, [], "`!` patterns match nearly every changed file")

    def test_every_pattern_stays_inside_the_reimplemented_dialect(self) -> None:
        for name, patterns in self.filters.items():
            for pattern in patterns:
                with self.subTest(filter=name, pattern=pattern):
                    body = pattern[: -len("/**")] if pattern.endswith("/**") else pattern
                    self.assertNotIn("**", body, "`**` is only supported as a trailing segment")
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
        selected = select(
            self.filters,
            [
                "dashboard/src/app.tsx",
                "src/services/discord/relay_recovery.rs",
                "migrations/postgres/0102_example.sql",
                "docs/architecture/relay.md",
            ],
        )
        self.assertEqual(
            selected,
            select(self.filters, ["dashboard/src/app.tsx"])
            | select(self.filters, ["src/services/discord/relay_recovery.rs"])
            | select(self.filters, ["migrations/postgres/0102_example.sql"])
            | select(self.filters, ["docs/architecture/relay.md"]),
        )
        self.assertLessEqual({"dashboard", LANE, "pg_db"}, selected)

    def test_ci_script_checks_runs_this_contract(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_high_risk_recovery_path_filter', script
        )


if __name__ == "__main__":
    unittest.main()
