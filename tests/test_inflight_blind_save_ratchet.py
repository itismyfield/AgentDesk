"""Self-tests for scripts/check_inflight_blind_save_ratchet.py (#4259 PR-1).

Covers the counting contract: (a) production call count, (b) test-file
exclusion, (c) `#[cfg(test)]` module exclusion (incl. a test block sitting
BEFORE a later production call, mirroring turn_bridge/mod.rs), (d) guarded /
suffixed variants not counted, (e) comment/string exclusion — plus the
FAIL/NOTE/OK exit branches and the live-repo baseline agreement.
"""

from __future__ import annotations

import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "check_inflight_blind_save_ratchet.py"

_spec = importlib.util.spec_from_file_location("check_inflight_blind_save_ratchet", SCRIPT)
ratchet = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ratchet)


def _write(root: Path, rel: str, body: str) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")


# A single production file exercising every counting rule. Expected production
# hits are the four lines tagged `// COUNT`.
SAMPLE_RS = """\
// line comment mentioning save_inflight_state(&ignored) must NOT count
/// doc comment mentioning save_inflight_state(&ignored) must NOT count

// the definition itself is not a blind call site
fn save_inflight_state(state: &S) -> Result<(), String> {
    save_inflight_state_in_root(&root, state)
}

fn production() {
    let _ = save_inflight_state(&a);                      // COUNT
    save_inflight_state(&b).expect("save inflight");      // COUNT
    let s = "save_inflight_state(&in_a_string)";          // string, NOT counted
    save_inflight_state_if_identity_unchanged(&g, "who"); // guard variant, NOT counted
    save_inflight_state_if_matches_identity(&g);          // suffix variant, NOT counted
    save_inflight_state(&c); // trailing save_inflight_state(&x) comment  COUNT (once)
}

#[cfg(test)]
mod early_tests {
    fn t() {
        save_inflight_state(&t1).unwrap();  // cfg(test), NOT counted
        save_inflight_state(&t2).unwrap();  // cfg(test), NOT counted
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    fn u() {
        save_inflight_state(&u1).unwrap();  // cfg(all(test, ...)), NOT counted
    }
}

fn after_tests() {
    // a production call AFTER earlier cfg(test) blocks must still count
    let _ = save_inflight_state(&z);                      // COUNT
}
"""


class CountingTests(unittest.TestCase):
    def _count(self, builder) -> tuple[int, list[str]]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            builder(root)
            return ratchet.count_blind_saves(root)

    def test_production_calls_counted_and_variants_comments_strings_defn_excluded(self):
        # (a) production count, (d) variants, (e) comments/strings, definition.
        total, locs = self._count(
            lambda r: _write(r, "src/services/discord/sample.rs", SAMPLE_RS)
        )
        self.assertEqual(total, 4, locs)
        self.assertEqual(len(locs), 4)
        self.assertTrue(all("sample.rs" in loc for loc in locs))

    def test_cfg_test_block_before_production_call_is_excluded_but_call_survives(self):
        # (c) balanced-brace tracking: the early cfg(test) blocks are skipped,
        # yet `after_tests` (past them) is still counted.
        _total, locs = self._count(
            lambda r: _write(r, "src/services/discord/sample.rs", SAMPLE_RS)
        )
        lines = sorted(int(loc.rsplit(":", 1)[1]) for loc in locs)
        # last COUNT (after_tests) is the highest line and must be present.
        self.assertIn(max(lines), lines)
        self.assertEqual(len(lines), 4)

    def test_test_files_excluded_by_name(self):
        # (b) tests.rs / *_tests.rs are never production.
        call = "fn t() { let _ = save_inflight_state(&x); }\n"

        def build(r: Path) -> None:
            _write(r, "src/services/discord/tests.rs", call)
            _write(r, "src/services/discord/widget_tests.rs", call)

        total, locs = self._count(build)
        self.assertEqual(total, 0, locs)

    def test_files_outside_scan_root_are_ignored(self):
        # Scoped to src/services/discord only.
        def build(r: Path) -> None:
            _write(r, "src/other_area/outside.rs", "fn f() { save_inflight_state(&x); }\n")
            _write(r, "src/services/discord/inside.rs", "fn f() { save_inflight_state(&x); }\n")

        total, locs = self._count(build)
        self.assertEqual(total, 1, locs)
        self.assertIn("inside.rs", locs[0])

    def test_guard_variant_only_file_counts_zero(self):
        # (d) a file with only the guarded/suffixed variants -> 0.
        body = (
            "fn f() {\n"
            "    save_inflight_state_if_identity_unchanged(&s, \"c\");\n"
            "    save_inflight_state_in_root(&root, &s);\n"
            "    save_inflight_state_if_matches_identity(&s);\n"
            "}\n"
        )
        total, _ = self._count(
            lambda r: _write(r, "src/services/discord/guarded.rs", body)
        )
        self.assertEqual(total, 0)


class ExitBranchTests(unittest.TestCase):
    """FAIL/NOTE/OK branches, isolated from the real tree via mocking."""

    def _run_main(self, current: int, baseline: int) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(ratchet, "BASELINE", baseline), mock.patch.object(
            ratchet, "count_blind_saves", return_value=(current, ["a.rs:1"] * current)
        ), redirect_stdout(out), redirect_stderr(err):
            code = ratchet.main()
        return code, out.getvalue(), err.getvalue()

    def test_over_baseline_fails(self):
        code, _out, err = self._run_main(current=30, baseline=29)
        self.assertEqual(code, 1)
        self.assertIn("FAIL", err)

    def test_below_baseline_notes_and_passes(self):
        code, out, _err = self._run_main(current=28, baseline=29)
        self.assertEqual(code, 0)
        self.assertIn("NOTE", out)
        self.assertIn("Lower BASELINE to 28", out)

    def test_at_baseline_ok(self):
        code, out, _err = self._run_main(current=29, baseline=29)
        self.assertEqual(code, 0)
        self.assertIn("OK", out)


class LiveRepoBaselineTests(unittest.TestCase):
    def test_live_repo_matches_baseline(self):
        # (a) The real src/services/discord tree matches the frozen BASELINE.
        result = subprocess.run(
            [sys.executable, str(SCRIPT)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"OK: {ratchet.BASELINE}", result.stdout)


if __name__ == "__main__":
    unittest.main()
