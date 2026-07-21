import tempfile
import unittest
from pathlib import Path

from scripts.check_message_outbox_inserts import audit


class MessageOutboxInsertAuditTests(unittest.TestCase):
    def test_new_production_insert_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "src/services/drift.rs"
            path.parent.mkdir(parents=True)
            path.write_text(
                'sqlx::query("INSERT INTO message_outbox (target) VALUES ($1)");',
                encoding="utf-8",
            )
            self.assertEqual(len(audit(root)), 1)

    def test_canonical_modules_are_allowed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in (
                "src/services/message_outbox.rs",
                "src/services/message_outbox_circuit_authority.rs",
            ):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(
                    'sqlx::query("INSERT INTO message_outbox (target) VALUES ($1)");',
                    encoding="utf-8",
                )
            self.assertEqual(audit(root), [])

    def test_circuit_authority_fixture_is_allowed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "src/services/message_outbox_circuit_authority_tests.rs"
            path.parent.mkdir(parents=True)
            path.write_text(
                'sqlx::query("INSERT INTO message_outbox (target) VALUES ($1)");',
                encoding="utf-8",
            )
            self.assertEqual(audit(root), [])


if __name__ == "__main__":
    unittest.main()
