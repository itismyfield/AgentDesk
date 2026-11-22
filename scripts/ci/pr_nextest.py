"""PR-only phase layout and observations for the complete nextest plan."""

import datetime
import json
import os
from pathlib import Path

import nextest_required


def phase_layout(ci, args, weights):
    """Use separate LPT queues so the first wave uses the same concurrency as the sweep."""
    from nextest_scope import select_scope
    from pr_scope_core import CoreScopeError, MissingCoreTests, select_core_ids

    repo = Path.cwd()
    universe = set(weights)
    branch = os.environ.get("GITHUB_HEAD_REF", "")
    reason = None
    if branch.startswith("land/batch-"):
        reason = "landing-batch"
    else:
        try:
            _, base = nextest_required.comparison_base(
                repo, args.sha, os.environ.get("PR_HEAD_SHA", ""), "origin/main")
        except (OSError, ValueError):
            reason = "git-error"
    if reason is None:
        try:
            core = select_core_ids(repo, universe)
        except MissingCoreTests as exc:
            raise ci.PlanError(str(exc)) from exc
        except CoreScopeError:
            reason = "core-mapping"
    if reason is None:
        choice = select_scope(repo, base, args.sha, branch, universe, core_ids=core)
        reason = choice.reason
        if choice.mode == "scope":
            if not core <= choice.ids <= universe:
                raise ci.PlanError("scope selection does not contain the complete required core")
            if choice.ids and choice.ids < universe:
                scope, scope_loads = ci.lpt({t: weights[t] for t in choice.ids}, min(args.workers, len(choice.ids)))
                other = universe - choice.ids
                rest, rest_loads = ci.lpt({t: weights[t] for t in other}, min(args.workers, len(other)))
                return scope + rest, scope_loads + rest_loads, {
                    "mode": "scoped", "reason": reason, "scope_workers": len(scope)}
            reason = "scope-covers-all"
    workers, loads = ci.lpt(weights, args.workers)
    return workers, loads, {"mode": "full", "reason": reason, "scope_workers": 0}


def validate_layout(ci, plan):
    phase = plan["pr_scope"]
    mode, count = phase.get("mode"), phase.get("scope_workers")
    if not (
        type(count) is int and isinstance(phase.get("reason"), str) and phase["reason"]
        and ((mode == "full" and count == 0) or (mode == "scoped" and 0 < count < len(plan["workers"])))
    ):
        raise ci.PlanError("invalid PR scope layout")
    return count


def base_diff_inventory(ci, args, plan, planned):
    """Use independent discovery for PR removal checks and bind it to the full plan."""
    if not args.head_universe:
        if "pr_scope" in plan:
            raise ci.PlanError("PR base comparison requires the unfiltered head discovery list")
        return planned
    inventory = ci.load_universe(args.head_universe)
    if inventory != planned:
        raise ci.PlanError("unfiltered head discovery does not match the full plan")
    return inventory


def write_matrix(plan, path):
    count = plan["pr_scope"]["scope_workers"]
    with open(path, "a") as output:
        output.write(f"scope_workers={json.dumps(list(range(count)), separators=(',', ':'))}\n")
        output.write(f"full_workers={json.dumps(list(range(count, len(plan['workers']))), separators=(',', ':'))}\n")


def cmd_scope_worker(ci, args):
    plan = ci.load_plan(args.plan)
    if "pr_scope" not in plan or not 0 <= args.worker < validate_layout(ci, plan):
        raise ci.PlanError(f"worker {args.worker} is not a scope worker")
    policy_ids = {(entry["binary_id"], entry["test"]) for entry in plan["retry_policy"]}
    problems, _, _ = ci.audit_worker(plan, args.worker, Path(args.record), policy_ids)
    for problem in problems[:20]:
        print(f"ERROR scope worker {args.worker} {problem['kind']}: {problem.get('id', '')} {problem['detail']}")
    return int(bool(problems))


def _phase_complete(verdict, indices):
    problems = {}
    for problem in verdict["problems"]:
        problems.setdefault(problem["worker"], set()).add(problem["kind"])
    if None in problems:
        return False
    failed_kinds = {"failed", "retry-exhausted"}
    for index in indices:
        row = verdict["workers"][index]
        kinds = problems.get(index, set())
        expected_exit = 100 if kinds & failed_kinds else 0
        if (row["executed"] != row["planned"] or row["exit_code"] != expected_exit
                or kinds - failed_kinds - {"nonzero-exit"}):
            return False
    return True


def _scope_seconds(results, count):
    # An Actions partial rerun may reuse successful workers from older attempts.
    attempt = os.environ["GITHUB_RUN_ATTEMPT"]
    windows = []
    for index in range(count):
        try:
            record = results / f"worker-{index}"
            meta = json.loads((record / "worker.json").read_text())
            if not isinstance(meta, dict) or meta.get("run_attempt") != attempt:
                return None
            start, end = map(int, (record / "scope-time").read_text().split())
            if start <= 0 or start > end:
                return None
            windows.append((start, end))
        except (OSError, ValueError):
            return None
    return max(end for _, end in windows) - min(start for start, _ in windows) if windows else None


def write_observation(path, summary, *, mode, reason, **fields):
    record = {
        "version": 1, "recorded_at": datetime.datetime.now(datetime.UTC).isoformat(),
        "run_id": os.environ["GITHUB_RUN_ID"], "run_attempt": os.environ["GITHUB_RUN_ATTEMPT"],
        "mode": mode, "reason": reason, "scope_ids": [], "scope_seconds": None,
        "scope_state": "not_run", "full_state": "incomplete", "scope_failure_ids": [],
        "full_failure_ids": [], "scope_miss_ids": [], **fields,
    }
    Path(path).write_text(json.dumps(record, sort_keys=True) + "\n")
    names = ", ".join(f"`{b} {n}`" for b, n in record["scope_miss_ids"][:10]) or "—"
    misses = len(record["scope_miss_ids"])
    duration = "—" if record["scope_seconds"] is None else f"{record['scope_seconds']}s"
    line = (f"PR Rust scope: {mode} ({reason}) · {len(record['scope_ids'])} IDs · {duration} "
            f"· scope {record['scope_state']} · full {record['full_state']} · SCOPE_MISS={misses} {names}")
    if misses > 10:
        line += " (complete IDs in artifact)"
    print(line)
    if summary:
        with open(summary, "a") as output:
            output.write(line + "\n\n")


def audit_observation(ci, args, plan, verdict, cases):
    count = validate_layout(ci, plan)
    phase = plan["pr_scope"]
    scope_ids = {tuple(t) for w in plan["workers"][:count] for t in w["tests"]}
    # A foreign, extra or otherwise invalid testcase must never become an observed miss.
    unusable = {p["worker"] for p in verdict["problems"] if p["kind"] in
                ("bad-identity", "missing-exit-code", "extra", "duplicate")}
    failed = {case["id"] for case in cases if case["outcome"] == "failed"
              and case["worker"] not in unusable and verdict["workers"][case["worker"]]["exit_code"] == 100}
    early, late = failed & scope_ids, failed - scope_ids
    scope_state = "not_run"
    if count:
        scope_state = "failed" if early else "incomplete"
        if args.pr_scope_result == "success" and not early and _phase_complete(verdict, range(count)):
            scope_state = "passed"
    full_state = "incomplete"
    if args.pr_full_result == "skipped":
        full_state, late = "not_run", set()
    elif ((not count or scope_state == "passed") and args.pr_full_result in ("success", "failure")
          and _phase_complete(verdict, range(count, len(plan["workers"])))):
        full_state = "complete"
    write_observation(
        args.pr_scope_out, os.environ.get("GITHUB_STEP_SUMMARY"), mode=phase["mode"], reason=phase["reason"],
        scope_ids=sorted(scope_ids), scope_seconds=_scope_seconds(Path(args.results), count),
        scope_state=scope_state, full_state=full_state, scope_failure_ids=sorted(early),
        full_failure_ids=sorted(late), scope_miss_ids=sorted(late) if count else [],
    )


def cmd_unavailable(args):
    skipped = args.reason == "rust-path-filter"
    write_observation(args.output, os.environ.get("GITHUB_STEP_SUMMARY"),
                      mode="skipped" if skipped else "full", reason=args.reason,
                      full_state="not_run" if skipped else "incomplete")
    return 0


def register(sub):
    worker = sub.add_parser("audit-scope-worker")
    worker.add_argument("--plan", required=True)
    worker.add_argument("--worker", type=int, required=True)
    worker.add_argument("--record", required=True)
    unavailable = sub.add_parser("pr-scope-unavailable")
    unavailable.add_argument("--output", required=True)
    unavailable.add_argument("--reason", choices=("rust-path-filter", "rust-build-unavailable", "plan-unavailable"),
                             required=True)
    return {"audit-scope-worker": cmd_scope_worker, "pr-scope-unavailable": lambda _, args: cmd_unavailable(args)}
