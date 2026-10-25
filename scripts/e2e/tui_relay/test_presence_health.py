"""Offline Unknown health attribution and readiness adapter contracts."""

import copy
import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_tui_relay as driver
from tui_relay import herdr


TARGET = "41"
PREFIX = "turn_presence_unknown:*"


def unknown(channel=42, *, provider="claude", agent="other", leaf="none", elapsed=61):
    return dict(channel_id=channel, provider=provider, agent_id=agent,
                reason="session_identity_unresolved", hold_leaf=None if leaf == "none" else leaf,
                unknown_since="2026-10-20T00:00:00Z", elapsed_secs=elapsed)


def encode(agent):
    if agent is None:
        return "~"
    return "".join(chr(b) if chr(b).isascii() and (chr(b).isalnum() or chr(b) in "_.-")
                   else f"%{b:02X}" for b in agent.encode())


def payload(*rows, extra=()):
    groups = {}
    for row in rows:
        key = (row["provider"], encode(row["agent_id"]), row["reason"], row["hold_leaf"] or "none")
        group = groups.setdefault(key, [])
        group.append(row)
    reasons = [f"turn_presence_unknown:{':'.join(key)}:{len(group)}:{max(r['elapsed_secs'] for r in group)}"
               for key, group in groups.items()] + list(extra)
    return dict(status="degraded" if reasons else "healthy", fully_recovered=True,
                ok=not reasons, degraded=bool(reasons), degraded_reasons=reasons,
                turn_presence_unknown=list(rows), mailboxes=[])


class PresenceHealth(unittest.TestCase):
    def setUp(self):
        self.enterContext(patch("urllib.request.urlopen", side_effect=AssertionError("offline only")))

    def detail(self, body, *, channel=TARGET, allowed=(PREFIX,)):
        return herdr.detail_ready(driver, 200, body, allowed=allowed, channel=channel)

    def test_e1_no_waiver_remains_strict(self):
        with self.assertRaises(driver.assertions.AssertionError):
            self.detail(payload(unknown()), allowed=())

    def test_e2_foreign_unknown_with_explicit_waiver(self):
        for agent in ("other", "agent41", "~", "역할:%\n", None):
            body = payload(unknown(agent=agent, leaf="herdr_owner_mismatch"),
                           unknown(43, agent=agent, leaf="herdr_owner_mismatch", elapsed=90))
            with self.subTest(agent=agent):
                self.assertEqual(self.detail(body), body["degraded_reasons"])

    def test_e3_target_unknown_cannot_be_waived(self):
        for provider in ("claude", "codex"):
            with self.subTest(provider=provider), self.assertRaises(driver.assertions.AssertionError):
                self.detail(payload(unknown(41, provider=provider), unknown()))

    def test_e4_group_count_mismatch_cannot_be_waived(self):
        body = payload(unknown())
        body["degraded_reasons"][0] = body["degraded_reasons"][0].replace(":1:61", ":2:61")
        with self.assertRaises(driver.assertions.AssertionError):
            self.detail(body)

    def test_e4_malformed_and_incomplete_evidence(self):
        valid = payload(unknown())
        bad = []
        for array in (None, [], {}, [None]):
            bad.append({**valid, "turn_presence_unknown": array})
        bad.append({k: v for k, v in valid.items() if k != "turn_presence_unknown"})
        for field in unknown():
            row = unknown()
            del row[field]
            bad.append({**valid, "turn_presence_unknown": [row]})
        for field, values in {
            "channel_id": (True, 42.0, 0, -1, "42", 2**64),
            "provider": (None, "other", True), "agent_id": (False, [], ""),
            "reason": (None, "bad:reason", "Upper"), "hold_leaf": (False, "bad:leaf", ""),
            "elapsed_secs": (False, 61.0, "61", 59, -1),
            "unknown_since": (None, "2026-10-20T00:00:00", "2026-02-30T00:00:00Z",
                              "2026-10-20T00:00:00+24:00", "not-a-date"),
        }.items():
            for value in values:
                bad.append({**valid, "turn_presence_unknown": [{**unknown(), field: value}]})
        for value in ("different", "%61", "%7e", "%FF", "%GG", "~a", ""):
            bad.append({**valid, "degraded_reasons": [valid["degraded_reasons"][0].replace(":other:", f":{value}:")]})
        for suffix in (":1:60", ":1:061", ":0:61", ":1:-1", ":1:61:extra"):
            bad.append({**valid, "degraded_reasons": [valid["degraded_reasons"][0].replace(":1:61", suffix)]})
        for name in ("turn_presence_unknown", "turn_presence_unknown_other", "turn_presence_unknown:"):
            bad.append({**valid, "degraded_reasons": [name]})
        bad.extend((
            {**valid, "degraded_reasons": valid["degraded_reasons"] * 2},
            {**valid, "degraded_reasons": []},
            {**valid, "degraded_reasons": [*valid["degraded_reasons"], None]},
            {**valid, "turn_presence_unknown": [unknown(), unknown()]},
            {**valid, "turn_presence_unknown": [unknown(), unknown(provider="codex")]},
            {**valid, "turn_presence_unknown": [unknown(), unknown(43)]},
            {**valid, "turn_presence_unknown": [unknown(provider="codex")]},
            {**valid, "turn_presence_unknown": [unknown(agent="different")]},
            {**valid, "turn_presence_unknown": [{**unknown(), "reason": "facts_halted"}]},
            {**valid, "turn_presence_unknown": [unknown(leaf="different")]},
        ))
        for index, body in enumerate(bad):
            with self.subTest(case=index, body=body), self.assertRaises(driver.assertions.AssertionError):
                self.detail(body)

    def test_e4_missing_or_invalid_target_context(self):
        for channel in (None, False, 0, "0", "041", "invalid", 2**64):
            with self.subTest(channel=channel), self.assertRaises(driver.assertions.AssertionError):
                self.detail(payload(unknown()), channel=channel)

    def test_e4_group_maximum_comes_from_same_rows(self):
        body = payload(unknown(elapsed=61), unknown(43, elapsed=90))
        self.assertEqual(self.detail(body), body["degraded_reasons"])
        body["turn_presence_unknown"][1]["elapsed_secs"] = 89
        with self.assertRaises(driver.assertions.AssertionError):
            self.detail(body)

    def test_e5_other_reasons_still_block(self):
        for reason in ("disk_low_free_bytes:1", "database_unavailable", "new_reason"):
            with self.subTest(reason=reason), self.assertRaises(driver.assertions.AssertionError):
                self.detail(payload(unknown(), extra=(reason,)))

    def test_e6_implicit_allowlist_cannot_waive_unknown(self):
        with patch.object(driver, "_read_api_json", return_value=(200, payload(unknown()))):
            with self.assertRaises(driver.assertions.AssertionError):
                driver._assert_health_once("http://offline.invalid", dict(
                    require_status=["degraded"], forbid_degraded_reasons=["unrelated"]), channel_id=TARGET)

    def run_adapter(self, adapter, public, detail, *, allowed=(PREFIX,), target=TARGET):
        reads = []
        def read(base, endpoint, *, timeout):
            reads.append(endpoint)
            return copy.deepcopy(public if endpoint == "/api/health" else detail)
        with patch.object(driver, "_read_api_json", side_effect=read):
            if adapter == "wait":
                driver.wait_for_health("http://offline.invalid", timeout_s=0.001, poll_interval_s=0,
                    allowed_statuses=("healthy", "degraded"), allowed_degraded_reasons=allowed,
                    target_channel=target)
            else:
                driver._assert_health_once("http://offline.invalid", dict(
                    require_status=["healthy", "degraded"], allowed_degraded_reasons=list(allowed)),
                    channel_id=target)
        return reads

    def test_e6_adapters_pass_context_and_read_detail_once(self):
        for adapter in ("wait", "assert"):
            with self.subTest(adapter=adapter):
                body = payload(unknown())
                public = {k: v for k, v in body.items() if k != "turn_presence_unknown"}
                try:
                    reads = self.run_adapter(adapter, (200, public), (200, body))
                except driver.assertions.AssertionError as error:
                    self.fail(f"complete foreign detail must be ready: {error}")
                self.assertEqual(reads, ["/api/health", "/api/health/detail"])
                for target in (TARGET, None):
                    with self.subTest(target=target), self.assertRaises(driver.assertions.AssertionError):
                        self.run_adapter(adapter, (200, public), (200, payload(unknown(41))), target=target)

    def test_e6_direct_helper_cannot_bypass_context(self):
        self.assertTrue(driver._health_ready_violations(payload(unknown()),
                        allowed_statuses=("degraded",), allowed_degraded_reasons=(PREFIX,)))

    def test_e6_counter_and_idle_checks_reuse_selected_detail(self):
        public = {**payload(unknown()), "global_active": 999}
        detail = {**payload(unknown()), "global_active": 0}
        for maximum in (0, 1):
            with self.subTest(maximum=maximum), \
                    patch.object(driver, "_read_api_json", side_effect=[(200, public), (200, detail)]) as read, \
                    patch.object(driver, "_assert_health_target_idle", return_value={"status": "idle"}) as idle:
                result = driver._assert_health_once("http://offline.invalid", dict(
                    require_status=["degraded"], allowed_degraded_reasons=[PREFIX], global_active_max=maximum),
                    channel_id=TARGET, cell="claude-tui")
                self.assertEqual(result["global_active"], 0)
                self.assertEqual([c.args[1] for c in read.call_args_list], ["/api/health", "/api/health/detail"])
                if maximum == 0:
                    idle.assert_called_once_with(detail, channel_id=TARGET, cell="claude-tui")
                else:
                    idle.assert_not_called()

    def test_e6_restart_adapter_passes_its_target(self):
        with patch.object(driver, "_guard_no_foreign_active_turns"), \
                patch.object(driver.subprocess, "run", return_value=SimpleNamespace(returncode=0)), \
                patch.object(driver, "_boot_incarnation", side_effect=["before", "after"]), \
                patch.object(driver, "wait_for_health") as wait:
            driver.restart_dcserver_for_e2e(target="dev", args=SimpleNamespace(restart_script=None),
                base_url="http://offline.invalid", cell="claude-tui", channel_id=TARGET,
                require_status=("degraded",), allowed_degraded_reasons=(PREFIX,))
        self.assertEqual(wait.call_args.kwargs["target_channel"], TARGET)

    def observe(self, body, *, waived=True):
        args = SimpleNamespace(herdr_allowed_degraded_reason=[PREFIX] if waived else [],
            base_url="http://127.0.0.1:8797", channel_id=TARGET, cell="claude-herdr", herdr_endpoint="test")
        body = {**body, "herdr": dict(admission="open", restart_required=False,
            configured_channels=[TARGET], endpoints={"test": dict(local=True)}, input_holds=0,
            reconnect=dict(channels=0, published=0, withheld=0, unknown=0, pending=0))}
        row = dict(channel=TARGET, provider="claude", state="bound", nonce="same", pane="provider_running",
                   launch_evidence="recorded", input_hold=None)
        with patch.object(driver, "_read_api_json", return_value=(200, body)), \
                patch.object(herdr.subprocess, "run", return_value=SimpleNamespace(stdout=json.dumps(
                    dict(executions=[row], other_input_holds=[])))) as cli:
            try:
                result = herdr.observe(driver, args)
            except driver.assertions.AssertionError:
                cli.assert_not_called()
                raise
        return result

    def test_e6_herdr_blocks_before_cli_on_target_and_unknown_attribution(self):
        for body, waived in ((payload(unknown()), False), (payload(unknown(41)), True),
                             ({**payload(unknown()), "turn_presence_unknown": []}, True)):
            with self.subTest(body=body, waived=waived), self.assertRaises(driver.assertions.AssertionError) as caught:
                self.observe(body, waived=waived)
            self.assertIn("error", caught.exception.herdr_observation["readiness"])
        self.assertEqual(self.observe(payload(unknown()))["readiness"]["presence_unknown_waiver"], "verified_foreign")

    def test_e7_other_explicit_waivers_stay_on_public(self):
        for adapter in ("wait", "assert"):
            body = payload(extra=("disk_low_free_bytes:1",))
            with self.subTest(adapter=adapter):
                self.assertEqual(self.run_adapter(adapter, (200, body), (403, {}),
                    allowed=("disk_low_free_bytes:*",)), ["/api/health"])
                self.assertEqual(self.run_adapter(adapter, (200, payload()), (403, {})), ["/api/health"])
                with patch.object(driver, "_read_api_json", return_value=(200, payload(unknown()))) as read:
                    with self.assertRaises(driver.assertions.AssertionError):
                        driver._assert_health_once("http://offline.invalid", dict(
                            require_status=["degraded"], allowed_degraded_reasons=["disk_low_free_bytes:*"]),
                            channel_id=TARGET)
                    self.assertEqual([c.args[1] for c in read.call_args_list], ["/api/health"])
                with self.assertRaises(driver.assertions.AssertionError):
                    self.run_adapter(adapter, (200, payload(unknown())), (403, {}))

    def test_e7_public_and_detail_facts_never_mix(self):
        public = payload(unknown())
        bad = [payload(unknown(41)), payload(unknown(), extra=("database_unavailable",)),
               {**payload(unknown()), "fully_recovered": False},
               {**payload(unknown()), "status": "healthy"},
               {**payload(unknown()), "ok": True}, {**payload(unknown()), "degraded": False},
               {**payload(unknown()), "degraded_reasons": []},
               {**payload(unknown()), "turn_presence_unknown": []}]
        for adapter in ("wait", "assert"):
            for body in bad:
                with self.subTest(adapter=adapter, body=body), self.assertRaises(driver.assertions.AssertionError):
                    self.run_adapter(adapter, (200, public), (200, body))
            # A whole fresh healthy detail may show recovery; stale public facts cannot poison it.
            self.assertEqual(self.run_adapter(adapter, (200, public), (200, payload())),
                             ["/api/health", "/api/health/detail"])


if __name__ == "__main__":
    unittest.main()
