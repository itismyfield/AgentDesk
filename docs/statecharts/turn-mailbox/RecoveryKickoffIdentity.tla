------------------------- MODULE RecoveryKickoffIdentity -------------------------
\* One turn's mailbox ownership across a restart and its identity-guarded finalize.
\* Written against 9a12a379b93049c5849900c80795b5ddf446a65f.
\*
\* RowKind "real": the inflight row carries a Discord user message id (U = FinId = 1).
\* RowKind "zero": user_msg_id = 0 (TUI-direct, turn/deliver, injected input); the
\*   finalizer key is effective_finalizer_turn_id = injected/finalizer/synthetic id (2).
\*   src/services/discord/inflight/model.rs:1241
\* Policy "before": recovery kickoff binds optional_message_id(row.user_msg_id).
\*   src/services/discord/recovery_engine/restore_inflight/kickoff_identity.rs:19
\* Policy "after":  recovery kickoff binds the row's effective finalizer id.
\* Policy "weak":   rejected alternative, finalizer also releases a held slot whose
\*   active id is 0 (channel-scoped fallback on a guarded miss).
EXTENDS Naturals

CONSTANTS RowKind, Policy

UserId == IF RowKind = "real" THEN 1 ELSE 0
FinId == IF RowKind = "real" THEN 1 ELSE 2
OtherId == 3   \* a stale terminal of an earlier turn in the same channel

KickoffId == IF Policy = "after" THEN FinId ELSE UserId

VARIABLES phase, token, active, holder, wrong, staleDone
vars == <<phase, token, active, holder, wrong, staleDone>>

TypeOK ==
    /\ phase \in {"idle", "live", "down", "recovered", "finished"}
    /\ token \in BOOLEAN
    /\ active \in {0, 1, 2, 3}
    /\ holder \in {"none", "ours"}
    /\ wrong \in BOOLEAN
    /\ staleDone \in BOOLEAN

Init ==
    /\ phase = "idle"
    /\ token = FALSE
    /\ active = 0
    /\ holder = "none"
    /\ wrong = FALSE
    /\ staleDone = FALSE

\* Identity-guarded finish: active id must equal the key.
\*   src/services/turn_orchestrator/episode_identity.rs:345 (finish_turn_identity_matches)
Matches(id) == token /\ (active = id \/ (Policy = "weak" /\ active = 0))

\* Live admission binds the finalizer id (Discord message, or the TUI-direct anchor).
\*   src/services/turn_orchestrator.rs:2006, tui_prompt_relay/synthetic_start/claim/admitted.rs:121
Start ==
    /\ phase = "idle"
    /\ phase' = "live" /\ token' = TRUE /\ active' = FinId /\ holder' = "ours"
    /\ UNCHANGED <<wrong, staleDone>>

\* dcserver restart: mailbox actor state is in memory; the inflight row survives.
Restart ==
    /\ phase = "live"
    /\ phase' = "down" /\ token' = FALSE /\ active' = 0 /\ holder' = "none"
    /\ UNCHANGED <<wrong, staleDone>>

\* Pane alive: watcher reattach re-registers with the effective finalizer id.
\*   src/services/discord/recovery_engine/runtime.rs:193, turn_orchestrator.rs:2045
Reattach ==
    /\ phase = "down"
    /\ phase' = "recovered" /\ token' = TRUE /\ active' = FinId /\ holder' = "ours"
    /\ UNCHANGED <<wrong, staleDone>>

\* Pane not alive: recovery reader kickoff on the empty slot.
\*   src/services/discord/recovery_engine/restore_inflight.rs:2245, turn_orchestrator.rs:2078
Kickoff ==
    /\ phase = "down"
    /\ phase' = "recovered" /\ token' = TRUE /\ active' = KickoffId /\ holder' = "ours"
    /\ UNCHANGED <<wrong, staleDone>>

\* The bridge finalizes with key effective_finalizer_turn_id.
\*   turn_bridge/post_loop_finalize.rs:686 -> turn_finalizer/episode.rs:93 -> finalize.rs:188
\*   miss: finalize.rs:254 "identity-guarded mailbox release skipped" (active printed as 0)
Finalize ==
    /\ phase \in {"live", "recovered"}
    /\ phase' = "finished"
    /\ IF Matches(FinId)
         THEN token' = FALSE /\ active' = 0 /\ holder' = "none"
         ELSE UNCHANGED <<token, active, holder>>
    /\ UNCHANGED <<wrong, staleDone>>

\* A stale terminal of an earlier turn reaches the guarded finish once.
\*   turn_finalizer/finalize.rs:188 (mailbox_finish_turn_if_matches)
Stale ==
    /\ ~staleDone
    /\ phase \in {"live", "recovered"}
    /\ staleDone' = TRUE
    /\ IF Matches(OtherId)
         THEN /\ token' = FALSE /\ active' = 0 /\ holder' = "none"
              /\ wrong' = (holder = "ours")
         ELSE UNCHANGED <<token, active, holder, wrong>>
    /\ UNCHANGED phase

Done == phase = "finished" /\ UNCHANGED vars

Recover == Reattach \/ Kickoff

Next == Start \/ Restart \/ Reattach \/ Kickoff \/ Finalize \/ Stale \/ Done

Spec == Init /\ [][Next]_vars /\ WF_vars(Start) /\ WF_vars(Recover) /\ WF_vars(Finalize)

\* A stale terminal never releases the slot our turn holds.
NoWrongRelease == ~wrong

\* A started turn eventually leaves the mailbox free.
MailboxEventuallyReleased == <>[](~token)
=============================================================================
