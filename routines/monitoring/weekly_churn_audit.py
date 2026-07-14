#!/usr/bin/env python3
"""Audit one week of repeated fix churn and flag redesign candidates."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from collections import Counter
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Callable, Mapping, Sequence

from daily_log_digest import (
    REPOSITORY,
    create_github_issue,
    load_open_issues,
    runtime_root,
)
from log_digest_issue_drafts import (
    CONFIRMED_APPROVAL,
    IssueDraft,
    OpenIssue,
    PostDecision,
    maybe_post_approved_drafts,
    write_pending_drafts,
)


DEFAULT_THRESHOLD = 3
DEFAULT_SINCE = "7 days"
DEFAULT_API_URL = "http://127.0.0.1:8791/api/discord/send"
LINEAGE_PATH_STATE_LIMIT = 10_000
_FIX_SUBJECT_RE = re.compile(r"^fix(?:\([^)\r\n]+\))?!?:")
_ISSUE_REFERENCE_RE = re.compile(r"(?<![\w#])#([1-9]\d*)\b")
# Trailing (#N) groups are GitHub squash-merge PR numbers and are intentionally
# excluded from lineage; including them would reintroduce the original
# PR-number-as-generation bug.
_SQUASH_PR_SUFFIX_RE = re.compile(r"(?:\s+\(#[1-9]\d*\))+\s*$")


@dataclass(frozen=True)
class GitCommit:
    sha: str
    subject: str
    body: str
    files: tuple[str, ...]


@dataclass(frozen=True)
class ChurnCandidate:
    file: str
    count: int
    commits: tuple[GitCommit, ...]


@dataclass(frozen=True)
class IssueLineage:
    issues: tuple[int, ...]

    @property
    def generations(self) -> int:
        return len(self.issues)


@dataclass(frozen=True)
class ChurnAudit:
    since: str
    threshold: int
    fix_commits: tuple[GitCommit, ...]
    file_counts: Mapping[str, int]
    module_counts: Mapping[str, int]
    candidates: tuple[ChurnCandidate, ...]
    lineages: tuple[IssueLineage, ...]


def is_fix_commit_subject(subject: str) -> bool:
    """Recognize conventional fix subjects, including breaking-change markers."""

    return _FIX_SUBJECT_RE.match(subject) is not None


def issue_references(subject: str, body: str = "") -> tuple[int, ...]:
    """Return genuine issue references in text order, excluding squash PR suffixes."""

    ordered: list[int] = []
    seen: set[int] = set()
    issue_text = f"{_SQUASH_PR_SUFFIX_RE.sub('', subject)}\n{body}"
    for match in _ISSUE_REFERENCE_RE.finditer(issue_text):
        number = int(match.group(1))
        if number not in seen:
            seen.add(number)
            ordered.append(number)
    return tuple(ordered)


def module_for_file(file: str) -> str:
    """Map a file to its containing repo-relative module directory."""

    path = PurePosixPath(file)
    if path.name == "mod.rs":
        return str(path.parent)
    if str(path.parent) != ".":
        return str(path.parent)
    return path.stem


def log(message: str) -> None:
    print(f"weekly-churn-audit: {message}", file=sys.stderr)


def _git(repo_root: Path, args: Sequence[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo_root), *args],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def _require_git(result: subprocess.CompletedProcess[str], operation: str) -> str:
    if result.returncode != 0:
        detail = result.stderr.strip() or f"git exited {result.returncode}"
        raise RuntimeError(f"could not {operation}: {detail}")
    return result.stdout


def collect_git_commits(repo_root: Path, since: str = DEFAULT_SINCE) -> list[GitCommit]:
    """Collect commit text and changed paths from the requested local git window."""

    log_output = _require_git(
        _git(repo_root, ["log", f"--since={since}", "--format=%H"]),
        "read weekly git log",
    )
    commits: list[GitCommit] = []
    for sha in (line.strip() for line in log_output.splitlines() if line.strip()):
        text = _require_git(
            _git(repo_root, ["show", "-s", "--format=%s%x00%b", sha]),
            f"read commit text for {sha}",
        )
        subject, separator, body = text.rstrip("\n").partition("\x00")
        if not separator:
            raise RuntimeError(f"could not parse commit text for {sha}")
        if not is_fix_commit_subject(subject):
            commits.append(GitCommit(sha=sha, subject=subject, body=body.strip(), files=()))
            continue
        changed = _require_git(
            _git(
                repo_root,
                ["diff-tree", "--root", "--no-commit-id", "--name-only", "-r", sha],
            ),
            f"read changed files for {sha}",
        )
        files = tuple(sorted({line for line in changed.splitlines() if line}))
        commits.append(GitCommit(sha=sha, subject=subject, body=body.strip(), files=files))
    return commits


def _longest_lineage(
    component: set[int], edges: Mapping[int, set[int]]
) -> tuple[int, ...]:
    best: tuple[int, ...] = ()
    starts = sorted(component)
    pending = [(node, ()) for node in reversed(starts[:LINEAGE_PATH_STATE_LIMIT])]
    scheduled = len(pending)
    truncated = len(starts) > LINEAGE_PATH_STATE_LIMIT
    while pending:
        node, path = pending.pop()
        extended = (*path, node)
        if len(extended) > len(best) or (len(extended) == len(best) and extended < best):
            best = extended
        for child in reversed(sorted(edges.get(node, set()))):
            if child in extended:
                continue
            if scheduled >= LINEAGE_PATH_STATE_LIMIT:
                truncated = True
                break
            pending.append((child, extended))
            scheduled += 1
    if truncated:
        log(
            "issue-lineage search truncated at "
            f"{LINEAGE_PATH_STATE_LIMIT} path states for a {len(component)}-issue component"
        )
    return best


def compute_issue_lineages(commits: Sequence[GitCommit]) -> tuple[IssueLineage, ...]:
    """Merge commit-text #A→#B edges and report the longest chain per component."""

    nodes: set[int] = set()
    edges: dict[int, set[int]] = {}
    neighbours: dict[int, set[int]] = {}
    for commit in commits:
        references = issue_references(commit.subject, commit.body)
        nodes.update(references)
        for parent, child in zip(references, references[1:]):
            edges.setdefault(parent, set()).add(child)
            neighbours.setdefault(parent, set()).add(child)
            neighbours.setdefault(child, set()).add(parent)

    lineages: list[IssueLineage] = []
    unseen = set(nodes)
    while unseen:
        first = min(unseen)
        component: set[int] = set()
        pending = [first]
        while pending:
            node = pending.pop()
            if node in component:
                continue
            component.add(node)
            pending.extend(neighbours.get(node, set()) - component)
        unseen -= component
        lineages.append(IssueLineage(_longest_lineage(component, edges)))
    return tuple(sorted(lineages, key=lambda item: (-item.generations, item.issues)))


def analyze_churn(
    commits: Sequence[GitCommit],
    *,
    since: str = DEFAULT_SINCE,
    threshold: int = DEFAULT_THRESHOLD,
) -> ChurnAudit:
    if threshold <= 0:
        raise ValueError("threshold must be positive")

    fix_commits = tuple(commit for commit in commits if is_fix_commit_subject(commit.subject))
    file_counts: Counter[str] = Counter()
    module_counts: Counter[str] = Counter()
    commits_by_file: dict[str, list[GitCommit]] = {}
    for commit in fix_commits:
        unique_files = set(commit.files)
        file_counts.update(unique_files)
        module_counts.update({module_for_file(file) for file in unique_files})
        for file in unique_files:
            commits_by_file.setdefault(file, []).append(commit)

    candidates = tuple(
        ChurnCandidate(file=file, count=count, commits=tuple(commits_by_file[file]))
        for file, count in sorted(file_counts.items(), key=lambda item: (-item[1], item[0]))
        if count >= threshold
    )
    return ChurnAudit(
        since=since,
        threshold=threshold,
        fix_commits=fix_commits,
        file_counts=dict(file_counts),
        module_counts=dict(module_counts),
        candidates=candidates,
        lineages=compute_issue_lineages(commits),
    )


def _candidate_marker(candidate: ChurnCandidate) -> str:
    return f"<!-- churn-audit:candidate={candidate.file} -->"


def build_candidate_draft(candidate: ChurnCandidate, since: str, threshold: int) -> IssueDraft:
    evidence = [
        f"- `{commit.sha[:12]}` {commit.subject}" for commit in candidate.commits
    ]
    body = "\n".join(
        [
            _candidate_marker(candidate),
            "# 주간 회귀 churn 재설계 후보",
            "",
            "This is a pending draft generated for human review. It has not been posted to GitHub.",
            "",
            f"- File: `{candidate.file}`",
            f"- Window: `git log --since={since!r}`",
            f"- Fix commits: `{candidate.count}`",
            f"- Candidate threshold: `>={threshold}`",
            "",
            "## Fix lineage evidence",
            "",
            *evidence,
            "",
            "## Human review",
            "",
            "Confirm the recurring failure model, intended ownership, and redesign boundary before approval.",
        ]
    )
    return IssueDraft(
        severity="CHURN",
        signature=_candidate_marker(candidate),
        count=candidate.count,
        title=f"ops(process): redesign candidate for {candidate.file}",
        body=body,
    )


def candidate_drafts(
    candidates: Sequence[ChurnCandidate],
    open_issues: Sequence[OpenIssue],
    *,
    since: str,
    threshold: int,
    dedup_available: bool = True,
) -> tuple[list[IssueDraft], list[tuple[ChurnCandidate, OpenIssue]]]:
    """Build drafts only when the complete open-issue dedup authority is available."""

    drafts: list[IssueDraft] = []
    matches: list[tuple[ChurnCandidate, OpenIssue]] = []
    for candidate in candidates:
        matching = next(
            (
                issue
                for issue in open_issues
                if _candidate_marker(candidate) in issue.body
            ),
            None,
        )
        if matching is not None:
            matches.append((candidate, matching))
        elif dedup_available:
            drafts.append(build_candidate_draft(candidate, since, threshold))
    return drafts, matches


def format_report(audit: ChurnAudit, notes: Sequence[str] = ()) -> str:
    lines = [
        f"📈 AgentDesk 주간 회귀 churn 감사 — git log --since={audit.since!r}",
        f"Fix commits: {len(audit.fix_commits)} | 재설계 후보 threshold: >={audit.threshold}",
        "",
        f"재설계 후보 ({len(audit.candidates)}):",
    ]
    if audit.candidates:
        lines.extend(f"- {item.count}× `{item.file}`" for item in audit.candidates)
    else:
        lines.append("- none")

    lines.extend(["", f"파일별 fix-commit tally ({len(audit.file_counts)}):"])
    if audit.file_counts:
        lines.extend(
            f"- {count}× `{file}`"
            for file, count in sorted(
                audit.file_counts.items(), key=lambda item: (-item[1], item[0])
            )
        )
    else:
        lines.append("- none")

    lines.extend(["", f"모듈별 fix-commit tally ({len(audit.module_counts)}):"])
    if audit.module_counts:
        lines.extend(
            f"- {count}× `{module}`"
            for module, count in sorted(
                audit.module_counts.items(), key=lambda item: (-item[1], item[0])
            )
        )
    else:
        lines.append("- none")

    lines.extend(["", f"Issue-reference lineages ({len(audit.lineages)}):"])
    if audit.lineages:
        lines.extend(
            f"- generations={lineage.generations}: "
            + "→".join(f"#{issue}" for issue in lineage.issues)
            for lineage in audit.lineages
        )
    else:
        lines.append("- none")
    lines.extend(["", *(f"⚠ {note}" for note in notes)])
    return "\n".join(lines).rstrip()


def _post_report(api_url: str, channel_id: str, report: str) -> None:
    payload = json.dumps(
        {
            "target": f"channel:{channel_id}",
            "content": report,
            "source": "weekly-churn-audit",
            "bot": "notify",
        }
    ).encode()
    request = urllib.request.Request(
        api_url,
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        response_payload = json.loads(response.read().decode())
    if not isinstance(response_payload, dict) or response_payload.get("ok") is not True:
        raise RuntimeError(f"AgentDesk channel post rejected: {response_payload!r}")


def maybe_post_weekly_channel(
    report: str,
    approval_mode: str,
    channel_id: str | None,
    state_path: Path,
    post_report: Callable[[str], None],
) -> tuple[bool, str]:
    """Post once per report fingerprint, and only behind literal confirmation."""

    if approval_mode != CONFIRMED_APPROVAL:
        return False, "weekly ops channel post disabled"
    if not channel_id:
        return False, "channel post confirmed but AGENTDESK_CHURN_AUDIT_CHANNEL_ID is unset"

    fingerprint = hashlib.sha256(report.encode()).hexdigest()
    try:
        prior = json.loads(state_path.read_text(encoding="utf-8")) if state_path.is_file() else {}
    except (OSError, json.JSONDecodeError):
        prior = {}
    if isinstance(prior, dict) and prior.get("fingerprint") == fingerprint:
        return False, "identical weekly report already posted"

    post_report(report)
    state_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = state_path.with_suffix(".tmp")
    temporary.write_text(json.dumps({"fingerprint": fingerprint}) + "\n", encoding="utf-8")
    temporary.replace(state_path)
    return True, "weekly ops channel report posted"


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def threshold_from_env(value: str | None) -> tuple[int, str | None]:
    configured = value if value is not None else str(DEFAULT_THRESHOLD)
    try:
        return positive_int(configured), None
    except (ValueError, argparse.ArgumentTypeError):
        return (
            DEFAULT_THRESHOLD,
            "invalid AGENTDESK_CHURN_AUDIT_THRESHOLD "
            f"value {configured!r}; using default {DEFAULT_THRESHOLD}",
        )


def parse_args() -> argparse.Namespace:
    script_repo_root = Path(__file__).resolve().parents[2]
    env_threshold, threshold_warning = threshold_from_env(
        os.environ.get("AGENTDESK_CHURN_AUDIT_THRESHOLD")
    )
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=script_repo_root)
    parser.add_argument("--runtime-root", type=Path, default=runtime_root())
    parser.add_argument(
        "--repo", default=os.environ.get("AGENTDESK_CHURN_AUDIT_REPO", REPOSITORY)
    )
    parser.add_argument(
        "--since", default=os.environ.get("AGENTDESK_CHURN_AUDIT_SINCE", DEFAULT_SINCE)
    )
    parser.add_argument(
        "--threshold",
        type=positive_int,
        default=None,
    )
    args = parser.parse_args()
    if args.threshold is None:
        args.threshold = env_threshold
        if threshold_warning:
            log(threshold_warning)
    return args


def main() -> int:
    args = parse_args()
    audit = analyze_churn(
        collect_git_commits(args.repo_root, args.since),
        since=args.since,
        threshold=args.threshold,
    )
    notes: list[str] = []

    issue_mode = os.environ.get("AGENTDESK_CHURN_AUDIT_CREATE_ISSUE", "off")
    if issue_mode == CONFIRMED_APPROVAL:
        open_issues, dedup_warning = load_open_issues(args.repo)
        if dedup_warning:
            notes.append(dedup_warning)
        proposed, matches = candidate_drafts(
            audit.candidates,
            open_issues,
            since=args.since,
            threshold=args.threshold,
            dedup_available=dedup_warning is None,
        )
        pending_dir = (
            args.runtime_root / "runtime" / "pending-issue-drafts" / "weekly-churn-audit"
        )
        drafts = write_pending_drafts(proposed, pending_dir)
        if matches:
            notes.append(
                "open-issue dedup matched: "
                + ", ".join(f"{item.file}→#{issue.number}" for item, issue in matches)
            )
        post: PostDecision = maybe_post_approved_drafts(
            drafts,
            issue_mode,
            lambda draft: create_github_issue(args.repo, draft),
        )
        notes.append(
            "pending drafts: "
            + (", ".join(str(draft.path) for draft in drafts) if drafts else "none")
        )
        if not post.attempted:
            notes.append(post.reason)
        elif post.created_urls:
            notes.append("human-confirmed issues created: " + ", ".join(post.created_urls))
    elif issue_mode == "off":
        notes.append(
            "issue drafts dry-run only; set AGENTDESK_CHURN_AUDIT_CREATE_ISSUE=confirmed "
            "to dedup and emit human-review drafts"
        )
    else:
        notes.append(
            "invalid AGENTDESK_CHURN_AUDIT_CREATE_ISSUE value ignored; "
            "use literal 'confirmed' or 'off'"
        )

    report = format_report(audit, notes)
    channel_mode = os.environ.get("AGENTDESK_CHURN_AUDIT_POST_CHANNEL", "off")
    try:
        posted, channel_note = maybe_post_weekly_channel(
            report,
            channel_mode,
            os.environ.get("AGENTDESK_CHURN_AUDIT_CHANNEL_ID"),
            args.runtime_root / "runtime" / "weekly-churn-audit" / "post-state.json",
            lambda content: _post_report(
                os.environ.get("AGENTDESK_CHURN_AUDIT_API", DEFAULT_API_URL),
                os.environ.get("AGENTDESK_CHURN_AUDIT_CHANNEL_ID", ""),
                content,
            ),
        )
    except (OSError, RuntimeError, ValueError, urllib.error.URLError) as error:
        posted, channel_note = False, f"weekly ops channel post failed: {error}"
    if channel_mode not in {"off", CONFIRMED_APPROVAL}:
        channel_note = (
            "invalid AGENTDESK_CHURN_AUDIT_POST_CHANNEL value ignored; "
            "use literal 'confirmed' or 'off'"
        )
    if posted or channel_mode != "off":
        report = f"{report}\n⚠ {channel_note}"

    print(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
