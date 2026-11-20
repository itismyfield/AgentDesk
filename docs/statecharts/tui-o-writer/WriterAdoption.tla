--------------------------- MODULE WriterAdoption ---------------------------
EXTENDS Naturals
VARIABLES lateModal, lateBound, lateOutput, lateBoundFirst, lateJudge,
          lateCommits, lateSkips, lateEnd, lateKind, lateCheckedOutput,
          lateLife, lateSends, lateBadCommit, lateRestored,
          lateRevision, lateCheckedRevision
lateVars == <<lateModal, lateBound, lateOutput, lateBoundFirst, lateJudge,
              lateCommits, lateSkips, lateEnd, lateKind, lateCheckedOutput,
              lateLife, lateSends, lateBadCommit, lateRestored,
              lateRevision, lateCheckedRevision>>
CONSTANTS Fix, Mut
VARIABLE agy
VARIABLES sel, site, root, exec, bound, store, adopt, oStart, turn, waitMark,
          otherCustody, lp, op, rowCleared, dispatchSettled, attempts, restarts,
          recovered, rejected, commitWithOther, hturn, hp
vars == <<sel, site, root, exec, bound, store, adopt, oStart, turn, waitMark,
          otherCustody, lp, op, rowCleared, dispatchSettled, attempts, restarts,
          recovered, rejected, commitWithOther, hturn, hp>>

\* Code baseline 13f9acdd9a; the stage-1 adoption plan r2 with its r2.1 corrections.
\* One TUI-bound channel with one turn, plus one non-TUI (headless) channel in the same config.
\* Fix: "main" as written, "gateOnly" holds unadopted bodies without the warm takeover from the
\* Legacy delivery frontier, "stage1" is the whole stage (boot refusal, hold, frontier takeover).
\* Mut removes one stage-1 guard. Value ranges come from the entry table in report-p1-2.md.
\*   sel      tap.rs:61-67, channel_policy.rs:73-134 (explicit off, list excluding, late off,
\*            implicit default off when the default cannot apply)
\*   site     adoption.rs:384-390 (only the O home has candidates)
\*   root     adoption.rs:419-421 (no runtime root reads every channel as held)
\*   exec     channel_policy.rs:446-513, execution.rs:72-79, channel_gate.rs:461-471
\*   bound    late_adopt/codex.rs:205-207 (no new bind), prefix.rs:205-213 with
\*            codex.rs:229-230 (bound rollout already has output), fresh source or rollout
\*   store    adoption.rs:414-439 (init, damaged or era-only, none)
\*   adopt    adoption.rs:16-28; Refused is the stage-1 refusal state
\*   custody  o_writer_host.rs:306-339, deferred.rs:89-92 (all kinds block alike)
\*   attempts inflight.rs:341-344, recovery_paths/shared.rs:156-171 (tmux absent)

Booted == Fix = "main" \/ sel \in {"default", "defaultOff"}
Selected == sel \in {"default", "lateOff"}
Adoptable == site = "home" /\ sel = "default"

Seed(st) ==
    IF ~Selected \/ site = "foreign" THEN "None"
    ELSE IF ~root THEN (IF Fix = "main" THEN "Held" ELSE "Refused")
    ELSE IF exec = "unverifiedHeld" /\ Fix = "main" THEN "Held"
    ELSE IF st = "init" THEN "Committed"
    ELSE IF st = "damaged" THEN "Held"
    ELSE IF exec = "unverifiedHeld" THEN "Refused"
    ELSE IF sel = "lateOff" THEN "Pending"
    ELSE "Deferred"

\* What the shared gate (channel_gate.rs:433-494) answers for the TUI channel's body.
Answer ==
    IF Fix = "main"
    THEN IF ~Selected \/ site = "foreign" THEN "legacy"
         ELSE IF exec # "none" THEN "owned"
         ELSE IF adopt \in {"Committed", "Held"} THEN "owned"
         ELSE "legacy"
    ELSE IF sel = "defaultOff" \/ site = "foreign" THEN "held"
         ELSE IF exec # "none" /\ Mut = "unadoptedOwned" THEN "owned"
         ELSE IF adopt = "Committed" THEN "owned"
         ELSE IF adopt = "Held" /\ (store # "none" \/ Mut = "unadoptedOwned") THEN "owned"
         ELSE IF adopt = "Deferred" /\ Mut = "ownedForDeferred" THEN "owned"
         ELSE "held"

\* O store owns the turn's range unposted: committed from the frontier, or a store O holds.
Duty == store # "none" /\ lp = 0
        /\ (adopt = "Held" \/ (adopt = "Committed" /\ oStart = "F"))
Evidence == op >= 1 \/ lp >= 1 \/ Duty

OwnCustody == turn \in {"running", "held"}
Exempt == Mut = "exemptAllRows" \/ (Fix # "main" /\ waitMark /\ turn = "held")
Blocked == (otherCustody /\ Mut # "exemptAllRows") \/ (OwnCustody /\ ~Exempt)

QuietLate ==
    /\ lateModal = FALSE /\ lateBound = FALSE /\ lateOutput = FALSE
    /\ lateBoundFirst = FALSE /\ lateJudge = "waiting"
    /\ lateCommits = 0 /\ lateSkips = 0 /\ lateEnd = "waiting"
    /\ lateKind = "clean" /\ lateCheckedOutput = FALSE /\ lateLife = "live"
    /\ lateSends = 0 /\ lateBadCommit = FALSE /\ lateRestored = FALSE
    /\ lateRevision = 0 /\ lateCheckedRevision = 0
Init ==
    /\ QuietLate /\ agy = 0
    /\ sel \in {"default", "explicitOff", "listExcl", "lateOff", "defaultOff"}
    /\ site \in IF sel = "default" THEN {"home", "foreign"} ELSE {"home"}
    /\ root \in IF sel = "default" THEN BOOLEAN ELSE {TRUE}
    /\ exec \in IF sel = "default" THEN {"none", "permit", "unverifiedHeld"} ELSE {"none"}
    /\ bound \in {"none", "fresh", "withOutput"}
    /\ store \in IF sel = "default" /\ root THEN {"none", "init", "damaged"} ELSE {"none"}
    /\ adopt = Seed(store)
    /\ oStart = IF store = "init" THEN "F" ELSE "none"
    /\ rejected = (adopt = "Refused")
    /\ turn = "none" /\ waitMark = FALSE /\ otherCustody \in BOOLEAN
    /\ lp = 0 /\ op = 0 /\ rowCleared = FALSE /\ dispatchSettled = FALSE
    /\ attempts = 0 /\ restarts = 0 /\ recovered = FALSE /\ commitWithOther = FALSE
    /\ hturn = "none" /\ hp = 0

Fixed == UNCHANGED <<sel, site, root, exec, bound>>

\* Discord intake on the TUI channel opens a turn (intake_dispatch.rs:281-294).
Input == /\ Booted /\ turn = "none" /\ turn' = "running"
         /\ Fixed
         /\ UNCHANGED <<store, adopt, oStart, waitMark, otherCustody, lp, op, rowCleared,
                         dispatchSettled, attempts, restarts, recovered, rejected,
                         commitWithOther, hturn, hp>>

\* One gate answer at a body site: Legacy sends, O keeps the body, or the body is held.
Judge(next) ==
    CASE Answer = "legacy" ->
            /\ lp' = lp + 1 /\ rowCleared' = TRUE /\ turn' = "done" /\ waitMark' = FALSE
            /\ adopt' = IF adopt = "Pending" THEN "Released" ELSE adopt
      [] Answer = "owned" ->
            /\ rowCleared' = TRUE /\ turn' = "done" /\ waitMark' = FALSE
            /\ UNCHANGED <<lp, adopt>>
      [] OTHER ->
            /\ turn' = next /\ waitMark' = (Fix # "main")
            /\ UNCHANGED <<lp, rowCleared, adopt>>

\* Bridge stream tick and terminal (stream_tick.rs:389-397, terminal_outcome_delivery.rs:440-453).
TerminalBody == /\ Booted /\ turn = "running" /\ Judge("held")
                /\ Fixed
                /\ UNCHANGED <<store, oStart, otherCustody, op, dispatchSettled, attempts,
                                restarts, recovered, rejected, commitWithOther, hturn, hp>>

\* A held row judged again: the live bridge's next tick, or the adoption wake.
ReEval == /\ Booted /\ turn = "held" /\ Answer # "held" /\ Judge("held")
          /\ Fixed
          /\ UNCHANGED <<store, oStart, otherCustody, op, dispatchSettled, attempts,
                          restarts, recovered, rejected, commitWithOther, hturn, hp>>

\* Restart recovery of a held row, once per boot (completion_delivery.rs:117-151).
Counts == Fix = "main" \/ Mut = "countWaiting"
RecoveryBody ==
    /\ Booted /\ turn = "held" /\ restarts >= 1 /\ ~recovered /\ recovered' = TRUE
    /\ IF Answer # "held"
       THEN /\ Judge("held") /\ UNCHANGED attempts
       ELSE IF Counts
            THEN /\ attempts' = attempts + 1
                 /\ IF attempts + 1 >= 3
                    THEN /\ rowCleared' = TRUE /\ turn' = "done" /\ waitMark' = FALSE
                    ELSE UNCHANGED <<rowCleared, turn, waitMark>>
                 /\ UNCHANGED <<lp, adopt>>
            ELSE UNCHANGED <<attempts, lp, rowCleared, turn, waitMark, adopt>>
    /\ Fixed
    /\ UNCHANGED <<store, oStart, otherCustody, op, dispatchSettled, restarts, rejected,
                    commitWithOther, hturn, hp>>

\* The watcher's dispatch finalizer (tmux_watcher.rs:2351-2392); stage 1 waits for evidence.
WatcherDispatch ==
    /\ Booted /\ turn \in {"held", "done"} /\ ~dispatchSettled
    /\ (Fix = "main" \/ Mut = "watcherNoGuard" \/ Evidence)
    /\ dispatchSettled' = TRUE
    /\ Fixed
    /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, op, rowCleared,
                    attempts, restarts, recovered, rejected, commitWithOther, hturn, hp>>

Commit(start) ==
    /\ adopt' = "Committed" /\ store' = "init" /\ oStart' = start
    /\ commitWithOther' = (commitWithOther \/ otherCustody)
    /\ Fixed
    /\ UNCHANGED <<turn, waitMark, otherCustody, lp, op, rowCleared, dispatchSettled,
                    attempts, restarts, recovered, rejected, hturn, hp>>

\* A fresh source or rollout is taken before it has output (late_adopt.rs:182-186, codex.rs:203-310).
FreshAdopt == /\ Booted /\ adopt \in {"Deferred", "Pending"} /\ bound = "fresh" /\ ~Blocked
              /\ Commit("F")
\* Warm takeover from the Legacy delivery frontier (stage 1 P3a and P3b).
WarmAdoptF == /\ Fix = "stage1" /\ adopt = "Deferred" /\ bound # "fresh" /\ ~Blocked
              /\ Commit("F")
\* Stale handoff at the end of file (deferred.rs:26-29); not guaranteed to happen.
EofHandoff == /\ Fix # "stage1" /\ adopt = "Deferred" /\ bound = "withOutput" /\ ~Blocked
              /\ Commit(IF turn = "none" THEN "F" ELSE "EOF")

\* The writer refuses adoption (host.rs:659-678, first_init.rs:79-80, activation.rs:223-225).
Reject == /\ Booted /\ adopt \in {"Deferred", "Pending"}
          /\ adopt' = IF Fix = "main" THEN "Released"
                      ELSE IF Mut = "unadoptedOwned" THEN "Held" ELSE "Refused"
          /\ rejected' = TRUE
          /\ Fixed
          /\ UNCHANGED <<store, oStart, turn, waitMark, otherCustody, lp, op, rowCleared,
                          dispatchSettled, attempts, restarts, recovered, commitWithOther,
                          hturn, hp>>

\* The O writer posts the turn from the transcript when its range is O's.
OPost == /\ adopt = "Committed" /\ oStart = "F" /\ lp = 0 /\ op = 0
         /\ turn \in {"running", "held", "done"}
         /\ op' = 1
         /\ Fixed
         /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, rowCleared,
                         dispatchSettled, attempts, restarts, recovered, rejected,
                         commitWithOther, hturn, hp>>

\* Another turn's row, pending start or open intake ends.
OtherTurnEnds == /\ otherCustody /\ otherCustody' = FALSE
                 /\ Fixed
                 /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, lp, op, rowCleared,
                                 dispatchSettled, attempts, restarts, recovered, rejected,
                                 commitWithOther, hturn, hp>>

\* A process restart keeps the row and seeds adoption from the store (channel_policy.rs:285-351).
\* A running turn's bridge is restored and judges again (restore_inflight.rs:2378); a held row
\* waits for the boot recovery.
Restart == /\ Booted /\ restarts < 3 /\ turn \in {"running", "held"}
           /\ restarts' = restarts + 1 /\ recovered' = FALSE
           /\ adopt' = Seed(store)
           /\ rejected' = (rejected \/ Seed(store) = "Refused")
           /\ Fixed
           /\ UNCHANGED <<store, oStart, turn, waitMark, otherCustody, lp, op, rowCleared,
                           dispatchSettled, attempts, commitWithOther, hturn, hp>>

\* A headless or routine turn on the non-TUI channel (headless_delivery.rs:588-608).
HeadlessInput == /\ Booted /\ hturn = "none" /\ hturn' = "running"
                 /\ Fixed
                 /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, op,
                                 rowCleared, dispatchSettled, attempts, restarts, recovered,
                                 rejected, commitWithOther, hp>>
HeadlessTerminal == /\ hturn = "running"
                    /\ IF Mut = "headlessCut"
                       THEN hturn' = "held" /\ UNCHANGED hp
                       ELSE hturn' = "done" /\ hp' = hp + 1
                    /\ Fixed
                    /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, op,
                                    rowCleared, dispatchSettled, attempts, restarts,
                                    recovered, rejected, commitWithOther>>

Rest == UNCHANGED vars
LegacyNext == Input \/ TerminalBody \/ ReEval \/ RecoveryBody \/ WatcherDispatch \/ FreshAdopt
        \/ WarmAdoptF \/ EofHandoff \/ Reject \/ OPost \/ OtherTurnEnds \/ Restart
        \/ HeadlessInput \/ HeadlessTerminal \/ Rest
Next == LegacyNext /\ UNCHANGED <<lateVars, agy>>

\* Writer, recovery and turn-end actions are fair. Input, Restart, Reject, the stale handoff
\* and the watcher finalizer are environment.
Spec == Init /\ [][Next]_<<vars, agy>>
        /\ WF_vars(TerminalBody) /\ WF_vars(ReEval) /\ WF_vars(RecoveryBody)
        /\ WF_vars(FreshAdopt) /\ WF_vars(WarmAdoptF) /\ WF_vars(OPost)
        /\ WF_vars(OtherTurnEnds) /\ WF_vars(HeadlessTerminal)

NoLegacyBodyOnTui == lp = 0
AnswerAtMostOnce == op + lp <= 1
UnadoptedNotOwned == store = "none" => Answer # "owned"
RowClearedHasEvidence == rowCleared => Evidence
NoEarlyDispatch == dispatchSettled => Evidence
HeldBodyNotSkipped == (turn = "held" /\ adopt = "Committed") => oStart = "F"
OtherCustodyBlocks == ~commitWithOther
HeadlessKept == hturn # "held" /\ (hturn = "done" => hp = 1)

HeldTurnAnswered == [](turn = "held" /\ Adoptable => <>(op = 1 \/ rejected))
AdoptionSettles == [](adopt = "Deferred" /\ Adoptable /\ turn # "none"
                      => <>(adopt = "Committed" \/ rejected))
RowEventuallySettled == [](op = 1 => <>rowCleared)

\* Cold-launch domain: codex-late-launch.report.md entry census. Native records and Discord
\* sends are distinct. lateBuffered changes prefix acceptance, not the argv/input protocol.
LegacyIdle ==
    /\ sel = 0 /\ site = 0 /\ root = 0 /\ exec = 0 /\ bound = 0
    /\ store = 0 /\ adopt = 0 /\ oStart = 0 /\ turn = 0 /\ waitMark = 0
    /\ otherCustody = 0 /\ lp = 0 /\ op = 0 /\ rowCleared = 0
    /\ dispatchSettled = 0 /\ attempts = 0 /\ restarts = 0 /\ recovered = 0
    /\ rejected = 0 /\ commitWithOther = 0 /\ hturn = 0 /\ hp = 0
LateInit == /\ LegacyIdle /\ agy = 0
            /\ lateModal \in BOOLEAN
            /\ lateBound = FALSE /\ lateOutput = FALSE /\ lateBoundFirst = FALSE
            /\ lateJudge = "waiting" /\ lateCommits = 0 /\ lateSkips = 0
            /\ lateEnd = "waiting" /\ lateCheckedOutput = FALSE
            /\ lateKind \in {"clean", "announced", "identified", "unidentified",
                              "turnEnd", "secondInput", "otherTurn"}
            /\ lateLife = "live" /\ lateSends = 0
            /\ lateBadCommit = FALSE /\ lateRestored = FALSE
            /\ lateRevision = 0 /\ lateCheckedRevision = 0

LateValid == ~lateOutput \/ lateKind \in {"announced", "identified", "turnEnd"}
LateAccept == IF Fix = "lateBuffered"
              THEN LateValid \/ Mut = "identity"
              ELSE ~lateOutput
LateCurrent == lateLife = "live" \/ Mut = "incarnation"
LateQuiet == lateSends = 0 \/ Mut = "unfenced"
LateRecheck == ~(Mut = "recheck" \/ (Mut = "bufferedRecheck" /\ lateCheckedOutput))
LateReserved == Fix = "lateBuffered" /\ lateLife = "live"
                /\ lateJudge \in {"waiting", "checked"}

LateBind == /\ UNCHANGED vars /\ ~lateModal /\ ~lateBound
            /\ (Fix # "lateBefore" \/ lateOutput)
            /\ lateBound' = TRUE /\ lateBoundFirst' = ~lateOutput
            /\ UNCHANGED <<lateModal, lateOutput, lateJudge, lateCommits, lateSkips,
                            lateEnd, lateKind, lateCheckedOutput, lateLife, lateSends,
                            lateBadCommit, lateRestored, lateRevision, lateCheckedRevision>>

LateNativeOutput ==
    /\ UNCHANGED vars /\ ~lateModal /\ ~lateOutput /\ lateKind # "clean"
    /\ (Fix \in {"lateBefore", "lateBuffered"} \/ lateBound)
    /\ (Fix # "lateFenced" \/ Mut = "unfenced" \/ lateJudge = "committed")
    /\ lateOutput' = TRUE
    /\ lateRevision' = 1
    /\ UNCHANGED <<lateModal, lateBound, lateBoundFirst, lateJudge, lateCommits,
                    lateSkips, lateEnd, lateKind, lateCheckedOutput, lateLife,
                    lateSends, lateBadCommit, lateRestored, lateCheckedRevision>>

\* A checked buffered prefix can grow once more, either safely or with an invalid record.
LateAppend ==
    /\ UNCHANGED vars /\ Fix = "lateBuffered" /\ lateJudge = "checked"
    /\ lateOutput /\ LateValid /\ lateRevision = 1
    /\ lateRevision' = 2
    /\ lateKind' \in {"announced", "identified", "turnEnd", "unidentified",
                      "secondInput", "otherTurn", "rewritten"}
    /\ UNCHANGED <<lateModal, lateBound, lateOutput, lateBoundFirst, lateJudge,
                    lateCommits, lateSkips, lateEnd, lateCheckedOutput, lateLife,
                    lateSends, lateBadCommit, lateRestored, lateCheckedRevision>>

\* Separate initial judge and final judgment: output can arrive during intake_fence.await.
LateJudge == /\ UNCHANGED vars /\ lateBound /\ lateJudge = "waiting" /\ LateCurrent
             /\ lateJudge' = IF LateAccept THEN "checked" ELSE "skipped"
             /\ lateCheckedOutput' = lateOutput
             /\ lateCheckedRevision' = lateRevision
             /\ lateSkips' = IF LateAccept THEN 0 ELSE 1
             /\ UNCHANGED <<lateModal, lateBound, lateOutput, lateBoundFirst,
                             lateCommits, lateEnd, lateKind, lateLife, lateSends,
                             lateBadCommit, lateRestored, lateRevision>>

LateCommit ==
    /\ UNCHANGED vars /\ lateJudge = "checked" /\ LateCurrent
    /\ IF (~LateAccept /\ LateRecheck) \/ ~LateQuiet
       THEN /\ lateJudge' = "skipped" /\ lateSkips' = 1
            /\ UNCHANGED <<lateCommits, lateBadCommit>>
       ELSE IF lateCheckedRevision # lateRevision /\ LateRecheck
            THEN /\ lateJudge' = "waiting"
                 /\ UNCHANGED <<lateSkips, lateCommits, lateBadCommit>>
            ELSE /\ lateJudge' = "committed" /\ lateCommits' = lateCommits + 1
                 /\ lateBadCommit' = (~LateValid \/ lateSends # 0 \/ lateLife # "live"
                                      \/ lateRevision # lateCheckedRevision)
                 /\ UNCHANGED lateSkips
    /\ UNCHANGED <<lateModal, lateBound, lateOutput, lateBoundFirst, lateEnd,
                    lateKind, lateCheckedOutput, lateLife, lateSends, lateRestored,
                    lateRevision, lateCheckedRevision>>

\* Lifecycle invalidation is environment, including before/inside/after the commit fence.
\* A restarted process restores a durable init; it must not recommit an unproved old attempt.
LateInvalidate ==
    /\ UNCHANGED vars /\ Fix = "lateBuffered" /\ lateLife = "live" /\ ~lateModal
    /\ lateLife' \in {"restarted", "cancelled", "terminated", "cleared", "stopped",
                      "rebound", "evicted"}
    /\ UNCHANGED <<lateModal, lateBound, lateOutput, lateBoundFirst, lateJudge,
                    lateCommits, lateSkips, lateEnd, lateKind, lateCheckedOutput,
                    lateSends, lateBadCommit, lateRestored, lateRevision, lateCheckedRevision>>
LateRestore ==
    /\ UNCHANGED vars /\ lateLife = "restarted" /\ ~lateRestored
    /\ lateRestored' = TRUE
    /\ lateCommits' = IF Mut = "restore" /\ lateCommits = 1 THEN 2 ELSE lateCommits
    /\ UNCHANGED <<lateModal, lateBound, lateOutput, lateBoundFirst, lateJudge,
                    lateSkips, lateEnd, lateKind, lateCheckedOutput, lateLife,
                    lateSends, lateBadCommit, lateRevision, lateCheckedRevision>>

LateLegacySend ==
    /\ UNCHANGED vars /\ Fix = "lateBuffered" /\ ~lateModal /\ lateSends = 0
    /\ lateJudge # "committed"
    /\ (~LateReserved \/ Mut \in {"admissionHold", "unfenced"})
    /\ lateSends' = 1
    /\ UNCHANGED <<lateModal, lateBound, lateOutput, lateBoundFirst, lateJudge,
                    lateCommits, lateSkips, lateEnd, lateKind, lateCheckedOutput,
                    lateLife, lateBadCommit, lateRestored, lateRevision, lateCheckedRevision>>

LateStartupEnd ==
    /\ UNCHANGED vars /\ lateModal /\ lateEnd = "waiting"
    /\ lateEnd' = IF Fix = "lateBefore" \/ Mut = "timeout"
                   THEN "timeout" ELSE "reasonedFailure"
    /\ UNCHANGED <<lateModal, lateBound, lateOutput, lateBoundFirst,
                    lateJudge, lateCommits, lateSkips, lateKind, lateCheckedOutput,
                    lateLife, lateSends, lateBadCommit, lateRestored,
                    lateRevision, lateCheckedRevision>>

LateNext == (LateBind \/ LateNativeOutput \/ LateAppend \/ LateJudge \/ LateCommit
            \/ LateStartupEnd \/ LateInvalidate \/ LateRestore \/ LateLegacySend) /\ UNCHANGED agy
LateSpec == LateInit /\ [][LateNext]_<<lateVars, agy>>
            /\ WF_lateVars(LateBind) /\ WF_lateVars(LateNativeOutput)
            /\ WF_lateVars(LateJudge) /\ WF_lateVars(LateCommit)
            /\ WF_lateVars(LateStartupEnd) /\ WF_lateVars(LateRestore)

LateBindingPrecedesOutput == lateOutput => lateBound
LateCommitAtMostOnce == lateCommits <= 1
LateBoundBeforeNeverSkipped == lateBoundFirst => lateSkips = 0
LateBoundBeforeCommits == [](lateBoundFirst => <>(lateCommits = 1))
LateModalExplained == [](lateModal => <>(lateEnd = "reasonedFailure"))
LateNoUnsafeCommit == ~lateBadCommit
LateEligibleCommits == []((lateBound /\ LateValid /\ lateLife = "live" /\ lateSends = 0)
                          => <>(lateCommits = 1 \/ ~LateValid \/ lateLife # "live"))
LateRestartRestores == [](lateLife = "restarted" => <>lateRestored)
\* Antigravity has a durable logical owner before a one-shot child can run.
\* One unit has one or three ordered pieces; counts stand for remote effects,
\* not POST attempts. Store/authority interruptions preserve durable debt.
AgyPages == IF Fix = "short" THEN 1 ELSE 3
AgyTurnLimit == IF Fix = "two" THEN 2 ELSE 1
AgyZero == [i \in 1..AgyPages |-> 0]
AgyTurn(id, phase) ==
    [id |-> id, phase |-> phase, oldDebt |-> phase = "waiting" /\ Fix # "short",
     ingress |-> "discordForeground", accepted |-> FALSE,
     deltas |-> 0, terminal |-> "none", spool |-> FALSE,
     prepared |-> FALSE, discarded |-> FALSE, unresolved |-> FALSE,
     remote |-> AgyZero, offset |-> 0, frontier |-> 0, ends |-> 0, effects |-> 0, answers |-> 0,
     authority |-> TRUE, interrupted |-> FALSE, reconcile |-> FALSE,
     rejected |-> FALSE, retried |-> FALSE,
     legacy |-> 0, alarm |-> FALSE, stale |-> FALSE]
AgyInit == /\ LegacyIdle /\ QuietLate
           /\ IF Fix = "headless"
              THEN \E ingress \in {"routine", "authenticatedHttp"} :
                  agy = [AgyTurn(1, "ready") EXCEPT !.ingress = ingress]
              ELSE agy = AgyTurn(1, "waiting")

AgyLegacyReceipt ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "waiting" /\ agy.oldDebt
    /\ agy' = [agy EXCEPT !.oldDebt = FALSE]
AgyAdopt ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "waiting" /\ (~agy.oldDebt \/ Mut = "borrow")
    /\ agy' = [agy EXCEPT !.phase = "ready"]
AgyReserve ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "ready" /\ agy.ingress = "discordForeground"
    /\ agy' = [agy EXCEPT !.phase = "reserved", !.accepted = TRUE, !.spool = TRUE]
AgySpawn ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "reserved"
    /\ agy' = [agy EXCEPT !.phase = "running"]
AgyAppendDelta ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "running" /\ agy.deltas < AgyPages - 1
    /\ agy' = [agy EXCEPT !.deltas = @ + 1]
AgyObserveSuccess ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "running" /\ agy.deltas = AgyPages - 1 /\ Fix # "failed"
    /\ agy' = [agy EXCEPT !.phase = "childSucceeded"]
AgyAppendDone ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "childSucceeded" /\ agy.authority /\ Mut # "missingDone"
    /\ agy' = [agy EXCEPT !.phase = "sealed", !.terminal = "done"]
AgyObserveFailure ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase \in {"reserved", "running"}
    /\ agy' = [agy EXCEPT !.phase = IF agy.phase = "reserved" THEN "spawnFailed" ELSE "childLost"]
AgyChildFailed ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.authority
    /\ \/ agy.phase = "spawnFailed" /\ Mut # "missingSpawnFailure"
       \/ agy.phase = "childLost" /\ Mut # "missingLostChild"
    /\ agy' = [agy EXCEPT !.phase = "sealed", !.terminal = "failed"]
AgyDiscard ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ Fix = "clear" /\ agy.authority /\ ~agy.discarded
    /\ agy.phase \in {"reserved", "running", "childSucceeded", "sealed", "delivery"}
    /\ agy' = IF Mut = "missingDisposition"
              THEN [agy EXCEPT !.phase = "dropped"]
              ELSE [agy EXCEPT !.phase = "delivery", !.discarded = TRUE,
                               !.unresolved = agy.prepared, !.prepared = FALSE]
AgyReadSeal ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "sealed" /\ agy.spool /\ agy.authority
    /\ agy' = [agy EXCEPT !.phase = "delivery"]
AgyPrepare ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "delivery" /\ agy.terminal = "done" /\ agy.authority /\ ~agy.discarded
    /\ ~agy.prepared /\ agy.offset < AgyPages
    /\ agy' = [agy EXCEPT !.prepared = TRUE]
AgyPost ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "delivery" /\ agy.prepared /\ agy.authority /\ agy.offset < AgyPages
    /\ agy.remote[agy.offset + 1] = 0
    /\ agy' = [agy EXCEPT !.remote[agy.offset + 1] = 1]
AgyReceipt ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "delivery" /\ agy.prepared /\ agy.authority /\ agy.offset < AgyPages
    /\ agy.remote[agy.offset + 1] > 0
    /\ agy' = [agy EXCEPT !.prepared = FALSE, !.reconcile = FALSE, !.offset = @ + 1,
                           !.frontier = IF agy.offset < @ THEN @ ELSE agy.offset + 1]
AgyNotifyFailure ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "delivery" /\ agy.authority /\ agy.terminal = "failed" /\ ~agy.alarm
    /\ agy' = [agy EXCEPT !.alarm = TRUE]
AgyCanFinish == agy.phase = "delivery" /\ ~agy.prepared
                /\ (agy.offset = AgyPages \/ agy.alarm \/ agy.discarded)
AgyLoseAuthority ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase \in {"reserved", "running", "childSucceeded", "spawnFailed", "childLost",
                      "sealed", "delivery", "finishing"}
    /\ Mut = "abandon" => agy.terminal = "done"
    /\ agy.authority /\ ~agy.interrupted
    /\ agy' = [agy EXCEPT !.authority = FALSE, !.interrupted = TRUE,
                           !.reconcile = IF agy.prepared THEN agy.remote[agy.offset + 1] > 0 ELSE FALSE]
AgyRestore ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ ~agy.authority /\ Mut # "abandon"
    /\ agy' = [agy EXCEPT !.authority = TRUE]
AgyRejectFinish ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ (AgyCanFinish \/ agy.phase = "finishing") /\ agy.authority /\ ~agy.rejected
    /\ agy' = [agy EXCEPT !.authority = FALSE, !.rejected = TRUE, !.interrupted = TRUE]
AgyFinish ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ AgyCanFinish /\ agy.authority
    /\ agy' = [agy EXCEPT !.phase = "finishing", !.ends = @ + 1,
                           !.answers = IF agy.terminal = "done" /\ ~agy.discarded THEN @ + 1 ELSE @]
\* Each step represents an idempotent projection with its own durable marker.
AgyApplyEnd ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "finishing" /\ agy.authority /\ agy.effects < 3
    /\ agy' = [agy EXCEPT !.effects = @ + 1,
                           !.phase = IF agy.effects = 2 THEN "ended" ELSE @]
AgyArchive ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ (agy.phase = "ended" \/ (Mut = "purge" /\ agy.phase = "sealed"))
    /\ agy.spool /\ ~agy.unresolved
    /\ agy' = [agy EXCEPT !.spool = FALSE]
AgyNextInput ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "ended" /\ agy.id < AgyTurnLimit
    /\ agy' = AgyTurn(agy.id + 1, "ready")
AgyHeadlessAnswer ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.phase = "ready" /\ agy.ingress # "discordForeground"
    /\ agy' = [agy EXCEPT !.phase = "headlessDone", !.legacy = @ + 1]
AgyLegacyRace ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ Mut = "dual" /\ agy.phase = "delivery" /\ agy.terminal = "done" /\ agy.legacy = 0
    /\ agy' = [agy EXCEPT !.legacy = 1]
AgyBlindRepost ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ Mut = "repost" /\ agy.phase = "delivery" /\ agy.prepared
    /\ agy.reconcile /\ agy.authority /\ agy.offset < AgyPages
    /\ agy.remote[agy.offset + 1] = 1
    /\ agy' = [agy EXCEPT !.remote[agy.offset + 1] = 2]
AgyRepeatDone ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.terminal = "done" /\ agy.spool /\ ~agy.retried /\ agy.offset >= 2
    /\ agy' = [agy EXCEPT !.retried = TRUE,
                           !.offset = IF Mut = "rewind" THEN 0 ELSE @,
                           !.ends = IF Mut = "doubleEnd" /\ agy.phase = "ended" THEN @ + 1 ELSE @]
AgyStaleChild ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ Mut = "stale" /\ agy.id = 2 /\ agy.phase = "running" /\ ~agy.stale
    /\ agy' = [agy EXCEPT !.stale = TRUE]
AgyPrematurePost ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ Mut = "preview" /\ agy.phase = "running" /\ agy.deltas > 0
    /\ agy.remote[1] = 0
    /\ agy' = [agy EXCEPT !.remote[1] = 1]
AgyRest == UNCHANGED <<agy, vars, lateVars>>
AgyNext ==
       \/ AgyLegacyReceipt \/ AgyAdopt \/ AgyReserve \/ AgySpawn \/ AgyAppendDelta
       \/ AgyObserveSuccess \/ AgyAppendDone \/ AgyObserveFailure \/ AgyChildFailed \/ AgyDiscard
       \/ AgyReadSeal \/ AgyPrepare \/ AgyPost \/ AgyReceipt
       \/ AgyNotifyFailure \/ AgyLoseAuthority \/ AgyRestore \/ AgyRejectFinish \/ AgyFinish \/ AgyApplyEnd
       \/ AgyArchive \/ AgyNextInput \/ AgyHeadlessAnswer \/ AgyLegacyRace \/ AgyBlindRepost
       \/ AgyRepeatDone \/ AgyStaleChild \/ AgyPrematurePost \/ AgyRest
AgySpec == AgyInit /\ [][AgyNext]_agy
    /\ WF_agy(AgyLegacyReceipt) /\ WF_agy(AgyAdopt) /\ WF_agy(AgyReserve) /\ WF_agy(AgySpawn)
    /\ WF_agy(AgyAppendDelta) /\ WF_agy(AgyObserveSuccess \/ AgyObserveFailure)
    /\ WF_agy(AgyAppendDone) /\ WF_agy(AgyReadSeal)
    /\ WF_agy(AgyPrepare) /\ WF_agy(AgyPost) /\ WF_agy(AgyReceipt) /\ WF_agy(AgyRestore)
    /\ WF_agy(AgyFinish) /\ WF_agy(AgyApplyEnd) /\ WF_agy(AgyArchive) /\ WF_agy(AgyNextInput)
    /\ WF_agy(AgyChildFailed) /\ WF_agy(AgyNotifyFailure) /\ WF_agy(AgyHeadlessAnswer)

AgyOPath == agy.ingress = "discordForeground"
AgyOnlyDiscordAdmission == (agy.accepted \/ agy.spool) => AgyOPath
AgyNoDuplicate ==
    AgyOPath => /\ agy.legacy = 0
                /\ \A i \in 1..AgyPages : agy.remote[i] <= 1
AgyNoSilentLoss ==
    /\ agy.accepted => agy.spool \/ agy.phase = "ended"
    /\ (AgyOPath /\ agy.phase # "waiting") => ~agy.oldDebt
    /\ agy.phase = "ended" => (agy.terminal = "done" \/ agy.alarm \/ agy.discarded)
AgyExactlyOneAnswer ==
    /\ agy.answers <= 1
    /\ (agy.phase = "ended" /\ agy.terminal = "done" /\ ~agy.discarded) =>
        /\ agy.answers = 1
        /\ \A i \in 1..AgyPages : agy.remote[i] = 1
AgyOffsetMonotone == agy.offset = agy.frontier
AgyNoPrematureBody == agy.terminal # "done" => agy.remote = AgyZero
AgyEndOnce ==
    /\ agy.ends <= 1
    /\ agy.effects > 0 => agy.ends = 1
    /\ agy.phase = "ended" => agy.effects = 3
AgyInputIdentity == ~agy.stale
\* A later id witnesses End of the earlier one independently of spool retirement.
AgyAcceptedEnds ==
    \A id \in 1..AgyTurnLimit :
        []((agy.id = id /\ agy.accepted) =>
            <>(agy.id > id \/ (agy.id = id /\ agy.phase = "ended" /\ agy.ends = 1)))
AgySecondTurnEnds == <>(agy.id = 2 /\ agy.phase = "ended")
AgyFailedEnds == (Fix = "failed") => <>(agy.phase = "ended" /\ agy.alarm)
AgyHeadlessKept == (Fix = "headless") => <>(agy.phase = "headlessDone" /\ agy.legacy = 1)

\* One verified child and its prior Legacy obligation. Shadow is always record-only.
\* "cutover" exercises the future permission with the same lifecycle guards.
OwnerInit ==
    /\ LegacyIdle /\ QuietLate
    /\ \E selected \in BOOLEAN :
       agy = [selected |-> selected, verified |-> FALSE, prior |-> TRUE,
              boot |-> "starting", generation |-> 1, owners |-> {"legacy"},
              retired |-> FALSE, late |-> FALSE]

OwnerVerify ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.selected /\ ~agy.verified
    /\ agy' = [agy EXCEPT !.verified = TRUE]

OwnerLegacyReceipt ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.prior
    /\ agy' = [agy EXCEPT !.prior = FALSE]

OwnerBoot ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.verified /\ agy.boot = "starting"
    /\ agy' = [agy EXCEPT !.boot = "scanned"]

OwnerAdopt ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.boot = "scanned" /\ ~agy.prior /\ ~agy.retired
    /\ (Mut # "noOwner" \/ agy.generation = 1)
    /\ agy' = [agy EXCEPT !.boot = "ready",
        !.owners = IF Fix = "shadow" THEN {"legacy"}
                   ELSE IF Mut = "dualOwner" THEN {"legacy", "o"} ELSE {"o"}]

OwnerRestart ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.boot = "ready" /\ agy.generation = 1 /\ ~agy.retired
    /\ agy' = [agy EXCEPT !.boot = "starting", !.generation = 2,
                           !.owners = IF Fix = "shadow" THEN {"legacy"} ELSE {}]

OwnerRetire ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.boot = "ready" /\ ~agy.retired
    /\ agy' = [agy EXCEPT !.retired = TRUE, !.owners = {}]

OwnerLateBody ==
    /\ UNCHANGED <<vars, lateVars>>
    /\ agy.retired /\ Mut = "lateBody" /\ ~agy.late
    /\ agy' = [agy EXCEPT !.late = TRUE]

OwnerRest == UNCHANGED <<agy, vars, lateVars>>
OwnerNext == OwnerVerify \/ OwnerLegacyReceipt \/ OwnerBoot \/ OwnerAdopt
             \/ OwnerRestart \/ OwnerRetire \/ OwnerLateBody \/ OwnerRest

AgyOwnerSpec == OwnerInit /\ [][OwnerNext]_agy
    /\ WF_agy(OwnerVerify) /\ WF_agy(OwnerLegacyReceipt)
    /\ WF_agy(OwnerBoot) /\ WF_agy(OwnerAdopt)

AgyChildSingleOwner == ~({"legacy", "o"} \subseteq agy.owners)
AgyRetirementFenced == agy.retired => agy.owners = {} /\ ~agy.late
AgyShadowRecordOnly == Fix = "shadow" => "o" \notin agy.owners
AgyRestartResolvesOwner ==
    []((agy.generation = 2 /\ ~agy.retired) =>
       <> (agy.retired \/ (agy.boot = "ready" /\ agy.owners # {})))
====
