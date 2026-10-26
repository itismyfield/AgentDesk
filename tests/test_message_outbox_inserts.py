import tempfile
import unittest
from pathlib import Path

from scripts.check_message_outbox_inserts import audit


class MessageOutboxInsertAuditTests(unittest.TestCase):
    @staticmethod
    def write_insert(root, relative):
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            'sqlx::query("INSERT INTO message_outbox (target) VALUES ($1)");',
            encoding="utf-8",
        )

    def test_replay_hold_notice_exact_helper_is_allowed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write_insert(root, "src/db/replay_disposition/hold_notice/outbox.rs")
            self.assertEqual(audit(root), [])

    def test_replay_hold_notice_adjacent_producers_are_rejected(self):
        relatives = {
            "src/db/replay_disposition/hold_notice/outbox_retry.rs",
            "src/db/replay_disposition/hold_notice/outboxes.rs",
            "src/db/replay_disposition/hold_notice/outbox/producer.rs",
            "src/db/replay_disposition/hold_notice/outbox_tests.rs",
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in relatives:
                self.write_insert(root, relative)
            findings = audit(root)
            self.assertEqual(len(findings), len(relatives))
            self.assertEqual({finding.split(":", 1)[0] for finding in findings}, relatives)

    def test_new_replay_producer_outside_exact_helper_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            relative = "src/db/replay_disposition/hold_notice_extra/outbox.rs"
            self.write_insert(root, relative)
            findings = audit(root)
            self.assertEqual(len(findings), 1)
            self.assertTrue(findings[0].startswith(f"{relative}:"))

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
