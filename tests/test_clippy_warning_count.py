import json
import unittest
from scripts.check_clippy_warning_count import measure


def stream(*events):
    return "\n".join(json.dumps(event) for event in events)


class WarningCountTest(unittest.TestCase):
    def test_valid_zero_and_warning_count(self):
        finish = {"reason": "build-finished", "success": True}
        warning = {"reason": "compiler-message", "message": {"level": "warning"}}
        self.assertEqual(measure(stream(finish)), 0)
        self.assertEqual(measure(stream(warning, warning, finish)), 2)

    def test_invalid_is_not_zero(self):
        for text in ("", "garbage", "[]", stream({"reason": "build-finished", "success": False}), stream({"reason": "compiler-message"}), stream({"reason": "compiler-message", "message": {"level": "error"}}), stream({"reason": "build-finished", "success": True}, {"reason": "build-finished", "success": True})):
            with self.subTest(text=text), self.assertRaises((ValueError, KeyError, TypeError)):
                measure(text)
