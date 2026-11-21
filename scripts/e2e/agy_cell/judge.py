"""E-65 verdicts from observed Discord bodies, completion footers and turn leases.

A verdict is PASS only when every required piece of evidence was observed; a missing
observation is a reason, never a silent success.
"""
from dataclasses import dataclass, field

LATE_WINDOW_S = 5.0


def long_lines(run):
    """The exact long answer E-65 asks for: start marker, 55 numbered lines, end marker."""
    return [f"START-{run}", *(f"{n:02d} " + "0123456789" * 8 for n in range(1, 56)), f"END-{run}"]


def split_footer(content, footer_prefix):
    """Return (body without trailing `-# ` chrome lines, whether a completion footer was among them)."""
    lines = content.rstrip("\n").split("\n")
    footer = False
    while lines and lines[-1].startswith("-# "):
        footer = footer or lines[-1].startswith(footer_prefix)
        lines.pop()
    return "\n".join(lines), footer


@dataclass
class Turn:
    kind: str
    expected: object
    input_id: str
    footer_prefix: str
    bodies: dict = field(default_factory=dict)
    done_at: float = None
    lease_turn: dict = None
    idle_at: float = None
    session: str = None
    observed_until: float = None
    timed_out: bool = False

    def observe(self, row, now):
        """Fold one provider-authored row; an edit of the same ID replaces its body."""
        body, footer = split_footer(row.get("content") or "", self.footer_prefix)
        if footer and self.done_at is None:
            self.done_at = now
        if not body.strip():
            return
        current = self.bodies.get(row["id"])
        if current is None:
            self.bodies[row["id"]] = {"content": body, "first_seen": now, "changed": now}
        elif current["content"] != body:
            current.update(content=body, changed=now)

    def terminal(self):
        return self.done_at is not None and self.lease_turn is not None and self.idle_at is not None

    def ordered(self):
        return [self.bodies[key] for key in sorted(self.bodies, key=int)]


def judge_turn(turn, last):
    reasons = []
    if turn.timed_out:
        reasons.append("timeout")
    if turn.done_at is None:
        reasons.append("no_completion_footer")
    if turn.lease_turn is None or str(turn.lease_turn.get("user_message_id")) != turn.input_id:
        reasons.append("turn_identity_unobserved")
    if turn.idle_at is None:
        reasons.append("lease_not_released")
    bodies = turn.ordered()
    contents = [body["content"] for body in bodies]
    if len(set(contents)) != len(contents):
        reasons.append("duplicate_body")
    if turn.done_at is not None:
        if any(body["first_seen"] > turn.done_at for body in bodies):
            reasons.append("late_body")
        if last and (turn.observed_until is None or turn.observed_until < turn.done_at + LATE_WINDOW_S):
            reasons.append("late_window_unobserved")
    if turn.kind == "short":
        if [content.strip() for content in contents] != [turn.expected]:
            reasons.append("short_body_mismatch")
        return reasons
    if len(bodies) < 3:
        reasons.append("not_split_into_three")
    elif turn.done_at is None or any(body["first_seen"] >= turn.done_at or body["changed"] > turn.done_at
                                     for body in bodies[:2]):
        reasons.append("early_fragments_not_fixed")
    lines = [line for line in "\n".join(contents).split("\n") if line.strip()]
    if lines != turn.expected:
        reasons.append("reassembly_mismatch")
    return reasons


def verdict(turns, health_nonce_before, health_nonce_after):
    """Combine the T1 → L → T2 turns into one E-65 result."""
    reasons = {}
    for index, (name, turn) in enumerate(turns):
        found = judge_turn(turn, last=index == len(turns) - 1)
        if found:
            reasons[name] = found
    nonces = [turn.lease_turn.get("turn_nonce") for _, turn in turns if turn.lease_turn]
    if len(nonces) != len(turns) or len(set(nonces)) != len(nonces):
        reasons.setdefault("scenario", []).append("turn_ids_not_distinct")
    sessions = {turn.session for _, turn in turns}
    if None in sessions or len(sessions) != 1:
        reasons.setdefault("scenario", []).append("conversation_not_continued")
    if health_nonce_before is None or health_nonce_before != health_nonce_after:
        reasons.setdefault("scenario", []).append("instance_restarted_or_unobserved")
    return {"status": "FAIL" if reasons else "PASS", "reasons": reasons, "tool_observation": "unobserved",
            "output_path": "legacy"}
