"""agy cell E-65 verdicts, input gating, deploy requests and the deploy hook branch (synthetic only)."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO))
from scripts.dev_instance import discord_rest  # noqa: E402
from scripts.dev_instance.driver_admission import Admission, IdentityRefused  # noqa: E402
from scripts.dev_instance.files import InstanceError  # noqa: E402
from scripts.e2e.agy_cell import instance, judge, scenario  # noqa: E402
from scripts.e2e.continuous.store import read_json  # noqa: E402

BOT, ANNOUNCE, CHANNEL, HEAD = "1573471163663785994", "900006", "900004", "a" * 40
FOOTER = "-# ⏱️ 3s"
MANIFEST = {"instance_id": "inst", "head": HEAD, "root": "/nonexistent-agy-root",
            "api": {"self_url": "http://127.0.0.1:18791"},
            "profile": {"name": "agy-headless", "channels": [CHANNEL], "bots": {"antigravity": BOT, "driver": "900003"}}}
ADMITTED = Admission(MANIFEST, {"nonce": "n1", "binary_sha256": "b" * 64},
                     [{"name": "announce_bot_token", "application_id": ANNOUNCE}])


def long_parts(run):
    lines = judge.long_lines(run)
    return ["\n".join(lines[:20]), "\n".join(lines[20:40]), "\n".join(lines[40:]) + "\n" + FOOTER]


class Cell:
    """A synthetic instance: scripted provider rows and lease state per input, on a fake clock."""

    def __init__(self, replies, busy=4.0, session=lambda index: "s1", nonce_after="n1"):
        self.now, self.inputs, self.replies, self.busy = 0.0, [], replies, busy
        self.session, self.nonce_after = session, nonce_after

    def clock(self):
        return self.now

    def sleep(self, seconds):
        self.now += max(seconds, 0.5)

    def send_announce(self, channel, text):
        self.inputs.append((str(1000 * (len(self.inputs) + 1)), self.now, text))
        return self.inputs[-1][0]

    def channel_messages(self, channel, after):
        index = next(i for i, row in enumerate(self.inputs) if row[0] == after)
        sent, run = self.inputs[index][1], self.inputs[index][2].split("-")[1] if "AGY-" in self.inputs[index][2] else None
        rows = []
        for delay, offset, content in self.replies(index, self.inputs[index][2]):
            if self.now >= sent + delay:
                rows.append({"id": str(int(after) + offset), "channel_id": channel, "author": {"id": BOT}, "content": content})
        return rows

    def local(self, url, deadline):
        if url.endswith("/api/health"):
            return {"test_instance": {"nonce": self.nonce_after}}
        if not self.inputs:
            return None
        input_id, sent, _ = self.inputs[-1]
        return {"user_message_id": int(input_id), "turn_nonce": f"t{input_id}"} if self.now < sent + self.busy else None

    def run(self, **patches):
        with mock.patch.object(scenario, "inflight_session",
                               lambda root, channel, input_id: self.session(int(input_id) // 1000 - 1)):
            return scenario.run_e65(ADMITTED, self, scenario.load_spec(), local=self.local, clock=self.clock,
                                    sleep=self.sleep, run_id="r1", **patches)


def good(index, prompt):
    if index == 1:
        return [(1, 1, part) for part in long_parts("r1")[:1]] + [(2, 2, long_parts("r1")[1]), (3, 3, long_parts("r1")[2])]
    marker = prompt.rsplit(" ", 1)[-1]
    return [(1, 1, marker), (2, 2, FOOTER)]


class E65Verdict(unittest.TestCase):
    def test_recorded_short_long_short_sequence_passes_with_every_evidence(self):
        result = Cell(good).run()
        self.assertEqual((result["status"], result["transmissions"], result["reasons"]), ("PASS", 3, {}))
        self.assertEqual(result["tool_observation"], "unobserved")

    def test_false_pass_paths_are_reasons(self):
        lost = lambda i, p: [good(i, p)[0], good(i, p)[2]] if i == 1 else good(i, p)
        duplicated = lambda i, p: good(i, p) + [(4, 9, long_parts("r1")[0])] if i == 1 else good(i, p)
        two = lambda i, p: [(1, 1, "\n".join(long_parts("r1")[:2])), (3, 3, long_parts("r1")[2])] if i == 1 else good(i, p)
        late = lambda i, p: good(i, p) + [(5, 7, p.rsplit(" ", 1)[-1])] if i == 2 else good(i, p)
        unfixed = lambda i, p: good(i, p) + [(4, 1, long_parts("r1")[0] + " ")] if i == 1 else good(i, p)
        no_footer = lambda i, p: [(1, 1, p.rsplit(" ", 1)[-1])] if i == 0 else good(i, p)
        cases = [(Cell(lost), "L", "reassembly_mismatch"), (Cell(duplicated), "L", "duplicate_body"),
                 (Cell(two), "L", "not_split_into_three"), (Cell(late), "T2", "late_body"),
                 (Cell(unfixed, busy=6), "L", "early_fragments_not_fixed"), (Cell(no_footer), "T1", "timeout"),
                 (Cell(good, busy=99), "T1", "lease_not_released"),
                 (Cell(good, session=lambda i: f"s{i}"), "scenario", "conversation_not_continued"),
                 (Cell(good, session=lambda i: None), "scenario", "conversation_not_continued"),
                 (Cell(good, nonce_after="n2"), "scenario", "instance_restarted_or_unobserved")]
        for cell, part, reason in cases:
            with self.subTest(reason=reason):
                result = cell.run()
                self.assertEqual(result["status"], "FAIL")
                self.assertIn(reason, result["reasons"].get(part, []))
        # A turn that never terminates stops the sequence: no further input is sent.
        self.assertEqual(Cell(no_footer).run()["transmissions"], 1)

    def test_only_the_fresh_session_banner_on_the_first_body_is_ignored(self):
        def first(lead, extra=()):
            return lambda i, p: [(1, 1, lead + p.rsplit(" ", 1)[-1]), *extra, (2, 2, FOOTER)] if i == 0 else good(i, p)
        for lead in ("🆕 새 세션 시작\n\n", "🆕 새 세션 시작 (최근 대화 7개를 읽어들였습니다)\n\n"):
            self.assertEqual(Cell(first(lead)).run()["status"], "PASS")
        for cell in (Cell(first("안녕\n\n")), Cell(first("🆕 새 세션 시작\n\n", [(1, 5, "🆕 새 세션 시작\n\nAGY-r1-T1")]))):
            self.assertIn("short_body_mismatch", cell.run()["reasons"].get("T1", []))

    def test_last_turn_needs_its_late_duplicate_window(self):
        turn = judge.Turn("short", "AGY-r1-T2", "3000", "-# ⏱️", done_at=10.0, idle_at=11.0, session="s1",
                          lease_turn={"user_message_id": 3000, "turn_nonce": "x"}, observed_until=14.0)
        turn.observe({"id": "3001", "content": "AGY-r1-T2"}, 9.0)
        self.assertIn("late_window_unobserved", judge.judge_turn(turn, last=True))
        self.assertNotIn("late_window_unobserved", judge.judge_turn(turn, last=False))

    def test_busy_or_unreadable_lease_at_start_sends_nothing(self):
        cell = Cell(good)
        cell.inputs = [("1", 0.0, "foreign")]
        self.assertEqual((cell.run()["status"], len(cell.inputs)), ("UNEVALUABLE(precondition)", 1))
        broken = Cell(good)
        broken.local = mock.Mock(side_effect=InstanceError("down"))
        self.assertEqual((broken.run()["transmissions"], broken.inputs), (0, []))

    def test_refused_driver_admission_never_builds_the_rest_client(self):
        factory = mock.Mock()
        with mock.patch.object(scenario, "admit", side_effect=IdentityRefused("x")):
            result = scenario.execute(Path("/b"), "inst", scenario.load_spec(), rest_factory=factory)
        self.assertEqual((result["status"], result["transmissions"]), ("UNEVALUABLE(identity)", 0))
        factory.assert_not_called()

    def test_announce_input_is_bound_to_the_allowlist_and_announce_author(self):
        calls = []
        def request(role, method, path, body=None):
            calls.append((role, method, path))
            return {"id": "77", "channel_id": CHANNEL, "author": {"id": ANNOUNCE if body["content"] == "ok" else BOT}}
        client = discord_rest.Client(ADMITTED, 0, request=request)
        self.assertEqual(client.send_announce(CHANNEL, "ok"), "77")
        for channel, text in [("900099", "ok"), (CHANNEL, "spoofed")]:
            with self.subTest(channel=channel, text=text), self.assertRaises(InstanceError):
                client.send_announce(channel, text)
        self.assertEqual([call[:2] for call in calls], [("announce", "POST")] * 2)


class DeployRequests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.config = self.root / "agy.json"
        self.save(True)

    def save(self, enabled):
        paths = {key: str(self.root / key) for key in ("state", "worktree", "profile", "admission", "resources")}
        self.config.write_text(json.dumps({
            "schema_version": 1, "enabled": enabled, "state_dir": paths["state"], "worktree": paths["worktree"],
            "profile": paths["profile"], "admission": paths["admission"], "resources": paths["resources"],
            "deploy_lock": str(self.root / "deploy.lock"),
            "credentials": {name: f"op://AgentDesk/Test/{name}" for name in
                            ("antigravity_bot_token", "announce_bot_token", "notify_bot_token", "gemini_api_key")}}))

    def test_switched_off_queues_and_runs_nothing(self):
        self.save(False)
        self.assertEqual(instance.enqueue(self.config, "deploy-1", HEAD), {"status": "DISABLED"})
        run = mock.Mock()
        self.assertEqual(instance.drain(instance.load_config(self.config), run), {"status": "DISABLED"})
        run.assert_not_called()
        self.assertFalse((self.root / "state").exists())

    def test_only_the_newest_request_runs_and_stale_ones_have_no_result(self):
        with mock.patch("time.time", side_effect=[100, 200]):
            for request in ("deploy-1", "deploy-2"):
                self.assertEqual(instance.enqueue(self.config, request, HEAD)["status"], "SUBMITTED")
        config, run = instance.load_config(self.config), mock.Mock(return_value={"status": "PASS"})
        self.assertEqual(instance.drain(config, run, now=300)["superseded"], ["deploy-1"])
        run.assert_called_once()
        self.assertEqual(read_json(self.root / "state/requests.json", {}),
                         {"deploy-1": "SUPERSEDED(deploy-2)", "deploy-2": "PASS"})
        with mock.patch("time.time", return_value=400):
            instance.enqueue(self.config, "deploy-3", HEAD)
        stale = mock.Mock()
        self.assertEqual(instance.drain(config, stale, now=400 + instance.NO_RESULT_S + 1)["status"], "FAIL(FAIL)")
        stale.assert_not_called()
        self.assertEqual(read_json(self.root / "state/results/deploy-3.json", {})["reasons"], {"request": ["NO_RESULT"]})

    def test_ensure_pipes_credentials_and_refuses_an_unclean_head_before_prepare(self):
        config, calls, pipes = instance.load_config(self.config), [], []
        replies = {"rev-parse": HEAD + "\n", "status": "", "prepare": '{"instance_id": "u1"}',
                   "validate": '{"status": "DECLARATION_VALID"}'}
        def execute(argv, timeout, env=None, cwd=None):
            calls.append([str(part) for part in argv])
            return next((value for key, value in replies.items() if key in map(str, argv)), "")
        self.assertEqual(instance.ensure(config, HEAD, execute=execute, pipe=lambda *a: pipes.append(a)), "u1")
        self.assertEqual([source for source, _, _ in pipes],
                         [["agent-op", "read", f"op://AgentDesk/Test/{name}"]
                          for name in ("announce_bot_token", "antigravity_bot_token", "gemini_api_key", "notify_bot_token")])
        self.assertTrue(all("op://" not in " ".join(call) for call in calls))
        self.assertEqual(pipes[0][2]["PATH"].split(":")[0], str(self.root / "worktree/target/debug"))
        replies["status"] = " M dirty\n"
        (self.root / "state/instance.json").unlink()
        calls.clear()
        with self.assertRaises(InstanceError):
            instance.ensure(config, HEAD, execute=execute, pipe=lambda *a: None)
        self.assertFalse(any("prepare" in call for call in calls))
        with self.assertRaises(InstanceError):
            instance.ensure(config, "HEAD", execute=execute)

    def hook(self, agy_config):
        tools = self.root / "tools"
        tools.mkdir(exist_ok=True)
        (tools / "python3").write_text(f"#!/bin/sh\nexec {sys.executable} \"$@\"\n")
        (tools / "python3").chmod(0o755)
        env = {"HOME": str(self.root), "PATH": f"{tools}:/usr/bin:/bin",
               "AGENTDESK_E2E_CONTINUOUS_CONFIG": str(self.root / "absent.json"),
               "AGENTDESK_E2E_AGY_CONFIG": str(agy_config)}
        return subprocess.run(["bash", str(REPO / "scripts/e2e/continuous/after_deploy.sh"), str(REPO), HEAD, "s1"],
                              env=env, capture_output=True, text=True, timeout=60)

    def test_deploy_hook_queues_without_the_continuous_config_and_obeys_the_switch(self):
        done = self.hook(self.config)
        self.assertEqual(done.returncode, 0)
        self.assertEqual(read_json(self.root / "state/inbox/deploy-s1.json", {})["deploy_sha"], HEAD)
        (self.root / "state/inbox/deploy-s1.json").unlink()
        self.save(False)
        done = self.hook(self.config)
        self.assertEqual((done.returncode, '"status": "DISABLED"' in done.stdout), (0, True))
        self.assertFalse((self.root / "state/inbox/deploy-s1.json").exists())
        done = self.hook(self.root / "missing.json")
        self.assertEqual((done.returncode, done.stdout), (0, ""))


if __name__ == "__main__":
    unittest.main()
