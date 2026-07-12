import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANUAL_REBIND = (
    ROOT / "src/services/discord/recovery_engine/manual_rebind/mod.rs"
).read_text(encoding="utf-8")


class ManualRebindPlatformGuardTests(unittest.TestCase):
    def test_unix_only_watcher_claim_helper_stays_cfg_gated(self) -> None:
        self.assertIn("#[cfg(unix)]\nmod watcher_claim;", MANUAL_REBIND)
        self.assertIn(
            "#[cfg(unix)]\nuse watcher_claim::claim_rebind_watcher;",
            MANUAL_REBIND,
        )


if __name__ == "__main__":
    unittest.main()
