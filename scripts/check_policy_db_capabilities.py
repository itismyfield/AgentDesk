#!/usr/bin/env python3
"""Static guard for policy raw-DB capability manifests.

The first rollout slice is intentionally audit-oriented: manifest-enabled
policies may keep legacy broad raw SQL access, but CI pins the existing
callsite set so new agentdesk.db.* usage cannot grow silently.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import re
import shlex
import sys
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[1]
POLICIES_ROOT = REPO_ROOT / "policies"
LEGACY_BROAD_MODES = {"legacy"}
ALLOWED_RAW_MODES = {"forbidden", "audited", "transitional", *LEGACY_BROAD_MODES}
RAW_DB_RE = re.compile(r"agentdesk\.db\.(query|execute)\s*\(")
MARKER_RE = re.compile(r"legacy-raw-db:\s*([^*\n]+)")


@dataclasses.dataclass(frozen=True)
class Callsite:
    path: Path
    rel_path: str
    line: int
    op: str
    expression: str
    marker: dict[str, str]

    @property
    def fingerprint(self) -> str:
        canonical = " ".join(self.expression.split())
        payload = f"{self.rel_path}\0{self.op}\0{canonical}".encode("utf-8")
        return hashlib.sha256(payload).hexdigest()


@dataclasses.dataclass(frozen=True)
class Manifest:
    path: Path
    data: dict[str, Any]
    js_path: Path
    rel_js_path: str
    expected_policy: str

    @property
    def policy(self) -> str:
        return str(self.data.get("policy", ""))

    @property
    def raw_sql(self) -> dict[str, Any]:
        db = self.data.get("db")
        if not isinstance(db, dict):
            return {}
        raw_sql = db.get("raw_sql")
        return raw_sql if isinstance(raw_sql, dict) else {}

    @property
    def raw_mode(self) -> str:
        return str(self.raw_sql.get("mode", "")).strip().lower()

    @property
    def is_broad_legacy(self) -> bool:
        return self.raw_mode in LEGACY_BROAD_MODES

    @property
    def no_silent_growth(self) -> dict[str, Any]:
        baseline = self.raw_sql.get("no_silent_growth")
        return baseline if isinstance(baseline, dict) else {}

    @property
    def raw_capabilities(self) -> list[str]:
        capabilities = self.raw_sql.get("capabilities")
        if isinstance(capabilities, list):
            return [str(capability) for capability in capabilities]
        return []


def parse_scalar(value: str) -> Any:
    value = value.strip()
    if value == "":
        return ""
    if value in {"true", "True"}:
        return True
    if value in {"false", "False"}:
        return False
    if value in {"null", "Null", "~"}:
        return None
    if value == "[]":
        return []
    if value == "{}":
        return {}
    if (value.startswith('"') and value.endswith('"')) or (
        value.startswith("'") and value.endswith("'")
    ):
        return value[1:-1]
    try:
        return int(value)
    except ValueError:
        return value


def preprocess_yaml(text: str) -> list[tuple[int, str]]:
    lines: list[tuple[int, str]] = []
    for raw in text.splitlines():
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        lines.append((indent, raw.strip()))
    return lines


def parse_yaml_subset(text: str) -> dict[str, Any]:
    lines = preprocess_yaml(text)
    if not lines:
        return {}
    node, index = parse_yaml_node(lines, 0, lines[0][0])
    if index != len(lines):
        raise ValueError(f"Could not parse YAML near line {index + 1}: {lines[index][1]}")
    if not isinstance(node, dict):
        raise ValueError("Manifest root must be a mapping")
    return node


def parse_yaml_node(
    lines: list[tuple[int, str]], index: int, indent: int
) -> tuple[Any, int]:
    if index >= len(lines):
        return {}, index
    current_indent, content = lines[index]
    if current_indent < indent:
        return {}, index
    if current_indent != indent:
        raise ValueError(f"Unexpected indentation before: {content}")
    if content.startswith("- "):
        return parse_yaml_list(lines, index, indent)
    return parse_yaml_mapping(lines, index, indent)


def parse_yaml_list(
    lines: list[tuple[int, str]], index: int, indent: int
) -> tuple[list[Any], int]:
    result: list[Any] = []
    while index < len(lines):
        current_indent, content = lines[index]
        if current_indent < indent:
            break
        if current_indent != indent or not content.startswith("- "):
            break
        item = content[2:].strip()
        index += 1
        if item == "":
            child, index = parse_yaml_node(lines, index, indent + 2)
            result.append(child)
        elif ":" in item and not item.startswith(("'", '"')):
            key, value = item.split(":", 1)
            item_dict: dict[str, Any] = {key.strip(): parse_scalar(value)}
            if index < len(lines) and lines[index][0] > indent:
                child, index = parse_yaml_node(lines, index, lines[index][0])
                if isinstance(child, dict):
                    item_dict.update(child)
                else:
                    raise ValueError("List item mapping child must be a mapping")
            result.append(item_dict)
        else:
            result.append(parse_scalar(item))
    return result, index


def parse_yaml_mapping(
    lines: list[tuple[int, str]], index: int, indent: int
) -> tuple[dict[str, Any], int]:
    result: dict[str, Any] = {}
    while index < len(lines):
        current_indent, content = lines[index]
        if current_indent < indent:
            break
        if current_indent != indent or content.startswith("- "):
            break
        if ":" not in content:
            raise ValueError(f"Expected mapping entry, got: {content}")
        key, value = content.split(":", 1)
        key = key.strip()
        value = value.strip()
        index += 1
        if value:
            result[key] = parse_scalar(value)
        elif index < len(lines) and lines[index][0] > indent:
            child, index = parse_yaml_node(lines, index, lines[index][0])
            result[key] = child
        else:
            result[key] = {}
    return result, index


def parse_marker(marker_text: str) -> dict[str, str]:
    marker: dict[str, str] = {}
    for token in shlex.split(marker_text):
        if "=" not in token:
            continue
        key, value = token.split("=", 1)
        if key in {"policy_name", "event"}:
            key = {"policy_name": "policy", "event": "source_event"}[key]
        marker[key] = value
    return marker


def marker_in_call_expression(expression: str) -> dict[str, str]:
    literal_text = "\n".join(extract_string_literals(first_call_argument(expression)))
    matches = MARKER_RE.findall(literal_text)
    if not matches:
        return {}
    return parse_marker(matches[-1])


def scan_callsites(path: Path, repo_root: Path = REPO_ROOT) -> list[Callsite]:
    repo_root = repo_root.resolve()
    path = (repo_root / path).resolve() if not path.is_absolute() else path.resolve()
    text = path.read_text(encoding="utf-8")
    rel_path = path.relative_to(repo_root).as_posix()
    callsites: list[Callsite] = []
    for match in RAW_DB_RE.finditer(text):
        op = match.group(1)
        expression = extract_call_expression(text, match.start(), match.end() - 1)
        line = text.count("\n", 0, match.start()) + 1
        callsites.append(
            Callsite(
                path=path,
                rel_path=rel_path,
                line=line,
                op=op,
                expression=expression,
                marker=marker_in_call_expression(expression),
            )
        )
    return callsites


def first_call_argument(expression: str) -> str:
    open_paren = expression.find("(")
    if open_paren < 0:
        return expression
    quote: str | None = None
    comment: str | None = None
    escaped = False
    nested = 0
    i = open_paren + 1
    while i < len(expression):
        ch = expression[i]
        nxt = expression[i + 1] if i + 1 < len(expression) else ""
        if comment == "line":
            if ch == "\n":
                comment = None
            i += 1
            continue
        if comment == "block":
            if ch == "*" and nxt == "/":
                comment = None
                i += 2
                continue
            i += 1
            continue
        if quote:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == quote:
                quote = None
            i += 1
            continue
        if ch == "/" and nxt == "/":
            comment = "line"
            i += 2
            continue
        if ch == "/" and nxt == "*":
            comment = "block"
            i += 2
            continue
        if ch in {"'", '"', "`"}:
            quote = ch
            i += 1
            continue
        if ch in "([{":
            nested += 1
        elif ch in ")]}":
            if ch == ")" and nested == 0:
                return expression[open_paren + 1 : i]
            nested = max(0, nested - 1)
        elif ch == "," and nested == 0:
            return expression[open_paren + 1 : i]
        i += 1
    return expression[open_paren + 1 :]


def extract_string_literals(text: str) -> list[str]:
    literals: list[str] = []
    quote: str | None = None
    comment: str | None = None
    escaped = False
    buf: list[str] = []
    i = 0
    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""
        if comment == "line":
            if ch == "\n":
                comment = None
            i += 1
            continue
        if comment == "block":
            if ch == "*" and nxt == "/":
                comment = None
                i += 2
                continue
            i += 1
            continue
        if quote:
            if escaped:
                buf.append(ch)
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == quote:
                literals.append("".join(buf))
                buf = []
                quote = None
            else:
                buf.append(ch)
            i += 1
            continue
        if ch == "/" and nxt == "/":
            comment = "line"
            i += 2
            continue
        if ch == "/" and nxt == "*":
            comment = "block"
            i += 2
            continue
        if ch in {"'", '"', "`"}:
            quote = ch
            buf = []
        i += 1
    return literals


def extract_call_expression(text: str, start: int, open_paren: int) -> str:
    depth = 0
    quote: str | None = None
    comment: str | None = None
    escaped = False
    i = open_paren
    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""
        if comment == "line":
            if ch == "\n":
                comment = None
            i += 1
            continue
        if comment == "block":
            if ch == "*" and nxt == "/":
                comment = None
                i += 2
                continue
            i += 1
            continue
        if quote:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == quote:
                quote = None
            i += 1
            continue
        if ch == "/" and nxt == "/":
            comment = "line"
            i += 2
            continue
        if ch == "/" and nxt == "*":
            comment = "block"
            i += 2
            continue
        if ch in {"'", '"', "`"}:
            quote = ch
            i += 1
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return text[start : i + 1]
        i += 1
    raise ValueError(f"Unterminated agentdesk.db call starting near byte {start}")


def manifest_policy_from_path(manifest_path: Path, policies_root: Path) -> str:
    rel = manifest_path.relative_to(policies_root).as_posix()
    if not rel.endswith(".cap.yaml"):
        raise ValueError(f"Manifest must end with .cap.yaml: {manifest_path}")
    return rel[: -len(".cap.yaml")]


def load_manifest(path: Path, policies_root: Path, repo_root: Path) -> Manifest:
    data = parse_yaml_subset(path.read_text(encoding="utf-8"))
    policy_from_path = manifest_policy_from_path(path, policies_root)
    js_path = policies_root / f"{policy_from_path}.js"
    return Manifest(
        path=path,
        data=data,
        js_path=js_path,
        rel_js_path=js_path.relative_to(repo_root).as_posix(),
        expected_policy=policy_from_path,
    )


def load_manifests(policies_root: Path, repo_root: Path) -> list[Manifest]:
    return [
        load_manifest(path, policies_root, repo_root)
        for path in sorted(policies_root.rglob("*.cap.yaml"))
    ]


def manifest_scan_paths(manifest: Manifest, repo_root: Path) -> list[Path]:
    paths = [manifest.js_path]
    include_files = manifest.data.get("include_files", [])
    if not isinstance(include_files, list):
        return paths
    for include_file in include_files:
        paths.append(repo_root / str(include_file))
    return paths


def callsite_baseline(callsites: list[Callsite]) -> tuple[int, str]:
    fingerprints = sorted(callsite.fingerprint for callsite in callsites)
    payload = json.dumps(fingerprints, separators=(",", ":"), sort_keys=True).encode("utf-8")
    return len(callsites), "sha256:" + hashlib.sha256(payload).hexdigest()


def validate_manifest_shape(manifest: Manifest, repo_root: Path) -> list[str]:
    errors: list[str] = []
    if manifest.policy != manifest.expected_policy:
        errors.append(
            f"{manifest.path}: policy must be '{manifest.expected_policy}' to match {manifest.rel_js_path}"
        )
    if not manifest.path.name.endswith(".cap.yaml"):
        errors.append(f"{manifest.path}: manifest filename must end with .cap.yaml")
    if manifest.data.get("version") != 1:
        errors.append(f"{manifest.path}: version must be 1")
    if manifest.data.get("trust") != "trusted-automation":
        errors.append(f"{manifest.path}: trust must be trusted-automation")
    events = manifest.data.get("source_events")
    if not isinstance(events, list) or not events or not all(isinstance(e, str) for e in events):
        errors.append(f"{manifest.path}: source_events must be a non-empty list of strings")
    include_files = manifest.data.get("include_files", [])
    if include_files != [] and (
        not isinstance(include_files, list) or not all(isinstance(p, str) for p in include_files)
    ):
        errors.append(f"{manifest.path}: include_files must be a list of repo-relative paths")
    elif isinstance(include_files, list):
        for include_file in include_files:
            include_path = repo_root / include_file
            if not include_path.exists():
                errors.append(f"{manifest.path}: include file does not exist: {include_file}")
    if not manifest.raw_mode:
        errors.append(f"{manifest.path}: db.raw_sql.mode is required")
    elif manifest.raw_mode not in ALLOWED_RAW_MODES:
        errors.append(
            f"{manifest.path}: db.raw_sql.mode={manifest.raw_mode!r} is not one of "
            f"{', '.join(sorted(ALLOWED_RAW_MODES))}"
        )
    if manifest.raw_mode != "forbidden" and not manifest.raw_capabilities:
        errors.append(f"{manifest.path}: db.raw_sql.capabilities must declare at least one capability")
    if not manifest.js_path.exists():
        errors.append(f"{manifest.path}: described JS policy file does not exist: {manifest.js_path}")
    return errors


def validate_callsites(
    manifest: Manifest,
    callsites: list[Callsite],
    no_silent_growth: bool,
) -> list[str]:
    errors: list[str] = []
    if manifest.raw_mode == "forbidden":
        for callsite in callsites:
            errors.append(
                f"{callsite.rel_path}:{callsite.line}: agentdesk.db.{callsite.op} is not allowed "
                f"because {manifest.path} sets db.raw_sql.mode=forbidden"
            )
        return errors
    if manifest.is_broad_legacy:
        if no_silent_growth:
            expected_count = manifest.no_silent_growth.get("callsites")
            expected_fingerprint = manifest.no_silent_growth.get("fingerprint")
            current_count, current_fingerprint = callsite_baseline(callsites)
            if expected_count != current_count or expected_fingerprint != current_fingerprint:
                errors.append(
                    f"{manifest.path}: raw DB baseline drift for {manifest.rel_js_path}; "
                    f"expected callsites={expected_count} fingerprint={expected_fingerprint}, "
                    f"got callsites={current_count} fingerprint={current_fingerprint}. "
                    "Review the diff and update db.raw_sql.no_silent_growth only for intentional changes."
                )
        return errors

    for callsite in callsites:
        marker = callsite.marker
        missing = [field for field in ("policy", "capability", "source_event") if not marker.get(field)]
        if missing:
            errors.append(
                f"{callsite.rel_path}:{callsite.line}: unmarked agentdesk.db.{callsite.op} "
                f"for manifest-enabled policy {manifest.policy}; missing {', '.join(missing)}"
            )
            continue
        if marker.get("policy") != manifest.policy:
            errors.append(
                f"{callsite.rel_path}:{callsite.line}: legacy-raw-db policy={marker.get('policy')} "
                f"does not match manifest policy={manifest.policy}"
            )
        if marker.get("capability") not in set(manifest.raw_capabilities):
            errors.append(
                f"{callsite.rel_path}:{callsite.line}: legacy-raw-db capability={marker.get('capability')} "
                f"is not declared in {manifest.path}"
            )
        events = manifest.data.get("source_events")
        if isinstance(events, list) and marker.get("source_event") not in set(events):
            errors.append(
                f"{callsite.rel_path}:{callsite.line}: legacy-raw-db source_event={marker.get('source_event')} "
                f"is not declared in {manifest.path}"
            )
    return errors


def emit_baselines(manifests: list[Manifest], repo_root: Path) -> int:
    for manifest in manifests:
        callsites: list[Callsite] = []
        for path in manifest_scan_paths(manifest, repo_root):
            callsites.extend(scan_callsites(path, repo_root))
        count, fingerprint = callsite_baseline(callsites)
        print(f"{manifest.path.relative_to(repo_root).as_posix()}:")
        print("  no_silent_growth:")
        print(f"    callsites: {count}")
        print(f"    fingerprint: {fingerprint}")
    return 0


def run_check(args: argparse.Namespace) -> int:
    repo_root = args.repo_root.resolve()
    policies_root = (repo_root / args.policies_root).resolve()
    manifests = load_manifests(policies_root, repo_root)
    manifest_rel_paths = {manifest.path.relative_to(repo_root).as_posix() for manifest in manifests}
    for required in args.require_manifest:
        if required not in manifest_rel_paths:
            print(
                f"policy DB capability check failed:\n"
                f"  - required manifest is missing: {required}",
                file=sys.stderr,
            )
            return 1
    if args.emit_baseline:
        return emit_baselines(manifests, repo_root)
    errors: list[str] = []
    checked_callsites = 0
    for manifest in manifests:
        errors.extend(validate_manifest_shape(manifest, repo_root))
        scan_paths = manifest_scan_paths(manifest, repo_root)
        if not all(path.exists() for path in scan_paths):
            continue
        callsites: list[Callsite] = []
        for path in scan_paths:
            callsites.extend(scan_callsites(path, repo_root))
        checked_callsites += len(callsites)
        errors.extend(validate_callsites(manifest, callsites, args.no_silent_growth))
    if errors:
        print("policy DB capability check failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print(
        "policy DB capability check passed: "
        f"{len(manifests)} manifest(s), {checked_callsites} raw DB callsite(s) checked"
    )
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help="Repository root. Defaults to the parent of scripts/.",
    )
    parser.add_argument(
        "--policies-root",
        default="policies",
        help="Policies directory relative to --repo-root.",
    )
    parser.add_argument(
        "--no-silent-growth",
        action="store_true",
        help="Require broad legacy manifests to match their checked-in callsite baseline.",
    )
    parser.add_argument(
        "--emit-baseline",
        action="store_true",
        help="Print no_silent_growth baseline snippets for current manifest-enabled policies.",
    )
    parser.add_argument(
        "--require-manifest",
        action="append",
        default=[],
        metavar="PATH",
        help="Require a specific manifest path relative to --repo-root. May be repeated.",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return run_check(args)


if __name__ == "__main__":
    raise SystemExit(main())
