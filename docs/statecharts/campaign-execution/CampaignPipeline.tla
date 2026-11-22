--------------------------- MODULE CampaignPipeline ---------------------------
\* #8249 P0: one job's free-edit pipeline instance and the shared save/control boundary.
\* Graph: work --(p10 retry, budget q)--> work (self-loop), work --(p20)--> review|done,
\*        review --(p10 repair, budget q)--> work, review --(p20, gate land)--> done,
\*        every live state --(p0 timeout)--> failed; an exhausted retry goes to failed.
\* An edit may delete the unoccupied review state; a run uses its pinned graph until it adopts.
\* Bug selects one injected defect ("none" is the design).
EXTENDS Naturals, FiniteSets
CONSTANTS Jobs, Workers, MaxBudget, MaxCrash, MaxStops, MaxEdits, Gates, LateEvents, Bug

VARIABLES control, mode, fence, savedReview, activeReview, check, edits,
          state, visit, facts, used, eff, grant, entry, appr, late,
          cmd, mem, crashes, stops, ghost, editor
store == <<control, mode, fence, savedReview, activeReview, check, edits>>
job == <<state, visit, facts, used, eff, grant, entry, appr, late>>
vars == <<store, job, cmd, mem, crashes, stops, ghost>>

Terminal == {"done", "failed"}
Busy == {"intent", "launching", "started", "unknown"}
States(review) == IF review THEN {"work", "review", "done", "failed"} ELSE {"work", "done", "failed"}
Fresh == [a |-> "U", b |-> "U", t |-> FALSE, old |-> FALSE]
NoEff == [st |-> "none", f |-> 0, n |-> 0]
Intent(f) == [st |-> "intent", f |-> f, n |-> 0]
NoCmd == [j |-> "-", v |-> 0, s |-> "-", ch |-> "-", ctl |-> "-"]
Inc(v) == (v + 1) % 3
Min(a, b) == IF a < b THEN a ELSE b
Live(j) == state[j] \notin Terminal \cup {"none"}
Quiet == \A j \in Jobs : eff[j].st \notin Busy
\* Approvals are kept for the current and the previous visit only (no aliasing of visit ids).
Keep(j, v) == {x \in appr : x[1] # j \/ x[3] = v}

\* ---- Pure evaluator: priority order, Unknown waits, first True must be approved ----
GateOf(j, ch) == CASE ch = "retry" -> "impl"
                   [] ch = "next" /\ state[j] = "review" -> "land"
                   [] OTHER -> "-"
Exact(j, g) == g \notin Gates \/ <<j, g, visit[j]>> \in appr
Approved(j, g) == IF Bug = "approvalByGate"
                  THEN g \notin Gates \/ \E v \in 0..2 : <<j, g, v>> \in appr
                  ELSE Exact(j, g)
Raw(j) == CASE facts[j].t -> "timeout"
            [] facts[j].a = "T" -> "retry"
            [] facts[j].a = "U" /\ Bug # "skipUnknown" -> "wait"
            [] facts[j].b = "T" -> "next"
            [] OTHER -> "wait"
Choice(j) == IF Raw(j) = "wait" \/ ~Approved(j, GateOf(j, Raw(j))) THEN "wait" ELSE Raw(j)
Decidable(j) == Live(j) /\ eff[j].st = "done" /\ Choice(j) # "wait"
NextOf(j) == IF state[j] = "work" /\ activeReview THEN "review" ELSE "done"
Counted(j) == ~(Bug = "outerUncounted" /\ state[j] = "review")

EditorInitial == [control |-> "prepared", revision |-> 1, valid |-> TRUE,
              review |-> TRUE, stage |-> "none", validation |-> "none",
              savedFrom |-> "prepared", cas |-> TRUE,
              deletedOccupied |-> FALSE, migrated |-> FALSE]

Init ==
    /\ editor = EditorInitial
    /\ control = "prepared" /\ mode = "none" /\ fence = 0
    /\ savedReview = TRUE /\ activeReview = TRUE /\ check = "none" /\ edits = 0
    /\ state = [j \in Jobs |-> "none"] /\ visit = [j \in Jobs |-> 0]
    /\ facts = [j \in Jobs |-> Fresh] /\ used = [j \in Jobs |-> 0]
    /\ eff = [j \in Jobs |-> NoEff] /\ grant = [j \in Jobs |-> "none"]
    /\ entry = [j \in Jobs |-> FALSE] /\ appr = {} /\ late = [j \in Jobs |-> 0]
    /\ cmd = [w \in Workers |-> NoCmd] /\ mem = [w \in Workers |-> "-"]
    /\ crashes = 0 /\ stops = 0
    /\ ghost = [dup |-> FALSE, prio |-> FALSE, appr |-> FALSE, cond |-> FALSE,
                intent |-> FALSE, launch |-> FALSE, reentry |-> FALSE, finish |-> FALSE]

\* ---- Save / Start ----
CanDelete == \A j \in Jobs : state[j] # "review"
Validate ==
    /\ control \in {"prepared", "stopped"} /\ edits < MaxEdits /\ check = "none"
    /\ check' = IF CanDelete THEN "ok" ELSE "no"
    /\ UNCHANGED <<control, mode, fence, savedReview, activeReview, edits, job, cmd, mem, crashes, stops, ghost>>
\* PUT re-validates under the campaign lock; the design never trusts an earlier validate.
SaveDelete ==
    /\ control \in {"prepared", "stopped"} /\ Quiet /\ edits < MaxEdits /\ savedReview
    /\ IF Bug = "saveTrustsValidate" THEN check = "ok" ELSE CanDelete
    /\ savedReview' = FALSE /\ edits' = edits + 1
    /\ UNCHANGED <<control, mode, fence, activeReview, check, job, cmd, mem, crashes, stops, ghost>>

BeginRun ==
    /\ control = "prepared" /\ control' = "running" /\ activeReview' = savedReview
    /\ UNCHANGED <<mode, fence, savedReview, check, edits, job, cmd, mem, crashes, stops, ghost>>
\* Prepared jobs carry state_id = null; the real Start assigns the start of the pinned graph.
StartJob(j) ==
    /\ control = "running" /\ state[j] = "none"
    /\ state' = [state EXCEPT ![j] = "work"] /\ eff' = [eff EXCEPT ![j] = Intent(fence)]
    /\ UNCHANGED <<store, visit, facts, used, grant, entry, appr, late, cmd, mem, crashes, stops, ghost>>

\* ---- Environment: approvals, facts, timer, late events ----
Approve(j, g) ==
    /\ control = "running" /\ Live(j) /\ g \in Gates /\ <<j, g, visit[j]>> \notin appr
    /\ g \in (IF state[j] = "work" THEN {"impl"} ELSE {"impl", "land"})
    /\ appr' = appr \cup {<<j, g, visit[j]>>}
    /\ UNCHANGED <<store, state, visit, facts, used, eff, grant, entry, late, cmd, mem, crashes, stops, ghost>>
Observe(j) ==
    /\ Live(j) /\ eff[j].st = "done"
    /\ \E x \in {"a", "b"}, val \in {"T", "F"} :
         /\ facts[j][x] = "U" /\ facts' = [facts EXCEPT ![j][x] = val]
    /\ UNCHANGED <<store, state, visit, used, eff, grant, entry, appr, late, cmd, mem, crashes, stops, ghost>>
\* No valid exit: the timeout transition becomes the failure exit (progress premise).
TimerFire(j) ==
    /\ Live(j) /\ eff[j].st = "done" /\ facts[j].a = "F" /\ facts[j].b = "F" /\ ~facts[j].t
    /\ facts' = [facts EXCEPT ![j].t = TRUE]
    /\ UNCHANGED <<store, state, visit, used, eff, grant, entry, appr, late, cmd, mem, crashes, stops, ghost>>
\* A late event of an earlier visit only reconciles its own record.
LateEvent(j) ==
    /\ LateEvents /\ late[j] = 1 /\ Live(j) /\ facts[j].b = "U"
    /\ late' = [late EXCEPT ![j] = 2]
    /\ facts' = IF Bug = "lateCounts" THEN [facts EXCEPT ![j].b = "T", ![j].old = TRUE] ELSE facts
    /\ UNCHANGED <<store, state, visit, used, eff, grant, entry, appr, cmd, mem, crashes, stops, ghost>>

\* ---- Worker: compute outside the lock, commit once per visit inside it ----
Compute(w, j) ==
    /\ cmd[w] = NoCmd /\ control \in {"running", "stopping"} /\ Decidable(j)
    /\ cmd' = [cmd EXCEPT ![w] = [j |-> j, v |-> visit[j], s |-> state[j], ch |-> Choice(j), ctl |-> control]]
    /\ UNCHANGED <<store, job, mem, crashes, stops, ghost>>

CommitOk(c) ==
    /\ IF Bug = "visitUnguarded" THEN c.s = state[c.j] /\ Live(c.j)
       ELSE c.v = visit[c.j] /\ Decidable(c.j) /\ Choice(c.j) = c.ch
    /\ IF Bug = "stopStaleRead" THEN c.ctl = "running"
       ELSE control = "running" \/ (control = "stopping" /\ mode = "finish")

Commit(w) ==
    LET c == cmd[w]
        j == c.j
        ch == c.ch
        retryTo == ~Counted(j) \/ used[j] < MaxBudget \/ Bug = "exhaustIgnored"
        tgt == CASE ch = "timeout" -> "failed"
                 [] ch = "retry" -> IF retryTo THEN "work" ELSE "failed"
                 [] OTHER -> NextOf(j)
        \* A new entry intent is written in the same commit, and only while running.
        open == tgt \notin Terminal /\
                (control = "running" \/ Bug \in {"finishAllowsAll", "stopStaleRead"})
        nv == Inc(visit[j])
    IN
    /\ c # NoCmd /\ CommitOk(c)
    /\ state' = [state EXCEPT ![j] = tgt] /\ visit' = [visit EXCEPT ![j] = nv]
    /\ facts' = [facts EXCEPT ![j] = Fresh]
    /\ used' = [used EXCEPT ![j] = IF ch = "retry" /\ Counted(j) /\ retryTo
                                   THEN Min(@ + 1, MaxBudget + 1) ELSE @]
    /\ eff' = [eff EXCEPT ![j] = CASE tgt \in Terminal -> @
                                   [] open /\ Bug # "intentLost" -> Intent(fence)
                                   [] OTHER -> NoEff]
    /\ mem' = IF open /\ Bug = "intentLost" THEN [mem EXCEPT ![w] = j] ELSE mem
    /\ entry' = [entry EXCEPT ![j] = tgt \notin Terminal /\ ~open]
    /\ grant' = [grant EXCEPT ![j] = "none"]
    /\ appr' = Keep(j, visit[j])
    /\ late' = [late EXCEPT ![j] = IF @ = 0 THEN 1 ELSE @]
    /\ cmd' = [cmd EXCEPT ![w] = NoCmd]
    /\ ghost' = [ghost EXCEPT
         !.dup = @ \/ c.v # visit[j],
         !.prio = @ \/ (ch = "next" /\ facts[j].a = "U"),
         !.appr = @ \/ ~Exact(j, GateOf(j, ch)),
         !.cond = @ \/ (ch = "next" /\ facts[j].old),
         !.intent = @ \/ (open /\ control # "running")]
    /\ UNCHANGED <<store, crashes, stops>>

Drop(w) ==
    /\ cmd[w] # NoCmd /\ ~CommitOk(cmd[w]) /\ cmd' = [cmd EXCEPT ![w] = NoCmd]
    /\ UNCHANGED <<store, job, mem, crashes, stops, ghost>>

\* Defect only: the entry intent is written after the transition commit.
WriteIntent(w) ==
    /\ mem[w] # "-"
    /\ eff' = [eff EXCEPT ![mem[w]] = Intent(fence)] /\ mem' = [mem EXCEPT ![w] = "-"]
    /\ UNCHANGED <<store, state, visit, facts, used, grant, entry, appr, late, cmd, crashes, stops, ghost>>

\* ---- Host gate and effects ----
Granted(j) == control = "stopping" /\ mode = "finish" /\ grant[j] \in {"frozen", "provisional"}
GateOpen(j) ==
    \/ control = "running" /\ eff[j].f = fence
    \/ Granted(j) /\ Bug # "stopBlocksAll"
    \/ control = "stopping" /\ mode = "finish" /\ Bug = "finishAllowsAll"
EffUnchanged == UNCHANGED <<store, state, visit, facts, used, entry, appr, late, cmd, mem, crashes, stops>>

HostLaunch(j) ==
    /\ eff[j].st = "intent" /\ GateOpen(j)
    /\ eff' = [eff EXCEPT ![j].st = "launching", ![j].n = @ + 1]
    /\ ghost' = [ghost EXCEPT !.launch = @ \/ (control # "running" /\ ~Granted(j))]
    /\ UNCHANGED grant /\ EffUnchanged
HostCancel(j) ==
    /\ eff[j].st = "intent" /\ control # "running" /\ ~GateOpen(j)
    /\ eff' = [eff EXCEPT ![j].st = "cancelled"]
    /\ ghost' = [ghost EXCEPT !.finish = @ \/ (mode = "finish" /\ grant[j] = "frozen")]
    /\ UNCHANGED grant /\ EffUnchanged
\* The started receipt confirms a provisional grant.
Receipt(j) ==
    /\ eff[j].st = "launching" /\ eff' = [eff EXCEPT ![j].st = "started"]
    /\ grant' = [grant EXCEPT ![j] = IF @ = "provisional" THEN "frozen" ELSE @]
    /\ UNCHANGED ghost /\ EffUnchanged
\* The host fence arrived before the initiation: not sent, the provisional grant is withdrawn.
FenceFirst(j) ==
    /\ eff[j].st = "launching" /\ control = "stopping" /\ grant[j] # "frozen"
    /\ eff' = [eff EXCEPT ![j].st = "cancelled"] /\ grant' = [grant EXCEPT ![j] = "none"]
    /\ UNCHANGED ghost /\ EffUnchanged
Interrupt(j) ==
    /\ eff[j].st = "started" /\ control = "stopping" /\ mode = "immediate"
    /\ eff' = [eff EXCEPT ![j].st = "cancelled"]
    /\ UNCHANGED <<grant, ghost>> /\ EffUnchanged
Finish(j) ==
    /\ eff[j].st = "started" /\ eff' = [eff EXCEPT ![j].st = "done"]
    /\ UNCHANGED <<grant, ghost>> /\ EffUnchanged
\* A lost receipt is Unknown and is reconciled from observation, never re-initiated.
Reconcile(j) ==
    /\ eff[j].st = "unknown"
    /\ eff' = IF Bug = "reinitiate" THEN [eff EXCEPT ![j].st = "launching", ![j].n = @ + 1]
              ELSE [eff EXCEPT ![j].st = "started"]
    /\ UNCHANGED <<grant, ghost>> /\ EffUnchanged

Crash ==
    /\ crashes < MaxCrash /\ crashes' = crashes + 1
    /\ cmd' = [w \in Workers |-> NoCmd] /\ mem' = [w \in Workers |-> "-"]
    /\ eff' = [j \in Jobs |-> IF eff[j].st = "launching" THEN [eff[j] EXCEPT !.st = "unknown"] ELSE eff[j]]
    /\ UNCHANGED <<store, state, visit, facts, used, grant, entry, appr, late, stops, ghost>>

\* ---- Stop: a new intent is barred from the RequestStop commit; in-flight effects by the host fence ----
FreezeGrant(j) == CASE eff[j].st \in {"intent", "started"} -> "frozen"
                    [] eff[j].st \in {"launching", "unknown"} -> "provisional"
                    [] OTHER -> "none"
RequestStop(m) ==
    /\ \/ control = "running" /\ stops < MaxStops
       \/ control = "stopping" /\ mode = "finish" /\ m = "immediate"   \* Immediate strengthening
    /\ control' = "stopping" /\ mode' = m /\ fence' = fence + 1
    /\ grant' = [j \in Jobs |-> IF m = "finish" THEN FreezeGrant(j) ELSE "none"]
    /\ stops' = IF control = "running" THEN stops + 1 ELSE stops
    /\ UNCHANGED <<savedReview, activeReview, check, edits, state, visit, facts, used, eff, entry,
                    appr, late, cmd, mem, crashes, ghost>>
Settle ==
    /\ control = "stopping" /\ Quiet /\ \A w \in Workers : mem[w] = "-"
    /\ control' = "stopped" /\ grant' = [j \in Jobs |-> "none"]
    /\ UNCHANGED <<mode, fence, savedReview, activeReview, check, edits, state, visit, facts, used,
                    eff, entry, appr, late, cmd, mem, crashes, stops, ghost>>

\* Explicit recovery opens a new visit at the Work anchor; adoption alone keeps the visit.
Resume(adopt) ==
    /\ control = "stopped" /\ Quiet
    /\ IF adopt THEN savedReview # activeReview /\ \A j \in Jobs : ~Live(j) \/ state[j] \in States(savedReview)
       ELSE savedReview = activeReview
    /\ LET nf == fence + 1
           Recover(j) == Live(j) /\ eff[j].st = "cancelled"
           Enter(j) == Live(j) /\ ~Recover(j) /\ entry[j]
           Reenter(j) == adopt /\ Bug = "adoptReenters" /\ Live(j) /\ ~Recover(j) /\ ~Enter(j)
           Moves(j) == Recover(j) \/ Reenter(j)
       IN
       /\ control' = "running" /\ mode' = "none" /\ fence' = nf
       /\ activeReview' = IF adopt THEN savedReview ELSE activeReview
       /\ state' = [j \in Jobs |-> IF Recover(j) THEN "work" ELSE state[j]]
       /\ visit' = [j \in Jobs |-> IF Moves(j) THEN Inc(visit[j]) ELSE visit[j]]
       /\ facts' = [j \in Jobs |-> IF Moves(j) THEN Fresh ELSE facts[j]]
       /\ eff' = [j \in Jobs |-> IF Moves(j) \/ Enter(j) THEN Intent(nf) ELSE eff[j]]
       /\ entry' = [j \in Jobs |-> IF Moves(j) \/ Enter(j) THEN FALSE ELSE entry[j]]
       /\ appr' = IF adopt THEN {} ELSE {x \in appr : ~Moves(x[1]) \/ x[3] = visit[x[1]]}
       /\ ghost' = [ghost EXCEPT !.reentry = @ \/ \E j \in Jobs : Reenter(j)]
    /\ UNCHANGED <<savedReview, check, edits, used, grant, late, cmd, mem, crashes, stops>>

Done == UNCHANGED vars
RunNext ==
    \/ Validate \/ SaveDelete \/ BeginRun \/ Crash \/ Settle \/ Done
    \/ \E m \in {"immediate", "finish"} : RequestStop(m)
    \/ \E a \in BOOLEAN : Resume(a)
    \/ \E w \in Workers : Commit(w) \/ Drop(w) \/ WriteIntent(w) \/ \E j \in Jobs : Compute(w, j)
    \/ \E j \in Jobs : \/ StartJob(j) \/ Observe(j) \/ TimerFire(j) \/ LateEvent(j)
                       \/ \E g \in Gates : Approve(j, g)
                       \/ HostLaunch(j) \/ HostCancel(j) \/ Receipt(j) \/ FenceFirst(j)
                       \/ Interrupt(j) \/ Finish(j) \/ Reconcile(j)

\* Progress premise: finite crash/stop/edit, fair scheduler and host, facts and approvals
\* eventually arrive, Unknown is eventually reconciled, a stop is eventually resumed.
Fair ==
    /\ WF_vars(BeginRun) /\ WF_vars(Settle) /\ WF_vars(\E a \in BOOLEAN : Resume(a))
    /\ \A w \in Workers : WF_vars(Commit(w)) /\ WF_vars(Drop(w)) /\ WF_vars(WriteIntent(w))
    /\ \A j \in Jobs :
         /\ WF_vars(\E w \in Workers : Compute(w, j)) /\ WF_vars(StartJob(j))
         /\ WF_vars(Observe(j)) /\ WF_vars(TimerFire(j)) /\ WF_vars(\E g \in Gates : Approve(j, g))
         /\ WF_vars(HostLaunch(j)) /\ WF_vars(HostCancel(j)) /\ WF_vars(Receipt(j))
         /\ WF_vars(Interrupt(j)) /\ WF_vars(Finish(j)) /\ WF_vars(Reconcile(j))
Next == RunNext /\ UNCHANGED editor
Spec == Init /\ [][Next]_<<vars, editor>> /\ Fair

\* ---- Rules ----
OneCommittedExitPerVisit == ~ghost.dup
NoOrphanState ==
    \A j \in Jobs : Live(j) =>
        /\ state[j] \in States(activeReview)
        /\ (control \in {"prepared", "stopped"} => state[j] \in States(savedReview))
NoReplayAfterRestart == \A j \in Jobs : eff[j].n <= 1
NoNewIntentAfterStop == ~ghost.intent
NoLaunchAfterFence == ~ghost.launch
FinishStageKeepsGrant == ~ghost.finish
PriorityRespected == ~ghost.prio
ApprovalNamesExactVisit == ~ghost.appr
ConditionFromCurrentVisit == ~ghost.cond
RetryWithinBudget == \A j \in Jobs : used[j] <= MaxBudget
AdoptionDoesNotReenter == ~ghost.reentry
TerminalNeedsEvidence == \A j \in Jobs : state[j] \in Terminal => eff[j].st \notin Busy

InstanceTerminates == \A j \in Jobs : (state[j] # "none") ~> (state[j] \in Terminal)

\* ---- Editor transaction (separate finite configuration) ----
EditorGuarded == TRUE
EditorBefore == FALSE
Editable(c) == c \in {"prepared", "stopped"}
StageIds == [plan |-> "requirements", design |-> "design",
             design_review |-> "design_review", work |-> "implementation",
             review |-> "review", landing |-> "landing",
             main_check |-> "main_verification", final_check |-> "final_verification",
             complete |-> "complete"]
LegacyResult(s) == CASE s = "complete" -> "success"
                     [] s \in {"design", "design_review"} -> "unavailable"
                     [] OTHER -> "pending"
MigratedResult(s) ==
    LET mapped == IF ~EditorGuarded /\ s = "work" THEN "complete" ELSE StageIds[s]
    IN CASE mapped = "complete" -> "success"
         [] mapped \in {"design", "design_review"} -> "unavailable"
         [] OTHER -> "pending"

EditorValidate(valid, remove) ==
    editor' = [editor EXCEPT !.validation =
        CASE ~valid -> "invalid"
          [] remove /\ editor.stage = "review" -> "in_use"
          [] OTHER -> "ok"]

EditorAdvance ==
    \/ /\ editor.control = "prepared"
       /\ editor' = [editor EXCEPT !.control = "running", !.stage = "review"]
    \/ /\ editor.control = "running"
       /\ editor' = [editor EXCEPT !.control = "stopping"]
    \/ /\ editor.control = "stopping"
       /\ editor' = [editor EXCEPT !.control = "stopped"]
    \/ /\ editor.control = "running"
       /\ editor' = [editor EXCEPT !.control = "succeeded", !.stage = "complete"]

EditorWritable(rev) ==
    /\ editor.revision < 3
    /\ ~EditorGuarded \/ (Editable(editor.control) /\ rev = editor.revision)

EditorSaved(rev) ==
    [editor EXCEPT !.revision = @ + 1, !.savedFrom = editor.control,
                   !.cas = rev = editor.revision]

EditorSave(rev, valid, remove) ==
    /\ EditorWritable(rev)
    /\ ~EditorGuarded \/ (valid /\ (~remove \/ editor.stage # "review"))
    /\ editor' = [EditorSaved(rev) EXCEPT !.valid = valid, !.review = @ /\ ~remove,
          !.deletedOccupied = @ \/ (remove /\ editor.stage = "review" /\ editor.review)]

EditorMigrate(rev) ==
    /\ EditorWritable(rev) /\ editor.review /\ ~editor.migrated
    /\ editor' = [EditorSaved(rev) EXCEPT !.migrated = TRUE]

EditStep ==
    \/ EditorAdvance
    \/ \E v, d \in BOOLEAN : EditorValidate(v, d)
    \/ \E r \in 1..2 : EditorMigrate(r) \/ \E v, d \in BOOLEAN : EditorSave(r, v, d)
    \/ UNCHANGED editor

EditorNext == EditStep /\ UNCHANGED vars

ValidatedDefinitionOnly == editor.valid
RevisionOnlyWhenEditable == Editable(editor.savedFrom)
RevisionMatchesExpected == editor.cas
OccupiedStateKept == ~editor.deletedOccupied
MigrationKeepsResult ==
    editor.migrated => \A s \in DOMAIN StageIds : MigratedResult(s) = LegacyResult(s)

=============================================================================
