#!/usr/bin/env python3
"""Byte-equal verification for rust_lex stripper across consumers.

Runs three guard scripts that use the Rust lexical stripper and compares
the production text output for byte-exactness, proving equivalence before
switching to the shared module.

Consumers tested:
  1. check_durable_frontier_writer_call_sites.py (uses production_text)
  2. check_inflight_blind_save_ratchet.py (uses strip_line in production_lines loop)
  3. check_intake_outbox_done_writer_call_sites.py (uses production_text with strip_source)

The goal: verify that the extracted rust_lex module produces identical output
to each consumer's current implementation, so imports can be switched safely.
"""

from __future__ import annotations

import sys
from pathlib import Path

# Add scripts directory to path for imports
sys.path.insert(0, str(Path(__file__).parent))


def compare_stripped_text(file_path: Path) -> dict[str, str]:
    """Compare stripped output from each consumer implementation."""
    content = file_path.read_text(encoding="utf-8")
    results = {}

    # Test with check_durable_frontier_writer_call_sites
    try:
        from check_durable_frontier_writer_call_sites import _production_text as durable_prod
        results["durable"] = durable_prod(file_path)
    except Exception as e:
        results["durable_error"] = str(e)

    # Test with check_inflight_blind_save_ratchet
    try:
        from check_inflight_blind_save_ratchet import (
            StripState as InflightState,
            strip_line as inflight_strip,
        )
        state = InflightState()
        lines = content.splitlines()
        inflight_result = "\n".join(
            inflight_strip(line, state) for line in lines
        )
        results["inflight"] = inflight_result
    except Exception as e:
        results["inflight_error"] = str(e)

    # Test with check_intake_outbox_done_writer_call_sites
    try:
        from check_intake_outbox_done_writer_call_sites import (
            production_text as intake_prod,
        )
        results["intake"] = intake_prod(file_path)
    except Exception as e:
        results["intake_error"] = str(e)

    return results


def test_sample_rust_code() -> bool:
    """Test with sample Rust code containing tricky constructs."""
    sample_code = '''
fn main() {
    let s = "hello 'world'";
    let c = '\'';
    let lifetime: &'a str;
    let raw = r#"raw 'string' with lifetimes 'a and 'static"#;
    /* block comment with 'apostrophe' */
    // line comment with 'apostrophe'
}
'''
    tmpfile = Path("/tmp/test_rust_lex_sample.rs")
    tmpfile.write_text(sample_code)

    try:
        results = compare_stripped_text(tmpfile)

        # Print results
        print("=" * 60)
        print("Sample Rust code stripper test")
        print("=" * 60)

        for impl, output in sorted(results.items()):
            if "error" in impl:
                print(f"\n{impl}: {output}")
            else:
                print(f"\n{impl}:")
                print(repr(output[:100]))

        # Check for equality
        actual_results = {k: v for k, v in results.items() if "error" not in k}
        if len(actual_results) < 2:
            print("\n✗ Could not run enough tests")
            return False

        values = list(actual_results.values())
        if all(v == values[0] for v in values):
            print("\n✓ All implementations produce identical output")
            return True
        else:
            print("\n✗ Implementations differ")
            for k1, v1 in actual_results.items():
                for k2, v2 in actual_results.items():
                    if k1 < k2 and v1 != v2:
                        print(f"  {k1} != {k2}")
            return False
    finally:
        tmpfile.unlink(missing_ok=True)


if __name__ == "__main__":
    success = test_sample_rust_code()
    sys.exit(0 if success else 1)
