#!/usr/bin/env python3
"""Retry failed jobs once when every observed failure is an allowlisted test."""

from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path

from pr_head_current import pr_head_is_current


MANIFEST = Path(__file__).with_name("known-flaky-tests.json")
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
PREFIX = re.compile(r"^.*?\b\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+Z ?")
RUST_TEST = re.compile(r"^test (\S+) \.\.\. FAILED$")
RUST_SUMMARY = re.compile(r"^test result: FAILED\. \d+ passed; (\d+) failed;")
PY_TEST = re.compile(r"^(?:FAIL|ERROR): (\w+) \(([\w.]+)\)$")
PY_SUMMARY = re.compile(r"^FAILED \(([^)]+)\)$")
OTHER_FAILURE = re.compile(
    r"^ERROR |error\[E|error: could not compile|Diff in .*:\d+:|"
    r"SC\d{4} \((?:error|warning|info|style)\):|"
    r"yaml\.(?:scanner|parser|composer|constructor)\.\w+Error:|"
    r"(?:^|[\s/])ld: cannot find "
)
# Only these trusted workflow steps can fail solely because a test job failed.
# Match matrix job display names against their source, never against log prose.
MIRRORS = {
    ("Fast check (ubuntu-latest)", "Mirror fast check result for branch protection"):
        ("rust_or_policy", "lint", r"Lint runner"),
    ("Fast check (ubuntu-latest)", "Relay-contract fast check mirror (always,"):
        ("relay_contract", "lint", r"Lint runner"),
    ("Fast targeted tests (ubuntu-latest)", "Mirror targeted test result for branch protection"):
        ("pg_db", "test_fast", r"PostgreSQL tests \(shard \d+\)"),
    ("Fast targeted tests (ubuntu-latest)", "Mirror PostgreSQL 15 owner proof result for branch protection"):
        ("pg_db", "pg15_owner_proof", r"PostgreSQL 15 owner proof"),
    ("Dashboard (Node 22)", "Mirror dashboard result for branch protection"):
        ("dashboard", "dashboard", r"Dashboard \(Node 22\) runner"),
    ("High-risk recovery", "Mirror high-risk recovery result across path-filter skips"):
        ("high_risk_recovery", "high-risk-recovery", r"High-risk recovery runner"),
    ("Library test sweep (ubuntu-latest)", "Mirror library sweep result across path-filter skips"):
        ("rust_tests", "library_sweep", r"Library test sweep \(shard \d+\)"),
    ("Lint", "Mirror lint result for branch protection"):
        ("rust_or_policy_or_relay_contract", "lint", r"Lint runner"),
    ("Fast check cross OS required context (ubuntu-latest)", "Mirror check_fast_cross_os result for branch protection"):
        ("cross_os_rust", "check_fast_cross_os", r"Fast check \+ non-PG tests \(windows-latest\)"),
    ("Fast check cross OS required context (ubuntu-latest)", "Mirror check_fast_cross_os_targets result for branch protection"):
        ("cross_os_rust", "check_fast_cross_os_targets", r"Windows exact targets \(windows-latest\)"),
    ("Win32 build-token required context", "Run ./scripts/required-check-mirror.sh"):
        ("win32_build_token", "win32_build_token", r"win32_build_token"),
    ("Script checks", "Mirror script checks result for branch protection"):
        ("scripts", "scripts", r"Script checks runner"),
    ("Script checks", "Mirror script checks guards shard result for branch protection"):
        ("scripts", "scripts_guards", r"Script checks runner \(guards\)"),
    ("Script checks", "Mirror script checks contracts shard result for branch protection"):
        ("scripts", "scripts_contracts", r"Script checks runner \(contracts\)"),
    ("relay-authority-contract", "Mirror relay authority targets result for branch protection"):
        ("relay_authority", "relay_authority_targets", r"Relay authority targets"),
    ("relay-authority-contract", "Mirror relay authority mutations result for branch protection"):
        ("relay_authority", "relay_authority_mutations", r"Relay authority mutations \(\d+\)"),
}


def log_lines(log: str) -> list[str]:
    return [PREFIX.sub("", ANSI.sub("", raw)).strip() for raw in log.splitlines()]


def load_known(path: Path) -> set[str]:
    rows = json.loads(path.read_text())
    if not isinstance(rows, list):
        raise ValueError("known flaky manifest must be a list")
    names: set[str] = set()
    for row in rows:
        if (set(row) != {"test", "run", "reason"}
                or not all(isinstance(value, str) and value.strip() for value in row.values())
                or not re.fullmatch(r"https://github.com/itismyfield/AgentDesk/actions/runs/\d+", row["run"])
                or row["test"] in names):
            raise ValueError("invalid or duplicate known flaky entry")
        names.add(row["test"])
    return names


def failed_tests(log: str) -> set[str] | None:
    """Require named failures and complete, matching test-run summaries."""
    names: set[str] = set()
    failures = 0
    named_failures = 0
    for line in log_lines(log):
        # The lane runner repeats an already named failure as a diagnostic.
        if line.startswith("ERROR unlisted test failure: "):
            name = line.split(": ", 1)[1].split(". Fix it", 1)[0]
            if name not in names:
                return None
            continue
        if line.startswith(("##[error]", "::error")) and not re.fullmatch(
            r"##\[error\]Process completed with exit code (?:1|101)\.", line
        ):
            return None
        if OTHER_FAILURE.search(line):
            return None
        rust = RUST_TEST.fullmatch(line)
        python = PY_TEST.fullmatch(line)
        if rust or python:
            name = rust[1] if rust else python[2]
            if python and not name.endswith(f".{python[1]}"):
                name += f".{python[1]}"
            named_failures += 1
            names.add(name)
        elif match := RUST_SUMMARY.match(line):
            failures += int(match[1])
        elif match := PY_SUMMARY.fullmatch(line):
            field = r"(?:failures|errors|skipped|expected failures|unexpected successes)=\d+"
            if not re.fullmatch(rf"{field}(?:, {field})*", match[1]):
                return None
            counts = {key: int(value) for key, value in
                      (part.split("=") for part in match[1].split(", "))}
            if counts.get("unexpected successes", 0):
                return None
            failures += counts.get("failures", 0) + counts.get("errors", 0)
        elif re.search(r"(?:^test .* \.\.\. FAILED|^FAIL:|^ERROR:|^test result: FAILED|^FAILED \()", line):
            return None
    return names if names and failures == named_failures else None


def derived_mirror(job: dict, log: str, sources: list[dict], jobs: list[dict]) -> bool:
    failed_step = next(step for step in job["steps"] if step.get("conclusion") == "failure")
    contract = MIRRORS.get((job.get("name"), failed_step.get("name")))
    if not contract:
        return False
    filter_name, upstream, source_pattern = contract
    changes = [item for item in jobs if item.get("name") == "Changed paths"]
    if len(changes) != 1 or changes[0].get("conclusion") != "success":
        return False
    if not any(re.fullmatch(source_pattern, source.get("name", "")) for source in sources):
        return False
    lines = log_lines(log)
    errors = [line.removeprefix("##[error]") for line in lines if line.startswith("##[error]")]
    expected = f"{filter_name}=true requires {upstream} to succeed; result was 'failure'"
    return errors == [expected, "Process completed with exit code 1."] and not any(
        OTHER_FAILURE.search(line) or line.startswith("::error") for line in lines)


def gh(*args: str) -> str:
    return subprocess.run(["gh", *args], check=True, text=True, capture_output=True).stdout


def api(path: str) -> dict:
    return json.loads(gh("api", path))


def classify_run(repo: str, run_id: str, attempt: int, known: set[str]) -> tuple[bool, str]:
    if attempt != 1:
        return False, "no-op:attempt-cap"
    if not known:
        return False, "no-op:empty-allowlist"
    base = f"repos/{repo}/actions/runs/{run_id}"
    run = api(f"{base}/attempts/{attempt}")
    if not all(run.get(k) == v for k, v in {
        "name": "CI PR", "event": "pull_request", "status": "completed",
        "conclusion": "failure", "run_attempt": attempt,
    }.items()):
        return False, "no-op:invalid-attempt"
    payload = api(f"{base}/attempts/{attempt}/jobs?per_page=100")
    jobs = payload.get("jobs", [])
    if not jobs or payload.get("total_count") != len(jobs):
        return False, "no-op:incomplete-jobs"
    failures = [job for job in jobs if job.get("conclusion") == "failure"]
    if not failures or any(job.get("conclusion") not in {"failure", "success", "skipped"} for job in jobs):
        return False, "no-op:incomplete-jobs"
    sources, mirrors = [], []
    mirror_jobs = {name for name, _ in MIRRORS}
    for job in failures:
        steps = job.get("steps") or []
        if sum(step.get("conclusion") == "failure" for step in steps) != 1:
            return False, "no-op:unknown-steps"
        log = gh("api", f"repos/{repo}/actions/jobs/{job['id']}/logs", "--allow-escape-sequences")
        if job.get("name") in mirror_jobs:
            mirrors.append((job, log))
            continue
        names = failed_tests(log)
        if not names or not names <= known:
            return False, "no-op:unlisted-failure"
        sources.append(job)
    if not sources or any(not derived_mirror(job, log, sources, jobs) for job, log in mirrors):
        return False, "no-op:unlisted-mirror"
    latest = api(base)
    if not all(latest.get(k) == v for k, v in {
        "status": "completed", "conclusion": "failure", "run_attempt": attempt,
    }.items()):
        return False, "no-op:stale-attempt"
    if not pr_head_is_current(repo, run):
        return False, "no-op:stale-head"
    if os.environ.get("RERUN_DRY_RUN") == "1":
        return True, "would-rerun:known-flaky"
    gh("run", "rerun", run_id, "--repo", repo, "--failed")
    return True, "rerun-requested:known-flaky"


def main() -> None:
    handled, decision = False, "no-op:invalid-input"
    try:
        repo = os.environ["GITHUB_REPOSITORY"]
        run_id = os.environ["RUN_ID"]
        attempt = int(os.environ["RUN_ATTEMPT"])
        if re.fullmatch(r"[\w.-]+/[\w.-]+", repo) and re.fullmatch(r"[1-9]\d*", run_id):
            handled, decision = classify_run(repo, run_id, attempt, load_known(MANIFEST))
    except (KeyError, ValueError, TypeError, OSError, subprocess.CalledProcessError) as error:
        decision = f"no-op:unavailable-evidence ({type(error).__name__})"
    print(f"decision={decision}")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a") as file:
            file.write(f"handled={str(handled).lower()}\n")
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as file:
            file.write(f"### Known flaky retry\n\nDecision: `{decision}`\n")


if __name__ == "__main__":
    main()
