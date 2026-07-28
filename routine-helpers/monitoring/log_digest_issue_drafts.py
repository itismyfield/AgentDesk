#!/usr/bin/env python3
"""Reusable log-signature aggregation and human-gated issue-draft helpers.

The daily log digest and future audits (notably #4265) share this module so
signature normalization, open-issue deduplication, and the default-off issue
creation boundary do not drift between routines.
"""

from __future__ import annotations

import hashlib
import re
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Sequence


DEFAULT_DAILY_THRESHOLD = 50
CONFIRMED_APPROVAL = "confirmed"

_ANSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
_SEVERITY_RE = re.compile(r"\b(ERROR|WARN(?:ING)?)\b", re.IGNORECASE)
_TIMESTAMP_RE = re.compile(
    r"(?<!\d)"
    r"(?:\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}"
    r"(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?)"
)
_UUID_RE = re.compile(
    r"\b[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-"
    r"[89ab][0-9a-f]{3}-[0-9a-f]{12}\b",
    re.IGNORECASE,
)
_HASH_RE = re.compile(r"\b(?=[0-9a-f]{7,64}\b)(?=[0-9a-f]*[a-f])[0-9a-f]+\b", re.IGNORECASE)
_EMBEDDED_ID_RE = re.compile(
    r"\b((?:req(?:uest)?|trace|span|session|turn|task|card|dispatch|run|job|message|id)[_-])"
    r"(?=[a-z0-9]{4,}\b)(?=[a-z0-9]*[a-z])(?=[a-z0-9]*\d)[a-z0-9]+\b",
    re.IGNORECASE,
)
_DYNAMIC_KEY_VALUE_RE = re.compile(
    r"\b("
    r"(?:request|trace|span|session|turn|task|card|dispatch|run|job|message|channel|user|agent)"
    r"[_-]?id|id|token|nonce|cursor"
    r")\s*[:=]\s*(?:\"[^\"]+\"|'[^']+'|[^\s,;\]}]+)",
    re.IGNORECASE,
)
_MEASURED_NUMBER_RE = re.compile(
    r"(?<![A-Za-z0-9_-])(?<![A-Za-z0-9]\.)\d+(?:\.\d+)?\s*"
    r"(req/s|/s|kib|mib|gib|kb|mb|gb|ms|us|ns|b|s|m|h|%)"
    r"(?![A-Za-z0-9_-]|\.[A-Za-z0-9])",
    re.IGNORECASE,
)
_NUMBER_RE = re.compile(r"(?<![A-Za-z0-9])\d+(?:\.\d+)?(?![A-Za-z0-9])")
_WHITESPACE_RE = re.compile(r"\s+")
_TOKEN_RE = re.compile(r"[a-z][a-z0-9_-]{1,}")
_STOP_WORDS = {
    "and",
    "are",
    "error",
    "errors",
    "failed",
    "failure",
    "for",
    "from",
    "hash",
    "has",
    "id",
    "into",
    "issue",
    "log",
    "logs",
    "the",
    "this",
    "warn",
    "warning",
    "with",
    "uuid",
}

# Known best-effort limits: labelled ports and HTTP/status numbers intentionally
# remain distinct, so high-cardinality ephemeral values can fragment; collapsing
# them would also collapse meaningful service ports and status codes. Open-issue
# fuzzy matching can over-suppress a short unrelated signature or under-suppress
# a long reworded duplicate, and signatures longer than the 120-character issue
# title preview may fail to match their own title on a later run. Retuning the
# candidate window, Jaccard/containment thresholds, or number preservation merely
# trades precision for recall (and vice versa), so it is deliberately not done.
# Drafts are advisory: independently of threshold/dedup, the summary exposes the
# top three patterns per severity for human review; stable_draft_filename also
# overwrites one path instead of accumulating redraft churn.


@dataclass(frozen=True)
class SignatureCount:
    severity: str
    signature: str
    count: int
    sample: str


@dataclass(frozen=True)
class OpenIssue:
    number: int
    title: str
    body: str = ""
    url: str = ""


@dataclass(frozen=True)
class IssueDraft:
    severity: str
    signature: str
    count: int
    title: str
    body: str
    path: Path | None = None


@dataclass(frozen=True)
class DraftDecision:
    pattern: SignatureCount
    draft: IssueDraft | None
    matching_issue: OpenIssue | None


@dataclass(frozen=True)
class PostDecision:
    attempted: bool
    created_urls: tuple[str, ...]
    reason: str


def extract_severity(line: str) -> str | None:
    """Return canonical ERROR/WARN severity when the line contains one."""

    match = _SEVERITY_RE.search(_ANSI_RE.sub("", line))
    if not match:
        return None
    return "ERROR" if match.group(1).upper() == "ERROR" else "WARN"


def normalize_signature(line: str) -> str:
    """Collapse volatile log fields while retaining the semantic message.

    Timestamps, UUIDs, hashes, and known dynamic identifiers become stable
    placeholders. Bare numbers are treated as volatile except HTTP/status codes
    and values explicitly labelled as ports. This deliberately narrow heuristic
    preserves the common semantic cases without pretending every number's role
    can be inferred from free-form text; unlabeled semantic numbers may collapse.
    """

    normalized = _ANSI_RE.sub("", line).strip()
    normalized = _TIMESTAMP_RE.sub("", normalized)
    normalized = _SEVERITY_RE.sub("", normalized)
    normalized = _UUID_RE.sub("<uuid>", normalized)
    normalized = _HASH_RE.sub("<hash>", normalized)
    normalized = _EMBEDDED_ID_RE.sub(lambda match: f"{match.group(1)}<id>", normalized)
    normalized = _DYNAMIC_KEY_VALUE_RE.sub(lambda match: f"{match.group(1).lower()}=<id>", normalized)
    normalized = _MEASURED_NUMBER_RE.sub(_normalize_measured_number, normalized)
    normalized = _NUMBER_RE.sub(_normalize_bare_number, normalized)
    normalized = _WHITESPACE_RE.sub(" ", normalized).strip(" -:|\t")
    return normalized.lower()[:500]


def _normalize_measured_number(match: re.Match[str]) -> str:
    unit = match.group(1).lower()
    if unit in {"ms", "us", "ns", "s", "m", "h"}:
        return "<dur>"
    if unit in {"b", "kb", "mb", "gb", "kib", "mib", "gib"}:
        return "<size>"
    return "<rate>"


def _normalize_bare_number(match: re.Match[str]) -> str:
    prefix = match.string[max(0, match.start() - 24) : match.start()]
    if re.search(r"\b(?:http(?:\s+status)?|status|port)\s*$", prefix, re.IGNORECASE):
        return match.group(0)
    return "<n>"


def aggregate_normalized_signatures(lines: Iterable[str]) -> list[SignatureCount]:
    """Count ERROR/WARN lines by ``(severity, normalized signature)``."""

    counts: Counter[tuple[str, str]] = Counter()
    samples: dict[tuple[str, str], str] = {}
    for raw_line in lines:
        severity = extract_severity(raw_line)
        if severity is None:
            continue
        signature = normalize_signature(raw_line)
        if not signature:
            continue
        key = (severity, signature)
        counts[key] += 1
        samples.setdefault(key, _WHITESPACE_RE.sub(" ", _ANSI_RE.sub("", raw_line)).strip()[:500])

    return sorted(
        (
            SignatureCount(severity=severity, signature=signature, count=count, sample=samples[key])
            for key, count in counts.items()
            for severity, signature in [key]
        ),
        key=lambda pattern: (-pattern.count, pattern.severity, pattern.signature),
    )


def exceeds_threshold(count: int, threshold: int = DEFAULT_DAILY_THRESHOLD) -> bool:
    """The issue contract says *exceeds*, so equality does not cross the gate."""

    if threshold < 0:
        raise ValueError("threshold must be non-negative")
    return count > threshold


def _similarity_tokens_from_normalized(normalized: str) -> set[str]:
    return {
        token
        for token in _TOKEN_RE.findall(normalized)
        if token not in _STOP_WORDS and not token.startswith("timestamp")
    }


def _normalized_issue_candidates(issue: OpenIssue) -> list[str]:
    """Return bounded, issue-defining text rather than an unbounded epic body."""

    first_body_line = next((line.strip() for line in issue.body.splitlines() if line.strip()), "")
    return [normalize_signature(part) for part in (issue.title, first_body_line) if part]


def issue_matches_signature(signature: str, issue: OpenIssue) -> bool:
    """Match against the issue title or first non-empty body line.

    Each bounded candidate must share at least three meaningful tokens and reach
    50% symmetric Jaccard similarity. Direct containment is accepted only when
    the candidate is still signature-like (at most 3x the signature token count,
    with a floor of 12 tokens). This avoids a three-token signature matching an
    unrelated long epic that merely mentions those words. A duplicate described
    only deep in a long issue body may therefore require human reconciliation.
    """

    normalized_signature = normalize_signature(signature)
    signature_tokens = _similarity_tokens_from_normalized(normalized_signature)
    if len(signature_tokens) < 3:
        return False
    for candidate in _normalized_issue_candidates(issue):
        issue_tokens = _similarity_tokens_from_normalized(candidate)
        overlap = signature_tokens & issue_tokens
        if len(overlap) < 3:
            continue
        signature_like_limit = max(12, len(signature_tokens) * 3)
        if normalized_signature in candidate and len(issue_tokens) <= signature_like_limit:
            return True
        union = signature_tokens | issue_tokens
        if union and len(overlap) / len(union) >= 0.50:
            return True
    return False


def build_issue_draft(pattern: SignatureCount, window_label: str, threshold: int) -> IssueDraft:
    signature_preview = pattern.signature[:120]
    title = f"ops(log-digest): {pattern.severity} {signature_preview}"
    safe_sample = normalize_signature(pattern.sample) or pattern.signature
    body = "\n".join(
        [
            "# Daily log-digest draft",
            "",
            "This is a pending draft generated for human review. It has not been posted to GitHub.",
            "",
            f"- Window: `{window_label}`",
            f"- Severity: `{pattern.severity}`",
            f"- Count: `{pattern.count}`",
            f"- Draft threshold: `>{threshold}`",
            f"- Normalized signature: `{pattern.signature}`",
            "",
            "## Representative sample",
            "",
            "```text",
            f"{pattern.severity} {safe_sample}",
            "```",
            "",
            "## Human review",
            "",
            "Confirm impact, reproduction, ownership, and labels before approving issue creation.",
        ]
    )
    return IssueDraft(
        severity=pattern.severity,
        signature=pattern.signature,
        count=pattern.count,
        title=title,
        body=body,
    )


def decide_issue_drafts(
    patterns: Sequence[SignatureCount],
    open_issues: Sequence[OpenIssue],
    *,
    threshold: int = DEFAULT_DAILY_THRESHOLD,
    window_label: str = "last 24 hours",
    dedup_available: bool = True,
) -> list[DraftDecision]:
    """Apply threshold and fail-closed open-issue deduplication.

    If the GitHub open-issue scan is unavailable, threshold crossings are
    reported but no draft is emitted; this prevents duplicate drafts when the
    dedup authority cannot be consulted.
    """

    decisions: list[DraftDecision] = []
    for pattern in patterns:
        if not exceeds_threshold(pattern.count, threshold):
            continue
        matching_issue = next(
            (issue for issue in open_issues if issue_matches_signature(pattern.signature, issue)),
            None,
        )
        draft = None
        if dedup_available and matching_issue is None:
            draft = build_issue_draft(pattern, window_label, threshold)
        decisions.append(DraftDecision(pattern=pattern, draft=draft, matching_issue=matching_issue))
    return decisions


def stable_draft_filename(draft: IssueDraft) -> str:
    digest = hashlib.sha256(f"{draft.severity}\0{draft.signature}".encode()).hexdigest()[:16]
    return f"{draft.severity.lower()}-{digest}.md"


def write_pending_drafts(drafts: Iterable[IssueDraft], pending_dir: Path) -> list[IssueDraft]:
    pending_dir.mkdir(parents=True, exist_ok=True)
    written: list[IssueDraft] = []
    for draft in drafts:
        path = pending_dir / stable_draft_filename(draft)
        path.write_text(draft.body + "\n", encoding="utf-8")
        written.append(
            IssueDraft(
                severity=draft.severity,
                signature=draft.signature,
                count=draft.count,
                title=draft.title,
                body=draft.body,
                path=path,
            )
        )
    return written


def maybe_post_approved_drafts(
    drafts: Sequence[IssueDraft],
    approval_mode: str,
    create_issue: Callable[[IssueDraft], str],
) -> PostDecision:
    """Post only after the operator supplies the literal ``confirmed`` gate.

    This check is intentionally in the shared helper (not only its CLI caller),
    so every future consumer inherits the same default-off safety boundary.
    """

    if approval_mode != CONFIRMED_APPROVAL:
        return PostDecision(
            attempted=False,
            created_urls=(),
            reason="issue creation disabled; set approval mode to literal 'confirmed' after human review",
        )

    approved = [
        draft
        for draft in drafts
        if draft.path is not None and Path(f"{draft.path}.approved").is_file()
    ]
    if not approved:
        return PostDecision(
            attempted=False,
            created_urls=(),
            reason="confirmation enabled, but no human-reviewed .approved draft markers exist",
        )

    created_urls = tuple(create_issue(draft) for draft in approved)
    return PostDecision(attempted=True, created_urls=created_urls, reason="human-confirmed issue creation")


def _compact(text: str, limit: int = 100) -> str:
    return text if len(text) <= limit else text[: limit - 1] + "…"


def format_daily_summary(
    patterns: Sequence[SignatureCount],
    decisions: Sequence[DraftDecision],
    drafts: Sequence[IssueDraft],
    *,
    threshold: int,
    window_label: str,
    warnings: Sequence[str] = (),
    top_per_severity: int = 3,
) -> str:
    """Format the single routine-channel digest for one daily run."""

    lines = [f"📊 dcserver daily log digest — {window_label}"]
    for severity in ("ERROR", "WARN"):
        top = [pattern for pattern in patterns if pattern.severity == severity][:top_per_severity]
        if top:
            lines.append(f"{severity} top: " + " | ".join(
                f"{pattern.count}× {_compact(pattern.signature, 90)}" for pattern in top
            ))
        else:
            lines.append(f"{severity} top: none")
    lines.append("ℹ best-effort signatures; verify top patterns manually")

    crossed = [decision for decision in decisions]
    lines.append(f"Threshold >{threshold}: {len(crossed)} crossed")
    if crossed:
        lines.append(
            "Crossed: "
            + " | ".join(
                f"{decision.pattern.count}× {decision.pattern.severity} "
                f"{_compact(decision.pattern.signature, 75)}"
                for decision in crossed[:10]
            )
        )
        if len(crossed) > 10:
            lines.append(f"Crossed: +{len(crossed) - 10} more")
    matched = [decision for decision in crossed if decision.matching_issue is not None]
    if matched:
        references = ", ".join(f"#{decision.matching_issue.number}" for decision in matched)
        lines.append(f"Open-issue dedup: {len(matched)} matched ({references})")
    if drafts:
        lines.append("Pending drafts: " + ", ".join(str(draft.path or draft.title) for draft in drafts))
    else:
        lines.append("Pending drafts: none")
    lines.extend(f"⚠ {warning}" for warning in warnings)
    return "\n".join(lines)
