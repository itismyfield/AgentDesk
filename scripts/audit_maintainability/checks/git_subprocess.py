"""Check: ad-hoc ``Command::new("git")`` callsites.

The repo has a ``services::git`` helper module that wraps git invocations.
Direct ``std::process::Command::new("git")`` (or ``tokio::process``) outside
that module obscures error handling and bypasses logging — flag for review.
"""

from __future__ import annotations

import re
from typing import Iterable

from ..common import Finding, line_of, read_text, rel_posix, strip_rust_comments
from . import CheckSpec

ALLOWED_PARENTS = (
    "src/services/git",
    "src/services/git.rs",
    "src/services/git_runner.rs",
)

PATTERN = re.compile(
    r"Command::new\(\s*\"git\"\s*\)"
    r"|\.new\(\s*\"git\"\s*\)\s*\.\s*args",
)


def _run(allowlist: set[str]) -> Iterable[Finding]:
    from ..common import production_rust_files

    findings: list[Finding] = []
    for path in production_rust_files():
        rel = rel_posix(path)
        if any(rel.startswith(parent) for parent in ALLOWED_PARENTS):
            continue
        if rel in allowlist:
            continue
        text = strip_rust_comments(read_text(path))
        for match in PATTERN.finditer(text):
            findings.append(
                Finding(
                    rule="git_subprocess_callsites",
                    severity="info",
                    file=rel,
                    line=line_of(text, match.start()),
                    message="direct git subprocess (consider services::git helper)",
                )
            )
    findings.sort(key=lambda f: (f.file, f.line or 0))
    return findings


CHECK = CheckSpec(
    key="git_subprocess_callsites",
    title="Direct git subprocess callsites",
    description=(
        "std::process::Command::new(\"git\") callsites outside src/services/git. "
        "Prefer the centralised git helper for consistent error handling."
    ),
    hard_gate=False,
    runner=_run,
)
