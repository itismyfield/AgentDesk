"""Hand-defined fifty-turn evidence; hypothetical support is confined to test copies."""
import copy
import json
import shutil
import subprocess
import sys
import unittest

from tests import test_n1_canary_oracle as input_fixture

PACKAGE, ENTRY = input_fixture.PACKAGE, input_fixture.ENTRY
canonical, digest = input_fixture.canonical, input_fixture.digest


class AuditCLI(unittest.TestCase):
    save = input_fixture.Fixture.save

    def setUp(self):
        input_fixture.Fixture.setUp(self)
        self.shipped = {p.name: p.read_bytes() for p in (PACKAGE / "compatibility.json", PACKAGE / "evidence.py")}
        self.copy = self.base / "isolated-cli"
        shutil.copytree(PACKAGE, self.copy / "n1_canary", ignore=shutil.ignore_patterns("__pycache__"))
        shutil.copy2(ENTRY, self.copy / ENTRY.name)
        rows = [{"type": "session_meta", "payload": {"id": "native-session"}}]
        offsets, units = [], []
        for n in range(50):
            start = len(b"".join(canonical(r) + b"\n" for r in rows))
            rows.extend([
                {"type": "event_msg", "payload": {"type": "task_started", "turn_id": f"t{n}"}},
                {"type": "response_item", "payload": {"type": "message", "role": "assistant", "id": f"b{n}",
                 "content": [{"type": "output_text", "text": f"body {n}"}]}},
                {"type": "event_msg", "payload": {"type": "task_complete", "turn_id": f"t{n}"}}])
            offsets.append((start, len(b"".join(canonical(r) + b"\n" for r in rows))))
            units.append(dict(channel_id=123, provider="codex", native_key=f"b{n}", kind="body"))
        raw = b"".join(canonical(r) + b"\n" for r in rows)
        self.values["transcript"] = raw
        self.manifest["scope"].update(fiftieth_offset=len(raw), closure_offset=len(raw))
        self.manifest["source_digests"] = dict(prefix_sha256=digest(b""), window_sha256=digest(raw))
        self.manifest["observation"].update(seq_end=51, actual_confirmed_at_us=15)
        self.values["log"][0]["actual_confirmed_at_us"] = 15
        self.values["end_snapshot"]["seq_high_water"] = 51
        self.values["end_snapshot"]["counters"]["closed_events"] = 50
        source = self.manifest["scope"]["source"]
        self.inputs = []
        for n, (start, end) in enumerate(offsets):
            physical = dict(provider="codex", channel_id=123, source_id=source, start_offset=start,
                            end_offset=end, native_turn_id=f"t{n}")
            record_digest = digest(canonical([rows[1+n*3:4+n*3], "completed", False]))
            logical = dict(provider="codex", channel_id=123, provider_session_id="native-session", native_turn_id=f"t{n}")
            self.values["log"].append(dict(event="n1_turn_closed", seq=n+2, observed_at_us=30+n,
                boot_id=self.manifest["runtime"]["boot_id"], channel_id=123, provider="codex", schema_version=2,
                physical_observation_key=physical, logical_turn_key=logical, autonomous=False, outcome="completed"))
            self.inputs.append(dict(physical_observation_key=physical, native_record_digest=record_digest,
                input_kind="tty", discord_message_id=None, execution_identity=self.manifest["binding"]["identity"]))
        self.values["o_init"] = dict(channel=123, sources=[dict(source_id=source, delivery_start=0, prefix_hash=digest(b""))],
            initial_anchor=99, build_digest=self.manifest["runtime"]["build_commit"], at="2026-10-12T23:00:00Z")
        self.values["o_ledger"] = []
        anchor = 99
        for n, key in enumerate(units):
            at = "2026-10-13T00:00:01Z"
            self.values["o_ledger"].extend([
                dict(at=at, entry=dict(type="prepared", serial=n+1, unit_key=key, piece_index=0,
                     payload=f"body {n}", anchor_id=anchor, epoch=1)),
                dict(at=at, entry=dict(type="posted", serial=n+1, msg_id=200+n))])
            anchor = 200+n
        self.values["legacy"] = self.capture(inputs=self.inputs, confirmed_deliveries=[], delivered_frontier=None,
            owner=dict(writer="o", turn_mode="n1", identity=self.manifest["binding"]["identity"]), allowed_messages=[])
        for name in ("inflight", "pending", "abort"):
            self.values[name] = self.capture(records=[])
        self.values["debt"] = self.capture(undelivered=[], abandoned=[], baseline_messages=[dict(
            message_id="99", payload_hash=digest(b"baseline anchor"), last_changed_seq=0)], ledger_sha256="")
        self.manifest["discord"]["individual_ids"] = ["99"]
        self.values["discord"]["individual"] = {"99": dict(id="99", channel_id="123", author={"id":"456"}, content="baseline anchor")}
        self.messages = [dict(id=str(200+n), channel_id="123", author={"id": "456"}, content=f"body {n}") for n in range(50)]
        self.refresh_history()
        self.save()

    def capture(self, **fields):
        scope = self.manifest["scope"]
        return dict(schema_version=1, complete=True, scope=dict(boot_id=self.manifest["runtime"]["boot_id"],
            channel_id=scope["channel_id"], provider=scope["provider"], source=scope["source"],
            seq_end=self.manifest["observation"]["seq_end"], closure_offset=scope["closure_offset"]), **fields)

    def refresh_history(self):
        messages = sorted(self.messages, key=lambda m: int(m["id"]), reverse=True)
        self.values["discord"] = dict(complete=True, reasons=[], channels=[123], lower_inclusive="100", upper_exclusive="500",
            pages=2, page_boundaries=[dict(channel_id=123, before="500", ids=[m["id"] for m in messages]),
                                     dict(channel_id=123, before=messages[-1]["id"], ids=[])], messages=messages,
                        individual=copy.deepcopy(self.values["discord"].get("individual", {})))

    def run_cli(self, expected, supported=True, extra=()):
        ledger = b"".join(canonical(r) + b"\n" for r in self.values["o_ledger"])
        self.values["debt"]["ledger_sha256"] = digest(ledger)
        self.save()
        if supported:
            table_path = self.copy / "n1_canary/compatibility.json"
            table = json.loads(self.shipped["compatibility.json"])
            table.update(unknown_outcome_produced=True, pending_stages=[])
            table_path.write_bytes(canonical(table))
            paths = [self.copy / ENTRY.name, *sorted((self.copy / "n1_canary").glob("*.py")), table_path]
            self.manifest["oracle"]["bundle_sha256"] = digest(b"".join(p.name.encode()+b"\0"+p.read_bytes()+b"\0" for p in paths))
            self.path.write_bytes(canonical(self.manifest))
            shim = '''import runpy, sys
sys.path.insert(0, sys.argv[1])
from n1_canary import evidence, loads
from n1_canary.manifest import PACKAGE
original = evidence.observation_checks
def supported_observation(manifest, values):
    summary, confirmation, reasons = original(manifest, values)
    if loads((PACKAGE / "compatibility.json").read_bytes())["unknown_outcome_produced"]:
        reasons = [r for r in reasons if r != "unknown_outcome_unproduced"]
        summary["unknown_outcome_events"] = 0
    return summary, confirmation, reasons
evidence.observation_checks = supported_observation
sys.argv = [sys.argv[1] + "/n1_turn_canary_oracle.py", "--manifest", sys.argv[2], *sys.argv[3:]]
runpy.run_path(sys.argv[0], run_name="__main__")
'''
            command = [sys.executable, "-c", shim, str(self.copy), str(self.path), *extra]
        else:
            paths = [ENTRY, *sorted(PACKAGE.glob("*.py")), PACKAGE / "compatibility.json"]
            self.manifest["oracle"]["bundle_sha256"] = digest(b"".join(p.name.encode()+b"\0"+p.read_bytes()+b"\0" for p in paths))
            self.path.write_bytes(canonical(self.manifest))
            command = [sys.executable, str(ENTRY), "--manifest", str(self.path), *extra]
        process = subprocess.run(command, capture_output=True, text=True, timeout=20)
        self.assertEqual(process.returncode, expected, process.stdout + process.stderr)
        output = json.loads(process.stdout)
        self.assertEqual(output["verdict"], {0: "PASS", 1: "FAIL", 2: "[U]"}[expected])
        return output

    def test_supported_fifty_pass_and_shipped_profile_stays_unknown(self):
        supported = self.run_cli(0)
        self.assertTrue(supported["evidence_complete"])
        self.assertEqual(supported["turns"]["completed_distinct"], 50)
        self.assertEqual(supported["delivery"]["expected_pieces"], 50)
        self.assertEqual(supported["delivery"]["delivered_pieces"], 50)
        shipped = self.run_cli(2, supported=False)
        self.assertEqual(shipped["reasons"], ["unknown_outcome_unproduced"])
        self.assertIsNone(shipped["observation"]["unknown_outcome_events"])
        for name, raw in self.shipped.items():
            self.assertEqual((PACKAGE / name).read_bytes(), raw)

    def test_incomplete_stage_never_composes_pass(self):
        self.values["pending"]["complete"] = False
        self.assertIn("audit_capture_incomplete:pending", self.run_cli(2)["reasons"])

    def test_forward_loss_duplicate_and_order_are_failures(self):
        original = copy.deepcopy(self.values["o_ledger"])
        self.messages.pop(0)
        self.refresh_history()
        self.assertIn("delivery_missing", self.run_cli(1)["failures"])
        self.messages.insert(0, dict(id="200", channel_id="123", author={"id":"456"}, content="body 0"))
        self.refresh_history()
        self.values["o_ledger"].extend([dict(at="2026-10-13T00:00:01Z", entry=dict(
            type="prepared", serial=51, unit_key=original[0]["entry"]["unit_key"], piece_index=0, payload="body 0", anchor_id=249, epoch=1)),
            dict(at="2026-10-13T00:00:01Z", entry=dict(type="posted", serial=51, msg_id=250))])
        self.messages.append(dict(id="250", channel_id="123", author={"id":"456"}, content="body 0"))
        self.refresh_history()
        self.assertIn("delivery_duplicate", self.run_cli(1)["failures"])
        self.values["o_ledger"] = original
        self.values["o_ledger"][0]["entry"]["piece_index"] = 1
        self.assertIn("delivery_piece_order", self.run_cli(1)["failures"])

    def test_reverse_unexplained_and_grounded_extra(self):
        self.messages.append(dict(id="300", channel_id="123", author={"id":"456"}, content="unexplained"))
        self.refresh_history()
        self.assertIn("discord_message_unexplained", self.run_cli(2)["reasons"])
        self.values["o_ledger"].extend([
            dict(at="2026-10-13T00:00:01Z", entry=dict(type="prepared", serial=51, unit_key=dict(
                channel_id=123, provider="codex", native_key="source-never-produced", kind="body"), piece_index=0,
                payload="unexplained", anchor_id=249, epoch=1)),
            dict(at="2026-10-13T00:00:01Z", entry=dict(type="posted", serial=51, msg_id=300))])
        self.assertIn("delivery_source_missing", self.run_cli(1)["failures"])

    def test_historical_debt_reported_separately(self):
        self.values["debt"]["undelivered"] = [dict(source=self.manifest["scope"]["source"], start=0, end=0, reason="before baseline")]
        self.assertIn("historical_debt_unverified", self.run_cli(2)["reasons"])
        self.assertEqual(self.run_cli(2)["historical_debt"]["unresolved"], 1)

    def test_search_call_id_is_typed_before_exclusion(self):
        from tests.test_n1_canary_native_cli import NativeCLI
        from tests.test_n1_canary_native import encode
        case = NativeCLI()
        case.setUp()
        try:
            rows = case.codex_turn()
            rows[5]["payload"] = dict(type="tool_search_call", id="search-1", execution="server", arguments={}, call_id=42)
            case.use_native(encode(rows))
            output = case.cli()
            self.assertIn("native_record_unsupported", output["reasons"])
            self.assertIsNone(output["turns"]["completed_native_distinct"])
        finally:
            case.doCleanups()

    def test_capture_scope_and_history_partial_are_unknown(self):
        self.values["legacy"]["scope"]["closure_offset"] += 1
        self.assertIn("audit_capture_incomplete:legacy", self.run_cli(2)["reasons"])
        self.values["legacy"]["scope"]["closure_offset"] -= 1
        self.values["discord"]["complete"] = False
        self.assertIn("discord_incomplete", self.run_cli(2)["reasons"])

    def historical_prefix(self, delivered=1):
        meta = json.loads(self.values["transcript"].splitlines()[0])
        old_rows = [dict(type="event_msg", payload=dict(type="task_started", turn_id="old-turn")),
                    dict(type="response_item", payload=dict(type="message", role="assistant", id="old-body",
                         content=[dict(type="output_text", text="historical body")])),
                    dict(type="event_msg", payload=dict(type="task_complete", turn_id="old-turn"))]
        prefix = b"".join(canonical(r)+b"\n" for r in [meta, *old_rows])
        meta_length = len(canonical(meta)+b"\n")
        raw = prefix + self.values["transcript"][meta_length:]
        shift = len(prefix) - meta_length
        self.values["transcript"] = raw
        self.manifest["scope"].update(baseline_offset=len(prefix), fiftieth_offset=len(raw), closure_offset=len(raw))
        self.manifest["source_digests"] = dict(prefix_sha256=digest(prefix), window_sha256=digest(raw[len(prefix):]))
        self.manifest["observation"].update(seq_end=52, confirmation_seq=2)
        for snap in (self.values["baseline"], self.values["end_snapshot"]):
            snap["seq_high_water"] += 1
            snap["counters"]["closed_events"] += 1
            snap["first_confirmation"]["seq"] = 1
        for proof in self.inputs:
            physical = proof["physical_observation_key"]
            physical["start_offset"] += shift
            physical["end_offset"] += shift
        for row in self.values["log"]:
            row["seq"] += 1
        self.values["log"][0]["confirmation_window_start"]["seq"] = 1
        physical = dict(provider="codex", channel_id=123, source_id=self.manifest["scope"]["source"],
                        start_offset=meta_length, end_offset=len(prefix), native_turn_id="old-turn")
        self.inputs.insert(0, dict(physical_observation_key=physical, native_record_digest=digest(canonical([old_rows,"completed",False])),
            input_kind="tty", discord_message_id=None, execution_identity=self.manifest["binding"]["identity"]))
        self.values["log"].insert(0, dict(event="n1_turn_closed", seq=1, observed_at_us=5,
            boot_id=self.manifest["runtime"]["boot_id"], channel_id=123, provider="codex", schema_version=2,
            physical_observation_key=physical, logical_turn_key=dict(provider="codex", channel_id=123,
            provider_session_id="native-session", native_turn_id="old-turn"), autonomous=False, outcome="completed"))
        for name in ("legacy", "debt", "inflight", "pending", "abort"):
            self.values[name]["scope"].update(seq_end=52, closure_offset=len(raw))
        self.values["debt"]["abandoned"] = [dict(source=self.manifest["scope"]["source"], start=meta_length,
            end=len(prefix), reason="adoption debt")]
        self.values["o_init"]["initial_anchor"] = 96
        old_entries, anchor = [], 96
        for n in range(max(1, delivered)):
            old_entries.extend([dict(at="2026-10-12T23:30:00Z", entry=dict(type="prepared", serial=n+1,
                unit_key=dict(channel_id=123, provider="codex", native_key="old-body", kind="body"),
                piece_index=0, payload="historical body", anchor_id=anchor, epoch=1)),
                dict(at="2026-10-12T23:30:00Z", entry=dict(type="posted", serial=n+1, msg_id=97+n)
                     if delivered else dict(type="not_found", serial=n+1))])
            if delivered:
                anchor = 97+n
        for row in self.values["o_ledger"]:
            row["entry"]["serial"] += max(1, delivered)
        self.values["o_ledger"][0]["entry"]["anchor_id"] = anchor
        self.values["o_ledger"] = old_entries + self.values["o_ledger"]
        ids = [str(97+n) for n in range(delivered)]
        self.manifest["discord"]["individual_ids"] = ["96", *ids]
        self.values["debt"]["baseline_messages"] = [dict(message_id="96", payload_hash=digest(b"baseline anchor"), last_changed_seq=0)]
        self.values["discord"]["individual"] = {i: dict(id=i, channel_id="123", author={"id":"456"}, content="historical body") for i in ids}
        self.values["discord"]["individual"]["96"] = dict(id="96", channel_id="123", author={"id":"456"}, content="baseline anchor")

    def test_real_prewindow_debt_settled_missing_and_duplicate_are_separate(self):
        self.historical_prefix(1)
        result = self.run_cli(0)
        self.assertEqual(result["historical_debt"], dict(expected_pieces=1, undelivered=0, duplicates=0, inventory_ranges=1, unresolved=0))
        self.assertEqual(result["delivery"]["expected_pieces"], 50)

    def test_real_prewindow_missing_is_unknown_with_count_not_current_failure(self):
        self.historical_prefix(0)
        result = self.run_cli(2)
        self.assertIn("historical_debt", result)
        self.assertEqual(result["historical_debt"]["undelivered"], 1)
        self.assertEqual(result["delivery"]["delivered_pieces"], 50)
        self.assertEqual(result["failures"], [])

    def test_real_prewindow_duplicates_are_retained_without_current_failure(self):
        self.historical_prefix(2)
        result = self.run_cli(2)
        self.assertIn("historical_debt", result)
        self.assertEqual(result["historical_debt"]["duplicates"], 1)
        self.assertEqual(result["delivery"]["expected_pieces"], 50)
        self.assertEqual(result["failures"], [])

    def test_definite_delivery_failure_survives_missing_debt_or_legacy(self):
        self.messages.pop(0)
        self.refresh_history()
        self.values["debt"]["complete"] = False
        self.values["legacy"]["complete"] = False
        self.assertIn("delivery_missing", self.run_cli(1)["failures"])

    def synthetic_event(self):
        return dict(event="n1_synthetic_create", seq=52, schema_version=2, observed_at_us=100,
            boot_id=self.manifest["runtime"]["boot_id"], channel_id=123, provider="codex", phase="committed",
            site="fixture", turn_source="external_input", synthetic_kind="watcher", turn_nonce="n",
            user_msg_id=0, request_owner_user_id=0, relay_owner_kind="watcher", rebind_origin=False)

    def append_log_rows(self, rows):
        self.values["log"].extend(rows)
        seq_end = self.values["log"][-1]["seq"]
        self.manifest["observation"]["seq_end"] = seq_end
        self.values["end_snapshot"]["seq_high_water"] = seq_end
        counters = {k: 0 for k in self.values["end_snapshot"]["counters"]}
        opened = set()
        for row in self.values["log"]:
            if row["event"] == "n1_turn_closed":
                counters["closed_events"] += 1
                counters["aborted_events"] += row["outcome"] == "aborted"
            elif row["event"] == "n1_synthetic_create":
                counters["create_attempt" if row["phase"] == "attempt" else "create_committed"] += 1
            elif row["event"] == "n1_placeholder_lifecycle":
                op_id = tuple(row["op_id"])
                if row["phase"] == "attempt":
                    counters["attempt"] += 1
                    opened.add(op_id)
                else:
                    opened.discard(op_id)
                    field = "failed_or_uncertain" if row["phase"] == "failed_or_uncertain" else (
                        "succeeded_post" if row["operation"] == "post_placeholder" else "succeeded_patch")
                    counters[field] += 1
        self.values["end_snapshot"].update(counters=counters, open_attempts=len(opened))
        for name in ("legacy", "debt", "inflight", "pending", "abort"):
            self.values[name]["scope"]["seq_end"] = seq_end

    def test_synthetic_commit_has_fail_precedence_over_partial_residue(self):
        self.append_log_rows([self.synthetic_event()])
        self.values["pending"]["complete"] = False
        self.assertIn("synthetic_committed", self.run_cli(1)["failures"])

    def test_unknown_event_missing_closed_counter_drop_and_origin_block_pass(self):
        original = copy.deepcopy(self.values)
        for edit in ("unsupported", "closed", "counter", "drop", "origin"):
            self.values = copy.deepcopy(original)
            if edit == "unsupported":
                self.values["log"][-1]["event"] = "future_event"
            elif edit == "closed":
                self.values["log"][-1]["physical_observation_key"]["native_turn_id"] = "missing"
            elif edit == "counter":
                self.values["end_snapshot"]["counters"]["closed_events"] = 49
            elif edit == "drop":
                self.values["end_snapshot"]["dropped_events"] = 1
            else:
                self.values["legacy"]["inputs"][0]["input_kind"] = "discord"
            self.run_cli(2)

    def test_ledger_unknown_record_and_partial_result_are_unknown(self):
        original = copy.deepcopy(self.values["o_ledger"])
        self.values["o_ledger"][-1]["entry"] = dict(type="future_outcome", serial=50)
        self.assertIn("o_ledger_record_unsupported", self.run_cli(2)["reasons"])
        self.values["o_ledger"] = original[:-1]
        self.assertIn("o_delivery_uncertain", self.run_cli(2)["reasons"])

    def test_search_optional_call_id_none_and_string_remain_supported(self):
        from tests.test_n1_canary_native_cli import NativeCLI
        from tests.test_n1_canary_native import encode
        case = NativeCLI()
        case.setUp()
        try:
            for call_id in (None, "search-id"):
                rows = case.codex_turn()
                rows[5]["payload"] = dict(type="tool_search_call", id="search-1", execution="server", arguments={}, call_id=call_id)
                case.use_native(encode(rows))
                self.assertEqual(case.cli()["turns"]["completed_native_distinct"], 1)
        finally:
            case.doCleanups()

    def test_required_guards_audit_recipe_is_executable(self):
        script = (ENTRY.parent / "ci-script-checks.sh").read_text()
        self.assertIn('if run_check guards "N1 canary final audit contracts"; then\n'
                      '"$PYTHON" -m unittest tests.test_n1_canary_audit\n'
                      '"$PYTHON" tests/run_n1_canary_audit_mutations.py\nfi', script)

    def placeholder_rows(self, outcome="succeeded", patch=False):
        request = dict(event="n1_placeholder_lifecycle", seq=52, schema_version=2, observed_at_us=100,
            boot_id=self.manifest["runtime"]["boot_id"], channel_id=123, provider="codex",
            op_id=[self.manifest["runtime"]["boot_id"], 1], origin="tui_direct_synthetic",
            operation="patch_placeholder" if patch else "post_placeholder", reference=None,
            target_message_id=200 if patch else None, input_message_id=None, phase="attempt", message_id=None, error_class=None)
        result = dict(request, seq=53, observed_at_us=101, phase=outcome,
                      message_id=None if outcome == "failed_or_uncertain" else 200 if patch else 300,
                      error_class="timeout" if outcome == "failed_or_uncertain" else None)
        return [request, result]

    def test_successful_direct_placeholder_fails_even_when_deleted_or_patched(self):
        original = copy.deepcopy(self.values["log"])
        for patch in (False, True):
            self.values["log"] = copy.deepcopy(original)
            self.append_log_rows(self.placeholder_rows(patch=patch))
            result = self.run_cli(1)
            self.assertIn("placeholder_created", result["failures"])
            self.assertEqual(result["effects"]["placeholder_created_confirmed"], 1)

    def test_placeholder_failed_open_and_conflicting_results_are_unknown(self):
        original = copy.deepcopy(self.values["log"])
        for variant in ("failed_or_uncertain", "open", "conflict"):
            rows = self.placeholder_rows(outcome="failed_or_uncertain" if variant == "failed_or_uncertain" else "succeeded")
            if variant == "open":
                rows = rows[:1]
            elif variant == "conflict":
                rows[-1]["operation"] = "patch_placeholder"
                rows[-1]["target_message_id"] = 200
            self.values["log"] = copy.deepcopy(original)
            self.append_log_rows(rows)
            result = self.run_cli(2)
            self.assertIsNone(result["effects"]["placeholder_created_exact"])

    def test_synthetic_fact_survives_partial_discord_and_unknown_native(self):
        self.append_log_rows([self.synthetic_event()])
        self.values["discord"]["complete"] = False
        self.values["transcript"] += b'{"type":"future_record"}\n'
        self.manifest["scope"]["closure_offset"] = len(self.values["transcript"])
        self.manifest["source_digests"]["window_sha256"] = digest(self.values["transcript"])
        self.assertIn("synthetic_committed", self.run_cli(1)["failures"])

    def test_legacy_receipt_and_frontier_are_both_audited(self):
        native_range = self.inputs[0]["physical_observation_key"]
        self.values["legacy"]["confirmed_deliveries"] = [dict(source=dict(provider="codex", tmux_session_name="capture-session",
            turn_nonce="t0", range=[native_range["start_offset"],native_range["end_offset"]], generation_mtime_ns=1,
            offset_authority_channel_id=123, delivery_channel_id=123), delivery_channel_id=123, message_id=300)]
        self.messages.append(dict(id="300", channel_id="123", author={"id":"456"}, content="body 0"))
        self.refresh_history()
        self.values["legacy"]["delivered_frontier"] = dict(range=[native_range["start_offset"],native_range["end_offset"]],
            generation_mtime_ns=1, attempts=1, panel_msg_id=300, panel_channel_id=123)
        self.assertIn("legacy_body_effect", self.run_cli(1)["failures"])
        self.values["legacy"]["confirmed_deliveries"] = []
        result = self.run_cli(2)
        self.assertIn("legacy_frontier_unverified", result["reasons"])
        self.assertIn("discord_message_unexplained", result["reasons"])

    def test_manifest_mismatch_unavailable_and_stage_absence_block_support(self):
        self.manifest["policies"]["ledger_schema"] = 99
        self.assertIn("manifest_invalid", self.run_cli(2)["reasons"])
        self.manifest["policies"]["ledger_schema"] = 1
        self.values["end_snapshot"] = dict(status="unavailable")
        self.assertIn("snapshot_unavailable", self.run_cli(2)["reasons"])

    def test_output_file_contains_the_composed_verdict(self):
        path = self.base / "result.json"
        result = self.run_cli(0, extra=("--output", str(path)))
        self.assertEqual(json.loads(path.read_text()), result)

    def test_default_cli_has_no_test_support_option_or_table_override(self):
        table = json.loads((PACKAGE / "compatibility.json").read_bytes())
        self.assertFalse(table["unknown_outcome_produced"])
        from unittest.mock import patch
        import os
        with patch.dict(os.environ, {"N1_CANARY_SUPPORT_TABLE": str(self.copy / "n1_canary/compatibility.json")}):
            self.assertIn("unknown_outcome_unproduced", self.run_cli(2, supported=False)["reasons"])
        process = subprocess.run([sys.executable, str(ENTRY), "--manifest", str(self.path), "--support-table", "anything"],
                                 capture_output=True, text=True, timeout=20)
        self.assertEqual(process.returncode, 2)
        self.assertIn("unrecognized arguments", process.stderr)

    def test_real_prewindow_unit_reposted_inside_window_is_a_new_duplicate(self):
        self.historical_prefix(1)
        old_key = self.values["o_ledger"][0]["entry"]["unit_key"]
        self.values["o_ledger"].extend([
            dict(at="2026-10-13T00:01:00Z", entry=dict(type="prepared", serial=52, unit_key=old_key,
                 piece_index=0, payload="historical body", anchor_id=249, epoch=1)),
            dict(at="2026-10-13T00:01:00Z", entry=dict(type="posted", serial=52, msg_id=300))])
        self.messages.append(dict(id="300", channel_id="123", author={"id":"456"}, content="historical body"))
        individual = copy.deepcopy(self.values["discord"]["individual"])
        self.refresh_history()
        self.values["discord"]["individual"] = individual
        self.assertIn("delivery_duplicate", self.run_cli(1)["failures"])

    def test_residue_distinguishes_synthetic_from_discord_custody(self):
        self.values["inflight"]["records"] = [dict(provider="codex", channel_id=123, turn_source="external_input",
            request_owner_user_id=0, user_msg_id=0, relay_owner_kind="watcher", rebind_origin=False)]
        self.assertIn("synthetic_residue", self.run_cli(1)["failures"])
        self.values["inflight"]["records"] = [dict(provider="codex", channel_id=123, turn_source="managed",
            request_owner_user_id=789, user_msg_id=98, relay_owner_kind="session_bound_relay", rebind_origin=False)]
        self.manifest["discord"]["individual_ids"].append("98")
        self.values["discord"]["individual"]["98"] = dict(id="98", channel_id="123", author={"id":"789"}, content="input")
        self.assertEqual(self.run_cli(0)["failures"], [])

    def test_current_delivery_cannot_be_hidden_as_a_shared_legacy_receipt(self):
        bounds = self.inputs[0]["physical_observation_key"]
        self.values["legacy"]["confirmed_deliveries"] = [dict(source=dict(provider="codex", tmux_session_name="capture-session",
            turn_nonce="t0", range=[bounds["start_offset"],bounds["end_offset"]], generation_mtime_ns=1,
            offset_authority_channel_id=123, delivery_channel_id=123), delivery_channel_id=123, message_id=200)]
        self.assertIn("legacy_shared_receipt_origin_unknown", self.run_cli(2)["reasons"])

    def test_effect_without_confirmed_mode_is_unknown_rather_than_false_failure(self):
        self.values["log"][0]["event"] = "future_confirmation"
        self.append_log_rows([self.synthetic_event()])
        result = self.run_cli(2)
        self.assertEqual(result["failures"], [])

    def test_default_profile_partial_history_never_reports_exact_zero(self):
        self.values["discord"]["complete"] = False
        result = self.run_cli(2, supported=False)
        self.assertIsNone(result["effects"]["synthetic_committed_exact"])
        self.assertIsNone(result["effects"]["placeholder_created_exact"])

    def test_existing_init_does_not_need_to_be_recreated_for_current_build(self):
        self.values["o_init"]["build_digest"] = "historical-build"
        self.run_cli(0)

    def test_binding_transition_blocks_origin_count_and_source_absence_claims(self):
        self.values["bindings"][1]["source"] = {**self.manifest["scope"]["source"], "ino": 9}
        self.messages.pop(0)
        self.refresh_history()
        result = self.run_cli(2)
        self.assertIsNone(result["turns"]["completed_distinct"])
        self.assertIn("binding_discontinuous", result["reasons"])
        self.assertEqual(result["failures"], [])

    def test_conflicting_individual_lookup_is_unknown_not_payload_failure(self):
        self.manifest["discord"]["individual_ids"].append("200")
        self.values["discord"]["individual"]["200"] = dict(id="200", channel_id="123", author={"id":"456"}, content="changed during collection")
        result = self.run_cli(2)
        self.assertIn("discord_lookup_conflict", result["reasons"])
        self.assertEqual(result["failures"], [])

    def test_o_5xx_is_uncertain_even_if_a_later_retry_has_a_receipt(self):
        prepared = copy.deepcopy(self.values["o_ledger"][0])
        posted = copy.deepcopy(self.values["o_ledger"][1])
        self.values["o_ledger"][1]["entry"] = dict(type="rejected", serial=1, status=500)
        for row in self.values["o_ledger"][2:]:
            row["entry"]["serial"] += 1
        prepared["entry"]["serial"] = posted["entry"]["serial"] = 2
        self.values["o_ledger"][2:2] = [prepared, posted]
        self.assertIn("o_delivery_uncertain", self.run_cli(2)["reasons"])

    def test_incoherent_synthetic_owner_fields_are_unknown(self):
        row = self.synthetic_event()
        row["relay_owner_kind"] = "none"
        self.append_log_rows([row])
        self.assertEqual(self.run_cli(2)["failures"], [])

    def test_effect_outside_declared_closure_does_not_become_a_canary_failure(self):
        self.values["log"].append(self.synthetic_event())
        result = self.run_cli(2)
        self.assertIn("audit_event_invalid", result["reasons"])
        self.assertEqual(result["failures"], [])


if __name__ == "__main__":
    unittest.main()
