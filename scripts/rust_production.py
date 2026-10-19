"""Rust production-line views shared by call-site and admission guards."""

from __future__ import annotations

import re
from bisect import bisect_right
from pathlib import Path

try:
    from rust_cfg import find_test_only_cfg_attributes as _cfg_test_only_matches
    from rust_lex import StripState, strip_line
except ModuleNotFoundError:
    from scripts.rust_cfg import find_test_only_cfg_attributes as _cfg_test_only_matches
    from scripts.rust_lex import StripState, strip_line

# Anchor item keywords to item positions so function-pointer fields still
# terminate their test-only region at the following comma.
ITEM_START_RE = re.compile(
    r"(?:^|\]|\})\s*"
    r"(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r'(?:extern\s+(?:"[^"]*"\s+)?)?'
    r"(?:fn|impl|mod|trait|macro_rules)\b"
)
# Other pipe prefixes stay uncertain and expose a following body conservatively.
CLOSURE_START_RE = re.compile(r"(?:[=([{,;]|=>|\b(?:move|async|return))\s*$")

def _only_inner_attributes_before(code: str, end: int) -> bool:
    """Whether a prefix contains only whitespace and complete `#![...]` attrs."""
    cursor = 0
    while cursor < end:
        while cursor < end and code[cursor].isspace():
            cursor += 1
        if cursor == end:
            return True
        opening = re.match(r"#!\s*\[", code[cursor:end])
        if opening is None:
            return False
        bracket = code.find("[", cursor, cursor + opening.end())
        depth = 0
        close = None
        for index in range(bracket, end):
            if code[index] == "[":
                depth += 1
            elif code[index] == "]":
                depth -= 1
                if depth == 0:
                    close = index
                    break
        if close is None:
            return False
        cursor = close + 1
    return True


def production_lines(path: Path):
    """Yield ``(lineno, stripped_code, is_production)`` for one Rust file."""
    state = StripState()
    raw_lines = path.read_text(encoding="utf-8").splitlines()
    code_lines = [strip_line(raw, state) for raw in raw_lines]
    line_starts: list[int] = []
    offset = 0
    for code in code_lines:
        line_starts.append(offset)
        offset += len(code) + 1
    full_code = "\n".join(code_lines)
    all_matches = list(_cfg_test_only_matches(full_code))
    file_test_only = any(
        match.is_inner and _only_inner_attributes_before(full_code, match.start())
        for match in all_matches
    )
    arm_matches = {}
    for match in all_matches:
        arm_offset = match.attribute_end() - 1
        line_index = bisect_right(line_starts, arm_offset) - 1
        arm_matches[line_index + 1] = match
    brace_depth = 0
    group_depth = 0  # `(` and `[` nesting
    mode = "normal"  # normal | armed (saw cfg(test) attr) | skip (in test item)
    armed_start_depth = 0
    skip_start_depth = 0
    saw_body_keyword = False
    closure_parameter_depth: int | None = None
    closure_parameters_known = False
    armed_parameter = False
    armed_parameter_known = False
    previous_code = ""
    for lineno, code in enumerate(code_lines, start=1):
        countable = mode == "normal" and not file_test_only
        parameter_mask_start = 0 if mode == "armed" and armed_parameter else None
        arm_at = None
        match = None
        if mode == "normal":
            match = arm_matches.get(lineno)
            if match:
                # Arm after the complete attribute so its own delimiters
                # cannot affect the attributed statement's starting depth.
                line_start = line_starts[lineno - 1]
                arm_at = match.attribute_end() - 1 - line_start
        # Resolve the item-start keyword position BEFORE walking the line: the
        # comma that must not disarm (`-> HashMap<String, u64> {`) sits on the
        # same line as the `fn` that suppresses the comma rule, so deciding at
        # end-of-line is one line too late.
        item_start = ITEM_START_RE.search(code, arm_at if arm_at is not None else 0)
        item_start_at = item_start.end() if item_start else None
        if arm_at is not None and item_start is not None:
            prefix = code[:max(0, match.start() - line_starts[lineno - 1])]
            # Expose mixed lines to admission rather than hide a production API.
            countable = not file_test_only and ITEM_START_RE.search(prefix) is not None
        for index, char in enumerate(code):
            if item_start_at is not None and index >= item_start_at:
                saw_body_keyword = True
            if char in "([":
                group_depth += 1
            elif char in ")]":
                group_depth -= 1
            elif char == "{":
                # An uncertain parameter header cannot make its body test-only.
                if armed_parameter and (
                    not armed_parameter_known or closure_parameter_depth is not None
                ):
                    mode = "normal"
                    countable = True
                    if parameter_mask_start is not None:
                        code = (
                            code[:parameter_mask_start]
                            + " " * (index - parameter_mask_start) + code[index:]
                        )
                if mode == "armed":
                    mode = "skip"
                    skip_start_depth = brace_depth
                closure_parameter_depth = None
                armed_parameter = False
                brace_depth += 1
            elif char == "}":
                brace_depth -= 1
                if mode == "skip" and brace_depth <= skip_start_depth:
                    mode = "normal"
                    if not file_test_only and code[index + 1:].strip(" \t};,"):
                        countable = True
            elif (
                char == ";"
                and mode == "armed"
                and brace_depth + group_depth == armed_start_depth
            ):
                mode = "normal"  # Test-only statements / unbraced items end here.
            elif (
                char == ","
                and mode == "armed"
                and brace_depth + group_depth == armed_start_depth
                and not saw_body_keyword
            ):
                mode = "normal"  # `#[cfg(test)] field: T,` / enum variant
            if (
                char in ")]}"
                and mode == "armed"
                and brace_depth + group_depth < armed_start_depth
            ):
                mode = "normal"
            if (
                char == "|" and mode != "skip"
                and (index == 0 or code[index - 1] != "|")
                and (index + 1 == len(code) or code[index + 1] != "|")
            ):
                depth = brace_depth + group_depth
                if closure_parameter_depth == depth:
                    closure_parameter_depth = None
                    if armed_parameter:
                        mode = "normal"
                        countable = True
                        if parameter_mask_start is not None:
                            end = index + 1
                            code = (
                                code[:parameter_mask_start]
                                + " " * (end - parameter_mask_start) + code[end:]
                            )
                        armed_parameter = False
                elif closure_parameter_depth is None:
                    closure_parameter_depth = depth
                    prefix = code[:index].rstrip() or previous_code
                    closure_parameters_known = bool(CLOSURE_START_RE.search(prefix))
            if closure_parameter_depth is not None and (
                (char == ";" and brace_depth + group_depth <= closure_parameter_depth)
                or (char in ")]}" and brace_depth + group_depth < closure_parameter_depth)
            ):
                closure_parameter_depth = None
                armed_parameter_known = False
            if arm_at is not None and index == arm_at:
                mode = "armed"
                armed_start_depth = brace_depth + group_depth
                saw_body_keyword = False
                armed_parameter = closure_parameter_depth is not None
                armed_parameter_known = closure_parameters_known
                if armed_parameter:
                    parameter_mask_start = max(0, match.start() - line_starts[lineno - 1])
        if code.strip():
            previous_code = code.rstrip()
        yield lineno, code, countable


def _production_text(path: Path) -> str:
    """Return stripped production text while preserving cross-line chains."""

    classified = list(production_lines(path))
    lines = [
        code if countable else " " * len(code)
        for _line, code, countable in classified
    ]
    line_starts: list[int] = []
    offset = 0
    for line in lines:
        line_starts.append(offset)
        offset += len(line) + 1
    full_code = "\n".join(code for _line, code, _countable in classified)
    for match in _cfg_test_only_matches(full_code):
        start_line = bisect_right(line_starts, match.start()) - 1
        end_line = bisect_right(line_starts, match.attribute_end() - 1) - 1
        end = match.attribute_end() - line_starts[end_line]
        if classified[end_line][2] and ITEM_START_RE.search(
            classified[end_line][1], max(0, end - 1)
        ):
            continue  # Keep mixed production/test item lines fail-closed here too.
        if start_line == end_line and classified[end_line][2]:
            start = match.start() - line_starts[start_line]
            lines[end_line] = lines[end_line][:start] + " " * (len(lines[end_line]) - start)
        elif classified[end_line][2]:
            lines[end_line] = " " * len(lines[end_line])
    return "\n".join(lines)
