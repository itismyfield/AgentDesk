"""Required-context verdicts, comparison base and worker failure class for the nextest CI.

  required-context  one branch-protection context from `toJSON(needs)`: N AND its own checks
  comparison-base   the SHA a plan is compared with: merge ref -> HEAD^1, PR head -> merge-base
  classify-worker   infra / test / unknown / pass per worker attempt; a run is retryable only when
                    every failed worker is infra, and infra needs the caller's log evidence
  collect-results   copy each worker's newest attempt artifact (up to this attempt) to worker-<i>/ for audit
  selector-audit    every targeted selector (scripts/ci/targeted-test-selectors.txt) matches a planned test

nextest_ci.py registers these subcommands; this module holds no state of its own.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess

# Jobs whose success is N: one archive build, every planned worker, and the result audit.
N_JOBS = ("rust_build", "rust_tests", "test_evidence")
# A gate is the changes outputs that make a job expected to run; "!x" reads x == "false", () always runs.
CONTEXTS = {
    "Lint": ((N_JOBS, ("rust_tests",)), (("lint",), ("rust_or_policy", "relay_contract"))),
    "Script checks": ((N_JOBS, ("rust_tests",)), (("scripts", "scripts_guards", "scripts_contracts"), ())),
    "Fast check (ubuntu-latest)": ((N_JOBS, ("rust_tests",)), (("lint",), ("rust_or_policy", "relay_contract"))),
    "High-risk recovery": ((N_JOBS, ("rust_tests", "high_risk_recovery")),),
    "Dashboard (Node 22)": ((N_JOBS, ("rust_tests",)), (("dashboard",), ("dashboard",))),
    "Fast targeted tests (ubuntu-latest)": ((N_JOBS, ("rust_tests",)), (("pg15_owner_proof",), ("rust_tests",))),
    "relay-authority-contract": ((N_JOBS, ("rust_tests",)),
                                 (("relay_authority_targets", "relay_authority_mutations"), ("!rust_tests_skip",))),
    "Library test sweep (ubuntu-latest)": ((N_JOBS, ("rust_tests",)),),
    "Fast check cross OS required context (ubuntu-latest)": (
        (N_JOBS, ("rust_tests",)), (("check_fast_cross_os", "check_fast_cross_os_targets"), ("cross_os_rust",))),
}
MAX_ATTEMPTS = 2  # the first attempt plus one infra retry


def artifact_name(worker, attempt):
    """Upload name of one worker attempt, so a retried attempt never reads an older attempt's files."""
    return f"nextest-worker-{worker}-attempt-{attempt}"


def _gate(terms, outputs):
    """True/False when every named output is exactly "true"/"false", else an error string."""
    if not terms:
        return True
    value = False
    for term in terms:
        name = term.lstrip("!")
        raw = outputs.get(name)
        if raw not in ("true", "false"):
            return f"changes output {name} is missing or invalid: {raw!r}"
        value |= (raw == "true") != term.startswith("!")
    return value


def context_verdict(name, needs):
    """(ok, lines) for one required context; every gap fails closed like required-check-mirror.sh."""
    if name not in CONTEXTS:
        return False, [f"unknown required context {name!r}"]
    if not isinstance(needs, dict):
        return False, ["needs is not an object"]
    changes = needs.get("changes") if isinstance(needs.get("changes"), dict) else {}
    if changes.get("result") != "success":
        return False, [f"Changed paths result is {changes.get('result')!r}; failing closed"]
    outputs = changes.get("outputs") if isinstance(changes.get("outputs"), dict) else {}
    ok, lines = True, []
    for jobs, terms in CONTEXTS[name]:
        expected = _gate(terms, outputs)
        for job in jobs:
            entry = needs.get(job)
            result = entry.get("result") if isinstance(entry, dict) else None
            if isinstance(expected, str):
                ok, line = False, f"{job}: {expected}"
            elif entry is None:
                ok, line = False, f"{job}: missing from needs"
            elif result == "success" or (result == "skipped" and not expected):
                line = f"{job}: {result} (expected to run: {str(expected).lower()})"
            else:
                ok, line = False, f"{job}: {result!r} but expected to run: {str(expected).lower()}"
            lines.append(line)
    return ok, lines


def cmd_required_context(ci, args):
    raw = args.needs_json if args.needs_json is not None else os.environ.get("NEEDS_JSON")
    try:
        needs = json.loads(Path(raw[1:]).read_text() if raw and raw.startswith("@") else raw or "")
    except (OSError, ValueError) as exc:
        print(f"::error::needs JSON unreadable: {exc}")
        return 1
    ok, lines = context_verdict(args.name, needs)
    for line in lines:
        print(line if ok else f"::error::{args.name}: {line}")
    print(f"{args.name}: {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


def _git(repo, *argv):
    done = subprocess.run(["git", "-C", str(repo), *argv], capture_output=True, text=True)
    return done.stdout.strip() if done.returncode == 0 else None


def comparison_base(repo, head, pr_head, base_ref):
    """(mode, base SHA) for the tested commit; raises ValueError when neither rule applies."""
    head_sha = _git(repo, "rev-parse", "--verify", f"{head}^{{commit}}")
    pr_sha = _git(repo, "rev-parse", "--verify", f"{pr_head}^{{commit}}")
    if not head_sha or not pr_sha:
        raise ValueError(f"cannot resolve {head!r} or PR head {pr_head!r}")
    parents = (_git(repo, "rev-list", "--parents", "-n", "1", head_sha) or "").split()[1:]
    if head_sha == pr_sha:
        base = _git(repo, "merge-base", base_ref, head_sha)
        if not base:
            raise ValueError(f"no merge-base between {base_ref} and {head_sha}")
        return "head", base
    if len(parents) == 2 and parents[1] == pr_sha:
        return "merge", parents[0]
    raise ValueError(f"{head_sha} is neither the PR head {pr_sha} nor a merge of it")


def cmd_comparison_base(ci, args):
    try:
        mode, base = comparison_base(args.repo, args.head, args.pr_head_sha, args.base_ref)
    except ValueError as exc:
        print(f"ERROR {exc}")
        return 1
    print(f"comparison base {base} ({mode})")
    if args.github_output:
        with open(args.github_output, "a") as out:
            out.write(f"base_sha={base}\nmode={mode}\n")
    if args.baseline is None:
        return 0
    if not Path(args.baseline).is_file():
        print(f"ERROR no baseline plan; regenerate it at {base}")
        return 2
    plan = ci.load_plan(args.baseline)
    if (plan["identity"]["sha"], plan["identity"]["scope"]) != (base, args.scope):
        print(f"ERROR baseline is {plan['identity']['sha']} {plan['identity']['scope']}, need {base} {args.scope}")
        return 2
    return 0


def classify_worker(ci, plan, artifacts, worker, attempt, infra_evidence=False):
    """(class, reason) for one worker attempt. Missing results are infra only with the caller's log evidence."""
    wdir = Path(artifacts) / artifact_name(worker, attempt)
    absent = "infra" if infra_evidence else "unknown"
    if not wdir.is_dir():
        return absent, f"no {wdir.name} artifact; infra log evidence: {str(infra_evidence).lower()}"
    try:
        meta = json.loads((wdir / "worker.json").read_text())
    except (OSError, ValueError):
        return "unknown", "no readable worker.json: the preflight did not pass"
    ident = plan["identity"]
    want = {"worker": worker, "run_attempt": str(attempt), "plan_digest": plan["plan_digest"],
            "sha": ident["sha"], "run_id": ident["run_id"]}
    if not isinstance(meta, dict) or any(str(meta.get(k)) != str(v) for k, v in want.items()):
        return "unknown", f"worker.json does not belong to worker {worker} attempt {attempt} of this plan"
    has_exit, has_junit = (wdir / "exit-code").is_file(), (wdir / "junit.xml").is_file()
    if not has_exit and not has_junit:
        return absent, f"preflight passed but no exit code or JUnit; infra log evidence: {str(infra_evidence).lower()}"
    try:
        root = ci._junit_root(wdir / "junit.xml")
    except ci.PlanError as exc:
        return "unknown", str(exc)
    failed = [case.get("name") for _, case in ci._cases(root) if ci.classify(case)[0] == "failed"]
    if failed:
        return "test", f"{len(failed)} test(s) failed, e.g. {failed[0]}"
    try:
        code = int((wdir / "exit-code").read_text().strip())
    except (OSError, ValueError):
        return "unknown", "JUnit is clean but the exit code is unreadable"
    if code != 0 or not list(ci._cases(root)):
        return "unknown", f"nextest exited {code} with a clean JUnit"
    return "pass", "exit 0 and every testcase passed"


def cmd_classify_worker(ci, args):
    plan = ci.load_plan(args.plan)
    attempt = int(args.run_attempt)
    evidence = set(args.infra_evidence or [])
    classes = []
    for worker in args.worker:
        cls, reason = classify_worker(ci, plan, args.artifacts, worker, attempt, worker in evidence)
        classes.append(cls)
        print(f"worker {worker} attempt {attempt}: {cls} ({reason})")
    # The worst class decides: one test or unknown worker vetoes the retry of every other worker.
    cls = next(c for c in ("test", "unknown", "infra", "pass") if c in classes)
    retryable = cls == "infra" and attempt < MAX_ATTEMPTS
    print(f"run attempt {attempt}: {cls}; retryable={str(retryable).lower()}")
    if args.github_output:
        with open(args.github_output, "a") as out:
            out.write(f"class={cls}\nretryable={str(retryable).lower()}\n")
    return 0


def cmd_collect_results(ci, args):
    plan = ci.load_plan(args.plan)
    out = Path(args.output)
    for worker in range(len(plan["workers"])):
        for attempt in range(int(args.run_attempt), 0, -1):
            src = Path(args.artifacts) / artifact_name(worker, attempt)
            if src.is_dir():
                # A job not rerun keeps its earlier attempt; the audit still checks every file's identity.
                shutil.copytree(src, out / f"worker-{worker}")
                print(f"worker {worker}: attempt {attempt}")
                break
        else:
            print(f"worker {worker}: no artifact up to attempt {args.run_attempt}")
    return 0


def load_selectors(path):
    """(section, selector) pairs; a leading = marks an exact test ID."""
    section, rows = None, []
    for raw in Path(path).read_text().splitlines():
        line = raw.strip()
        if line.startswith("[") and line.endswith("]"):
            section = line[1:-1]
        elif line and not line.startswith("#"):
            rows.append((section, line))
    return rows


def cmd_selector_audit(ci, args):
    plan = ci.load_plan(args.plan)
    names = [name for w in plan["workers"] for _, name in w["tests"]]
    rows = load_selectors(args.selectors)
    missing = [(section, sel) for section, sel in rows
               if not any(name == sel[1:] if sel.startswith("=") else sel in name for name in names)]
    for section, sel in missing:
        print(f"::error::targeted selector {sel!r} ({section}) matches no planned test")
    print(f"{len(rows) - len(missing)}/{len(rows)} targeted selectors match planned tests")
    return 1 if missing or not rows else 0


def register(sub):
    """Add the subcommands to nextest_ci's parser; returns name -> handler(ci, args)."""
    r = sub.add_parser("required-context")
    r.add_argument("name", help="exact branch-protection context name")
    r.add_argument("--needs-json", help="JSON text or @file (default: $NEEDS_JSON)")
    c = sub.add_parser("comparison-base")
    c.add_argument("--repo", default=".")
    c.add_argument("--head", default="HEAD", help="the commit under test")
    c.add_argument("--pr-head-sha", required=True)
    c.add_argument("--base-ref", default="origin/main")
    c.add_argument("--baseline", help="plan.json that must belong to the comparison base and --scope")
    c.add_argument("--scope")
    c.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT"))
    w = sub.add_parser("classify-worker")
    w.add_argument("--plan", required=True)
    w.add_argument("--artifacts", required=True, help=f"directory holding {artifact_name('<i>', '<attempt>')}/")
    w.add_argument("--worker", type=int, action="append", required=True, help="a failed worker (repeatable)")
    w.add_argument("--infra-evidence", type=int, action="append",
                   help="a worker whose job log the caller's classifier proved infra (repeatable)")
    w.add_argument("--run-attempt", default=os.environ.get("GITHUB_RUN_ATTEMPT", "1"))
    w.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT"))
    o = sub.add_parser("collect-results")
    o.add_argument("--plan", required=True)
    o.add_argument("--artifacts", required=True)
    o.add_argument("--run-attempt", default=os.environ.get("GITHUB_RUN_ATTEMPT", "1"))
    o.add_argument("--output", required=True)
    t = sub.add_parser("selector-audit")
    t.add_argument("--plan", required=True)
    t.add_argument("--selectors", default="scripts/ci/targeted-test-selectors.txt")
    return {"required-context": cmd_required_context, "comparison-base": cmd_comparison_base,
            "classify-worker": cmd_classify_worker, "collect-results": cmd_collect_results,
            "selector-audit": cmd_selector_audit}
