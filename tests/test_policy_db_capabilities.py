import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from scripts import check_policy_db_capabilities as checker


class PolicyDbCapabilitiesTest(unittest.TestCase):
    def write(self, root: Path, rel: str, text: str) -> Path:
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def legacy_manifest(self, root: Path, policy: str, callsites: int, fingerprint: str) -> None:
        self.write(
            root,
            f"policies/{policy}.cap.yaml",
            f"""version: 1
policy: {policy}
trust: trusted-automation
source_events:
  - onTick
db:
  raw_sql:
    mode: legacy
    capabilities:
      - legacy_raw_db
    markers_required: false
    no_silent_growth:
      callsites: {callsites}
      fingerprint: {fingerprint}
""",
        )

    def run_checker(self, root: Path, *args: str) -> int:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            return checker.main(["--repo-root", str(root), *args])

    def test_legacy_baseline_accepts_current_callsites(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            policy_path = self.write(
                root,
                "policies/example.js",
                """module.exports = {
  onTick: function() {
    return agentdesk.db.query(
      "SELECT id FROM kanban_cards WHERE id = ?",
      ["card-1"]
    );
  }
};
""",
            )
            callsites = checker.scan_callsites(policy_path, root)
            count, fingerprint = checker.callsite_baseline(callsites)
            self.legacy_manifest(root, "example", count, fingerprint)

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 0)

    def test_legacy_baseline_rejects_new_callsite_growth(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            policy_path = self.write(
                root,
                "policies/example.js",
                """module.exports = {
  onTick: function() {
    agentdesk.db.query("SELECT id FROM kanban_cards");
  }
};
""",
            )
            callsites = checker.scan_callsites(policy_path, root)
            count, fingerprint = checker.callsite_baseline(callsites)
            self.legacy_manifest(root, "example", count, fingerprint)
            policy_path.write_text(
                """module.exports = {
  onTick: function() {
    agentdesk.db.query("SELECT id FROM kanban_cards");
    agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", ["x"]);
  }
};
""",
                encoding="utf-8",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 1)

    def test_legacy_baseline_counts_agentdesk_db_alias_callsites(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            policy_path = self.write(
                root,
                "policies/example.js",
                """module.exports = {
  onTick: function() {
    var db = agentdesk.db;
    db.query("SELECT id FROM kanban_cards");
    db["execute"]("DELETE FROM kv_meta WHERE key = ?", ["x"]);
  }
};
""",
            )
            callsites = checker.scan_callsites(policy_path, root)
            count, fingerprint = checker.callsite_baseline(callsites)
            self.legacy_manifest(root, "example", count, fingerprint)

            self.assertEqual(count, 2)
            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 0)

    def test_legacy_baseline_counts_agentdesk_db_method_alias_callsites(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            policy_path = self.write(
                root,
                "policies/example.js",
                """module.exports = {
  onTick: function() {
    const query = agentdesk["db"].query;
    const execute = agentdesk.db["execute"];
    query("SELECT id FROM kanban_cards");
    execute("DELETE FROM kv_meta WHERE key = ?", ["x"]);
  }
};
""",
            )
            callsites = checker.scan_callsites(policy_path, root)
            count, fingerprint = checker.callsite_baseline(callsites)
            self.legacy_manifest(root, "example", count, fingerprint)

            self.assertEqual(count, 2)
            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 0)

    def test_legacy_baseline_counts_agentdesk_db_method_destructure_callsites(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            policy_path = self.write(
                root,
                "policies/example.js",
                """module.exports = {
  onTick: function() {
    const { query, execute: rawExecute } = agentdesk.db;
    query("SELECT id FROM kanban_cards");
    rawExecute("DELETE FROM kv_meta WHERE key = ?", ["x"]);
  }
};
""",
            )
            callsites = checker.scan_callsites(policy_path, root)
            count, fingerprint = checker.callsite_baseline(callsites)
            self.legacy_manifest(root, "example", count, fingerprint)

            self.assertEqual(count, 2)
            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 0)

    def test_legacy_baseline_rejects_alias_growth_from_zero_literal_callsites(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            policy_path = self.write(
                root,
                "policies/example.js",
                """module.exports = {
  onTick: function() {
    return null;
  }
};
""",
            )
            callsites = checker.scan_callsites(policy_path, root)
            count, fingerprint = checker.callsite_baseline(callsites)
            self.legacy_manifest(root, "example", count, fingerprint)
            policy_path.write_text(
                """module.exports = {
  onTick: function() {
    var db = agentdesk.db;
    return db.query("SELECT id FROM kanban_cards");
  }
};
""",
                encoding="utf-8",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 1)

    def test_marker_mode_requires_runtime_visible_marker_fields(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(
                root,
                "policies/marked.js",
                """module.exports = {
  onTick: function() {
    return agentdesk.db.query(
      "/* legacy-raw-db: policy=marked capability=read_cards source_event=onTick */ " +
      "SELECT id FROM kanban_cards"
    );
  }
};
""",
            )
            self.write(
                root,
                "policies/marked.cap.yaml",
                """version: 1
policy: marked
trust: trusted-automation
source_events:
  - onTick
db:
  raw_sql:
    mode: audited
    capabilities:
      - read_cards
    markers_required: true
""",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 0)

    def test_marker_mode_rejects_static_only_js_comment_marker(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(
                root,
                "policies/marked.js",
                """module.exports = {
  onTick: function() {
    /* legacy-raw-db: policy=marked capability=read_cards source_event=onTick */
    return agentdesk.db.query("SELECT id FROM kanban_cards");
  }
};
""",
            )
            self.write(
                root,
                "policies/marked.cap.yaml",
                """version: 1
policy: marked
trust: trusted-automation
source_events:
  - onTick
db:
  raw_sql:
    mode: audited
    capabilities:
      - read_cards
    markers_required: true
""",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 1)

    def test_marker_mode_rejects_js_comment_inside_call_expression(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(
                root,
                "policies/marked.js",
                """module.exports = {
  onTick: function() {
    return agentdesk.db.query(
      /* legacy-raw-db: policy=marked capability=read_cards source_event=onTick */
      "SELECT id FROM kanban_cards"
    );
  }
};
""",
            )
            self.write(
                root,
                "policies/marked.cap.yaml",
                """version: 1
policy: marked
trust: trusted-automation
source_events:
  - onTick
db:
  raw_sql:
    mode: audited
    capabilities:
      - read_cards
    markers_required: true
""",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 1)

    def test_marker_mode_rejects_marker_in_params_argument(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(
                root,
                "policies/marked.js",
                """module.exports = {
  onTick: function() {
    return agentdesk.db.query(
      "SELECT id FROM kanban_cards WHERE note = ?",
      ["/* legacy-raw-db: policy=marked capability=read_cards source_event=onTick */"]
    );
  }
};
""",
            )
            self.write(
                root,
                "policies/marked.cap.yaml",
                """version: 1
policy: marked
trust: trusted-automation
source_events:
  - onTick
db:
  raw_sql:
    mode: audited
    capabilities:
      - read_cards
    markers_required: true
""",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 1)

    def test_marker_mode_rejects_unmarked_callsite(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(
                root,
                "policies/marked.js",
                """module.exports = {
  onTick: function() {
    return agentdesk.db.query("SELECT id FROM kanban_cards");
  }
};
""",
            )
            self.write(
                root,
                "policies/marked.cap.yaml",
                """version: 1
policy: marked
trust: trusted-automation
source_events:
  - onTick
db:
  raw_sql:
    mode: audited
    capabilities:
      - read_cards
    markers_required: true
""",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 1)

    def test_forbidden_mode_rejects_any_raw_db_callsite(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(
                root,
                "policies/forbidden.js",
                """module.exports = {
  onTick: function() {
    /* legacy-raw-db: policy=forbidden capability=read_cards source_event=onTick */
    return agentdesk.db.query("SELECT id FROM kanban_cards");
  }
};
""",
            )
            self.write(
                root,
                "policies/forbidden.cap.yaml",
                """version: 1
policy: forbidden
trust: trusted-automation
source_events:
  - onTick
db:
  raw_sql:
    mode: forbidden
    capabilities:
      - read_cards
    markers_required: true
""",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 1)

    def test_unknown_raw_sql_mode_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(
                root,
                "policies/bogus.js",
                """module.exports = {
  onTick: function() {
    /* legacy-raw-db: policy=bogus capability=read_cards source_event=onTick */
    return agentdesk.db.query("SELECT id FROM kanban_cards");
  }
};
""",
            )
            self.write(
                root,
                "policies/bogus.cap.yaml",
                """version: 1
policy: bogus
trust: trusted-automation
source_events:
  - onTick
db:
  raw_sql:
    mode: typo
    capabilities:
      - read_cards
    markers_required: true
""",
            )

            self.assertEqual(self.run_checker(root, "--no-silent-growth"), 1)

    def test_required_manifest_missing_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.write(
                root,
                "policies/example.js",
                """module.exports = {
  onTick: function() {
    return null;
  }
};
""",
            )

            self.assertEqual(
                self.run_checker(
                    root,
                    "--no-silent-growth",
                    "--require-manifest",
                    "policies/example.cap.yaml",
                ),
                1,
            )


if __name__ == "__main__":
    unittest.main()
