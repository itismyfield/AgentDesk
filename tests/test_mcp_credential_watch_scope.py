"""MCP config monitoring must not subscribe to a native directory event stream."""

from pathlib import Path
import re
import unittest


class McpCredentialWatchScope(unittest.TestCase):
    def test_no_native_filesystem_subscription(self):
        source = (
            Path(__file__).resolve().parents[1]
            / "src/services/discord/mcp_credential_watcher.rs"
        ).read_text()
        self.assertIsNone(
            re.search(r"\b(?:recommended_watcher|RecommendedWatcher|FsEventWatcher|FSEventStreamCreate)\b", source),
            "MCP config monitoring must poll its two files without a native event stream",
        )


if __name__ == "__main__":
    unittest.main()
