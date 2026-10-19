"""Conservative literal-path checks for Rust consumers of changed docs and YAML.

This is not interprocedural dataflow: paths assembled by arbitrary helpers,
configuration or encoded strings cannot be resolved. Known read/open calls and
directory enumeration are checked against literals in the same source file.
"""

from __future__ import annotations

import hashlib
import posixpath
from dataclasses import dataclass

from rust_include_reads import CODE, LITERAL, decode, git_bytes, group_end, pieces, read_blobs, tokens, tree_sites

READ_NAMES = frozenset({"read", "read_to_string", "read_to_end", "read_dir", "open"})
SCENARIO_READER = "src/services/discord/tui_prompt_relay/tests/scenario_census_e2e.rs"


@dataclass(frozen=True)
class ParseOnlyReader:
    sha256: str
    directory: str
    read_line: int
    parse_line: int
    reason: str


# Whole-source pins make an added raw-text assertion revoke the exception.
PARSE_ONLY_READERS = {
    SCENARIO_READER: ParseOnlyReader(
        "7b6888b233771fc5ec26d6a6bada9b4f29bb22d2f9216fba30ed3cbe9bff961a",
        "tests/e2e/tui_relay/scenarios",
        87,
        90,
        "Lines 70-75 enumerate .yaml only; 87-90 read then immediately parse with serde_yaml; "
        "later assertions consume parsed values, never raw YAML text.",
    ),
}


def runtime_tokens(text: str) -> list[tuple[str, str, int]]:
    """Drop include macro bodies; the separate include scanner owns those reads."""
    toks = tokens(text)
    out = []
    index = 0
    while index < len(toks):
        kind, value, _ = toks[index]
        if (kind == CODE and value in {"include", "include_str", "include_bytes"}
                and index + 2 < len(toks) and toks[index + 1][1] == "!"
                and toks[index + 2][1] in {"(", "[", "{"}):
            index = group_end(toks, index + 2) + 1
        else:
            out.append(toks[index])
            index += 1
    return out


def literal_path(value: str) -> str:
    """Normalize a source-relative or manifest-relative literal for suffix matching."""
    path = posixpath.normpath(value.replace("\\", "/"))
    while path.startswith("../"):
        path = path[3:]
    return path.removeprefix("./").strip("/")


def enumeration_literals(toks: list[tuple[str, str, int]]) -> set[str]:
    """Direct read_dir arguments and simple let/const path bindings."""
    bindings: dict[str, set[str]] = {}
    for index, (kind, name, _line) in enumerate(toks):
        if kind != CODE or name not in {"let", "const", "static"}:
            continue
        start = index + 1
        if start < len(toks) and toks[start][1] == "mut":
            start += 1
        if start >= len(toks):
            continue
        end = start
        while end < len(toks) and toks[end][1] not in {"=", ";"}:
            end += 1
        if end == len(toks) or toks[end][1] != "=":
            continue
        expr = []
        for token in toks[end + 1:]:
            if token[0] == CODE and token[1] == ";":
                break
            expr.append(token)
        values = {literal_path(value) for k, raw, _ in expr
                  if k == LITERAL and (value := decode(raw)) is not None}
        values.update(value for value, _ in concat_literals("", expr))
        values.update(value for k, ref, _ in expr if k == CODE for value in bindings.get(ref, set()))
        bindings.setdefault(toks[start][1], set()).update(values)
    values = set()
    for index, (kind, name, _line) in enumerate(toks[:-1]):
        if kind == CODE and name == "read_dir" and toks[index + 1][1] == "(":
            args = toks[index + 2:group_end(toks, index + 1)]
            values.update(value for value, _ in concat_literals("", args))
            for k, raw, _ in args:
                if k == LITERAL and (value := decode(raw)) is not None:
                    values.add(literal_path(value))
                elif k == CODE:
                    values.update(bindings.get(raw, set()))
    return values


def joined_directory(parts: list[str], values: set[str]) -> bool:
    """Literal directory pieces may be individual joins or slash-joined segments."""
    reachable = {0}
    for start in range(len(parts)):
        if start in reachable:
            reachable.update(end for end in range(start + 1, len(parts) + 1)
                             if "/".join(parts[start:end]) in values)
    return len(parts) in reachable


def concat_literals(path: str, toks: list[tuple[str, str, int]]) -> list[tuple[str, int]]:
    """Literal concat! values, or the known suffix after its last unknown piece."""
    out = []
    for index, (kind, name, line) in enumerate(toks[:-2]):
        if kind == CODE and name == "concat" and toks[index + 1][1] == "!" and toks[index + 2][1] in {"(", "[", "{"}:
            parts = pieces(toks[index:group_end(toks, index + 2) + 1], path, None)
            last_unknown = max((n for n, part in enumerate(parts) if not isinstance(part, str)), default=-1)
            tail = "".join(parts[last_unknown + 1:])
            if tail:
                out.append((literal_path(tail), line))
    return out


def scan(path: str, text: str, changed: set[str]) -> list[str]:
    toks = runtime_tokens(text)
    names = {value for kind, value, _ in toks if kind == CODE}
    if not names & READ_NAMES:
        return []
    literals = [(literal_path(value), line) for kind, raw, line in toks
                if kind == LITERAL and (value := decode(raw)) is not None]
    literals.extend(concat_literals(path, toks))
    values = {value for value, _ in literals}
    enumerated = enumeration_literals(toks) if "read_dir" in names else set()
    hits = []
    for changed_path in sorted(changed):
        parts = changed_path.split("/")
        basename = parts[-1]
        ancestors = {"/".join(parts[:end]) for end in range(1, len(parts))}
        direct = next(((value, line) for value, line in literals
                       if value == changed_path or value.endswith("/" + changed_path)
                       or ("/" in value and changed_path.endswith("/" + value))), None)
        joined = basename in values and joined_directory(parts[:-1], values)
        directory = next(((value, line) for value, line in literals
                          if "read_dir" in names and value in ancestors
                          and ("/" in value or value in enumerated)), None)
        if direct or joined or directory:
            _value, line = direct or directory or next(item for item in literals if item[0] == basename)
            hits.append(f"{path}:{line} runtime reader may read {changed_path}")
    return hits


def runtime_readers(root: str, rev: str, changed: set[str], yaml_inplace: set[str]) -> list[str]:
    """Blocking readers; git/lexer failures propagate so callers must fail closed.

    ``yaml_inplace`` must contain only M YAML paths already proven token-identical
    by the caller. The pinned reader filters out all non-YAML directory entries.
    """
    if not changed:
        return []
    splices = {site for site in tree_sites(root, rev) if site.macro == "include"}
    unresolved = sorted(site.describe() for site in splices if site.target is None)
    if unresolved:
        return [f"{site}: unresolved Rust splice may contain runtime readers" for site in unresolved]
    listed = git_bytes(root, "grep", "-l", "-z", "--full-name", "-E", "|".join(sorted(READ_NAMES)),
                       rev, "--", "*.rs", ok=(0, 1))
    paths = {item.decode().removeprefix(f"{rev}:") for item in listed.split(b"\0") if item}
    paths.update(site.target for site in splices if site.target and not site.target.endswith(".rs"))
    pinned = [path for path, pin in PARSE_ONLY_READERS.items()
              if any(name.startswith(pin.directory + "/") for name in changed)]
    if pinned:
        present = git_bytes(root, "ls-tree", "-z", "--name-only", rev, "--", *pinned)
        paths.update(item.decode() for item in present.split(b"\0") if item)
    blockers = []
    for path, text in read_blobs(root, rev, sorted(paths)).items():
        candidates = changed
        if pin := PARSE_ONLY_READERS.get(path):
            scoped = {name for name in changed if name.startswith(pin.directory + "/")}
            if scoped:
                if hashlib.sha256(text.encode()).hexdigest() != pin.sha256:
                    blockers.append(f"{path}:{pin.read_line} parse-only reader source pin mismatch")
                    continue
                candidates = changed - {name for name in scoped
                                        if not name.endswith(".yaml") or name in yaml_inplace}
        blockers.extend(scan(path, text, candidates))
    return blockers
