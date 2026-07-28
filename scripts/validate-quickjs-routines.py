#!/usr/bin/env python3
"""Bounded lexical validator for AgentDesk QuickJS routine roots.

Each JavaScript file must contain a standalone, program-level call through the
unshadowed injected ``agentdesk.routines.register`` authority. Its sole
argument must be an object literal whose effective ``tick`` property is an
inline callable, matching the runtime loader's capture/object/function checks.
Marker text inside comments, quoted strings, template literals,
regular-expression literals, or nested blocks is deliberately ignored. This
is a deploy preflight, not a general JavaScript parser; it recognizes the
narrow registration contract the runtime loads and fails closed on malformed
or oversized input.
"""

from __future__ import annotations

import argparse
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
AUTHORITY_ESCAPE_IDENTIFIERS = {"eval", "Function", "globalThis"}
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
            "...",
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


Token = tuple[str, int]


def _matching_delimiter(
    tokens: list[Token], open_index: int, close_token: str, limit: int | None = None
) -> int | None:
    open_depth = tokens[open_index][1]
    stop = len(tokens) if limit is None else min(limit, len(tokens))
    for index in range(open_index + 1, stop):
        token, depth = tokens[index]
        if token == close_token and depth == open_depth:
            return index
    return None


def _property_starts_at(tokens: list[Token], index: int, object_open: int) -> bool:
    object_depth = tokens[object_open][1] + 1
    if index == object_open + 1:
        return True
    previous, previous_depth = tokens[index - 1]
    return previous == "," and previous_depth == object_depth


def _method_has_body(
    tokens: list[Token], paren_index: int, object_close: int
) -> bool:
    object_depth = tokens[paren_index][1]
    params_close = _matching_delimiter(tokens, paren_index, ")", object_close)
    if params_close is None or params_close + 1 >= object_close:
        return False
    body_open, body_depth = tokens[params_close + 1]
    if body_open != "{" or body_depth != object_depth:
        return False
    return _matching_delimiter(tokens, params_close + 1, "}", object_close + 1) is not None


def _inline_tick_value_is_callable(
    tokens: list[Token], value_index: int, object_depth: int, object_close: int
) -> bool:
    index = value_index
    if index >= object_close:
        return False
    token, depth = tokens[index]
    if depth != object_depth:
        return False

    if token == "async":
        index += 1
        if index >= object_close:
            return False
        token, depth = tokens[index]
        if depth != object_depth:
            return False

    if token == "function":
        index += 1
        if index < object_close and tokens[index] == ("*", object_depth):
            index += 1
        if (
            index < object_close
            and tokens[index][1] == object_depth
            and _is_identifier_start(tokens[index][0][0])
            and tokens[index][0] != "<literal>"
        ):
            index += 1
        if index >= object_close or tokens[index] != ("(", object_depth):
            return False
        return _method_has_body(tokens, index, object_close)

    if token == "(":
        params_close = _matching_delimiter(tokens, index, ")", object_close)
        if params_close is None or params_close + 1 >= object_close:
            return False
        if tokens[params_close + 1] != ("=>", object_depth):
            return False
        body_index = params_close + 2
    elif _is_identifier_start(token[0]) and token != "<literal>":
        if index + 1 >= object_close or tokens[index + 1] != ("=>", object_depth):
            return False
        body_index = index + 2
    else:
        return False

    if body_index >= object_close:
        return False
    body_token, body_depth = tokens[body_index]
    if body_depth < object_depth or (body_token == "," and body_depth == object_depth):
        return False
    if body_token == "{" and body_depth == object_depth:
        return (
            _matching_delimiter(tokens, body_index, "}", object_close + 1) is not None
        )
    return True


def _object_has_callable_tick(
    tokens: list[Token], object_open: int, object_close: int
) -> bool:
    object_depth = tokens[object_open][1] + 1
    last_tick_callable: bool | None = None

    for index in range(object_open + 1, object_close):
        token, depth = tokens[index]
        if depth != object_depth:
            continue
        if token == "...":
            # A spread can replace a previously proven tick property.
            return False
        if token in {"[", "<literal>"} and _property_starts_at(
            tokens, index, object_open
        ):
            # Computed/quoted keys can be another `tick`; fail closed rather
            # than accepting an earlier callable that runtime would overwrite.
            return False
        if token in {"get", "set"} and _property_starts_at(
            tokens, index, object_open
        ):
            if index + 1 < object_close and tokens[index + 1] == (
                "tick",
                object_depth,
            ):
                last_tick_callable = False
            continue
        if token != "tick":
            continue

        key_start = index
        method_prefix = False
        if index > object_open + 1 and tokens[index - 1] in {
            ("async", object_depth),
            ("*", object_depth),
        }:
            key_start = index - 1
            method_prefix = True
            if (
                tokens[index - 1] == ("*", object_depth)
                and key_start > object_open + 1
                and tokens[key_start - 1] == ("async", object_depth)
            ):
                key_start -= 1
        if not _property_starts_at(tokens, key_start, object_open):
            continue
        if index + 1 >= object_close:
            last_tick_callable = False
            continue

        following, following_depth = tokens[index + 1]
        if following_depth != object_depth:
            last_tick_callable = False
        elif following == "(":
            last_tick_callable = _method_has_body(tokens, index + 1, object_close)
        elif following == ":" and not method_prefix:
            last_tick_callable = _inline_tick_value_is_callable(
                tokens, index + 2, object_depth, object_close
            )
        else:
            last_tick_callable = False

    return last_tick_callable is True


def _registration_contract_is_valid(tokens: list[Token]) -> bool:
    authority_refs = [
        index for index, (token, _depth) in enumerate(tokens) if token == "agentdesk"
    ]
    # A standalone call contains the sole executable reference to the injected
    # authority. Any declaration, assignment, or second access can shadow or
    # replace that authority before capture and therefore fails closed.
    if len(authority_refs) != 1:
        return False
    if any(
        (token in AUTHORITY_ESCAPE_IDENTIFIERS or token == "\\") and depth == 0
        for token, depth in tokens
    ):
        return False

    start = authority_refs[0]
    if start + len(TARGET) > len(tokens):
        return False
    target_tokens = tokens[start : start + len(TARGET)]
    if tuple(token for token, _depth in target_tokens) != TARGET:
        return False
    if any(depth != 0 for _token, depth in target_tokens):
        return False
    if start > 0:
        previous, previous_depth = tokens[start - 1]
        if previous not in STATEMENT_BOUNDARIES or previous_depth != 0:
            return False

    call_open = start + len(TARGET) - 1
    call_close = _matching_delimiter(tokens, call_open, ")")
    if call_close is None or call_open + 1 >= call_close:
        return False
    object_open = call_open + 1
    if tokens[object_open] != ("{", 1):
        return False
    object_close = _matching_delimiter(tokens, object_open, "}", call_close)
    if object_close is None:
        return False

    tail = tokens[object_close + 1 : call_close]
    if tail not in ([], [(",", 1)]):
        return False
    if call_close + 1 < len(tokens):
        suffix, suffix_depth = tokens[call_close + 1]
        if suffix != ";" or suffix_depth != 0:
            return False
    return _object_has_callable_tick(tokens, object_open, object_close)


def has_program_level_registration(text: str, display: str) -> bool:
    index = 0
    delimiters: list[str] = []
    regex_allowed = True
    tokens: list[Token] = []

    if text.startswith("#!"):
        newline = text.find("\n")
        index = len(text) if newline < 0 else newline + 1

    def emit(token: str, depth: int) -> None:
        nonlocal regex_allowed
        tokens.append((token, depth))
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
    return _registration_contract_is_valid(tokens)


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
                "no authority-safe program-level agentdesk.routines.register"
                f"({{ tick: <callable> }}) contract: {display}"
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
