#!/usr/bin/env python3
"""Focused safety and smoke tests for issue #4555's toolchain routine."""

from __future__ import annotations

import json
import plistlib
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Sequence
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import toolchain_update as update  # noqa: E402
from toolchain_manifest import tool_inventory  # noqa: E402


class FakeRunner(update.Runner):
    def __init__(self) -> None:
        self.calls: list[tuple[str, ...]] = []
        self.urls: list[str] = []
        self.overrides: dict[tuple[str, ...], update.CommandResult] = {}
        self.sequence_overrides: dict[tuple[str, ...], list[update.CommandResult]] = {}

    def run(
        self,
        argv: Sequence[str],
        *,
        timeout: int = update.DEFAULT_TIMEOUT_SECONDS,
        env: Mapping[str, str] | None = None,
    ) -> update.CommandResult:
        del timeout, env
        command = tuple(argv)
        self.calls.append(command)
        if command in self.sequence_overrides and self.sequence_overrides[command]:
            return self.sequence_overrides[command].pop(0)
        if command in self.overrides:
            return self.overrides[command]
        if command == ("agentdesk", "status", "--json"):
            return update.CommandResult(
                0,
                json.dumps(
                    {
                        "sessions": {"working": 0, "with_active_dispatch": 0},
                        "queue": {"status": "idle"},
                    }
                ),
                "",
            )
        if command[:2] == ("pgrep", "-x"):
            return update.CommandResult(1, "", "")
        if command == ("ps", "-axo", "command="):
            return update.CommandResult(0, "launchd\nagentdesk dcserver\n", "")
        if command[:3] == ("brew", "list", "--versions"):
            return update.CommandResult(0, f"{command[-1]} 1.0.0\n", "")
        if command[:3] == ("brew", "info", "--json=v2"):
            return update.CommandResult(
                0,
                json.dumps({"formulae": [{"versions": {"stable": "1.1.0"}}]}),
                "",
            )
        if command[:2] == ("npm", "view"):
            return update.CommandResult(0, '"1.1.0"\n', "")
        if command == ("rustup", "check"):
            return update.CommandResult(0, "stable-aarch64 - Update available : 1.1.0\n", "")
        if command[:2] in {
            ("npm", "install"),
            ("brew", "upgrade"),
            ("pipx", "install"),
            ("claude", "update"),
            ("opencode", "upgrade"),
            ("rustup", "update"),
        } or command[:3] == ("uv", "tool", "install"):
            return update.CommandResult(0, "updated\n", "")
        if command == ("cswap", "--list", "--json"):
            return update.CommandResult(
                0,
                '{"schemaVersion":1,"accounts":[{"number":1,"active":true,"usageAgeSeconds":5.4}]}',
                "",
            )
        if command == ("ocx", "health"):
            return update.CommandResult(0, "healthy\n", "")
        if command == ("npm", "ls", "-g", "--depth=0", "--json"):
            return update.CommandResult(0, '{"dependencies":{}}', "")
        if command and command[0].endswith("SidecarLauncher"):
            return update.CommandResult(0, "iPad\n", "")
        return update.CommandResult(0, "tool 1.0.0\n", "")

    def get_json(
        self,
        url: str,
        *,
        timeout: int = 5,
        headers: Mapping[str, str] | None = None,
    ) -> Any:
        del timeout, headers
        self.urls.append(url)
        if url.endswith("/health"):
            return {"ok": True, "version": "1.0.0"}
        return {"info": {"version": "1.1.0"}}


def check_for(key: str, *, tier: str, method: str) -> update.ToolCheck:
    spec = next(item for item in tool_inventory() if item.key == key)
    return update.ToolCheck(
        key=key,
        display_name=spec.display_name,
        method=method,
        tier=tier,
        current="1.0.0",
        latest="1.1.0",
        decision="update-available",
        current_detail="tool 1.0.0",
        latest_detail="registry 1.1.0",
        risk=spec.risk,
        changelog_url=spec.changelog_url,
        report_only=False,
    )


class InventoryAndDraftTests(unittest.TestCase):
    def test_inventory_matches_issue_4555_without_silent_omissions(self) -> None:
        inventory = tool_inventory()
        self.assertEqual(
            {spec.key for spec in inventory},
            {
                "claude",
                "codex",
                "ocx",
                "claude-e",
                "cswap",
                "cargo-rustc",
                "tmux",
                "gh",
                "node",
                "python-3-14",
                "uv",
                "pipx",
                "jq",
                "ripgrep",
                "ffmpeg",
                "whisper-cpp",
                "postgresql-17",
                "edge-tts",
                "opencode",
                "memento-mcp",
                "brave-search-mcp",
                "sidecar-launcher",
                "playwright-chromium",
            },
        )
        self.assertEqual(
            {spec.method for spec in inventory},
            {
                "native",
                "npm-g",
                "uv-tool",
                "rustup",
                "homebrew",
                "pipx",
                "installer",
                "remote-service",
                "npx-always-latest",
                "manual",
            },
        )
        self.assertEqual(len(inventory), len({spec.key for spec in inventory}))

    def test_check_writes_every_row_without_any_update_command(self) -> None:
        runner = FakeRunner()
        checks = update.collect_checks(runner)
        with tempfile.TemporaryDirectory() as temp:
            markdown, json_path, draft_id = update.write_draft(
                checks,
                Path(temp),
                now=datetime(2026, 7, 15, tzinfo=timezone.utc),
            )
            report = markdown.read_text(encoding="utf-8")
            payload = json.loads(json_path.read_text(encoding="utf-8"))

        self.assertEqual(len(checks), len(tool_inventory()))
        self.assertEqual(payload["draft_id"], draft_id)
        self.assertEqual(len(payload["checks"]), len(tool_inventory()))
        for spec in tool_inventory():
            self.assertIn(spec.display_name, report)
        mutating_prefixes = {
            ("npm", "install"),
            ("brew", "upgrade"),
            ("rustup", "update"),
            ("pipx", "install"),
            ("claude", "update"),
            ("opencode", "upgrade"),
        }
        self.assertFalse(
            any(call[:2] in mutating_prefixes or call[:3] == ("uv", "tool", "install") for call in runner.calls),
            runner.calls,
        )
        self.assertIn("No update command was executed", report)

    def test_offline_check_skips_all_http_including_remote_memento(self) -> None:
        runner = FakeRunner()
        checks = update.collect_checks(runner, offline=True)
        memento = next(check for check in checks if check.key == "memento-mcp")
        self.assertEqual(runner.urls, [])
        self.assertEqual(memento.current, "offline/not-queried")

    def test_launchd_schedule_can_only_enter_check_path(self) -> None:
        plist_path = ROOT / "scripts" / "launchd" / "com.agentdesk.toolchain-update.plist"
        with plist_path.open("rb") as stream:
            plist = plistlib.load(stream)
        arguments = plist["ProgramArguments"]
        self.assertEqual(arguments[:2], ["/usr/bin/env", "python3"])
        self.assertIn("check", arguments)
        self.assertNotIn("apply", arguments)
        self.assertNotIn("approve", arguments)
        self.assertIn("StartCalendarInterval", plist)


class ApprovalAndApplyTests(unittest.TestCase):
    def test_destructive_npm_hygiene_requires_per_tool_approval(self) -> None:
        runner = FakeRunner()
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("claude-e", tier="hygiene", method="npm-g")], Path(temp)
            )
            with self.assertRaises(update.ApprovalError):
                update.apply_draft(
                    draft,
                    requested_tools=[],
                    apply_hygiene=True,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertNotIn(("npm", "install", "-g", "claude-e@1.1.0"), runner.calls)

    def test_approval_is_bound_to_exact_draft_and_allows_smoked_apply(self) -> None:
        runner = FakeRunner()
        runner.sequence_overrides[("codex", "--version")] = [
            update.CommandResult(0, "codex-cli 1.0.0\n", ""),
            update.CommandResult(0, "old-candidate 0.9.0\ncodex-cli 1.1.0\n", ""),
            update.CommandResult(0, "old-candidate 0.9.0\ncodex-cli 1.1.0\n", ""),
        ]
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("codex", tier="approval", method="npm-g")], Path(temp)
            )
            update.approve_tool(draft, "codex", update.APPROVAL_CONFIRMATION)
            applied, alert = update.apply_draft(
                draft,
                requested_tools=["codex"],
                apply_hygiene=False,
                safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                runner=runner,
            )
        self.assertEqual(applied, ["codex"])
        self.assertIsNone(alert)
        self.assertIn(("npm", "install", "-g", "@openai/codex@1.1.0"), runner.calls)

    def test_busy_agentdesk_window_fails_closed_before_mutation(self) -> None:
        runner = FakeRunner()
        runner.overrides[("agentdesk", "status", "--json")] = update.CommandResult(
            0,
            '{"sessions":{"working":1,"with_active_dispatch":0},"queue":{"status":"idle"}}',
            "",
        )
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("gh", tier="hygiene", method="homebrew")], Path(temp)
            )
            with self.assertRaises(update.ToolchainError):
                update.apply_draft(
                    draft,
                    requested_tools=["gh"],
                    apply_hygiene=False,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertNotIn(("brew", "upgrade", "gh"), runner.calls)

    def test_running_deploy_fails_closed_before_mutation(self) -> None:
        runner = FakeRunner()
        runner.overrides[("ps", "-axo", "command=")] = update.CommandResult(
            0, "/bin/bash /release/scripts/deploy-release.sh\n", ""
        )
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("gh", tier="hygiene", method="homebrew")], Path(temp)
            )
            with self.assertRaises(update.ToolchainError):
                update.apply_draft(
                    draft,
                    requested_tools=["gh"],
                    apply_hygiene=False,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertNotIn(("brew", "upgrade", "gh"), runner.calls)

    def test_smoke_failure_stops_batch_and_emits_pin_plan(self) -> None:
        runner = FakeRunner()
        runner.overrides[("gh", "--version")] = update.CommandResult(1, "", "broken loader")
        runner.sequence_overrides[("brew", "list", "--versions", "gh")] = [
            update.CommandResult(0, "gh 1.0.0\n", ""),
            update.CommandResult(0, "gh 1.1.0\n", ""),
        ]
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("gh", tier="hygiene", method="homebrew")], Path(temp)
            )
            applied, alert = update.apply_draft(
                draft,
                requested_tools=["gh"],
                apply_hygiene=False,
                safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                runner=runner,
            )
            self.assertEqual(applied, [])
            self.assertIsNotNone(alert)
            alert_text = alert.read_text(encoding="utf-8")
        self.assertIn("brew pin gh", alert_text)
        self.assertIn("apply batch stopped", alert_text)

    def test_stale_draft_blocks_before_update_command(self) -> None:
        runner = FakeRunner()
        runner.overrides[("brew", "list", "--versions", "gh")] = update.CommandResult(
            0, "gh 1.0.1\n", ""
        )
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("gh", tier="hygiene", method="homebrew")], Path(temp)
            )
            with self.assertRaises(update.ToolchainError):
                update.apply_draft(
                    draft,
                    requested_tools=["gh"],
                    apply_hygiene=False,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertNotIn(("brew", "upgrade", "gh"), runner.calls)


class SmokeGateTests(unittest.TestCase):
    def test_highest_semver_selects_newest_codex_candidate(self) -> None:
        self.assertEqual(
            update.highest_semver("PATH-A codex 0.139.0\nPATH-B codex 0.142.3"),
            "0.142.3",
        )

    def test_homebrew_loose_versions_cover_tmux_and_postgresql(self) -> None:
        self.assertEqual(update._loose_version_key("tmux 3.6a"), ((3, 6, 0), "a"))
        self.assertEqual(update._loose_version_key("psql 17.9"), ((17, 9, 0), ""))

    def test_cswap_shape_accepts_fractional_age_and_rejects_drift(self) -> None:
        valid, detail = update.validate_cswap_shape(
            '{"schemaVersion":1,"activeAccountNumber":2,"accounts":'
            '[{"number":2,"active":true,"usageAgeSeconds":5.4}]}'
        )
        invalid, invalid_detail = update.validate_cswap_shape(
            '{"schemaVersion":1,"accounts":{"number":2}}'
        )
        self.assertTrue(valid, detail)
        self.assertFalse(invalid)
        self.assertIn("accounts must be a list", invalid_detail)
        bool_schema, bool_detail = update.validate_cswap_shape(
            '{"schemaVersion":true,"accounts":[]}'
        )
        self.assertFalse(bool_schema)
        self.assertIn("schemaVersion must be an integer", bool_detail)

    def test_postgresql_strict_gate_requires_server_comparison(self) -> None:
        runner = FakeRunner()
        with patch.dict(
            update.os.environ,
            {"DATABASE_URL": "", "AGENTDESK_DATABASE_URL": ""},
            clear=False,
        ):
            results = update.run_smoke_profile("postgresql-17", runner, strict=True)
        self.assertFalse(results[-1].ok)
        self.assertIn("required", results[-1].detail)

    def test_postgresql_strict_gate_compares_two_part_server_major(self) -> None:
        runner = FakeRunner()
        runner.overrides[("psql", "--version")] = update.CommandResult(
            0, "psql (PostgreSQL) 17.9\n", ""
        )
        runner.overrides[("psql", "-Atqc", "SHOW server_version")] = update.CommandResult(
            0, "17.8\n", ""
        )
        with patch.dict(update.os.environ, {"DATABASE_URL": "postgresql://example"}, clear=False):
            results = update.run_smoke_profile("postgresql-17", runner, strict=True)
        self.assertTrue(all(result.ok for result in results), results)


if __name__ == "__main__":
    unittest.main()
