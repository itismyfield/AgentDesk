#!/usr/bin/env python3
"""Enforce the checked-in occurrence baseline for four structural Clippy lints."""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SOURCE_ROOT = REPO_ROOT / "src"
BASELINE = REPO_ROOT / "scripts" / "clippy_allow_occurrences.json"
LINTS = (
    "large_enum_variant",
    "result_large_err",
    "too_many_arguments",
    "type_complexity",
)
ATTRIBUTE_START_RE = re.compile(r"#!?\s*\[")
IDENTIFIER_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
LINT_RE = re.compile(r"\bclippy::(?P<lint>[a-zA-Z0-9_]+)\b")
# Treat every Clippy lint group as suppressing all governed lints. This is
# intentionally conservative: group membership changes across Clippy releases,
# and a broad allow must never bypass this occurrence ratchet.
CLIPPY_GROUPS = frozenset(
    {
        "all",
        "cargo",
        "complexity",
        "correctness",
        "nursery",
        "pedantic",
        "perf",
        "restriction",
        "style",
        "suspicious",
    }
)


def rust_attributes(text: str) -> list[str]:
    """Return balanced Rust attributes, including cfg_attr nested calls."""
    attributes: list[str] = []
    cursor = 0
    while match := ATTRIBUTE_START_RE.search(text, cursor):
        depth = 1
        index = match.end()
        in_string = False
        escaped = False
        while index < len(text) and depth:
            char = text[index]
            if in_string:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    in_string = False
            elif char == '"':
                in_string = True
            elif char == "[":
                depth += 1
            elif char == "]":
                depth -= 1
            index += 1
        if depth:
            break
        attributes.append(text[match.start() : index])
        cursor = index
    return attributes


def suppression_bodies(attribute: str) -> list[str]:
    """Extract balanced allow/expect argument lists from one Rust attribute."""
    bodies: list[str] = []
    index = 0
    while index < len(attribute):
        identifier = IDENTIFIER_RE.match(attribute, index)
        if identifier is None:
            index += 1
            continue
        name = identifier.group()
        index = identifier.end()
        if name not in {"allow", "expect"}:
            continue
        while index < len(attribute) and attribute[index].isspace():
            index += 1
        if index >= len(attribute) or attribute[index] != "(":
            continue
        body_start = index + 1
        depth = 1
        index += 1
        in_string = False
        escaped = False
        while index < len(attribute) and depth:
            char = attribute[index]
            if in_string:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    in_string = False
            elif char == '"':
                in_string = True
            elif char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
            index += 1
        if depth == 0:
            bodies.append(attribute[body_start : index - 1])
    return bodies


def collect_occurrences(source_root: Path = SOURCE_ROOT) -> Counter[tuple[str, str]]:
    occurrences: Counter[tuple[str, str]] = Counter()
    for path in sorted(source_root.rglob("*.rs")):
        relative = path.relative_to(REPO_ROOT).as_posix()
        text = path.read_text(encoding="utf-8")
        for attribute in rust_attributes(text):
            for body in suppression_bodies(attribute):
                for lint_match in LINT_RE.finditer(body):
                    lint = lint_match.group("lint")
                    governed = LINTS if lint in CLIPPY_GROUPS else (lint,)
                    for governed_lint in governed:
                        if governed_lint in LINTS:
                            occurrences[(relative, governed_lint)] += 1
    return occurrences


def load_baseline(path: Path = BASELINE) -> Counter[tuple[str, str]]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("lints") != list(LINTS):
        raise ValueError("baseline lint set/order must exactly match the four governed lints")
    entries = payload.get("occurrences")
    if not isinstance(entries, list):
        raise ValueError("baseline occurrences must be a list")
    result: Counter[tuple[str, str]] = Counter()
    for entry in entries:
        key = (entry.get("path"), entry.get("lint"))
        count = entry.get("count")
        if (
            not isinstance(key[0], str)
            or key[1] not in LINTS
            or not isinstance(count, int)
            or isinstance(count, bool)
            or count <= 0
            or key in result
        ):
            raise ValueError(f"invalid or duplicate baseline occurrence: {entry!r}")
        result[key] = count
    return result


def validate_occurrences(
    actual: Counter[tuple[str, str]], baseline: Counter[tuple[str, str]]
) -> list[str]:
    problems: list[str] = []
    for key, count in sorted(actual.items()):
        allowed = baseline.get(key, 0)
        if count > allowed:
            path, lint = key
            problems.append(
                f"{path}: clippy::{lint} has {count} allow/expect occurrence(s), baseline {allowed}"
            )
    return problems


def write_baseline(actual: Counter[tuple[str, str]], path: Path = BASELINE) -> None:
    payload = {
        "schema_version": 1,
        "lints": list(LINTS),
        "occurrences": [
            {"path": source, "lint": lint, "count": count}
            for (source, lint), count in sorted(actual.items())
        ],
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true", help="replace baseline with current occurrences")
    args = parser.parse_args()
    actual = collect_occurrences()
    if args.write:
        write_baseline(actual)
        print(f"wrote {BASELINE.relative_to(REPO_ROOT)} ({sum(actual.values())} occurrences)")
        return 0
    try:
        baseline = load_baseline()
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"clippy allow ratchet baseline error: {error}")
        return 1
    problems = validate_occurrences(actual, baseline)
    if problems:
        print("clippy allow occurrence ratchet failed:")
        for problem in problems:
            print(f"  - {problem}")
        return 1
    print(
        "clippy allow occurrence ratchet passed "
        f"({sum(actual.values())}/{sum(baseline.values())} occurrences)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
