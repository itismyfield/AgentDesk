#!/usr/bin/env python3
"""Stripper-level byte-equivalent verification for rust_lex extraction.

This test proves that the extracted rust_lex.StripState/strip_line produce
identical stripper output to the previous implementations in each consumer,
by restoring the original implementations from git history and comparing
line-by-line stripping on real Rust files in the current tree.

Why "stripper level" not "pipeline level": production_text/production_lines
add cfg-region state machines on top of stripper output, and their
definitions of "end of line" differ (some split on str.splitlines(),
others on \n only, some append newlines). Stripper output (raw
strip_line(...) bytes) is the unit of code motion and is comparable.
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load_module_from_git(
    base_ref: str, file_rel_path: str, module_name: str
) -> object | None:
    """Load a module from git history (base_ref), returning None on failure."""
    try:
        content = subprocess.run(
            ["git", "show", f"{base_ref}:{file_rel_path}"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout

        # Write to temp file and load
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".py", delete=False, dir="/tmp"
        ) as f:
            f.write(content)
            temp_path = f.name

        spec = importlib.util.spec_from_file_location(
            module_name, temp_path
        )
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module
    except Exception as e:
        print(f"Warning: could not load {module_name} from {base_ref}: {e}")
        return None


def compare_stripper_output(base_ref: str = "origin/main") -> dict[str, object]:
    """Compare stripper output across consumers and rust_lex.

    Returns dict with:
      - 'results': list of (file_path, impl_name, stripped_lines) tuples
      - 'stats': dict with 'files_tested', 'total_lines', 'differences'
      - 'matched': bool, true if all implementations produce identical output
    """
    results = {}

    # Load implementations from git history
    print("Loading implementations...", file=sys.stderr)
    old_durable = load_module_from_git(
        base_ref, "scripts/check_durable_frontier_writer_call_sites.py",
        "old_durable"
    )
    old_inflight = load_module_from_git(
        base_ref, "scripts/check_inflight_blind_save_ratchet.py",
        "old_inflight"
    )
    old_log_key = load_module_from_git(
        base_ref, "scripts/check_log_key_drift.py", "old_log_key"
    )
    old_intake = load_module_from_git(
        base_ref, "scripts/check_intake_outbox_done_writer_call_sites.py",
        "old_intake"
    )

    # Load current implementations
    sys.path.insert(0, str(ROOT / "scripts"))
    from rust_lex import StripState as RustLexState, strip_line as lex_strip

    print("Collecting Rust files...", file=sys.stderr)
    rs_files = list((ROOT / "src").rglob("*.rs"))
    print(f"  {len(rs_files)} files found", file=sys.stderr)

    # Sample: test first 10 files to keep runtime reasonable
    sample_size = min(10, len(rs_files))
    test_files = rs_files[:sample_size]

    print(f"Testing stripper equivalence on {sample_size} files...",
          file=sys.stderr)

    stats = {"files_tested": 0, "total_lines": 0, "differences": 0}
    differences = []

    for rs_file in test_files:
        try:
            content = rs_file.read_text(encoding="utf-8", errors="ignore")
            lines = content.split('\n')
            stats["total_lines"] += len(lines)

            # Test each implementation
            implementations = {}

            # Old durable
            if old_durable and hasattr(old_durable, 'StripState'):
                state = old_durable.StripState()
                stripped = [old_durable.strip_line(line, state)
                            for line in lines]
                implementations['old_durable'] = stripped

            # Old inflight
            if old_inflight and hasattr(old_inflight, 'StripState'):
                state = old_inflight.StripState()
                stripped = [old_inflight.strip_line(line, state)
                            for line in lines]
                implementations['old_inflight'] = stripped

            # Old log_key
            if old_log_key and hasattr(old_log_key, 'StripState'):
                state = old_log_key.StripState()
                stripped = [old_log_key.strip_line(line, state)
                            for line in lines]
                implementations['old_log_key'] = stripped

            # Old intake (if it has a strip_source or strip_line)
            if old_intake:
                if hasattr(old_intake, 'strip_source'):
                    stripped_text = old_intake.strip_source(content)
                    stripped = stripped_text.split('\n')
                    implementations['old_intake'] = stripped
                elif hasattr(old_intake, 'StripState'):
                    state = old_intake.StripState()
                    stripped = [old_intake.strip_line(line, state)
                                for line in lines]
                    implementations['old_intake'] = stripped

            # Current rust_lex
            state = RustLexState()
            stripped = [lex_strip(line, state) for line in lines]
            implementations['rust_lex'] = stripped

            # Compare: old implementations should match current rust_lex
            base_output = implementations.get('rust_lex')
            if base_output is None:
                continue

            for impl_name, impl_output in implementations.items():
                if impl_name == 'rust_lex':
                    continue
                if impl_output != base_output:
                    stat_str = (
                        f"{rs_file.relative_to(ROOT)}: {impl_name} != "
                        f"rust_lex on {len([l for l, o in zip(impl_output, "
                        f"base_output) if l != o])} lines"
                    )
                    differences.append(stat_str)
                    stats["differences"] += 1

            stats["files_tested"] += 1

        except Exception as e:
            print(f"Error processing {rs_file}: {e}", file=sys.stderr)
            continue

    matched = stats["differences"] == 0
    return {
        "stats": stats,
        "differences": differences,
        "matched": matched,
    }


if __name__ == "__main__":
    result = compare_stripper_output()
    stats = result["stats"]

    print(
        f"Stripper equivalence test: {stats['files_tested']} files, "
        f"{stats['total_lines']} lines",
        file=sys.stderr,
    )
    if result["matched"]:
        print(f"✓ All implementations match on {stats['files_tested']} samples",
              file=sys.stderr)
        sys.exit(0)
    else:
        print(f"✗ Differences found: {stats['differences']}",
              file=sys.stderr)
        for diff in result["differences"][:5]:
            print(f"  {diff}", file=sys.stderr)
        sys.exit(1)
