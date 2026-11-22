#!/usr/bin/env python3
"""Measure Rust test failures in CI PR and CI Main, including landing PRs.

Requires authenticated gh. JSON includes coverage gaps as well as observations.
Re-use --cache and --until to resume a snapshot without re-fetching evidence.
"""

import argparse
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
import datetime as dt
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
import threading
import zipfile

import nextest_ci


WORKFLOWS = ("ci-pr.yml", "ci-main.yml")
PAGE_SIZE = 100
PAGE_CAP = 1000
WORKER = re.compile(r"nextest-worker-(\d+)-attempt-(\d+)$")
JOB_WORKER = re.compile(r"Rust tests \(worker (\d+)\)$")
RUST_JOB = re.compile(
    r"^(?:Rust tests \(worker |Library tests? (?:sweep|\()|(?:Fast|Relay authority) targeted tests"
    r"|Fast check(?: \(ubuntu-latest\)| \+ non-PG tests)|Windows exact targets|PostgreSQL (?:15 owner proof|tests)"
    r"|High-risk recovery|Relay authority targets|Main lint and non-lib tests"
    r"|Main (?:full|non-PG|PostgreSQL|Rust) tests|Full tests|Test suite)", re.I,
)
BAD = {"failure", "timed_out", "cancelled", "startup_failure", "action_required"}


class FetchError(Exception):
    pass


def timestamp(value):
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))


def iso(value):
    return value.astimezone(dt.timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


class GitHub:
    def __init__(self, repo, cache, limit):
        self.root = f"repos/{repo}/actions"
        self.cache = cache
        self.cache.mkdir(parents=True, exist_ok=True)
        self.limit = limit
        self.calls = 0
        self.hits = 0
        self.lock = threading.Lock()
        self.stopped = None

    def get(self, endpoint, *, snapshot=""):
        url = f"{self.root}/{endpoint}"
        key = hashlib.sha256(f"{url}\n{snapshot}".encode()).hexdigest()
        path = self.cache / f"{key}.gz"
        try:
            contents = gzip.decompress(path.read_bytes())
        except (FileNotFoundError, EOFError, OSError):
            pass
        else:
            with self.lock:
                self.hits += 1
            return contents
        with self.lock:
            if self.stopped:
                raise FetchError(self.stopped)
            if self.calls >= self.limit:
                raise FetchError("request budget exhausted")
            self.calls += 1
        try:
            flags = ["--allow-escape-sequences"] if endpoint.endswith("/logs") else []
            result = subprocess.run(["gh", "api", *flags, url], capture_output=True, timeout=90)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise FetchError(f"gh {type(exc).__name__}") from exc
        if result.returncode:
            problem = result.stderr.decode(errors="replace")
            if re.search(r"rate limit|HTTP 429|HTTP 401|not logged|authentication", problem, re.I):
                with self.lock:
                    self.stopped = "GitHub authentication or rate limit"
                raise FetchError(self.stopped)
            status = re.search(r"HTTP (\d+)", problem)
            raise FetchError(f"HTTP {status[1]}" if status else f"gh exit {result.returncode}")
        with tempfile.NamedTemporaryFile(dir=self.cache, delete=False) as tmp:
            tmp.write(gzip.compress(result.stdout, compresslevel=3))
            tmp_path = Path(tmp.name)
        tmp_path.replace(path)
        return result.stdout

    def data(self, endpoint, **kwargs):
        try:
            return json.loads(self.get(endpoint, **kwargs))
        except (ValueError, UnicodeError) as exc:
            raise FetchError("invalid GitHub JSON") from exc

    def first_page(self, endpoint, page=1, **kwargs):
        join = "&" if "?" in endpoint else "?"
        return self.data(f"{endpoint}{join}per_page={PAGE_SIZE}&page={page}", **kwargs)

    def pages(self, endpoint, key, first, **kwargs):
        yield from first[key]
        for page in range(2, (first["total_count"] + PAGE_SIZE - 1) // PAGE_SIZE + 1):
            yield from self.first_page(endpoint, page, **kwargs)[key]


def workflow_runs(gh, workflow, start, end, snapshot):
    endpoint = f"workflows/{workflow}/runs?created={iso(start)}..{iso(end)}"
    first = gh.first_page(endpoint, snapshot=snapshot)
    if first["total_count"] >= PAGE_CAP:
        middle = (start + (end - start) / 2).replace(microsecond=0)
        if middle <= start:
            raise FetchError("workflow listing reached GitHub's 1,000-result cap")
        yield from workflow_runs(gh, workflow, start, middle, snapshot)
        yield from workflow_runs(gh, workflow, middle, end, snapshot)
        return
    yield from gh.pages(endpoint, "workflow_runs", first, snapshot=snapshot)


def get_runs(gh, start, end):
    rows = {}
    for workflow in WORKFLOWS:
        for run in workflow_runs(gh, workflow, start, end, iso(end)):
            if not start <= timestamp(run["created_at"]) < end:
                continue
            branch, event = run["head_branch"], run["event"]
            if workflow == "ci-main.yml" and (branch != "main" or event not in {"push", "workflow_dispatch"}):
                continue
            prs = run.get("pull_requests", [])
            rows[run["id"]] = {
                "id": run["id"], "head": run["head_sha"], "branch": branch,
                "workflow": workflow, "event": event, "created_at": run["created_at"],
                "conclusion": run["conclusion"], "status": run["status"],
                "attempt": run["run_attempt"], "landing": (branch or "").startswith(("land/", "land-prep/", "batch/land-")),
                "pr": prs[0]["number"] if len(prs) == 1 else None,
                "base_sha": prs[0]["base"].get("sha") if len(prs) == 1 else None,
            }
    return sorted(rows.values(), key=lambda row: (row["created_at"], row["id"]), reverse=True)


def get_jobs(gh, run, attempt, snapshot):
    endpoint = f"runs/{run['id']}/attempts/{attempt}/jobs"
    kwargs = {"snapshot": snapshot} if run["status"] != "completed" else {}
    try:
        first = gh.first_page(endpoint, **kwargs)
        jobs = [{k: job[k] for k in ("id", "head_sha", "name", "conclusion", "started_at", "completed_at", "steps")}
                for job in gh.pages(endpoint, "jobs", first, **kwargs)]
    except FetchError as exc:
        return [], [f"jobs {run['id']}/{attempt}: {exc}"]
    for job in jobs:
        job.update(run_id=run["id"], attempt=attempt)
    return jobs, []


def inventory(gh, start, end, parallel):
    runs = get_runs(gh, start, end)
    tasks = [(r, n) for r in runs if r["status"] == "completed" for n in range(1, r["attempt"] + 1)]
    tasks.sort(key=lambda task: task[0]["conclusion"] not in BAD and task[0]["attempt"] == 1)
    jobs, issues = [], []
    with ThreadPoolExecutor(max_workers=parallel) as pool:
        for rows, errors in pool.map(lambda task: get_jobs(gh, *task, iso(end)), tasks):
            jobs.extend(rows)
            issues.extend(errors)
    return {"start": iso(start), "end": iso(end), "runs": runs, "jobs": jobs, "issues": issues}


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as tmp:
        json.dump(value, tmp, separators=(",", ":"), ensure_ascii=False, allow_nan=False)
        tmp.write("\n")
        tmp_path = Path(tmp.name)
    tmp_path.replace(path)


ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
LOG_TIME = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z\s*")
ID = r"[A-Za-z0-9_:.\-]+"
NEXTEST = re.compile(
    rf"(?:(?:TRY (\d+) )?(PASS|FAIL|TIMEOUT|LEAK|ABORT|SIG[A-Z0-9]+|EXECFAIL)|FLAKY (\d+)/(\d+))"
    rf"\s+\[\s*(\d+(?:\.\d+)?)s\]\s+(?:\([^)]*\)\s+)?({ID})\s+({ID})"
)
LIBTEST = re.compile(r"test (\S+|src/\S+ - .+ \(line \d+\))(?: - should panic)? \.\.\. (ok|FAILED|ignored)(?:, .*)?")
HARNESS = re.compile(r"Running (?:unittests )?([^ ]+\.rs) \(([^)]+)\)")
BINARY_HASH = re.compile(r"-[0-9a-f]{8,}(?:\.exe)?$")
LIB_SUMMARY = re.compile(r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed;")
NEXT_SUMMARY = re.compile(r"Summary \[[^]]+\]\s+\d+ tests? run: (\d+) passed(?: \([^)]*\))?(?:, (\d+) failed)?(?:, \d+ skipped)?$")
STACK_OVERFLOW = re.compile(r"thread '([\w:]+::[\w:]+)' (?:\(\d+\) )?has overflowed its stack")
CHECKOUT_SHA = re.compile(r"[0-9a-f]{40}")
AUDIT_HEADER = re.compile(r"SHA `([0-9a-f]{40})` .*?executed (\d+)/(\d+) · failed (\d+) · retry-pass (\d+) · retry mode \S+")
AUDIT_RETRY = re.compile(rf"\| `({ID}) ({ID})` \| \d+ \| [\d.]+ \| ([\d.]+) \| .*")
AUDIT_FAILURE = re.compile(rf"- worker (\d+) `(?:failed|retry-exhausted)` ({ID}) ({ID}) .*")


def log_lines(raw):
    for line in raw.decode(errors="replace").splitlines():
        yield LOG_TIME.sub("", ANSI.sub("", line)).strip()


def rust_log(raw):
    cases, observed, failed, statuses = {}, 0, 0, defaultdict(dict)
    binary = "agentdesk"
    sha, checkout_next = None, False
    for line in log_lines(raw):
        if checkout_next and CHECKOUT_SHA.fullmatch(line):
            sha = line
        checkout_next = sha is None and line.endswith("git log -1 --format=%H")
        if match := NEXTEST.fullmatch(line):
            retry, status, flaky_try, _, seconds, crate, name = match.groups()
            n = int(retry or flaky_try or 1)
            statuses[crate, name][n] = ("passed" if flaky_try or status == "PASS" else "failed", float(seconds))
        elif match := HARNESS.search(line):
            src, executable = match.groups()
            raw_name = re.split(r"[/\\]", executable)[-1]
            binary = "agentdesk" if src == "src/lib.rs" else BINARY_HASH.sub("", raw_name).removesuffix(".exe")
        elif line.startswith("Doc-tests "):
            binary = line.removeprefix("Doc-tests ")
        elif match := LIBTEST.fullmatch(line):
            name, outcome = match.groups()
            key = binary, name
            if outcome == "FAILED" or (outcome == "ok" and key not in cases):
                cases[key] = ("failed" if outcome == "FAILED" else "passed", None)
        elif match := STACK_OVERFLOW.fullmatch(line):
            cases[binary, match[1]] = ("failed", None)
        elif match := LIB_SUMMARY.match(line):
            observed += int(match[1]) + int(match[2])
            failed += int(match[2])
        elif match := NEXT_SUMMARY.fullmatch(line):
            observed += int(match[1]) + int(match[2] or 0)
            failed += int(match[2] or 0)
    for key, attempts in statuses.items():
        n = max(attempts)
        outcome, seconds = attempts[n]
        if n > 1 and outcome == "passed":
            outcome = "retry-pass"
        total = sum(span for _, span in attempts.values()) if len(attempts) == n else None
        cases[key] = outcome, seconds if n == 1 else total
    named_failed = sum(outcome == "failed" for outcome, _ in cases.values())
    return cases, sha, observed > 0 and failed == named_failed


def read_junit(raw, job, worker):
    try:
        with zipfile.ZipFile(io.BytesIO(raw)) as bundle:
            identity = json.loads(bundle.read("worker.json"))
            root = ET.fromstring(bundle.read("junit.xml"))
        expected = (job["run_id"], job["attempt"], worker)
        actual = tuple(int(identity[k]) for k in ("run_id", "run_attempt", "worker"))
        sha = identity["sha"]
        if actual != expected or root.tag != "testsuites" or not CHECKOUT_SHA.fullmatch(sha):
            raise ValueError("wrong worker or attempt")
        cases = {}
        seen = set()
        for binary, case in nextest_ci._cases(root):
            name = case.get("name")
            if not binary or not name or (binary, name) in seen:
                raise ValueError("invalid or duplicate JUnit identity")
            seen.add((binary, name))
            outcome, tries = nextest_ci.classify(case)
            if outcome != "skipped":
                _, seconds = nextest_ci.case_seconds(case)
                cases[binary, name] = ("retry-pass" if outcome == "passed" and tries > 1 else outcome, seconds)
        if not cases or int(root.get("tests", len(seen))) != len(seen):
            raise ValueError("empty or inconsistent JUnit report")
        return cases, sha
    except (ValueError, KeyError, TypeError, OSError, zipfile.BadZipFile, ET.ParseError) as exc:
        raise FetchError(f"unusable JUnit ({type(exc).__name__})") from exc


def live_artifacts(gh, run, snapshot):
    endpoint = f"runs/{run['id']}/artifacts"
    try:
        first = gh.first_page(endpoint, snapshot=snapshot)
        artifacts = {tuple(map(int, match.groups())): a["id"]
                     for a in gh.pages(endpoint, "artifacts", first, snapshot=snapshot)
                     if not a["expired"] and (match := WORKER.fullmatch(a["name"]))}
        return artifacts, []
    except FetchError as exc:
        return {}, [f"artifacts {run['id']}: {exc}"]


def job_evidence(gh, job, artifacts):
    errors = []
    if match := JOB_WORKER.fullmatch(job["name"]):
        worker = int(match[1])
        if artifact := artifacts.get((worker, job["attempt"])):
            try:
                cases, sha = read_junit(gh.get(f"artifacts/{artifact}/zip"), job, worker)
                return job, cases, "junit", sha, True, errors
            except FetchError as exc:
                errors.append(f"artifact {artifact}: {exc}")
    try:
        cases, sha, summary = rust_log(gh.get(f"jobs/{job['id']}/logs"))
    except FetchError as exc:
        errors.append(f"logs {job['id']}: {exc}")
        return job, {}, "missing", None, False, errors
    return job, cases, "log", sha, summary, errors


def audit_evidence(gh, job):
    try:
        raw = gh.get(f"jobs/{job['id']}/logs")
    except FetchError as exc:
        return job, None, [], [f"audit {job['id']}: {exc}"]
    header, cases = None, []
    for line in log_lines(raw):
        if match := AUDIT_HEADER.fullmatch(line):
            header = (match[1], *map(int, match.groups()[1:]))
        elif match := AUDIT_RETRY.fullmatch(line):
            cases.append((None, match[1], match[2], "retry-pass", float(match[3])))
        elif match := AUDIT_FAILURE.fullmatch(line):
            cases.append((int(match[1]), match[2], match[3], "failed", None))
    return job, header, cases, []


def mirror(job):
    steps = [step["name"].lower() for step in job["steps"]]
    return not steps or any("required-check-mirror" in name for name in steps)


def evidence(gh, data, timing_runs, parallel):
    runs = {r["id"]: r for r in data["runs"]}
    rust_jobs = [j for j in data["jobs"] if RUST_JOB.match(j["name"]) and not mirror(j)]
    bad_jobs = [j for j in rust_jobs if j["conclusion"] in BAD]
    audits = [j for j in data["jobs"] if j["name"] == "Rust tests evidence"
              and j["conclusion"] in {"success", "failure"}]
    selected = defaultdict(set)
    for job in rust_jobs:
        if JOB_WORKER.fullmatch(job["name"]) and job["conclusion"] == "success":
            selected[job["run_id"], job["attempt"]].add(job["name"])
    recent = timestamp(data["end"]) - dt.timedelta(days=7)
    timed = set()
    for group in ("ci-main.yml", "landing", "pr"):
        candidates = [r for r in runs.values() if r["conclusion"] == "success" and timestamp(r["created_at"]) >= recent
                      and len(selected[r["id"], r["attempt"]]) == 6
                      and (r["workflow"] if r["workflow"] == "ci-main.yml" else "landing" if r["landing"] else "pr") == group]
        timed.update(r["id"] for r in candidates[:timing_runs])
    rerun_ids = {j["run_id"] for j in bad_jobs if j["attempt"] < runs[j["run_id"]]["attempt"]}
    possible = timed | rerun_ids | {j["run_id"] for j in bad_jobs if JOB_WORKER.fullmatch(j["name"])}
    artifact_runs = [r for r in runs.values() if r["id"] in possible and timestamp(r["created_at"]) >= recent]
    data.update(observations=[], coverage=[], audit_coverage=[], timing_run_ids=sorted(timed))
    observations, coverage, issues = data["observations"], data["coverage"], data["issues"]
    failed_by_run = defaultdict(set)
    artifact_map = {}

    def collect(results, retain):
        for job, cases, source, sha, complete, errors in results:
            issues.extend(errors)
            rid = job["run_id"]
            coverage.append({"job": job["id"], "source": source, "cases": len(cases), "summary_complete": complete})
            for (binary, name), (status, seconds) in sorted(cases.items()):
                if status == "failed":
                    failed_by_run[rid].add((binary, name))
                if status != "passed" or retain(job, binary, name):
                    observations.append({"run_id": rid, "attempt": job["attempt"], "job": job["id"],
                                         "binary": binary, "test": name, "status": status,
                                         "seconds": seconds, "source": source, "tested_sha": sha})

    with ThreadPoolExecutor(max_workers=parallel) as pool:
        for run, (items, errors) in zip(artifact_runs, pool.map(lambda r: live_artifacts(gh, r, data["end"]), artifact_runs)):
            artifact_map[run["id"]] = items
            issues.extend(errors)
        fetch_job = lambda j: job_evidence(gh, j, artifact_map.get(j["run_id"], {}))
        collect(pool.map(fetch_job, sorted(bad_jobs, key=lambda j: j["attempt"])),
                lambda j, b, n: j["attempt"] > 1 and (b, n) in failed_by_run[j["run_id"]])
        # A rerun may reshard nextest tests. Read its executed workers, not its overall conclusion.
        previous = defaultdict(list)
        for job in bad_jobs:
            if failed_by_run[job["run_id"]]:
                previous[job["run_id"]].append(job)
        later = [j for j in rust_jobs if j["conclusion"] == "success" and
                 (any(j["attempt"] > before["attempt"] and
                      (j["name"] == before["name"] or
                       (JOB_WORKER.fullmatch(j["name"]) and JOB_WORKER.fullmatch(before["name"])))
                      for before in previous[j["run_id"]])
                  or (j["run_id"] in timed and j["attempt"] == runs[j["run_id"]]["attempt"]
                      and JOB_WORKER.fullmatch(j["name"])))]
        collect(pool.map(fetch_job, later), lambda j, b, n: j["run_id"] in timed or (b, n) in failed_by_run[j["run_id"]])

        worker_jobs = {(j["run_id"], j["attempt"], int(match[1])): j["id"] for j in rust_jobs
                       if (match := JOB_WORKER.fullmatch(j["name"]))}
        existing = {(o["run_id"], o["attempt"], o["job"], o["binary"], o["test"], o["status"]) for o in observations}
        retries = {(o["run_id"], o["binary"], o["test"]) for o in observations if o["status"] == "retry-pass"}
        for job, header, cases, errors in pool.map(lambda j: audit_evidence(gh, j), audits):
            issues.extend(errors)
            rid, attempt = job["run_id"], job["attempt"]
            named = {(w, b, n, s) for w, b, n, s, _ in cases}
            data["audit_coverage"].append({"job": job["id"], "summary": header,
                                           "named_failures": sum(s == "failed" for _, _, _, s in named),
                                           "named_retry_passes": sum(s == "retry-pass" for _, _, _, s in named)})
            if header is None:
                continue
            sha = header[0]
            for worker, binary, name, status, seconds in cases:
                job_id = worker_jobs.get((rid, attempt, worker))
                if status == "failed" and (not job_id or (rid, attempt, job_id, binary, name, status) in existing):
                    continue
                if status == "retry-pass":
                    if (rid, binary, name) in retries:
                        continue
                    retries.add((rid, binary, name))
                observations.append({"run_id": rid, "attempt": attempt, "job": job_id or job["id"],
                                     "binary": binary, "test": name, "status": status,
                                     "seconds": seconds, "source": "audit", "tested_sha": sha})
    data["rust_jobs"] = [j["id"] for j in rust_jobs]
    return data


def stats(data):
    runs = {r["id"]: r for r in data["runs"]}
    jobs = {j["id"]: j for j in data["jobs"]}
    groups = defaultdict(list)
    for event in data["observations"]:
        groups[event["binary"], event["test"]].append(event)
    rows = []
    for (binary, name), events in groups.items():
        failed = [e for e in events if e["status"] == "failed"]
        previous = defaultdict(list)
        for event in failed:
            previous[event["run_id"]].append(event)
        reruns, recovered = {}, set()
        for event in sorted(events, key=lambda e: (e["run_id"], e["attempt"], e["job"])):
            if event["status"] == "failed" or event["source"] == "audit":
                continue
            now = jobs[event["job"]]["name"]
            for before in previous[event["run_id"]]:
                old = jobs[before["job"]]["name"]
                old_key = (before["run_id"], before["attempt"], before["job"])
                if old_key in recovered or before["attempt"] >= event["attempt"] or not (old == now or
                        (JOB_WORKER.fullmatch(old) and JOB_WORKER.fullmatch(now))):
                    continue
                old_sha, new_sha = before["tested_sha"], event["tested_sha"]
                if old_sha and new_sha and old_sha != new_sha:
                    continue
                key = (event["run_id"], event["attempt"], now if not JOB_WORKER.fullmatch(now) else "nextest")
                reruns[key] = reruns.get(key, False) or bool(old_sha and new_sha)
                recovered.add(old_key)
        spans = [e["seconds"] for e in events if e["seconds"] is not None]
        passed_junit = [e["seconds"] for e in events if e["seconds"] is not None
                        and e["source"] == "junit" and e["status"] == "passed"]
        rows.append({"binary": binary, "test": name, "failures": len(failed),
                     "main_failures": sum(runs[e["run_id"]]["workflow"] == "ci-main.yml" for e in failed),
                     "rerun_passes": len(reruns), "rerun_passes_same_tested_sha": sum(reruns.values()),
                     "in_job_retry_passes": sum(e["status"] == "retry-pass" for e in events),
                     "main_in_job_retry_passes": sum(e["status"] == "retry-pass" and
                                                   runs[e["run_id"]]["workflow"] == "ci-main.yml" for e in events),
                     "timing_samples": len(spans), "mean_seconds": sum(spans) / len(spans) if spans else None,
                     "max_seconds": max(spans, default=None), "passed_junit_samples": len(passed_junit),
                     "mean_passed_junit_seconds": sum(passed_junit) / len(passed_junit) if passed_junit else None})
    return sorted(rows, key=lambda r: (-r["failures"], -r["in_job_retry_passes"], r["binary"], r["test"]))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="itismyfield/AgentDesk", help="GitHub owner/repo")
    parser.add_argument("--days", type=int, default=30)
    parser.add_argument("--until", help="exclusive ISO 8601 end with timezone, defaults to now")
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--max-requests", type=int, default=4000, help="uncached gh API calls")
    parser.add_argument("--parallel", type=int, default=8)
    parser.add_argument("--timing-runs", type=int, default=3, help="recent successful six-worker runs per cohort")
    args = parser.parse_args(argv)
    if args.days < 1 or args.max_requests < 0 or not 1 <= args.parallel <= 12 or args.timing_runs < 0:
        parser.error("use positive days, zero or more requests and timing runs, and 1–12 concurrent calls")
    try:
        end = timestamp(args.until) if args.until else dt.datetime.now(dt.timezone.utc)
        if end.tzinfo is None:
            raise ValueError
        end = end.replace(microsecond=0)
    except ValueError:
        parser.error("--until needs an ISO 8601 timestamp with timezone")
    gh = GitHub(args.repo, args.cache, args.max_requests)
    try:
        result = inventory(gh, end - dt.timedelta(days=args.days), end, args.parallel)
        evidence(gh, result, args.timing_runs, args.parallel)
    except FetchError as exc:
        print(f"workflow inventory incomplete: {exc}", file=sys.stderr)
        return 2
    result["tests"] = stats(result)
    coverage = result["coverage"]
    def audit_gap(row):
        summary = row["summary"]
        return not summary or summary[1] != summary[2] or summary[3:] != (row["named_failures"], row["named_retry_passes"])

    incomplete = (not coverage or bool(result["issues"]) or any(not row["summary_complete"] for row in coverage)
                  or any(map(audit_gap, result["audit_coverage"])))
    result["collection"] = {"requests": gh.calls, "cache_hits": gh.hits, "partial": incomplete}
    write_json(args.output, result)
    print(f"runs={len(result['runs'])} jobs_with_evidence={len(coverage)} "
          f"failing_tests={sum(row['failures'] > 0 or row['in_job_retry_passes'] > 0 for row in result['tests'])} "
          f"rerun_passes={sum(row['rerun_passes'] for row in result['tests'])} "
          f"requests={gh.calls} cache_hits={gh.hits} coverage={'partial' if incomplete else 'sampled'}")
    return 2 if not coverage or gh.stopped or gh.calls >= gh.limit and result["issues"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
