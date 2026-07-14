"""Shared cap-admission validation for the giant and hot-file ratchets (#4269).

The checked-in cap manifests describe the current ceilings.  A cap increase is
an admission only when the current manifest value is greater than the value at
the branch's merge-base with ``origin/main``.  Every such increase must append
one event to ``ratchet_admission_history.toml``.

The history is deliberately small and append-only.  Each ``[[admission]]`` has
this schema::

    ratchet = "hotfile_ratchet"  # or "giant_file_ratchet"
    file = "repo/relative/path.rs"
    old_cap = 100
    new_cap = 120
    decompose_issue = 4269        # positive integer, no leading '#'
    count = 2                     # previous event count for this file + 1

``count`` is global per file (across both metrics), so churn audit #4265 can
read a single monotonic sequence.  The checker only warns after repeated
admissions; it never creates issues or invokes ``gh``.
"""

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
) -> list[str]:
    """Require one valid newly appended history event for every raised cap."""

    errors = validate_history(current_events)
    if prior_history_exists and current_events[: len(prior_events)] != prior_events:
        errors.append(
            f"{HISTORY_REL_PATH} is append-only; existing admission events "
            "must not be edited, reordered, or removed"
        )
        return errors

    # The first schema commit may migrate reviewable annotated admissions such
    # as #3983 without those historical events corresponding to a current cap
    # raise.  Once the history exists at the base, every appended event must
    # correspond exactly to a raise in the same change.
    new_events = current_events[len(prior_events) :] if prior_history_exists else []
    raised = {
        file: (prior_caps[file], cap)
        for file, cap in current_caps.items()
        if file in prior_caps and cap > prior_caps[file]
    }

    for file, (old_cap, new_cap) in sorted(raised.items()):
        matches = [
            event
            for event in new_events
            if event.ratchet == ratchet
            and event.file == file
            and event.old_cap == old_cap
            and event.new_cap == new_cap
        ]
        if not matches:
            errors.append(
                f"{ratchet} cap raised for {file}: {old_cap} -> {new_cap}; "
                f"append a matching [[admission]] to {HISTORY_REL_PATH} with "
                "a mandatory decompose_issue issue number"
            )
        elif len(matches) > 1:
            errors.append(
                f"{ratchet} cap raised for {file}: {old_cap} -> {new_cap}; "
                "history contains duplicate matching admission events"
            )

    if prior_history_exists:
        expected = [
            (ratchet, file, old_cap, new_cap)
            for file, (old_cap, new_cap) in raised.items()
        ]
        for event in new_events:
            key = (event.ratchet, event.file, event.old_cap, event.new_cap)
            if event.ratchet == ratchet and key not in expected:
                errors.append(
                    f"unmatched {ratchet} admission history event for "
                    f"{event.file!r}: {event.old_cap!r} -> {event.new_cap!r}; "
                    "only append an event in the change that raises that cap"
                )
    return errors


def _run_git(repo_root: Path, args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo_root), *args],
        check=False,
        capture_output=True,
        text=True,
    )


def _merge_base(repo_root: Path) -> tuple[str | None, str | None]:
    configured = os.environ.get("RATCHET_ADMISSION_BASE_REF")
    candidates = [configured] if configured else []
    github_base = os.environ.get("GITHUB_BASE_REF")
    if github_base:
        candidates.append(f"origin/{github_base}")
    candidates.extend(["origin/main", "main"])

    for candidate in candidates:
        if not candidate:
            continue
        result = _run_git(repo_root, ["merge-base", "HEAD", candidate])
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip(), None
    return None, (
        "cannot resolve a ratchet admission base (tried "
        + ", ".join(candidate for candidate in candidates if candidate)
        + "); set RATCHET_ADMISSION_BASE_REF to a fetched base ref"
    )


def _git_text(repo_root: Path, commit: str, rel_path: str) -> tuple[str | None, bool]:
    result = _run_git(repo_root, ["show", f"{commit}:{rel_path}"])
    if result.returncode == 0:
        return result.stdout, True
    # A missing file is expected for the first history-schema commit.  Distinguish
    # it from a broader git failure by asking whether the path exists at all.
    exists = _run_git(repo_root, ["cat-file", "-e", f"{commit}:{rel_path}"])
    if exists.returncode != 0:
        return None, False
    raise ValueError(
        f"git could not read {rel_path} at {commit}: "
        f"{result.stderr.strip() or 'unknown error'}"
    )


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
    *,
    repo_root: Path,
    ratchet: str,
    config_rel_path: str,
    table_name: str,
) -> AdmissionAudit:
    """Audit current cap raises against the base manifest and event history."""

    try:
        current_text = (repo_root / config_rel_path).read_text(encoding="utf-8")
        current_caps = parse_cap_table(current_text, table_name)
        history_path = repo_root / HISTORY_REL_PATH
        current_events = parse_history(history_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError, ValueError) as exc:
        return AdmissionAudit((f"ratchet admission metadata invalid: {exc}",), ())

    schema_errors = validate_history(current_events)
    warnings = admission_warning_messages(current_events, ratchet)

    # Unit fixtures and source exports do not carry git metadata.  The schema is
    # still validated there; branch-delta enforcement runs in every CI checkout.
    if not (repo_root / ".git").exists():
        return AdmissionAudit(tuple(schema_errors), tuple(warnings))

    base, base_error = _merge_base(repo_root)
    if base is None:
        return AdmissionAudit(
            tuple(schema_errors + [base_error or "missing base"]), tuple(warnings)
        )

    try:
        prior_config_text, prior_config_exists = _git_text(
            repo_root, base, config_rel_path
        )
        if not prior_config_exists or prior_config_text is None:
            return AdmissionAudit(tuple(schema_errors), tuple(warnings))
        prior_caps = parse_cap_table(prior_config_text, table_name)
        prior_history_text, prior_history_exists = _git_text(
            repo_root, base, HISTORY_REL_PATH
        )
        prior_events = (
            parse_history(prior_history_text)
            if prior_history_text is not None
            else []
        )
        errors = validate_admission_delta(
            ratchet=ratchet,
            current_caps=current_caps,
            prior_caps=prior_caps,
            current_events=current_events,
            prior_events=prior_events,
            prior_history_exists=prior_history_exists,
        )
    except (tomllib.TOMLDecodeError, ValueError) as exc:
        errors = schema_errors + [f"ratchet admission base metadata invalid: {exc}"]
    return AdmissionAudit(tuple(errors), tuple(warnings))
