#!/usr/bin/env python3
"""Plan, preflight and audit time-weighted cargo-nextest workers for CI.

  plan            nextest list JSON + archive -> plan.json and one config per worker (LPT by duration)
  verify-archive  worker preflight: archive digest, checkout SHA, run and config must match the plan
  audit           per-worker artifacts -> verdict JSON and Markdown summary; any gap is red
  base-diff       compare a plan with the plan of a fixed comparison base SHA
  required-context, comparison-base, classify-worker   see nextest_required.py

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


def _exact(binary_id, tests):
    names = " | ".join(f"test(={t})" for t in tests)
    return f"binary_id(={binary_id}) & ({names})"


def default_filter(tests):
    by_binary = {}
    for binary_id, name in tests:
        by_binary.setdefault(binary_id, []).append(name)
    return " | ".join(f"({_exact(b, sorted(n))})" for b, n in sorted(by_binary.items()))


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


def worker_config(base_text, tests, policy):
    """Base config with this worker's exact default-filter and the policy retry overrides appended."""
    base = tomllib.loads(base_text)
    _check_base_config(base)
    lines = base_text.splitlines()
    hits = [i for i, line in enumerate(lines) if line.strip() == DEFAULT_FILTER_LINE]
    if len(hits) != 1:
        raise PlanError(f"base config must contain exactly one `{DEFAULT_FILTER_LINE}` line")
    expr = default_filter(tests)
    lines[hits[0]] = f'default-filter = "{expr}"'
    for entry in policy:
        lines += ["", "[[profile.ci.overrides]]",
                  f'filter = "{_exact(entry["binary_id"], [entry["test"]])}"',
                  "retries = 1", 'flaky-result = "pass"']
    text = "\n".join(lines) + "\n"
    parsed = tomllib.loads(text)
    ci = parsed["profile"]["ci"]
    retry_filters = [o["filter"] for o in ci.get("overrides", []) if "retries" in o]
    wanted = [_exact(e["binary_id"], [e["test"]]) for e in policy]
    if ci.get("default-filter") != expr or retry_filters != wanted or \
            any(o.get("retries") != 1 for o in ci.get("overrides", []) if "retries" in o):
        raise PlanError("generated worker config does not round-trip to the planned filter and policy")
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


def cmd_plan(args):
    build = build_identity(args)
    universe, ignored = load_universe(args.universe)
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
        text = worker_config(base_text, tests, policy)
        (out / f"worker-{index}.toml").write_text(text)
        workers.append({"index": index, "tests": [list(t) for t in tests],
                        "expected_seconds": round(loads[index], 3),
                        "config_sha256": hashlib.sha256(text.encode()).hexdigest()})
    plan = {"kind": PLAN_KIND, "version": PLAN_VERSION,
            "identity": {"sha": args.sha, "archive_sha256": _sha256_file(archive), "scope": args.scope,
                         "run_id": args.run_id, **build},
            "universe_count": len(universe), "universe_digest": _digest(sorted(list(t) for t in universe)),
            "ignored": sorted(list(t) for t in ignored), "retry_policy": policy,
            "timing": timing, "workers": workers}
    plan["plan_digest"] = _plan_digest(plan)
    (out / "plan.json").write_text(json.dumps(plan, indent=1, sort_keys=True) + "\n")
    print(f"planned {len(universe)} tests on {args.workers} workers "
          f"(timing {timing['mode']}, known {timing['known']}, fallback {timing['fallback']}); "
          f"plan {plan['plan_digest'][:12]}")
    return 0


def load_plan(path):
    try:
        plan = json.loads(Path(path).read_text())
    except (OSError, ValueError) as exc:
        raise PlanError(f"cannot read plan {path}: {exc}") from exc
    if not isinstance(plan, dict) or plan.get("kind") != PLAN_KIND or plan.get("version") != PLAN_VERSION:
        raise PlanError(f"plan {path} is not a version {PLAN_VERSION} {PLAN_KIND}")
    if plan.get("plan_digest") != _plan_digest(plan):
        raise PlanError("plan digest does not match its content")
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
    executed, cases = set(), []
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
        if attempts > 1 and test_id not in policy_ids:
            bad("retry-not-allowed", f"{attempts} attempts but not in the retry policy", test_id)
        elif attempts > 2:
            bad("retry-over-limit", f"{attempts} attempts; the policy allows one retry", test_id)
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
            if case["outcome"] == "passed" and case["attempts"] == 2 and case["id"] in policy:
                entry = policy[case["id"]]
                retry_pass.append({"id": _id_text(case["id"]), "worker": worker["index"],
                                   "attempts": case["attempts"], "seconds": case["seconds"],
                                   "total_attempt_seconds": case["total_attempt_seconds"],
                                   "issue": entry["issue"], "expires": entry["expires"]})
    totals["retry_pass"] = len(retry_pass)
    if totals["executed"] == 0 and not any(p["kind"] == "zero-tests" for p in problems):
        problems.append({"worker": None, "kind": "zero-tests", "detail": "no testcase executed"})
    return {"version": VERDICT_VERSION, "verdict": "fail" if problems else "pass",
            "plan_digest": plan["plan_digest"], "identity": plan["identity"], "totals": totals,
            "workers": rows, "retry_pass": retry_pass, "problems": problems}


def render_summary(verdict, limit=50):
    t, ident = verdict["totals"], verdict["identity"]
    lines = [f"## nextest {ident['scope']}: {verdict['verdict'].upper()}", "",
             f"SHA `{ident['sha']}` · plan `{verdict['plan_digest'][:12]}` · "
             f"executed {t['executed']}/{t['planned']} · failed {t['failed']} · retry-pass {t['retry_pass']}", "",
             "| worker | planned | executed | exit | seconds | all attempts |", "|---|---|---|---|---|---|"]
    lines += [f"| {r['index']} | {r['planned']} | {r['executed']} | {r['exit_code']} | {r['seconds']} "
              f"| {r['total_attempt_seconds']} |"
              for r in verdict["workers"]]
    if verdict["retry_pass"]:
        lines += ["", "### Passed only on retry", "", "| test | attempts | seconds | all attempts | issue | expires |",
                  "|---|---|---|---|---|---|"]
        lines += [f"| `{r['id']}` | {r['attempts']} | {r['seconds']} | {r['total_attempt_seconds']} "
                  f"| {r['issue']} | {r['expires']} |"
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
    return 0 if verdict["verdict"] == "pass" else 1


def cmd_base_diff(args):
    plan = load_plan(args.plan)
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
    diff = {"comparison_base_sha": args.comparison_base_sha, "scope": plan["identity"]["scope"],
            "removed": sorted(_id_text(t) for t in (base_run | base_ign) - (run | ign)),
            "runnable_to_ignored": sorted(_id_text(t) for t in base_run & ign),
            "ignored_to_runnable": sorted(_id_text(t) for t in base_ign & run),
            "added": sorted(_id_text(t) for t in (run | ign) - (base_run | base_ign))}
    Path(args.json_out).write_text(json.dumps(diff, indent=1, sort_keys=True) + "\n")
    lost = len(diff["removed"]) + len(diff["runnable_to_ignored"])
    print(f"removed {len(diff['removed'])} · runnable->ignored {len(diff['runnable_to_ignored'])} · "
          f"ignored->runnable {len(diff['ignored_to_runnable'])} · added {len(diff['added'])}")
    return 1 if lost and not args.report_only else 0


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
    p.add_argument("--scope", required=True)
    p.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", "local"))
    p.add_argument("--rust-version", required=True)
    p.add_argument("--nextest-version", required=True)
    p.add_argument("--target", required=True, help="target triple the archive was built for")
    p.add_argument("--features", default="default", help="comma-separated cargo features of the build")
    p.add_argument("--cargo-profile", default="test")
    p.add_argument("--nextest-profile", default="ci")
    p.add_argument("--build-option", action="append", help="KEY=VALUE build env such as RUSTFLAGS= (repeatable)")
    p.add_argument("--timings", action="append", help="nextest JUnit from an earlier run (repeatable)")
    p.add_argument("--base-config", default=".config/nextest.toml")
    p.add_argument("--retry-policy", default="scripts/ci/nextest-retry-policy.toml")
    p.add_argument("--today", help="YYYY-MM-DD for policy expiry (default: UTC today)")
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
    b.add_argument("--json-out", required=True)
    b.add_argument("--report-only", action="store_true")
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
