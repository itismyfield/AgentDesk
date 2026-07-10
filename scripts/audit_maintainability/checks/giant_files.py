"""Check: giant Rust files missing from the change-surfaces map.

The giant-file signal keys off *production* LoC (lines outside
``#[cfg(test)] mod`` blocks) so a module that is only large because of inline
test fixtures is not flagged or frozen (#3036). The production/test split is
shared with ``scripts/generate_inventory_docs.py`` so both surfaces agree.
"""

from __future__ import annotations

import importlib.util
import re
import sys
from pathlib import Path
from typing import Iterable

from .. import common
from ..common import (
    Finding,
    is_allowlisted,
    line_of,
    production_rust_files,
    read_text,
    rel_posix,
)
from . import CheckSpec
from .namespace_size_caps import (
    load_namespace_size_caps,
    matching_namespace_cap,
)

THRESHOLD = 1000


def _load_inventory_generator():
    name = "generate_inventory_docs"
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.spec_from_file_location(
        name,
        common.REPO_ROOT / "scripts" / "generate_inventory_docs.py",
    )
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    # Register before exec so the module's @dataclass definitions resolve their
    # own module namespace during import.
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


_INVENTORY = _load_inventory_generator()


def _test_only_module_files() -> set[Path]:
    """Files reached only through a test-gated parent ``mod`` declaration.

    Delegates to ``generate_inventory_docs.test_only_module_files`` while
    passing the audit's patchable file iterators. This keeps generator, audit,
    and ratchet on one test-only module graph instead of maintaining a parser
    mirror here (#3036/#4394).
    """

    return _INVENTORY.test_only_module_files(
        production_files=production_rust_files(),
        all_files=common.all_rust_files(),
        read_text_fn=read_text,
    )


def giant_production_loc() -> dict[str, int]:
    """Map each prod-giant file path to its production LoC.

    Reuses the generator's production/test split (including cross-file test-only
    subtree handling) so the audit, the inventory, and the giant-file registry
    agree on which files are production giants (#3036).
    """

    test_only = _test_only_module_files()
    giants: dict[str, int] = {}
    for path in production_rust_files():
        if path in test_only:
            continue
        prod, _test = _INVENTORY.split_prod_test_lines(read_text(path))
        if prod >= THRESHOLD:
            giants[rel_posix(path)] = prod
    return giants


CHANGE_SURFACES_DOC = "docs/agent-maintenance/change-surfaces.md"

_BACKTICK_SPAN = re.compile(r"`([^`]+)`", re.DOTALL)
_EXPLICIT_RS_PATH = re.compile(r"src/[A-Za-z0-9_./-]+\.rs")


def _clean_doc_path(path: str) -> str:
    path = " ".join(path.split())
    path = path.split("::", 1)[0]
    path = path.split(":", 1)[0]
    path = re.sub(r"\s*\([^)]*\)", "", path).strip()
    return path


def documented_change_surface_paths() -> set[str]:
    """Rust file paths named in change-surfaces.md.

    The maintenance page is intentionally prose-heavy, so this accepts both
    explicit backtick paths and compact brace groups such as
    ``src/dispatch/{mod,dispatch_context}.rs``.
    """

    path = common.REPO_ROOT / CHANGE_SURFACES_DOC
    text = read_text(path)
    documented: set[str] = set()
    for span_match in _BACKTICK_SPAN.finditer(text):
        span = span_match.group(1)
        compact = " ".join(span.split())
        if not compact.startswith("src/"):
            continue
        if "{" in compact and "}" in compact:
            prefix, rest = compact.split("{", 1)
            body, suffix = rest.split("}", 1)
            suffix = suffix.strip()
            for item in body.split(","):
                item = re.sub(r"\s*\([^)]*\)", "", item).strip()
                if not item:
                    continue
                candidate = _clean_doc_path(f"{prefix}{item}{suffix}")
                if _EXPLICIT_RS_PATH.fullmatch(candidate):
                    documented.add(candidate)
            continue
        for match in _EXPLICIT_RS_PATH.finditer(compact):
            documented.add(_clean_doc_path(match.group(0)))
    return documented


def _run(allowlist: set[str]) -> Iterable[Finding]:
    findings: list[Finding] = []
    documented = documented_change_surface_paths()
    namespace_caps = load_namespace_size_caps()
    for rel, loc in giant_production_loc().items():
        if matching_namespace_cap(rel, namespace_caps) is not None:
            continue
        if rel in documented or is_allowlisted(allowlist, rel):
            continue
        findings.append(
            Finding(
                rule="giant_files",
                severity="warn",
                file=rel,
                line=None,
                message=(
                    f"{loc} production LoC >= {THRESHOLD} threshold and is "
                    f"missing from {CHANGE_SURFACES_DOC}"
                ),
                extra={"loc": str(loc), "source_of_truth": CHANGE_SURFACES_DOC},
            )
        )
    findings.sort(key=lambda f: -(int(f.extra.get("loc", "0"))))
    return findings


# Re-export for harness discovery.
_ = line_of  # quiet unused-import warnings if line_of becomes used later

CHECK = CheckSpec(
    key="giant_files",
    title="Giant files",
    description=(
        f"Production Rust files in src/ with >= {THRESHOLD} lines that are not "
        f"listed in {CHANGE_SURFACES_DOC}."
    ),
    hard_gate=True,
    runner=_run,
)
