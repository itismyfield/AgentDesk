# Discord / TUI-direct relay unification — design + open-incident root causes

Tracking issue: **#2855**. Related incidents: **#2853**, **#2854**.
Status: design / investigation. No behavioral code change shipped yet (see "What is safe to ship" below).

This document is the code-grounded result of the #2855 discussion request
("how can Discord-input and TUI-direct-input relay share as much logic as
possible"). It records the *current* relay-owner map as it actually exists in
the tree, the single-owner problem, a staged convergence plan, and the
validated root causes of the two live incidents (#2853, #2854) that the
unification is meant to reduce.

---

## 1. Goal (from #2855)

Reduce divergence between Discord-originated turns and TUI-direct input relay
**without resubmitting the prompt to the provider**. At TUI-direct detection
time the prompt has *already* been submitted to the provider, so the path must
never route back through Discord intake (`handle_text_message`). Instead, treat
TUI-direct input as an already-submitted *external* turn and share
relay/finalization from that point on, behind a **single relay owner**.

---

## 2. Current relay-owner map (as in tree)

Four code paths can deliver provider output to Discord today:

| Path | File | Role today |
|------|------|------------|
| `spawn_turn_bridge` | `turn_bridge/mod.rs` | Owns Discord-originated turns: placeholder edits, chunking, finalization, mailbox cleanup. Sets `relay_owner_kind = Watcher` for the live turn. |
| `tmux_watcher` | `tmux_watcher.rs` | Tails the rollout/jsonl, relays output, post-terminal suppression, heartbeat-updates inflight, and has a *direct-send* path for TUI input it detects itself. |
| `SessionBoundDiscordRelaySink` (idle jsonl relay) | `session_relay_sink.rs` | `run_idle_jsonl_relay_loop` tails `matched.expected_rollout_path` for sessions with **no** active inflight (scheduled wakeups / idle background output). |
| `tui_prompt_relay` | `tui_prompt_relay.rs` | Detects TUI-direct prompts (Claude hooks, Codex rollout scan, watcher fallback), holds `external_input_relay_lease` + `prompt_anchor`, and an idle-tail path that sends the response directly. |

Relevant types (`inflight.rs`):

- `TurnSource`: `Managed` (default) · `MonitorTriggered` · `ExternalInput`
  (already exists: "User typed directly into the tmux pane … detected by the
  watcher when rollout activity advances without a Discord-origin inflight").
- `RelayOwnerKind`: `None` (default) · `Watcher` · `StandbyRelay` · `Unknown`.
- `InflightTurnState.rebind_origin: bool`.

Sink ownership gate (`session_relay_sink.rs:40`):

```rust
fn session_bound_discord_relay_can_own_terminal_delivery(inflight, tmux) -> bool {
    // no inflight  -> sink owns (idle/scheduled)
    // inflight present, tmux matches, rebind_origin == true -> sink owns
    // otherwise (e.g. ExternalInput with rebind_origin=false) -> false
}
```

### The single-owner problem

TUI-direct input has **no canonical owner today**:

1. `tui_prompt_relay` detects the prompt and sends an anchor/placeholder
   *without* creating an inflight.
2. `tmux_watcher` later sees the same rollout advance and may treat it as an
   `ExternalInput` turn — but only after the prompt was already submitted.
3. `SessionBoundDiscordRelaySink` will *not* own an `ExternalInput` inflight,
   because its gate only accepts `rebind_origin == true` (line 59). So if an
   `ExternalInput` inflight with `rebind_origin = false` exists, the sink skips
   and the watcher/idle-tail must deliver — which is exactly where duplicate or
   dropped relay comes from.

The recent merged fixes are the first concrete steps toward a single owner:

- **#2836** (`aac8b5b9c`): watcher no longer suppresses post-terminal assistant
  *continuations* — only result-only duplicate envelopes — by passing
  `watcher_batch_contains_assistant_event(&data)` into
  `should_suppress_post_terminal_output_without_inflight`.
- **#2837** (`12ea1ad52` + e2e follow-ups): the idle-tail delivery path now
  clears its own matching inflight via
  `clear_inflight_state_if_matches_tmux_response`, so a delivered TUI-direct
  response no longer leaves stale inflight that blocks the next direct relay.

Both are consistent with the convergence direction below.

---

## 3. Staged convergence plan

### Stage 1 — represent TUI-direct as a synthetic external turn (smallest correct step)

After `tui_prompt_relay` records the prompt anchor:

1. Normalize the detection into a common observation
   `RelayTurnObserved { origin: TuiDirect, provider, channel_id, tmux_session, prompt, start_offset, prompt_anchor }`.
   (Claude hooks, Codex rollout scan, watcher fallback stay specialized
   upstream of this type.)
2. Create a synthetic `InflightTurnState`:
   - `turn_source = ExternalInput`
   - `rebind_origin = false`
   - real `tmux_session_name`, `output_path`, `turn_start_offset`
   - synthetic/headless `user_msg_id`; bind the anchor message as `current_msg_id`
   - **explicit** relay owner (new variant — see below)
   - persist with an **atomic create-new** so a racing watcher detection cannot
     create a second inflight for the same session.

### Stage 2 — single owner = the session-bound sink

Add a dedicated `RelayOwnerKind` for sink ownership (e.g.
`SessionBoundRelay`) and:

- widen `session_bound_discord_relay_can_own_terminal_delivery` to also return
  `true` when `relay_owner_kind == SessionBoundRelay` (in addition to the
  existing no-inflight / `rebind_origin` cases);
- in `spawn_turn_bridge` and `tmux_watcher`, when an inflight already carries
  `relay_owner_kind == SessionBoundRelay`, **yield**: heartbeat-update the
  offset but never claim terminal delivery and never set `Watcher`.

This makes the sink the one owner for `ExternalInput` turns and the bridge the
one owner for `Managed` turns. No provider resubmission occurs because the
prompt was already in the rollout before the inflight was created.

### Stage 3 — bridge-compatible adapter (longer term)

A provider-output tailer emits bridge-compatible `StreamMessage` events so
`spawn_turn_bridge` itself can drive `ExternalInput` turns, sharing chunking,
formatting, error handling, and terminal finalization with Discord-originated
turns. Larger change; not required for Stage 1/2.

---

## 4. Invariants & risks

1. **One owner.** Adding a synthetic inflight without a single owner *creates*
   duplicate relay — the exact failure class #2855 warns about. Stage 1 must
   land together with Stage 2's ownership gate, not before it.
2. **Backward-compatible enum.** A new `RelayOwnerKind` variant must round-trip
   through the tolerant deserializer; older binaries reading the new on-disk
   value must fall back to `Unknown` and treat it as "do not let the watcher
   claim ownership" rather than panicking.
3. **Detection race.** `tui_prompt_relay` and `tmux_watcher` can both try to
   create the inflight; the create-new must be atomic and the loser must adopt,
   not double-create.
4. **Synthetic `user_msg_id`.** It is non-zero but does not refer to a real
   Discord message. Audit reactions / transcript / analytics writers so none
   assume a real message exists for `ExternalInput` rows.
5. **Idle-relay grace.** `run_idle_jsonl_relay_loop` skips delivery while an
   inflight was seen within `IDLE_JSONL_RELAY_RECENT_INFLIGHT_GRACE`; a
   synthetic inflight must be persisted long enough that the sink doesn't buffer
   the response indefinitely.

---

## 5. Related incident root causes (validated against code + disk)

These are the "related current incidents" #2855 lists. Their precise root
causes were confirmed by reading the tree and the live runtime sessions dir.

### #2853 — adk-cc relay "stuck on missing expected rollout session file"

- `expected_rollout_path_for(session)` (cluster `session_matcher.rs:425`) always
  returns `session_temp_path(session, "jsonl")` — the **codex-wrapper** rollout
  convention.
- **claude-TUI sessions never write that `.jsonl`.** On disk the adk-cc session
  files are `…AgentDesk-claude-adk-cc-t<id>.claude-tui.sh`,
  `.claude-tui-settings.json`, `.claude-tui-relay-offset.json`, `.owner`, etc.
  — no `agentdesk-…-AgentDesk-claude-adk-cc.jsonl`. Claude TUI relays by tailing
  the Claude project rollout transcript via the relay-offset file and the
  watcher, **not** the cluster jsonl sink.
- Consequence A (cosmetic): `run_idle_jsonl_relay_loop` `fs::metadata` on the
  absent `.jsonl` fails and `continue`s (session_relay_sink.rs:398). So
  `relay_frames_received = 0` for adk-cc is *expected and harmless* — that path
  was never the claude-TUI relay owner. The cluster monitor showing the session
  "active/recovering with 0 frames" is a **reporting artifact**, not the stall.
- **Confirmed live (2026-05-29):** `GET /api/cluster/sessions` reports channel
  `1479671298497183835` (adk-cc, claude) with
  `expected_rollout_path=…AgentDesk-claude-adk-cc.jsonl` and
  `relay_frames_received=0`, while that `.jsonl` does not exist on disk (only
  `.claude-tui.sh` / `.claude-tui-relay-offset.json` / `.runtime-kind` files do).
  The local watcher relay is unaffected.
- Consequence B (the real symptom): the session is kept "active/recovering" by
  recovery semantics (`restore_inflight` after a completed/`cleared_by_bridge`
  turn), not by the missing jsonl. `restore_inflight_turns` runs at boot
  (`runtime_bootstrap.rs:1862`), emitting `recovery_fired reason=restore_inflight`
  (`recovery_engine.rs:1464`).
- **Note:** the filename host segment is `unknown-host` because `HOSTNAME` /
  `COMPUTERNAME` are unset in the dcserver process (`tmux_common.rs:294`). This
  is *consistent* (all files use `unknown-host`), so it is **not** a host
  mismatch — the file is genuinely absent because claude-TUI never emits it.
- **Proposed fix direction:** make the cluster matcher / health reporting
  runtime-kind aware so it does not expect a codex `.jsonl` rollout for
  `ClaudeTui` sessions (report them as a non-jsonl relay model rather than a
  permanently-absent expected path), and gate `restore_inflight` so a completed
  claude-TUI turn whose codex-style rollout will never exist does not re-arm
  recovery. **Needs live-runtime confirmation of the exact `restore_inflight`
  trigger sequence before merging.**

### #2854 — reused live provider tmux session killed by stale cleanup

- Codex wrapper path (`codex.rs:1963-2023`): `session_usable` requires a live
  pane **and** `resolve_session_temp_path(…, "jsonl")` **and**
  `resolve_session_temp_path(…, "input")` **and** a pipe-input wrapper script.
  When the pane is alive but the local jsonl/input FIFO/`.sh` files are
  unresolvable, `session_usable = false`, so control falls to
  `else if session_exists { kill_session("stale local session cleanup before recreate") }`
  (codex.rs:2008) — killing the very session the higher-level session strategy
  selected for reuse (`resumed=true`, `tmux_alive=true`).
- The same shape exists in `claude.rs` via
  `classify_local_tmux_startup_plan` → `RecreateStaleSession` (claude.rs:1987,
  3313) and in `qwen.rs`.
- **Why the diagnosis agent's naive gate is wrong:** gating the kill behind
  `session_id.is_none()` (or `!resumed`) would *skip the kill but still fall
  through to "create a new tmux session"* with the **same name that still
  exists**, which fails — leaving a live-but-unusable pane and no delivery path.
  A correct fix cannot be a one-line gate.
- **Real tension:** if the input FIFO is genuinely gone, you cannot send a new
  prompt to the existing wrapper by path, so recreation (kill + relaunch with
  `--resume-session-id`, which *does* preserve the provider conversation) is the
  only way to deliver. The user-visible harm is the **repeated** kill/recreate
  churn (observed at 18:23 *and* 18:25), whose deeper cause is the session
  runtime files going missing while the pane stays alive.
- **Confirmed live (2026-05-29, codex-tui E-2, 3 turns, PASS):** codex
  *direct-TUI* kills + recreates the tmux session **every turn**
  (`reason="codex tui local session restart before direct launch"`,
  `codex.rs:683`), even though session-strategy logs `resumed=true`,
  `tmux_alive=true`, `reused_session=true`. The "reuse" refers to the **codex
  conversation** (resumed via `--resume-session-id`), *not* the tmux pane — you
  cannot warm-followup into a live TUI pane, so relaunch is the design. **Relay
  still succeeded on all three turns and zero `stale local session cleanup
  before recreate` events fired.** So for direct-TUI this is **not dropped
  relay** — the harm is watcher/pane churn plus the misleading conflation of
  *conversation-reuse* with *pane-recreate* in the logs.
- The original incident's reason string (`stale local session cleanup before
  recreate`) is the **wrapper** path (`codex.rs:2008`), which only kills when
  `session_usable == false` (jsonl/input FIFO/`.sh` unresolvable for a live
  pane). When the files *are* resolvable the wrapper warm-follows-up with no
  kill.
- **Proposed fix direction:** (a) clarify logging so conversation-reuse and
  pane-recreate are not conflated into an alarming "reused then killed"
  sequence; (b) for the *wrapper* path, find and fix *why* the jsonl/input/`.sh`
  files disappear for a live pane (the only case where the wrapper kills a
  genuinely reusable session). A blanket "don't kill" gate is **wrong**: it
  cannot apply to direct-TUI (relaunch is mandatory) and in the wrapper case
  would skip the kill yet still fall through to recreate a same-named session
  and fail. Net: **no relay-breaking bug observed; the open work is logging
  clarity + the wrapper file-disappearance root cause.**

---

## 6. Acceptance-criteria mapping (#2855)

| Criterion | Status under this plan |
|-----------|------------------------|
| TUI-direct represented by a synthetic external turn/inflight after detection | Stage 1 |
| TUI-direct output uses the same terminal delivery ownership model as Discord turns (or a documented transitional sink) | Stage 2 (transitional sink = `SessionBoundDiscordRelaySink`) |
| TUI-direct prompt never resubmitted through Discord intake | Guaranteed: synthetic inflight is created *after* provider submission; intake is never re-entered |
| Duplicate relay prevented by a single relay owner | Stage 2 ownership gate (`SessionBoundRelay` owner variant) |
| Regression coverage: both Discord and TUI-direct reach finalization through the shared model | Add once Stage 1+2 land together |

---

## 7. What is safe to ship now

- **#2836, #2837:** already merged with passing regression tests
  (`post_terminal_output_without_inflight_is_suppressed`,
  `post_terminal_hard_result_after_committed_turn_requires_direct_input_evidence`,
  `tmux_response_guard_clears_matching_delivered_idle_relay`). Recommend closing.
- **#2853, #2854:** root causes pinned above and **confirmed against the live
  release runtime** (cluster API + codex-tui E-2 PASS + claude-tui E-1 PASS,
  0 `restore_inflight` and 0 `stale local session cleanup` events during normal
  turns). **Neither breaks relay in practice** — both E2E cells passed. The
  remaining work is bounded and low-risk: (#2853) make cluster monitoring /
  health runtime-kind aware so claude-TUI sessions are not flagged as
  "0-frame / stuck" against a `.jsonl` they never write; (#2854) clarify the
  reuse-vs-recreate logging and chase the wrapper file-disappearance root cause.
  These are diagnostic/monitoring fixes, not relay-correctness fixes.
- **#2855:** Stage 1 + Stage 2 should land **together** (Stage 1 alone adds an
  owner nobody yields to → duplicate relay). This is the only safe ordering.
