"""Bounded observation of post-clear native identity, with an offline clock."""

import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

from .. import assertions, clear_fresh_session


OLD = "00000000-0000-4000-8000-000000000001"
NEW = "00000000-0000-4000-8000-000000000002"


class AfterClearCaptureTests(unittest.TestCase):
    def setUp(self):
        self.elapsed = 0.0
        self.record = {"clear_provider_sessions": {
            "before_clear": {"provider": "claude", "session_id": OLD}}}
        self.enterContext(patch.object(clear_fresh_session, "time", SimpleNamespace(
            monotonic=lambda: self.elapsed, sleep=self.advance), create=True))

    def advance(self, seconds):
        self.assertGreater(seconds, 0)
        self.elapsed += seconds

    def evidence(self, session, **changes):
        return 200, {"provider": "claude", "channel_id": "42",
                     "raw_provider_session_id": session, **changes}

    def capture(self, read, label="after_clear"):
        return clear_fresh_session.capture(read, base_url="http://fixture", channel_id="42",
                                           provider="claude", label=label, record=self.record)

    def assert_changed(self):
        clear_fresh_session.assert_changed(
            {"before": "before_clear", "after": "after_clear"}, record=self.record)

    def test_null_then_new_id_passes(self):
        read = Mock(side_effect=[self.evidence(None), self.evidence(NEW)])
        self.assertEqual(self.capture(read)["session_id"], NEW)
        self.assert_changed()
        self.assertEqual(read.call_count, 2)
        self.assertGreater(self.elapsed, 0)
        self.assertEqual(self.record["clear_provider_sessions"]["before_clear"]["session_id"], OLD)

    def test_null_until_deadline_keeps_observed_id_failure(self):
        read = Mock(return_value=self.evidence(None))
        with self.assertRaisesRegex(assertions.AssertionError,
                                    "^clear requires an observed native provider session ID$"):
            self.capture(read)
        self.assertGreater(read.call_count, 1)
        self.assertEqual(self.elapsed, clear_fresh_session.AFTER_CLEAR_CAPTURE_TIMEOUT_S)
        self.assertLessEqual(self.elapsed, 30)
        self.assertLessEqual(read.call_args.kwargs["timeout"],
                             clear_fresh_session.AFTER_CLEAR_CAPTURE_INTERVAL_S)
        self.assertNotIn("after_clear", self.record["clear_provider_sessions"])

    def test_old_id_immediately_fails_as_resume(self):
        read = Mock(side_effect=[self.evidence(OLD), self.evidence(NEW)])
        self.capture(read)
        with self.assertRaisesRegex(assertions.AssertionError, "resumed the pre-clear"):
            self.assert_changed()
        self.assertEqual(read.call_count, 1)
        self.assertEqual(self.elapsed, 0)

    def test_new_id_read_after_deadline_is_not_fresh(self):
        read = Mock()

        def delayed(*args, **kwargs):
            if read.call_count == 1:
                return self.evidence(None)
            self.elapsed = clear_fresh_session.AFTER_CLEAR_CAPTURE_TIMEOUT_S + 0.01
            return self.evidence(NEW)

        read.side_effect = delayed
        with self.assertRaisesRegex(assertions.AssertionError, "observed native provider session ID"):
            self.capture(read)
        self.assertEqual(read.call_count, 2)
        self.assertNotIn("after_clear", self.record["clear_provider_sessions"])

    def test_unavailable_or_foreign_evidence_fails_without_retry(self):
        for response in ((503, {}), (200, []), self.evidence(None, channel_id="43"),
                         self.evidence(None, provider="codex")):
            with self.subTest(response=response):
                self.elapsed = 0
                read = Mock(side_effect=[self.evidence(None), response, self.evidence(NEW)])
                with self.assertRaisesRegex(assertions.AssertionError, "unavailable or mismatched"):
                    self.capture(read)
                self.assertEqual(read.call_count, 2)
                self.assertEqual(self.elapsed, clear_fresh_session.AFTER_CLEAR_CAPTURE_INTERVAL_S)

    def test_before_clear_null_still_fails_without_retry(self):
        read = Mock(side_effect=[self.evidence(None), self.evidence(NEW)])
        with self.assertRaisesRegex(assertions.AssertionError, "observed native provider session ID"):
            self.capture(read, label="before_clear")
        self.assertEqual(read.call_count, 1)
        self.assertEqual(self.elapsed, 0)


if __name__ == "__main__":
    unittest.main()
