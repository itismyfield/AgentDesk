#!/usr/bin/env python3
"""One-time PR verifier for the ``rust_lex`` extraction.

This tool loads the four pre-extraction guard implementations from an explicit
git ``--base`` and sweeps every ``src/**/*.rs`` file in the current tree.  It
is intentionally not a CI test: after the PR merges and the base branch moves,
the historical side can become the extracted implementation itself, making
the proof vacuous.

The durable, inflight, and log-key implementations were verbatim copies, so
their per-line output must be byte-for-byte identical to ``rust_lex``.  The
older intake whole-source stripper is reported separately.  The shared
stripper's trailing line-comment truncation is padded back to the historical
intake width before comparison; that is a representation difference outside
the extracted per-line unit.
Every remaining intake difference must match one of two known signatures:

* a source line containing ``b\"`` (the old opener regex left the ``b`` byte
  visible while the shared implementation blanks the whole byte string); or
* an output line-count mismatch corresponding to a source ``'`` + newline +
  ``'`` span (the old char-literal regex incorrectly matched across a newline
  and replaced that line boundary with a space).

The intake classifier is deliberately narrow and lexical.  It proves only
that every observed mismatch on this tree has one of those signatures; it
does not prove semantic equivalence for arbitrary Rust, distinguish a real
byte string from the same spelling in every malformed context, or generally
validate Rust syntax.  Any unclassified difference fails the run.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STRICT_IMPLEMENTATIONS = {
    "old_durable": "scripts/check_durable_frontier_writer_call_sites.py",
    "old_inflight": "scripts/check_inflight_blind_save_ratchet.py",
    "old_log_key": "scripts/check_log_key_drift.py",
}
INTAKE_PATH = "scripts/check_intake_outbox_done_writer_call_sites.py"
CHAR_NEWLINE_SPAN_RE = re.compile(r"'\n'")


def load_module_from_git(base_ref: str, file_rel_path: str, module_name: str) -> object:
    """Execute one historical module with its original path as ``__file__``."""
    try:
        content = subprocess.run(
            ["git", "show", f"{base_ref}:{file_rel_path}"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        module = types.ModuleType(module_name)
        module.__file__ = str(ROOT / file_rel_path)
        module.__package__ = ""
        sys.modules[module_name] = module
        exec(compile(content, module.__file__, "exec"), module.__dict__)
        return module
    except Exception as exc:
        raise RuntimeError(
            f"could not load {module_name} from {base_ref}:{file_rel_path}: {exc}"
        ) from exc


def strip_with_line_implementation(module: object, lines: list[str]) -> list[str]:
    """Run a historical or current ``StripState``/``strip_line`` pair."""
    state = module.StripState()
    return [module.strip_line(line, state) for line in lines]


def intake_compatible_current_output(module: object, lines: list[str]) -> str:
    """Run rust_lex and restore only old intake's trailing comment padding."""
    state = module.StripState()
    output: list[str] = []
    for line in lines:
        stripped = module.strip_line(line, state)
        output.append(stripped + " " * (len(line) - len(stripped)))
    return "\n".join(output)


def source_line_number(text: str, offset: int) -> int:
    """Return the one-based source line containing ``offset``."""
    return text.count("\n", 0, offset) + 1


def classify_intake_differences(
    source: str, old_output: str, new_output: str
) -> tuple[set[int], int, list[int]]:
    """Classify intake mismatch offsets; return byte lines, spans, unknowns."""
    if len(old_output) != len(new_output) or len(new_output) != len(source):
        return set(), 0, [-1]

    mismatch_offsets = {
        offset
        for offset, (old_char, new_char) in enumerate(zip(old_output, new_output))
        if old_char != new_char
    }
    byte_string_lines: set[int] = set()
    classified_offsets: set[int] = set()

    line_start = 0
    for line_number, line in enumerate(source.split("\n"), start=1):
        line_end = line_start + len(line)
        line_offsets = {
            offset
            for offset in mismatch_offsets
            if line_start <= offset < line_end
        }
        if line_offsets and 'b"' in line:
            byte_string_lines.add(line_number)
            classified_offsets.update(line_offsets)
        line_start = line_end + 1

    span_count = 0
    old_line_count = len(old_output.split("\n"))
    new_line_count = len(new_output.split("\n"))
    if old_line_count != new_line_count:
        for match in CHAR_NEWLINE_SPAN_RE.finditer(source):
            span_offsets = set(range(match.start(), match.end()))
            if mismatch_offsets & span_offsets:
                span_count += 1
                classified_offsets.update(mismatch_offsets & span_offsets)

    return (
        byte_string_lines,
        span_count,
        sorted(mismatch_offsets - classified_offsets),
    )


def compare_stripper_output(base_ref: str) -> dict[str, object]:
    """Sweep all Rust files and return strict and classified intake totals."""
    sys.path.insert(0, str(ROOT / "scripts"))
    import rust_lex

    print(f"Loading historical implementations from {base_ref}...")
    strict_modules = {
        name: load_module_from_git(base_ref, path, name)
        for name, path in STRICT_IMPLEMENTATIONS.items()
    }
    old_intake = load_module_from_git(base_ref, INTAKE_PATH, "old_intake")

    rs_files = sorted((ROOT / "src").rglob("*.rs"))
    print(f"Sweeping all Rust files: {len(rs_files)} files")

    strict_diff_lines = {name: 0 for name in strict_modules}
    intake_byte_lines = 0
    intake_char_spans = 0
    intake_unclassified: list[str] = []
    total_lines = 0

    for rs_file in rs_files:
        source = rs_file.read_text(encoding="utf-8")
        lines = source.split("\n")
        total_lines += len(lines)
        current_lines = strip_with_line_implementation(rust_lex, lines)

        for name, module in strict_modules.items():
            old_lines = strip_with_line_implementation(module, lines)
            diff_count = sum(
                old_line != current_line
                for old_line, current_line in zip(old_lines, current_lines)
            ) + abs(len(old_lines) - len(current_lines))
            strict_diff_lines[name] += diff_count

        old_intake_output = old_intake.strip_source(source)
        current_intake_output = intake_compatible_current_output(rust_lex, lines)
        if old_intake_output == current_intake_output:
            continue

        byte_lines, char_spans, unknown_offsets = classify_intake_differences(
            source, old_intake_output, current_intake_output
        )
        intake_byte_lines += len(byte_lines)
        intake_char_spans += char_spans
        if unknown_offsets:
            rel = rs_file.relative_to(ROOT).as_posix()
            rendered = ", ".join(
                "length mismatch"
                if offset == -1
                else f"line {source_line_number(source, offset)} offset {offset}"
                for offset in unknown_offsets[:10]
            )
            intake_unclassified.append(f"{rel}: {rendered}")

    return {
        "files_tested": len(rs_files),
        "total_lines": total_lines,
        "strict_diff_lines": strict_diff_lines,
        "intake_byte_string_diff_lines": intake_byte_lines,
        "intake_char_newline_spans": intake_char_spans,
        "intake_unclassified": intake_unclassified,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base",
        required=True,
        help="git ref containing the four pre-extraction implementations",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        stats = compare_stripper_output(args.base)
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1

    print(
        f"Tree sweep: {stats['files_tested']} files, "
        f"{stats['total_lines']} lines"
    )
    print("Strict verbatim-copy equivalence (differing lines):")
    for name, count in stats["strict_diff_lines"].items():
        print(f"  {name}: {count}")
    print("Old intake classified differences:")
    print(
        '  byte-string lines (source contains b"): '
        f"{stats['intake_byte_string_diff_lines']}"
    )
    print(
        "  char-newline spans (old output deleted a line boundary): "
        f"{stats['intake_char_newline_spans']}"
    )
    unclassified = stats["intake_unclassified"]
    print(f"  unclassified: {len(unclassified)}")
    for difference in unclassified[:20]:
        print(f"    {difference}")

    strict_ok = all(count == 0 for count in stats["strict_diff_lines"].values())
    if strict_ok and not unclassified:
        print("PASS: strict copies match and every intake difference is classified")
        return 0
    print("FAIL: strict mismatch or unclassified intake difference", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
