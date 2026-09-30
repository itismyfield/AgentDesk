"""Find the files Rust ``include!``/``include_str!``/``include_bytes!`` calls read.

A test that includes a ``.rs`` file sees its comments, so the comment-only gate
keeps the library sweep when a changed file is read this way. An argument this
scanner cannot resolve counts as reading any file whose extension its known
literal tail does not rule out.
"""

from __future__ import annotations

import ast
import posixpath
import re
import subprocess
from dataclasses import dataclass

from rust_lex import CODE, LITERAL, StripState, lex_segments

INCLUDE_MACROS = frozenset({"include", "include_str", "include_bytes"})
TOKEN = re.compile(r"\$?[A-Za-z_][A-Za-z0-9_]*|\S")
CLOSERS = {"(": ")", "[": "]", "{": "}"}
MANIFEST_DIR = object()  # env!("CARGO_MANIFEST_DIR") inside concat!


@dataclass(frozen=True)
class Site:
    path: str  # file holding the call
    line: int
    macro: str
    target: str | None  # repo-relative path read, when resolved
    tail: str  # known literal end of the argument, when unresolved

    def reads(self, changed: str) -> bool:
        if self.target is not None:
            return self.target == changed
        return len(self.tail) < 3 or changed.endswith(self.tail[-3:])

    def describe(self) -> str:
        where = f"{self.path}:{self.line} {self.macro}!"
        return f"{where} -> {self.target}" if self.target is not None else f"{where} (unresolved, tail {self.tail!r})"


def tokens(text: str) -> list[tuple[str, str, int]]:
    """Code tokens and whole literals with their line; comments and spaces drop out."""
    state = StripState()
    out: list[tuple[str, str, int]] = []
    for lineno, line in enumerate(text.splitlines(keepends=True), 1):
        continuing = state.in_string or state.raw_hashes is not None
        for kind, segment in lex_segments(line, state):
            if kind == LITERAL and continuing and out and out[-1][0] == LITERAL:
                out[-1] = (LITERAL, out[-1][1] + segment, out[-1][2])
            elif kind == LITERAL:
                out.append((LITERAL, segment, lineno))
            elif kind == CODE:
                out.extend((CODE, token, lineno) for token in TOKEN.findall(segment))
            continuing = False
    return out


def decode(literal: str) -> str | None:
    """Value of a Rust str literal, or None for byte/char literals and unknown escapes."""
    raw = re.fullmatch(r'r(#*)"(.*)"\1', literal, re.S)
    if raw:
        return raw.group(2)
    if not (literal.startswith('"') and literal.endswith('"')) or len(literal) < 2:
        return None
    body = re.sub(r"\\\r?\n\s*", "", literal[1:-1])
    if re.search(r"\\[^\\\"'nrt0]", body):
        return None
    try:
        return ast.literal_eval('"' + body + '"')
    except (SyntaxError, ValueError):
        return None


def group_end(toks: list[tuple[str, str, int]], start: int) -> int:
    """Index of the token closing the delimiter at ``start``."""
    depth = 0
    for index in range(start, len(toks)):
        kind, value, _line = toks[index]
        if kind != CODE:
            continue
        if value in CLOSERS:
            depth += 1
        elif value in CLOSERS.values():
            depth -= 1
            if depth == 0:
                return index
    raise ValueError("unbalanced macro arguments")


def split_args(toks: list[tuple[str, str, int]]) -> list[list[tuple[str, str, int]]]:
    args: list[list[tuple[str, str, int]]] = [[]]
    depth = 0
    for token in toks:
        kind, value, _line = token
        if kind == CODE and value in CLOSERS:
            depth += 1
        elif kind == CODE and value in CLOSERS.values():
            depth -= 1
        if kind == CODE and value == "," and depth == 0:
            args.append([])
        else:
            args[-1].append(token)
    return [arg for arg in args if arg]


def pieces(arg: list[tuple[str, str, int]], path: str, crate: str | None) -> list[object]:
    """The argument as literal strings, MANIFEST_DIR, and None for unknown parts."""
    if len(arg) == 1 and arg[0][0] == LITERAL:
        return [decode(arg[0][1])]
    is_call = (
        len(arg) >= 4 and arg[0][0] == CODE and arg[1][1] == "!" and arg[2][1] in CLOSERS
    )
    if not is_call or group_end(arg, 2) != len(arg) - 1:
        return [None]
    name, inner = arg[0][1], split_args(arg[3:-1])
    if name == "concat":
        return [piece for item in inner for piece in pieces(item, path, crate)]
    if name == "env" and len(inner) == 1 and pieces(inner[0], path, crate) == ["CARGO_MANIFEST_DIR"]:
        return [MANIFEST_DIR if crate is not None else None]
    if name == "file" and not inner:
        return [path if crate == "" else None]
    return [None]


def resolve(parts: list[object], path: str, crate: str | None, symlinks: set[str]) -> tuple[str | None, str]:
    """(repo-relative target, "") when resolved, else (None, known literal tail).

    The target is "" when the path leaves the repository, where no tracked file lives.
    """
    unknown = [index for index, part in enumerate(parts) if not isinstance(part, str)]
    rooted = unknown == [0] and parts[0] is MANIFEST_DIR
    if unknown and not rooted:
        return None, "".join(part for part in parts[unknown[-1] + 1:] if isinstance(part, str))
    text = "".join(part for part in parts if isinstance(part, str))
    if "\\" in text or text.startswith("/") != rooted:
        return None, text
    stack = [part for part in (crate if rooted else posixpath.dirname(path)).split("/") if part]
    for part in text.split("/"):
        if part in ("", "."):
            continue
        if part == "..":
            if not stack:
                return "", ""
            stack.pop()
            continue
        stack.append(part)
        if "/".join(stack) in symlinks:
            return None, ""  # a link's own name says nothing about the file it reaches
    return "/".join(stack), ""


def crate_dir(path: str, cargo_dirs: set[str]) -> str | None:
    """Nearest directory holding a Cargo.toml, "" for the repository root."""
    parts = path.split("/")[:-1]
    while True:
        candidate = "/".join(parts)
        if candidate in cargo_dirs:
            return candidate
        if not parts:
            return None
        parts.pop()


def scan(path: str, text: str, cargo_dirs: set[str], symlinks: set[str]) -> list[Site]:
    """Every include call in ``text``; a bare macro name (a ``use ... as`` alias) is unresolved."""
    toks = tokens(text)
    crate = crate_dir(path, cargo_dirs)
    sites: list[Site] = []
    for index, (kind, value, line) in enumerate(toks):
        if kind != CODE or value not in INCLUDE_MACROS:
            continue
        is_call = index + 2 < len(toks) and toks[index + 1][1] == "!" and toks[index + 2][1] in CLOSERS
        if not is_call:
            sites.append(Site(path, line, value, None, ""))
            continue
        try:
            args = split_args(toks[index + 3:group_end(toks, index + 2)])
            parts = pieces(args[0], path, crate) if len(args) == 1 else [None]
            target, tail = resolve(parts, path, crate, symlinks)
        except ValueError:
            target, tail = None, ""
        if target != "":
            sites.append(Site(path, line, value, target, tail))
    return sites


def git_bytes(root: str, *args: str, stdin: bytes | None = None, ok: tuple[int, ...] = (0,)) -> bytes:
    result = subprocess.run(["git", "-C", root, *args], input=stdin, capture_output=True, check=False)
    if result.returncode not in ok:
        raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr.decode(errors='replace').strip()}")
    return result.stdout


def read_blobs(root: str, rev: str, paths: list[str]) -> dict[str, str]:
    batch = git_bytes(root, "cat-file", "--batch", stdin="".join(f"{rev}:{path}\n" for path in paths).encode())
    texts: dict[str, str] = {}
    offset = 0
    for path in paths:
        header_end = batch.index(b"\n", offset)
        header = batch[offset:header_end].decode().split(" ")
        if len(header) != 3 or header[1] != "blob":
            raise RuntimeError(f"cat-file {rev}:{path}: {' '.join(header)}")
        size = int(header[2])
        texts[path] = batch[header_end + 1:header_end + 1 + size].decode("utf-8", errors="replace")
        offset = header_end + 1 + size + 1
    return texts


def tree_sites(root: str, rev: str) -> set[Site]:
    """Include calls in every tracked ``.rs`` file of ``rev`` and in non-``.rs`` files ``include!`` splices."""
    cargo_dirs: set[str] = set()
    symlinks: set[str] = set()
    for entry in git_bytes(root, "ls-tree", "-r", "-z", "--full-tree", rev).decode().split("\0"):
        if not entry:
            continue
        meta, path = entry.split("\t", 1)
        if meta.split(" ")[0] == "120000":
            symlinks.add(path)
        if posixpath.basename(path) == "Cargo.toml":
            cargo_dirs.add(posixpath.dirname(path))
    # Every call names a macro containing "include", so other .rs files cannot hold one.
    listed = git_bytes(root, "grep", "-l", "-z", "--full-name", "-F", "-e", "include", rev, "--", "*.rs", ok=(0, 1))
    pending = [item.decode().removeprefix(f"{rev}:") for item in listed.split(b"\0") if item]
    scanned: set[str] = set()
    sites: set[Site] = set()
    while pending:
        scanned.update(pending)
        for path, text in read_blobs(root, rev, pending).items():
            sites.update(scan(path, text, cargo_dirs, symlinks))
        spliced = {site.target for site in sites if site.macro == "include" and site.target}
        pending = sorted(path for path in spliced - scanned if not path.endswith(".rs"))
    return sites
