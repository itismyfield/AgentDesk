#!/usr/bin/env python3
"""List self-test bundles safe to skip for a known pull-request change set."""

from __future__ import annotations

import argparse
import json
import os
from collections.abc import Mapping
from fnmatch import fnmatchcase


COMMON_INPUTS = (
    "scripts/ci-script-checks.sh",
    "scripts/ci/script_check_inputs.py",
    "tests/test_script_check_inputs.py",
    ".github/workflows/**",
)
SCRIPT_TEST_INPUTS = ("scripts/**", "tests/**")

# Broad script/test roots cover imported helpers and fixture-copy dependencies.
# Source assertions retain their explicit source inputs instead of hiding them.
SELF_TEST_INPUTS: dict[str, tuple[str, ...]] = {
    "Script checks input selection contract": (
        "scripts/ci/script_check_inputs.py",
        "tests/test_script_check_inputs.py",
        "tests/test_high_risk_recovery_path_filter.py",
    ),
    "Writer namespace Windows exact-target runner fixtures (#5670)": SCRIPT_TEST_INPUTS,
    "External toolchain draft/approval/smoke tests (#4555)": SCRIPT_TEST_INPUTS,
    "Comment-only change checker self-tests": (
        *SCRIPT_TEST_INPUTS,
        "src/services/discord/tui_prompt_relay/tests/scenario_census_e2e.rs",
    ),
    "PR infrastructure failure rerun classifier (#4392/#5207)": (
        *SCRIPT_TEST_INPUTS,
        ".config/nextest.toml",
    ),
    "Lib test build and clippy swap + memory probe contract": (
        *SCRIPT_TEST_INPUTS,
        ".github/actions/ensure-swap/**",
    ),
    "CI timeout wrapper tests (#4413)": (
        "scripts/ci-timeout.py",
        "tests/test_ci_timeout.py",
    ),
    "Relay-authority mutation wiring digest (#5997)": SCRIPT_TEST_INPUTS,
    "Relay recovery targeted-lane wiring contract (#4423)": (
        "tests/test_relay_recovery_ci_wiring.py",
    ),
    "TUI relay e2e harness unit tests (#5065/#5997)": (
        *SCRIPT_TEST_INPUTS,
        "src/services/discord/tui_prompt_relay/injected_prompt_policy.rs",
        "src/services/discord/commands/**",
    ),
    "Unfiltered cargo test in main, nightly and Windows warm": SCRIPT_TEST_INPUTS,
    "Scheduled-message PG path-filter wiring contract": (
        "tests/test_scheduled_messages_ci_wiring.py",
    ),
    "High-risk recovery path-filter selection contract (#5232)": (
        "tests/test_high_risk_recovery_path_filter.py",
        "docs/ci/release-gates.md",
    ),
    "PG tunnel supervisor and retired relay cleanup tests": SCRIPT_TEST_INPUTS,
    "Session anchor CLI tests": SCRIPT_TEST_INPUTS,
    "Build token fairness tests (#5968)": SCRIPT_TEST_INPUTS,
    "Shell test suites (tests/*.sh)": (
        *SCRIPT_TEST_INPUTS,
        ".github/**",
        ".config/**",
        "skills/**",
        "src/cli/build_info.rs",
        "Cargo.toml",
        "defaults.json",
        "Makefile",
        ".gitignore",
        ".githooks/**",
        "policies/**",
    ),
}


def changed_files(env: Mapping[str, str]) -> list[str] | None:
    event = env.get("GITHUB_EVENT_NAME") or env.get("GFP_EVENT_NAME")
    if event != "pull_request" or env.get("SCRIPT_CHECK_CHANGES_RESULT") != "success":
        return None
    try:
        paths = json.loads(env.get("SCRIPT_CHECK_CHANGED_FILES", ""))
    except (ValueError, TypeError):
        return None
    if not isinstance(paths, list) or not paths:
        return None
    for path in paths:
        if (not isinstance(path, str) or not path or path.startswith("/")
                or any(char in path for char in "\x00\r\n\t\\")
                or any(part in {"", ".", ".."} for part in path.split("/"))):
            return None
    return paths


def skipped_checks(env: Mapping[str, str]) -> dict[str, tuple[str, ...]]:
    paths = changed_files(env)
    if paths is None:
        return {}
    skipped = {}
    for title, inputs in SELF_TEST_INPUTS.items():
        all_inputs = COMMON_INPUTS + inputs
        if not any(fnmatchcase(path, pattern) for path in paths for pattern in all_inputs):
            skipped[title] = all_inputs
    return skipped


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list-self-tests", action="store_true")
    args = parser.parse_args()
    if args.list_self_tests:
        print("\n".join(SELF_TEST_INPUTS))
        return
    for title, inputs in skipped_checks(os.environ).items():
        print(f"{title}\t{' '.join(inputs)}")


if __name__ == "__main__":
    main()
