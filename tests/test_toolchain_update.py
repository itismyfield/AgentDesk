#!/usr/bin/env python3
"""Focused safety and smoke tests for issue #4555's toolchain routine."""

from __future__ import annotations

import json
import plistlib
import sys
import tempfile
import unittest
from dataclasses import replace
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
        self.invocations: list[tuple[tuple[str, ...], int, dict[str, str]]] = []
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
        command = tuple(argv)
        self.calls.append(command)
        self.invocations.append((command, timeout, dict(env or {})))
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
                json.dumps({"formulae": [{"versions": {"stable": "1.1.0"}, "revision": 0}]}),
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
        self.assertEqual(
            plist["StartCalendarInterval"],
            {"Weekday": 0, "Hour": 9, "Minute": 30},
        )
        path = plist["EnvironmentVariables"]["PATH"].split(":")
        self.assertEqual(path[0], "__HOME__/.local/bin")
        self.assertIn("__HOME__/.cargo/bin", path)
        self.assertIn("__HOME__/.opencode/bin", path)
        self.assertIn("__HOME__/bin", path)


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

    def test_installer_hygiene_requires_per_tool_approval(self) -> None:
        runner = FakeRunner()
        with tempfile.TemporaryDirectory() as temp:
            markdown, draft, _ = update.write_draft(
                [check_for("opencode", tier="hygiene", method="installer")], Path(temp)
            )
            self.assertIn(
                "per-tool approval also required: native/self-updater/rustup/npm mutation",
                markdown.read_text(encoding="utf-8"),
            )
            with self.assertRaises(update.ApprovalError):
                update.apply_draft(
                    draft,
                    requested_tools=[],
                    apply_hygiene=True,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertNotIn(("opencode", "upgrade"), runner.calls)

    def test_rustup_hygiene_requires_approval_and_disables_self_update(self) -> None:
        runner = FakeRunner()
        rustc_version = ("rustup", "run", "stable", "rustc", "--version")
        runner.sequence_overrides[rustc_version] = [
            update.CommandResult(0, "rustc 1.0.0\n", ""),
            update.CommandResult(0, "rustc 1.1.0\n", ""),
            update.CommandResult(0, "rustc 1.1.0\n", ""),
        ]
        update_command = ("rustup", "update", "stable", "--no-self-update")
        with tempfile.TemporaryDirectory() as temp:
            markdown, draft, _ = update.write_draft(
                [check_for("cargo-rustc", tier="hygiene", method="rustup")], Path(temp)
            )
            self.assertIn("native/self-updater/rustup/npm", markdown.read_text(encoding="utf-8"))
            with self.assertRaises(update.ApprovalError):
                update.apply_draft(
                    draft,
                    requested_tools=[],
                    apply_hygiene=True,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
            self.assertNotIn(update_command, runner.calls)

            update.approve_tool(draft, "cargo-rustc", update.APPROVAL_CONFIRMATION)
            applied, alert = update.apply_draft(
                draft,
                requested_tools=[],
                apply_hygiene=True,
                safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                runner=runner,
            )

        self.assertEqual(applied, ["cargo-rustc"])
        self.assertIsNone(alert)
        self.assertIn(update_command, runner.calls)
        self.assertFalse(any(call == ("rustup", "update", "stable") for call in runner.calls))

    def test_revisioned_homebrew_upgrade_accepts_approved_bottle(self) -> None:
        runner = FakeRunner()
        runner.overrides[("brew", "info", "--json=v2", "gh")] = update.CommandResult(
            0,
            json.dumps({"formulae": [{"versions": {"stable": "1.1.0"}, "revision": 1}]}),
            "",
        )
        runner.sequence_overrides[("brew", "list", "--versions", "gh")] = [
            update.CommandResult(0, "gh 1.0.0\n", ""),
            update.CommandResult(0, "gh 1.1.0_1\n", ""),
        ]
        check = replace(
            check_for("gh", tier="hygiene", method="homebrew"),
            latest="1.1.0_1",
        )
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft([check], Path(temp))
            applied, alert = update.apply_draft(
                draft,
                requested_tools=["gh"],
                apply_hygiene=False,
                safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                runner=runner,
            )

        self.assertEqual(applied, ["gh"])
        self.assertIsNone(alert)
        self.assertIn(("gh", "--version"), runner.calls)

    def test_revisioned_homebrew_upgrade_rejects_different_bottle_before_smoke(self) -> None:
        runner = FakeRunner()
        runner.overrides[("brew", "info", "--json=v2", "gh")] = update.CommandResult(
            0,
            json.dumps({"formulae": [{"versions": {"stable": "1.1.0"}, "revision": 1}]}),
            "",
        )
        runner.sequence_overrides[("brew", "list", "--versions", "gh")] = [
            update.CommandResult(0, "gh 1.0.0\n", ""),
            update.CommandResult(0, "gh 1.1.0_2\n", ""),
        ]
        check = replace(
            check_for("gh", tier="hygiene", method="homebrew"),
            latest="1.1.0_1",
        )
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft([check], Path(temp))
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

        self.assertIn("expected=1.1.0_1, observed=1.1.0_2", alert_text)
        self.assertIn("smoke not run", alert_text)
        self.assertNotIn(("gh", "--version"), runner.calls)

    def test_generated_contract_says_exact_version_can_prevent_smoke(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            markdown, _, _ = update.write_draft(
                [check_for("gh", tier="hygiene", method="homebrew")], Path(temp)
            )
            report = markdown.read_text(encoding="utf-8")

        self.assertIn("exact-version verification", report)
        self.assertIn("may stop the batch before smoke", report)
        self.assertNotIn("Every applied tool runs its smoke profile", report)

    def test_non_homebrew_default_update_timeout_is_1800_seconds(self) -> None:
        spec = next(item for item in tool_inventory() if item.key == "cargo-rustc")
        self.assertEqual(update.DEFAULT_UPDATE_TIMEOUT_SECONDS, 1800)
        self.assertEqual(update._update_timeout(spec), 1800)

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

    def test_newer_than_approved_native_result_fails_before_smoke_with_alert(self) -> None:
        runner = FakeRunner()
        runner.sequence_overrides[("claude", "--version")] = [
            update.CommandResult(0, "claude 1.0.0\n", ""),
            update.CommandResult(0, "claude 1.2.0\n", ""),
        ]
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("claude", tier="approval", method="native")], Path(temp)
            )
            update.approve_tool(draft, "claude", update.APPROVAL_CONFIRMATION)
            applied, alert = update.apply_draft(
                draft,
                requested_tools=["claude"],
                apply_hygiene=False,
                safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                runner=runner,
            )
            self.assertEqual(applied, [])
            self.assertIsNotNone(alert)
            alert_text = alert.read_text(encoding="utf-8")
        self.assertIn("expected=1.1.0, observed=1.2.0", alert_text)
        self.assertIn("smoke not run", alert_text)
        self.assertEqual(runner.calls.count(("claude", "--version")), 2)

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

    def test_active_queue_fails_closed_before_mutation(self) -> None:
        runner = FakeRunner()
        runner.overrides[("agentdesk", "status", "--json")] = update.CommandResult(
            0,
            '{"sessions":{"working":0,"with_active_dispatch":0},"queue":{"status":"active"}}',
            "",
        )
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("gh", tier="hygiene", method="homebrew")], Path(temp)
            )
            with self.assertRaisesRegex(update.ToolchainError, "queue_status=active"):
                update.apply_draft(
                    draft,
                    requested_tools=["gh"],
                    apply_hygiene=False,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertNotIn(("brew", "upgrade", "gh"), runner.calls)

    def test_completed_queue_history_is_not_treated_as_live(self) -> None:
        runner = FakeRunner()
        runner.overrides[("agentdesk", "status", "--json")] = update.CommandResult(
            0,
            '{"sessions":{"working":0,"with_active_dispatch":0},"queue":{"status":"completed"}}',
            "",
        )
        update.assert_safe_window(runner)

    def test_safe_window_is_rechecked_before_each_mutation(self) -> None:
        runner = FakeRunner()
        idle = update.CommandResult(
            0,
            '{"sessions":{"working":0,"with_active_dispatch":0},"queue":{"status":"idle"}}',
            "",
        )
        active = update.CommandResult(
            0,
            '{"sessions":{"working":0,"with_active_dispatch":0},"queue":{"status":"active"}}',
            "",
        )
        runner.sequence_overrides[("agentdesk", "status", "--json")] = [idle, idle, active]
        runner.sequence_overrides[("brew", "list", "--versions", "gh")] = [
            update.CommandResult(0, "gh 1.0.0\n", ""),
            update.CommandResult(0, "gh 1.1.0\n", ""),
        ]
        runner.sequence_overrides[("brew", "list", "--versions", "jq")] = [
            update.CommandResult(0, "jq 1.0.0\n", ""),
        ]
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [
                    check_for("gh", tier="hygiene", method="homebrew"),
                    check_for("jq", tier="hygiene", method="homebrew"),
                ],
                Path(temp),
            )
            with self.assertRaisesRegex(update.ToolchainError, "queue_status=active"):
                update.apply_draft(
                    draft,
                    requested_tools=["gh", "jq"],
                    apply_hygiene=False,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertIn(("brew", "upgrade", "gh"), runner.calls)
        self.assertNotIn(("brew", "upgrade", "jq"), runner.calls)
        self.assertEqual(runner.calls.count(("agentdesk", "status", "--json")), 3)

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

    def test_update_timeout_uses_build_window_and_emits_rollback_alert(self) -> None:
        runner = FakeRunner()
        runner.overrides[("brew", "upgrade", "gh")] = update.CommandResult(
            124, "", "timed out during source build"
        )
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
        timeout = next(
            timeout
            for command, timeout, _env in runner.invocations
            if command == ("brew", "upgrade", "gh")
        )
        self.assertEqual(timeout, update.BUILD_UPDATE_TIMEOUT_SECONDS)
        self.assertGreaterEqual(timeout, 3600)
        self.assertIn("timed out during source build", alert_text)
        self.assertIn("brew pin gh", alert_text)

    def test_post_mutation_smoke_setup_error_emits_rollback_alert(self) -> None:
        runner = FakeRunner()
        runner.sequence_overrides[("brew", "list", "--versions", "gh")] = [
            update.CommandResult(0, "gh 1.0.0\n", ""),
            update.CommandResult(0, "gh 1.1.0\n", ""),
        ]
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft(
                [check_for("gh", tier="hygiene", method="homebrew")], Path(temp)
            )
            with patch.object(
                update,
                "run_smoke_profile",
                side_effect=update.ToolchainError("smoke profile misconfigured"),
            ):
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
        self.assertIn("smoke profile misconfigured", alert_text)
        self.assertIn("brew pin gh", alert_text)

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

    def test_stale_manifest_row_has_operator_friendly_error(self) -> None:
        runner = FakeRunner()
        stale = replace(check_for("gh", tier="hygiene", method="homebrew"), key="retired-tool")
        with tempfile.TemporaryDirectory() as temp:
            _, draft, _ = update.write_draft([stale], Path(temp))
            with self.assertRaisesRegex(
                update.ToolchainError,
                "draft contains tool.*retired-tool.*generate a fresh draft",
            ):
                update.apply_draft(
                    draft,
                    requested_tools=[],
                    apply_hygiene=True,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertNotIn(("brew", "upgrade", "gh"), runner.calls)

    def test_later_identical_draft_rejects_old_approval_and_reprobe_catches_move(self) -> None:
        runner = FakeRunner()
        check = check_for("codex", tier="approval", method="npm-g")
        same_time = datetime(2026, 7, 15, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as temp:
            output_dir = Path(temp)
            _, draft, draft_a_id = update.write_draft([check], output_dir, now=same_time)
            update.approve_tool(draft, "codex", update.APPROVAL_CONFIRMATION)
            _, draft, draft_b_id = update.write_draft([check], output_dir, now=same_time)
            self.assertNotEqual(draft_a_id, draft_b_id)
            runner.overrides[("codex", "--version")] = update.CommandResult(
                0, "codex-cli 1.0.1\n", ""
            )
            with self.assertRaises(update.ApprovalError):
                update.apply_draft(
                    draft,
                    requested_tools=["codex"],
                    apply_hygiene=False,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
            update.approve_tool(draft, "codex", update.APPROVAL_CONFIRMATION)
            with self.assertRaisesRegex(update.ToolchainError, "stale draft for codex"):
                update.apply_draft(
                    draft,
                    requested_tools=["codex"],
                    apply_hygiene=False,
                    safe_window_confirmation=update.SAFE_WINDOW_CONFIRMATION,
                    runner=runner,
                )
        self.assertNotIn(("npm", "install", "-g", "@openai/codex@1.1.0"), runner.calls)


class SmokeGateTests(unittest.TestCase):
    def test_highest_semver_selects_newest_codex_candidate(self) -> None:
        self.assertEqual(
            update.highest_semver("PATH-A codex 0.139.0\nPATH-B codex 0.142.3"),
            "0.142.3",
        )

    def test_homebrew_loose_versions_cover_tmux_and_postgresql(self) -> None:
        self.assertEqual(update._loose_version_key("tmux 3.6a"), ((3, 6, 0), "a", 0))
        self.assertEqual(update._loose_version_key("psql 17.9"), ((17, 9, 0), "", 0))

    def test_homebrew_revision_nine_to_ten_is_update_available(self) -> None:
        spec = next(item for item in tool_inventory() if item.key == "gh")
        decision = update.decide_check(
            spec,
            update.ValueProbe(True, "1.1.0_9", "installed"),
            update.ValueProbe(True, "1.1.0_10", "registry"),
        )

        self.assertEqual(decision, "update-available")

    def test_homebrew_revision_ten_to_nine_is_installed_newer(self) -> None:
        spec = next(item for item in tool_inventory() if item.key == "gh")
        decision = update.decide_check(
            spec,
            update.ValueProbe(True, "1.1.0_10", "installed"),
            update.ValueProbe(True, "1.1.0_9", "registry"),
        )

        self.assertEqual(decision, "installed-newer-than-registry")

    def test_homebrew_revision_zero_orders_before_one_without_loosening_equality(self) -> None:
        spec = next(item for item in tool_inventory() if item.key == "gh")
        decision = update.decide_check(
            spec,
            update.ValueProbe(True, "1.1.0", "installed"),
            update.ValueProbe(True, "1.1.0_1", "registry"),
        )

        self.assertEqual(decision, "update-available")
        self.assertFalse(update._same_version("1.1.0", "1.1.0_0"))
        self.assertFalse(update._same_version("1.1.0_01", "1.1.0_1"))

    def test_malformed_homebrew_revision_suffix_falls_back_without_crashing(self) -> None:
        self.assertEqual(
            update._loose_version_key("1.1.0_not-a-revision"),
            ((1, 1, 0), "_not-a-revision", 0),
        )

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
        for invalid_payload in ("{}", '{"accounts":null}'):
            valid_accounts, accounts_detail = update.validate_cswap_shape(invalid_payload)
            self.assertFalse(valid_accounts)
            self.assertIn("present, non-null list", accounts_detail)

    def test_node_smoke_fails_when_expected_global_inventory_is_empty(self) -> None:
        runner = FakeRunner()
        results = update.run_smoke_profile("node", runner, strict=True)
        inventory = next(result for result in results if result.check == "node global CLI inventory")
        self.assertFalse(inventory.ok)
        self.assertIn("expected=3", inventory.detail)
        self.assertIn("@openai/codex", inventory.detail)

    def test_node_smoke_probes_every_manifest_npm_global_cli(self) -> None:
        runner = FakeRunner()
        dependencies = {
            spec.update_value: {}
            for spec in tool_inventory()
            if spec.method == "npm-g" and isinstance(spec.update_value, str)
        }
        runner.overrides[("npm", "ls", "-g", "--depth=0", "--json")] = update.CommandResult(
            0, json.dumps({"dependencies": dependencies}), ""
        )
        results = update.run_smoke_profile("node", runner, strict=True)
        self.assertTrue(all(result.ok for result in results), results)
        for command in (("codex", "--version"), ("ocx", "--version"), ("claude-e", "--version")):
            self.assertIn(command, runner.calls)

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
        database_url = "postgresql://example/agentdesk"
        runner.overrides[("psql", "--version")] = update.CommandResult(
            0, "psql (PostgreSQL) 17.9\n", ""
        )
        server_command = ("psql", "-Atqc", "SHOW server_version", database_url)
        runner.overrides[server_command] = update.CommandResult(
            0, "17.8\n", ""
        )
        with patch.dict(
            update.os.environ,
            {"DATABASE_URL": database_url, "AGENTDESK_DATABASE_URL": ""},
            clear=False,
        ):
            results = update.run_smoke_profile("postgresql-17", runner, strict=True)
        self.assertTrue(all(result.ok for result in results), results)
        self.assertIn(server_command, runner.calls)
        server_invocation = next(item for item in runner.invocations if item[0] == server_command)
        self.assertNotIn("PGDATABASE", server_invocation[2])


if __name__ == "__main__":
    unittest.main()
