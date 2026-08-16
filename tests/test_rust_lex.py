"""Unit tests for scripts.rust_lex cross-line Rust lexical stripper."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

# Load rust_lex module
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "scripts"))
from rust_lex import StripState, strip_line


class RustLexStripperTests(unittest.TestCase):
    """Test rust_lex.StripState and strip_line() stripper behavior."""

    def test_strip_line_with_state(self):
        """StripState and strip_line should export correctly."""
        state = StripState()
        self.assertIsNotNone(state)
        # line-comment: rest of line should be blanked
        result = strip_line("let x = 1; // comment", state)
        self.assertTrue(result.startswith("let x = 1;"))
        self.assertTrue(all(c == ' ' for c in result[11:]))

    def test_string_literal_blanked(self):
        """String literals should be blanked."""
        state = StripState()
        result = strip_line('let s = "hello";', state)
        # Quote opens, content blanked
        self.assertEqual(result[8], ' ')  # start of blank
        self.assertEqual(result[-1], ';')  # semicolon preserved

    def test_char_literal_blanked(self):
        """Char literals 'x' should be blanked."""
        state = StripState()
        result = strip_line("let c = 'x';", state)
        # Char literal should be blanked
        self.assertEqual(result[0:8], "let c = ")
        self.assertEqual(result[-1], ';')

    def test_lifetime_preserved(self):
        """Lifetimes like 'a should NOT be blanked."""
        state = StripState()
        result = strip_line("let x: &'a str;", state)
        # Lifetime should be preserved (no closing quote immediately follows)
        self.assertIn("'a", result)

    def test_raw_string_blanked(self):
        """Raw strings r"…" should be blanked."""
        state = StripState()
        result = strip_line('let s = r"raw";', state)
        # Raw string should be blanked
        self.assertEqual(result[0:8], "let s = ")
        self.assertTrue(any(c == ' ' for c in result[8:14]))

    def test_block_comment_blanked(self):
        """Block comments should be blanked."""
        state = StripState()
        result = strip_line("let x = /* comment */ 1;", state)
        # Block comment region should be blanked
        self.assertTrue(any(c == ' ' for c in result[8:22]))
        self.assertTrue(result.endswith("1;"))

    def test_escaped_char_literal(self):
        """Escaped char literals like '\\n' should be blanked (raw string r\"\n\")."""
        state = StripState()
        # Use raw string to ensure backslash is literal
        result = strip_line(r"let c = '\n';", state)
        # Should be blanked
        self.assertEqual(result[0:8], "let c = ")

    def test_multiline_string_state_carry(self):
        """Multi-line strings should track state across lines."""
        state = StripState()
        # First line: quote opens, content blanked
        result1 = strip_line('let s = "start', state)
        # After quote, content is blanked
        self.assertTrue(all(c == ' ' for c in result1[8:]))
        # Inside string, state should be active
        self.assertTrue(state.in_string)
        # Middle line: all blanked
        result2 = strip_line('middle', state)
        self.assertTrue(all(c == ' ' for c in result2))
        # Last line: blanked until close
        result3 = strip_line('end";', state)
        self.assertFalse(state.in_string)

    def test_nested_block_comment(self):
        """Nested block comments should track depth."""
        state = StripState()
        result = strip_line("/* outer /* inner */ still */", state)
        # All should be blanked
        self.assertTrue(all(c == ' ' for c in result))

    def test_byte_string_blanked(self):
        """Byte strings b"…" should be fully blanked."""
        state = StripState()
        result = strip_line('let b = b"test";', state)
        # After b quote should be blanked
        self.assertTrue(any(c == ' ' for c in result[8:]))


if __name__ == "__main__":
    unittest.main()
