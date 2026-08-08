from __future__ import annotations

import unittest

from scripts.check_database_fixture_ddl import rust_string_literals


class DatabaseFixtureDdlScannerTests(unittest.TestCase):
    def matched(self, source: str) -> list[str]:
        import re

        pattern = re.compile(r'(?is)^(?:br#*|r#*|b)?"\s*CREATE\s+DATABASE')
        return [literal for literal in rust_string_literals(source) if pattern.search(literal)]

    def test_supported_literal_forms_match(self) -> None:
        for source in [
            '"CREATE DATABASE x"',
            'b" create\n database x"',
            'r"CREATE DATABASE x"',
            'br#"CREATE   DATABASE x"#',
        ]:
            with self.subTest(source=source):
                self.assertEqual(len(self.matched(source)), 1)

    def test_comments_and_nonleading_or_split_sql_do_not_match(self) -> None:
        for source in [
            '// "CREATE DATABASE x"',
            '/* r"CREATE DATABASE x" */',
            '"prefix CREATE DATABASE x"',
            'concat!("CREATE ", "DATABASE x")',
            '"CREATE\\nDATABASE x"',
            "'\"'",
            "b'\"'",
        ]:
            with self.subTest(source=source):
                self.assertEqual(self.matched(source), [])

    def test_nested_comment_cannot_hide_a_real_emission_after_it(self) -> None:
        source = '/* outer /* "CREATE DATABASE hidden" */ done */ r#"DROP TABLE x"#; "CREATE DATABASE live"'
        self.assertEqual(self.matched(source), ['"CREATE DATABASE live"'])


if __name__ == "__main__":
    unittest.main()
