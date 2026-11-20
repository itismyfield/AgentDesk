"""Resolve report revisions and publish complete generated-artifact evidence."""

from __future__ import annotations

from dataclasses import asdict, dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


class ReportError(ValueError):
    """The requested comparison or evidence could not be produced."""


def git(root: Path, *args: str) -> bytes:
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True)
    if result.returncode:
        raise ReportError(f"git {args[0]}: {result.stderr.decode(errors='replace').strip()}")
    return result.stdout


@dataclass(frozen=True)
class Comparison:
    mode: str
    base: str
    head: str

    def describe(self) -> str:
        if self.mode == "self":
            return f"Self check at `{self.head}`."
        return f"`{self.base}` → `{self.head}` ({self.mode})."


def resolve_comparison(root: Path, env) -> Comparison:
    head = git(root, "rev-parse", "--verify", "HEAD").decode().strip()
    candidate = env.get("GITHUB_SHA") or env.get("GFP_CANDIDATE_SHA")
    if candidate and candidate != head:
        raise ReportError(f"candidate {candidate} differs from checked-out HEAD {head}")
    event = env.get("GITHUB_EVENT_NAME") or env.get("GFP_EVENT_NAME")
    if event == "pull_request":
        parents = git(root, "show", "-s", "--format=%P", head).decode().split()
        if len(parents) != 2 or env.get("GFP_HEAD_SHA", parents[-1]) != parents[-1]:
            raise ReportError("pull request report requires the checked-out two-parent PR candidate")
        bases = git(root, "merge-base", "--all", *parents).decode().split()
        if len(bases) != 1:
            raise ReportError("pull request report requires a unique merge base")
        return Comparison("pull_request", bases[0], head)
    if event == "push":
        event_path = env.get("GITHUB_EVENT_PATH")
        if not event_path:
            raise ReportError("main push report requires GITHUB_EVENT_PATH")
        payload = json.loads(Path(event_path).read_text(encoding="utf-8"))
        before = payload.get("before") if isinstance(payload, dict) else None
        if (not isinstance(before, str) or not re.fullmatch(r"[0-9a-f]{40}", before)
                or not int(before, 16) or payload.get("ref") != "refs/heads/main"):
            raise ReportError("main push report requires the previous main SHA in the event payload")
        if before == head:
            raise ReportError("previous main SHA is the candidate")
        git(root, "merge-base", "--is-ancestor", before, head)
        return Comparison("push", before, head)
    return Comparison("self", head, head)


def publish(directory: Path, comparison: Comparison, summary: str, files: dict[str, bytes], env) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    if any(directory.iterdir()):
        raise ReportError(f"{directory}: expected an empty artifact directory")
    files = {**files, "summary.md": summary.encode("utf-8")}
    for name, data in files.items():
        if not data:
            raise ReportError(f"{directory / name}: empty artifact")
        (directory / name).write_bytes(data)
    digests = {}
    for name, data in files.items():
        target = directory / name
        actual = target.read_bytes()
        if actual != data:
            raise ReportError(f"{target}: artifact bytes differ from generated output")
        digests[name] = hashlib.sha256(actual).hexdigest()
    manifest = {**asdict(comparison), "sha256": digests}
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    if destination := env.get("GITHUB_STEP_SUMMARY"):
        with Path(destination).open("a", encoding="utf-8") as stream:
            stream.write(summary + "\n")
