#!/usr/bin/env python3
"""Check the observation producer's effect boundary and required call sites."""
import re
import sys
from pathlib import Path

from check_o_shadow_write_zero import TOKEN, expand

ROOT = Path(__file__).resolve().parent.parent
PRODUCER = "src/services/tui_o/n1_observation.rs"
HOOKS = {
    "src/services/tui_o/writer/input_facts.rs": "observation::turn_closed",
    "src/services/discord/inflight/save_store.rs": "observe_synthetic_create",
    "src/services/discord/gateway/outbound_messages.rs": "placeholder_attempt",
    "src/services/discord/tui_direct_pending_start/turn_retirement.rs": "confirmation_boundary",
    "src/services/discord/turn_presence/supervisor.rs": "observation::snapshot",
    "src/logging.rs": "n1_observation::sink::initialize",
}
ALLOWED = ("std::collections::", "std::sync::", "std::time::Instant", "serde::Serialize",
           "uuid::Uuid", "super::shadow::ShadowProvider", "super::shadow::SourceBinding",
           "super::shadow::SourceId", "super::shadow::seal::TurnSpan")
CALLERS = {
    "src/services/discord/tui_prompt_relay/synthetic_start_wiring.rs": ("tui_direct_synthetic", 1),
    "src/services/discord/router/message_handler/intake_turn.rs": ("discord_active", 2),
    "src/services/discord/router/intake_gate/queue_effects.rs": ("discord_queued", 1),
    "src/services/discord/router/message_handler/intake_turn/race_loss.rs": ("discord_race", 1),
    "src/services/discord/router/message_handler/intake_turn/placeholder_handoff.rs": ("discord_active", 1),
}
DENIED = re.compile(r"\.await\b|\.(?:lock|send|reserve|flush|join)\s*\(|"
                    r"\bsink::|\b(?:lock|send|reserve|flush|join|sleep|park|spin_loop|get_or_init|blocking_send)\b|"
                    r"\b(?:tracing|println|eprintln|panic|assert|unwrap|expect|spawn|unsafe|include)\b|"
                    r"\b(?:fs|net|process|Http|SharedData|sqlx|reqwest|tmux|save_inflight)\b")


def producer_errors(source):
    # Tests live in a separate file; the producer has no conditional inline escapes.
    code = TOKEN.sub(lambda m: "\n" * m.group(0).count("\n"), source)
    errors = [f"forbidden producer operation: {m.group()}" for m in DENIED.finditer(code)]
    for use in re.findall(r"\buse\s+([^;]+);", code):
        for path, _ in expand(use):
            if not any(path == p or path.startswith(p) for p in ALLOWED):
                errors.append(f"unaudited import: {path}")
    body = re.sub(r"\buse\s+[^;]+;", "", code)
    for path in re.findall(r"\b(?:crate|std|tokio|serde_json|reqwest|sqlx|super)::[\w:]+", body):
        if not any(path.startswith(p) for p in ALLOWED):
            errors.append(f"unaudited path: {path}")
    if ".try_send(" not in code or ".try_lock(" not in code:
        errors.append("bounded nonblocking publication missing")
    if set(re.findall(r"\bmod\s+(\w+)", code)) != {"sink", "tests"}:
        errors.append("unaudited producer child module")
    return errors


def main():
    errors = []
    if not (ROOT / PRODUCER).exists():
        errors.append("observation producer missing")
    else:
        errors.extend(producer_errors((ROOT / PRODUCER).read_text()))
    for path, hook in HOOKS.items():
        code = TOKEN.sub(lambda m: "\n" * m.group(0).count("\n"), (ROOT / path).read_text())
        if hook not in code:
            errors.append(f"{path}: missing {hook}")
    for path, (origin, count) in CALLERS.items():
        code = (ROOT / path).read_text()
        if len(re.findall(rf'origin:\s*"{origin}"', code)) != count:
            errors.append(f"{path}: placeholder context inventory changed")
    outbound = (ROOT / "src/services/discord/gateway/outbound_messages.rs").read_text()
    for name in ("send_intake_placeholder", "edit_intake_placeholder"):
        body = outbound.split(f"async fn {name}(", 1)[1].split("\npub(", 1)[0]
        if "placeholder_attempt" not in body or body.index("placeholder_attempt") > body.index(".await"):
            errors.append(f"{name}: attempt must precede first await")
    print("\n".join(errors) if errors else "N1 observation boundary: PASS")
    return bool(errors)


if __name__ == "__main__":
    sys.exit(main())
