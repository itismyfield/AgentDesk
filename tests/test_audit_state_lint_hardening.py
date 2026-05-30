from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "audit_state_lint_hardening.py"

_SPEC = importlib.util.spec_from_file_location("audit_state_lint_hardening", SCRIPT_PATH)
AUDIT = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = AUDIT
_SPEC.loader.exec_module(AUDIT)


class MigrationIntegerAuditTests(unittest.TestCase):
    def test_add_column_if_not_exists_integer_is_flagged(self) -> None:
        fixture = AUDIT.AddedLine(
            "migrations/postgres/0099_fixture.sql",
            7,
            "ALTER TABLE pr_tracking ADD COLUMN IF NOT EXISTS retry_count INTEGER NOT NULL DEFAULT 0;",
        )

        findings = AUDIT.audit_migration_integers([fixture])

        self.assertEqual(len(findings), 1)
        self.assertIn("retry_count INTEGER", findings[0])
        self.assertIn("use BIGINT", findings[0])


if __name__ == "__main__":
    unittest.main()
