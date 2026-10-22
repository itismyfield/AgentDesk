#!/usr/bin/env python3
"""Audit ignored libtest subprocesses and bind inherited output to its parent."""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
REL = Path("scripts/ci/nested_libtest_declarations.json")
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rust_lex import CODE, COMMENT, LITERAL, StripState, lex_segments

MODES = {"inherited", "null", "captured", "file", "piped"}
ID = re.compile(r"\w+(?:::\w+)+\Z")


def _object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate declaration key: {key}")
        result[key] = value
    return result


def _source(value):
    if not isinstance(value, str) or not value.startswith("src/") or not value.endswith(".rs") \
            or ".." in Path(value).parts:
        raise ValueError(f"invalid declaration source: {value!r}")


def declarations(root: Path, path: Path | None = None) -> list[dict]:
    payload = json.loads((path or root / REL).read_text(), object_pairs_hook=_object)
    if not isinstance(payload, dict) or set(payload) != {"schema", "children"} \
            or type(payload["schema"]) is not int or payload["schema"] != 1 \
            or not isinstance(payload["children"], list):
        raise ValueError("nested libtest declarations require schema 1 and children list")
    seen = set()
    for child in payload["children"]:
        if not isinstance(child, dict) or set(child) != {"id", "source", "platform", "parents", "calls"}:
            raise ValueError("invalid nested libtest child fields")
        name = child["id"]
        if not isinstance(name, str) or not ID.fullmatch(name) or name in seen:
            raise ValueError(f"invalid or duplicate nested child: {name!r}")
        seen.add(name)
        _source(child["source"])
        parents, calls = child["parents"], child["calls"]
        if child["platform"] not in {"linux", "windows"} or not isinstance(parents, list) \
                or any(not isinstance(p, str) or not ID.fullmatch(p) or p == name for p in parents) \
                or len(set(parents)) != len(parents) or not isinstance(calls, list) or not calls:
            raise ValueError(f"invalid child platform, parents or calls: {name}")
        keys = set()
        for call in calls:
            if not isinstance(call, dict) or set(call) != {"source", "function", "stdout", "sha256", "ignored_args"}:
                raise ValueError(f"invalid call fields: {name}")
            _source(call["source"])
            if not isinstance(call["function"], str) or not re.fullmatch(r"\w+", call["function"]) \
                    or call["stdout"] not in MODES or not isinstance(call["sha256"], str) \
                    or not re.fullmatch(r"[0-9a-f]{64}", call["sha256"]) \
                    or type(call["ignored_args"]) is not int or call["ignored_args"] < 1:
                raise ValueError(f"invalid call declaration: {name}")
            key = call["source"], call["function"]
            if key in keys:
                raise ValueError(f"duplicate child call: {name} {key}")
            keys.add(key)
        inherited = any(call["stdout"] == "inherited" for call in calls)
        if bool(parents) != inherited:
            raise ValueError(f"only inherited children require exact parents: {name}")
    return payload["children"]


def inherited_pairs(root: Path, path: Path | None = None) -> tuple[tuple[str, str], ...]:
    return tuple(sorted((parent, child["id"]) for child in declarations(root, path)
                        for parent in child["parents"]))


def validate_execution(root: Path, expected: set[str], executed: set[str],
                       path: Path | None = None) -> list[str]:
    parents: dict[str, set[str]] = {}
    for parent, child in inherited_pairs(root, path):
        parents.setdefault(child, set()).add(parent)
    return [f"nested child {child} executed without a declared parent both expected and executed"
            for child, owners in sorted(parents.items())
            if child in executed and not (owners & expected & executed)]


def functions(text: str) -> tuple[dict, list[int]]:
    """Lexical function bounds and exact --ignored literals, excluding comments/prose."""
    state, masked, clean, markers, offset = StripState(), [], [], [], 0
    for line in text.splitlines(keepends=True):
        for kind, value in lex_segments(line, state):
            masked.append(value if kind == CODE else " " * len(value))
            clean.append(value if kind != COMMENT else " " * len(value))
            if kind == LITERAL and value == '"--ignored"':
                markers.append(offset)
            offset += len(value)
    masked, clean = "".join(masked), "".join(clean)
    found = {}
    for match in re.finditer(r"\bfn\s+(\w+)\s*(?:<[^;{}]*>)?\s*\(", masked):
        start = masked.find("{", match.end())
        if start < 0 or ";" in masked[match.end():start]:
            continue
        depth, end = 1, start + 1
        while depth and end < len(masked):
            depth += (masked[end] == "{") - (masked[end] == "}")
            end += 1
        if depth:
            raise ValueError(f"unbalanced Rust function: {match.group(1)}")
        body = clean[match.start():end]
        compact_state, tokens = StripState(), []
        for line in body.splitlines(keepends=True):
            for kind, value in lex_segments(line, compact_state):
                if kind == CODE:
                    tokens.append(re.sub(r"\s+", "", value))
                elif kind == LITERAL:
                    tokens.append(value)
        compact = "".join(tokens)
        found[match.start(), end] = {
            "function": match.group(1), "sha256": hashlib.sha256(compact.encode()).hexdigest(),
            "compact": compact, "start": match.start(), "end": end,
        }
    return found, markers


def own_attributes(text: str, function_start: int) -> list[list[str]]:
    from check_test_target_integrity import _rust_tokens
    tokens = _rust_tokens(text)
    cursor = next(index for index, token in enumerate(tokens) if token.start == function_start) - 1
    while cursor >= 0 and tokens[cursor].value in {"async", "unsafe", "const", "pub"}:
        cursor -= 1
    attributes = []
    while cursor >= 0 and tokens[cursor].value == "]":
        end, depth = cursor, 1
        cursor -= 1
        while cursor >= 0 and depth:
            depth += (tokens[cursor].value == "]") - (tokens[cursor].value == "[")
            cursor -= 1
        if depth or cursor < 0 or tokens[cursor].value != "#":
            break
        attributes.append([token.value for token in tokens[cursor + 2:end]])
        cursor -= 1
    return attributes


def _transport(compact: str) -> str:
    if re.search(r"\.stdout\((?:std::process::)?Stdio::null\(\)\)", compact):
        return "null"
    if re.search(r"\.stdout\((?:std::process::)?Stdio::piped\(\)\)", compact):
        return "piped"
    if ".stdout(out.try_clone().unwrap())" in compact and "File::create(" in compact:
        return "file"
    if ".stdout(" in compact and ".stdout(Stdio::inherit())" not in compact \
            and ".stdout(std::process::Stdio::inherit())" not in compact:
        raise ValueError("unclassified child stdout expression")
    return "captured" if ".output()" in compact else "inherited"


def source_calls(root: Path) -> dict[tuple[str, str], dict]:
    found = {}
    for path in sorted((root / "src").rglob("*.rs")):
        text = path.read_text()
        if '"--ignored"' not in text:
            continue
        bounds, markers = functions(text)
        for marker in markers:
            containers = [item for (start, end), item in bounds.items() if start < marker < end]
            if not containers:
                raise ValueError(f"--ignored literal outside an audited function: {path}")
            owner = min(containers, key=lambda item: item["end"] - item["start"])
            key = path.relative_to(root).as_posix(), owner["function"]
            record = found.setdefault(key, {"sha256": owner["sha256"], "ignored_args": 0,
                                            "stdout": _transport(owner["compact"])})
            record["ignored_args"] += 1
    return found


def names_child(compact: str, text: str, name: str) -> bool:
    literal = json.dumps(name)
    if literal in compact:
        return True
    for match in re.finditer(r"concat!\(([^()]*)\)", compact):
        values = re.findall(r'"(?:[^"\\]|\\.)*"', match.group(1))
        if values and "".join(json.loads(value) for value in values) == name:
            return True
    for match in re.finditer(r"\bconst\s+(\w+)\s*:[^;=]+=\s*" + re.escape(literal) + r"\s*;", text):
        if re.search(r"\b" + re.escape(match.group(1)) + r"\b", compact):
            return True
    leaf = name.rsplit("::", 1)[1]
    return (json.dumps("{parent}::" + leaf) in compact
            and 'CHILD.rsplit_once("::harness::")' in compact
            and bool(re.search(r'const\s+CHILD\s*:[^;=]+=\s*concat!\(module_path!\(\),\s*'
                               + re.escape(json.dumps("::" + leaf)) + r'\);', text)))


def check(root: Path, path: Path | None = None) -> list[str]:
    from check_test_target_integrity import collect_static_tests, load_lib_inventory_manifest
    from check_test_lane_coverage import load_non_pg_filter_replay, load_non_pg_skip_args
    from check_pg_test_lane_membership import pg_test_shard
    from run_test_lane import derive_expected_selection, parse_ledger
    children = declarations(root, path)
    manifest = load_lib_inventory_manifest(root / "scripts/lib_test_inventory_manifest.txt")
    runtime, errors = derive_expected_selection(root / "scripts/lib_test_inventory_manifest.txt", "linux", [])
    ignored, ignored_by_lane = set(), {}
    for lane in ("pg", "non-pg-sweep"):
        entries, problems = parse_ledger(root / "scripts/known_test_failures.txt", lane, datetime.date.today())
        errors.extend(problems)
        ignored_by_lane[lane] = {entry.pattern for entry in entries if entry.mode == "ignored"}
        ignored.update(ignored_by_lane[lane])
    calls, declared = source_calls(root), {}
    static = collect_static_tests(root / "src/lib.rs", root)
    for child in children:
        name, source = child["id"], root / child["source"]
        bounds, _ = functions(source.read_text())
        if static.tests.get(name, "").rsplit(":", 1)[0] != child["source"]:
            errors.append(f"child inventory source differs from declaration: {name}")
        definitions = [item for item in bounds.values() if item["function"] == name.rsplit("::", 1)[1]]
        if len(definitions) != 1:
            errors.append(f"missing or ambiguous child definition: {name}")
        else:
            attributes = own_attributes(source.read_text(), definitions[0]["start"])
            if not any(attr and attr[0] == "ignore" for attr in attributes):
                errors.append(f"child definition is not ignored: {name}")
            if child["platform"] == "windows" and ["cfg", "(", "windows", ")"] not in attributes:
                errors.append(f"excluded child lacks cfg(windows): {name}")
        if child["platform"] == "linux" and name not in manifest:
            errors.append(f"child absent from inventory: {name}")
        if (name in runtime or child["parents"]) and name not in ignored:
            errors.append(f"child absent from ignored ledger: {name}")
        actual_parents = set()
        for call in child["calls"]:
            if call["stdout"] != "inherited":
                continue
            call_text = (root / call["source"]).read_text()
            owners, _ = functions(call_text)
            for owner in owners.values():
                if owner["function"] == call["function"]:
                    site = f"{call['source']}:{call_text[:owner['start']].count(chr(10)) + 1}"
                    actual_parents.update(test for test, location in static.tests.items() if location == site)
        if set(child["parents"]) != actual_parents:
            errors.append(f"declared parents do not match inherited test call owners: {name}")
        for parent in child["parents"]:
            if parent not in manifest:
                errors.append(f"parent absent from inventory: {parent}")
        for call in child["calls"]:
            key = call["source"], call["function"]
            call_text = (root / call["source"]).read_text()
            owners, _ = functions(call_text)
            if not any(owner["function"] == call["function"] and names_child(owner["compact"], call_text, name)
                       for owner in owners.values()):
                errors.append(f"declared child is not named by its spawn function: {name} {key}")
            value = {field: call[field] for field in ("sha256", "stdout", "ignored_args")}
            if key in declared:
                if any(declared[key][field] != value[field] for field in ("sha256", "stdout")):
                    errors.append(f"conflicting call declarations: {key}")
                declared[key]["ignored_args"] += value["ignored_args"]
            else:
                declared[key] = value
    for key in sorted(calls.keys() | declared.keys()):
        if calls.get(key) != declared.get(key):
            errors.append(f"nested subprocess source audit differs: {key}: actual={calls.get(key)} declared={declared.get(key)}")
    skips = load_non_pg_skip_args(root)[1::2]
    pg = {name for name in runtime if any(skip in name for skip in skips)}
    replay = set(load_non_pg_filter_replay(root))
    sweep = (set(runtime) - pg) | replay
    for parent, child in inherited_pairs(root, path):
        lanes = (["pg"] if parent in pg else []) + (["non-pg-sweep"] if parent in sweep else [])
        for lane in lanes:
            if child not in ignored_by_lane[lane]:
                errors.append(f"inherited child is not ignored in its parent's lane {lane}: {child}")
        if parent in pg and (child not in pg or pg_test_shard(parent) != pg_test_shard(child)):
            errors.append(f"inherited PG child is outside its parent's numeric shard: {parent} -> {child}")
        if parent in sweep and (child not in sweep or (parent in replay) != (child in replay)):
            errors.append(f"inherited sweep child is outside its parent's judged/replay harness: {parent} -> {child}")
    return errors


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check",))
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    try:
        errors = check(args.root.resolve())
    except (OSError, ValueError, KeyError, TypeError) as error:
        errors = [str(error)]
    for error in errors:
        print(f"ERROR nested-libtest: {error}", file=sys.stderr)
    if not errors:
        print("NESTED_LIBTEST_OK")
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
