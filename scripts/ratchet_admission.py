"""Freeze hotfile caps; giant references and admission history are advisory."""

from __future__ import annotations

import os
import subprocess
import tomllib
from dataclasses import dataclass
from pathlib import Path

HISTORY_REL_PATH = "scripts/ratchet_admission_history.toml"
ADMISSION_WARN_THRESHOLD = 3


@dataclass(frozen=True)
class AdmissionEvent:
    ratchet: object
    file: object
    old_cap: object
    new_cap: object
    decompose_issue: object
    count: object


@dataclass(frozen=True)
class AdmissionAudit:
    errors: tuple[str, ...]
    warnings: tuple[str, ...]
    base_sha: str | None = None


def parse_cap_table(text: str, table_name: str) -> dict[str, int]:
    """Parse one path -> positive integer cap table from TOML text."""

    parsed = tomllib.loads(text)
    table = parsed.get(table_name, {})
    if not isinstance(table, dict):
        raise ValueError(f"[{table_name}] must be a TOML table")

    caps: dict[str, int] = {}
    for file, cap in table.items():
        if not isinstance(file, str) or not file:
            raise ValueError(f"[{table_name}] contains an invalid file key")
        if isinstance(cap, bool) or not isinstance(cap, int) or cap <= 0:
            raise ValueError(
                f"[{table_name}] cap for {file!r} must be a positive integer"
            )
        caps[file] = cap
    return caps


def parse_history(text: str) -> list[AdmissionEvent]:
    """Parse admission events, retaining malformed fields for clear validation."""

    parsed = tomllib.loads(text)
    if parsed.get("schema_version") != 1:
        raise ValueError("ratchet admission history schema_version must be 1")
    raw_events = parsed.get("admission", [])
    if not isinstance(raw_events, list):
        raise ValueError(
            "ratchet admission history [[admission]] entries must be an array"
        )
    events: list[AdmissionEvent] = []
    for raw in raw_events:
        if not isinstance(raw, dict):
            raise ValueError("ratchet admission history contains a non-table admission")
        events.append(
            AdmissionEvent(
                ratchet=raw.get("ratchet"),
                file=raw.get("file"),
                old_cap=raw.get("old_cap"),
                new_cap=raw.get("new_cap"),
                decompose_issue=raw.get("decompose_issue"),
                count=raw.get("count"),
            )
        )
    return events


def validate_history(events: list[AdmissionEvent]) -> list[str]:
    """Validate the simple append-only event schema and per-file counts."""

    errors: list[str] = []
    last_count: dict[str, int] = {}
    allowed_ratchets = {"giant_file_ratchet", "hotfile_ratchet"}
    for index, event in enumerate(events, start=1):
        label = f"admission event {index}"
        if not isinstance(event.ratchet, str) or event.ratchet not in allowed_ratchets:
            errors.append(
                f"{label}: ratchet must be one of {sorted(allowed_ratchets)}, "
                f"got {event.ratchet!r}"
            )
        if not isinstance(event.file, str) or not event.file:
            errors.append(f"{label}: file must be a non-empty repo-relative path")
            continue
        for field_name, value in (
            ("old_cap", event.old_cap),
            ("new_cap", event.new_cap),
            ("count", event.count),
        ):
            if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
                errors.append(
                    f"{label} ({event.file}): {field_name} must be a positive integer"
                )
        if (
            isinstance(event.old_cap, int)
            and not isinstance(event.old_cap, bool)
            and isinstance(event.new_cap, int)
            and not isinstance(event.new_cap, bool)
            and event.new_cap <= event.old_cap
        ):
            errors.append(
                f"{label} ({event.file}): new_cap {event.new_cap} must exceed "
                f"old_cap {event.old_cap}"
            )
        if (
            event.decompose_issue is None
            or isinstance(event.decompose_issue, bool)
            or not isinstance(event.decompose_issue, int)
            or event.decompose_issue <= 0
        ):
            errors.append(
                f"{label} ({event.file}): decompose_issue is mandatory and must "
                "be a positive issue number"
            )
        expected_count = last_count.get(event.file, 0) + 1
        if event.count != expected_count:
            errors.append(
                f"{label} ({event.file}): count must be {expected_count}, "
                f"got {event.count!r}"
            )
        if isinstance(event.count, int) and not isinstance(event.count, bool):
            last_count[event.file] = event.count
    return errors


def validate_admission_delta(
    *,
    ratchet: str,
    current_caps: dict[str, int],
    prior_caps: dict[str, int],
    current_events: list[AdmissionEvent],
    prior_events: list[AdmissionEvent],
    prior_history_exists: bool = True,
    advisory_paths: frozenset[str] = frozenset(),
) -> list[str]:
    """History cannot authorize a hotfile cap increase; giant references warn."""
    if ratchet == "giant_file_ratchet":
        return []
    return [f"{ratchet} cap raised for {file}: {prior_caps[file]} -> {cap}; shrink the file"
            for file, cap in sorted(current_caps.items())
            if file in prior_caps and cap > prior_caps[file] and file not in advisory_paths]


def _run_git(repo_root: Path, args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo_root), *args],
        check=False,
        capture_output=True,
        text=True,
    )


def comparison_base(repo_root: Path) -> tuple[str | None, str | None]:
    """Resolve one immutable comparison SHA; explicit or CI inputs never fall back."""
    if os.environ.get("GFP_EVENT_NAME") == "pull_request":
        try:
            def oid(ref):
                result = _run_git(repo_root, ["rev-parse", "--verify", f"{ref}^{{commit}}"])
                if result.returncode or not result.stdout.strip():
                    raise ValueError(f"cannot resolve ratchet object {ref!r}")
                return result.stdout.strip()
            candidate = oid(os.environ.get("GFP_CANDIDATE_SHA", ""))
            event_base = oid(os.environ.get("GFP_BASE_SHA", ""))
            head = oid(os.environ.get("GFP_HEAD_SHA", ""))
            parents = _run_git(repo_root, ["rev-list", "--parents", "-n1", candidate])
            lineage = parents.stdout.split()
            if (parents.returncode or oid("HEAD") != candidate or len(lineage) != 3
                    or lineage[0] != candidate or lineage[2] != head):
                raise ValueError("ratchet candidate/base/head provenance mismatch")
            ancestry = _run_git(repo_root, ["merge-base", "--is-ancestor", event_base, lineage[1]])
            if ancestry.returncode:
                raise ValueError("ratchet comparison base ancestry unavailable or invalid")
            return lineage[1], None
        except ValueError as error:
            return None, str(error)
    configured = os.environ.get("RATCHET_ADMISSION_BASE_REF")
    github_base = os.environ.get("GITHUB_BASE_REF")
    candidates = ([configured] if configured else
                  ([f"origin/{github_base}"] if github_base else ["origin/main", "main"]))
    for candidate in candidates:
        result = _run_git(repo_root, ["merge-base", "HEAD", candidate])
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip(), None
    return None, "cannot resolve a ratchet comparison base: " + ", ".join(candidates)


def _merge_base(repo_root: Path) -> tuple[str | None, str | None]:
    return comparison_base(repo_root)


def _git_text(repo_root: Path, commit: str, rel_path: str) -> tuple[str | None, bool]:
    result = _run_git(repo_root, ["show", f"{commit}:{rel_path}"])
    if result.returncode == 0:
        return result.stdout, True
    exists = _run_git(repo_root, ["ls-tree", "--name-only", commit, "--", rel_path])
    if exists.returncode == 0 and not exists.stdout.strip():
        return None, False
    raise ValueError(f"git could not read {rel_path} at {commit}: "
                     f"{result.stderr.strip() or 'unknown error'}")


def admission_warning_messages(
    events: list[AdmissionEvent], ratchet: str
) -> list[str]:
    latest: dict[str, AdmissionEvent] = {}
    for event in events:
        if isinstance(event.file, str):
            latest[event.file] = event
    warnings: list[str] = []
    for file, event in sorted(latest.items()):
        if (
            event.ratchet == ratchet
            and isinstance(event.count, int)
            and event.count > ADMISSION_WARN_THRESHOLD
        ):
            warnings.append(
                f"WARN: RATCHET ADMISSION COUNT EXCEEDED: {file} has been "
                f"admitted {event.count} times (threshold "
                f"{ADMISSION_WARN_THRESHOLD}); prioritize decomposition."
            )
    return warnings


def audit_repository_admissions(
    *, repo_root: Path, ratchet: str, config_rel_path: str, table_name: str,
) -> AdmissionAudit:
    """Cap input/read failures block; history shape and chronology only warn."""
    warnings = []
    try:
        current_caps = parse_cap_table((repo_root / config_rel_path).read_text(encoding="utf-8"), table_name)
    except (OSError, ValueError) as error:
        return AdmissionAudit((f"ratchet cap metadata invalid: {error}",), ())
    try:
        current_events = parse_history((repo_root / HISTORY_REL_PATH).read_text(encoding="utf-8"))
        warnings.extend(f"WARN: {message}" for message in validate_history(current_events))
        warnings.extend(admission_warning_messages(current_events, ratchet))
    except (OSError, ValueError) as error:
        current_events = []
        warnings.append(f"WARN: ratchet admission history invalid: {error}")
    if not (repo_root / ".git").exists():
        return AdmissionAudit((), tuple(warnings))
    base, error = comparison_base(repo_root)
    if base is None:
        return AdmissionAudit((error or "missing base",), tuple(warnings))
    try:
        prior_text, exists = _git_text(repo_root, base, config_rel_path)
        prior_caps = parse_cap_table(prior_text, table_name) if exists else {}
        prior_history, _ = _git_text(repo_root, base, HISTORY_REL_PATH)
        try:
            prior_events = parse_history(prior_history) if prior_history is not None else []
            if current_events[:len(prior_events)] != prior_events:
                warnings.append(f"WARN: {HISTORY_REL_PATH} is not append-only")
        except ValueError as error:
            warnings.append(f"WARN: ratchet base admission history invalid: {error}")
        from generate_inventory_docs import GIANT_FILE_THRESHOLD, split_prod_test_lines

        advisory = frozenset(
            file for file in current_caps
            if ratchet == "giant_file_ratchet"
            or split_prod_test_lines((repo_root / file).read_text(encoding="utf-8"))[0]
            >= GIANT_FILE_THRESHOLD
        )
        errors = validate_admission_delta(ratchet=ratchet, current_caps=current_caps,
            prior_caps=prior_caps, current_events=current_events, prior_events=[], advisory_paths=advisory)
        if advisory:
            unit = "production" if ratchet == "giant_file_ratchet" else "raw"
            warnings.extend(f"WARN: giant reference increased: {file}: +{cap - prior_caps[file]} "
                            f"{unit} lines ({prior_caps[file]} -> {cap})"
                            for file, cap in sorted(current_caps.items())
                            if file in advisory and file in prior_caps and cap > prior_caps[file])
    except (OSError, ValueError) as error:
        errors = [f"ratchet base cap metadata invalid: {error}"]
    return AdmissionAudit(tuple(errors), tuple(warnings), base)
