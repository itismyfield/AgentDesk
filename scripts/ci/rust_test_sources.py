#!/usr/bin/env python3
"""Rust test sources: which #[test] fns the crate roots reach, and on which platforms they compile.

  inventory  walk src/lib.rs through `mod`, `#[path]` and `include!`, keeping each test's cfg chain
  check      fail on test files no crate root reaches, missing module/include files and unreadable cfg
  compare    compare the source inventory for one platform with an unfiltered nextest list

A compiled list cannot see a test file that no `mod` connects, so the orphan check stays source based.
Nothing here runs cargo.
"""

import argparse
from dataclasses import dataclass, field
import json
from pathlib import Path
import re
import sys
import tomllib


TEST_ATTRS = {("test",), ("tokio", "test"), ("async_std", "test"), ("actix_rt", "test")}
# Rust `target_os` value per supported CI platform.
PLATFORMS = {"linux": "linux", "macos": "macos", "windows": "windows"}
RAW_STRING = re.compile(r'b?r(#{0,255})"')
CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'\n])'")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# cfg_attr payload attributes that change which files compile or which fns are tests.
GATING_CFG_ATTR = {"path", "test", "cfg"}
MANIFEST_DIR_INCLUDE = ("concat", "!", "(", "env", "!", "(", "CARGO_MANIFEST_DIR", ")")


@dataclass(frozen=True)
class RustToken:
    value: str
    line: int
    kind: str = "punct"


def rust_tokens(text):
    """Tokenize the Rust subset needed for attributes, items, macros and brace scopes."""
    tokens, index, line = [], 0, 1
    while index < len(text):
        char = text[index]
        if char.isspace():
            line += char == "\n"
            index += 1
            continue
        if text.startswith("//", index):
            end = text.find("\n", index)
            index = len(text) if end < 0 else end
            continue
        if text.startswith("/*", index):
            depth, index = 1, index + 2
            while index < len(text) and depth:
                if text.startswith("/*", index):
                    depth, index = depth + 1, index + 2
                elif text.startswith("*/", index):
                    depth, index = depth - 1, index + 2
                else:
                    line += text[index] == "\n"
                    index += 1
            continue
        raw = RAW_STRING.match(text, index) if char in "br" else None
        if raw:
            marker = '"' + raw.group(1)
            start = raw.end()
            end = text.find(marker, start)
            end = len(text) if end < 0 else end
            tokens.append(RustToken(text[start:end], line, "string"))
            line += text[start:end].count("\n")
            index = min(len(text), end + len(marker))
            continue
        if char == '"' or text.startswith('b"', index):
            start_line, index = line, index + (2 if char == "b" else 1)
            value = []
            while index < len(text) and text[index] != '"':
                step = 2 if text[index] == "\\" else 1
                value.append(text[index:index + step])
                line += text[index:index + step].count("\n")
                index += step
            tokens.append(RustToken("".join(value), start_line, "string"))
            index += 1
            continue
        if char == "'":
            literal = CHAR_LITERAL.match(text, index)
            if literal:
                index = literal.end()
                continue
        ident = IDENT.match(text, index)
        if ident:
            tokens.append(RustToken(ident.group(), line, "ident"))
            index = ident.end()
            continue
        if char in "#![]{}();=:":
            tokens.append(RustToken(char, line))
        index += 1
    return tokens


def attribute_span(tokens, index):
    """(opening bracket, exclusive end) of a real `#[...]` or `#![...]` at index, else None."""
    if tokens[index].kind != "punct" or tokens[index].value != "#":
        return None
    bracket = index + 1
    if bracket < len(tokens) and tokens[bracket].value == "!" and tokens[bracket].kind == "punct":
        bracket += 1
    if bracket >= len(tokens) or tokens[bracket].value != "[" or tokens[bracket].kind != "punct":
        return None
    end, depth = bracket + 1, 1
    while end < len(tokens) and depth:
        if tokens[end].kind == "punct":
            depth += (tokens[end].value == "[") - (tokens[end].value == "]")
        end += 1
    return bracket, end


def cfg_attr_payload(tokens):
    """Attribute names after the predicate of `(PRED, attr, ...)`, nested cfg_attr included; commas are not tokens."""
    values = [t.value for t in tokens]
    if len(values) < 3 or values[0] != "(":
        return {"cfg"}
    i = 2
    if values[2:3] == ["("]:
        depth, i = 1, 3
        while i < len(values) and depth:
            depth += (values[i] == "(") - (values[i] == ")")
            i += 1
    elif values[2:3] == ["="]:
        i = 4
    names, depth, nested = set(), 0, None
    for n in range(i, len(values) - 1):
        if depth == 0 and tokens[n].kind == "ident":
            names.add(values[n])
            if values[n] == "cfg_attr":
                nested = n + 1
        depth += (values[n] == "(") - (values[n] == ")")
        if nested is not None and depth == 0 and n >= nested:
            names |= cfg_attr_payload(tokens[nested:n + 1])
            nested = None
    return names


@dataclass
class Inventory:
    # Test ID -> every (site, cfg chain) that declares it; cfg-disjoint duplicates keep one entry each.
    tests: dict = field(default_factory=dict)
    reached: set = field(default_factory=set)
    # Reached file -> the module path its items live in, from the first walk that reached it.
    modules: dict = field(default_factory=dict)
    errors: list = field(default_factory=list)
    unreadable: list = field(default_factory=list)


def _include_target(tokens, index, source, repo_root):
    """Path an `include!(...)` at tokens[index] names, None for a non-literal form."""
    args = tokens[index + 3:index + 3 + len(MANIFEST_DIR_INCLUDE) + 1]
    if tokens[index + 3].kind == "string":
        return source.parent / tokens[index + 3].value
    if tuple(t.value for t in args[:-1]) == MANIFEST_DIR_INCLUDE and args[-1].kind == "string":
        return repo_root / args[-1].value.lstrip("/")
    return None


def walk(root, repo_root, inventory=None):
    """Add every test the crate root reaches to the inventory, following mod, #[path] and include!."""
    inv = inventory if inventory is not None else Inventory()
    # (file, module names, children directory, directory its own #[path] resolves against, cfg chain)
    queue = [(root, (), root.parent, root.parent, ())]
    seen = set()
    while queue:
        source, outer, base_dir, path_dir, file_cfgs = queue.pop()
        identity = (source.resolve(), outer, file_cfgs)
        if identity in seen or not source.is_file():
            continue
        seen.add(identity)
        inv.reached.add(source.resolve())
        inv.modules.setdefault(source.resolve(), outer)
        tokens = rust_tokens(source.read_text("utf-8"))
        rel = source.relative_to(repo_root)
        scopes, cfg_stack = [], [(0, file_cfgs)]
        depth, pending_path, pending_test, pending_cfgs = 0, None, False, ()
        index = 0
        while index < len(tokens):
            token = tokens[index]
            names = outer + tuple(s[1] for s in scopes)
            directory = scopes[-1][2] if scopes else base_dir
            # A #[path] in an inline module resolves under that module; at file level, beside the file.
            redirect_dir = scopes[-1][2] if scopes else path_dir
            cfgs = cfg_stack[-1][1]
            site = f"{rel}:{token.line}"
            span = attribute_span(tokens, index)
            if span is not None:
                bracket, end = span
                attr = tokens[bracket + 1:end - 1]
                path_end = next((n for n, item in enumerate(attr) if item.value in ("(", "=", "]")), len(attr))
                attr_path = tuple(item.value for item in attr[:path_end] if item.kind == "ident")
                outer_attr = bracket == index + 1
                pending_test = pending_test or attr_path in TEST_ATTRS
                if attr_path == ("path",) and outer_attr:
                    pending_path = next((item.value for item in attr if item.kind == "string"), pending_path)
                elif attr_path == ("cfg",):
                    predicate = tuple(item.value for item in attr[path_end:])
                    if outer_attr:
                        pending_cfgs += (predicate,)
                    else:
                        cfg_stack[-1] = (cfg_stack[-1][0], cfg_stack[-1][1] + (predicate,))
                elif attr_path == ("cfg_attr",) and GATING_CFG_ATTR & cfg_attr_payload(attr[path_end:]):
                    inv.unreadable.append(f"{site} cfg_attr can change compilation or test status")
                index = end
                continue
            if token.kind == "ident" and token.value == "fn" and index + 1 < len(tokens):
                if pending_test and tokens[index + 1].kind == "ident":
                    test_id = "::".join(names + (tokens[index + 1].value,))
                    inv.tests.setdefault(test_id, []).append((site, cfgs + pending_cfgs))
                pending_path, pending_test, pending_cfgs = None, False, ()
            elif token.kind == "ident" and token.value == "mod" and index + 2 < len(tokens) \
                    and tokens[index + 1].kind == "ident":
                name = tokens[index + 1].value
                cursor = index + 2
                while cursor < len(tokens) and tokens[cursor].value not in (";", "{"):
                    cursor += 1
                if cursor < len(tokens) and tokens[cursor].value == "{":
                    depth += 1
                    scopes.append((depth, name, redirect_dir / pending_path if pending_path else directory / name))
                    cfg_stack.append((depth, cfgs + pending_cfgs))
                    index = cursor + 1
                    pending_path, pending_test, pending_cfgs = None, False, ()
                    continue
                if cursor < len(tokens):
                    candidates = ((redirect_dir / pending_path,) if pending_path
                                  else (directory / f"{name}.rs", directory / name / "mod.rs"))
                    child = next((c for c in candidates if c.is_file()), None)
                    if child is None:
                        inv.errors.append(f"{site} mod {name}: no file at "
                                          f"{', '.join(str(c.relative_to(repo_root)) for c in candidates)}")
                    else:
                        owns_dir = pending_path is not None or child.name == "mod.rs"
                        queue.append((child, names + (name,), child.parent if owns_dir else child.parent / child.stem,
                                      child.parent, cfgs + pending_cfgs))
                    pending_path, pending_test, pending_cfgs = None, False, ()
            elif token.kind == "ident" and token.value == "include" and index + 3 < len(tokens) \
                    and tokens[index + 1].value == "!" and tokens[index + 2].value == "(":
                target = _include_target(tokens, index, source, repo_root)
                if target is None:
                    inv.unreadable.append(f"{site} include! with a non-literal path")
                elif not target.is_file():
                    inv.errors.append(f"{site} include! names missing file {target}")
                else:
                    # Included text is spliced into this module, so it keeps this module's names and directory.
                    queue.append((target, names, directory, redirect_dir, cfgs + pending_cfgs))
            if token.kind == "punct" and token.value == "{":
                depth += 1
                if pending_cfgs:
                    cfg_stack.append((depth, cfgs + pending_cfgs))
                pending_cfgs = ()
            elif token.kind == "punct" and token.value == "}":
                # A tail-expression attribute ends with its block; it must not reach the next item.
                depth, pending_cfgs = depth - 1, ()
                while scopes and scopes[-1][0] > depth:
                    scopes.pop()
                while len(cfg_stack) > 1 and cfg_stack[-1][0] > depth:
                    cfg_stack.pop()
            elif token.kind == "punct" and token.value == ";":
                pending_path, pending_test, pending_cfgs = None, False, ()
            index += 1
    return inv


def crate_roots(repo_root):
    src = repo_root / "src"
    roots = [src / "lib.rs", src / "main.rs", *sorted((src / "bin").glob("*.rs")),
             *sorted((src / "bin").glob("*/main.rs"))]
    return [r for r in roots if r.is_file()]


def has_test_attr(path):
    tokens = rust_tokens(path.read_text("utf-8"))
    for index in range(len(tokens)):
        span = attribute_span(tokens, index)
        if span is not None:
            attr = tokens[span[0] + 1:span[1] - 1]
            path_end = next((n for n, item in enumerate(attr) if item.value in ("(", "=", "]")), len(attr))
            attr_path = tuple(item.value for item in attr[:path_end] if item.kind == "ident")
            if attr_path in TEST_ATTRS or attr_path == ("cfg_attr",) and "test" in cfg_attr_payload(attr[path_end:]):
                return True
    return False


def lib_inventory(repo_root):
    """Lib-target tests plus every file any crate root reaches (for the orphan check)."""
    lib = walk(repo_root / "src/lib.rs", repo_root)
    reached = Inventory(reached=set(lib.reached))
    for root in crate_roots(repo_root):
        if root.name != "lib.rs" or root.parent != repo_root / "src":
            walk(root, repo_root, reached)
    lib.reached = reached.reached
    lib.errors += [e for e in reached.errors if e not in lib.errors]
    lib.unreadable += [u for u in reached.unreadable if u not in lib.unreadable]
    return lib


def orphans(repo_root, inventory):
    """Test-bearing files under src/ that no crate root reaches through mod, #[path] or include!."""
    return sorted(str(p.relative_to(repo_root)) for p in (repo_root / "src").rglob("*.rs")
                  if p.resolve() not in inventory.reached and has_test_attr(p))


def cfg_holds(predicate, target_os, features):
    """Evaluate one `cfg(...)` token tuple for a `cargo test --lib` build; None when unknown."""
    tokens = list(predicate)
    if len(tokens) < 3 or tokens[0] != "(" or tokens[-1] != ")":
        return None
    facts = {"target_os": target_os, "target_family": "windows" if target_os == "windows" else "unix"}
    flags = {"test": True, "debug_assertions": True, "unix": target_os != "windows",
             "windows": target_os == "windows"}

    def parse(i):
        word = tokens[i]
        if word in ("all", "any", "not") and tokens[i + 1] == "(":
            values, i = [], i + 2
            while tokens[i] != ")":
                value, i = parse(i)
                values.append(value)
            if word == "not":
                return (None if len(values) != 1 or values[0] is None else not values[0]), i + 1
            decided = word == "any"
            if decided in values:
                return decided, i + 1
            return (None if None in values else not decided), i + 1
        if tokens[i + 1] == "=":
            key, value = word, tokens[i + 2]
            if key == "feature":
                return value in features, i + 3
            return (facts[key] == value if key in facts else None), i + 3
        return flags.get(word), i + 1

    try:
        value, end = parse(1)
    except (IndexError, KeyError):
        return None
    return value if end == len(tokens) - 1 else None


def default_features(repo_root):
    """Features a plain `cargo test --lib` enables: `default` and what it names, transitively."""
    table = tomllib.loads((repo_root / "Cargo.toml").read_text("utf-8")).get("features", {})
    enabled, pending = set(), ["default"]
    while pending:
        name = pending.pop()
        if name in enabled:
            continue
        enabled.add(name)
        pending += [item for item in table.get(name, []) if "/" not in item and not item.startswith("dep:")]
    return frozenset(enabled - {"default"})


def platform_split(inventory, target_os, features):
    """(compiled, not compiled, unknown) test IDs for one platform; any declaration whose chain holds counts."""
    compiled, excluded, unknown = set(), set(), {}
    for test_id, declarations in inventory.tests.items():
        verdicts = [[cfg_holds(p, target_os, features) for p in chain] for _, chain in declarations]
        if any(all(v is True for v in chain) for chain in verdicts):
            compiled.add(test_id)
        elif all(False in chain for chain in verdicts):
            excluded.add(test_id)
        else:
            unknown[test_id] = [site for site, _ in declarations]
    return compiled, excluded, unknown


def listed_lib_tests(path):
    """Every test (runnable or ignored) of the lib binary in an unfiltered nextest list JSON."""
    doc = json.loads(Path(path).read_text())
    suites = doc.get("rust-suites") if isinstance(doc, dict) else None
    if not isinstance(suites, dict):
        raise ValueError("nextest list has no rust-suites object")
    libs = [s for s in suites.values() if isinstance(s, dict) and s.get("kind") == "lib"]
    if len(libs) != 1 or not isinstance(libs[0].get("testcases"), dict) or not libs[0]["testcases"]:
        raise ValueError(f"nextest list must hold exactly one lib suite with tests, found {len(libs)}")
    return set(libs[0]["testcases"])


def compare(inventory, listed, target_os, features):
    compiled, _, unknown = platform_split(inventory, target_os, features)
    return {"static_only": sorted(compiled - listed), "cargo_only": sorted(listed - set(inventory.tests)),
            "unknown_cfg": sorted(set(unknown) - listed)}


def check_findings(repo_root):
    inv = lib_inventory(repo_root)
    found = [f"orphan test file: {path}" for path in orphans(repo_root, inv)]
    found += [f"missing source: {e}" for e in inv.errors]
    found += [f"unreadable: {u}" for u in inv.unreadable]
    features = default_features(repo_root)
    for platform, target_os in PLATFORMS.items():
        _, _, unknown = platform_split(inv, target_os, features)
        found += [f"cfg unknown on {platform}: {t} ({', '.join(s)})" for t, s in sorted(unknown.items())]
    return inv, found


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    sub = parser.add_subparsers(dest="command", required=True)
    i = sub.add_parser("inventory")
    i.add_argument("--platform", choices=sorted(PLATFORMS), help="only tests compiled there")
    i.add_argument("--json-out", required=True)
    sub.add_parser("check")
    c = sub.add_parser("compare")
    c.add_argument("--platform", choices=sorted(PLATFORMS), required=True)
    c.add_argument("--list", required=True, help="unfiltered `cargo nextest list --message-format json`")
    c.add_argument("--json-out", required=True)
    args = parser.parse_args(argv)
    root = args.repo_root.resolve()
    if args.command == "check":
        inv, found = check_findings(root)
        for line in found:
            print(f"ERROR {line}", file=sys.stderr)
        print(f"{len(inv.tests)} lib tests reached; {len(found)} problem(s)")
        return 1 if found else 0
    inv = lib_inventory(root)
    features = default_features(root)
    if args.command == "inventory":
        ids = (platform_split(inv, PLATFORMS[args.platform], features)[0] if args.platform else set(inv.tests))
        Path(args.json_out).write_text(json.dumps(sorted(ids), indent=0) + "\n")
        print(f"{len(ids)} tests")
        return 0
    try:
        listed = listed_lib_tests(args.list)
    except (OSError, ValueError) as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 1
    result = compare(inv, listed, PLATFORMS[args.platform], features)
    Path(args.json_out).write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    print(" · ".join(f"{k} {len(v)}" for k, v in sorted(result.items())))
    return 1 if result["static_only"] or result["unknown_cfg"] else 0


if __name__ == "__main__":
    sys.exit(main())
