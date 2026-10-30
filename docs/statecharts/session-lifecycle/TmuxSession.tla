---- MODULE TmuxSession ----
\* One Discord channel's Claude session on the tmux host (HostKind::Tmux), across one dcserver
\* crash and restart. AgentDesk main e8d94425a04867279ced4ec967e9d93df0d7ec0a.
\* The session name is derived from the channel (AgentDesk-claude-<channel>), so `sess` is the
\* one tmux session that name can hold. Each action cites the code it stands for.
EXTENDS Naturals

VARIABLES
    srv,        \* "up" | "down" | "boot": dcserver; "boot" = reconcile gate closed
    sess,       \* "none" | "live" | "dead": the channel's tmux session (dead = pane exited, remain-on-exit)
    exitKind,   \* "normal" | "abnormal": how the last pane exited (wrapper removes its FIFO only on exit:0)
    row,        \* "none" | "held": the channel's inflight JSON row on disk
    turn,       \* in-memory turn: "idle" | "starting" | "sending" | "running" | "recreating" | "stopping"
    msg,        \* a user message waits in the channel queue
    delivered,  \* "none" | "live" | "dead": the session state when a prompt last landed in it
    restored,   \* "no" | "rows" | "watchers": boot passes done (restore_inflight, restore_tmux_watchers)
    crashes, deaths, probeFails

vars == <<srv, sess, exitKind, row, turn, msg, delivered, restored, crashes, deaths, probeFails>>

MaxCrash == 1
MaxDeath == 1
MaxProbeFail == 2

Init ==
    /\ srv = "up" /\ sess = "none" /\ exitKind = "normal" /\ row = "none" /\ turn = "idle"
    /\ msg = FALSE /\ delivered = "none" /\ restored = "no"
    /\ crashes = 0 /\ deaths = 0 /\ probeFails = 0

\* A user posts; intake queues it while busy or while the reconcile gate is closed
\* (router/intake_gate.rs:1190-1195).
UserSends ==
    /\ ~msg /\ msg' = TRUE
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, delivered, restored, crashes, deaths, probeFails>>

\* Intake claims the mailbox and creates the inflight row with create-new; an existing row
\* makes the start defer and requeue (router/message_handler/foreign_row.rs:63-82,
\* inflight/save_store.rs:1195-1206). The row is written before the provider spawns.
ClaimTurn ==
    /\ srv = "up" /\ msg /\ turn = "idle" /\ row = "none"
    /\ row' = "held" /\ turn' = "starting" /\ msg' = FALSE
    /\ UNCHANGED <<srv, sess, exitKind, delivered, restored, crashes, deaths, probeFails>>

\* Startup plan WarmFollowup: session exists with a live pane (claude/backend_routing.rs:18-31,
\* claude.rs:2386-2415). The probe and the FIFO write are separate steps.
PlanWarm ==
    /\ turn = "starting" /\ sess = "live"
    /\ turn' = "sending"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, restored, crashes, deaths, probeFails>>

\* ColdStart / RecreateStaleSession: kill a dead session, create a new one whose launch script
\* carries the prompt (claude.rs:2524-2533 teardown, 2627-2649 create + remain-on-exit).
PlanCold ==
    /\ turn = "starting" /\ sess \in {"none", "dead"}
    /\ sess' = "live" /\ turn' = "running" /\ delivered' = "live"
    /\ UNCHANGED <<srv, exitKind, row, msg, restored, crashes, deaths, probeFails>>

\* The follow-up opens the input FIFO and writes the prompt (claude.rs:2714-2727).
FollowupDelivered ==
    /\ turn = "sending" /\ sess = "live"
    /\ turn' = "running" /\ delivered' = "live"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, restored, crashes, deaths, probeFails>>

\* The wrapper exited normally and removed its FIFO (tmux_wrapper.rs:457-461): the open fails
\* with "No such file", which asks for a recreate (tmux_diagnostics.rs:279-300, claude.rs:2729-2733).
\* After an abnormal exit the FIFO stays with no reader and a write-only open blocks: no step.
FollowupFails ==
    /\ turn = "sending" /\ sess # "live" /\ exitKind = "normal"
    /\ turn' = "recreating"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, restored, crashes, deaths, probeFails>>

\* Recreate: tear the old session down and cold-start with the same prompt
\* (claude.rs:2452-2473 then 2536-2649; restart retry: turn_bridge/retry_state.rs:355).
Recreate ==
    /\ turn = "recreating"
    /\ sess' = "live" /\ turn' = "running" /\ delivered' = "live"
    /\ UNCHANGED <<srv, exitKind, row, msg, restored, crashes, deaths, probeFails>>

\* The provider process exits; tmux keeps the dead pane (claude.rs:2649 remain-on-exit).
ProviderExits ==
    /\ sess = "live" /\ deaths < MaxDeath
    /\ sess' = "dead" /\ deaths' = deaths + 1
    /\ exitKind' \in {"normal", "abnormal"}
    /\ UNCHANGED <<srv, row, turn, msg, delivered, restored, crashes, probeFails>>

\* The watcher relays the result; the finalizer clears the row. The session stays warm.
\* A watcher reattached during boot already runs, so this can happen before intake opens.
TurnEnds ==
    /\ srv # "down" /\ turn = "running" /\ sess = "live"
    /\ row' = "none" /\ turn' = "idle"
    /\ UNCHANGED <<srv, sess, exitKind, msg, delivered, restored, crashes, deaths, probeFails>>

\* The watcher sees the pane die mid-turn and finalizes with a notice, clearing the row.
TurnDiesMidway ==
    /\ srv # "down" /\ turn = "running" /\ sess = "dead"
    /\ row' = "none" /\ turn' = "idle"
    /\ UNCHANGED <<srv, sess, exitKind, msg, delivered, restored, crashes, deaths, probeFails>>

\* Force stop: kill the session first, then clear the channel's own row
\* (turn_lifecycle.rs:471-494, then 519-).
StopKill ==
    /\ srv = "up" /\ turn = "running"
    /\ sess' = "none" /\ turn' = "stopping"
    /\ UNCHANGED <<srv, exitKind, row, msg, delivered, restored, crashes, deaths, probeFails>>

StopClear ==
    /\ turn = "stopping"
    /\ row' = "none" /\ turn' = "idle"
    /\ UNCHANGED <<srv, sess, exitKind, msg, delivered, restored, crashes, deaths, probeFails>>

\* dcserver dies: in-memory turn and watcher are gone; tmux, the row and the queue survive.
Crash ==
    /\ srv # "down" /\ crashes < MaxCrash
    /\ srv' = "down" /\ turn' = "idle" /\ restored' = "no" /\ crashes' = crashes + 1
    /\ UNCHANGED <<sess, exitKind, row, msg, delivered, deaths, probeFails>>

\* Boot with the reconcile gate closed (runtime_bootstrap/shared_data.rs:164).
Boot ==
    /\ srv = "down"
    /\ srv' = "boot"
    /\ UNCHANGED <<sess, exitKind, row, turn, msg, delivered, restored, crashes, deaths, probeFails>>

\* restore_inflight_turns (runtime_bootstrap/recovery_flush.rs:48 -> recovery_engine/restore_inflight.rs).
\* Live pane: reattach the watcher and re-own the turn (restore_inflight.rs:1773-1789, 2193).
RestoreReattach ==
    /\ srv = "boot" /\ restored = "no" /\ row = "held" /\ sess = "live"
    /\ turn' = "running" /\ restored' = "rows"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, crashes, deaths, probeFails>>

\* Session present, pane dead: read what is left, then retry the turn on a new session
\* (phase_policy.rs:407-418 InflightRestore; restore_inflight.rs:2280-2293 "__session_died_retry__").
RestoreRetry ==
    /\ srv = "boot" /\ restored = "no" /\ row = "held" /\ sess = "dead"
    /\ turn' = "recreating" /\ restored' = "rows"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, crashes, deaths, probeFails>>

\* Session missing: post the interrupted notice and clear the row (restore_inflight.rs:1547-1615).
RestoreMissing ==
    /\ srv = "boot" /\ restored = "no" /\ row = "held" /\ sess = "none"
    /\ row' = "none" /\ restored' = "rows"
    /\ UNCHANGED <<srv, sess, exitKind, turn, msg, delivered, crashes, deaths, probeFails>>

\* The probe fails, or the host guard keeps the row: leave row and report as stored
\* (recovery_engine/tmux_probe.rs:86-108, 224-237; restore_inflight.rs:1548-1549).
RestoreDefer ==
    /\ srv = "boot" /\ restored = "no" /\ row = "held" /\ probeFails < MaxProbeFail
    /\ restored' = "rows" /\ probeFails' = probeFails + 1
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, msg, delivered, crashes, deaths>>

RestoreNoRow ==
    /\ srv = "boot" /\ restored = "no" /\ row = "none"
    /\ restored' = "rows"
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, msg, delivered, crashes, deaths, probeFails>>

\* restore_tmux_watchers (recovery_flush.rs:73 -> watchers/lifecycle/restore.rs). A channel that
\* restore_inflight did not take: a live session with a row re-registers the turn (restore.rs:484-517);
\* a dead session is killed and its row is left as is (restore.rs:459-476, 811-893).
WatchersAdopt ==
    /\ srv = "boot" /\ restored = "rows" /\ turn = "idle" /\ row = "held" /\ sess = "live"
    /\ turn' = "running" /\ restored' = "watchers"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, crashes, deaths, probeFails>>

WatchersKillDead ==
    /\ srv = "boot" /\ restored = "rows" /\ turn = "idle" /\ sess = "dead"
    /\ sess' = "none" /\ restored' = "watchers"
    /\ UNCHANGED <<srv, exitKind, row, turn, msg, delivered, crashes, deaths, probeFails>>

WatchersNothing ==
    /\ srv = "boot" /\ restored = "rows"
    /\ ~(turn = "idle" /\ row = "held" /\ sess = "live")
    /\ ~(turn = "idle" /\ sess = "dead")
    /\ restored' = "watchers"
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, msg, delivered, crashes, deaths, probeFails>>

\* The watcher pass's own pane probe fails: that session is skipped (restore.rs:453-458).
WatchersProbeFails ==
    /\ srv = "boot" /\ restored = "rows" /\ turn = "idle" /\ sess # "none"
    /\ probeFails < MaxProbeFail
    /\ restored' = "watchers" /\ probeFails' = probeFails + 1
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, msg, delivered, crashes, deaths>>

\* Kick queued turns, then open intake (recovery_flush.rs:95-120).
OpenIntake ==
    /\ srv = "boot" /\ restored = "watchers"
    /\ srv' = "up"
    /\ UNCHANGED <<sess, exitKind, row, turn, msg, delivered, restored, crashes, deaths, probeFails>>

Next ==
    \/ UserSends \/ ClaimTurn \/ PlanWarm \/ PlanCold \/ FollowupDelivered \/ FollowupFails
    \/ Recreate \/ ProviderExits \/ TurnEnds \/ TurnDiesMidway \/ StopKill \/ StopClear
    \/ Crash \/ Boot \/ RestoreReattach \/ RestoreRetry \/ RestoreMissing \/ RestoreDefer
    \/ RestoreNoRow \/ WatchersAdopt \/ WatchersKillDead \/ WatchersNothing \/ WatchersProbeFails
    \/ OpenIntake

Spec == Init /\ [][Next]_vars

\* Rules the reader agreed to.
\* A prompt lands only in a session that is running.
PromptOnlyToLiveSession == delivered \in {"none", "live"}
\* Once intake is open, the channel's inflight row belongs to a turn the server is running.
RowOnlyWithTurn == (srv = "up" /\ turn = "idle") => row = "none"
\* Once intake is open, a stored row names a session that exists, unless its turn is creating
\* or killing that session right now.
RowNamesSession ==
    (srv = "up" /\ row = "held" /\ turn \notin {"starting", "recreating", "stopping"}) => sess # "none"
====
