# Discord Outbound Migration — Coverage Map (#1006 v3 / #1280 / #1436 / #1457)

> Implementation refresh for #1457 / #2368 / #2533 / #2535: v3 delivery now
> covers dispatch outbox, review followups, issue announcements, monitoring
> status, meeting notifications, short manual/DM notifications, gateway
> placeholder sends, routine Discord logs, and dispatch completion summaries
> directly. `OutboundDeduper` has an in-flight reservation primitive for atomic
> lookup/send/record/release behavior, and migrated producers can share one
> process-wide in-memory deduper after building their outbound delivery key.
> Turn-owned delivery paths can pass a `CancelToken` so post-cancel sends,
> fallback retries, split chunks, and headless outbox enqueue are suppressed.
>
> Last refreshed: 2026-05-18 (against #2535 legacy outbound bridge removal).
>
> Companion docs: [`docs/discord-outbound-remaining-producers.md`](../discord-outbound-remaining-producers.md) (#1175 closure), [`docs/source-of-truth.md`](../source-of-truth.md).

This is the single source of truth for "where is each Discord outbound callsite
on the v3 migration path?". The former compatibility facade
`src/services/discord/outbound/legacy.rs` was removed in #2535 after the last
production producers moved to direct v3 envelopes. The outbound API now lives
in `src/services/discord/outbound/{message, policy, decision, result, delivery,
transport}.rs`.

As of #2535, "migrated_v3" means the callsite builds a v3
`DiscordOutboundMessage` and calls `outbound::delivery::deliver_outbound`
directly. Everything else is either a direct serenity call (`channel_id.say` /
`channel_id.send_message` / `channel_id.edit_message` / `ctx.say`) or a custom
HTTP path.

---

## 1. v3 API surface (`src/services/discord/outbound/`)

| Symbol | Path | Status | When to use |
|---|---|---|---|
| `DiscordOutboundMessage` (v3) | `outbound/message.rs:312` | active — multiple production callers | All future sends/edits. Carries `OutboundDeliveryId` (mandatory `correlation_id` + `semantic_event_id`), `OutboundTarget`, `OutboundOperation`, `DiscordOutboundPolicy`. |
| `OutboundTarget::{Channel, Thread, DmUser}` | `outbound/message.rs:82` | active for channel, thread, and DM delivery | Replaces legacy `(channel_id, Option<thread_id>)` pair with sum type. `DmUser` lets v3 resolve/create the DM channel through `DiscordOutboundClient::resolve_dm_channel`. |
| `OutboundOperation::{Send, Edit{message_id}}` | `outbound/message.rs:125` | active through v3 and legacy adapter | Encodes send-vs-edit at the type level (legacy used `Option<edit_message_id>`). |
| `OutboundDedupKey` | `outbound/message.rs:68` | active | Structured key that prevents `("a::b","c")` vs `("a","b::c")` collisions in the legacy delimiter-joined form. |
| `decide_policy(...) -> DiscordOutboundPolicyDecision` | `outbound/decision.rs:133` | active pure planner | Pure function that turns a v3 message + policy into a delivery plan (split / fallback / dedup). Does not perform I/O. |
| `DiscordOutboundPolicy` (v3 in `policy.rs`) | `outbound/policy.rs:57` | active | New policy with named presets, including `dispatch_outbox()`, `review_notification()`, and `preserve_inline_content()`. |
| `DeliveryResult` | `outbound/result.rs:126` | active | Single outbound result source; carries ordered `DeliveredMessage` metadata for success, fallback, and duplicate replay. |
| **v3 delivery** `deliver_outbound<C>(...)` | `outbound/delivery.rs:46` | active | Executes the v3 message/policy/decision/result contract. Accepts an optional `CancelToken`; split delivery records ordered chunk metadata and duplicate replay preserves it. Success paths record the reservation; terminal skip/permanent-failure paths explicitly release it before returning. |
| `DiscordOutboundClient`, `HttpOutboundClient`, `OutboundDeduper` | `outbound/transport.rs` | active | Transport trait, HTTP client, fingerprint helper, and in-memory dedup store with atomic `reserve` / in-flight wait semantics over the lookup -> send -> record/release window. v3 stores serialized `Vec<DeliveredMessage>`. |
| `shared_outbound_deduper()` | `outbound/mod.rs` | active | Process-wide in-memory deduper shared by migrated producers once they have built a structured outbound delivery key. This is only the final in-process duplicate-send guard; durable SQL outbox uniqueness still belongs to the `message_outbox` enqueue/claim path. |

`outbound/mod.rs` re-exports the v3 message/policy/result and shared
transport primitives. New production callsites should import
`outbound::delivery::deliver_outbound` explicitly.

---

## 2. `production_callsite_coverage` map

The five keys come from the static-analysis §1.2 schema. State values:
`direct` = bypasses the outbound layer entirely. `migrated_v3` = uses v3
`outbound/message.rs` types and `outbound::delivery::deliver_outbound`
directly.

| Key | State | v3 ready? | Owner | Source of truth |
|---|---|---|---|---|
| `dispatch_outbox` | `migrated_v3` | yes | dispatch / outbox squad | §3.A |
| `review_notifications` | `migrated_v3` | yes | dispatch / review squad | §3.A |
| `dm_reply` | `mixed` (short text = `migrated_v3`; oversize attachment/chunk shim = compatibility) | partial | health / DM squad | §3.A + §3.B.attachment |
| `placeholder_sends` | `mixed` (gateway/turn_bridge = `migrated_v3`; tmux watcher = `direct`) | partial | tmux / turn-bridge squad | §3.A + §3.B.placeholder |
| `dashboard_discord_proxy` | n/a (read-only) | not applicable | dashboard squad | §3.C |

`mixed` for `placeholder_sends` is the load-bearing finding: any future
guard must scope to "new callsites" rather than "no `.send_message`/`.edit_message`
allowed", because the streaming rollover path in
`services/discord/tmux.rs:4750-4900` legitimately bypasses outbound for
order-preserving multi-message stream continuation (see §4 exclusions).

`mixed` for `dm_reply` is intentionally narrow: sub-2k text now uses
`OutboundTarget::DmUser(UserId)` and the v3 transport resolves the DM channel.
Oversize `/api/discord/send` and `/api/discord/send-dm` payloads still call the existing
attachment/chunk helpers until v3 grows attachment-capable transport.

---

## 3. Callsite inventory

### 3.A Migrated through outbound `deliver_outbound`

These callsites already use the unified delivery engine. Rows marked
`migrated_v3` build a v3 envelope and call `outbound::delivery` directly.

| File:line | Producer | Notes |
|---|---|---|
| `src/server/routes/dispatches/discord_delivery.rs:782` (`post_dispatch_message_to_channel_with_delivery`) | `dispatch_outbox` | **migrated_v3**. Builds a v3 `DiscordOutboundMessage` with `OutboundTarget::Channel`, `DiscordOutboundPolicy::dispatch_outbox()`, and summary-based minimal fallback. Correlation = `dispatch:<id>`, semantic = `dispatch:<id>:notify`. |
| `src/server/routes/dispatches/discord_delivery.rs:3153` (`send_review_result_message_via_http`) | `review_notifications` | **migrated_v3**. Pass/Unknown verdict followups use `DiscordOutboundPolicy::review_notification()`. Correlation = `review:<card_id>`, semantic = `review:<dispatch>:<verdict>:<api_base>`. Per-producer static `review_followup_deduper()`. |
| `src/server/routes/dispatches/outbox.rs:1124` (`post_dispatch_completion_summary`) | final dispatch thread callback | **migrated_v3**. Ensures the dispatch thread is postable, then posts the completion summary through v3 with a per-producer `dispatch_completion_summary_deduper()`. |
| `src/services/issue_announcements.rs:408` (`send_issue_announcement_message`) | issue announcements | **migrated_v3**. Review-style policy, `OutboundOperation::Edit` for edits, and the shared process-wide outbound deduper. |
| `src/services/discord/discord_io.rs:478` (`deliver_channel_message`) | CLI text/DM helper | **migrated_v3**. Used by `--discord-sendmessage` / `--discord-senddm` *after* the DM channel has been resolved. Static `discord_io_deduper`; no caller-supplied delivery id means `without_idempotency()`. |
| `src/services/discord/outbound/manual_delivery.rs` (`deliver_manual_notification`) | manual `/api/discord/send` | **migrated_v3 for sub-2k text**. Over-limit content remains a compatibility shim to `post_text_attachment` (announce) or `deliver_chunked_manual_notification` (notify). Moved from `health/` to `outbound/` in #3038 S1 with `health::` compatibility re-exports preserved. |
| `src/services/discord/outbound/manual_delivery.rs` (`deliver_manual_dm_notification`) | `dm_reply` / `/api/discord/send-dm` | **migrated_v3 for sub-2k text** using `OutboundTarget::DmUser(UserId)`. The v3 transport resolves the DM channel and duplicate delivery returns before a second resolve. Over-limit content keeps the compatibility attachment/chunk path. Moved from `health/` to `outbound/` in #3038 S1. |
| `src/services/discord/gateway.rs:359` (`send_intake_placeholder`) | `placeholder_sends` (intake) | **migrated_v3**. Posts the `"..."` placeholder before a turn via direct v3. Uses `preserve_inline_content().without_idempotency()` to preserve streaming behavior. |
| `src/services/discord/gateway.rs:377` (`edit_outbound_message`) | `placeholder_sends` (edit) | **migrated_v3**. Encodes edit through `OutboundOperation::Edit`. |
| `src/services/discord/gateway.rs:400` (`TurnGateway::{send_message, edit_message}`) | turn-bridge messages/edits | **migrated_v3 transitively via gateway**. Used for handoff, rollover freeze, snapshot, stable update, and terminal edit. |
| `src/services/discord/monitoring_status.rs:115` (`deliver_monitoring_status`) | monitoring status | **migrated_v3**. Status banner send + edit with `preserve_inline_content`; edits use `without_idempotency()`. |
| `src/services/discord/meeting_orchestrator.rs:754, 796` (`meeting_outbound_message` / edit path) | meeting status / cancel / parse-error | **migrated_v3**. Stable meeting dedup metadata plus `OutboundOperation::Edit`. |
| `src/services/routines/discord_log.rs:486, 531` (`deliver_or_update_discord_summary`) | routine Discord summary | **migrated_v3**. Uses direct v3 send/edit and disables semantic dedupe for repeated summary writes. |
| `src/integration_tests/discord_flow/scenarios.rs` (removed in #3035 Phase 1) | integration test harness | Mock-Discord roundtrip for §1.2 validation; legacy-sqlite-only harness deleted. |
| `src/integration_tests/agents_setup_e2e.rs` (removed in #3035 Phase 1) | integration test | Wizard-ready E2E; legacy-sqlite-only harness deleted. |

Total **direct migrated_v3 production families: 11** (`dispatch_outbox`,
`review_notifications`, final dispatch completion summaries, issue
announcements, monitoring status, meeting notifications, routine Discord
summaries, CLI text/DM helper, short manual notifications, short manual DM
notifications, and gateway/turn-bridge placeholder sends/edits).

No production caller uses the removed legacy facade. Verify with
`git grep -n 'deliver_outbound' -- src/services src/bin tests`.

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
| `src/services/discord/outbound/manual_delivery.rs` (`post_text_attachment`) | announce-bot oversize fallback | excluded — attachment fallback that remains a compatibility shim while the v3 outbound text contract truncates first |
| `src/services/discord/router/message_handler.rs:4820` | skill-running banner (file path) | excluded |
| `src/services/discord/commands/skill.rs:273, 339` | skill announce | excluded |

Total attachments-bucket: **7 callsites**.

#### B.3 Long-message streaming (ordered continuation)

The v3 text contract can split one completed payload and returns ordered
`DeliveredMessage` chunk metadata, including duplicate replay. It still does
not model live streaming continuation, placeholder freeze, or offset
bookkeeping. Those callsites remain excluded by #1175 until a dedicated
stream/placeholder lifecycle contract lands.

| File:line | Notes | Triage |
|---|---|---|
| `src/services/discord/formatting.rs:1944, 1960, 1963, 1991, 2046, 2153, 2210` (`send_long_message_raw`, `replace_long_message_raw`) | streaming chunker | **blocker**: needs a future streaming lifecycle variant, not just static split metadata. |
| `src/services/discord/router/message_handler.rs:179, 1542, 1571, 2096, 2103, 3104, 3505, 3513, 3531, 3539, 3558, 3563, 3602` | watchdog / restore / upload notices | mixed — some are short, some forward to `send_long_message_raw`. Triage: **medium priority**; short notices can migrate now, streaming-forwarding paths wait for the lifecycle contract. |

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
button hits the manual outbound API, which is covered under §3.A
(`outbound/manual_delivery.rs`). **No migration needed.**

---

## 4. Recommended migration order

1. **Landed in #1436 — v3 deliver impl + `dispatch_outbox`.**
   `outbound::delivery::deliver_outbound` consumes
   `outbound::message::DiscordOutboundMessage`; `dispatch_outbox` calls v3
   directly.
2. **Landed in #1457 — review followups, dispatch completion summaries,
   gateway/turn-bridge, and short manual/DM text.**
   These callsites now build v3 envelopes directly. `OutboundTarget::DmUser`
   owns the DM-channel resolve step for `/api/discord/send-dm` and duplicate replay uses
   stored delivery metadata before resolving again.
3. **Landed in #2535 — final legacy bridge removal.**
   Issue announcements, monitoring status, meeting notifications, routine
   Discord logs, and the CLI text helper now build v3 envelopes directly.
   `outbound/legacy.rs` was deleted; `transport.rs` owns the shared
   `DiscordOutboundClient`, `HttpOutboundClient`, fingerprint helper, and
   in-memory deduper.
4. **Next — attachment-capable v3 transport.**
   Remove the manual `/api/discord/send` and `/api/discord/send-dm` over-2k compatibility shims
   once v3 can send multipart attachment payloads or explicitly delegate to a
   chunk/attachment transport variant.
5. **Direct-send candidates (low priority).** §B.1
   intake-gate notices, §B.4 tmux lifecycle notices, §B.5 router announces.
   Each is a fixed short string; v3 buys consistent dedup keying.
6. **Out of scope (separate follow-up issues recommended).**
   - §B.3 streaming chunker — needs a v3 stream/placeholder lifecycle variant.
   - §B.4 tmux rollover freeze/post — needs the same contract variant plus a
     "placeholder lifecycle" sub-API.
   - §B.1 ACK / interaction replies — needs interaction-token modeling that
     #1175 explicitly deferred.

---

## 5. Regression coverage

- `src/services/discord/outbound/delivery.rs`:
  `v3_split_duplicate_preserves_ordered_chunk_metadata` verifies static v3
  split delivery preserves chunk order and duplicate replay metadata.
- `src/services/discord/outbound/delivery.rs`:
  `v3_dedup_reservation_suppresses_concurrent_retry_send` and
  `v3_dedup_reservation_retries_after_inflight_owner_failure` verify the
  in-flight reservation primitive suppresses concurrent duplicate sends while
  allowing a waiting retry to claim the key if the first owner fails before
  recording a delivery.
- `src/services/discord/outbound/delivery.rs`:
  `v3_dm_user_target_resolves_before_posting` verifies `OutboundTarget::DmUser`
  resolves before first post and duplicate replay does not resolve the DM
  channel again.
- `src/services/discord/turn_bridge/mod.rs`:
  `final_completion_delivery_stays_blocked_until_terminal_message_commits`
  verifies final completion delivery remains blocked until the terminal Discord
  message commit has happened.
- `src/services/discord/outbound/manual_delivery.rs`:
  `manual_dm_notification_uses_v3_dm_target_and_dedupes_before_resolve`
  verifies `/api/discord/send-dm` short text uses v3 DM target semantics and preserves
  the manual duplicate response contract.
- `src/services/discord/outbound/manual_delivery.rs`:
  `api_send_rejects_user_supplied_voice_delivery_id_namespace` verifies manual
  `/api/discord/send` callers cannot forge the reserved `voice:` correlation
  namespace used by voice announce delivery ids.

## 6. Guardrail proposal (DoD #4)

To stop new callsites slipping back to direct sends, the recommended belt +
braces:

1. **Module doc gate (updated in #2535).** `src/services/discord/outbound/mod.rs`
   now states that the legacy bridge is gone and production callsites should
   use v3 envelopes via `outbound::delivery::deliver_outbound`.
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
     - §B.3 — long-message streaming chunker / placeholder lifecycle
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

## 7. Validation commands

Reproduce the inventory locally:

```bash
# Total direct-send/edit footprint inside discord services + routes
rg -n '\.send_message\(|\.say\(|\.edit_message\(' src/services/discord src/server/routes

# Outbound-layer callsites
rg -n 'deliver_outbound|DiscordOutboundMessage' src --type rust

# v3 direct imports (direct migrated_v3 callsites should be present after #1457)
rg -n 'use crate::services::discord::outbound::(message|policy|decision|result|delivery)::' src
```

Expected counts as of the #1457 refresh:

- direct sends in `src/services/discord/**`: **133** matches across **26**
  files (this includes the explicitly-excluded ACK/attachment/streaming buckets).
- direct sends in `src/server/routes/**`: **0**.
- direct migrated_v3 production families: **6** (`dispatch_outbox`,
  `review_notifications`, final dispatch completion summaries, short manual
  notifications, short manual DM notifications, gateway/turn-bridge
  placeholder sends/edits).
- legacy-facade production callsites and explicit compatibility shims still
  remain; migrate them in the order above.
