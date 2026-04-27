"""Check: production Rust files exceeding the giant-file LoC threshold."""

from __future__ import annotations

from typing import Iterable

from ..common import Finding, line_of, production_rust_files, read_text, rel_posix
from . import CheckSpec

THRESHOLD = 1000


def _run(allowlist: set[str]) -> Iterable[Finding]:
    findings: list[Finding] = []
    for path in production_rust_files():
        text = read_text(path)
        loc = text.count("\n") + (1 if text and not text.endswith("\n") else 0)
        if loc < THRESHOLD:
            continue
        rel = rel_posix(path)
        if rel in allowlist:
            continue
        findings.append(
            Finding(
                rule="giant_files",
                severity="warn",
                file=rel,
                line=None,
                message=f"{loc} LoC >= {THRESHOLD} threshold",
                extra={"loc": str(loc)},
            )
        )
    findings.sort(key=lambda f: -(int(f.extra.get("loc", "0"))))
    return findings


# Re-export for harness discovery.
_ = line_of  # quiet unused-import warnings if line_of becomes used later

CHECK = CheckSpec(
    key="giant_files",
    title="Giant files",
    description=f"Production Rust files in src/ with >= {THRESHOLD} lines.",
    hard_gate=False,
    runner=_run,
)
