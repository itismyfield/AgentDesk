"""E-65 on an admitted agy instance: announce inputs T1 → L → T2, observation, verdict.

Nothing is sent before Driver admission; a busy or unreadable lease at the start sends nothing.
"""
import json
from pathlib import Path
import time
import urllib.error
import uuid

import yaml

from scripts.dev_instance import discord_rest, identity
from scripts.dev_instance.driver_admission import IdentityRefused, admit
from scripts.dev_instance.files import InstanceError
from scripts.dev_instance.manifest import read_json

from . import judge

SPEC = Path(__file__).with_name("E-65.yaml")


def load_spec(path=SPEC):
    spec = yaml.safe_load(Path(path).read_text())
    if (not isinstance(spec, dict) or spec.get("id") != "E-65"
            or [turn.get("name") for turn in spec.get("turns", [])] != ["T1", "L", "T2"]):
        raise ValueError("E-65 spec must declare T1, L and T2")
    return spec


def local_json(url, deadline):
    """GET one instance-local JSON value; `null` is a valid answer, an error is never one."""
    if not url.startswith("http://127.0.0.1:"):
        raise InstanceError("instance request must stay on loopback")
    try:
        with identity._transport(deadline) as opener, opener.open(  # noqa: SLF001
                url, timeout=max(0.1, min(5, deadline - time.monotonic()))) as response:
            data = response.read(65537)
            if response.status != 200 or len(data) > 65536:
                raise InstanceError("instance response refused")
            return json.loads(data)
    except (urllib.error.URLError, OSError, ValueError):
        raise InstanceError("instance endpoint unavailable") from None


def inflight_session(root, channel, input_id):
    """The provider session of the in-flight turn for this input, when its row is visible."""
    try:
        row = read_json(Path(root) / "runtime/discord_inflight/antigravity" / f"{channel}.json")
    except InstanceError:
        return None
    if str(row.get("user_msg_id")) == input_id and isinstance(row.get("session_id"), str) and row["session_id"]:
        return row["session_id"]
    return None


def observe(turn, client, channel, bot, lease_url, root, *, last, deadline, local, clock, sleep, poll):
    while True:
        now = clock()
        for row in client.channel_messages(channel, turn.input_id):
            if str(row.get("author", {}).get("id")) == bot:
                turn.observe(row, now)
        try:
            lease = local(lease_url, deadline)
        except InstanceError:
            lease = "unreadable"
        if isinstance(lease, dict) and str(lease.get("user_message_id")) == turn.input_id:
            turn.lease_turn, turn.idle_at = lease, None
        elif lease is None and turn.lease_turn is not None and turn.idle_at is None:
            turn.idle_at = now
        turn.session = turn.session or inflight_session(root, channel, turn.input_id)
        turn.observed_until = now
        if turn.terminal() and (not last or now >= turn.done_at + judge.LATE_WINDOW_S):
            return
        if now >= deadline:
            turn.timed_out = not turn.terminal()
            return
        sleep(min(poll, max(0, deadline - now)))


def run_e65(admitted, client, spec, *, local=local_json, clock=time.monotonic, sleep=time.sleep, run_id=None):
    manifest = admitted.manifest
    channel, bot = manifest["profile"]["channels"][0], manifest["profile"]["bots"]["antigravity"]
    base, root = manifest["api"]["self_url"], manifest["root"]
    lease_url = f"{base}/api/turn-lease/antigravity/{channel}"
    run_id = run_id or uuid.uuid4().hex[:12]
    result = {"scenario": "E-65", "run_id": run_id, "instance_id": manifest["instance_id"],
              "nonce": admitted.run["nonce"], "head": manifest["head"],
              "binary_sha256": admitted.run["binary_sha256"], "transmissions": 0, "inputs": {}}
    try:
        busy = local(lease_url, clock() + spec["preflight_s"])
    except InstanceError:
        return {**result, "status": "UNEVALUABLE(precondition)", "reasons": {"preflight": ["lease_unreadable"]}}
    if busy is not None:
        return {**result, "status": "UNEVALUABLE(precondition)", "reasons": {"preflight": ["channel_busy"]}}
    turns = []
    for index, step in enumerate(spec["turns"]):
        if turns and not turns[-1][1].terminal():
            break
        expected = (judge.long_lines(run_id) if step["kind"] == "long"
                    else step["marker"].format(run=run_id))
        input_id = client.send_announce(channel, step["prompt"].format(run=run_id))
        result["transmissions"] += 1
        result["inputs"][step["name"]] = input_id
        turn = judge.Turn(step["kind"], expected, input_id, spec["footer_prefix"])
        observe(turn, client, channel, bot, lease_url, root, last=index == len(spec["turns"]) - 1,
                deadline=clock() + step["timeout_s"] + (judge.LATE_WINDOW_S if index == len(spec["turns"]) - 1 else 0),
                local=local, clock=clock, sleep=sleep, poll=spec["poll_s"])
        turns.append((step["name"], turn))
        result.setdefault("bodies", {})[step["name"]] = sorted(turn.bodies, key=int)
    try:
        after = local(base + "/api/health", clock() + spec["judge_s"])["test_instance"]["nonce"]
    except (InstanceError, KeyError, TypeError):
        after = None
    outcome = judge.verdict(turns, admitted.run["nonce"], after)
    if len(turns) != len(spec["turns"]):
        outcome["status"] = "FAIL"
        outcome["reasons"].setdefault("scenario", []).append("sequence_stopped_before_" + spec["turns"][len(turns)]["name"])
    return {**result, **outcome}


def execute(base, instance_id, spec, *, identity_request=None, rest_factory=None, **options):
    """Admit the instance Driver first; any refusal before the first send is input 0."""
    deadline = time.monotonic() + spec["admission_s"] + sum(step["timeout_s"] for step in spec["turns"]) + spec["judge_s"]
    try:
        admitted = admit(base, instance_id, deadline, request=identity_request)
    except IdentityRefused:
        return {"scenario": "E-65", "status": "UNEVALUABLE(identity)", "transmissions": 0}
    if admitted.manifest["profile"]["name"] != "agy-headless":
        return {"scenario": "E-65", "status": "UNEVALUABLE(identity)", "transmissions": 0}
    client = (rest_factory or discord_rest.Client)(admitted, deadline)
    try:
        return run_e65(admitted, client, spec, **options)
    except (InstanceError, OSError, ValueError, KeyError, TypeError):
        return {"scenario": "E-65", "status": "FAIL", "reasons": {"scenario": ["observation_failed"]},
                "instance_id": instance_id}
