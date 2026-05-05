#!/usr/bin/env python3
"""Maintainability audit harness for AgentDesk (#1282).

Runs the per-rule checks under ``scripts/audit_maintainability/checks/`` and
writes a structured report. Default output is YAML on stdout (sufficient for
``> target/maintainability-audit.yaml``); ``--format`` switches to JSON or
Markdown. ``--write-report`` mirrors the markdown form into
``docs/generated/maintainability-audit.md`` for review.

Stdlib only; Python 3.11+.

Hard gates
----------
Six checks are blocking in ``--check`` mode: direct Discord send regressions,
direct git subprocess regressions, runtime SQLite/legacy DB references,
source-of-truth alias writes, giant files missing from
``docs/agent-maintenance/change-surfaces.md``, and configured namespace size
cap regressions. Existing findings from the enablement commit are captured in
``scripts/audit_allowlist.toml``.
"""

from __future__ import annotations

import argparse
import importlib
import io
import json
import sys
from dataclasses import asdict
from pathlib import Path
from typing import Iterable

# Make the sibling package importable when invoked as a plain script.
_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))

from audit_maintainability.checks import CheckSpec  # noqa: E402
from audit_maintainability.common import REPO_ROOT, Finding  # noqa: E402

CHECK_MODULES = (
    "audit_maintainability.checks.giant_files",
    "audit_maintainability.checks.namespace_size_caps",
    "audit_maintainability.checks.route_srp",
    "audit_maintainability.checks.direct_discord_sends",
    "audit_maintainability.checks.manual_json_mapping",
    "audit_maintainability.checks.limit_clamp_duplication",
    "audit_maintainability.checks.git_subprocess",
    "audit_maintainability.checks.legacy_sqlite",
    "audit_maintainability.checks.source_of_truth_alias",
)

DEFAULT_ALLOWLIST_FILE = REPO_ROOT / "scripts" / "audit_allowlist.toml"
DEFAULT_REPORT_PATH = REPO_ROOT / "docs" / "generated" / "maintainability-audit.md"
DEFAULT_YAML_PATH = REPO_ROOT / "target" / "maintainability-audit.yaml"


def load_check_specs() -> list[CheckSpec]:
    specs: list[CheckSpec] = []
    for module_name in CHECK_MODULES:
        module = importlib.import_module(module_name)
        spec = getattr(module, "CHECK", None)
        if not isinstance(spec, CheckSpec):
            raise SystemExit(f"check module {module_name} did not export a CheckSpec")
        specs.append(spec)
    return specs


def load_allowlist(path: Path) -> dict[str, set[str]]:
    """Tiny TOML-ish loader; supports ``key = ["..."]`` arrays per rule.

    Avoids a tomllib dependency on the runtime path so the script works on
    Python <3.11 hosts too. Format is intentionally minimal::

        # comment
        giant_files = [
          "src/services/discord/recovery_engine.rs",
          "src/services/discord/tmux.rs:1255",
        ]

    Entries may be repo-relative paths or path:line findings. Unknown keys are
    kept as-is so the harness can warn on typos if desired.
    """

    if not path.is_file():
        return {}
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return {}
    out: dict[str, set[str]] = {}
    current_key: str | None = None
    in_array = False
    for raw_line in text.splitlines():
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        if "=" in line and "[" in line and not in_array:
            key = line.split("=", 1)[0].strip()
            current_key = key
            out.setdefault(current_key, set())
            in_array = "]" not in line
            after = line.split("[", 1)[1]
            for token in _extract_strings(after):
                out[current_key].add(token)
            continue
        if in_array and current_key is not None:
            for token in _extract_strings(line):
                out[current_key].add(token)
            if "]" in line:
                in_array = False
                current_key = None
    return out


def _extract_strings(fragment: str) -> Iterable[str]:
    # Naive but safe: pull double-quoted tokens.
    cursor = 0
    while True:
        start = fragment.find('"', cursor)
        if start < 0:
            return
        end = fragment.find('"', start + 1)
        if end < 0:
            return
        yield fragment[start + 1 : end]
        cursor = end + 1


def run_all(
    specs: list[CheckSpec], allowlist: dict[str, set[str]]
) -> dict[str, list[Finding]]:
    out: dict[str, list[Finding]] = {}
    for spec in specs:
        out[spec.key] = list(spec.runner(allowlist.get(spec.key, set())))
    return out


# ---------------------------------------------------------------------------
# Output formatters
# ---------------------------------------------------------------------------


def _yaml_escape(value: str) -> str:
    if value == "":
        return '""'
    if any(ch in value for ch in (":", "#", "`", "{", "}", "[", "]", ",", "&", "*", "?", "|", "<", ">", "=", "!", "%", "@", "\\")) or value.strip() != value:
        escaped = value.replace("\\", "\\\\").replace('"', '\\"')
        return f'"{escaped}"'
    return value


def render_yaml(specs: list[CheckSpec], findings: dict[str, list[Finding]]) -> str:
    hard_gate_keys = [spec.key for spec in specs if spec.hard_gate]
    buf = io.StringIO()
    buf.write("# Generated by scripts/audit_maintainability.py — do not edit by hand.\n")
    buf.write("schema_version: 1\n")
    buf.write(f"hard_gate_enabled: {'true' if hard_gate_keys else 'false'}\n")
    buf.write(f"hard_gate_count: {len(hard_gate_keys)}\n")
    buf.write("checks:\n")
    for spec in specs:
        hits = findings.get(spec.key, [])
        buf.write(f"  {spec.key}:\n")
        buf.write(f"    title: {_yaml_escape(spec.title)}\n")
        buf.write(f"    description: {_yaml_escape(spec.description)}\n")
        buf.write(f"    hard_gate: {'true' if spec.hard_gate else 'false'}\n")
        buf.write(f"    count: {len(hits)}\n")
        buf.write("    findings:\n")
        if not hits:
            buf.write("      []\n")
            continue
        for finding in hits:
            buf.write(f"      - file: {_yaml_escape(finding.file)}\n")
            if finding.line is not None:
                buf.write(f"        line: {finding.line}\n")
            buf.write(f"        severity: {finding.severity}\n")
            buf.write(f"        message: {_yaml_escape(finding.message)}\n")
            if finding.extra:
                buf.write("        extra:\n")
                for k, v in sorted(finding.extra.items()):
                    buf.write(f"          {k}: {_yaml_escape(str(v))}\n")
    return buf.getvalue()


def render_json(specs: list[CheckSpec], findings: dict[str, list[Finding]]) -> str:
    hard_gate_keys = [spec.key for spec in specs if spec.hard_gate]
    payload = {
        "schema_version": 1,
        "hard_gate_enabled": bool(hard_gate_keys),
        "hard_gate_count": len(hard_gate_keys),
        "checks": {
            spec.key: {
                "title": spec.title,
                "description": spec.description,
                "hard_gate": spec.hard_gate,
                "count": len(findings.get(spec.key, [])),
                "findings": [asdict(f) for f in findings.get(spec.key, [])],
            }
            for spec in specs
        },
    }
    return json.dumps(payload, indent=2, ensure_ascii=False) + "\n"


def render_markdown(specs: list[CheckSpec], findings: dict[str, list[Finding]]) -> str:
    hard_gate_keys = [spec.key for spec in specs if spec.hard_gate]
    buf = io.StringIO()
    buf.write("<!-- Generated by scripts/audit_maintainability.py — do not edit by hand. -->\n\n")
    buf.write("# Maintainability audit\n\n")
    buf.write(
        "Automated audit of giant files, route SRP violations, direct Discord "
        "sends, manual JSON row mapping, limit/days clamp duplication, git "
        "subprocess callsites, legacy SQLite references, source-of-truth "
        "alias writes, and namespace size caps. See "
        "`scripts/audit_maintainability.py` (#1282).\n\n"
    )
    if hard_gate_keys:
        buf.write(
            f"Hard-gating is **enabled** for {len(hard_gate_keys)} checks: "
            + ", ".join(f"`{key}`" for key in hard_gate_keys)
            + ".\n\n"
        )
    else:
        buf.write("Hard-gating is **disabled**; this report is informational.\n\n")
    buf.write("## Summary\n\n")
    buf.write("| Rule | Hits | Hard gate |\n")
    buf.write("|---|---:|:--:|\n")
    for spec in specs:
        hits = findings.get(spec.key, [])
        buf.write(
            f"| `{spec.key}` | {len(hits)} | {'YES' if spec.hard_gate else 'no'} |\n"
        )
    buf.write("\n")

    for spec in specs:
        hits = findings.get(spec.key, [])
        buf.write(f"## {spec.title} (`{spec.key}`)\n\n")
        buf.write(f"{spec.description}\n\n")
        if not hits:
            buf.write("_No findings._\n\n")
            continue
        # Sort by severity then file/line for readability.
        sev_order = {"error": 0, "warn": 1, "info": 2}
        ordered = sorted(
            hits,
            key=lambda f: (sev_order.get(f.severity, 9), f.file, f.line or 0),
        )
        buf.write("| Severity | File | Line | Message |\n")
        buf.write("|---|---|---:|---|\n")
        for finding in ordered:
            line_cell = "" if finding.line is None else str(finding.line)
            msg = finding.message.replace("|", "\\|")
            buf.write(
                f"| {finding.severity} | `{finding.file}` | {line_cell} | {msg} |\n"
            )
        buf.write("\n")
    return buf.getvalue()


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run maintainability audit checks (giant files, SRP, legacy, "
            "duplicate helpers, namespace caps, etc.) and emit a structured "
            "report."
        )
    )
    parser.add_argument(
        "--format",
        choices=("yaml", "json", "markdown"),
        default="yaml",
        help="output format on stdout (default: yaml)",
    )
    parser.add_argument(
        "--allowlist",
        type=Path,
        default=DEFAULT_ALLOWLIST_FILE,
        help=f"allowlist TOML path (default: {DEFAULT_ALLOWLIST_FILE.relative_to(REPO_ROOT)})",
    )
    parser.add_argument(
        "--write-report",
        action="store_true",
        help=(
            "also write the markdown report to "
            f"{DEFAULT_REPORT_PATH.relative_to(REPO_ROOT)}"
        ),
    )
    parser.add_argument(
        "--write-yaml",
        action="store_true",
        help=(
            "also write the YAML report to "
            f"{DEFAULT_YAML_PATH.relative_to(REPO_ROOT)}"
        ),
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help=(
            "exit non-zero if any hard-gated check has non-allowlisted findings."
        ),
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)

    specs = load_check_specs()
    allowlist = load_allowlist(args.allowlist)
    findings = run_all(specs, allowlist)

    if args.format == "yaml":
        rendered = render_yaml(specs, findings)
    elif args.format == "json":
        rendered = render_json(specs, findings)
    else:
        rendered = render_markdown(specs, findings)
    sys.stdout.write(rendered)

    if args.write_report:
        DEFAULT_REPORT_PATH.parent.mkdir(parents=True, exist_ok=True)
        DEFAULT_REPORT_PATH.write_text(render_markdown(specs, findings), encoding="utf-8")

    if args.write_yaml:
        DEFAULT_YAML_PATH.parent.mkdir(parents=True, exist_ok=True)
        DEFAULT_YAML_PATH.write_text(render_yaml(specs, findings), encoding="utf-8")

    if args.check:
        failed: list[tuple[CheckSpec, list[Finding]]] = []
        for spec in specs:
            if spec.hard_gate and findings.get(spec.key):
                failed.append((spec, findings[spec.key]))
        if failed:
            for spec, hits in failed:
                print(
                    f"audit_maintainability: hard-gate `{spec.key}` failed with "
                    f"{len(hits)} finding(s)",
                    file=sys.stderr,
                )
            return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
