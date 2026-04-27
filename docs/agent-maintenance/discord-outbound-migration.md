# Discord Outbound Migration — Coverage Map (#1006 v3 / #1280)

> Inventory-only document. No source changes were made for this audit.
>
> Last refreshed: 2026-04-27 (against `main` @ `4a3c28e5`).
>
> Companion docs: [`docs/discord-outbound-remaining-producers.md`](../discord-outbound-remaining-producers.md) (#1175 closure), [`docs/source-of-truth.md`](../source-of-truth.md).

This is the single source of truth for "where is each Discord outbound callsite
on the v3 migration path?". It exists because the static analysis report
(2026-04-27 §1.2/§4) found that two parallel APIs coexist inside
`src/services/discord/outbound/`:

- **legacy** — `src/services/discord/outbound/legacy.rs` (963 lines), exporting
  the v2 surface used by every production callsite today.
- **v3 (types only, slice 1.0)** — `src/services/discord/outbound/{message,
  policy, decision, result}.rs` (~1.4 k lines combined), with
  unit-test coverage but **zero production callsites**.

The v3 deliver implementation (slice 1.1) and the outbox rewire (slice 1.2)
are not landed yet. Until they are, "migrated" in this document means
**routes through the legacy `deliver_outbound`**, which is the v2 unified API
that owns length safety + idempotency. Everything else is either a direct
serenity call (`channel_id.say` / `channel_id.send_message` /
`channel_id.edit_message` / `ctx.say`) or a custom HTTP path.

---

## 1. v3 API surface (`src/services/discord/outbound/`)

| Symbol | Path | Status | When to use |
|---|---|---|---|
| `DiscordOutboundMessage` (v3) | `outbound/message.rs:312` | **types only** — no callers | All future sends/edits. Carries `OutboundDeliveryId` (mandatory `correlation_id` + `semantic_event_id`), `OutboundTarget`, `OutboundOperation`, `DiscordOutboundPolicy`. |
| `OutboundTarget::{Channel, Thread, DmUser}` | `outbound/message.rs:82` | types only | Replaces legacy `(channel_id, Option<thread_id>)` pair with sum type. `DmUser` removes the manual `create_dm_channel` step. |
| `OutboundOperation::{Send, Edit{message_id}}` | `outbound/message.rs:125` | types only | Encodes send-vs-edit at the type level (legacy used `Option<edit_message_id>`). |
| `OutboundDedupKey` | `outbound/message.rs:67` | types only | Structured key that prevents `("a::b","c")` vs `("a","b::c")` collisions in the legacy delimiter-joined form. |
| `decide_policy(...) -> DiscordOutboundPolicyDecision` | `outbound/decision.rs:129` | pure planner, types only | Pure function that turns a v3 message + policy into a delivery plan (split / fallback / dedup). Does not perform I/O. |
| `DiscordOutboundPolicy` (v3 in `policy.rs`) | `outbound/policy.rs` | types only | New policy with named presets. |
| `DeliveryAttemptResult` | `outbound/result.rs` | types only | Successor of legacy `DeliveryResult`; richer error/fallback tagging. |
| **Legacy bridge** `DiscordOutboundMessage` (v2) | `outbound/legacy.rs:153` | **all production traffic** | Two-arg constructor `(channel_id, content)` + builder fluent. Everything below routes through this. |
| **Legacy bridge** `deliver_outbound<C>(...)` | `outbound/legacy.rs:352` | **all production traffic** | Owns length truncation, minimal-fallback retry, in-process dedup via `OutboundDeduper`. |
| `OutboundDeduper` | `outbound/legacy.rs:313` | active | In-memory `HashMap<key, message_id>` dedup; one static instance per producer (gateway / discord_io / dispatch / review / health). |

Legacy re-exports live in `outbound/mod.rs:34-40`; v3 modules are
`pub(crate)` and addressable only via their submodule paths so they cannot
shadow the legacy names.

---

## 2. `production_callsite_coverage` map

The five keys come from the static-analysis §1.2 schema. State values:
`migrated_v2` = uses legacy `deliver_outbound`. `direct` = bypasses the
outbound layer entirely. `migrated_v3` = uses v3 `outbound/message.rs` types
(currently always 0).

| Key | State | v3 ready? | Owner | Source of truth |
|---|---|---|---|---|
| `dispatch_outbox` | `migrated_v2` | no (waiting on slice 1.2) | dispatch / outbox squad | §3.A |
| `review_notifications` | `migrated_v2` | no | dispatch / review squad | §3.A |
| `dm_reply` | `migrated_v2` (manual outbound path) | no | health / DM squad | §3.A |
| `placeholder_sends` | `mixed` (gateway = `migrated_v2`; tmux watcher = `direct`; turn_bridge = `migrated_v2` via gateway) | no | tmux / turn-bridge squad | §3.A + §3.B.placeholder |
| `dashboard_discord_proxy` | n/a (read-only) | not applicable | dashboard squad | §3.C |

`mixed` for `placeholder_sends` is the load-bearing finding: any future
guard must scope to "new callsites" rather than "no `.send_message`/`.edit_message`
allowed", because the streaming rollover path in
`services/discord/tmux.rs:4750-4900` legitimately bypasses outbound for
order-preserving multi-message stream continuation (see §4 exclusions).

---

## 3. Callsite inventory

### 3.A Migrated through legacy `deliver_outbound` (v2)

These callsites already use the unified API. They will move to v3 when
slice 1.1 lands the v3 deliver impl + a thin compat shim.

| File:line | Producer | Notes |
|---|---|---|
| `src/server/routes/dispatches/discord_delivery.rs:775` (`post_dispatch_message_via_outbound`) | `dispatch_outbox` | Notify path. Builds `DiscordOutboundMessage::new(channel_id, message)` + `DiscordOutboundPolicy::dispatch_outbox(minimal_message)`. Correlation = `dispatch:<id>`, semantic = `dispatch:<id>:notified`. |
| `src/server/routes/dispatches/discord_delivery.rs:3893` (`post_review_followup`) | `review_notifications` | Pass/Unknown verdict followups. Correlation = `review:<card_id>`, semantic = `review:<dispatch>:<verdict>:<api_base>`. Per-producer static `review_followup_deduper()`. |
| `src/services/discord/discord_io.rs:461` (`deliver_channel_message`) | CLI text/DM helper | Used by `--discord-sendmessage` / `--discord-senddm` *after* the DM channel has been resolved. Static `discord_io_deduper`. |
| `src/services/discord/health.rs:2318` (`deliver_manual_notification`) | manual `/api/send` + `dm_reply` | Wraps legacy `deliver_outbound` for sub-2 k-char content; over-limit content falls through to `post_text_attachment` (announce) or `deliver_chunked_manual_notification` (notify). DM path at `health.rs:2688` resolves `UserId::create_dm_channel` then re-enters this function. |
| `src/services/discord/gateway.rs:285` (`send_intake_placeholder`) | `placeholder_sends` (intake) | Posts the `"..."` placeholder before a turn. Static `gateway_deduper`. |
| `src/services/discord/gateway.rs:306` (`edit_outbound_message`) | `placeholder_sends` (edit) | Wraps `with_edit_message_id`. |
| `src/services/discord/gateway.rs:335` (`TurnGateway::send_message`) | turn-bridge messages | Used by `turn_bridge/mod.rs:1525` for rollover continuations. |
| `src/services/discord/gateway.rs:354` (`TurnGateway::edit_message`) | turn-bridge edits | Used by `turn_bridge/mod.rs:1164, 1522, 1556, 1590, 1827, 1976, 2243` (handoff, rollover freeze, snapshot, stable update, terminal edit). All eight turn-bridge callsites are migrated transitively via the gateway. |
| `src/services/discord/monitoring_status.rs:116, 118` (`update_status_banner`) | monitoring status | Status banner send + edit. `preserve_inline_content` policy. (#1175) |
| `src/services/discord/meeting_orchestrator.rs:738, 757` (`deliver_meeting_notification`) | meeting status / cancel / parse-error | (#1175) |
| `src/services/discord/outbound/legacy.rs:*` tests | n/a | 12 test callsites, ignored for coverage. |
| `src/integration_tests/discord_flow/scenarios.rs:44, 55` | integration test harness | Mock-Discord roundtrip for §1.2 validation. |
| `src/integration_tests/agents_setup_e2e.rs:259` | integration test | Wizard-ready E2E. |

Total **migrated_v2 production callsites: 11** (excluding tests). Verify with
`rg -n 'deliver_outbound\(' src --type rust | rg -v 'integration_tests|outbound/legacy.rs'`.

### 3.B Direct sends (bypass outbound)

These callsites use serenity's `channel_id.{say, send_message, edit_message}`
directly. Each gets a category, current owner, and a "blocker / design
question / low priority" tag for migration triage.

#### B.1 Slash command ACK / interaction replies (`ctx.say` / `ctx.send`)

These are **explicitly excluded** by the #1175 contract. They are
ACK/token-bound, often ephemeral, and the outbound contract does not model
interaction tokens or ephemeral visibility.

| File:line | Notes | Triage |
|---|---|---|
| `src/services/discord/commands/restart.rs:185, 194, 223` | `/restart` ACK | excluded — interaction reply |
| `src/services/discord/commands/receipt.rs:57, 138` | `/receipt` ACK | excluded |
| `src/services/discord/commands/fast_mode.rs:83, 109` | `/fast` ACK | excluded |
| `src/services/discord/commands/help.rs:77, 83` | `/help` ACK | excluded |
| `src/services/discord/commands/mod.rs:93` | shared command reply helper | excluded |
| `src/services/discord/commands/skill.rs:141, 158, 161, 187, 206, 209, 247, 314, 321, 330` | `/skill` family ACK | excluded |
| `src/services/discord/commands/control.rs:452, 456, 478, 510, 532, 550, 559, 564` | `/stop`, `/clear`, `/down` ACK | excluded |
| `src/services/discord/commands/diagnostics.rs:759, 765, 780, 833, 839, 854, 883, 893, 961` | `/sessions`, `/deletesession`, `/debug` ACK | excluded |
| `src/services/discord/commands/session.rs:72, 114, 125, 153, 362, 365` | `/start` ACK | excluded |
| `src/services/discord/commands/config.rs:756, 762, 767, 801, 824, 837, 845, 870, 885, 892, 917, 944` | `/allowed`, `/adduser`, `/removeuser`, `/public` ACK | excluded |
| `src/services/discord/commands/meeting_cmd.rs:33, 42, 83, 97` | `/meeting` ACK | excluded |
| `src/services/discord/commands/text_commands.rs:1239` | text-command running banner | excluded |
| `src/services/discord/commands/model_picker.rs:60, 132` | `/model` picker | excluded |
| `src/services/discord/router/intake_gate.rs:258` | reaction-control reply | excluded — reply with reference inside slash interaction |
| `src/services/discord/router/intake_gate.rs:960, 1071` | "duplicate-queue" + "drain-pending" notices | candidate — these are bot notifications, not interaction tokens. Triage: **low priority**, very short fixed strings, no length risk. |

Total commands-bucket: **62 callsites**, all explicitly excluded.

#### B.2 File / attachment uploads

The current outbound contract does not model attachment payloads (only text
fallback policy). Excluded by #1175.

| File:line | Notes | Triage |
|---|---|---|
| `src/services/discord/discord_io.rs:390` (`send_file_to_channel`) | CLI `--discord-sendfile` | excluded — attachment is the payload |
| `src/services/discord/router/message_handler.rs:4598` | text-command file output | excluded |
| `src/services/discord/commands/text_commands.rs:975` | text-command attached output | excluded |
| `src/services/discord/health.rs:2235` (`post_text_attachment`) | announce-bot oversize fallback | excluded — attachment fallback that lives below the outbound layer because outbound truncates first |
| `src/services/discord/router/message_handler.rs:4820` | skill-running banner (file path) | excluded |
| `src/services/discord/commands/skill.rs:273, 339` | skill announce | excluded |

Total attachments-bucket: **7 callsites**.

#### B.3 Long-message streaming (ordered continuation)

The outbound contract models a single send/edit, not ordered chunk
continuation. Splitting these would corrupt stream offset bookkeeping.
Excluded by #1175.

| File:line | Notes | Triage |
|---|---|---|
| `src/services/discord/formatting.rs:1944, 1960, 1963, 1991, 2046, 2153, 2210` (`send_long_message_raw`, `replace_long_message_raw`) | streaming chunker | **blocker**: needs a future contract variant that returns ordered chunk metadata. Out of scope for #1280. |
| `src/services/discord/router/message_handler.rs:179, 1542, 1571, 2096, 2103, 3104, 3505, 3513, 3531, 3539, 3558, 3563, 3602` | watchdog / restore / upload notices | mixed — some are short, some forward to `send_long_message_raw`. Triage: **medium priority**; candidate for v3 once the chunker contract lands. |

Total chunker-bucket: **20 callsites**.

#### B.4 tmux watcher placeholder (rollover + lifecycle)

This is the **load-bearing exception**. The watcher streams provider output
into a single Discord placeholder message, freezes it on rollover, and posts
a fresh placeholder for the continuation. Order-preservation across multiple
messages is the invariant; outbound-layer dedup would corrupt it.

| File:line | Notes | Triage |
|---|---|---|
| `src/services/discord/tmux.rs:1101` (`edit_placeholder_with_operation`) | placeholder bookkeeping | **blocker** — order-preserving stream continuation |
| `src/services/discord/tmux.rs:4752, 4762, 4793, 4834, 4843` | rollover freeze + new placeholder + fallback `say` | blocker (rollover order) |
| `src/services/discord/tmux.rs:4888, 4896` | ready-for-input failure notice | candidate — short fixed string. **low priority**, design question: do we want this dedup-keyed by tmux session? |
| `src/services/discord/tmux.rs:4970, 4974` | context-limit notice | candidate — short fixed string. **low priority**, same design question. |
| `src/services/discord/tmux.rs:5048, 5052` | auth-error notice | candidate — same shape. **low priority**. |
| `src/services/discord/tmux.rs:5168, 5176` | provider-overload retry notice | candidate — same shape. **low priority**. |
| `src/services/discord/tmux.rs:5442` | stale-session recovery edit | candidate — single edit. **medium priority**. |

Total tmux-bucket: **15 callsites**, of which ~9 are short lifecycle notices
that could migrate, and the rollover/freeze path (~6) is a hard blocker.

#### B.5 Restore / watchdog / upload announcements (router)

These are notifications that *could* migrate but currently sit on
`channel_id.say(...)` for historical reasons.

| File:line | Notes | Triage |
|---|---|---|
| `src/services/discord/router/message_handler.rs:179` (`announce_alert`) | alert-channel announce | candidate — **medium priority**; passes via dedicated alert channel, no thread routing |
| `src/services/discord/router/message_handler.rs:1542, 1571` (`send_session_restore_notice`) | restore-bot announce + provider fallback | candidate — **medium priority**; would benefit from dedup so duplicate restores in retry loops don't double-post |
| `src/services/discord/router/message_handler.rs:3104` (watchdog timeout notice) | turn-watchdog timeout | candidate — **medium priority**; fixed-shape notice |

These overlap with §B.3 in some places — counted once here.

### 3.C `dashboard_discord_proxy` (read-only)

| File:line | Notes |
|---|---|
| `src/server/routes/discord.rs:88` (`channel_messages`) | GET-only proxy to `discord.com/api/v10/channels/{id}/messages` |
| `src/server/routes/discord.rs:135` (`channel_info`) | GET-only proxy to `/channels/{id}` |
| `src/server/routes/discord.rs:16` (`list_bindings`) | DB-only, no Discord call |
| `src/server/routes/messages.rs:61` (`create_message`) | INSERT into `messages` (postgres), not a Discord send |

Dashboard never sends to Discord through these routes; the dashboard's send
button hits `/api/discord/send` which is the manual outbound path covered
under §3.A (`health.rs:2318`). **No migration needed.**

---

## 4. Recommended migration order (slice 1.1 / 1.2 onwards)

1. **Slice 1.1 — v3 deliver impl + legacy compat shim.**
   Add a v3 `deliver_outbound` that consumes
   `outbound::message::DiscordOutboundMessage` and provide a `From<v2> for v3`
   bridge so `outbound/legacy.rs:352` becomes a thin adapter. No callsite
   changes yet.
2. **Slice 1.2.a — `dispatch_outbox`.** Lowest risk: only one callsite
   (`server/routes/dispatches/discord_delivery.rs:775`), already paired with
   correlation/semantic ids that map 1:1 onto `OutboundDeliveryId`.
3. **Slice 1.2.b — `review_notifications`.** Same shape as dispatch_outbox,
   different policy preset. One callsite
   (`server/routes/dispatches/discord_delivery.rs:3893`).
4. **Slice 1.2.c — gateway / turn-bridge.** Migrate
   `services/discord/gateway.rs:285, 306, 335, 354`. Eight transitive turn-bridge
   callers ride along.
5. **Slice 1.2.d — manual outbound (`health.rs` `/api/send` + `/api/senddm`).**
   Includes the DM-channel resolve step. The `OutboundTarget::DmUser(UserId)`
   v3 variant exists specifically for this; the deliver impl will own the
   `create_dm_channel` step.
6. **Slice 1.2.e — `discord_io.rs` CLI helpers.** Trivial after 1.2.d.
7. **Slice 1.2.f — monitoring_status + meeting_orchestrator.** Already
   migrated to legacy; mechanical port.
8. **Slice 1.3 — direct-send candidates (low priority).** §B.1
   intake-gate notices, §B.4 tmux lifecycle notices, §B.5 router announces.
   Each is a fixed short string; v3 buys consistent dedup keying.
9. **Out of scope (separate follow-up issues recommended).**
   - §B.3 streaming chunker — needs a v3 contract variant for ordered chunks.
   - §B.4 tmux rollover freeze/post — needs the same contract variant plus a
     "placeholder lifecycle" sub-API.
   - §B.1 ACK / interaction replies — needs interaction-token modeling that
     #1175 explicitly deferred.

---

## 5. Guardrail proposal (DoD #4)

To stop new callsites slipping back to direct sends, the recommended belt +
braces:

1. **Module doc gate (immediate, low cost).** Add a "no new callsites"
   doc-comment to `src/services/discord/outbound/legacy.rs` head + a
   "use this surface for new sends" pointer to `outbound/message.rs`.
   *Not landed in this PR — flagged for the slice 1.1 author.*
2. **`audit_maintainability.py` hard gate (medium cost).** Extend the audit
   script (#1282 follow-up) so a new `\.send_message\(|\.say\(|\.edit_message\(`
   inside `src/services/discord/` fails CI unless the callsite lives in a
   **hard-exclusion** path. The allowlist MUST consume only the
   `permanent_exclusion` set below — categories tagged `migration_candidate`
   are explicitly NOT exempt because they're still subject to follow-up
   migration, and mixing the two would silently allow exactly the
   callsites the migration is supposed to clean up (codex P2 on #1286).

   - `permanent_exclusion` (allowlist source-of-truth — never migrated):
     - §B.1 — poise slash-command ACK / interaction replies (framework
       contract; #1175 exclusion)
     - §B.2 — file / attachment uploads (multipart path is out of scope
       for the v3 text-message surface)
     - §B.3 — long-message streaming chunker, ordered-chunk continuation
     - §B.4-rollover — tmux rollover freeze/post sequence dependent on
       chunker ordering

   - `migration_candidate` (tracked here; allowlist must NOT reference —
     when a candidate lands a v3 migration the audit observes the
     callsite disappear with no allowlist change):
     - §B.1-intake-gate — short fixed-string reaction replies in
       `intake_gate.rs` that don't need interaction-token semantics
     - §B.4-lifecycle — short tmux lifecycle status notices outside the
       rollover sequence
     - §B.5 — router restore/watchdog announces

   When something graduates out of `migration_candidate` to migrated, the
   matching row in §3 flips to `migrated` and no allowlist change is
   needed. Movement out of `permanent_exclusion` (rare — implies the v3
   surface gained an interaction or multipart variant) updates this doc
   and the allowlist together.
3. **Refresh cadence.** Re-run §3 inventory every release-cut and on any PR
   that touches `src/services/discord/outbound/**` or adds files under
   `src/services/discord/`.

---

## 6. Validation commands

Reproduce the inventory locally:

```bash
# Total direct-send/edit footprint inside discord services + routes
rg -n '\.send_message\(|\.say\(|\.edit_message\(' src/services/discord src/server/routes

# Migrated v2 callsites
rg -n 'deliver_outbound|DiscordOutboundMessage' src --type rust

# v3 surface (should remain test-only until slice 1.1 lands)
rg -n 'use crate::services::discord::outbound::(message|policy|decision|result)::' src
```

Expected counts as of `4a3c28e5`:

- direct sends in `src/services/discord/**`: **128** matches across **24**
  files (this includes the explicitly-excluded ACK/attachment/streaming buckets).
- direct sends in `src/server/routes/**`: **0**.
- migrated v2 production callsites (excluding tests): **13**.
- v3 production callsites: **0**.
