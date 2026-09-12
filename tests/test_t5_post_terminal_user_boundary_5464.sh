#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PYTHON="${PYTHON:-python3}"

"$PYTHON" - "$ROOT" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
prompt = (root / "src/services/discord/tmux_watcher/prompt_observe.rs").read_text(encoding="utf-8")
prologue = (root / "src/services/discord/tmux_watcher/loop_poll_prologue.rs").read_text(encoding="utf-8")
policy = (root / "src/services/discord/watchers/lifecycle/output_policy.rs").read_text(encoding="utf-8")

marker = "pub(super) fn watcher_batch_contains_assistant_event(data: &[u8]) -> bool {"
start = prompt.find(marker)
assert start >= 0, "missing watcher_batch_contains_assistant_event"
end = prompt.find("\npub(super) fn ", start + len(marker))
assert end >= 0, "cannot bound continuation helper body"
helper = prompt[start:end]

for needle in (
    '\\"type\\":\\"assistant\\"',
    '\\"type\\": \\"assistant\\"',
    '\\"type\\":\\"user\\"',
    '\\"type\\": \\"user\\"',
):
    assert needle in helper, f"continuation helper lost {needle}"

# Result-only duplicate envelopes must remain outside this continuation veto.
assert '\\"type\\":\\"result\\"' not in helper
assert '\\"type\\": \\"result\\"' not in helper

call = "watcher_batch_contains_assistant_event(payload.as_bytes())"
assert call in prologue, "post-terminal prologue no longer consumes continuation helper"
assert "post_terminal_payload_contains_assistant_event" in prologue
assert "should_suppress_post_terminal_output_without_inflight(" in prologue
assert "post_terminal_payload_contains_assistant_event," in prologue, (
    "continuation evidence is no longer passed into suppression policy"
)

assert "assistant_continuation_present: bool" in policy
assert "&& !assistant_continuation_present" in policy, (
    "continuation evidence no longer vetoes post-terminal suppression"
)

print("t5 post-terminal user-boundary veto wiring: OK")
PY
