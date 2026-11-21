---- MODULE EffortHerdrResume ----
\* #8109 S5 (reduced): `/effort` on a Herdr channel, on top of the S4 effort-restart model
\* (TmuxSession.tla EffortRestart*). A Codex pane ends its exact execution and the next launch from
\* that retired row resumes the same thread with the saved level; a Claude pane takes the level in
\* place and never restarts. One request, one possibly running turn A, users U arriving later.
\* Code coordinates (head of feat/i8109-s5n):
\*   H  = src/services/discord/commands/effort_restart/herdr.rs
\*   RI = src/services/codex/herdr_turn/restart_intent.rs
\*   HT = src/services/codex/herdr_turn.rs
\*   RC = src/services/discord/commands/effort_restart/recovery.rs
\*   CL = src/services/turn_orchestrator/queued_head/v2/clear.rs
\*   TP = src/services/discord/commands/tui_passthrough.rs
\* Value ranges come only from the entry census in the S5 report (row states Bound/Pending/Retired,
\* turn none/running, O cursor naming the ended thread or another/none).
EXTENDS Naturals

CONSTANTS
    Provider,   \* "codex" | "claude"
    StartRow,   \* "bound" | "pending" | "retired": the row /effort finds (H:91-99)
    TurnRuns,   \* BOOLEAN: a turn A runs on the old pane when /effort arrives
    Cursor,     \* "T" | "other": the live O cursor's thread at launch (RI:80-92)
    Mut         \* "none" | "save_first" | "any_thread" | "recover_ungated" | "clear_keeps" | "claude_restart"

VARIABLES
    cmd,       \* "idle" | "gated" | "prepared" | "done" | "refused"
    row,       \* "bound_old" | "pending_old" | "retired_old" | "bound_new"
    gate,      \* the restart gate excludes owner delivery (H:146, released H:235)
    saved,     \* channel setting: "old" | "new"
    paneLevel, \* level the live old pane was launched with or applied: "old" | "new"
    intent,    \* "none" | "idle" | "withC" | "cancelled"
    c,         \* the continuation: "none" | "pending" | "admitted" | "submitted"
    turn,      \* "none" | "running" | "cut" | "done"
    launched,  \* thread of the new execution: "none" | "T" | "other" | "fresh"
    newLevel,  \* level the new execution launched with
    rejected,  \* a prompt hit launch options its pane was not launched with (HT:418)
    stuck,     \* a launch refused for good (no recovery path)
    cleared,   \* the user cleared the channel
    users      \* users waiting: 0..1

vars == <<cmd, row, gate, saved, paneLevel, intent, c, turn, launched, newLevel, rejected,
          stuck, cleared, users>>

Init ==
    /\ cmd = "idle"
    /\ row = CASE StartRow = "bound" -> "bound_old"
               [] StartRow = "pending" -> "pending_old"
               [] OTHER -> "retired_old"
    /\ gate = FALSE /\ saved = "old" /\ paneLevel = "old"
    /\ intent = "none" /\ c = "none"
    /\ turn = IF TurnRuns /\ StartRow = "bound" THEN "running" ELSE "none"
    /\ launched = "none" /\ newLevel = "none" /\ rejected = FALSE /\ stuck = FALSE
    /\ cleared = FALSE /\ users = 0

\* TP:94 routes Codex to H; TP:99 keeps the Claude ready-pane overlay (herdr_settings.rs:236).
ClaudeApply ==
    /\ Provider = "claude" /\ cmd = "idle" /\ Mut # "claude_restart"
    /\ cmd' = "done" /\ saved' = "new" /\ paneLevel' = "new"
    /\ UNCHANGED <<row, gate, intent, c, turn, launched, newLevel, rejected, stuck, cleared, users>>

\* H:91-99: a starting execution is refused with its reason; a retired/vacant row only saves.
CodexRequest ==
    /\ (Provider = "codex" \/ Mut = "claude_restart") /\ cmd = "idle"
    /\ CASE row = "pending_old" ->
              cmd' = "refused" /\ UNCHANGED <<gate, saved>>
         [] row = "retired_old" ->
              cmd' = "done" /\ saved' = "new" /\ UNCHANGED gate
         [] OTHER ->
              cmd' = "gated" /\ gate' = TRUE /\ UNCHANGED saved
    /\ UNCHANGED <<row, paneLevel, intent, c, turn, launched, newLevel, rejected, stuck, cleared, users>>

\* H:154-204: expected row, live thread, captured turn, then the intent before anything ends.
Prepare ==
    /\ cmd = "gated"
    /\ cmd' = "prepared"
    /\ intent' = IF turn = "running" THEN "withC" ELSE "idle"
    /\ c' = IF turn = "running" THEN "pending" ELSE "none"
    /\ saved' = IF Mut = "save_first" THEN "new" ELSE saved
    /\ UNCHANGED <<row, gate, paneLevel, turn, launched, newLevel, rejected, stuck, cleared, users>>

\* H:219 exact terminate; postlude H:121-133 saves under the held exclusion; H:235 releases the gate.
TerminateOk ==
    /\ cmd = "prepared"
    /\ row' = "retired_old" /\ saved' = "new" /\ gate' = FALSE /\ cmd' = "done"
    /\ turn' = IF turn = "running" THEN "cut" ELSE turn
    /\ UNCHANGED <<paneLevel, intent, c, launched, newLevel, rejected, stuck, cleared, users>>

\* H:229: nothing ended, the turn keeps running and owes nothing; the level stays unsaved.
TerminateFails ==
    /\ cmd = "prepared"
    /\ cmd' = "refused" /\ gate' = FALSE
    /\ intent' = "cancelled" /\ c' = IF c = "pending" THEN "none" ELSE c
    /\ UNCHANGED <<row, saved, paneLevel, turn, launched, newLevel, rejected, stuck, cleared, users>>

\* H:243 -> tmux.rs advance: the cut turn's one continuation is admitted at the front.
Admit ==
    /\ cmd = "done" /\ c = "pending" /\ intent = "withC" /\ row = "retired_old"
    /\ c' = "admitted"
    /\ UNCHANGED <<cmd, row, gate, saved, paneLevel, intent, turn, launched, newLevel, rejected, stuck, cleared, users>>

\* A turn that runs its course outside a restart's capture window (natural completion).
TurnEnds ==
    /\ turn = "running" /\ cmd \notin {"gated", "prepared"}
    /\ turn' = "done"
    /\ UNCHANGED <<cmd, row, gate, saved, paneLevel, intent, c, launched, newLevel, rejected, stuck, cleared, users>>

\* Users arrive once the one /effort request is in (the census covers inputs during and after it).
UserArrives ==
    /\ users = 0 /\ cmd # "idle"
    /\ users' = 1
    /\ UNCHANGED <<cmd, row, gate, saved, paneLevel, intent, c, turn, launched, newLevel, rejected, stuck, cleared>>

\* A user prompt on the live old pane (owner delivery waits while gated). The pane takes it only
\* under the options it was launched with (HT:418); a cut continuation's barrier holds users back.
Followup ==
    /\ users = 1 /\ ~gate /\ row \in {"bound_old", "pending_old", "bound_new"} /\ turn # "running"
    /\ users' = 0
    /\ rejected' = (rejected \/ saved # IF row = "bound_new" THEN newLevel ELSE paneLevel)
    /\ UNCHANGED <<cmd, row, gate, saved, paneLevel, intent, c, turn, launched, newLevel, stuck, cleared>>

ResumeOwed == intent \in {"idle", "withC"}

\* HT:212,260 -> RI:61-75: a launch from the retired row resumes the ended thread; a cursor naming
\* another thread is refused, never replaced by a fresh launch while the resume is owed.
Launch ==
    /\ row = "retired_old" /\ ~gate /\ ~stuck
    /\ (c = "admitted" \/ (users = 1 /\ c \notin {"pending", "admitted"}))
    /\ LET cur == IF cleared THEN "none" ELSE Cursor IN
       IF ~ResumeOwed
       THEN /\ launched' = "fresh" /\ row' = "bound_new" /\ newLevel' = saved
            /\ UNCHANGED stuck
       ELSE IF cur = "T" \/ (Mut = "any_thread" /\ cur = "other")
       THEN /\ launched' = cur /\ row' = "bound_new" /\ newLevel' = saved
            /\ UNCHANGED stuck
       ELSE /\ stuck' = TRUE /\ UNCHANGED <<launched, row, newLevel>>
    /\ c' = IF c = "admitted" /\ ~stuck' THEN "submitted" ELSE c
    /\ users' = IF c = "admitted" \/ stuck' THEN users ELSE 0
    /\ UNCHANGED <<cmd, gate, saved, paneLevel, intent, turn, rejected, cleared>>

\* RC:50-58: a prepared Herdr intent is cancelled only once its row shows the execution alive, and
\* never while the restart that owns it holds the gate.
Recover ==
    /\ intent = "withC" /\ c = "pending" /\ row = "bound_old"
    /\ (~gate \/ Mut = "recover_ungated")
    /\ intent' = "cancelled" /\ c' = "none"
    /\ UNCHANGED <<cmd, row, gate, saved, paneLevel, turn, launched, newLevel, rejected, stuck, cleared, users>>

\* CL:221: an owner clear closes an idle resume; its thread's cursor is retired by the clear.
Clear ==
    /\ ~cleared /\ ~gate /\ cmd # "idle" /\ c \notin {"pending", "admitted"}
    /\ cleared' = TRUE
    /\ intent' = IF intent = "idle" /\ Mut # "clear_keeps" THEN "cancelled" ELSE intent
    /\ UNCHANGED <<cmd, row, gate, saved, paneLevel, c, turn, launched, newLevel, rejected, stuck, users>>

\* Nothing left to do once the request settled and no input waits: a quiet end, not a deadlock.
Quiet ==
    /\ cmd \in {"done", "refused"}
    /\ UNCHANGED vars

Next == Quiet \/ ClaudeApply \/ CodexRequest \/ Prepare \/ TerminateOk \/ TerminateFails \/ Admit
    \/ TurnEnds \/ UserArrives \/ Followup \/ Launch \/ Recover \/ Clear

Spec == Init /\ [][Next]_vars
    /\ WF_vars(ClaudeApply) /\ WF_vars(CodexRequest) /\ WF_vars(Prepare)
    /\ WF_vars(TerminateOk) /\ WF_vars(Admit) /\ WF_vars(Launch) /\ WF_vars(Followup)
    /\ WF_vars(TurnEnds)

\* Never another thread under an owed resume; a fresh session only after an explicit clear or
\* where no effort restart ended anything.
SameThread == launched \in {"none", "T", "fresh"}
            /\ (launched = "fresh" => (cleared \/ cmd = "refused" \/ StartRow # "bound"))
\* The new execution runs the saved level.
LaunchedSaved == launched # "none" => newLevel = saved
\* No prompt is refused for options its pane was not launched with.
NoOptionsDrift == ~rejected
\* A Claude pane is never restarted.
ClaudeNoRestart == Provider = "claude" => row \in {"bound_old", "pending_old", "retired_old"} /\ ~gate
\* A continuation exists only for a turn the restart actually cut, at most once.
ContinuationOnlyForCut == c # "none" => turn \in {"running", "cut"}

\* Liveness: the request ends applied or refused with a reason; a cut turn's continuation runs;
\* a waiting user is eventually served.
RequestSettles == <>(cmd \in {"done", "refused"})
CutContinues == (turn = "cut") ~> (c = "submitted")
UserServed == (users = 1) ~> (users = 0)
====
