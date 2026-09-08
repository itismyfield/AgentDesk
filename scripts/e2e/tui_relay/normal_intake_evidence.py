"""E36 normal-intake source consumer; no producer hooks or recovery actions."""

from __future__ import annotations

import json
import re
import shlex
from pathlib import Path

from . import assertions

KEYS = [f"S{i:02}" for i in range(1, 11)] + ["QA", "QB"]
PHASE_SECONDS = 3540
# Named for the reviewer, not tuned here: changing any value changes the acceptance profile.
REQUEST_DEADLINE_S = 240  # per-request response budget the phase reserve arithmetic is sized against.
QUEUE_COMMIT_OBSERVATION_S = 20  # QA holds a 20s tool, so QB's commit row must appear inside it.
LOG_SAMPLE_BYTES = 65536  # trailing log slice read once to prove the adk-tracing-text-v1 field shape.
BOUNDED_READ_BYTES = 1_048_576  # per-observation read ceiling; a larger tail means the source outran us.
LOG_TARGET = "agentdesk::services::discord::router::intake_queue_transaction"
LOG_MESSAGE = "discord intake queue transaction committed"


def validate(scenario, args):
    steps = scenario.get("steps", [])
    if (args.cell != "claude-tui" or args.reset_before_each or args.hard_reset_session_each
            or args.phase_deadline_s != PHASE_SECONDS or args.filter != "E-36"
            or getattr(args, "_e36_phase_started", None) is None
            or args.final_refetches != 2 or args.final_refetch_interval_s != 1
            or scenario.get("durable_delivery_probe")
            or scenario.get("e36_normal_intake") != {"sequential_count": 10, "queued_followup": True,
                                                     "reset_free": True, "intake_log_format": "adk-tracing-text-v1"}
            or [s.get("request_key") for s in steps] != KEYS
            or any(set(s) - {"request_key", "send_discord_prompt", "body_marker", "hold_marker"}
                   or not s.get("send_discord_prompt") or not s.get("body_marker") for s in steps)):
        raise assertions.AssertionError("E36 requires its twelve normal inputs, reset-free single-cell 3540s profile")


def cursor(path):
    stat = path.stat()
    return {"device": stat.st_dev, "inode": stat.st_ino, "offset": stat.st_size}


def tail(path, before):
    """Complete lines only; preserve incomplete suffix and reject lost generation."""
    current = cursor(path)
    if any(current[k] != before[k] for k in ("device", "inode")) or current["offset"] < before["offset"]:
        raise ValueError(f"source generation lost: {path}")
    with path.open("rb") as stream:
        stream.seek(before["offset"])
        data = stream.read(BOUNDED_READ_BYTES + 1)
    if len(data) > BOUNDED_READ_BYTES:
        raise ValueError(f"source observation exceeds bounded read: {path}")
    offset = before["offset"]
    for line in data.splitlines(keepends=True):
        if not line.endswith(b"\n"):
            break
        end = offset + len(line)
        yield {"start": offset, "end": end}, line.decode("utf-8", "strict")
        offset = end


def log_rows(lines):
    for locator, raw in lines:
        line = re.sub(r"\x1b\[[0-9;]*m", "", raw)
        if LOG_TARGET not in line or not re.search(r"\b(?:INFO|WARN)\b", line):
            continue
        message = LOG_MESSAGE if LOG_MESSAGE in line else "discord intake queue transaction finished with failed commit"
        if message not in line:
            continue
        fields = dict(part.split("=", 1) for part in shlex.split(line.split(message, 1)[1]) if "=" in part)
        if not {"channel_id", "message_id", "source", "outcome", "persistence_error"} <= fields.keys():
            raise ValueError("intake INFO format lacks required fields")
        yield {**fields, "locator": locator, "raw": raw}


def tool_result(row):
    """A user row carrying a tool_result block is the same turn's tool output, never new input."""
    blocks = row.get("message", {}).get("content", [])
    return isinstance(blocks, list) and any(
        isinstance(b, dict) and b.get("type") == "tool_result" for b in blocks)


def content(row):
    value = row.get("message", {}).get("content", [])
    return value if isinstance(value, str) else "\n".join(
        b.get("text", "") for b in value if isinstance(b, dict) and b.get("type") == "text")


def native_chain(path, before, request, session):
    rows = [(loc, json.loads(raw)) for loc, raw in tail(path, before) if raw.strip()]
    if any(not isinstance(row, dict) for _, row in rows):
        raise ValueError("native JSONL entry is not an object")
    inputs = [i for i, (_, row) in enumerate(rows)
              if row.get("type") == "user" and not tool_result(row)
              and request["prompt"].strip() in content(row)]
    if len(inputs) > 1:
        raise assertions.AssertionError("E36 repeated native prompt execution")
    if not inputs:
        raise ValueError("native request input unavailable")
    index = inputs[0]
    entry_loc, entry = rows[index]
    if entry.get("sessionId") != session:
        raise assertions.AssertionError("E36 native input session mismatch")
    work = []
    for loc, row in rows[index + 1:]:
        if row.get("type") == "user" and content(row) and not tool_result(row):
            break  # hook/system-reminder text rides along with tool_result; that is not new input.
        if row.get("type") == "assistant":
            if row.get("sessionId") != session:
                raise assertions.AssertionError("E36 native work session mismatch")
            work.append({"locator": loc, "id": row.get("uuid"), "text": content(row)})
    matched = [row for row in work if request["body_marker"] in row["text"]]
    if not matched:
        raise ValueError("native assistant response unavailable")
    if sum(row["text"].count(request["body_marker"]) for row in work) != 1:
        raise assertions.AssertionError("E36 repeated native assistant marker")
    return {"input_id": entry.get("uuid"), "input_locator": entry_loc, "work": work,
            "terminal_body_locator": matched[0]["locator"]}


def request_window(window, request, rows=None, excluded=()):
    sub = assertions.Window(str(request["discord_before_id"]))
    upper = request.get("discord_closed_after_id", float("inf"))
    for row in window.raw_messages if rows is None else rows:
        mid = str(row.get("id", ""))
        if not mid.isdigit():
            raise ValueError("Discord snapshot contains nonnumeric message ID")
        if request["discord_before_id"] < int(mid) <= upper and mid not in excluded:
            sub.add(row)
    return sub


def completion_candidates(sub):
    return {str(row["id"]) for row in assertions._raw_assertion_messages(sub)
            if any(p.search(row.get("content", "")) for p in assertions._COMPLETION_CHROME_PATTERNS)}


def completion_id(sub, request):
    ids = completion_candidates(sub)
    request["completion_candidate_ids"] = sorted(ids)
    if len(ids) > 1:
        raise assertions.AssertionError(f"E36 completion ownership ambiguous: {sorted(ids)}")
    if len(ids) != 1:
        raise assertions.AssertionError("E36 requires one completion ID per request")
    assertions.completion_chrome_after_body(sub, body_marker=request["body_marker"], required=True)
    bodies = [row for row in sub.messages if request["body_marker"] in (assertions.relay_body(row) or "")]
    if sum((assertions.relay_body(row) or "").count(request["body_marker"]) for row in bodies) != 1:
        raise assertions.AssertionError("E36 missing or repeated body marker")
    body_ids = [str(row["id"]) for row in bodies]
    if request.get("response_ids", body_ids) != body_ids:
        raise assertions.AssertionError("E36 final body identity changed")
    request.setdefault("response_ids", body_ids)
    return next(iter(ids))


def hold_publication_ids(window, observed, rows, marker):
    """IDs carrying QA's hold prefix: one bearer per final view, never repeated in any state.

    Edit history proves duplicates the product later edited away, so it may not stand in for
    the prefix still being published: both contracts keep the PRE text in the final view.
    """
    def bearer(mid, body):
        if body is None or marker not in body:
            return False
        if body.count(marker) != 1:
            raise assertions.AssertionError(f"E36 repeated hold publication in {mid}")
        return True

    final = [{str(row["id"]) for row in view if bearer(str(row["id"]), assertions.relay_body(row))}
             for view in (window.raw_messages, rows)]
    if any(len(view) != 1 for view in final):
        raise assertions.AssertionError(f"E36 hold publication changed in a final view: {[sorted(v) for v in final]}")
    ids = set().union(*final)
    for update in window.message_updates:
        mid = str(update["id"])
        if mid not in observed:
            continue
        for field in ("before", "after"):
            if bearer(mid, assertions.relay_body({**observed[mid], "content": update.get(field) or ""})):
                ids.add(mid)
    return ids


def edited_completion_candidates(window, observed, after_id):
    """Chrome we already observed cannot be retired by a later edit of the same message."""
    ids = set()
    for update in window.message_updates:
        mid = str(update["id"])
        if (row := observed.get(mid)) is None or assertions.is_our_send(row) or int(mid) <= after_id:
            continue
        if any(p.search(update.get(field) or "") for field in ("before", "after")
               for p in assertions._COMPLETION_CHROME_PATTERNS):
            ids.add(mid)
    return ids


def drained(state):
    """Rowless idle reads null unread_bytes as no-unread, never as UNMEASURED.

    The product can only count a tail against an inflight row's relay frontier, so with no
    row it reports null by design (measured 2026-09-08: idle claude-tui channel
    1484912492202168431 -> unread_bytes null, inflight_state_present false). Folding null to
    0 while a row exists would read an unknown tail as a drained one, so that stays an error.
    """
    unread = state.get("unread_bytes")
    if unread is None:
        if state.get("inflight_state_present") is not False:
            raise ValueError("E36 unread_bytes unmeasured while an inflight row is present")
        unread = 0
    return unread == 0 and state.get("has_pending_queue") is False


class Evidence:
    def __init__(self, driver, args, channel, record):
        self.d, self.args, self.channel, self.record = driver, args, channel, record
        self.root = Path(args.queue_runtime_root)
        self.log = Path(args.e36_intake_log or "")
        if not self.log.is_absolute():
            raise ValueError("E36 requires an absolute --e36-intake-log")
        self.baseline = self.watcher()
        self.path = Path(self.baseline["bound_output_path"])
        self.session = self.baseline["bound_session_id"]
        self.generation = cursor(self.path)
        self.log_cursor = cursor(self.log)
        sample_start = {**self.log_cursor, "offset": max(0, self.log_cursor["offset"] - LOG_SAMPLE_BYTES)}
        if not any(log_rows(tail(self.log, sample_start))):
            raise ValueError(f"E36 intake log {self.log}: INFO/format evidence unavailable")
        record["e36_acceptance"].update(phase_seconds=PHASE_SECONDS, lease_ttl_s=3600,
                                      core_allowance_s=3503, pessimistic_subtotal_s=6054)
        self.idle()

    def watcher(self):
        _, state = self.d._read_api_json(self.args.base_url, f"/api/channels/{self.channel}/watcher-state")
        if not isinstance(state, dict) or not all(state.get(k) for k in ("bound_output_path", "bound_session_id", "tmux_session")):
            raise ValueError("E36 watcher native binding unavailable")
        return state

    def idle(self):
        result = self.d.assert_cell_idle(base_url=self.args.base_url, channel_id=self.channel,
                                        cell=self.args.cell, runtime_root=self.root)
        state = self.watcher()
        if not drained(state):
            raise assertions.AssertionError("E36 final watcher still has unread input/queue")
        return {**result, "watcher": state}

    def reserve(self, key):
        remaining = PHASE_SECONDS - (self.d.time.monotonic() - self.args._e36_phase_started)
        self.record["e36_acceptance"]["remaining_phase_s"] = remaining
        if remaining < (545 if key == "QA" else 295):
            self.record["e36_acceptance"]["budget_exhausted_stage"] = key
            raise self.d.HarnessEvidenceError(f"E36 phase budget unavailable before {key}")

    def admission(self, request):
        path = self.d.provider_inflight_state_path(runtime_root=self.root, provider="claude", channel_id=self.channel)
        while self.d.time.monotonic() < request["deadline"]:
            try:
                row = json.loads(path.read_text())
            except FileNotFoundError:
                self.d.time.sleep(0.1)
                continue
            if not self.d._provider_hold_identity_mismatch(row, request["turn_identity"]):
                sources = row.get("source_message_ids")
                if not sources:
                    raise ValueError("E36 admitted source lineage unavailable")
                if sources != [int(request["inbound_message_id"])]:
                    raise assertions.AssertionError("E36 merged or missing source lineage")
                if row.get("session_id") != self.session:
                    raise assertions.AssertionError("E36 admitted provider session mismatch")
                request["admission"] = {k: row.get(k) for k in (
                    "user_msg_id", "source_message_ids", "turn_nonce", "session_id", "output_path")}
                return
            self.d.time.sleep(0.1)
        raise self.d.HarnessEvidenceError("E36 exact admitted inflight identity unavailable")

    def join(self, request):
        state = self.watcher()
        if any(state[k] != self.baseline[k] for k in ("bound_session_id", "bound_output_path", "tmux_session")):
            raise assertions.AssertionError("E36 channel/session binding changed")
        request["native"] = native_chain(self.path, request["native_before"], request, self.session)

    def queue(self, request, prior):
        matches = []
        deadline = min(prior["deadline"], self.d.time.monotonic() + QUEUE_COMMIT_OBSERVATION_S)
        while self.d.time.monotonic() < deadline and not matches:
            rows = list(log_rows(tail(self.log, request["log_before"])))
            matches = [r for r in rows if r["channel_id"] == self.channel and r["message_id"] == request["inbound_message_id"]]
            if matches:
                request["queue"] = matches[-1]  # Preserve exact commit before sampling transient ownership.
                if any(request["queue"][k] != v for k, v in
                       {"source": "busy_active_turn", "outcome": "enqueued", "persistence_error": "none"}.items()):
                    raise assertions.AssertionError("E36 explicit queue refusal/failure")
            state = self.watcher()
            owner = str(state.get("mailbox_active_user_msg_id"))
            request["queue_observation"] = {"active_message_id": state.get("mailbox_active_user_msg_id"),
                                            "expected_prior_message_id": prior["inbound_message_id"],
                                            "active_owner_sampled": owner == prior["inbound_message_id"]}
            if not matches:
                if owner != prior["inbound_message_id"]:
                    raise ValueError("E36 QA activity ended before any QB commit became observable")
                self.d.time.sleep(0.1)
        if not matches:
            raise ValueError(f"E36 intake log {self.log}: exact QB commit unavailable")
        # source=busy_active_turn on QB's own commit is the ownership evidence; a mailbox
        # sample that missed the handover does not retract it.
        request["queue"]["active_prior_message_id"] = prior["inbound_message_id"]

    def hold_input(self, request):
        request["hold_native"] = native_chain(self.path, request["native_before"],
            {**request, "body_marker": request["hold_marker"]}, self.session)


def run(evidence, scenario, client, window, record, run_id, after_id):
    d, args, channel = evidence.d, evidence.args, evidence.channel
    requests = record["discord_prompt_records"]

    def ingest(rows):
        for row in rows:
            window.add(row)

    def send(step):
        evidence.reserve(step["request_key"])
        ingest(client.fetch_messages(channel, after_id=after_id, limit=100))
        request = {k: str(v).replace("{run_id}", run_id) for k, v in step.items()}
        request.update(prompt=request.pop("send_discord_prompt"), send_state="attempted",
                       discord_before_id=max([int(after_id)] + [int(r["id"]) for r in window.raw_messages]),
                       native_before=cursor(evidence.path), log_before=cursor(evidence.log))
        requests.append(request)  # same list and dict reach the existing partial sink.
        response = client.send(channel, request["prompt"])
        request.update(send_response=response, send_state="acknowledged", acknowledged_at=d.time.monotonic())
        identity = d.turn_identity_from_send_response(response, channel_id=channel)
        request.update(turn_identity=identity, inbound_message_id=identity["user_msg_id"],
                       deadline=request["acknowledged_at"] + REQUEST_DEADLINE_S)
        if int(identity["user_msg_id"]) <= 0 or str(response.get("channel_id", channel)) != channel:
            raise ValueError("E36 invalid normal-send channel/message identity")
        record["real_provider_contacted"] = True
        window.mark_prompt_sent()
        return request

    def complete(request, excluded=(), narrow=None):
        found, observed = d.wait_for_discord_text_with_tui_idle_draft_guard(
            client=client, channel_id=channel, cell=args.cell, after_id=str(request["discord_before_id"]),
            needle=request["body_marker"], prompt=request["prompt"], thread_channel_id=None,
            timeout_s=max(0, request["deadline"] - d.time.monotonic()), debug_label="E36:" + request["request_key"])
        ingest(observed)
        if not found:
            raise assertions.AssertionError(f"E36 response missing within {REQUEST_DEADLINE_S}s")
        sub = request_window(window, request, excluded=excluded)

        def refetch():
            ingest(client.fetch_messages(channel, after_id=after_id, limit=100))
            fresh = request_window(window, request, excluded=excluded)
            sub.raw_messages[:], sub.messages[:] = fresh.raw_messages, fresh.messages

        # R3: no native/queue/log I/O between matching body and this call.
        try:
            d.run_assertion({"completion_chrome_after_body": {"body_marker": request["body_marker"], "required": True}},
                            window=sub, record=record, run_id=run_id, pending_refetch=refetch)
        except assertions.AssertionError:
            if narrow is not None and len(completion_candidates(sub)) > 1:
                narrow(window, request)  # reclassify a rival completion before the generic failure
            raise
        if narrow is not None and len(completion_candidates(sub)) > 1:
            narrow(window, request)
            sub = request_window(window, request, excluded=excluded)
        request["completion_message_id"] = completion_id(sub, request)
        request["discord_closed_after_id"] = max(int(r["id"]) for r in sub.raw_messages)
        evidence.join(request)

    for step in scenario["steps"][:10]:
        request = send(step)
        evidence.admission(request)
        complete(request)
        request["idle"] = evidence.idle()
        record["e36_acceptance"]["sequential_completed"] = len(requests)
    qa = send(scenario["steps"][10])
    evidence.admission(qa)
    hold = d.wait_for_provider_hold_state(runtime_root=evidence.root, provider="claude", channel_id=channel,
        expected_identity=qa["turn_identity"], ok_marker=qa["hold_marker"], late_marker=qa["body_marker"],
        timeout_s=min(180, max(0, qa["deadline"] - d.time.monotonic())), poll_interval_s=0.1)
    qa["hold"] = hold
    if not hold["provider_hold_observed"] or hold["terminal_delivery_committed"]:
        raise assertions.AssertionError("E36 QA did not enter its exact hold")
    evidence.hold_input(qa)
    qb = send(scenario["steps"][11])
    evidence.queue(qb, qa)
    complete(qa, narrow=sequential_boundary(evidence, qa, qb))
    evidence.admission(qb)
    complete(qb, qa["response_ids"] + [qa["completion_message_id"]])


def turn_order(path, before, qa, qb, session):
    """First native offsets of QA's terminal body and of QB's own input, in one transcript pass."""
    qa_body = qb_input = None
    for locator, raw in ((loc, line) for loc, line in tail(path, before) if line.strip()):
        row = json.loads(raw)
        if not isinstance(row, dict):
            raise ValueError("native JSONL entry is not an object")
        if row.get("sessionId") not in (None, session):
            continue
        if row.get("type") == "assistant" and qa_body is None and qa["body_marker"] in content(row):
            qa_body = locator["end"]
        if (row.get("type") == "user" and qb_input is None and not tool_result(row)
                and qb["prompt"].strip() in content(row)):
            qb_input = locator["start"]
    return qa_body, qb_input


def sequential_boundary(evidence, qa, qb):
    """Attribute a rival completion in QA's open window instead of assuming a bypass.

    QB's busy_active_turn commit proves it was enqueued while QA held the turn, not that it ran
    first, so a late poll catching two ordered turns on one page is not the defect. Native order
    decides: QB's input before QA's terminal body is the early execution E-36 targets, otherwise
    QA's window closes below QB's first publication. Neither shown -> unevaluable."""
    def narrow(window, request):
        qa_body_end, qb_input_start = turn_order(evidence.path, qa["native_before"], qa, qb, evidence.session)
        order = request["queued_followup_order"] = {"qa_body_end": qa_body_end, "qb_input_start": qb_input_start}
        if qa_body_end is None or qb_input_start is None:
            raise evidence.d.HarnessEvidenceError("E36 rival completion unattributable: native turn order incomplete")
        if qb_input_start < qa_body_end:
            raise assertions.AssertionError(
                "E36 completion ownership ambiguous: queued follow-up executed before QA terminal body")
        bodies = [int(row["id"]) for row in window.raw_messages
                  if int(row["id"]) > request["discord_before_id"]
                  and qb["body_marker"] in (assertions.relay_body(row) or "")]
        if not bodies:
            raise evidence.d.HarnessEvidenceError("E36 rival completion unattributable: QB publication absent")
        order["qb_first_body_id"] = request["discord_closed_after_id"] = min(bodies)
        request["discord_closed_after_id"] -= 1
    return narrow


def final_assertion(window, record, rows):
    requests = record["discord_prompt_records"]
    if len(rows) >= 100:
        raise ValueError("E36 final snapshot capped; interval coverage unavailable")
    if [r["request_key"] for r in requests] != KEYS:
        raise assertions.AssertionError("E36 needs ten sequential inputs and separate QA/QB")
    if len({r["inbound_message_id"] for r in requests}) != 12 or len({r["completion_message_id"] for r in requests}) != 12:
        raise assertions.AssertionError("E36 repeated input/completion ID")
    native_ids = {r["native"].get("input_id") or str(r["native"]["input_locator"]) for r in requests}
    if len(native_ids) != 12 or not requests[-1].get("queue") or not requests[-2].get("hold_native"):
        raise assertions.AssertionError("E36 distinct native inputs/actual queued hold missing")
    observed = {str(row["id"]): row for row in window.raw_messages}
    for request in requests:
        marker = request["body_marker"]
        # Window.add keeps prior content only here; editing a duplicate away is not repair.
        for update in window.message_updates:
            for field in ("before", "after"):
                text = update.get(field, "")
                if marker not in text:
                    continue
                mid = str(update["id"])
                body = assertions.relay_body({**observed[mid], "content": text})
                if body is not None and marker in body and (
                        mid not in request["response_ids"] or body.count(marker) != 1):
                    raise assertions.AssertionError(f"E36 observed edited duplicate publication: {request['request_key']}")
        # A scoped upper bound must not hide a later publication of this body.
        for view in (window.raw_messages, rows):
            bodies = {str(row["id"]): body for row in view
                      if (body := assertions.relay_body(row)) is not None and marker in body}
            if (sum(body.count(marker) for body in bodies.values()) != 1
                    or set(bodies) != set(request["response_ids"])):
                raise assertions.AssertionError(f"E36 global body publication count/identity changed: {request['request_key']}")
        # The QA hold prefix is a publication too, but #5731 lets the product land it on its own
        # message ID (contract B) instead of the terminal body's, so bind it to a count and not
        # to response_ids: E-22's stranded shape is the one that puts HOLD on two IDs.
        if request.get("hold_marker"):
            hold_ids = hold_publication_ids(window, observed, rows, request["hold_marker"])
            if len(hold_ids) != 1:
                raise assertions.AssertionError(
                    f"E36 hold publication count/identity changed: {request['request_key']} {sorted(hold_ids)}")
        excluded = requests[10]["response_ids"] + [requests[10]["completion_message_id"]] if request["request_key"] == "QB" else ()
        if completion_id(request_window(window, request, rows, excluded), request) != request["completion_message_id"]:
            raise assertions.AssertionError("E36 final completion identity changed")
        if not request.get("admission") or not request.get("native"):
            raise assertions.AssertionError("E36 input/work chain missing")
    # A completion outside every closed request window is still a completion: the scoped upper
    # bounds must not silently drop one, so every candidate has to be an attributed selection.
    scope = {"discord_before_id": requests[0]["discord_before_id"]}
    selected = {r["completion_message_id"] for r in requests}
    for candidates in [completion_candidates(request_window(window, scope, view)) for view in (None, rows)] + [
            edited_completion_candidates(window, observed, scope["discord_before_id"])]:
        if stray := candidates - selected:
            raise assertions.AssertionError(f"E36 unattributed completion chrome: {sorted(stray)}")
    assertions.no_duplicate_content(window)
    record["e36_acceptance"]["completed_inputs"] = 12
