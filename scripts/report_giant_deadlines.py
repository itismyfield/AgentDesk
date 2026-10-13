#!/usr/bin/env python3
"""Read-only report of registered giant-file deadlines and metadata freshness.

Reads only ``scripts/giant_file_registry.toml`` and
``scripts/giant_file_issue_metadata.json``, classifies registered shrink
deadlines against a single UTC observation time, and writes ``report.json`` and
``report.md`` to a run directory outside the checkout. The report is an
observation, not a gate result: it never runs the inventory generator, the
progress evaluator or the metadata refresher, and never writes repository files.

Exit codes: 0 complete report (overdue and stale are warnings, not failures);
2 partial report (malformed or missing input); 1 unusable run context or output,
with no report left behind.
"""

from __future__ import annotations

import argparse
import hashlib
import html
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from collections import Counter
from dataclasses import dataclass
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

REGISTRY_PATH = "scripts/giant_file_registry.toml"
METADATA_PATH = "scripts/giant_file_issue_metadata.json"
REPORT_SCHEMA_VERSION = 1
METADATA_SCHEMA_VERSION = 2
URGENT_DAYS = 14
HORIZON_DAYS = 60
FRESHNESS_WINDOW = timedelta(days=30)
STAMP_FORMAT = "%Y-%m-%dT%H:%M:%SZ"
ENTRY_FIELDS = frozenset({"file", "decision", "owner", "deadline", "decompose_issue", "keep_reason"})
PLACEHOLDERS = frozenset({"tbd", "unknown", "none", "umbrella"})
SELF_REFERENCE_ISSUE = "#4519"
DEADLINE_RE = re.compile(r"\d{4}-\d{2}-\d{2}")
ISSUE_RE = re.compile(r"#([1-9]\d*)")
REPORT_FILES = ("report.md", "report.json")
SCOPE_NOTE = (
    "Registered deadlines and the checked-in issue snapshot only. Not a gate "
    "result, not a measured giant-file scan, and not live GitHub state."
)


class ContextError(Exception):
    """The run context cannot back a report (wrong checkout or unsafe output)."""


@dataclass(frozen=True)
class InputFile:
    path: str
    data: bytes | None
    error: str | None

    @property
    def sha256(self) -> str | None:
        return None if self.data is None else hashlib.sha256(self.data).hexdigest()


def read_inputs(root: Path) -> dict[str, InputFile]:
    inputs: dict[str, InputFile] = {}
    for key, rel in (("registry", REGISTRY_PATH), ("metadata", METADATA_PATH)):
        try:
            inputs[key] = InputFile(rel, (root / rel).read_bytes(), None)
        except FileNotFoundError:
            inputs[key] = InputFile(rel, None, "missing")
        except OSError as error:
            inputs[key] = InputFile(rel, None, f"unreadable ({error.strerror or type(error).__name__})")
    return inputs


def classify(days: int) -> str | None:
    if days < 0:
        return "OVERDUE"
    if days <= URGENT_DAYS:
        return "URGENT"
    if days <= HORIZON_DAYS:
        return "HORIZON"
    return None


def _is_real(value: object) -> bool:
    return isinstance(value, str) and bool(value.strip()) and value.strip().lower() not in PLACEHOLDERS


def _parse_deadline(value: object) -> date | None:
    if not isinstance(value, str) or not DEADLINE_RE.fullmatch(value):
        return None
    try:
        return date.fromisoformat(value)
    except ValueError:
        return None


def _entry_problems(entry: dict) -> list[str]:
    problems = []
    if not isinstance(entry.get("file"), str) or not entry["file"].strip():
        problems.append("missing or non-string `file`")
    wrong_type = sorted(key for key in ENTRY_FIELDS - {"file"} if key in entry and not isinstance(entry[key], str))
    if wrong_type:
        return problems + [f"field(s) must be strings: {', '.join(wrong_type)}"]
    # Registrations that predate explicit decisions keep their shrink contract.
    decision = entry.get("decision", "shrink")
    owner = entry.get("owner", "")
    if decision == "shrink":
        if not _is_real(owner):
            problems.append("shrink requires a real owner")
        if _parse_deadline(entry.get("deadline")) is None:
            problems.append(f"shrink deadline {entry.get('deadline')!r} is not a calendar-valid YYYY-MM-DD")
        issue = entry.get("decompose_issue", "").strip()
        if not ISSUE_RE.fullmatch(issue) or issue == SELF_REFERENCE_ISSUE:
            problems.append(f"shrink decompose_issue {issue!r} is not a real numeric issue")
        if entry.get("keep_reason"):
            problems.append("shrink forbids keep_reason")
    elif decision == "keep":
        if not _is_real(owner):
            problems.append("keep requires a real owner")
        if not _is_real(entry.get("keep_reason")):
            problems.append("keep requires a substantive keep_reason")
        for field in ("deadline", "decompose_issue"):
            if entry.get(field):
                problems.append(f"keep forbids {field}")
    else:
        problems.append(f"unknown decision {decision!r}")
    return problems


def _registry_population(source: InputFile, diagnostics: list[dict]) -> dict | None:
    """Validated shrink registrations and exclusion counts, or None when unreadable."""

    def problem(path: str | None, text: str) -> None:
        diagnostics.append({"input": "registry", "path": path, "problem": text})

    if source.data is None:
        problem(None, f"{source.path} is {source.error}")
        return None
    try:
        document = tomllib.loads(source.data.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        problem(None, f"registry does not parse: {error}")
        return None
    entries = document.get("entry", [])
    grandfathered = document.get("grandfathered", [])
    if not isinstance(entries, list) or not all(isinstance(entry, dict) for entry in entries):
        problem(None, "registry `entry` must be an array of tables")
        return None
    if not isinstance(grandfathered, list) or not all(isinstance(path, str) for path in grandfathered):
        problem(None, "registry `grandfathered` must be an array of path strings")
        return None

    occurrences = Counter(entry["file"] for entry in entries if isinstance(entry.get("file"), str))
    occurrences.update(grandfathered)
    population = {"shrink": [], "keep": 0, "invalid": 0, "untracked": []}
    # Every registration is validated here, before any horizon filtering.
    for index, entry in enumerate(entries):
        path = entry.get("file") if isinstance(entry.get("file"), str) else None
        label = path or f"entry[{index}]"
        unknown = sorted(set(entry) - ENTRY_FIELDS)
        if unknown:
            problem(label, f"unknown field(s) not interpreted: {', '.join(unknown)}")
        problems = _entry_problems(entry)
        if path is not None and occurrences[path] > 1:
            problems.append("path is registered more than once; no occurrence is trusted")
        if problems:
            population["invalid"] += 1
            for text in problems:
                problem(label, text)
        elif entry.get("decision", "shrink") == "keep":
            population["keep"] += 1
        else:
            population["shrink"].append(entry)
    for path in grandfathered:
        if occurrences[path] > 1:
            population["invalid"] += 1
            problem(path, "path is registered more than once; no occurrence is trusted")
            continue
        population["untracked"].append(path)
        problem(path, "grandfathered registration has no decision or deadline; deadline tracking does not cover it")
    return population


class _JsonObject(dict):
    duplicates: tuple[str, ...] = ()


def _json_object(pairs: list[tuple[str, object]]) -> _JsonObject:
    result = _JsonObject()
    duplicates = []
    for key, value in pairs:
        if key in result:
            duplicates.append(key)
        result[key] = value
    result.duplicates = tuple(duplicates)
    return result


def _metadata_view(source: InputFile, now: datetime, diagnostics: list[dict]) -> tuple[dict, dict[int, str]]:
    """Snapshot stamp freshness and the snapshot state of each unambiguous issue."""

    def problem(text: str) -> None:
        diagnostics.append({"input": "metadata", "path": None, "problem": text})

    view = {"freshness": "INVALID", "refreshed_at": None, "fresh_through": None,
            "age_seconds": None, "remaining_seconds": None}
    if source.data is None:
        problem(f"{source.path} is {source.error}")
        return {**view, "freshness": "MISSING"}, {}
    try:
        payload = json.loads(source.data.decode("utf-8"), object_pairs_hook=_json_object)
    except (UnicodeDecodeError, ValueError) as error:
        problem(f"metadata does not parse: {error}")
        return view, {}
    if not isinstance(payload, dict):
        problem("metadata must be a JSON object")
        return view, {}
    if payload.duplicates:
        problem(f"metadata repeats top-level key(s): {', '.join(sorted(set(payload.duplicates)))}")
        return view, {}
    version = payload.get("schema_version")
    if type(version) is not int or version != METADATA_SCHEMA_VERSION:
        problem(f"unsupported metadata schema_version {version!r}")
        return view, {}

    stamp = payload.get("refreshed_at")
    try:
        refreshed = datetime.strptime(stamp, STAMP_FORMAT).replace(tzinfo=timezone.utc)
    except (TypeError, ValueError):
        problem(f"refreshed_at {stamp!r} is not a YYYY-MM-DDTHH:MM:SSZ timestamp")
    else:
        age = now - refreshed
        fresh_through = refreshed + FRESHNESS_WINDOW
        view.update(refreshed_at=stamp, fresh_through=fresh_through.strftime(STAMP_FORMAT),
                    age_seconds=age.total_seconds(), remaining_seconds=(fresh_through - now).total_seconds())
        if age < timedelta(0):
            problem(f"refreshed_at {stamp!r} is in the future")
        else:
            view["freshness"] = "FRESH" if age <= FRESHNESS_WINDOW else "STALE"

    states: dict[int, str] = {}
    conflicted: set[int] = set()
    records = payload.get("issues")
    if not isinstance(records, list):
        problem("metadata `issues` must be a list")
        records = []
    for index, record in enumerate(records):
        number = record.get("number") if isinstance(record, dict) else None
        state = record.get("state") if isinstance(record, dict) else None
        if type(number) is not int or number <= 0:
            problem(f"issue record {index} has no positive integer number")
            continue
        if record.duplicates or state not in {"open", "closed"}:
            problem(f"issue #{number} record has repeated keys or an unknown state")
            conflicted.add(number)
        elif number in states or number in conflicted:
            problem(f"issue #{number} is recorded more than once")
            conflicted.add(number)
        else:
            states[number] = state.upper()
    for number in conflicted:
        states.pop(number, None)
    return view, states


def build_report(inputs: dict[str, InputFile], *, now: datetime, provenance: dict) -> dict:
    if now.tzinfo is None:
        raise ValueError("now must be timezone-aware")
    now = now.astimezone(timezone.utc)
    today = now.date()
    diagnostics: list[dict] = []
    population = _registry_population(inputs["registry"], diagnostics)
    metadata, issue_states = _metadata_view(inputs["metadata"], now, diagnostics)

    rows, counts, untracked = [], None, []
    if population is not None:
        counts = {"overdue": 0, "urgent": 0, "horizon": 0, "beyond_horizon": 0,
                  "keep": population["keep"], "invalid": population["invalid"],
                  "untracked": len(population["untracked"])}
        untracked = sorted(population["untracked"])
        for entry in population["shrink"]:
            deadline = _parse_deadline(entry["deadline"])
            days = (deadline - today).days
            severity = classify(days)
            if severity is None:
                counts["beyond_horizon"] += 1
                continue
            counts[severity.lower()] += 1
            issue = entry["decompose_issue"].strip()
            rows.append({"path": entry["file"], "owner": entry["owner"], "issue": issue,
                         "deadline": entry["deadline"], "days_remaining": days, "severity": severity,
                         "snapshot_issue_state": issue_states.get(int(issue[1:]), "UNKNOWN")})
        rows.sort(key=lambda row: (row["days_remaining"], row["path"]))

    has = {source: any(item["input"] == source for item in diagnostics) for source in ("registry", "metadata")}
    deadline_coverage = "unavailable" if population is None else ("partial" if has["registry"] else "complete")
    if not has["metadata"]:
        metadata_coverage = "complete"
    elif metadata["freshness"] in {"MISSING", "INVALID"} and not issue_states:
        metadata_coverage = "unavailable"
    else:
        metadata_coverage = "partial"
    if deadline_coverage == "unavailable":
        status = "unavailable"
    elif deadline_coverage == "complete" and metadata_coverage == "complete":
        status = "complete"
    else:
        status = "partial"
    return {
        "schema_version": REPORT_SCHEMA_VERSION,
        "title": "Giant deadline warning",
        "scope": SCOPE_NOTE,
        "run": {**provenance, "observed_at": now.isoformat().replace("+00:00", "Z"),
                "observed_utc_date": today.isoformat()},
        "inputs": {key: {"path": source.path, "status": "read" if source.error is None else source.error,
                         "sha256": source.sha256} for key, source in inputs.items()},
        "completeness": {"status": status, "deadline_coverage": deadline_coverage,
                         "metadata_coverage": metadata_coverage, "diagnostics": diagnostics},
        "deadlines": {"rows": rows, "counts": counts, "untracked_paths": untracked},
        "metadata": metadata,
        "consumer_receipt": {"status": "UNVERIFIED", "evidence": None},
    }


def _md(value: object) -> str:
    text = html.escape("" if value is None else str(value), quote=False)
    text = re.sub(r"([\\`*_\[\]|~])", r"\\\1", text)
    return re.sub(r"[\r\n]+", " ", text)


def render_markdown(report: dict) -> str:
    run, completeness, deadlines, metadata = (report[key] for key in ("run", "completeness", "deadlines", "metadata"))
    lines = [f"# {report['title']}", "", f"> {_md(report['scope'])}", "",
             f"- Observed (UTC): {_md(run['observed_at'])}",
             f"- Source SHA: {_md(run.get('source_sha'))} (event SHA {_md(run.get('event_sha'))})",
             f"- Run: {_md(run.get('repository'))} run {_md(run.get('run_id'))} attempt "
             f"{_md(run.get('run_attempt'))} ({_md(run.get('event_name'))})",
             f"- Completeness: **{completeness['status']}** (deadlines {completeness['deadline_coverage']}, "
             f"metadata {completeness['metadata_coverage']})",
             f"- Consumer receipt: {report['consumer_receipt']['status']}", "", "## Deadlines", ""]
    counts = deadlines["counts"]
    if counts is None:
        lines.append("Deadline counts were **not produced**: the registry could not be read as a population.")
    else:
        lines.append(" · ".join(f"{name.replace('_', ' ')} {value}" for name, value in counts.items()))
        lines.append("")
        if deadlines["rows"]:
            lines += ["| Severity | Days | Deadline | Path | Owner | Issue | Snapshot state |",
                      "| --- | ---: | --- | --- | --- | --- | --- |"]
            lines += [f"| {row['severity']} | {row['days_remaining']} | {_md(row['deadline'])} | {_md(row['path'])} "
                      f"| {_md(row['owner'])} | {_md(row['issue'])} | {row['snapshot_issue_state']} |"
                      for row in deadlines["rows"]]
        else:
            lines.append(f"No valid shrink registration is overdue or due within {HORIZON_DAYS} days.")
        if deadlines["untracked_paths"]:
            lines += ["", "Registrations outside deadline tracking:"]
            lines += [f"- {_md(path)}" for path in deadlines["untracked_paths"]]
    lines += ["", "## Metadata freshness", "", f"- Freshness: **{metadata['freshness']}**",
              f"- refreshed_at: {_md(metadata['refreshed_at'])}",
              f"- fresh through (inclusive): {_md(metadata['fresh_through'])}"]
    if metadata["remaining_seconds"] is not None:
        lines.append(f"- remaining: {metadata['remaining_seconds'] / 86400:.2f} days")
    if completeness["diagnostics"]:
        lines += ["", "## Diagnostics", ""]
        lines += [f"- [{item['input']}] {_md(item['path'] or '-')}: {_md(item['problem'])}"
                  for item in completeness["diagnostics"]]
    lines += ["", "## Inputs", "", "| Input | Status | SHA-256 |", "| --- | --- | --- |"]
    lines += [f"| {_md(item['path'])} | {_md(item['status'])} | {_md(item['sha256'])} |"
              for item in report["inputs"].values()]
    return "\n".join(lines) + "\n"


def _git(root: Path, *args: str) -> str | None:
    try:
        done = subprocess.run(["git", "--no-optional-locks", "-C", str(root), *args],
                              capture_output=True, text=True, check=False)
    except OSError:
        return None
    return done.stdout.strip() if done.returncode == 0 else None


def checkout_sha(root: Path, expected: str | None) -> str | None:
    head = _git(root, "rev-parse", "HEAD")
    if expected is None:
        return head
    if head != expected:
        raise ContextError("checkout HEAD does not match the expected SHA")
    if _git(root, "status", "--porcelain", "--untracked-files=no", "--", REGISTRY_PATH, METADATA_PATH) != "":
        raise ContextError("registry or metadata differs from the checked-out commit")
    return head


def prepare_out_dir(out_dir: Path, root: Path) -> Path:
    if out_dir.is_symlink():
        raise ContextError("output directory must not be a symlink")
    resolved, checkout = out_dir.resolve(), root.resolve()
    if resolved == checkout or checkout in resolved.parents:
        raise ContextError("output directory must be outside the checkout")
    resolved.mkdir(parents=True, exist_ok=True)
    if any(os.path.lexists(resolved / name) for name in REPORT_FILES):
        raise ContextError("output directory already holds a report; refusing to reuse it")
    return resolved


def _write_atomic(path: Path, text: str) -> None:
    fd, temp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(text)
        os.replace(temp, path)
    except BaseException:
        os.unlink(temp)
        raise


def write_outputs(out_dir: Path, report: dict) -> None:
    # report.json is written last; its presence marks a complete write.
    texts = {"report.md": render_markdown(report), "report.json": json.dumps(report, indent=2, sort_keys=True) + "\n"}
    try:
        for name in REPORT_FILES:
            _write_atomic(out_dir / name, texts[name])
    except OSError:
        for name in REPORT_FILES:
            (out_dir / name).unlink(missing_ok=True)
        raise


def verify(root: Path, out_dir: Path, expected: str) -> list[str]:
    """Post-run check: checkout unchanged and report hashes match the inputs on disk."""
    errors = []
    if _git(root, "rev-parse", "HEAD") != expected:
        errors.append("checkout HEAD does not match the expected SHA")
    if _git(root, "status", "--porcelain", "--untracked-files=no") != "":
        errors.append("tracked files changed during the run")
    report_path = out_dir / "report.json"
    if not report_path.is_file():
        return errors
    report = json.loads(report_path.read_text(encoding="utf-8"))
    if report["run"].get("source_sha") != expected:
        errors.append("report source SHA does not match the expected SHA")
    for key, source in read_inputs(root).items():
        if report["inputs"][key]["sha256"] != source.sha256:
            errors.append(f"{source.path} hash differs from the report")
    return errors


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--out", type=Path, required=True, help="run directory outside the checkout")
    parser.add_argument("--expect-sha", help="event SHA the checkout HEAD must equal")
    parser.add_argument("--verify", action="store_true", help="verify a finished run instead of reporting")
    parser.add_argument("--now", help="fixed UTC observation time (ISO 8601) for reproducing a report")
    for name in ("repository", "run-id", "run-attempt", "event-name"):
        parser.add_argument(f"--{name}")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None, *, clock=_utc_now) -> int:
    args = _parse_args(argv)
    if args.verify:
        if not args.expect_sha:
            print("report_giant_deadlines: --verify requires --expect-sha", file=sys.stderr)
            return 1
        errors = verify(args.root, args.out, args.expect_sha)
        for error in errors:
            print(f"report_giant_deadlines: {error}", file=sys.stderr)
        return 1 if errors else 0
    try:
        source_sha = checkout_sha(args.root, args.expect_sha)
        out_dir = prepare_out_dir(args.out, args.root)
    except ContextError as error:
        print(f"report_giant_deadlines: {error}; no report written", file=sys.stderr)
        return 1
    now = datetime.fromisoformat(args.now) if args.now else clock()
    provenance = {"repository": args.repository, "source_sha": source_sha, "event_sha": args.expect_sha,
                  "run_id": args.run_id, "run_attempt": args.run_attempt, "event_name": args.event_name}
    report = build_report(read_inputs(args.root), now=now, provenance=provenance)
    try:
        write_outputs(out_dir, report)
    except OSError as error:
        print(f"report_giant_deadlines: cannot write report ({type(error).__name__}); no report written",
              file=sys.stderr)
        return 1
    status = report["completeness"]["status"]
    print(f"giant deadline report: {status}; counts {json.dumps(report['deadlines']['counts'], sort_keys=True)}")
    return 0 if status == "complete" else 2


if __name__ == "__main__":
    sys.exit(main())
