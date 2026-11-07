#!/usr/bin/env python3
"""Rust test sources: which #[test] fns the crate roots reach, and on which platforms they compile.

  inventory  walk src/lib.rs through `mod`, `#[path]` and `include!`, keeping each test's cfg chain
  check      fail on test files no crate root reaches, missing module/include files, unreadable cfg and
             test-only `.join()`/`.recv()` waits without a cap; the allowlist may only lose rows against
             --allowlist-base (HEAD^1: the PR base in CI; a base without the file counts its own violations)
  compare    compare the source inventory for one platform with an unfiltered nextest list
  removed    per platform, tests a base checkout compiles that this tree no longer compiles, and tests
             runnable there that `#[ignore]` (plain or cfg_attr) now skips here; this tree must read
             cleanly, the base may lack mod/include! files as long as no test file becomes unreachable

A compiled list cannot see a test file that no `mod` connects, so the orphan check stays source based.
Nothing here runs cargo.
"""

import argparse
from bisect import bisect_right
from dataclasses import dataclass, field
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib


TEST_ATTRS = {("test",), ("tokio", "test"), ("async_std", "test"), ("actix_rt", "test")}
# Rust `target_os` value per supported CI platform.
PLATFORMS = {"linux": "linux", "macos": "macos", "windows": "windows"}
RAW_STRING = re.compile(r'b?r(#{0,255})"')
CHAR_LITERAL = re.compile(r"'(?:\\u\{[0-9A-Fa-f_]{1,6}\}|\\x[0-9A-Fa-f]{2}|\\.|[^\\'\n])'")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# cfg_attr payload attributes that change which files compile or which fns are tests.
GATING_CFG_ATTR = {"path", "test", "cfg"}
MANIFEST_DIR_INCLUDE = ("concat", "!", "(", "env", "!", "(", "CARGO_MANIFEST_DIR", ")")
# Test-only `.join()`/`.recv()` calls must go through these capped helpers; a call nested in one is bounded.
BOUNDED_CALLS = {"timeout", "timeout_at", "join_within", "recv_within", "await_within"}
UNBOUNDED_WAITS = {"join": "thread join", "recv": "channel recv"}
WAIT_HELPER = "src/test_support/wait.rs"
WAIT_ALLOWLIST = "scripts/ci/test_wait_allowlist.txt"
WAIT_CALL = re.compile(r"\b(?:join|recv)\b")
# After `#[cfg(test)]`, one of these starts an item or block whose gate is its `{...}` block (or ends at `;`);
# anything else (a `let`, expression, field or parameter) stays gated to its own `;`, `,` or `)`.
BLOCK_WORDS = {"fn", "mod", "impl", "struct", "enum", "union", "trait", "type", "static", "const", "use", "pub",
               "async", "unsafe", "extern", "macro_rules", "if", "match", "for", "while", "loop"}
# Macros whose tokens are never compiled or run, so an attribute or call inside them gates nothing.
INERT_MACROS = {"stringify"}


@dataclass(frozen=True)
class RustToken:
    value: str
    line: int
    kind: str = "punct"
    # Source offset of the token; metadata only, so it takes no part in equality.
    start: int = field(default=-1, compare=False)


def rust_tokens(text, *, extended=False):
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
            tokens.append(RustToken(text[start:end], line, "string", index))
            line += text[start:end].count("\n")
            index = min(len(text), end + len(marker))
            continue
        if char == '"' or text.startswith('b"', index):
            opening = index
            start_line, index = line, index + (2 if char == "b" else 1)
            value = []
            while index < len(text) and text[index] != '"':
                step = 2 if text[index] == "\\" else 1
                value.append(text[index:index + step])
                line += text[index:index + step].count("\n")
                index += step
            tokens.append(RustToken("".join(value), start_line, "string", opening))
            index += 1
            continue
        if char == "'":
            literal = CHAR_LITERAL.match(text, index)
            if literal:
                index = literal.end()
                continue
        if extended and text.startswith("r#", index):
            raw_ident = IDENT.match(text, index + 2)
            if raw_ident:
                tokens.append(RustToken(raw_ident.group(), line, "ident", index))
                index = raw_ident.end()
                continue
        ident = IDENT.match(text, index)
        if ident:
            tokens.append(RustToken(ident.group(), line, "ident", index))
            index = ident.end()
            continue
        if char in "#![]{}();=:" or extended and char in ",*$<>.'":
            tokens.append(RustToken(char, line, "punct", index))
        index += 1
    return tokens


def production_tokens(path, *, unreadable=None, diagnostic_path=None):
    """Return production tokens, or None when a file-level test gate excludes the file."""
    try:
        from rust_production import _production_text, production_lines
        from rust_lex import StripState, cfg_item_end, strip_line
        from rust_cfg import find_test_only_cfg_attributes
        from generate_inventory_docs import scan_balanced
    except ModuleNotFoundError:
        from scripts.rust_production import _production_text, production_lines
        from scripts.rust_lex import StripState, cfg_item_end, strip_line
        from scripts.rust_cfg import find_test_only_cfg_attributes
        from scripts.generate_inventory_docs import scan_balanced
    raw = path.read_text("utf-8")
    tokens = rust_tokens(raw, extended=True)
    state = StripState()
    code = "".join(strip_line(line, state).ljust(len(line)) for line in raw.splitlines(keepends=True))
    matches = list(find_test_only_cfg_attributes(code))
    gated = {match.start() for match in matches if not match.is_inner}
    inner = {match.start() for match in matches if match.is_inner}
    containers, braces = {}, []
    for token in tokens:
        if token.start in inner:
            if not braces:
                return None
            containers[token.start] = braces[-1]
        if token.kind == "punct" and token.value == "{":
            braces.append(token.start)
        elif token.kind == "punct" and token.value == "}" and braces:
            braces.pop()
    for index, token in enumerate(tokens):
        span = attribute_span(tokens, index)
        if span is not None:
            attr = tokens[span[0] + 1:span[1] - 1]
            path_end = next((n for n, t in enumerate(attr) if t.value in ("(", "=")), len(attr))
            names = tuple(t.value for t in attr[:path_end] if t.kind == "ident")
            if names in TEST_ATTRS:
                gated.add(token.start)
    parameter_gates, parameter_ranges, same_line = set(), {}, {}
    if gated and "|" in code:
        list(production_lines(path, parameter_gates=parameter_gates, parameter_ranges=parameter_ranges))
    for match in matches:
        line = raw.count("\n", 0, match.attribute_end() - 1)
        same_line.setdefault(line, []).append(match.start())
    for starts in same_line.values():
        if len(starts) < 2:
            continue
        for start in starts:
            queried, ends = set(), {}
            list(production_lines(path, parameter_gates=queried, parameter_ranges=ends, gate_start=start))
            parameter_gates.discard(start)
            parameter_ranges.pop(start, None)
            parameter_gates.update(queried)
            parameter_ranges.update(ends)
    ranges, exact, handled_gates = [], True, set()
    for start in sorted(gated | inner):
        if ranges and start < ranges[-1][1]:
            continue
        try:
            if start in inner:
                _, end = scan_balanced(code, containers[start], "{", "}")
                end -= 1  # Keep the enclosing module's closing delimiter.
            else:
                if start in parameter_gates:
                    if start not in parameter_ranges:
                        raise ValueError("closure parameter boundary is uncertain")
                    ranges.append((start, parameter_ranges[start]))
                    handled_gates.add(start)
                    if unreadable is not None:
                        unreadable.append(f"{diagnostic_path if diagnostic_path is not None else path}:"
                                          f"{raw.count(chr(10), 0, start) + 1} uncertain test-only range: closure parameter boundary is uncertain")
                    continue
                end = cfg_item_end(code, start, scan_balanced)
                if code[end - 1:end] == "," and "<" in code[start:end]:
                    raise ValueError("generic field boundary is uncertain")
        except ValueError as exc:
            exact = False
            if unreadable is not None:
                unreadable.append(f"{diagnostic_path if diagnostic_path is not None else path}:"
                                  f"{raw.count(chr(10), 0, start) + 1} uncertain test-only range: {exc}")
            continue
        if ranges and start <= ranges[-1][1]:
            ranges[-1] = (ranges[-1][0], max(ranges[-1][1], end))
        else:
            ranges.append((start, end))
    range_starts = [start for start, _ in ranges]
    masked = _production_text(path, handled_gates=handled_gates).splitlines() if not exact else ()
    starts, offset = [], 0
    for line in raw.splitlines(keepends=True):
        starts.append(offset)
        offset += len(line)
    result = []
    for token in tokens:
        region = bisect_right(range_starts, token.start) - 1
        if region >= 0 and token.start < ranges[region][1]:
            continue
        column = token.start - starts[token.line - 1]
        line = masked[token.line - 1] if token.line <= len(masked) else ""
        if exact or token.kind == "string" or column < len(line) and not line[column].isspace():
            result.append(token)
    return result


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


def _paren(token):
    """+1/-1 for a real parenthesis token; a string such as "(" is not one."""
    return token.kind == "punct" and (token.value == "(") - (token.value == ")")


def cfg_attr_payload(tokens):
    """Attribute names after the predicate of `(PRED, attr, ...)`, nested cfg_attr included; commas are not tokens."""
    values = [t.value for t in tokens]
    if len(values) < 3 or _paren(tokens[0]) != 1:
        return {"cfg"}
    i = 2
    if _paren(tokens[2]) == 1:
        depth, i = 1, 3
        while i < len(values) and depth:
            depth += _paren(tokens[i])
            i += 1
    elif values[2:3] == ["="]:
        i = 4
    names, depth, nested = set(), 0, None
    for n in range(i, len(values) - 1):
        if depth == 0 and tokens[n].kind == "ident":
            names.add(values[n])
            if values[n] == "cfg_attr":
                nested = n + 1
        depth += _paren(tokens[n])
        if nested is not None and depth == 0 and n >= nested:
            names |= cfg_attr_payload(tokens[nested:n + 1])
            nested = None
    return names


@dataclass
class Inventory:
    # Test ID -> every (site, cfg chain) that declares it; cfg-disjoint duplicates keep one entry each.
    tests: dict = field(default_factory=dict)
    reached: set = field(default_factory=set)
    # Reached file -> every cfg chain it is reached under; all test-only means the whole file is test code.
    file_cfgs: dict = field(default_factory=dict)
    # Reached file -> every module path its items live in; more than one means one file, several modules.
    modules: dict = field(default_factory=dict)
    errors: list = field(default_factory=list)
    # The subset of errors that name a mod or include! target file that does not exist.
    missing_files: list = field(default_factory=list)
    unreadable: list = field(default_factory=list)
    # Declaration site -> its ignore conditions: () for a plain #[ignore], else a cfg_attr predicate.
    ignores: dict = field(default_factory=dict)
    # Production occurrences retain inherited cfgs, including mutually exclusive targets.
    sources: list = field(default_factory=list)
    logical_modules: set = field(default_factory=set)
    read_failures: list = field(default_factory=list)
    source_tokens: dict = field(default_factory=dict)


def _include_target(tokens, index, source, repo_root):
    """Path an `include!(...)` at tokens[index] names, None for a non-literal form."""
    args = [t for t in tokens[index + 3:index + 5 + len(MANIFEST_DIR_INCLUDE)]
            if t.kind != "punct" or t.value != ","][:len(MANIFEST_DIR_INCLUDE) + 1]
    if tokens[index + 3].kind == "string":
        return source.parent / tokens[index + 3].value
    if tuple(t.value for t in args[:-1]) == MANIFEST_DIR_INCLUDE and args[-1].kind == "string":
        return repo_root / args[-1].value.lstrip("/")
    return None


def walk(root, repo_root, inventory=None, *, production=False):
    """Add every test the crate root reaches to the inventory, following mod, #[path] and include!."""
    inv = inventory if inventory is not None else Inventory()
    if production:
        try:
            from rust_cfg import cfg_requires_test
        except ModuleNotFoundError:
            from scripts.rust_cfg import cfg_requires_test
    # (file, module names, children directory, directory its own #[path] resolves against, cfg chain)
    queue = [(root, (), root.parent, root.parent, ())]
    seen = set()
    while queue:
        source, outer, base_dir, path_dir, file_cfgs = queue.pop()
        identity = (source.resolve(), outer, file_cfgs)
        if identity in seen:
            continue
        seen.add(identity)
        if production and any(cfg_requires_test("".join(cfg[1:-1])) for cfg in file_cfgs):
            continue
        if production and not source.resolve().is_relative_to(repo_root.resolve()):
            inv.errors.append(f"{source}: source is outside repository root")
            continue
        if not source.is_file():
            if production:
                inv.read_failures.append(f"{source}: source is missing or not a file")
            continue
        inv.reached.add(source.resolve())
        inv.file_cfgs.setdefault(source.resolve(), set()).add(file_cfgs)
        inv.modules.setdefault(source.resolve(), set()).add(outer)
        try:
            if production:
                if source.resolve() not in inv.source_tokens:
                    inv.source_tokens[source.resolve()] = production_tokens(
                        source, unreadable=inv.unreadable, diagnostic_path=source.relative_to(repo_root).as_posix())
                tokens = inv.source_tokens[source.resolve()]
            else:
                tokens = rust_tokens(source.read_text("utf-8"))
        except (OSError, UnicodeError) as exc:
            if not production:
                raise
            inv.read_failures.append(f"{source}: {type(exc).__name__}: {exc}")
            continue
        if production and tokens is None:
            continue
        if production:
            inv.sources.append((source.resolve(), outer, file_cfgs))
            inv.logical_modules.add(outer)
        rel = source.relative_to(repo_root)
        scopes, cfg_stack = [], [(0, file_cfgs)]
        depth, pending_path, pending_test, pending_cfgs, pending_ignore = 0, None, False, (), ()
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
                elif attr_path == ("ignore",) and outer_attr:
                    pending_ignore += ((),)
                elif attr_path == ("cfg_attr",) and GATING_CFG_ATTR & cfg_attr_payload(attr[path_end:]):
                    inv.unreadable.append(f"{site} cfg_attr can change compilation or test status")
                elif attr_path == ("cfg_attr",) and "ignore" in cfg_attr_payload(attr[path_end:]):
                    predicate = _cfg_attr_predicate(attr[path_end:])
                    if predicate is None:
                        inv.unreadable.append(f"{site} cfg_attr ignore with an unreadable predicate")
                    else:
                        pending_ignore += (predicate,)
                index = end
                continue
            if token.kind == "ident" and token.value == "fn" and index + 1 < len(tokens):
                if pending_test and tokens[index + 1].kind == "ident":
                    test_id = "::".join(names + (tokens[index + 1].value,))
                    inv.tests.setdefault(test_id, []).append((site, cfgs + pending_cfgs))
                    if pending_ignore:
                        inv.ignores[site] = pending_ignore
                pending_path, pending_test, pending_cfgs, pending_ignore = None, False, (), ()
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
                    if production:
                        inv.logical_modules.add(names + (name,))
                    index = cursor + 1
                    pending_path, pending_test, pending_cfgs = None, False, ()
                    continue
                if cursor < len(tokens):
                    candidates = ((redirect_dir / pending_path,) if pending_path
                                  else (directory / f"{name}.rs", directory / name / "mod.rs"))
                    child = next((c for c in candidates if c.is_file()), None)
                    if child is None:
                        inv.errors.append(f"{site} mod {name}: no file at "
                                          f"{', '.join(str(c.relative_to(repo_root)) if c.is_relative_to(repo_root) else str(c) for c in candidates)}")
                        inv.missing_files.append(inv.errors[-1])
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
                    inv.missing_files.append(inv.errors[-1])
                else:
                    # Included text keeps this module's names, but rustc resolves its mod and #[path] beside the included file.
                    queue.append((target, names, target.parent, target.parent, cfgs + pending_cfgs))
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
                pending_path, pending_test, pending_cfgs, pending_ignore = None, False, (), ()
            index += 1
    return inv


def _cfg_attr_predicate(tokens):
    """`(PRED)` of `cfg_attr(PRED, ignore)` in cfg_holds form; None for a nested cfg_attr."""
    values = [t.value for t in tokens]
    if len(values) < 4 or _paren(tokens[0]) != 1 or "cfg_attr" in values[2:]:
        return None
    end = 2
    if _paren(tokens[2]) == 1:
        depth, end = 1, 3
        while end < len(values) and depth:
            depth += _paren(tokens[end])
            end += 1
    elif values[2] == "=":
        end = 4
    return ("(", *values[1:end], ")")


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
    for path, chains in reached.file_cfgs.items():
        lib.file_cfgs.setdefault(path, set()).update(chains)
    lib.errors += [e for e in reached.errors if e not in lib.errors]
    lib.missing_files += [e for e in reached.missing_files if e not in lib.missing_files]
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


def compiled_lib_tests(repo_root, platform="linux"):
    """Lib test IDs a plain `cargo test --lib` compiles on one platform, read from the sources alone."""
    inventory = walk(repo_root / "src/lib.rs", repo_root)
    return platform_split(inventory, PLATFORMS[platform], default_features(repo_root))[0]


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


def listed_lib_ignored(path):
    """Tests the lib suite of a nextest list marks ignored."""
    doc = json.loads(Path(path).read_text())
    libs = [s for s in doc["rust-suites"].values() if isinstance(s, dict) and s.get("kind") == "lib"]
    return {name for name, case in libs[0]["testcases"].items() if case.get("ignored") is True}


def compare(inventory, listed, target_os, features, listed_ignored=frozenset()):
    compiled, _, unknown = platform_split(inventory, target_os, features)
    ignored = {t for t in compiled & listed if _source_ignored(inventory, t, target_os, features)}
    return {"static_only": sorted(compiled - listed), "cargo_only": sorted(listed - set(inventory.tests)),
            "unknown_cfg": sorted(set(unknown) - listed),
            "ignore_mismatch": sorted(ignored ^ (listed_ignored & compiled))}


def _source_ignored(inv, test_id, target_os, features):
    states = [_ignored_on(inv.ignores.get(site, ()), target_os, features) for site, chain in inv.tests[test_id]
              if all(cfg_holds(p, target_os, features) is True for p in chain)]
    return bool(states) and all(s is not False for s in states)


def platform_tests(repo_root, allow_missing_files=False):
    """{platform: (compiled, ignored) test IDs} for one tree; anything unreadable fails, since it cannot prove a loss.

    allow_missing_files also reads a tree whose only errors are absent mod/include! files, unless a test file
    under src/ is then unreachable: its tests would silently drop out of the comparison."""
    inv, features, found = lib_inventory(repo_root), default_features(repo_root), {}
    if inv.unreadable:
        raise ValueError(f"{repo_root}: unreadable attributes ({inv.unreadable[0]}); run `check` there")
    if allow_missing_files and inv.missing_files:
        stranded = orphans(repo_root, inv)
        if stranded:
            raise ValueError(f"{repo_root}: missing modules leave test sources unreachable: {stranded[0]}")
    other = [e for e in inv.errors if not (allow_missing_files and e in inv.missing_files)]
    for platform, target_os in PLATFORMS.items():
        compiled, _, unknown = platform_split(inv, target_os, features)
        if unknown or other:
            raise ValueError(f"{repo_root}: cfg unknown or sources missing on {platform}; run `check` there")
        ignored = set()
        for test_id in compiled:
            # Ignored only when every declaration that compiles here is ignored here.
            states = [_ignored_on(inv.ignores.get(site, ()), target_os, features)
                      for site, chain in inv.tests[test_id]
                      if all(cfg_holds(p, target_os, features) is True for p in chain)]
            if None in states:
                raise ValueError(f"{repo_root}: cfg_attr ignore of {test_id} is unknown on {platform}")
            if all(states):
                ignored.add(test_id)
        found[platform] = (compiled, ignored)
    return found, list(inv.missing_files) if allow_missing_files else []


def _ignored_on(conditions, target_os, features):
    verdicts = [True if not c else cfg_holds(c, target_os, features) for c in conditions]
    return True if True in verdicts else (None if None in verdicts else False)


def _cfg_requires_test_text(expression):
    """`rust_cfg.cfg_requires_test` for a cfg predicate's source text; the script may run from any cwd."""
    try:
        from rust_cfg import cfg_requires_test
    except ModuleNotFoundError:
        sys.path.append(str(Path(__file__).resolve().parents[1]))
        from rust_cfg import cfg_requires_test
    return cfg_requires_test(expression)


def _cfg_requires_test(predicate):
    """Whether one cfg token tuple from `walk` can only hold under `test`."""
    words = list(predicate[1:-1])
    text = ""
    for n, word in enumerate(words):
        if n and words[n - 1] not in "(=" and word not in "()=":
            text += ","
        text += f'"{word}"' if n and words[n - 1] == "=" else word
    return _cfg_requires_test_text(text)


def unbounded_waits(raw, tokens, whole_file):
    """(line, kind) of every test-only `.join()`/`.recv()` that no bounded call (BOUNDED_CALLS) encloses.

    Test-only code: the whole file, a `#![cfg(test)]` block, and what follows a `#[cfg(test)]` or test
    attribute: an item's `{}` block, or a statement, parameter or field up to its own `;`, `,` or `)`."""
    found, parens, gated, depth, brackets, index = [], [], [], 0, 0, 0
    pending = None  # (kind, paren depth, brace depth) of a gate still waiting for its block or end
    while index < len(tokens):
        token = tokens[index]
        span = attribute_span(tokens, index)
        if span is not None:
            bracket, end = span
            attr = tokens[bracket + 1:end - 1]
            path_end = next((n for n, item in enumerate(attr) if item.value in ("(", "=")), len(attr))
            names = tuple(item.value for item in attr[:path_end] if item.kind == "ident")
            gate = names in TEST_ATTRS or names == ("cfg_attr",) and "test" in cfg_attr_payload(attr[path_end:])
            if names == ("cfg",) and path_end < len(attr) and attr[path_end].value == "(":
                gate = _cfg_requires_test_text(raw[attr[path_end].start + 1:attr[-1].start])
            if gate and bracket != index + 1:
                if depth == 0:
                    whole_file = True
                elif not gated or gated[-1] != depth:
                    gated.append(depth)
            elif gate and pending is None:
                after = end
                while after < len(tokens) and attribute_span(tokens, after) is not None:
                    after = attribute_span(tokens, after)[1]
                first = tokens[after] if after < len(tokens) else None
                block = first is not None and first.kind == "ident" and first.value in BLOCK_WORDS
                pending = ("block" if block else "span", len(parens), depth)
            index = end
            continue
        if token.kind == "ident" and token.value in INERT_MACROS and index + 2 < len(tokens) \
                and tokens[index + 1].value == "!" and tokens[index + 2].value in ("(", "[", "{"):
            index = _macro_end(tokens, index + 2)
            continue
        if token.kind == "punct":
            if token.value == "(":
                before = tokens[index - 1] if index else None
                parens.append(before.value if before is not None and before.kind == "ident" else None)
            elif token.value == ")":
                if parens:
                    parens.pop()
                if pending is not None and len(parens) < pending[1]:
                    pending = None
            elif token.value == "[":
                brackets += 1
            elif token.value == "]":
                brackets -= 1
            elif token.value == "{":
                depth += 1
                if pending is not None and pending[0] == "block":
                    gated.append(depth)
                    pending = None
            elif token.value == "}":
                if gated and gated[-1] == depth:
                    gated.pop()
                depth -= 1
                if pending is not None and depth < pending[2]:
                    pending = None
            elif token.value == ";" and not brackets and pending is not None and depth == pending[2]:
                pending = None
            elif token.value == "," and pending is not None and pending[0] == "span" \
                    and (len(parens), depth) == pending[1:]:
                pending = None
        elif token.kind == "ident" and token.value in UNBOUNDED_WAITS and index + 2 < len(tokens) \
                and index and tokens[index - 1].value == "." and tokens[index - 1].kind == "punct" \
                and tokens[index + 1].value == "(" and tokens[index + 2].value == ")" \
                and (whole_file or gated or pending is not None) and not BOUNDED_CALLS & set(parens):
            found.append((token.line, UNBOUNDED_WAITS[token.value]))
        index += 1
    return found


def _macro_end(tokens, opener):
    """Index just past the delimiter group that opens at tokens[opener]."""
    pairs, depth = {"(": ")", "[": "]", "{": "}"}, 0
    for index in range(opener, len(tokens)):
        if tokens[index].kind != "punct":
            continue
        if tokens[index].value in pairs:
            depth += 1
        elif tokens[index].value in pairs.values():
            depth -= 1
            if depth == 0:
                return index + 1
    return len(tokens)


def wait_allowlist(repo_root):
    """(entries, problems) of the unbounded-wait allowlist: one repo path per line, `#` comments without issue numbers."""
    path = repo_root / WAIT_ALLOWLIST
    entries, problems = [], []
    if not path.is_file():
        return entries, problems
    for number, line in enumerate(path.read_text("utf-8").splitlines(), 1):
        text = line.strip()
        if not text:
            continue
        if text.startswith("#"):
            if re.search(r"#\d+", text):
                problems.append(f"{WAIT_ALLOWLIST}:{number} comment names an issue number")
            continue
        if text in entries:
            problems.append(f"{WAIT_ALLOWLIST}:{number} duplicate entry {text}")
        entries.append(text)
    return entries, problems


def unbounded_wait_files(repo_root, inventory):
    """{repo path: [(line, kind)]} of the test code that waits without a cap."""
    root = repo_root.resolve()
    files = {path: chains for path, chains in inventory.file_cfgs.items()}
    for path in sorted((root / "tests").rglob("*.rs")):
        if "fixtures" not in path.relative_to(root).parts:
            files[path.resolve()] = None
    violations = {}
    for path in sorted(files):
        rel = path.relative_to(root).as_posix()
        if rel == WAIT_HELPER:
            continue
        chains = files[path]
        whole = chains is None or all(any(_cfg_requires_test(cfg) for cfg in chain) for chain in chains)
        raw = path.read_text("utf-8")
        if not WAIT_CALL.search(raw):
            continue
        hits = unbounded_waits(raw, rust_tokens(raw, extended=True), whole)
        if hits:
            violations[rel] = hits
    return violations


def test_wait_findings(repo_root, inventory):
    """Test code waiting without a cap: new violations and allowlist rows whose file no longer needs one."""
    violations = unbounded_wait_files(repo_root, inventory)
    allowed, found = wait_allowlist(repo_root)
    found += [f"unbounded wait allowlist: {rel} has no unbounded wait left; remove from {WAIT_ALLOWLIST}"
              for rel in allowed if rel not in violations]
    for rel, hits in sorted(violations.items()):
        if rel in allowed:
            continue
        found += [f"unbounded wait: {rel}:{line} {kind} without a cap; use test_support::wait "
                  f"(join_within/recv_within/await_within)" for line, kind in hits]
    return found


def base_allowlist(repo_root, rev):
    """Allowlist paths at git `rev`; where that tree has no allowlist yet, the files that wait without a cap in it."""
    result = subprocess.run(["git", "-C", str(repo_root), "show", f"{rev}:{WAIT_ALLOWLIST}"],
                            capture_output=True, text=True, check=False)
    if result.returncode:
        if "does not exist in" in result.stderr or "exists on disk, but not in" in result.stderr:
            return base_tree_violations(repo_root, rev)
        raise ValueError(result.stderr.strip() or f"git show {rev}:{WAIT_ALLOWLIST} failed")
    return {line.strip() for line in result.stdout.splitlines() if line.strip() and not line.startswith("#")}


def base_tree_violations(repo_root, rev):
    """Files with an unbounded wait in the tree at `rev`, read from a temporary `git archive` extraction."""
    archive = subprocess.run(["git", "-C", str(repo_root), "archive", rev, "Cargo.toml", "src", "tests"],
                             capture_output=True, check=False)
    if archive.returncode:
        raise ValueError(archive.stderr.decode().strip() or f"git archive {rev} failed")
    with tempfile.TemporaryDirectory() as tmp:
        subprocess.run(["tar", "-x", "-C", tmp], input=archive.stdout, check=True)
        base = Path(tmp)
        return set(unbounded_wait_files(base, lib_inventory(base)))


def allowlist_growth(current, base):
    """Rows the allowlist gained over `base`: it may only shrink, so each one is a problem."""
    return [f"unbounded wait allowlist: {rel} is new against the base allowlist; the allowlist only shrinks, "
            f"use test_support::wait instead" for rel in sorted(set(current) - base)]


def check_findings(repo_root, allowlist_base="HEAD^1"):
    inv = lib_inventory(repo_root)
    found = [f"orphan test file: {path}" for path in orphans(repo_root, inv)]
    found += [f"missing source: {e}" for e in inv.errors]
    found += [f"unreadable: {u}" for u in inv.unreadable]
    found += test_wait_findings(repo_root, inv)
    try:
        found += allowlist_growth(wait_allowlist(repo_root)[0], base_allowlist(repo_root, allowlist_base))
    except ValueError as exc:
        found.append(f"unbounded wait allowlist: base {allowlist_base} unreadable ({exc}); pass --allowlist-base")
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
    k = sub.add_parser("check")
    k.add_argument("--allowlist-base", default="HEAD^1",
                   help="git rev whose unbounded-wait allowlist the current one may only shrink from")
    c = sub.add_parser("compare")
    c.add_argument("--platform", choices=sorted(PLATFORMS), required=True)
    c.add_argument("--list", required=True, help="unfiltered `cargo nextest list --message-format json`")
    c.add_argument("--json-out", required=True)
    r = sub.add_parser("removed")
    r.add_argument("--base-root", type=Path, required=True, help="checkout of the comparison base")
    r.add_argument("--json-out", required=True)
    args = parser.parse_args(argv)
    root = args.repo_root.resolve()
    if args.command == "removed":
        # Head first and strict: only the base may lack files, and only mod/include! targets.
        try:
            head, _ = platform_tests(root)
        except (OSError, ValueError) as exc:
            print(f"ERROR head sources: {exc}", file=sys.stderr)
            return 1
        try:
            base, missing = platform_tests(args.base_root.resolve(), allow_missing_files=True)
        except (OSError, ValueError) as exc:
            print(f"ERROR base sources: {exc}", file=sys.stderr)
            return 1
        lost = {"removed": {p: sorted(base[p][0] - head[p][0]) for p in PLATFORMS},
                "runnable_to_ignored": {p: sorted((base[p][0] - base[p][1]) & head[p][1]) for p in PLATFORMS},
                "present_tests": {p: sorted(base[p][0] | head[p][0]) for p in PLATFORMS}}
        if missing:
            lost["base_missing_files"] = missing
        Path(args.json_out).write_text(json.dumps(lost, indent=1, sort_keys=True) + "\n")
        print(" · ".join(f"{p} -{len(lost['removed'][p])} ignored+{len(lost['runnable_to_ignored'][p])}"
                         for p in PLATFORMS))
        return 0
    if args.command == "check":
        inv, found = check_findings(root, args.allowlist_base)
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
        listed, listed_ignored = listed_lib_tests(args.list), listed_lib_ignored(args.list)
    except (OSError, ValueError) as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 1
    result = compare(inv, listed, PLATFORMS[args.platform], features, listed_ignored)
    Path(args.json_out).write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    print(" · ".join(f"{k} {len(v)}" for k, v in sorted(result.items())))
    return 1 if result["static_only"] or result["unknown_cfg"] or result["ignore_mismatch"] else 0


if __name__ == "__main__":
    sys.exit(main())
