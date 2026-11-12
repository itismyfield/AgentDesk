--------------------------- MODULE WriterAdoption ---------------------------
EXTENDS Naturals
CONSTANTS Fix, Mut
VARIABLES sel, site, root, exec, bound, store, adopt, oStart, turn, waitMark,
          otherCustody, lp, op, rowCleared, dispatchSettled, attempts, restarts,
          recovered, rejected, commitWithOther, hturn, hp, nativeEnded,
          endedBeforeBase, prevOwed, commitPastOwed, otherIntake, mk, wakeRead, claim, tries, woke, opened
vars == <<sel, site, root, exec, bound, store, adopt, oStart, turn, waitMark,
          otherCustody, lp, op, rowCleared, dispatchSettled, attempts, restarts,
          recovered, rejected, commitWithOther, hturn, hp, nativeEnded,
          endedBeforeBase, prevOwed, commitPastOwed, otherIntake, mk, wakeRead, claim, tries, woke, opened>>

\* Code baseline 13f9acdd9a; the stage-1 adoption plan r2 with its r2.1 corrections.
\* One TUI-bound channel with one turn, plus one non-TUI (headless) channel in the same config.
\* Fix: "main" as written, "gateOnly" holds unadopted bodies without the warm takeover from the
\* Legacy delivery frontier, "stage1" is the whole stage (boot refusal, hold, frontier takeover).
\* Mut removes one stage-1 guard. Value ranges come from the entry table in report-p1-2.md.
\*   nativeEnded  late_adopt/codex/prefix.rs frontier: the turn's own end record (task_complete)
\*            written before Legacy delivered it; advanceOnEnd is the takeover that moved F past it
\*   endedBeforeBase  that end was written before the takeover's baseline; relayedByTime counted it
\*            delivered by time alone (late_adopt/codex.rs start_on)
\*   prevOwed the rollout bound before holds a turn Legacy may have sent part of (codex.rs owed);
\*            swallowOwed committed the new rollout without it
\*   sel      tap.rs:61-67, channel_policy.rs:73-134 (explicit off, list excluding, late off,
\*            implicit default off when the default cannot apply)
\*   site     adoption.rs:384-390 (only the O home has candidates)
\*   root     adoption.rs:419-421 (no runtime root reads every channel as held)
\*   exec     channel_policy.rs:446-513, execution.rs:72-79, channel_gate.rs:461-471
\*   bound    late_adopt/codex.rs:205-207 (no new bind), prefix.rs:205-213 with
\*            codex.rs:229-230 (bound rollout already has output), fresh source or rollout
\*   store    adoption.rs:414-439 (init, damaged or era-only, none)
\*   adopt    adoption.rs:16-28; Refused is the stage-1 refusal state
\*   custody  o_writer_host.rs:317-350, deferred.rs:89-92 (all kinds block alike)
\*   attempts inflight.rs:341-344, recovery_paths/shared.rs:156-171 (tmux absent)
\*   otherIntake  another turn's open intake row read under the intake fence
\*            (o_channel_activation.rs fenced rows, o_channel_activation/owner.rs:25)
\*   mk       body-free recovery markers posted for the kept row (completion_delivery.rs:302-357)
\*   wakeRead a second recovery of the kept row (boot restore and ready wake,
\*            restore_inflight.rs:111 and adoption_wake.rs) has read it
\*   claim    a queued input's mailbox claim is open when the wake judges (adoption_wake.rs
\*            Held::Claimed; the kick claims, then foreign_row.rs:70-80 releases it)
\*   tries    the wake's judgements after the first, at most RETRIES = 3 (adoption_wake.rs)
\*   opened   the claim has opened once for the held row (a kick racing the wake, queue_io.rs:370)
\*   woke     the wake still judges; FALSE once its retries are spent and the row waits for the
\*            next ready transition or restart

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

\* No wait marker is stored: a row passes, while another turn's open intake, pending start or
\* terminal delivery blocks; waitMark only names the held turn's own row.
OwnCustody == turn \in {"running", "held"}
Exempt == Mut = "exemptAllRows" \/ (Fix # "main" /\ waitMark /\ turn = "held")
Blocked == ((otherCustody \/ otherIntake) /\ Mut # "exemptAllRows") \/ (OwnCustody /\ ~Exempt)

\* noSerial drops the per-channel restore lock and the disk recheck (restore_serial.rs);
\* noRetry ends the wake at its first judgement; retries4 allows a fourth retry.
Serial == Mut # "noSerial"
Retries == CASE Mut = "noRetry" -> 0 [] Mut = "retries4" -> 4 [] OTHER -> 3
\* Assumed: a queued input's claim stays open across at most MaxSpan wake judgements, within the
\* retry window (backoff 1+2+3 s; foreign_row.rs:70-80 releases it on AlreadyExists).
MaxSpan == IF Retries = 0 THEN 1 ELSE Retries
\* Only the configs that check the wake open a claim; the others keep their state space.
WakeModel == Fix = "stage1" /\ Mut \in {"none", "noRetry", "retries4"}

Init ==
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
    /\ otherIntake \in IF Fix = "stage1" THEN BOOLEAN ELSE {FALSE}
    /\ lp = 0 /\ op = 0 /\ rowCleared = FALSE /\ dispatchSettled = FALSE
    /\ attempts = 0 /\ restarts = 0 /\ recovered = FALSE /\ commitWithOther = FALSE
    /\ hturn = "none" /\ hp = 0 /\ nativeEnded = FALSE /\ endedBeforeBase = FALSE
    /\ prevOwed \in BOOLEAN /\ commitPastOwed = FALSE
    /\ mk = 0 /\ wakeRead = FALSE /\ claim = FALSE /\ tries = 0 /\ woke = TRUE /\ opened = FALSE

FixedBase == UNCHANGED <<sel, site, root, exec, bound, nativeEnded, endedBeforeBase, prevOwed,
                         commitPastOwed, otherIntake>>
Fixed == FixedBase /\ UNCHANGED <<mk, wakeRead, claim, tries, woke, opened>>

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
\* A wake that meets a claim judges only after it (WakeDefer), and not once it gave up.
ReEval == /\ Booted /\ turn = "held" /\ Answer # "held" /\ ~claim /\ woke /\ Judge("held")
          /\ Fixed
          /\ UNCHANGED <<store, oStart, otherCustody, op, dispatchSettled, attempts,
                          restarts, recovered, rejected, commitWithOther, hturn, hp>>

\* Restart recovery of a held row, once per boot (completion_delivery.rs:117-151).
Counts == Fix = "main" \/ Mut = "countWaiting"
RecoveryBody ==
    /\ Booted /\ turn = "held" /\ restarts >= 1 /\ ~recovered /\ recovered' = TRUE
    /\ IF Answer # "held"
       THEN /\ Judge("held") /\ UNCHANGED attempts
            /\ mk' = IF Answer = "owned" THEN mk + 1 ELSE mk
       ELSE IF Counts
            THEN /\ attempts' = attempts + 1
                 /\ IF attempts + 1 >= 3
                    THEN /\ rowCleared' = TRUE /\ turn' = "done" /\ waitMark' = FALSE
                    ELSE UNCHANGED <<rowCleared, turn, waitMark>>
                 /\ UNCHANGED <<lp, adopt>>
            ELSE UNCHANGED <<attempts, lp, rowCleared, turn, waitMark, adopt>>
    /\ (Answer = "held" => UNCHANGED mk)
    /\ FixedBase /\ UNCHANGED <<wakeRead, claim, tries, woke, opened>>
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
    /\ commitWithOther' = (commitWithOther \/ otherCustody \/ otherIntake)
    /\ commitPastOwed' = (commitPastOwed \/ prevOwed)
    /\ UNCHANGED <<sel, site, root, exec, bound, nativeEnded, endedBeforeBase, prevOwed,
                    otherIntake, mk, wakeRead, claim, tries, woke, opened>>
    /\ UNCHANGED <<turn, waitMark, otherCustody, lp, op, rowCleared, dispatchSettled,
                    attempts, restarts, recovered, rejected, hturn, hp>>

\* A fresh source or rollout is taken before it has output (late_adopt.rs:182-186, codex.rs:203-310).
\* Stage 1 retries while the rollout bound before still holds a turn Legacy may have sent from.
OwedClear == Fix # "stage1" \/ Mut = "swallowOwed" \/ ~prevOwed
FreshAdopt == /\ Booted /\ adopt \in {"Deferred", "Pending"} /\ bound = "fresh" /\ ~Blocked
              /\ OwedClear /\ Commit("F")
\* Warm takeover from the Legacy delivery frontier (stage 1 P3a and P3b). A turn's own end does
\* not move the frontier; advanceOnEnd starts past a turn Codex ended but nobody delivered.
\* relayedByTime counts a turn that ended before the baseline as delivered with no evidence.
SkipsEnded == nativeEnded /\ (Mut = "advanceOnEnd" \/ (Mut = "relayedByTime" /\ endedBeforeBase))
WarmAdoptF == /\ Fix = "stage1" /\ adopt = "Deferred" /\ bound # "fresh" /\ ~Blocked
              /\ OwedClear /\ Commit(IF SkipsEnded THEN "EOF" ELSE "F")

\* Codex writes the turn's end into its transcript before anyone delivered the turn.
NativeEnd == /\ turn \in {"running", "held"} /\ ~nativeEnded /\ nativeEnded' = TRUE
             /\ endedBeforeBase' \in BOOLEAN
             /\ UNCHANGED <<sel, site, root, exec, bound, store, adopt, oStart, turn, waitMark,
                             otherCustody, lp, op, rowCleared, dispatchSettled, attempts,
                             restarts, recovered, rejected, commitWithOther, hturn, hp,
                             prevOwed, commitPastOwed, otherIntake, mk, wakeRead, claim, tries, woke, opened>>

\* Legacy finishes the earlier rollout's turn, or a later read proves it unsent.
PrevDrains == /\ prevOwed /\ prevOwed' = FALSE
              /\ UNCHANGED <<sel, site, root, exec, bound, store, adopt, oStart, turn, waitMark,
                              otherCustody, lp, op, rowCleared, dispatchSettled, attempts,
                              restarts, recovered, rejected, commitWithOther, hturn, hp,
                              nativeEnded, endedBeforeBase, commitPastOwed, otherIntake, mk,
                              wakeRead, claim, tries, woke, opened>>
\* Late takeover of the source Legacy runs a turn on (late_adopt/codex.rs:284,363 and :440, which
\* claude.rs:84 takes): Legacy's running turn is taken, another turn's open intake still blocks.
\* lateExemptIntake lets every open intake row pass, as activation.rs activate_takeover did.
TakeoverBlocked == otherCustody \/ (otherIntake /\ Mut # "lateExemptIntake")
LateTakeover == /\ Fix = "stage1" /\ adopt \in {"Deferred", "Pending"} /\ ~TakeoverBlocked
                /\ OwedClear /\ Commit(IF bound # "fresh" /\ SkipsEnded THEN "EOF" ELSE "F")
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

\* Another turn's open intake row is routed or ends.
OtherIntakeEnds == /\ otherIntake /\ otherIntake' = FALSE
                   /\ UNCHANGED <<sel, site, root, exec, bound, store, adopt, oStart, turn,
                                   waitMark, otherCustody, lp, op, rowCleared, dispatchSettled,
                                   attempts, restarts, recovered, rejected, commitWithOther,
                                   hturn, hp, nativeEnded, endedBeforeBase, prevOwed,
                                   commitPastOwed, mk, wakeRead, claim, tries, woke, opened>>

\* The wake meets a queued input's claim and judges again after a backoff, at most Retries times.
WakeDefer == /\ Booted /\ turn = "held" /\ woke /\ claim /\ tries < Retries /\ tries < MaxSpan
             /\ tries' = tries + 1
             /\ FixedBase /\ UNCHANGED <<mk, wakeRead, claim, woke, opened>>
             /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, op, rowCleared,
                             dispatchSettled, attempts, restarts, recovered, rejected,
                             commitWithOther, hturn, hp>>
\* With its retries spent while the claim is still open, the wake ends and keeps the row.
WakeGiveUp == /\ Booted /\ turn = "held" /\ woke /\ claim /\ tries >= Retries /\ tries < MaxSpan
              /\ woke' = FALSE
              /\ FixedBase /\ UNCHANGED <<mk, wakeRead, claim, tries, opened>>
              /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, op,
                              rowCleared, dispatchSettled, attempts, restarts, recovered,
                              rejected, commitWithOther, hturn, hp>>
\* A queued input's kick claims the mailbox while the row is held, once (the wake races it).
ClaimOpens == /\ WakeModel /\ Booted /\ turn = "held" /\ ~opened
              /\ opened' = TRUE /\ claim' = TRUE
              /\ FixedBase /\ UNCHANGED <<mk, wakeRead, tries, woke>>
              /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, op,
                              rowCleared, dispatchSettled, attempts, restarts, recovered,
                              rejected, commitWithOther, hturn, hp>>
\* The queued input's claim ends (its create fails and foreign_row.rs:70-80 releases it).
ClaimEnds == /\ claim /\ claim' = FALSE
             /\ FixedBase /\ UNCHANGED <<mk, wakeRead, tries, woke, opened>>
             /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, op, rowCleared,
                             dispatchSettled, attempts, restarts, recovered, rejected,
                             commitWithOther, hturn, hp>>

\* A second recovery of the kept row reads it while the first may still be posting.
SecondRead == /\ Booted /\ turn = "held" /\ ~rowCleared /\ ~wakeRead /\ wakeRead' = TRUE
              /\ FixedBase /\ UNCHANGED <<mk, claim, tries, woke, opened>>
              /\ UNCHANGED <<store, adopt, oStart, turn, waitMark, otherCustody, lp, op,
                              rowCleared, dispatchSettled, attempts, restarts, recovered,
                              rejected, commitWithOther, hturn, hp>>
\* It posts its marker; serialized, it first rechecks that the row is still that turn.
SecondPost == /\ wakeRead /\ Answer = "owned" /\ (Serial => ~rowCleared)
              /\ mk' = mk + 1 /\ wakeRead' = FALSE
              /\ rowCleared' = TRUE /\ turn' = "done" /\ waitMark' = FALSE
              /\ FixedBase /\ UNCHANGED <<claim, tries, woke, opened>>
              /\ UNCHANGED <<store, adopt, oStart, otherCustody, lp, op, dispatchSettled,
                              attempts, restarts, recovered, rejected, commitWithOther, hturn, hp>>

Rest == UNCHANGED vars
Next == Input \/ TerminalBody \/ ReEval \/ RecoveryBody \/ WatcherDispatch \/ FreshAdopt
        \/ WarmAdoptF \/ EofHandoff \/ Reject \/ OPost \/ OtherTurnEnds \/ Restart \/ NativeEnd \/ PrevDrains
        \/ LateTakeover \/ OtherIntakeEnds \/ WakeDefer \/ WakeGiveUp \/ ClaimOpens \/ ClaimEnds
        \/ SecondRead \/ SecondPost
        \/ HeadlessInput \/ HeadlessTerminal \/ Rest

\* Writer, recovery and turn-end actions are fair. Input, Restart, Reject, the stale handoff,
\* the watcher finalizer and Codex's own turn end are environment.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(TerminalBody) /\ WF_vars(ReEval) /\ WF_vars(RecoveryBody)
        /\ WF_vars(FreshAdopt) /\ WF_vars(WarmAdoptF) /\ WF_vars(OPost)
        /\ WF_vars(OtherTurnEnds) /\ WF_vars(HeadlessTerminal) /\ WF_vars(PrevDrains)
        /\ WF_vars(LateTakeover) /\ WF_vars(OtherIntakeEnds) /\ WF_vars(WakeDefer)
        /\ WF_vars(ClaimEnds)

NoLegacyBodyOnTui == lp = 0
AnswerAtMostOnce == op + lp <= 1
UnadoptedNotOwned == store = "none" => Answer # "owned"
RowClearedHasEvidence == rowCleared => Evidence
NoEarlyDispatch == dispatchSettled => Evidence
HeldBodyNotSkipped == (turn = "held" /\ adopt = "Committed") => oStart = "F"
OtherCustodyBlocks == ~commitWithOther
NoCommitPastOwed == ~commitPastOwed
HeadlessKept == hturn # "held" /\ (hturn = "done" => hp = 1)
MarkerAtMostOnce == mk <= 1
TriesBound == tries \in 0..3

HeldTurnAnswered == [](turn = "held" /\ Adoptable => <>(op = 1 \/ rejected))
AdoptionSettles == [](adopt = "Deferred" /\ Adoptable /\ turn # "none"
                      => <>(adopt = "Committed" \/ rejected))
RowEventuallySettled == [](op = 1 => <>rowCleared)
\* A late takeover held by another turn's open intake commits once that input ends.
TakeoverSettles == [](otherIntake /\ Adoptable /\ adopt \in {"Deferred", "Pending"}
                      => <>(adopt = "Committed" \/ rejected))
=============================================================================
