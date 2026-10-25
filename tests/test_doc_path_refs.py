"""Unit tests for scripts/check_doc_path_refs.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "check_doc_path_refs.py"

_SPEC = importlib.util.spec_from_file_location("check_doc_path_refs", SCRIPT_PATH)
CHECKER = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = CHECKER
_SPEC.loader.exec_module(CHECKER)

TREE = {
    "src/services/turn_host.rs": "fn configured_turn() {}\n" * 10,
    "src/services/discord/turn_bridge/mod.rs": "fn a() {}\n" * 5,
    "docs/guide.md": "# Guide\n",
    "docs/generated/route-inventory.md": "`src/gone.rs`\n",
    "policies/lib/helpers.js": "module.exports = {};\n",
}


class RepoCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        for rel, body in TREE.items():
            self.write(rel, body)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write(self, rel: str, body: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(body).lstrip("\n"), encoding="utf-8")

    def findings(self, doc_body: str, doc: str = "docs/page.md") -> list[str]:
        self.write(doc, doc_body)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        subprocess.run(["git", "-C", str(self.root), "add", "-A"], check=True)
        return [f.render() for f in CHECKER.scan_repo(self.root)]


class PathExistenceTest(RepoCase):
    def test_missing_anchored_path_is_reported(self) -> None:
        found = self.findings("See `src/services/kanban.rs` and `src/services/turn_host.rs`.\n")
        self.assertEqual(len(found), 1)
        self.assertIn("src/services/kanban.rs -> path missing", found[0])

    def test_extensionless_token_outside_code_is_prose(self) -> None:
        self.assertEqual(self.findings("Route via the dashboard/API layer.\n"), [])
        found = self.findings("Lives under `src/nowhere/`.\n")
        self.assertIn("src/nowhere -> path missing", found[0])

    def test_glob_needs_its_literal_directory(self) -> None:
        self.assertEqual(self.findings("All of `policies/lib/*.js`.\n"), [])
        found = self.findings("All of `src/integration_tests/postgres_only/*`.\n")
        self.assertIn("directory missing", found[0])

    def test_partial_rust_path_matches_by_suffix(self) -> None:
        self.assertEqual(self.findings("Edit `turn_bridge/mod.rs` only.\n"), [])
        found = self.findings("Edit `dod.rs` only.\n")
        self.assertIn("dod.rs -> no tracked .rs file ends with this", found[0])

    def test_generated_docs_are_out_of_scope(self) -> None:
        self.assertEqual(self.findings("Nothing here.\n"), [])

    def test_checkout_local_output_may_be_named_but_not_linked(self) -> None:
        self.assertEqual(self.findings("Build `docs/generated/module-inventory.md` first.\n"), [])
        found = self.findings("[inventory](generated/module-inventory.md)\n")
        self.assertIn("link target missing", found[0])


class LineRefTest(RepoCase):
    def test_line_past_eof_is_reported(self) -> None:
        found = self.findings("`src/services/turn_host.rs:10` and `src/services/turn_host.rs:11`.\n")
        self.assertEqual(len(found), 1)
        self.assertIn("turn_host.rs:11 -> line past EOF (10 lines)", found[0])

    def test_range_end_and_continuation_refs_are_checked(self) -> None:
        found = self.findings("`src/services/turn_host.rs:9-12`; `turn_host.rs:2`, `:40`.\n")
        self.assertEqual(len(found), 2)
        self.assertIn(":9-12 -> line past EOF", found[0])
        self.assertIn("`:40` -> line past EOF", found[1])

    def test_missing_file_line_ref_is_reported(self) -> None:
        found = self.findings("`dispatches/outbox.rs:334`\n")
        self.assertIn("dispatches/outbox.rs:334 -> file missing", found[0])


class LinkTest(RepoCase):
    def test_relative_link_resolves_from_the_doc(self) -> None:
        self.assertEqual(self.findings("[guide](guide.md) [top](../docs/guide.md#x)\n"), [])
        found = self.findings("[gone](../docs/docs/guide.md)\n")
        self.assertIn("link target missing", found[0])


class MarkerTest(RepoCase):
    def test_inline_markers_accept_absent_paths(self) -> None:
        body = (
            "`src/kanban.rs` (removed) was split; `src/x.rs`(삭제됨);\n"
            "`src/new.rs` (proposed, not created) and `src/y.rs` (missing).\n"
        )
        self.assertEqual(self.findings(body), [])

    def test_doc_level_declaration_covers_partial_mentions(self) -> None:
        body = """
        <!-- doc-path-refs: removed src/services/relay/cohort.rs scripts/old.py -->
        `cohort.rs`, `relay/cohort.rs:105`, and `scripts/old.py:9` are history.
        """
        self.assertEqual(self.findings(body), [])

    def test_declaring_an_existing_path_is_an_error(self) -> None:
        found = self.findings("<!-- doc-path-refs: proposed src/services/turn_host.rs -->\n")
        self.assertIn("declared removed/proposed but exists", found[0])

    def test_external_prefix_is_skipped(self) -> None:
        body = """
        <!-- doc-path-refs: external packages/opencode/ -->
        See `packages/opencode/src/session/prompt.ts:1677-1704`.
        """
        self.assertEqual(self.findings(body), [])

    def test_deprecated_doc_checks_links_only(self) -> None:
        body = """
        ---
        deprecated: true
        ---
        Planned `src/db/schema.rs` and [missing](nope.md).
        """
        found = self.findings(body)
        self.assertEqual(len(found), 1)
        self.assertIn("nope.md -> link target missing", found[0])


class BaselineTest(RepoCase):
    def run_main(self, *args: str) -> tuple[int, str]:
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            code = CHECKER.main(["--root", str(self.root), *args])
        return code, err.getvalue()

    def test_baseline_tolerates_listed_pairs_only(self) -> None:
        self.findings("`src/a.rs`\n`src/b.rs`\n")
        code, err = self.run_main()
        self.assertEqual(code, 1)
        self.assertIn("2 broken doc path reference(s)", err)
        self.write("scripts/doc_path_refs_baseline.txt", "# c\ndocs/page.md\tsrc/a.rs\n")
        code, err = self.run_main()
        self.assertEqual(code, 1)
        self.assertIn("src/b.rs", err)
        self.assertNotIn("src/a.rs", err)

    def test_write_baseline_then_pass(self) -> None:
        self.findings("`src/a.rs`\n")
        self.assertEqual(self.run_main("--write-baseline")[0], 0)
        self.assertEqual(self.run_main()[0], 0)


if __name__ == "__main__":
    unittest.main()
