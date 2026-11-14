---- MODULE TmuxSession ----
\* One Discord channel's Claude session on the tmux host (HostKind::Tmux), across one dcserver
\* crash and restart. AgentDesk main e8d94425a04867279ced4ec967e9d93df0d7ec0a.
\* The session name is derived from the channel (AgentDesk-claude-<channel>), so `sess` is the
\* one tmux session that name can hold. Each action cites the code it stands for.
EXTENDS Naturals, Sequences, FiniteSets
VARIABLE effort

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

LegacyInit ==
    /\ srv = "up" /\ sess = "none" /\ exitKind = "normal" /\ row = "none" /\ turn = "idle"
    /\ msg = FALSE /\ delivered = "none" /\ restored = "no"
    /\ crashes = 0 /\ deaths = 0 /\ probeFails = 0

\* A user posts; intake queues it while busy or while the reconcile gate is closed
\* (router/intake_gate.rs:1190-1195).
UserSends ==
    /\ UNCHANGED effort
    /\ ~msg /\ msg' = TRUE
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, delivered, restored, crashes, deaths, probeFails>>

\* Intake claims the mailbox and creates the inflight row with create-new; an existing row
\* makes the start defer and requeue (router/message_handler/foreign_row.rs:63-82,
\* inflight/save_store.rs:1195-1206). The row is written before the provider spawns.
ClaimTurn ==
    /\ UNCHANGED effort
    /\ srv = "up" /\ msg /\ turn = "idle" /\ row = "none"
    /\ row' = "held" /\ turn' = "starting" /\ msg' = FALSE
    /\ UNCHANGED <<srv, sess, exitKind, delivered, restored, crashes, deaths, probeFails>>

\* Startup plan WarmFollowup: session exists with a live pane (claude/backend_routing.rs:18-31,
\* claude.rs:2386-2415). The probe and the FIFO write are separate steps.
PlanWarm ==
    /\ UNCHANGED effort
    /\ turn = "starting" /\ sess = "live"
    /\ turn' = "sending"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, restored, crashes, deaths, probeFails>>

\* ColdStart / RecreateStaleSession: kill a dead session, create a new one whose launch script
\* carries the prompt (claude.rs:2524-2533 teardown, 2627-2649 create + remain-on-exit).
PlanCold ==
    /\ UNCHANGED effort
    /\ turn = "starting" /\ sess \in {"none", "dead"}
    /\ sess' = "live" /\ turn' = "running" /\ delivered' = "live"
    /\ UNCHANGED <<srv, exitKind, row, msg, restored, crashes, deaths, probeFails>>

\* The follow-up opens the input FIFO and writes the prompt (claude.rs:2714-2727).
FollowupDelivered ==
    /\ UNCHANGED effort
    /\ turn = "sending" /\ sess = "live"
    /\ turn' = "running" /\ delivered' = "live"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, restored, crashes, deaths, probeFails>>

\* The wrapper exited normally and removed its FIFO (tmux_wrapper.rs:457-461): the open fails
\* with "No such file", which asks for a recreate (tmux_diagnostics.rs:279-300, claude.rs:2729-2733).
\* After an abnormal exit the FIFO stays with no reader and a write-only open blocks: no step.
FollowupFails ==
    /\ UNCHANGED effort
    /\ turn = "sending" /\ sess # "live" /\ exitKind = "normal"
    /\ turn' = "recreating"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, restored, crashes, deaths, probeFails>>

\* Recreate: tear the old session down and cold-start with the same prompt
\* (claude.rs:2452-2473 then 2536-2649; restart retry: turn_bridge/retry_state.rs:355).
Recreate ==
    /\ UNCHANGED effort
    /\ turn = "recreating"
    /\ sess' = "live" /\ turn' = "running" /\ delivered' = "live"
    /\ UNCHANGED <<srv, exitKind, row, msg, restored, crashes, deaths, probeFails>>

\* The provider process exits; tmux keeps the dead pane (claude.rs:2649 remain-on-exit).
ProviderExits ==
    /\ UNCHANGED effort
    /\ sess = "live" /\ deaths < MaxDeath
    /\ sess' = "dead" /\ deaths' = deaths + 1
    /\ exitKind' \in {"normal", "abnormal"}
    /\ UNCHANGED <<srv, row, turn, msg, delivered, restored, crashes, probeFails>>

\* The watcher relays the result; the finalizer clears the row. The session stays warm.
\* A watcher reattached during boot already runs, so this can happen before intake opens.
TurnEnds ==
    /\ UNCHANGED effort
    /\ srv # "down" /\ turn = "running" /\ sess = "live"
    /\ row' = "none" /\ turn' = "idle"
    /\ UNCHANGED <<srv, sess, exitKind, msg, delivered, restored, crashes, deaths, probeFails>>

\* The watcher sees the pane die mid-turn and finalizes with a notice, clearing the row.
TurnDiesMidway ==
    /\ UNCHANGED effort
    /\ srv # "down" /\ turn = "running" /\ sess = "dead"
    /\ row' = "none" /\ turn' = "idle"
    /\ UNCHANGED <<srv, sess, exitKind, msg, delivered, restored, crashes, deaths, probeFails>>

\* Force stop: kill the session first, then clear the channel's own row
\* (turn_lifecycle.rs:471-494, then 519-).
StopKill ==
    /\ UNCHANGED effort
    /\ srv = "up" /\ turn = "running"
    /\ sess' = "none" /\ turn' = "stopping"
    /\ UNCHANGED <<srv, exitKind, row, msg, delivered, restored, crashes, deaths, probeFails>>

StopClear ==
    /\ UNCHANGED effort
    /\ turn = "stopping"
    /\ row' = "none" /\ turn' = "idle"
    /\ UNCHANGED <<srv, sess, exitKind, msg, delivered, restored, crashes, deaths, probeFails>>

\* dcserver dies: in-memory turn and watcher are gone; tmux, the row and the queue survive.
Crash ==
    /\ UNCHANGED effort
    /\ srv # "down" /\ crashes < MaxCrash
    /\ srv' = "down" /\ turn' = "idle" /\ restored' = "no" /\ crashes' = crashes + 1
    /\ UNCHANGED <<sess, exitKind, row, msg, delivered, deaths, probeFails>>

\* Boot with the reconcile gate closed (runtime_bootstrap/shared_data.rs:164).
Boot ==
    /\ UNCHANGED effort
    /\ srv = "down"
    /\ srv' = "boot"
    /\ UNCHANGED <<sess, exitKind, row, turn, msg, delivered, restored, crashes, deaths, probeFails>>

\* restore_inflight_turns (runtime_bootstrap/recovery_flush.rs:48 -> recovery_engine/restore_inflight.rs).
\* Live pane: reattach the watcher and re-own the turn (restore_inflight.rs:1773-1789, 2193).
RestoreReattach ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "no" /\ row = "held" /\ sess = "live"
    /\ turn' = "running" /\ restored' = "rows"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, crashes, deaths, probeFails>>

\* Session present, pane dead: read what is left, then retry the turn on a new session
\* (phase_policy.rs:407-418 InflightRestore; restore_inflight.rs:2280-2293 "__session_died_retry__").
RestoreRetry ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "no" /\ row = "held" /\ sess = "dead"
    /\ turn' = "recreating" /\ restored' = "rows"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, crashes, deaths, probeFails>>

\* Session missing: post the interrupted notice and clear the row (restore_inflight.rs:1547-1615).
RestoreMissing ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "no" /\ row = "held" /\ sess = "none"
    /\ row' = "none" /\ restored' = "rows"
    /\ UNCHANGED <<srv, sess, exitKind, turn, msg, delivered, crashes, deaths, probeFails>>

\* A missing namespace socket is also confirmed absence, after reported-path and lstat checks
\* (platform/tmux/session_inventory.rs). Transport errors still use RestoreDefer.
RestoreMissingSocket == RestoreMissing

\* The probe fails, or the host guard keeps the row: leave row and report as stored
\* (recovery_engine/tmux_probe.rs:86-108, 224-237; restore_inflight.rs:1548-1549).
RestoreDefer ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "no" /\ row = "held" /\ probeFails < MaxProbeFail
    /\ restored' = "rows" /\ probeFails' = probeFails + 1
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, msg, delivered, crashes, deaths>>

RestoreNoRow ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "no" /\ row = "none"
    /\ restored' = "rows"
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, msg, delivered, crashes, deaths, probeFails>>

\* restore_tmux_watchers (recovery_flush.rs:73 -> watchers/lifecycle/restore.rs). A channel that
\* restore_inflight did not take: a live session with a row re-registers the turn (restore.rs:484-517);
\* a dead session is killed and its row is left as is (restore.rs:459-476, 811-893).
WatchersAdopt ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "rows" /\ turn = "idle" /\ row = "held" /\ sess = "live"
    /\ turn' = "running" /\ restored' = "watchers"
    /\ UNCHANGED <<srv, sess, exitKind, row, msg, delivered, crashes, deaths, probeFails>>

WatchersKillDead ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "rows" /\ turn = "idle" /\ sess = "dead"
    /\ sess' = "none" /\ restored' = "watchers"
    /\ UNCHANGED <<srv, exitKind, row, turn, msg, delivered, crashes, deaths, probeFails>>

WatchersNothing ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "rows"
    /\ ~(turn = "idle" /\ row = "held" /\ sess = "live")
    /\ ~(turn = "idle" /\ sess = "dead")
    /\ restored' = "watchers"
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, msg, delivered, crashes, deaths, probeFails>>

\* The watcher pass's own pane probe fails: that session is skipped (restore.rs:453-458).
WatchersProbeFails ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "rows" /\ turn = "idle" /\ sess # "none"
    /\ probeFails < MaxProbeFail
    /\ restored' = "watchers" /\ probeFails' = probeFails + 1
    /\ UNCHANGED <<srv, sess, exitKind, row, turn, msg, delivered, crashes, deaths>>

\* Kick queued turns, then open intake (recovery_flush.rs:95-120).
OpenIntake ==
    /\ UNCHANGED effort
    /\ srv = "boot" /\ restored = "watchers"
    /\ srv' = "up"
    /\ UNCHANGED <<sess, exitKind, row, turn, msg, delivered, restored, crashes, deaths, probeFails>>

Next ==
    \/ UserSends \/ ClaimTurn \/ PlanWarm \/ PlanCold \/ FollowupDelivered \/ FollowupFails
    \/ Recreate \/ ProviderExits \/ TurnEnds \/ TurnDiesMidway \/ StopKill \/ StopClear
    \/ Crash \/ Boot \/ RestoreReattach \/ RestoreRetry \/ RestoreMissing \/ RestoreMissingSocket \/ RestoreDefer
    \/ RestoreNoRow \/ WatchersAdopt \/ WatchersKillDead \/ WatchersNothing \/ WatchersProbeFails
    \/ OpenIntake

\* Effort restart projection, source baseline 193a57168f; full code census is in the report.
\* Existing lifecycle configs keep this projection dormant. One interrupted root, two inputs,
\* and a second bounded request distinguish coalescing from interrupting a running continuation.
allVars == <<vars, effort>>
EffortDormant == effort = [mode |-> "dormant"]

Init == LegacyInit /\ EffortDormant
Spec == Init /\ [][Next]_allVars

\* Rules the reader agreed to.
\* A prompt lands only in a session that is running.
PromptOnlyToLiveSession == delivered \in {"none", "live"}
\* Once intake is open, the channel's inflight row belongs to a turn the server is running.
RowOnlyWithTurn == (srv = "up" /\ turn = "idle") => row = "none"
\* Once intake is open, a stored row names a session that exists, unless its turn is creating
\* or killing that session right now.
RowNamesSession ==
    (srv = "up" /\ row = "held" /\ turn \notin {"starting", "recreating", "stopping"}) => sess # "none"

EffortPeople == 1..2
EffortRoots == 1..2
EffortTasks == 0..4
EffortInit(mode) ==
    /\ LegacyInit
    /\ effort = [mode |-> mode, phase |-> "running", changes |-> 0,
        active |-> (IF mode = "idle" THEN "none" ELSE "original"), root |-> 0,
        pending |-> 0, closed |-> {}, resumed |-> {}, noticeSettled |-> {},
        noticeIntents |-> [r \in EffortRoots |-> 0], notices |-> [r \in EffortRoots |-> 0],
        continuations |-> [r \in EffortRoots |-> 0], replies |-> [t \in EffortTasks |-> 0],
        users |-> <<>>, arrived |-> 0, userCounts |-> [i \in EffortPeople |-> 0], user |-> 0,
        wake |-> FALSE, row |-> (mode # "idle"), placeholder |-> (mode # "idle"),
        watcher |-> "old", presence |-> (IF mode = "idle" THEN "Idle" ELSE "Busy"),
        binding |-> 0, daemonGeneration |-> 0, sameSession |-> TRUE, revision |-> 0,
        failed |-> FALSE, failureVisible |-> FALSE, noticeFailed |-> FALSE,
        priorityBroken |-> FALSE, barrierBroken |-> FALSE, barrier |-> 0, queuedC |-> 0,
        admittedC |-> [r \in EffortRoots |-> 0], debt |-> 1, debtPosts |-> 0,
        late |-> 0, crashes |-> 0, durable |-> FALSE, cancelled |-> {}, discarded |-> {},
        clears |-> 0, resetPending |-> (mode \in {"reset", "reset-kept", "reset-c", "reset-newer"}),
        sessionResets |-> 0, newerChanges |-> 0]
EffortOriginalNeverFinishes == effort.mode \in
    {"nonfinishing", "nonfinishing-failure", "no-interrupt", "no-close"}
EffortRebooting == effort.phase \in {"requested", "prepared", "interrupted", "stopped", "booting"}
\* Phases where the old TUI is killed but the restart has not resolved yet.
EffortCutUnresolved == {"interrupted", "stopped", "booting"}
EffortRequest ==
    /\ UNCHANGED vars
    /\ effort.changes = 0
    /\ effort' = [effort EXCEPT !.changes = 1,
        !.phase = IF @ = "running" /\ effort.mode = "current-herdr" THEN "waiting-herdr"
                  ELSE IF effort.mode = "current-tmux" THEN "waiting" ELSE "requested"]
EffortSecondRequest ==
    /\ UNCHANGED vars
    /\ effort.changes = 1
    /\ (effort.mode = "coalesce" /\ effort.phase = "booting")
       \/ (effort.mode = "second" /\ effort.phase = "ready" /\ effort.active = "continuation")
    /\ effort' = [effort EXCEPT !.changes = 2,
        !.phase = IF effort.mode = "coalesce" THEN @ ELSE "requested"]
EffortInputArrives ==
    /\ UNCHANGED vars
    /\ effort.changes > 0 /\ effort.arrived < 2
    /\ effort' = [effort EXCEPT !.arrived = @ + 1,
        !.users = Append(@, effort.arrived + 1),
        !.wake = IF EffortRebooting \/ effort.phase = "failed" THEN @ ELSE TRUE]
\* Before any kill the owner commits the not-ready intent and its one C ticket; the barrier holds
\* from here. The late-intent mutant (S4b head) makes it durable only at ready.
EffortPrepare ==
    /\ UNCHANGED vars
    /\ effort.phase = "requested" /\ effort.active # "none" /\ effort.root < 2
    /\ effort.mode # "no-interrupt"
    /\ effort' = [effort EXCEPT !.phase = "prepared", !.pending = effort.root + 1,
        !.barrier = effort.root + 1, !.durable = effort.mode # "late-intent"]
\* The exact captured token/TurnKey decides whether a natural completion or interruption wins.
EffortInterrupt ==
    /\ UNCHANGED vars
    /\ effort.phase = "prepared"
    /\ effort' = [effort EXCEPT !.root = @ + 1,
        !.active = "none", !.phase = "interrupted", !.presence = "Unknown",
        !.noticeIntents[effort.root + 1] = IF effort.mode = "no-notice" THEN 0 ELSE 1]
\* A refused or failed kill cancels the prepared intent in one owner commit; the turn keeps running.
\* The kill-fails-keeps mutant leaves the barrier standing.
EffortKillFails ==
    /\ UNCHANGED vars
    /\ effort.phase = "prepared" /\ effort.mode \in {"kill-fails", "kill-fails-keeps"}
    /\ LET keep == effort.mode = "kill-fails-keeps" IN
       effort' = [effort EXCEPT !.phase = "kept", !.wake = TRUE,
           !.pending = IF keep THEN @ ELSE 0, !.barrier = IF keep THEN @ ELSE 0,
           !.cancelled = IF keep THEN @ ELSE @ \cup {effort.pending}]
\* /clear, cancel-all or a single cancel of C while C is unsubmitted closes the intent in the same
\* owner commit that drops C. The clear-keeps mutant (S4b head) drops only the queued item.
EffortClear ==
    /\ UNCHANGED vars
    /\ effort.mode \in {"clear", "cancel-c", "clear-keeps"} /\ effort.clears = 0
    /\ effort.pending > 0
    /\ effort.phase \in {"prepared", "interrupted", "stopped", "booting", "ready"}
    /\ effort.mode = "cancel-c" => effort.queuedC = effort.pending
    /\ LET keep == effort.mode = "clear-keeps"
           drop == effort.mode # "cancel-c"
       IN effort' = [effort EXCEPT !.clears = 1, !.queuedC = 0,
           !.users = IF drop THEN <<>> ELSE @,
           !.discarded = IF drop THEN @ \cup {effort.users[j] : j \in 1..Len(effort.users)} ELSE @,
           !.pending = IF keep THEN @ ELSE 0, !.barrier = IF keep THEN @ ELSE 0,
           !.cancelled = IF keep THEN @ ELSE @ \cup {effort.pending},
           !.wake = IF EffortRebooting THEN @ ELSE TRUE]
EffortNoticeFails ==
    /\ UNCHANGED vars
    /\ effort.root > 0 /\ ~effort.noticeFailed
    /\ effort' = [effort EXCEPT !.noticeFailed = TRUE]
\* Success means a visible stable-key notice, not merely an attempted send or queued future.
EffortTellInterrupted ==
    /\ UNCHANGED vars
    /\ \E r \in 1..effort.root :
       /\ effort.noticeIntents[r] = 1 /\ effort.notices[r] = 0
       /\ effort' = [effort EXCEPT !.notices[r] = 1, !.noticeSettled = @ \cup {r}]
EffortDuplicateNotice ==
    /\ UNCHANGED vars
    /\ effort.mode = "notice-twice"
    /\ \E r \in 1..effort.root :
       /\ effort.notices[r] = 1
       /\ effort' = [effort EXCEPT !.notices[r] = 2]
\* HostTerminated is a mute settlement: the reader still owns committed output.
EffortCloseInterrupted ==
    /\ UNCHANGED vars
    /\ effort.phase = "interrupted" /\ effort.mode # "no-close"
    /\ effort' = [effort EXCEPT !.closed = @ \cup {effort.root}, !.phase = "stopped",
        !.row = FALSE, !.placeholder = FALSE, !.watcher = "none"]
EffortNoActiveTurn ==
    /\ UNCHANGED vars
    /\ effort.phase = "requested" /\ effort.active = "none"
    /\ effort' = [effort EXCEPT !.phase = "stopped", !.watcher = "none"]
\* Current local path runs only at the next turn, after the old turn naturally ends.
EffortCurrentHerdrApply ==
    /\ UNCHANGED vars
    /\ effort.phase = "waiting-herdr" /\ effort.active = "none"
    /\ effort' = [effort EXCEPT !.phase = "ready", !.wake = TRUE]
EffortCurrentNextTurn ==
    /\ UNCHANGED vars
    /\ effort.phase = "waiting" /\ effort.active = "none" /\ effort.users # <<>>
    /\ effort' = [effort EXCEPT !.phase = "stopped", !.watcher = "none"]
EffortStopTui ==
    /\ UNCHANGED vars
    /\ effort.phase = "stopped"
    /\ effort' = [effort EXCEPT !.phase = "booting", !.presence = "Unknown",
        !.users = IF effort.mode = "drop-queue" THEN <<>> ELSE @,
        !.resetPending = IF effort.mode \in {"reset-kept", "reset-c"} THEN @ ELSE FALSE]
\* A /fast or /model saved after the restart consumed the pending its launch reflects.
EffortNewerChange ==
    /\ UNCHANGED vars
    /\ effort.mode = "reset-newer" /\ effort.newerChanges = 0
    /\ effort.phase \in {"booting", "ready"} /\ ~effort.resetPending
    /\ effort' = [effort EXCEPT !.newerChanges = 1, !.resetPending = TRUE]
EffortLaunchFails ==
    /\ UNCHANGED vars
    /\ effort.phase = "booting" /\ effort.mode \in {"failure", "retry", "nonfinishing-failure"} /\ ~effort.failureVisible
    /\ effort' = [effort EXCEPT !.phase = "failed", !.failed = TRUE, !.failureVisible = TRUE,
        !.wake = TRUE]
EffortRetryFailed ==
    /\ UNCHANGED vars
    /\ effort.phase = "failed" /\ effort.mode = "retry"
    /\ effort' = [effort EXCEPT !.phase = "booting", !.failed = FALSE]
\* A fresh binding nonce, unchanged daemon generation and same provider session/thread.
\* This summarizes verified rebind + writer ready, never process-spawn alone.
EffortResume ==
    /\ UNCHANGED vars
    /\ effort.phase = "booting" /\ effort.mode \notin {"failure", "nonfinishing-failure"}
    /\ (effort.mode # "retry" \/ effort.failureVisible)
    /\ effort' = [effort EXCEPT !.phase = "ready", !.binding = @ + 1, !.durable = TRUE,
        !.revision = effort.changes, !.watcher = "new", !.presence = "Idle",
        !.resumed = IF effort.pending > 0 THEN @ \cup {effort.pending} ELSE @,
        !.wake = effort.mode # "no-wake"]
\* Admission owns one durable synthetic ticket; the submit/receipt alone releases its barrier.
EffortContinue ==
    /\ UNCHANGED vars
    /\ effort.phase = "ready" /\ effort.active = "none" /\ effort.pending > 0 /\ effort.wake
    /\ effort.admittedC[effort.pending] = 0 /\ effort.mode # "no-continuation"
    /\ effort' = [effort EXCEPT !.admittedC[effort.pending] = 1, !.queuedC = effort.pending]
EffortSubmitContinuation ==
    /\ UNCHANGED vars
    /\ effort.phase = "ready" /\ effort.active = "none" /\ effort.pending > 0 /\ effort.wake
    /\ effort.queuedC = effort.pending
    \* C never resets the session; the reset-c mutant (S4b head) lets a pending reset take it.
    /\ effort' = [effort EXCEPT !.continuations[effort.pending] = @ + 1, !.pending = 0,
        !.barrier = 0, !.queuedC = 0,
        !.sameSession = IF effort.mode = "reset-c" /\ effort.resetPending THEN FALSE ELSE @,
        !.resetPending = IF effort.mode = "reset-c" THEN FALSE ELSE @,
        !.active = "continuation", !.row = TRUE, !.placeholder = TRUE, !.presence = "Busy"]
EffortAdmitUser ==
    /\ UNCHANGED vars
    /\ effort.phase \in {"ready", "kept"} /\ effort.active = "none" /\ effort.users # <<>> /\ effort.wake
    /\ effort.pending = 0 \/ effort.mode = "user-first"
    /\ LET i == Head(effort.users) IN
       effort' = [effort EXCEPT !.users = Tail(@), !.user = i, !.userCounts[i] = @ + 1,
           !.sessionResets = IF effort.resetPending THEN @ + 1 ELSE @, !.resetPending = FALSE,
           !.active = "user", !.row = TRUE, !.placeholder = TRUE, !.presence = "Busy",
           !.priorityBroken = @ \/ effort.pending > 0,
           !.barrierBroken = @ \/ effort.barrier > 0]
EffortFinish ==
    /\ UNCHANGED vars
    /\ (effort.active # "original" \/ ~EffortOriginalNeverFinishes)
    /\ effort.active # "none" /\ effort.phase \in {"running", "waiting", "waiting-herdr", "requested", "ready", "kept"}
    /\ LET task == IF effort.active = "original" THEN 0
                   ELSE IF effort.active = "continuation" THEN effort.root ELSE effort.user + 2
       IN effort' = [effort EXCEPT !.replies[task] = @ + 1, !.active = "none",
           !.row = FALSE, !.placeholder = FALSE, !.presence = "Idle", !.wake = TRUE]
EffortRepost ==
    /\ UNCHANGED vars
    /\ effort.mode = "repost" /\ effort.phase = "ready"
    /\ \E t \in EffortTasks : /\ effort.replies[t] = 1
                              /\ effort' = [effort EXCEPT !.replies[t] = 2]
\* A retained old body is delivered independently; resume never waits for this POST.
EffortDeliverOldDebt ==
    /\ UNCHANGED vars
    /\ effort.debt = 1
    /\ effort' = [effort EXCEPT !.debt = 0, !.debtPosts = @ + 1]
\* The interrupted turn's own completion arrives after the restart closed it. It names the old
\* turn, so it changes nothing; the mutant lets it settle whatever row the channel holds now.
EffortLateOldCompletion ==
    /\ UNCHANGED vars
    /\ effort.mode \in {"late", "late-clobber"} /\ effort.late = 0
    /\ effort.root > 0 /\ effort.phase \in {"stopped", "booting", "ready", "failed"}
    /\ effort' = IF effort.mode = "late-clobber"
                 THEN [effort EXCEPT !.late = 1, !.row = FALSE, !.placeholder = FALSE,
                                     !.presence = "Idle"]
                 ELSE [effort EXCEPT !.late = 1]
\* The server restarts while a restart is unfinished. The stored intent keeps its phase and its one
\* ticket and the owner is woken; the mutant starts the restart over with a new ticket.
\* crash/late-intent cut every step: before the kill the boot probe finds the old TUI alive and
\* cancels the intent; after it a durable intent resumes, a lost one (late-intent) owes nothing.
EffortDaemonRestart ==
    /\ UNCHANGED vars
    /\ effort.mode \in {"recover", "recover-twice", "crash", "late-intent"} /\ effort.crashes = 0
    /\ IF effort.mode \in {"crash", "late-intent"}
       THEN effort.phase \in {"prepared", "ready"} \cup EffortCutUnresolved
       ELSE effort.root > 0 /\ effort.phase \in {"stopped", "booting", "ready"}
    /\ effort' = IF effort.mode = "recover-twice"
                 THEN [effort EXCEPT !.crashes = 1, !.phase = "stopped", !.pending = effort.root,
                                     !.barrier = effort.root, !.queuedC = 0,
                                     !.admittedC[effort.root] = 0]
                 ELSE IF effort.phase = "prepared"
                 THEN [effort EXCEPT !.crashes = 1, !.phase = "kept", !.pending = 0, !.barrier = 0,
                                     !.cancelled = @ \cup {effort.pending}, !.wake = TRUE]
                 ELSE IF ~effort.durable
                 THEN [effort EXCEPT !.crashes = 1, !.pending = 0, !.barrier = 0, !.queuedC = 0]
                 ELSE [effort EXCEPT !.crashes = 1, !.wake = (effort.phase = "ready") \/ @]
\* Only a physically idle provider or an explicit failed restart is a normal rest state.
EffortRest ==
    /\ effort.phase = "failed"
       \/ (effort.phase \in {"running", "ready", "waiting", "waiting-herdr", "kept"} /\ effort.active = "none")
    /\ UNCHANGED allVars
EffortNext == EffortRequest \/ EffortSecondRequest \/ EffortInputArrives \/ EffortPrepare \/ EffortInterrupt
    \/ EffortKillFails \/ EffortClear \/ EffortNewerChange
    \/ EffortNoticeFails \/ EffortTellInterrupted \/ EffortDuplicateNotice \/ EffortCloseInterrupted
    \/ EffortNoActiveTurn \/ EffortCurrentHerdrApply \/ EffortCurrentNextTurn \/ EffortStopTui \/ EffortLaunchFails
    \/ EffortRetryFailed \/ EffortResume \/ EffortContinue \/ EffortSubmitContinuation \/ EffortAdmitUser \/ EffortFinish \/ EffortRepost \/ EffortDeliverOldDebt \/ EffortRest
    \/ EffortLateOldCompletion \/ EffortDaemonRestart
EffortFair ==
    /\ WF_allVars(EffortRequest)
    /\ WF_allVars(EffortPrepare)
    /\ WF_allVars(EffortInterrupt)
    /\ WF_allVars(EffortKillFails)
    /\ WF_allVars(EffortTellInterrupted)
    /\ WF_allVars(EffortCloseInterrupted)
    /\ WF_allVars(EffortNoActiveTurn)
    /\ WF_allVars(EffortCurrentHerdrApply)
    /\ WF_allVars(EffortCurrentNextTurn)
    /\ WF_allVars(EffortStopTui)
    /\ WF_allVars(EffortLaunchFails)
    /\ WF_allVars(EffortResume)
    /\ WF_allVars(EffortContinue)
    /\ WF_allVars(EffortSubmitContinuation)
    /\ WF_allVars(EffortAdmitUser)
    /\ WF_allVars(EffortFinish)
    /\ WF_allVars(EffortDeliverOldDebt)
EffortCurrentTmuxSpec == EffortInit("current-tmux") /\ [][EffortNext]_allVars /\ EffortFair
EffortCurrentHerdrSpec == EffortInit("current-herdr") /\ [][EffortNext]_allVars /\ EffortFair
EffortDesignSpec == EffortInit("design") /\ [][EffortNext]_allVars /\ EffortFair
EffortIdleSpec == EffortInit("idle") /\ [][EffortNext]_allVars /\ EffortFair
EffortRetrySpec == EffortInit("retry") /\ [][EffortNext]_allVars /\ EffortFair
EffortFailureSpec == EffortInit("failure") /\ [][EffortNext]_allVars /\ EffortFair
EffortCoalesceSpec == EffortInit("coalesce") /\ [][EffortNext]_allVars /\ EffortFair
EffortSecondSpec == EffortInit("second") /\ [][EffortNext]_allVars /\ EffortFair
EffortNoWakeSpec == EffortInit("no-wake") /\ [][EffortNext]_allVars /\ EffortFair
EffortNoContinuationSpec == EffortInit("no-continuation") /\ [][EffortNext]_allVars /\ EffortFair
EffortUserFirstSpec == EffortInit("user-first") /\ [][EffortNext]_allVars /\ EffortFair
EffortDropQueueSpec == EffortInit("drop-queue") /\ [][EffortNext]_allVars /\ EffortFair
EffortNoNoticeSpec == EffortInit("no-notice") /\ [][EffortNext]_allVars /\ EffortFair
EffortDuplicateNoticeSpec == EffortInit("notice-twice") /\ [][EffortNext]_allVars /\ EffortFair
EffortRepostSpec == EffortInit("repost") /\ [][EffortNext]_allVars /\ EffortFair
EffortNonFinishingSpec == EffortInit("nonfinishing") /\ [][EffortNext]_allVars /\ EffortFair
EffortNonFinishingFailureSpec == EffortInit("nonfinishing-failure") /\ [][EffortNext]_allVars /\ EffortFair
EffortNoInterruptSpec == EffortInit("no-interrupt") /\ [][EffortNext]_allVars /\ EffortFair
EffortNoCloseSpec == EffortInit("no-close") /\ [][EffortNext]_allVars /\ EffortFair
EffortLateSpec == EffortInit("late") /\ [][EffortNext]_allVars /\ EffortFair
EffortLateClobberSpec == EffortInit("late-clobber") /\ [][EffortNext]_allVars /\ EffortFair
EffortRecoverSpec == EffortInit("recover") /\ [][EffortNext]_allVars /\ EffortFair
EffortRecoverTwiceSpec == EffortInit("recover-twice") /\ [][EffortNext]_allVars /\ EffortFair
EffortClearSpec == EffortInit("clear") /\ [][EffortNext]_allVars /\ EffortFair
EffortCancelCSpec == EffortInit("cancel-c") /\ [][EffortNext]_allVars /\ EffortFair
EffortClearKeepsSpec == EffortInit("clear-keeps") /\ [][EffortNext]_allVars /\ EffortFair
EffortCrashSpec == EffortInit("crash") /\ [][EffortNext]_allVars /\ EffortFair
EffortLateIntentSpec == EffortInit("late-intent") /\ [][EffortNext]_allVars /\ EffortFair
EffortKillFailsSpec == EffortInit("kill-fails") /\ [][EffortNext]_allVars /\ EffortFair
EffortKillFailsKeepsSpec == EffortInit("kill-fails-keeps") /\ [][EffortNext]_allVars /\ EffortFair
EffortResetSpec == EffortInit("reset") /\ [][EffortNext]_allVars /\ EffortFair
EffortResetKeptSpec == EffortInit("reset-kept") /\ [][EffortNext]_allVars /\ EffortFair
EffortResetCSpec == EffortInit("reset-c") /\ [][EffortNext]_allVars /\ EffortFair
EffortResetNewerSpec == EffortInit("reset-newer") /\ [][EffortNext]_allVars /\ EffortFair
\* One request restarts the TUI at most once; a request that resolved ready restarted it once.
EffortOneRestartPerRequest == effort.binding <= effort.changes
\* A running successor keeps its row, placeholder and presence whatever arrives for the old turn.
EffortLateKeepsSuccessor == effort.active \in {"continuation", "user"}
    => effort.row /\ effort.placeholder /\ effort.presence = "Busy"
EffortReplyOnce == (\A t \in EffortTasks : effort.replies[t] <= 1) /\ effort.debtPosts <= 1
EffortOldDebtNotLost == effort.debt = 1 \/ effort.debtPosts = 1
EffortOldDebtEventuallyPosted == (effort.debt = 1) ~> (effort.debtPosts = 1)
EffortInputNotLost == \A i \in 1..effort.arrived : i \in {effort.users[j] : j \in 1..Len(effort.users)}
                       \/ effort.userCounts[i] = 1 \/ i \in effort.discarded
EffortInputOnce == \A i \in EffortPeople : effort.userCounts[i] <= 1
EffortUserFifo == effort.userCounts[2] = 1 => effort.userCounts[1] = 1 \/ 1 \in effort.discarded
EffortInterruptedNoticeExactlyOnce ==
    /\ \A r \in EffortRoots : effort.notices[r] <= 1
    /\ \A r \in 1..effort.root : effort.noticeIntents[r] = 1
    /\ \A r \in effort.noticeSettled : effort.notices[r] = 1
EffortContinuationAtMostOnce == \A r \in EffortRoots : effort.continuations[r] <= 1
EffortContinuationAuthorized == \A r \in EffortRoots : effort.continuations[r] > 0
                                => r \in effort.closed /\ r \in effort.resumed
EffortContinuationFirst == ~effort.priorityBroken
EffortSameSession == effort.sameSession /\ effort.daemonGeneration = 0
EffortFailedKeepsCustody == effort.failed => effort.failureVisible
                          /\ effort.resumed = {} /\ (\A r \in EffortRoots : effort.continuations[r] = 0)
EffortInterruptedEventuallyTold == \A r \in EffortRoots : (effort.root = r) ~> (effort.notices[r] = 1)
EffortContinuationEventuallyInjected == \A r \in EffortRoots :
    (r \in effort.resumed) ~> (effort.continuations[r] = 1 \/ r \in effort.cancelled)
EffortPendingEventuallyProcessed == \A i \in EffortPeople :
    (effort.phase \in {"ready", "kept"} /\ i <= effort.arrived)
        ~> (effort.userCounts[i] = 1 \/ i \in effort.discarded)
EffortFailureVisible == (effort.phase = "booting" /\ effort.mode \in {"failure", "retry", "nonfinishing-failure"} /\ ~effort.failureVisible) ~> effort.failureVisible

EffortRequestResolves == (effort.changes > 0) ~> (effort.phase \in {"ready", "failed", "kept"})

EffortContinuationCustody == \A r \in effort.resumed :
    effort.continuations[r] = 1
    \/ r \in effort.cancelled
    \/ (effort.pending = r /\ effort.barrier = r
        /\ (effort.queuedC = r \/ effort.admittedC[r] = 0))
EffortContinuationAdmittedOnce == \A r \in EffortRoots : effort.admittedC[r] <= 1
EffortNoUserBeforeBarrier == ~effort.barrierBroken
EffortSubmitReleasesBarrier == \A r \in EffortRoots :
    (r \in effort.resumed) ~> ((effort.continuations[r] = 1 \/ r \in effort.cancelled) /\ effort.barrier # r)
\* Every killed turn ends with its one continuation or an explicit cancel; a crash never loses it.
EffortCutEventuallyResolved == \A r \in EffortRoots :
    (effort.root >= r) ~> (effort.continuations[r] = 1 \/ r \in effort.cancelled)
EffortBarrierEventuallyReleased == \A r \in EffortRoots : (effort.barrier = r) ~> (effort.barrier # r)
\* A pending /fast or /model reset the restart's launch already reflects never resets the session
\* again; only a change saved after that launch's cut may.
EffortNoSecondRestart == effort.sessionResets <= effort.newerChanges

====
