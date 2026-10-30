#!/usr/bin/env python3
"""Plan, preflight and audit time-weighted cargo-nextest workers for CI.

  plan            nextest list JSON + archive -> plan.json and one config per worker (LPT by duration)
  verify-archive  worker preflight: archive digest, checkout SHA, run and config must match the plan
  audit           per-worker artifacts -> verdict JSON and Markdown summary; any gap is red
  base-diff       compare a plan with the plan of a fixed comparison base SHA; every lost test needs a
                  matching scripts/ci/test-removal-justifications.txt row; landed rows only warn
  required-gate, comparison-base, classify-worker   see nextest_required.py

Every input format carries a version and is rejected when it does not match; a missing or
unreadable artifact is a failure, never an empty pass.
"""

import argparse
import datetime
import hashlib
import heapq
import json
import math
import os
from pathlib import Path
import re
import sys
import tomllib
import xml.etree.ElementTree as ET

sys.path.insert(0, str(Path(__file__).resolve().parent))
import nextest_required  # noqa: E402


PLAN_KIND = "agentdesk-nextest-plan"
PLAN_VERSION = 2
WORKER_VERSION = 1
VERDICT_VERSION = 1
POLICY_VERSION = 1
MAX_WORKERS = 32
# nextest starts one process per test; this per-test cost keeps zero-duration tests spread out.
PER_TEST_OVERHEAD = 0.05
UNIFORM_WEIGHT = 1.0
FALLBACK_PERCENTILE = 0.9
# IDs outside this alphabet would need filterset escaping that has not been verified.
ID_RE = re.compile(r"^[A-Za-z0-9_:.\-]+$")
ISSUE_RE = re.compile(r"^#\d+$")
BUILD_OPTION_RE = re.compile(r"^[A-Z_][A-Z0-9_]*=[^\s]*$")
DEFAULT_FILTER_LINE = 'default-filter = "all()"'
POLICY_KEYS = {"scope", "binary-id", "test", "issue", "expires", "reason"}
# CLI/env settings outrank the config file and would bypass the generated policy.
FORBIDDEN_ENV = ("NEXTEST_RETRIES", "NEXTEST_TEST_THREADS", "NEXTEST_FLAKY_RESULT")
RETRY_TAGS = ("flakyFailure", "flakyError", "rerunFailure", "rerunError")
# policy: only the retry policy entries get one retry. all-once: every test gets one retry and a retry
# pass is green but listed as FLAKY.
RETRY_MODES = ("policy", "all-once")


class PlanError(ValueError):
    pass


def _sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def _digest(value):
    return hashlib.sha256(_canonical(value).encode()).hexdigest()


def _id_text(test_id):
    return f"{test_id[0]} {test_id[1]}"


def _check_id(binary_id, test_name):
    if not ID_RE.match(binary_id) or not ID_RE.match(test_name):
        raise PlanError(f"unsupported characters in test ID {binary_id!r} {test_name!r}")
    return (binary_id, test_name)


def load_universe(path):
    """Return (runnable IDs, ignored IDs) from an unfiltered `cargo nextest list --message-format json`."""
    try:
        doc = json.loads(Path(path).read_text())
    except (OSError, ValueError) as exc:
        raise PlanError(f"cannot read nextest list {path}: {exc}") from exc
    suites = doc.get("rust-suites") if isinstance(doc, dict) else None
    if not isinstance(suites, dict):
        raise PlanError("nextest list has no rust-suites object (old or non-nextest format)")
    runnable, ignored = set(), set()
    for key, suite in suites.items():
        if not isinstance(suite, dict) or not isinstance(suite.get("testcases"), dict):
            raise PlanError(f"suite {key!r} has no testcases object")
        if suite.get("status", "listed") != "listed":
            raise PlanError(f"suite {key!r} was not listed (status={suite.get('status')!r})")
        binary_id = suite.get("binary-id")
        if not isinstance(binary_id, str):
            raise PlanError(f"suite {key!r} has no binary-id")
        for name, case in suite["testcases"].items():
            if not isinstance(case, dict) or not isinstance(case.get("ignored"), bool):
                raise PlanError(f"testcase {name!r} has no ignored flag")
            match = case.get("filter-match") or {"status": "matches"}
            if match.get("status") == "mismatch" and match.get("reason") != "ignored":
                raise PlanError(f"discovery list was filtered ({name!r}: {match.get('reason')!r}); "
                                "list with --ignore-default-filter and no filter")
            test_id = _check_id(binary_id, name)
            (ignored if case["ignored"] else runnable).add(test_id)
    if not runnable:
        raise PlanError("nextest list contains no runnable tests")
    return runnable, ignored


def _junit_root(path):
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError) as exc:
        raise PlanError(f"cannot parse JUnit {path}: {exc}") from exc
    if root.tag != "testsuites":
        raise PlanError(f"JUnit {path} root is <{root.tag}>, expected <testsuites>")
    return root


def _cases(root):
    for suite in root.findall("testsuite"):
        for case in suite.findall("testcase"):
            yield suite.get("name"), case


def _seconds(element):
    try:
        value = float(element.get("time"))
    except (TypeError, ValueError):
        return None
    return value if math.isfinite(value) and value >= 0 else None


def case_seconds(case):
    """(wall seconds of the reported attempt, seconds summed over every attempt); None when unreadable."""
    wall = _seconds(case)
    retries = [_seconds(child) for tag in RETRY_TAGS for child in case.findall(tag)]
    if wall is None or None in retries:
        return None, None
    return wall, wall + sum(retries)


def load_timings(paths):
    """Per-ID seconds over all attempts from earlier nextest JUnit reports; the slowest observation wins."""
    timings, invalid = {}, 0
    for path in paths:
        for binary_id, case in _cases(_junit_root(path)):
            _, total = case_seconds(case)
            if total is None:
                invalid += 1
                continue
            key = (binary_id, case.get("name"))
            timings[key] = max(total, timings.get(key, 0.0))
    return timings, invalid


def weights_for(universe, timings):
    known = sorted(timings[t] for t in universe if t in timings)
    if not known:
        return {t: UNIFORM_WEIGHT for t in universe}, {"mode": "uniform", "known": 0,
                                                        "fallback": len(universe), "fallback_seconds": UNIFORM_WEIGHT}
    fallback = known[min(len(known) - 1, int(FALLBACK_PERCENTILE * len(known)))]
    weights = {t: timings.get(t, fallback) + PER_TEST_OVERHEAD for t in universe}
    return weights, {"mode": "timed", "known": len(known), "fallback": len(universe) - len(known),
                     "fallback_seconds": fallback}


def lpt(weights, workers):
    """Longest-processing-time assignment; ties fall to sorted IDs and the lowest worker index."""
    if not 1 <= workers <= MAX_WORKERS:
        raise PlanError(f"workers must be between 1 and {MAX_WORKERS}, got {workers}")
    if workers > len(weights):
        raise PlanError(f"{workers} workers but only {len(weights)} tests; every worker needs a test")
    heap = [(0.0, 0, i) for i in range(workers)]
    buckets = [[] for _ in range(workers)]
    loads = [0.0] * workers
    for test_id in sorted(weights, key=lambda t: (-weights[t], t)):
        load, count, index = heapq.heappop(heap)
        buckets[index].append(test_id)
        loads[index] = load + weights[test_id]
        heapq.heappush(heap, (loads[index], count + 1, index))
    return [sorted(b) for b in buckets], loads


def load_policy(path, scope, today, universe, ignored):
    try:
        doc = tomllib.loads(Path(path).read_text())
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise PlanError(f"cannot read retry policy {path}: {exc}") from exc
    if doc.get("version") != POLICY_VERSION:
        raise PlanError(f"retry policy version must be {POLICY_VERSION}")
    entries, seen = [], set()
    for n, entry in enumerate(doc.get("flaky", []), 1):
        if set(entry) != POLICY_KEYS:
            raise PlanError(f"retry policy entry {n} keys must be exactly {sorted(POLICY_KEYS)}")
        test_id = _check_id(entry["binary-id"], entry["test"])
        if test_id in seen:
            raise PlanError(f"retry policy lists {_id_text(test_id)} twice")
        seen.add(test_id)
        if not ISSUE_RE.match(str(entry["issue"])) or not str(entry["reason"]).strip():
            raise PlanError(f"retry policy entry {n} needs an issue like #123 and a reason")
        expires = entry["expires"]
        if type(expires) is not datetime.date:
            raise PlanError(f"retry policy entry {n} expires must be a TOML date")
        if entry["scope"] != scope:
            continue
        if expires < today:
            raise PlanError(f"retry policy entry {_id_text(test_id)} expired on {expires}")
        if test_id not in universe:
            state = "ignored" if test_id in ignored else "absent"
            raise PlanError(f"retry policy entry {_id_text(test_id)} is {state} in the {scope} list")
        entries.append({"binary_id": test_id[0], "test": test_id[1], "issue": entry["issue"],
                        "expires": expires.isoformat()})
    return sorted(entries, key=lambda e: (e["binary_id"], e["test"]))


def _balanced_or(terms):
    """Bound nextest's recursive filter tree depth even on small main-thread stacks."""
    terms = list(terms)
    while len(terms) > 1:
        terms = [f"({terms[i]} | {terms[i + 1]})" if i + 1 < len(terms) else terms[i]
                 for i in range(0, len(terms), 2)]
    return terms[0] if terms else ""


def _exact(binary_id, tests):
    names = _balanced_or(f"test(={t})" for t in tests)
    return f"binary_id(={binary_id}) & ({names})"


def default_filter(tests):
    by_binary = {}
    for binary_id, name in tests:
        by_binary.setdefault(binary_id, []).append(name)
    return _balanced_or(f"({_exact(b, sorted(n))})" for b, n in sorted(by_binary.items()))


def _check_base_config(cfg):
    ci = cfg.get("profile", {}).get("ci", {})
    expected = {"default-filter": "all()", "retries": 0, "flaky-result": "fail",
                "fail-fast": False, "test-threads": 1}
    for key, value in expected.items():
        if type(ci.get(key)) is not type(value) or ci.get(key) != value:
            raise PlanError(f"base config profile.ci.{key} must be {value!r}, got {ci.get(key)!r}")
    if cfg.get("profile", {}).get("default", {}).get("retries", 0) != 0:
        raise PlanError("base config profile.default.retries must be 0")
    for name, profile in cfg.get("profile", {}).items():
        for override in profile.get("overrides", []):
            if "retries" in override or "flaky-result" in override:
                raise PlanError(f"base config profile.{name} overrides may not set retries; "
                                "use the retry policy file")


def _replace_ci_line(lines, old, new):
    """Swap the one `old` line inside [profile.ci]; the same key also appears under other profiles."""
    starts = [i for i, line in enumerate(lines) if line.strip() == "[profile.ci]"]
    if len(starts) != 1:
        raise PlanError("base config must contain exactly one `[profile.ci]` line")
    start = starts[0]
    end = next((i for i in range(start + 1, len(lines)) if lines[i].lstrip().startswith("[")), len(lines))
    hits = [i for i in range(start + 1, end) if lines[i].strip() == old]
    if len(hits) != 1:
        raise PlanError(f"base config [profile.ci] must contain exactly one `{old}` line")
    lines[hits[0]] = new


def worker_config(base_text, tests, policy, mode="policy"):
    """Base config with this worker's exact default-filter and its retries: the policy overrides, or
    one retry with a passing flaky result for every test under all-once."""
    base = tomllib.loads(base_text)
    _check_base_config(base)
    lines = base_text.splitlines()
    hits = [i for i, line in enumerate(lines) if line.strip() == DEFAULT_FILTER_LINE]
    if len(hits) != 1:
        raise PlanError(f"base config must contain exactly one `{DEFAULT_FILTER_LINE}` line")
    expr = default_filter(tests)
    lines[hits[0]] = f'default-filter = "{expr}"'
    if mode == "all-once":
        _replace_ci_line(lines, "retries = 0", "retries = 1")
        _replace_ci_line(lines, 'flaky-result = "fail"', 'flaky-result = "pass"')
    else:
        for entry in policy:
            lines += ["", "[[profile.ci.overrides]]",
                      f'filter = "{_exact(entry["binary_id"], [entry["test"]])}"',
                      "retries = 1", 'flaky-result = "pass"']
    text = "\n".join(lines) + "\n"
    parsed = tomllib.loads(text)
    ci = parsed["profile"]["ci"]
    retry_filters = [o["filter"] for o in ci.get("overrides", []) if "retries" in o]
    wanted = [] if mode == "all-once" else [_exact(e["binary_id"], [e["test"]]) for e in policy]
    profile_retry = (1, "pass") if mode == "all-once" else (0, "fail")
    if ci.get("default-filter") != expr or retry_filters != wanted or \
            (ci.get("retries"), ci.get("flaky-result")) != profile_retry or \
            any(o.get("retries") != 1 for o in ci.get("overrides", []) if "retries" in o):
        raise PlanError("generated worker config does not round-trip to the planned filter and retries")
    return text


def build_identity(args):
    """Normalized build scope: the parts that must match before two plans are compared."""
    required = {"rust": args.rust_version, "nextest": args.nextest_version, "target": args.target,
                "cargo_profile": args.cargo_profile, "nextest_profile": args.nextest_profile}
    missing = sorted(key for key, value in required.items() if not (value or "").strip())
    if missing:
        raise PlanError(f"plan identity needs {', '.join(missing)}")
    if args.nextest_profile != "ci":
        raise PlanError(f"worker configs are generated for profile ci, not {args.nextest_profile!r}")
    options = sorted(set(args.build_option or []))
    bad = [o for o in options if not BUILD_OPTION_RE.match(o)]
    if bad:
        raise PlanError(f"build options must be KEY=VALUE without spaces: {bad}")
    features = sorted({f.strip() for f in args.features.split(",") if f.strip()})
    if not features:
        raise PlanError("plan identity needs at least one feature (use default)")
    return {"rust": required["rust"], "nextest": required["nextest"], "build": {
        "target": args.target, "features": features, "cargo_profile": args.cargo_profile,
        "nextest_profile": args.nextest_profile, "options": options}}


def _plan_digest(plan):
    return _digest({k: v for k, v in plan.items() if k != "plan_digest"})


def adopt_identity(args):
    """Fill the scope and build flags from an existing plan, so a rebuilt base plan is comparable."""
    ident = load_plan(args.identity_from)["identity"]
    build = ident["build"]
    args.scope, args.rust_version, args.nextest_version = ident["scope"], ident["rust"], ident["nextest"]
    args.target, args.features, args.build_option = build["target"], ",".join(build["features"]), build["options"]
    args.cargo_profile, args.nextest_profile = build["cargo_profile"], build["nextest_profile"]


def only_listed(universe, ignored, path):
    """Keep the lib tests pg_test_classifier.py's JSON names under non_pg."""
    try:
        names = set(json.loads(Path(path).read_text())["non_pg"])
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise PlanError(f"cannot read non_pg names from {path}: {exc}") from exc
    keep = {t for t in universe if t[1] in names}
    if not keep or names - {t[1] for t in universe | ignored}:
        raise PlanError(f"{path} names tests outside the listed universe or selects none")
    return keep, {t for t in ignored if t[1] in names}


def cmd_plan(args):
    if args.identity_from:
        adopt_identity(args)
    if not (args.scope or "").strip():
        raise PlanError("plan identity needs scope")
    build = build_identity(args)
    universe, ignored = load_universe(args.universe)
    if args.only:
        universe, ignored = only_listed(universe, ignored, args.only)
    timings, invalid = load_timings(args.timings or [])
    weights, timing = weights_for(universe, timings)
    timing["invalid_records"] = invalid
    buckets, loads = lpt(weights, args.workers)
    today = datetime.date.fromisoformat(args.today) if args.today else datetime.datetime.now(datetime.UTC).date()
    policy = load_policy(args.retry_policy, args.scope, today, universe, ignored)
    archive = Path(args.archive)
    if not archive.is_file() or archive.stat().st_size == 0:
        raise PlanError(f"archive {archive} is missing or empty")
    base_text = Path(args.base_config).read_text()
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=True)
    workers = []
    for index, tests in enumerate(buckets):
        text = worker_config(base_text, tests, policy, args.retry_mode)
        (out / f"worker-{index}.toml").write_bytes(text.encode())
        workers.append({"index": index, "tests": [list(t) for t in tests],
                        "expected_seconds": round(loads[index], 3),
                        "config_sha256": hashlib.sha256(text.encode()).hexdigest()})
    plan = {"kind": PLAN_KIND, "version": PLAN_VERSION,
            "identity": {"sha": args.sha, "archive_sha256": _sha256_file(archive), "scope": args.scope,
                         "run_id": args.run_id, **build},
            "universe_count": len(universe), "universe_digest": _digest(sorted(list(t) for t in universe)),
            "ignored": sorted(list(t) for t in ignored), "retry_policy": policy, "retry_mode": args.retry_mode,
            "timing": timing, "workers": workers}
    plan["plan_digest"] = _plan_digest(plan)
    (out / "plan.json").write_text(json.dumps(plan, indent=1, sort_keys=True) + "\n")
    print(f"planned {len(universe)} tests on {args.workers} workers "
          f"(timing {timing['mode']}, known {timing['known']}, fallback {timing['fallback']}, "
          f"retry mode {args.retry_mode}); "
          f"plan {plan['plan_digest'][:12]}")
    return 0


def retry_mode(plan):
    """Plans written before the mode existed ran under the policy."""
    return plan.get("retry_mode", "policy")


def load_plan(path):
    try:
        plan = json.loads(Path(path).read_text())
    except (OSError, ValueError) as exc:
        raise PlanError(f"cannot read plan {path}: {exc}") from exc
    if not isinstance(plan, dict) or plan.get("kind") != PLAN_KIND or plan.get("version") != PLAN_VERSION:
        raise PlanError(f"plan {path} is not a version {PLAN_VERSION} {PLAN_KIND}")
    if plan.get("plan_digest") != _plan_digest(plan):
        raise PlanError("plan digest does not match its content")
    if retry_mode(plan) not in RETRY_MODES:
        raise PlanError(f"plan retry_mode {plan['retry_mode']!r} is not one of {RETRY_MODES}")
    seen = set()
    for n, worker in enumerate(plan["workers"]):
        tests = {tuple(t) for t in worker["tests"]}
        if worker["index"] != n or not tests or len(tests) != len(worker["tests"]) or tests & seen:
            raise PlanError(f"plan worker {n} is empty, out of order or overlaps another worker")
        seen |= tests
    if len(seen) != plan["universe_count"] or _digest(sorted(list(t) for t in seen)) != plan["universe_digest"]:
        raise PlanError("plan workers do not cover the listed universe exactly")
    return plan


def cmd_verify_archive(args):
    plan = load_plan(args.plan)
    ident = plan["identity"]
    if not 0 <= args.worker < len(plan["workers"]):
        raise PlanError(f"worker {args.worker} is not in the plan")
    problems = [f"{name} is set; retries and threads come only from the generated config"
                for name in FORBIDDEN_ENV if os.environ.get(name)]
    if args.sha != ident["sha"]:
        problems.append(f"checkout {args.sha} is not the planned {ident['sha']}")
    if args.run_id != ident["run_id"]:
        problems.append(f"run {args.run_id} is not the planned run {ident['run_id']}")
    archive_sha = _sha256_file(args.archive) if Path(args.archive).is_file() else None
    if archive_sha != ident["archive_sha256"]:
        problems.append("archive digest differs from the plan; rebuild through the build job, not here")
    config_sha = _sha256_file(args.config) if Path(args.config).is_file() else None
    if config_sha != plan["workers"][args.worker]["config_sha256"]:
        problems.append(f"config {args.config} is not the planned worker-{args.worker}.toml")
    if problems:
        for problem in problems:
            print(f"ERROR {problem}", file=sys.stderr)
        return 1
    record = Path(args.record)
    record.mkdir(parents=True, exist_ok=True)
    (record / "worker.json").write_text(json.dumps({
        "version": WORKER_VERSION, "worker": args.worker, "plan_digest": plan["plan_digest"],
        "sha": args.sha, "archive_sha256": archive_sha, "run_id": args.run_id,
        "run_attempt": args.run_attempt}, sort_keys=True) + "\n")
    print(f"worker {args.worker} preflight ok ({len(plan['workers'][args.worker]['tests'])} tests)")
    return 0


def classify(case):
    """(outcome, attempts) for one nextest JUnit testcase."""
    if case.find("skipped") is not None:
        return "skipped", 0
    retries = sum(len(case.findall(tag)) for tag in RETRY_TAGS)
    failed = case.find("failure") is not None or case.find("error") is not None
    return ("failed" if failed else "passed"), 1 + retries


def audit_worker(plan, index, wdir, policy_ids):
    """Problems, executed-case details and summary row for one planned worker."""
    problems, planned = [], {tuple(t) for t in plan["workers"][index]["tests"]}
    row = {"index": index, "planned": len(planned), "executed": 0, "exit_code": None, "seconds": 0.0,
           "total_attempt_seconds": 0.0}

    def bad(kind, detail, test_id=None):
        problems.append({"worker": index, "kind": kind, "detail": detail,
                         **({"id": _id_text(test_id)} if test_id else {})})

    if not wdir.is_dir():
        bad("missing-artifact", f"no artifact directory {wdir.name}")
        return problems, [], row
    try:
        meta = json.loads((wdir / "worker.json").read_text())
        if not isinstance(meta, dict):
            bad("bad-identity", f"worker.json is {type(meta).__name__}, expected an object")
            meta = None
    except (OSError, ValueError) as exc:
        bad("bad-identity", f"worker.json unreadable: {exc}")
        meta = None
    if meta is not None:
        ident = plan["identity"]
        want = {"version": WORKER_VERSION, "worker": index, "plan_digest": plan["plan_digest"],
                "sha": ident["sha"], "archive_sha256": ident["archive_sha256"], "run_id": ident["run_id"]}
        for key, value in want.items():
            if meta.get(key) != value:
                bad("bad-identity", f"worker.json {key}={meta.get(key)!r}, planned {value!r}")
    try:
        row["exit_code"] = int((wdir / "exit-code").read_text().strip())
    except (OSError, ValueError) as exc:
        bad("missing-exit-code", f"exit-code unreadable: {exc}")
    if row["exit_code"] not in (None, 0):
        bad("nonzero-exit", f"nextest exited {row['exit_code']}")
    try:
        root = _junit_root(wdir / "junit.xml")
    except PlanError as exc:
        bad("missing-junit", str(exc))
        return problems, [], row
    executed, cases, all_once = set(), [], retry_mode(plan) == "all-once"
    for binary_id, case in _cases(root):
        test_id = (binary_id, case.get("name"))
        outcome, attempts = classify(case)
        if outcome == "skipped":
            bad("unexpected-skip", "testcase reported as skipped", test_id)
            continue
        if test_id in executed:
            bad("duplicate", "testcase reported twice", test_id)
            continue
        executed.add(test_id)
        seconds, total = case_seconds(case)
        if total is None:
            bad("bad-time", "testcase or retry time is missing or invalid", test_id)
            seconds = total = 0.0
        row["seconds"] += seconds
        row["total_attempt_seconds"] += total
        cases.append({"id": test_id, "outcome": outcome, "attempts": attempts, "seconds": seconds,
                      "total_attempt_seconds": total})
        if outcome == "failed":
            bad("failed" if attempts == 1 else "retry-exhausted",
                f"failed after {attempts} attempt(s)", test_id)
        if attempts > 1 and not all_once and test_id not in policy_ids:
            bad("retry-not-allowed", f"{attempts} attempts but not in the retry policy", test_id)
        elif attempts > 2:
            bad("retry-over-limit", f"{attempts} attempts; at most one retry is allowed", test_id)
    row["executed"] = len(executed)
    if not executed:
        bad("zero-tests", "no testcase executed")
    for test_id in sorted(planned - executed):
        bad("missing", "planned but not executed", test_id)
    for test_id in sorted(executed - planned):
        bad("extra", "executed but not planned for this worker", test_id)
    row["seconds"] = round(row["seconds"], 3)
    row["total_attempt_seconds"] = round(row["total_attempt_seconds"], 3)
    return problems, cases, row


def audit(plan, results):
    policy = {(e["binary_id"], e["test"]): e for e in plan["retry_policy"]}
    mode = retry_mode(plan)
    problems, rows, retry_pass, totals = [], [], [], {"planned": plan["universe_count"], "executed": 0,
                                                      "passed": 0, "failed": 0, "retry_pass": 0}
    planned_dirs = {f"worker-{w['index']}" for w in plan["workers"]}
    present = {p.name for p in results.glob("worker-*")} if results.is_dir() else set()
    for name in sorted(present - planned_dirs):
        problems.append({"worker": None, "kind": "unplanned-artifact", "detail": f"{name} is not in the plan"})
    for worker in plan["workers"]:
        found, cases, row = audit_worker(plan, worker["index"], results / f"worker-{worker['index']}",
                                         set(policy))
        problems += found
        rows.append(row)
        totals["executed"] += row["executed"]
        for case in cases:
            totals[case["outcome"]] += 1
            if case["outcome"] == "passed" and case["attempts"] == 2 and (mode == "all-once" or case["id"] in policy):
                entry = policy.get(case["id"], {"issue": None, "expires": None})
                retry_pass.append({"id": _id_text(case["id"]), "worker": worker["index"],
                                   "attempts": case["attempts"], "seconds": case["seconds"],
                                   "total_attempt_seconds": case["total_attempt_seconds"],
                                   "issue": entry["issue"], "expires": entry["expires"]})
    totals["retry_pass"] = len(retry_pass)
    if totals["executed"] == 0 and not any(p["kind"] == "zero-tests" for p in problems):
        problems.append({"worker": None, "kind": "zero-tests", "detail": "no testcase executed"})
    return {"version": VERDICT_VERSION, "verdict": "fail" if problems else "pass", "retry_mode": mode,
            "plan_digest": plan["plan_digest"], "identity": plan["identity"], "totals": totals,
            "workers": rows, "retry_pass": retry_pass, "problems": problems}


def render_summary(verdict, limit=50):
    t, ident = verdict["totals"], verdict["identity"]
    lines = [f"## nextest {ident['scope']}: {verdict['verdict'].upper()}", "",
             f"SHA `{ident['sha']}` · plan `{verdict['plan_digest'][:12]}` · "
             f"executed {t['executed']}/{t['planned']} · failed {t['failed']} · retry-pass {t['retry_pass']} · "
             f"retry mode {verdict['retry_mode']}", "",
             "| worker | planned | executed | exit | seconds | all attempts |", "|---|---|---|---|---|---|"]
    lines += [f"| {r['index']} | {r['planned']} | {r['executed']} | {r['exit_code']} | {r['seconds']} "
              f"| {r['total_attempt_seconds']} |"
              for r in verdict["workers"]]
    if verdict["retry_pass"]:
        lines += ["", "### FLAKY: passed only on retry", "", "| test | attempts | seconds | all attempts | issue | expires |",
                  "|---|---|---|---|---|---|"]
        lines += [f"| `{r['id']}` | {r['attempts']} | {r['seconds']} | {r['total_attempt_seconds']} "
                  f"| {r['issue'] or '-'} | {r['expires'] or '-'} |"
                  for r in verdict["retry_pass"]]
    if verdict["problems"]:
        lines += ["", f"### Problems ({len(verdict['problems'])})", ""]
        for p in verdict["problems"][:limit]:
            where = "-" if p["worker"] is None else p["worker"]
            lines.append(f"- worker {where} `{p['kind']}` {p.get('id', '')} {p['detail']}".rstrip())
        if len(verdict["problems"]) > limit:
            lines.append(f"- … {len(verdict['problems']) - limit} more in the verdict JSON")
    return "\n".join(lines) + "\n"


def cmd_audit(args):
    try:
        plan = load_plan(args.plan)
    except PlanError as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 1
    verdict = audit(plan, Path(args.results))
    Path(args.json_out).write_text(json.dumps(verdict, indent=1, sort_keys=True) + "\n")
    summary = render_summary(verdict)
    Path(args.summary_out).write_text(summary)
    print(summary)
    if verdict["retry_pass"]:
        # One workflow warning annotation naming every FLAKY test; %0A is an escaped newline.
        names = "%0A".join(r["id"] for r in verdict["retry_pass"])
        print(f"::warning title=FLAKY tests passed only on retry ({len(verdict['retry_pass'])})::{names}")
    return 0 if verdict["verdict"] == "pass" else 1


REMOVAL_KINDS = ("removed", "runnable->ignored")


def load_justifications(path):
    """Reasons keyed by test, kind and optional explicit platform scope."""
    rows = {}
    for n, line in enumerate(Path(path).read_text().splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = [part.strip() for part in line.split("|")]
        if len(parts) not in (3, 4) or parts[1] not in REMOVAL_KINDS or not parts[2] or len(parts[0].split(" ")) != 2:
            raise PlanError(f"{path}:{n}: need `<binary-id> <test> | removed|runnable->ignored | <reason> [| platform=<os>]`")
        _check_id(*parts[0].split(" "))
        platform = None
        if len(parts) == 4:
            if parts[1] != "removed" or parts[3] not in ("platform=linux", "platform=macos", "platform=windows"):
                raise PlanError(f"{path}:{n}: only removed rows may name platform=linux, platform=macos or platform=windows")
            platform = parts[3].split("=", 1)[1]
        if any(t == parts[0] and k == parts[1] and (p == platform or p is None or platform is None)
               for t, k, p in rows):
            raise PlanError(f"{path}:{n}: {parts[0]} {parts[1]} is listed twice")
        rows[(parts[0], parts[1], platform)] = parts[2]
    return rows


def platform_losses(path, binary_id):
    """Native losses and IDs present in either source tree; unknown presence stays fail-closed."""
    try:
        doc = json.loads(Path(path).read_text())
        lost = {(kind, p): doc[key][p] for kind, key in (("removed", "removed"), ("runnable->ignored", "runnable_to_ignored"))
                for p in ("macos", "windows")}
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise PlanError(f"cannot read source losses {path}: {exc}") from exc
    found = {}
    for (kind, platform), names in lost.items():
        for name in names:
            found.setdefault((_id_text(_check_id(binary_id, name)), kind), []).append(platform)
    presence = doc.get("present_tests")
    if not isinstance(presence, dict) or any(not isinstance(presence.get(p), list) for p in ("macos", "windows")):
        return found, None
    names = presence["macos"] + presence["windows"]
    if any(not isinstance(name, str) or not ID_RE.fullmatch(name) for name in names):
        return found, None
    by_platform = {p: {_check_id(binary_id, name) for name in presence[p]} for p in ("macos", "windows")}
    if isinstance(presence.get("linux"), list) and all(isinstance(t, str) and ID_RE.fullmatch(t) for t in presence["linux"]):
        by_platform["linux"] = {_check_id(binary_id, name) for name in presence["linux"]}
    return found, by_platform


def cmd_base_diff(args):
    plan = load_plan(args.plan)
    justified = load_justifications(args.justifications)
    path = Path(args.baseline)
    if not path.is_file():
        print(f"ERROR no baseline plan; regenerate it at {args.comparison_base_sha}", file=sys.stderr)
        return 2
    base = load_plan(path)
    if base["identity"]["sha"] != args.comparison_base_sha or base["identity"]["scope"] != plan["identity"]["scope"]:
        print(f"ERROR baseline is {base['identity']['sha']} {base['identity']['scope']}, need "
              f"{args.comparison_base_sha} {plan['identity']['scope']}; regenerate it", file=sys.stderr)
        return 2
    if base["identity"]["build"] != plan["identity"]["build"]:
        print(f"ERROR baseline build {_canonical(base['identity']['build'])} differs from "
              f"{_canonical(plan['identity']['build'])} under the same scope; regenerate it", file=sys.stderr)
        return 2

    def sets(p):
        run = {tuple(t) for w in p["workers"] for t in w["tests"]}
        return run, {tuple(t) for t in p["ignored"]}

    run, ign = sets(plan)
    base_run, base_ign = sets(base)
    binaries = {t[0] for t in run | ign}
    if len(binaries) != 1:
        raise PlanError(f"source removals need one lib binary in the plan, found {sorted(binaries)}")
    platform_lost, native_present = platform_losses(args.source_removed, binaries.pop())
    diff = {"comparison_base_sha": args.comparison_base_sha, "scope": plan["identity"]["scope"],
            "removed": sorted(_id_text(t) for t in (base_run | base_ign) - (run | ign)),
            "runnable_to_ignored": sorted(_id_text(t) for t in base_run & ign),
            "ignored_to_runnable": sorted(_id_text(t) for t in base_ign & run),
            "added": sorted(_id_text(t) for t in (run | ign) - (base_run | base_ign)),
            "platform_losses": {f"{t} | {kind}": p for (t, kind), p in sorted(platform_lost.items())}}
    losses = {key: set(platforms) for key, platforms in platform_lost.items()}
    for kind, field in (("removed", "removed"), ("runnable->ignored", "runnable_to_ignored")):
        for test in diff[field]:
            losses.setdefault((test, kind), set()).add("linux")
    missing = {(t, kind) for (t, kind), platforms in losses.items()
               if any((t, kind, None) not in justified and (t, kind, p) not in justified for p in platforms)}
    diff["unjustified"] = sorted(f"{t} | {kind}" for t, kind in missing)
    matched = {row for row in justified if row[:2] in losses
               and (row[2] is None or row[2] in losses[row[:2]])}
    stale = set(justified) - matched
    present = base_run | base_ign | run | ign
    # A platform exclusion remains meaningful only while another platform retains coverage.
    def platform_exclusion_remains(row):
        test, _kind, platform = row
        if platform is None or native_present is None or platform not in native_present:
            return False
        test_id = tuple(test.split(" "))
        target = native_present[platform] | (present if platform == "linux" else set())
        others = (present if platform != "linux" else set()) | set().union(
            *(ids for p, ids in native_present.items() if p != platform))
        return test_id not in target and test_id in others

    carried = {row for row in stale if platform_exclusion_remains(row)}
    stale -= carried
    # Legacy rows retain their existing whole-test removal and warning behavior.
    native_union = native_present["macos"] | native_present["windows"] if native_present is not None else None
    landed = {row for row in stale if row[2] is None
              and native_union is not None and tuple(row[0].split(" ")) not in present | native_union}

    def row_text(row):
        t, kind, platform = row
        return f"{t} | {kind}" + (f" | platform={platform}" if platform else "")

    diff["stale_justifications"] = sorted(map(row_text, stale))
    diff["landed_justifications"] = sorted(map(row_text, landed))
    diff["platform_justifications"] = sorted(map(row_text, carried))
    Path(args.json_out).write_text(json.dumps(diff, indent=1, sort_keys=True) + "\n")
    print(f"removed {len(diff['removed'])} · runnable->ignored {len(diff['runnable_to_ignored'])} · "
          f"platform losses {len(platform_lost)} · ignored->runnable {len(diff['ignored_to_runnable'])} · "
          f"added {len(diff['added'])} · justified {len(losses) - len(diff['unjustified'])}")
    for row in diff["unjustified"]:
        print(f"ERROR lost without a justification row: {row}", file=sys.stderr)
    for row in sorted(landed):
        print(f"WARN justification row refers to a test absent from base and head; clear in any PR: "
              f"{row_text(row)} | {justified[row]}", file=sys.stderr)
    for row in sorted(stale - landed, key=row_text):
        print(f"ERROR justification row matches no loss against {args.comparison_base_sha}: {row_text(row)}", file=sys.stderr)
    return 1 if diff["unjustified"] or stale - landed else 0


# The plan and JUnit readers that nextest_required reuses.
_SELF = argparse.Namespace(load_plan=load_plan, PlanError=PlanError, _junit_root=_junit_root, _cases=_cases,
                           classify=classify)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("plan")
    p.add_argument("--universe", required=True)
    p.add_argument("--archive", required=True)
    p.add_argument("--workers", type=int, required=True)
    p.add_argument("--sha", required=True)
    p.add_argument("--scope")
    p.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", "local"))
    p.add_argument("--rust-version")
    p.add_argument("--nextest-version")
    p.add_argument("--target", help="target triple the archive was built for")
    p.add_argument("--features", default="default", help="comma-separated cargo features of the build")
    p.add_argument("--cargo-profile", default="test")
    p.add_argument("--nextest-profile", default="ci")
    p.add_argument("--build-option", action="append", help="KEY=VALUE build env such as RUSTFLAGS= (repeatable)")
    p.add_argument("--timings", action="append", help="nextest JUnit from an earlier run (repeatable)")
    p.add_argument("--base-config", default=".config/nextest.toml")
    p.add_argument("--retry-policy", default="scripts/ci/nextest-retry-policy.toml")
    p.add_argument("--retry-mode", choices=RETRY_MODES, default="policy",
                   help="policy: retry only the policy entries; all-once: retry every test once (FLAKY passes)")
    p.add_argument("--today", help="YYYY-MM-DD for policy expiry (default: UTC today)")
    p.add_argument("--identity-from", help="plan.json whose scope and build flags this plan takes")
    p.add_argument("--only", help="pg_test_classifier.py JSON; plan only the tests it names under non_pg")
    p.add_argument("--output", required=True)
    v = sub.add_parser("verify-archive")
    v.add_argument("--plan", required=True)
    v.add_argument("--worker", type=int, required=True)
    v.add_argument("--archive", required=True)
    v.add_argument("--config", required=True)
    v.add_argument("--sha", required=True)
    v.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", "local"))
    v.add_argument("--run-attempt", default=os.environ.get("GITHUB_RUN_ATTEMPT", "1"))
    v.add_argument("--record", required=True)
    a = sub.add_parser("audit")
    a.add_argument("--plan", required=True)
    a.add_argument("--results", required=True, help="directory holding worker-<i>/{worker.json,exit-code,junit.xml}")
    a.add_argument("--json-out", required=True)
    a.add_argument("--summary-out", required=True)
    b = sub.add_parser("base-diff")
    b.add_argument("--plan", required=True)
    b.add_argument("--baseline", required=True)
    b.add_argument("--comparison-base-sha", required=True)
    b.add_argument("--justifications", required=True, help="scripts/ci/test-removal-justifications.txt")
    b.add_argument("--source-removed", required=True, help="rust_test_sources.py removed JSON")
    b.add_argument("--json-out", required=True)
    delegated = nextest_required.register(sub)
    args = parser.parse_args(argv)
    handler = {"plan": cmd_plan, "verify-archive": cmd_verify_archive,
               "audit": cmd_audit, "base-diff": cmd_base_diff}.get(args.command)
    try:
        return handler(args) if handler else delegated[args.command](_SELF, args)
    except PlanError as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
