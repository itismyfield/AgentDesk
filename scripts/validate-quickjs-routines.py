#!/usr/bin/env python3
"""Bounded lexical validator for AgentDesk QuickJS routine roots.

Each JavaScript file must contain a standalone, program-level
``agentdesk.routines.register(...)`` call.  Marker text inside comments,
quoted strings, template literals, regular-expression literals, or nested
blocks is deliberately ignored.  This is a deploy preflight, not a general
JavaScript parser; it recognizes the narrow registration contract the runtime
loads and fails closed on malformed or oversized input.
"""

from __future__ import annotations

import argparse
import collections
import os
import sys
from pathlib import Path


MAX_FILES = 256
MAX_TREE_ENTRIES = 4096
MAX_TREE_DEPTH = 64
MAX_FILE_BYTES = 2 * 1024 * 1024
MAX_TOTAL_BYTES = 16 * 1024 * 1024
TARGET = ("agentdesk", ".", "routines", ".", "register", "(")
STATEMENT_BOUNDARIES = {";", "}"}
REGEX_PREFIX_KEYWORDS = {
    "await",
    "case",
    "delete",
    "do",
    "else",
    "in",
    "instanceof",
    "new",
    "return",
    "throw",
    "typeof",
    "void",
    "yield",
}
REGEX_PREFIX_PUNCTUATORS = {
    "(",
    "[",
    "{",
    ",",
    ";",
    ":",
    "=",
    "==",
    "===",
    "!=",
    "!==",
    "!",
    "?",
    "??",
    "+",
    "-",
    "*",
    "%",
    "&",
    "&&",
    "|",
    "||",
    "^",
    "~",
    "<",
    ">",
    "<=",
    ">=",
    "=>",
}


class ValidationError(Exception):
    """A deterministic, user-facing validation failure."""


def _is_identifier_start(char: str) -> bool:
    return char == "_" or char == "$" or char.isalpha()


def _is_identifier_continue(char: str) -> bool:
    return _is_identifier_start(char) or char.isdigit()


def _skip_quoted(text: str, start: int, quote: str, display: str) -> int:
    index = start + 1
    while index < len(text):
        char = text[index]
        if char == "\\":
            index += 2
            continue
        if char == quote:
            return index + 1
        if quote != "`" and char in "\r\n":
            raise ValidationError(f"{display}: newline in quoted string")
        index += 1
    kind = "template literal" if quote == "`" else "quoted string"
    raise ValidationError(f"{display}: unterminated {kind}")


def _skip_regex(text: str, start: int, display: str) -> int:
    index = start + 1
    in_character_class = False
    while index < len(text):
        char = text[index]
        if char == "\\":
            index += 2
            continue
        if char in "\r\n":
            raise ValidationError(f"{display}: unterminated regular-expression literal")
        if char == "[":
            in_character_class = True
        elif char == "]":
            in_character_class = False
        elif char == "/" and not in_character_class:
            index += 1
            while index < len(text) and _is_identifier_continue(text[index]):
                index += 1
            return index
        index += 1
    raise ValidationError(f"{display}: unterminated regular-expression literal")


def _punctuator(text: str, index: int) -> tuple[str, int]:
    for width in (4, 3, 2):
        candidate = text[index : index + width]
        if candidate in {
            ">>>=",
            "===",
            "!==",
            ">>>",
            "**=",
            "&&=",
            "||=",
            "??=",
            "=>",
            "==",
            "!=",
            "<=",
            ">=",
            "++",
            "--",
            "&&",
            "||",
            "??",
            "**",
            "<<",
            ">>",
            "+=",
            "-=",
            "*=",
            "%=",
            "&=",
            "|=",
            "^=",
            "?.",
        }:
            return candidate, index + width
    return text[index], index + 1


def has_program_level_registration(text: str, display: str) -> bool:
    index = 0
    delimiters: list[str] = []
    found_registration = False
    regex_allowed = True
    recent: collections.deque[tuple[str, int]] = collections.deque(maxlen=7)

    if text.startswith("#!"):
        newline = text.find("\n")
        index = len(text) if newline < 0 else newline + 1

    def emit(token: str, depth: int) -> None:
        nonlocal found_registration, regex_allowed
        recent.append((token, depth))
        if len(recent) >= len(TARGET):
            tail = tuple(item[0] for item in list(recent)[-len(TARGET) :])
            depths = tuple(item[1] for item in list(recent)[-len(TARGET) :])
            if tail == TARGET and all(value == 0 for value in depths):
                prefix = list(recent)[: -len(TARGET)]
                if not prefix or prefix[-1][0] in STATEMENT_BOUNDARIES:
                    found_registration = True
        if _is_identifier_start(token[0]):
            regex_allowed = token in REGEX_PREFIX_KEYWORDS
        elif token in {"<literal>", ")", "]", "}", "++", "--"}:
            regex_allowed = False
        else:
            regex_allowed = token in REGEX_PREFIX_PUNCTUATORS
    while index < len(text):
        char = text[index]
        if char.isspace() or char == "\ufeff":
            index += 1
            continue

        if char == "/" and index + 1 < len(text):
            following = text[index + 1]
            if following == "/":
                newline = text.find("\n", index + 2)
                index = len(text) if newline < 0 else newline + 1
                continue
            if following == "*":
                end = text.find("*/", index + 2)
                if end < 0:
                    raise ValidationError(f"{display}: unterminated block comment")
                index = end + 2
                continue
            if regex_allowed:
                index = _skip_regex(text, index, display)
                emit("<literal>", len(delimiters))
                continue

        if char in {"'", '"', "`"}:
            index = _skip_quoted(text, index, char, display)
            emit("<literal>", len(delimiters))
            continue

        if _is_identifier_start(char):
            end = index + 1
            while end < len(text) and _is_identifier_continue(text[end]):
                end += 1
            emit(text[index:end], len(delimiters))
            index = end
            continue

        if char.isdigit():
            end = index + 1
            while end < len(text) and (text[end].isalnum() or text[end] in "._"):
                end += 1
            emit("<literal>", len(delimiters))
            index = end
            continue

        token, index = _punctuator(text, index)
        if token in {')', ']', '}'}:
            expected = {')': '(', ']': '[', '}': '{'}[token]
            if not delimiters or delimiters[-1] != expected:
                raise ValidationError(f"{display}: unmatched closing delimiter {token}")
            delimiters.pop()
        emit(token, len(delimiters))
        if token in {'(', '[', '{'}:
            delimiters.append(token)

    if delimiters:
        raise ValidationError(f"{display}: unbalanced delimiter {delimiters[-1]}")
    return found_registration


def discover_scripts(root: Path) -> list[Path]:
    scripts: list[Path] = []
    pending: list[tuple[Path, int]] = [(root, 0)]
    entries_seen = 0

    while pending:
        directory, depth = pending.pop()
        try:
            entries = os.scandir(directory)
        except OSError as error:
            raise ValidationError(f"cannot scan routine directory {directory}: {error}") from error
        with entries:
            for entry in entries:
                entries_seen += 1
                if entries_seen > MAX_TREE_ENTRIES:
                    raise ValidationError(
                        f"routine root exceeds tree entry cap ({MAX_TREE_ENTRIES}): {root}"
                    )
                path = Path(entry.path)
                if entry.is_symlink():
                    raise ValidationError(f"symlink in routine root is forbidden: {path}")
                if entry.is_dir(follow_symlinks=False):
                    if depth >= MAX_TREE_DEPTH:
                        raise ValidationError(
                            f"routine root exceeds directory depth cap ({MAX_TREE_DEPTH}): {path}"
                        )
                    pending.append((path, depth + 1))
                elif entry.is_file(follow_symlinks=False) and path.suffix == ".js":
                    scripts.append(path)
                    if len(scripts) > MAX_FILES:
                        raise ValidationError(
                            f"routine root exceeds file cap ({MAX_FILES}): {root}"
                        )
    return sorted(scripts)


def validate_root(root: Path) -> None:
    if root.is_symlink() or not root.is_dir():
        raise ValidationError(f"required routine root is not a regular directory: {root}")

    scripts = discover_scripts(root)
    if not scripts:
        raise ValidationError(f"routine root contains no JavaScript entrypoints: {root}")
    total_bytes = 0
    for script in scripts:
        if script.is_symlink():
            raise ValidationError(f"symlinked routine entrypoint is forbidden: {script}")
        advertised_size = script.stat().st_size
        if advertised_size > MAX_FILE_BYTES:
            raise ValidationError(
                "routine entrypoint exceeds byte cap "
                f"({advertised_size} > {MAX_FILE_BYTES}): {script}"
            )
        with script.open("rb") as handle:
            payload = handle.read(MAX_FILE_BYTES + 1)
        if len(payload) > MAX_FILE_BYTES:
            raise ValidationError(
                "routine entrypoint grew beyond byte cap while reading "
                f"({len(payload)} > {MAX_FILE_BYTES}): {script}"
            )
        total_bytes += len(payload)
        if total_bytes > MAX_TOTAL_BYTES:
            raise ValidationError(
                f"routine root exceeds total byte cap ({total_bytes} > {MAX_TOTAL_BYTES}): {root}"
            )
        try:
            text = payload.decode("utf-8")
        except UnicodeError as error:
            raise ValidationError(f"routine entrypoint is not UTF-8: {script}: {error}") from error
        if "\x00" in text:
            raise ValidationError(f"routine entrypoint contains NUL: {script}")
        display = script.relative_to(root).as_posix()
        if not has_program_level_registration(text, display):
            raise ValidationError(
                f"no standalone program-level agentdesk.routines.register call: {display}"
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    try:
        validate_root(args.root)
    except (OSError, ValidationError) as error:
        print(f"QuickJS routine validation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
