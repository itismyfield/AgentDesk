# Discord Outbound Migration — Coverage Map (#1006 v3 / #1280 / #1436 / #1457)

Last refreshed: 2026-06-16 (against `main` @ `8ec7336e32eb6ef89e1143fab2543f2fc644ebac`)

> #3664 outbound bot-selection note: the outbox drain (`src/server/mod.rs`)
> now resolves the delivery bot via `message_outbox::delivery_bot_for_target_session`.
> For a private (DM) session — tmux name `AgentDesk-<provider>-dm-<digits>` with a
> `channel:<id>` target — outbound delivery routes through the **provider bot**
> (e.g. claude), because the announce/notify bots are not participants in a user
> DM and Discord rejects their send (`Missing Access` → 403). Guild channels keep
> their configured announce/notify bot. The `send_gate` `dm_default_agent`
> authorization still gates the provider-bot DM path (no #2047 weakening).

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
> Last refreshed: 2026-06-20 (against #3593 codex HIGH — restores the generation-checked, read-authority-flag-INDEPENDENT durable-frontier-end reader `delivered_frontier_end_current_generation` in `outbound/delivery_record.rs` and fuses it via `max` with the in-memory committed offset in `committed_floor_for_resend_dedup`. The synthetic-resume / placeholder re-send dedup gate in `tmux_watcher.rs` reads this fused floor so it stays a TRUE superset of the #3520 new-message guard even under `AGENTDESK_DELIVERY_RECORD_AUTHORITY=OFF`, the default, where `effective_committed_offset` returns only the in-memory offset — which a restart/synthetic-resume resets to 0 while the durable current-generation frontier still holds the delivered high-watermark. `max` only raises the floor and the reader is current-generation-only per the #1270 guard, stale-gen → 0, so a genuinely-new post-reset answer is never over-suppressed. Earlier under #3520 the watcher-direct fallback gated `has_direct_terminal_response` on the same reader to suppress re-mirroring already-delivered text as a duplicate message at the same prompt anchor. Outbound delivery semantics otherwise unchanged).
>
> Last refreshed: 2026-06-20 (against #3610 codex review — M4 invariant gate on the bridge long-chunk arm. The durable-frontier helper `record_long_chunk_terminal_delivery` in `outbound/delivery_record.rs` is now documented to fire ONLY when the caller's `lease.commit_and_advance(.., Delivered)` returned `true`; the `turn_bridge/mod.rs` long-chunk `Ok` arm captures that bool and gates the helper call on it. A non-Leased / identity-mismatch / reclaimed cell makes `commit_and_advance` return `false` WITHOUT advancing the in-memory `confirmed_end_offset`, so recording `delivered_frontier.range = end` in that case would leave the durable frontier END ahead of the authority — the exact M4 violation. No signature / callsite-map change; the helper's coverage row and outbound delivery semantics are otherwise unchanged).
>
> Last refreshed: 2026-06-20 (#3610 PR-1d — watcher legacy long-chunk arm now also records a durable terminal delivery. The same `record_long_chunk_terminal_delivery` helper in `outbound/delivery_record.rs` is now reached from the watcher long-chunk fallback (`tmux_watcher::terminal_send`, the `watcher_should_send_ordered_new_chunks_for_terminal_fallback` path, later controller-routed by #3998 S1-d), which is the arm that watcher-owned sessions — `relay_owner_kind=watcher`, the production majority — actually take for long messages. Same M4 gate: the call lives in that arm's `if committed && Delivered` success branch, so it fires only when the in-memory advance succeeded. Anchor is the last-chunk `message_ids.last()` msg_id (placeholders dropped → range-only), same-channel (frontier key = delivery = `channel_id`), range = (`watcher_lease_start`, `watcher_lease_end`); dedup byte-0. Recording logic mirrors the bridge sibling; `tmux_watcher.rs` only wires the anchor through. No outbound API / callsite-map change).
>
> Last refreshed: 2026-06-20 (#3610 PR-2 + codex r2 — adds a READ-ONLY recovery anchor reader, now housed in `outbound/delivery_frontier_probe.rs`: `current_generation_delivered_anchor(provider, channel, tmux_session_name) -> Option<CurrentGenerationAnchor>` (and its path-based core `…_at`), returning the durable `delivered_frontier`'s `(panel_msg_id, panel_channel_id, range)` ONLY when (a) the #1270 generation gate passes and (b) the anchor pair is fully populated/non-zero. It is the structural stale-anchor guard for the default-OFF `recovery_paths/restart.rs::try_recover_anchor_repost` fallback (#3607 "committed-then-gone" repost); it NEVER writes and resolves NO new offset, so it adds no new dedup writer. codex r2 follow-ups touch ONLY non-outbound files: (Issue-2 storm guard) the committed-branch dispose in `recovery_engine.rs` passes `tmux_alive = false` so a repeatedly-transient send-new is budget-bounded rather than pane-preserved forever; (Issue-1) `restart.rs` documents that this reader is keyed by `state.channel_id` (delivery channel) so the bridge-reused-watcher CROSS-channel case (owner ≠ delivery; frontier file keyed by `watcher_owner_channel_id`) is a known coverage gap — a missed repost, never a mis-repost — pending owner-channel persistence on the inflight row. No change to the read/write callsite map or outbound delivery semantics).
>
> Last refreshed: 2026-06-26 (#3709/#3710 — TUI-direct bridge long terminal relay now calls the explicit long-message-with-rollback gateway method instead of falling through the single-message trait default. The trait default itself now splits and rolls back partial chunks for custom test gateways. `turn_bridge/mod.rs` completion logging remains blocked until terminal delivery is committed, so a placeholder-only `RejectOverLimit` failure can no longer be logged as a completed relay. No v3 outbound API shape or durable delivery-record writer changed).
>
> Last refreshed: 2026-06-28 (#3746 — release health now reports the delivery-record rollout mode as `delivery_record_rollout`: shadow flag state, authority flag state, effective dedup authority, same-turn backward-write enforcement mode, and configuration warning count. This is read-only visibility; `AGENTDESK_DELIVERY_RECORD_AUTHORITY=OFF` still means `effective_committed_offset` uses the in-memory committed offset and the same-turn backward-write guard remains observe-only. Synthetic-resume paths that intentionally read the current-generation durable frontier flag-independently are unchanged).
>
> Last refreshed: 2026-06-28 (#3751 — `InflightTurnState` still carries `watcher_owner_channel_id` for same-binary fast reads, but the mixed-binary-safe source of restart recovery is the separate `discord_delivery_owner_context` sidecar stored under the delivery channel. `recovery_paths/restart.rs::try_recover_anchor_repost` first resolves that sidecar delivery-channel → watcher-owner mapping, falls back to the inflight field / `state.channel_id` for legacy rows, and rejects shared-owner stale anchors whose recorded `panel_channel_id` does not match the recovered delivery channel or whose range starts at/after the row's `last_offset`. `logical_channel_id` remains a thread-parent axis and is explicitly not used for delivery-record lookup. The repost target still comes from recorded `panel_channel_id`; stale-generation and `MessageGone` guards are unchanged).
>
> Last refreshed: 2026-06-29 (#3807 — manual notification over-limit chunk delivery now wraps `split_message` output with compact continuation context markers, matching the other long-message split paths. This is still the existing compatibility chunk shim in `outbound/manual_delivery.rs`; no v3 outbound API shape, dedup writer, attachment policy, or direct-send inventory category changed).
> Last refreshed: 2026-06-29 (#3872 — long-message split paths removed visible `[n/m]` continuation markers while preserving ordered chunk delivery and existing rollback/delivery-confirmation semantics. No v3 outbound API shape, attachment policy, or direct-send inventory category changed).
>
> Last refreshed: 2026-06-29 (#3809 — idle-recap relay diagnostics add a READ-ONLY current-generation delivered-frontier report path in `outbound/delivery_frontier_probe.rs`. `outbound/delivery_record.rs` only widens existing internal read primitives (`delivery_record_path`, `read_record_at`, generation guard, generation mtime) so the probe can reuse the same trusted durable frontier without adding a writer, retry, cleanup, delivery API, or direct-send callsite. The production callsite coverage map is unchanged).
>
> Last refreshed: 2026-07-01 (#3794 — turn-output controller rollout closeout. All six owner cutovers (A2b sink, A3 standby, A4 watcher, A5 turn_bridge, A6a recovery, A6b tui_prompt_relay) are wired behind their `AGENTDESK_*_CONTROLLER` flags; every flag is compiled default OFF (byte-identical legacy → per-owner rollback lever), while the release `~/.adk/release/config/launchd.env` forces all six `=1` so the controller path is effective on every release node. Release health now reports the effective per-node rollout as `turn_output_controller_rollout` in `outbound/turn_output_controller_rollout_health.rs` (read-only, side-effect-free env snapshot mirroring the #3746 `delivery_record_rollout` pattern; preserved on the public `/api/health` allowlist): per-owner `enabled`, `enabled_count`, and `effective_authority` of `controller`/`mixed`/`legacy`. NO controller path, default, or flip changed in #3794; see the 2026-07-03 note for the later long-chunk migration and retained exclusions. Additive observability only).
>
> Last refreshed: 2026-07-03 (#3998 S1-d — A4 watcher anchored full-body long chunks and A5 turn_bridge anchored long chunks now route through the turn-output controller behind their existing owner flags. `OutputPlan::SendNewChunks` gained `delete_anchor`; controller transport sends rollback-aware chunks first, then best-effort deletes the active anchor, and returns chunk metadata for owner cleanup/durable-anchor records. The retained turn-output exclusions are now exactly five: empty body, `NoRange` deliver-without-advance (#4048), headless enqueue (no direct Discord POST), watcher no-placeholder new-message fresh-send (`placeholder_msg_id == None`; anchor-less fresh-send is not yet a controller verb, same class as A6a's `None`-placeholder fresh-send and re-evaluated with the #3998 flip/legacy-retirement phase), and TUI completion gate (#4047). Compiled defaults and release env remain unchanged).
>
> Last refreshed: 2026-07-03 (#3998 S1-e — the remaining A2b/A3/A6a retained exclusions are enumerated in §8.1.1 with code conditions, rationale, blockers, and pin tests. Together with #4053's A4/A5 inventory and #4054's A6a D1 idempotency fix, the Phase-B excluded-arm GO condition is satisfied as "all arms represented or explicitly retained with linked blockers". No code/default/flip changed).
>
> Companion docs: [`docs/discord-outbound-remaining-producers.md`](../discord-outbound-remaining-producers.md) (#1175 closure), [`docs/source-of-truth.md`](../source-of-truth.md).

This is the single source of truth for "where is each Discord outbound callsite
on the v3 migration path?". The former compatibility facade
`src/services/discord/outbound/legacy.rs` was removed in #2535 after the last
production producers moved to direct v3 envelopes. The outbound API now lives
in `src/services/discord/outbound/{message, policy, decision, result, delivery,
transport}.rs`, plus the #3089 A1 turn-output controller skeleton in
`outbound/turn_output_controller.rs` (pure add, no live owner yet).

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
| **turn-output controller** `deliver_turn_output<G, L>(...)` | `outbound/turn_output_controller.rs` | **all six owners wired + flag-gated (#3089 A2b–A6b); anchored A4/A5 long chunks migrated in #3998 S1-d; release `launchd.env` forces all six flags ON on every node; compiled default OFF is the per-owner rollback lever** | The single delivery entry point routes the turn-output surfaces through the controller (sink / standby / watcher / turn_bridge / recovery / tui_prompt_relay). All six owner cutovers are wired behind their `AGENTDESK_*_CONTROLLER` flags (A2b sink, A3 standby, A4 watcher, A5 turn_bridge, A6a recovery, A6b tui_prompt_relay). Each flag is **compiled default OFF → byte-identical legacy** so unsetting it is a per-owner rollback lever, but the release `~/.adk/release/config/launchd.env` sets all six `=1`, so on every release node the controller path is effective. A4/A5 now route anchored short-replace and anchored long-chunk-with-delete terminal delivery through the controller when their owner flag is ON; anchored long chunks use `SendNewChunks { delete_anchor: true }` (chunks first, best-effort anchor delete after full success, delete failure records cleanup but stays Delivered). The watcher no-placeholder new-message direct fallback remains legacy because anchor-less fresh-send is not yet a controller verb. The retained exclusions are empty body, `NoRange` deliver-without-advance, headless enqueue, watcher no-placeholder new-message fresh-send, and the TUI completion gate (see §8.1). A2b (`session_relay_sink` short-replace) owns lease `commit`+advance inline before any post-send await (I1), never advances on ambiguous/partial transport (I2), maps `ReplaceLongMessageOutcome::PartialContinuationFailure` to non-advance, and drives the live placeholder card to its terminal state via `PlaceholderController.transition` with the explicit `EditFailPlaceholderPolicy` (#2757) fence. The held lease is RAII-released on future cancel/panic via the internal `ControllerLeaseGuard` (review-fix H1 r2), matching legacy `SinkDeliveryLeaseGuard::Drop`; the guard now keys acquire/renew/commit/release on `DeliveryLeaseKey` instead of `TurnKey`, preserving non-zero turn identity while disambiguating id-0 rows with inflight `started_at` + `turn_start_offset` when both are present and otherwise using the explicit degenerate legacy fallback. If no `lease_key` is supplied, the controller uses the existing markerless path and never commits/releases a lease. The `DeliveryLease` trait abstracts the frozen #3041 `DeliveryLeaseCell` so the controller's commit invariants are mutation-tested. Release health surfaces the effective per-node rollout as `turn_output_controller_rollout` (#3794, read-only). |

`DeliveryOutcome::Delivered` replace metadata is additive: `FreshFallbackAfterEditFailure` carries the fallback replacement anchor when Discord returns one, so A6a recovery can re-record D1 idempotency while non-recovery owners continue to ignore the extra field.

`outbound/mod.rs` re-exports the v3 message/policy/result and shared
transport primitives. New production callsites should import
`outbound::delivery::deliver_outbound` explicitly.

The turn-output controller (`outbound/turn_output_controller.rs`, #3089) now has
**all six owner cutovers wired** (A2b sink, A3 standby, A4 watcher, A5 turn_bridge,
A6a recovery, A6b tui_prompt_relay), each behind its own `AGENTDESK_*_CONTROLLER`
flag. Every flag is **compiled default OFF** (byte-identical legacy → per-owner
rollback lever), but the release `~/.adk/release/config/launchd.env` sets all six
`=1`, so on every release node the controller path is the effective delivery
authority. A4/A5 flags now move the anchored short-replace arms and anchored
long-chunk-with-delete arms off legacy. The A4 watcher no-placeholder
new-message path still stays legacy: with a real ordered range and a non-empty
non-TUI body, `placeholder_msg_id == None` fails the watcher `has_placeholder`
gate and takes the raw fresh-send branch because anchor-less fresh-send is not
yet a controller verb. The retained exclusions are empty body, `NoRange`,
headless enqueue, watcher no-placeholder new-message fresh-send, and the TUI
completion gate; §8.1.1 records the remaining A2b/A3/A6a owner-specific arms
beside the #4053 A4/A5 inventory, so the legacy branches are intentional rather
than untracked. Release
health reports the effective per-node rollout under `turn_output_controller_rollout`
(#3794, read-only): per-owner `enabled`, `enabled_count`, and an
`effective_authority` of `controller` / `mixed` / `legacy` so operators can detect
a node that is missing an env override.

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
| `src/services/discord/router/intake_gate.rs` (`send_reaction_control_reply`) | reaction-control lifecycle replies | **migrated_v3**. Short fixed replies for queued-card POST fallback and duplicate stop now use referenced v3 lifecycle notices. Correlation = `intake-reaction-control:<channel_id>:<message_id>`, semantic = `intake-reaction-control:<channel_id>:<message_id>:<reason_key>`. |
| `src/services/discord/monitoring_status.rs:115` (`deliver_monitoring_status`) | monitoring status | **migrated_v3**. Status banner send + edit with `preserve_inline_content`; edits use `without_idempotency()`. |
| `src/services/discord/meeting_orchestrator.rs:754, 796` (`meeting_outbound_message` / edit path) | meeting status / cancel / parse-error | **migrated_v3**. Stable meeting dedup metadata plus `OutboundOperation::Edit`. |
| `src/services/routines/discord_log.rs:486, 531` (`deliver_or_update_discord_summary`) | routine Discord summary | **migrated_v3**. Uses direct v3 send/edit and disables semantic dedupe for repeated summary writes. |
| `src/integration_tests/discord_flow/scenarios.rs` (removed in #3035 Phase 1) | integration test harness | Mock-Discord roundtrip for §1.2 validation; legacy-sqlite-only harness deleted. |
| `src/integration_tests/agents_setup_e2e.rs` (removed in #3035 Phase 1) | integration test | Wizard-ready E2E; legacy-sqlite-only harness deleted. |

Total **direct migrated_v3 production families: 12** (`dispatch_outbox`,
`review_notifications`, final dispatch completion summaries, issue
announcements, monitoring status, meeting notifications, routine Discord
summaries, CLI text/DM helper, short manual notifications, short manual DM
notifications, intake reaction-control replies, and gateway/turn-bridge
placeholder sends/edits).

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
| `src/services/discord/router/intake_gate.rs:960, 1071` | "duplicate-queue" + "drain-pending" notices | candidate — these are bot notifications, not interaction tokens. Triage: **low priority**, very short fixed strings, no length risk. |

Total commands-bucket: **61 callsites**, all explicitly excluded.

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
5. **Direct-send candidates (low priority).** §B.1 remaining duplicate/drain
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
- `src/services/discord/outbound/delivery.rs`:
  `v3_referenced_send_preserves_reference_and_dedupes` verifies referenced v3
  sends preserve the reply target and duplicate replay avoids a second post.
- `src/services/discord/outbound/reaction_control.rs`:
  `reaction_control_reply_ids_are_stable_per_message_and_reason` verifies the
  reaction-control lifecycle replies keep stable correlation and semantic ids.
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
- `src/services/discord/outbound/manual_delivery.rs`:
  #3807 keeps the manual over-limit notification path on the compatibility
  chunk shim while adding compact continuation context to each split message.

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

---

## 8. Turn-output controller compiled-default flip — decision matrix (#3794 D3)

> Last refreshed: 2026-07-01 (#3794 D3 — compiled-default flip decision matrix +
> Phase-B tracking issue #3998. No code / default / flip changed; docs-only.)

**Scope.** #3794 D1 (release-health `turn_output_controller_rollout` read-only
exposure) and D2 (this page's rollout-state sync) landed in PR #3994. D3 is the
remaining decision: should the six `AGENTDESK_*_CONTROLLER` flags be flipped from
**compiled default OFF to compiled default ON**? This section records that
decision. The Phase-B follow-up is tracked in **#3998**.

**What "flip" means precisely.** Each owner getter today returns `false` when its
env var is unset (`std::env::var(FLAG) … is_some_and(v == "1" || "true")`, with
telemetry emitted *only* when ON so the default-OFF first evaluation is a
byte-identical / deploy no-op). A "flip" changes that default so an unset var
returns `true` (controller path), inverting the rollback semantics from
"unset → legacy" to "must set `=0` → legacy". It does **not** delete any legacy
code: even with a flag ON, the retained exclusions still route the legacy
`else` (see §1 / §3).

### 8.1 Current state (post-#3794 D1/D2)

- Six owner getters, all **compiled default OFF** — `session_relay_sink.rs:57`,
  `standby_relay.rs:51`, `tmux_watcher/terminal_send.rs:34`,
  `turn_bridge/terminal_controller_cutover.rs:36`,
  `recovery_paths/controller_cutover.rs:77`,
  `tui_prompt_relay_controller_cutover.rs:78`.
- Release `~/.adk/release/config/launchd.env` forces all six `=1`, so the
  controller path is **already the effective delivery authority on every release
  node**.
- Read-only per-node rollout is reported under `turn_output_controller_rollout`
  (`outbound/turn_output_controller_rollout_health.rs`, #3794 D1).
- Anchored A4/A5 long-chunk terminal delivery is controller-routed behind the
  existing owner flags (#3998 S1-d).
- Retained exclusions remain legacy by design:
  - empty body: controller `Skipped` would not match A2b/A3 empty-body parity.
  - `NoRange` deliver-without-advance: no advance authority until #4048.
  - headless enqueue: no direct Discord POST for the controller transport.
  - watcher no-placeholder new-message fresh-send: `placeholder_msg_id == None`
    fails the A4 `has_placeholder` gate even with a real ordered range and
    non-empty non-TUI body; anchor-less fresh-send is not yet a controller verb.
    This matches A6a's `None`-placeholder fresh-send class and must be
    re-evaluated with the #3998 flip/legacy-retirement phase.
  - TUI completion gate: lifecycle pause/commit semantics retire with #4047.

#### 8.1.1 Retained-exclusion inventory (#3998 S1-e)

#4053 already documents the A4/A5 retained arms. The remaining controller-owner
arms are explicitly retained here so the Phase-B inventory is closed without
changing runtime behavior.

| owner / arm | code condition | decision / rationale | blocker / re-eval | pin test |
|---|---|---|---|---|
| A2b sink `NoRange` / `cutover_range == None` short-replace | `src/services/discord/session_relay_sink.rs:884-895` requires `cutover_range.is_some()` before controller routing; the legacy replace arm starts at `session_relay_sink.rs:990`. | **RETAIN.** This is the no-advance class: without a real ordered `[start,end)` range, the controller has no offset authority to commit. | #4048 advance-authority work / #3998 flip re-eval. | `flag_on_exclusion_gate_keeps_no_range_and_empty_body_on_legacy_path` |
| A2b sink empty body | `src/services/discord/session_relay_sink.rs:891-895` requires `!relay_text.is_empty()` before controller routing. | **RETAIN.** Legacy zero-chunk replace is committed/advanced; the controller returns `Skipped`, so migrating would flip Skipped-vs-advance semantics. | #4047 / #4048 semantics re-eval. | `controller_skips_empty_body_so_cutover_gate_keeps_it_legacy`; `flag_on_exclusion_gate_keeps_no_range_and_empty_body_on_legacy_path` |
| A3 standby empty body | `src/services/discord/standby_relay.rs:77-79` gates short-replace on `controller_enabled && !formatted.is_empty()`; legacy replace starts at `standby_relay.rs:814`. | **RETAIN.** Same empty-body parity class as A2b/A4/A5: controller `Skipped` would not match legacy committed replace. | #4047 / #4048 semantics re-eval. | `standby_short_replace_should_cutover_pins_both_conditions` |
| A3 standby transport-only `NoLease` | `src/services/discord/standby_relay.rs:672-725` uses `toc::NoLease`, `lease_key: None`, `advance: None`, and `heartbeat: None`. | **RETAIN.** Standby has no lease, no offset authority, and no heartbeat to unify; this is intentionally transport-only instead of inventing a lease. | #3998 flip/legacy-retirement re-eval. | `edited_original_returns_true_and_does_not_delete_original`; `fallback_after_edit_failure_returns_true_and_preserves_original` |
| A3 standby `placeholder_msg_id == None` new-message send | `src/services/discord/standby_relay.rs:893-895` calls legacy `formatting::send_long_message_raw`. | **RETAIN.** Anchor-less fresh-send is not a controller verb yet, same class as watcher no-placeholder (#4053) and A6a `None`-placeholder fresh-send. | #3998 flip/legacy-retirement re-eval after an anchor-less fresh-send verb exists. | `none_placeholder_new_message_stays_legacy_even_when_controller_flag_on` |
| A6a recovery empty body | `src/services/discord/recovery_paths/controller_cutover.rs:112-117` requires `enabled && has_placeholder && !body.is_empty()`. | **RETAIN.** Same empty-body parity class: legacy anchored replace delivers, while the controller would `Skipped` → non-delivered. | #4047 / #4048 semantics re-eval. | `should_cutover_pins_each_condition` |
| A6a recovery `placeholder == None` fresh-send | `src/services/discord/recovery_paths/controller_cutover.rs:112-117` requires `has_placeholder`; caller legacy branch is `src/services/discord/recovery_engine.rs:463-470` via `relay_no_anchor_terminal_text`. | **RETAIN.** Anchor-less fresh-send is not a controller verb yet. #4054 already made this path idempotent through `RecoveryDeliveryContext`, so the residual risk is transport-uniformity, not correctness. The #3297 gone-channel probe remains represented in the anchored controller adapter and must stay. | #3998 flip/legacy-retirement re-eval after an anchor-less fresh-send verb exists. | `should_cutover_pins_each_condition`; `controller_fallback_records_replacement_anchor`; `non_delivered_gone_probe_escalates_permanent` |

### 8.2 Per-flag flip evaluation

| flag (owner) | flip runtime benefit | flip risk / blocker | decision |
|---|---|---|---|
| A2b `sink_short_replace` / A3 `standby_relay` | none — release already env-ON | loses the byte-identical rollback lever; excluded empty-body / `None`-range arms stay legacy | **DEFER** → couple with legacy retirement |
| A4 `watcher_terminal` / A5 `turn_bridge_terminal` | none — release already env-ON | anchored long chunks migrated in #3998 S1-d, but retained exclusions (`NoRange`, headless/no direct POST, empty body, watcher no-placeholder new-message fresh-send, TUI-gate) still keep the legacy `else` non-vestigial | **DEFER** → retained exclusions / legacy retirement are the real blockers |
| A6a `recovery_relay` | none — release already env-ON | #3297 recovery probe must survive; `None`-placeholder fresh-send stays legacy | **DEFER** |
| A6b `tui_prompt_relay` | none — reuses the A5 path | no independent cutover | **DEFER** → retire with A5 |

### 8.3 Why DEFER (flip cost/benefit)

- **Benefit ≈ 0 on the surface it targets.** Release nodes already run the
  controller via `launchd.env`; a compiled-default flip changes nothing there.
  The only marginal gain is defense-in-depth for a node that *lost* its env
  override — but "unset → known-good legacy" is precisely the safety property the
  default-OFF lever provides, so the flip trades that away.
- **Loses the clean rollback lever.** Today `unset env → byte-identical legacy`
  is the safest rollback (no rebuild, no behavior delta). Compiled-default ON
  makes rollback an explicit `=0` opt-out and removes the "unset = known-good"
  guarantee.
- **Does not complete #3089 single-authority.** The retained-exclusion legacy
  `else` branches survive a flip, so the flip alone cannot retire legacy — it is only
  worth taking **coupled with legacy retirement** once the retained exclusions are
  migrated.
- **CI OFF-assumption tests go red.** Six default-OFF assertions guard the
  byte-identical-legacy contract (`if env::var_os(FLAG).is_none() {
  assert!(!..._enabled()) }`). A naive flip inverts their premise and turns them
  red; they must be re-authored (assert the new default + add `=0`-opt-out
  coverage), not silently deleted.

This mirrors #3933, which deferred the `AGENTDESK_DELIVERY_RECORD_AUTHORITY`
default-ON flip until the combined flag was split / the enforce guard made
precise.

### 8.4 Phase-B GO conditions (tracked in #3998)

The flip becomes a real single-authority win only when coupled with legacy
retirement. Before flipping:

1. **Retained-exclusion resolution** — satisfied for Phase-B by #4053, #4054,
   and this §8.1.1 inventory: every excluded arm is either represented by the
   controller or explicitly retained with a linked blocker / flip re-eval
   (`NoRange` → #4048, TUI-gate → #4047, empty body → #4047/#4048, headless/no
   direct POST, anchor-less fresh-send → #3998 flip/legacy-retirement re-eval).
2. **CI-wide OFF-assumption audit** — invert/re-author the six default-OFF
   assertions and audit any other dev/CI default-OFF assumptions.
3. **Release soak evidence** — record duplicate-relay / missed-terminal-body
   metrics via `turn_output_controller_rollout` across a soak window (env-ON is
   behaviorally identical to compiled-ON, so the soak transfers).
4. **Then** flip compiled default OFF→ON **and delete the now-vestigial legacy
   `else` branches in the same slice**, keeping `=0` as the explicit rollback
   opt-out.

**Recommendation: DEFER the compiled-default flip; fold it into the Phase-B
retained-exclusion resolution + legacy retirement (#3998).** Do not flip standalone —
it costs the rollback lever for no runtime gain.
