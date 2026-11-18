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
VARIABLES ne, np, ticks, repairs, proof, cause, nb, nr, nl, retained, tomb, ever, reasonPrepared, noticeRestarts, ni,
          views, enters, elapsed
normVars == <<ne, np, ticks, repairs, proof, cause, nb, nr, nl, retained, tomb, ever, reasonPrepared, noticeRestarts, ni,
              views, enters, elapsed>>
NormDormant == /\ ne = 0 /\ np = 0 /\ ticks = 0 /\ repairs = 0 /\ proof = 0 /\ cause = 0
               /\ nb = 0 /\ nr = 0 /\ nl = 0 /\ retained = 0 /\ tomb = 0 /\ ever = 0 /\ reasonPrepared = 0 /\ noticeRestarts = 0 /\ ni = 0
               /\ views = 0 /\ enters = 0 /\ elapsed = 0
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
    /\ NormDormant
    /\ lateModal = FALSE /\ lateBound = FALSE /\ lateOutput = FALSE
    /\ lateBoundFirst = FALSE /\ lateJudge = "waiting"
    /\ lateCommits = 0 /\ lateSkips = 0 /\ lateEnd = "waiting"
    /\ lateKind = "clean" /\ lateCheckedOutput = FALSE /\ lateLife = "live"
    /\ lateSends = 0 /\ lateBadCommit = FALSE /\ lateRestored = FALSE
    /\ lateRevision = 0 /\ lateCheckedRevision = 0
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
\* Assumed: no restart lands inside a queued input's claim (it lasts under one wake backoff).
Restart == /\ Booted /\ restarts < 3 /\ turn \in {"running", "held"} /\ ~claim
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
\* A queued input's kick claims the mailbox while the row is held, once (the wake races it). It
\* races the wake of this process's ready transition, before a restart or a second recovery.
ClaimOpens == /\ WakeModel /\ Booted /\ turn = "held" /\ ~opened /\ restarts = 0 /\ ~wakeRead
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
LegacyNext == Input \/ TerminalBody \/ ReEval \/ RecoveryBody \/ WatcherDispatch \/ FreshAdopt
        \/ WarmAdoptF \/ EofHandoff \/ Reject \/ OPost \/ OtherTurnEnds \/ Restart \/ NativeEnd \/ PrevDrains
        \/ LateTakeover \/ OtherIntakeEnds \/ WakeDefer \/ WakeGiveUp \/ ClaimOpens \/ ClaimEnds
        \/ SecondRead \/ SecondPost
        \/ HeadlessInput \/ HeadlessTerminal \/ Rest
Next == LegacyNext /\ UNCHANGED <<lateVars, normVars>>

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

\* Cold-launch domain: codex-late-launch.report.md entry census. Native records and Discord
\* sends are distinct. lateBuffered changes prefix acceptance, not the argv/input protocol.
LateCoreInit == /\ sel = 0 /\ site = 0 /\ root = 0 /\ exec = 0 /\ bound = 0
            /\ store = 0 /\ adopt = 0 /\ oStart = 0 /\ turn = 0 /\ waitMark = 0
            /\ otherCustody = 0 /\ lp = 0 /\ op = 0 /\ rowCleared = 0
            /\ dispatchSettled = 0 /\ attempts = 0 /\ restarts = 0 /\ recovered = 0
            /\ rejected = 0 /\ commitWithOther = 0 /\ hturn = 0 /\ hp = 0
            /\ nativeEnded = 0 /\ endedBeforeBase = 0 /\ prevOwed = 0 /\ commitPastOwed = 0
            /\ otherIntake = 0 /\ mk = 0 /\ wakeRead = 0 /\ claim = 0 /\ tries = 0 /\ woke = 0 /\ opened = 0
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

LateInit == LateCoreInit /\ NormDormant

LateActions == LateBind \/ LateNativeOutput \/ LateAppend \/ LateJudge \/ LateCommit
            \/ LateStartupEnd \/ LateInvalidate \/ LateRestore
            \/ (Mut \in {"admissionHold", "unfenced", "legacyFallback"} /\ LateLegacySend)
LateNext == LateActions /\ UNCHANGED normVars
LateSpec == LateInit /\ [][LateNext]_lateVars
            /\ WF_lateVars(LateBind) /\ WF_lateVars(LateNativeOutput)
            /\ WF_lateVars(LateJudge) /\ WF_lateVars(LateCommit)
            /\ WF_lateVars(LateStartupEnd) /\ WF_lateVars(LateRestore)

LateBindingPrecedesOutput == lateOutput => lateBound
LateCommitAtMostOnce == lateCommits <= 1
LateBoundBeforeNeverSkipped == lateBoundFirst => lateSkips = 0
LateBoundBeforeCommits == [](lateBoundFirst => <>(lateCommits = 1))
LateModalExplained == [](lateModal => <>(lateEnd = "reasonedFailure"))
LateNoUnsafeCommit == ~lateBadCommit
LateNoRejectedLegacy == lateSends = 0
LateEligibleCommits == []((lateBound /\ LateValid /\ lateLife = "live" /\ lateSends = 0)
                          => <>(lateCommits = 1 \/ ~LateValid \/ lateLife # "live"))
LateRestartRestores == [](lateLife = "restarted" => <>lateRestored)

\* Per-output policy is uninstalled in D1. These actions describe its explicit test port.
\* A/B have independent receipts: a pending notice never holds a distinct new input.
NormInit == /\ LateCoreInit /\ lateModal = FALSE /\ lateKind = "clean" /\ ne = 0 /\ np = [e \in 0..1 |-> "normalizing"]
            /\ ticks = [e \in 0..1 |-> 0] /\ repairs = [e \in 0..1 |-> 0]
            /\ proof = [e \in 0..1 |-> FALSE]
            /\ cause \in [0..1 -> {"binding", "trust", "missing", "invalid"}]
            /\ cause[1] = "binding"
            /\ nb = [e \in 0..1 |-> 0] /\ nr = [e \in 0..1 |-> 0]
            /\ nl = 0 /\ retained = [s \in {"localDebt", "sharedManifest", "inflight", "terminalCustody"} |-> 0]
            /\ tomb = [e \in 0..1 |-> FALSE]
            /\ ever = [e \in 0..1 |-> FALSE]
            /\ reasonPrepared = [e \in 0..1 |-> FALSE]
            /\ noticeRestarts = [e \in 0..1 |-> 0]
            /\ ni = [e \in 0..1 |-> "none"]
            /\ views = [e \in 0..1 |-> 0] /\ enters = [e \in 0..1 |-> 0] /\ elapsed = [e \in 0..1 |-> 0]
NormStable == UNCHANGED <<vars, lateVars>>
NormRepair(e) == /\ np[e] = "normalizing" /\ repairs[e] = 0 /\ ticks[e] < 3
                 /\ Fix # "noRepair"
                 /\ repairs' = [repairs EXCEPT ![e] = 1]
                 /\ enters' = [enters EXCEPT ![e] = @ + 1]
                 /\ proof' = [proof EXCEPT ![e] = (cause[e] \in {"binding", "trust"})]
                 /\ UNCHANGED <<ne, np, ticks, cause, nb, nr, nl, retained, tomb, ever,
                                 reasonPrepared, noticeRestarts, ni, views, elapsed>>
NormTick(e) == /\ np[e] = "normalizing" /\ ticks[e] < 3
               /\ ticks' = [ticks EXCEPT ![e] = IF Mut = "deadlineReset" THEN 0 ELSE @ + 1]
               \* Capped past the deadline: one tick beyond it is all a rule needs to see.
               /\ elapsed' = [elapsed EXCEPT ![e] = IF @ < 4 THEN @ + 1 ELSE @]
               /\ UNCHANGED <<ne, np, repairs, proof, cause, nb, nr, nl, retained, tomb, ever,
                               reasonPrepared, noticeRestarts, ni, views, enters>>
\* Init publication and terminal refusal take the same candidate lock; either may win at expiry.
NormActorInit(e) == /\ ni[e] = "none"
                    /\ ((np[e] = "normalizing" /\ ~tomb[e] /\ (proof[e] \/ Mut = "earlyPublish"))
                        \/ (Mut = "actorAfterReject" /\ tomb[e] /\ proof[e]))
                    /\ ni' = [ni EXCEPT ![e] = IF proof[e] THEN "proved" ELSE "unproved"]
                    /\ UNCHANGED <<ne, np, ticks, repairs, proof, cause, nb, nr, nl, retained,
                                    tomb, ever, reasonPrepared, noticeRestarts, views, enters, elapsed>>
\* A published init remains adoption evidence after expiry or a new current execution.
NormAdopt(e) == /\ np[e] = "normalizing" /\ ni[e] # "none" /\ ~tomb[e]
                /\ np' = [np EXCEPT ![e] = IF nb[e] = 1 THEN "posted" ELSE "adopted"]
                /\ UNCHANGED <<ne, ticks, repairs, proof, cause, nb, nr, nl, retained, tomb, ever,
                                reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
NormInvalidate(e) == /\ np[e] = "normalizing" /\ cause[e] # "invalid"
                     /\ cause' = [cause EXCEPT ![e] = "invalid"]
                     /\ proof' = [proof EXCEPT ![e] = FALSE]
                     /\ UNCHANGED <<ne, np, ticks, repairs, nb, nr, nl, retained, tomb, ever,
                                     reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
NormReject(e) == /\ ((np[e] = "normalizing" /\ ni[e] = "none" /\ ((~proof[e] /\ cause[e] = "invalid") \/ ticks[e] = 3))
                    \/ (Mut = "mixed" /\ np[e] = "posted"))
                 /\ np' = [np EXCEPT ![e] = "rejectPending"]
                 /\ tomb' = [tomb EXCEPT ![e] = TRUE] /\ ever' = [ever EXCEPT ![e] = TRUE]
                 /\ nl' = IF Mut = "legacy" THEN 1 ELSE nl
                 /\ retained' = IF Mut \in {"retain", "custodyThenScrub"}
                                 THEN [retained EXCEPT !["terminalCustody"] = 1] ELSE retained
                 /\ UNCHANGED <<ne, ticks, repairs, proof, cause, nb, nr, reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
\* Adoption/HTTP and reason reservation/HTTP/receipt are distinct crash boundaries.
NormBody(e) == /\ ni[e] # "none" /\ nb[e] = 0
               /\ (~tomb[e] \/ Mut = "actorAfterReject")
               /\ nb' = [nb EXCEPT ![e] = 1]
               /\ np' = [np EXCEPT ![e] = IF np[e] = "adopted" THEN "posted" ELSE @]
               /\ UNCHANGED <<ne, ticks, repairs, proof, cause, nr, nl, retained, tomb, ever,
                               reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
NormReserveReason(e) == /\ np[e] = "rejectPending" /\ ~reasonPrepared[e]
                        /\ reasonPrepared' = [reasonPrepared EXCEPT ![e] = TRUE]
                        /\ UNCHANGED <<ne, np, ticks, repairs, proof, cause, nb, nr, nl, retained,
                                        tomb, ever, noticeRestarts, ni, views, enters, elapsed>>
NormReason(e) == /\ np[e] = "rejectPending" /\ reasonPrepared[e] /\ Mut # "noticeUnavailable"
                 /\ nr' = IF Mut = "silent" THEN nr ELSE [nr EXCEPT ![e] = @ + 1]
                 /\ np' = [np EXCEPT ![e] = "reasonEffect"]
                 /\ UNCHANGED <<ne, ticks, repairs, proof, cause, nb, nl, retained, tomb, ever,
                                 reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
NormConfirm(e) == /\ np[e] = "reasonEffect" /\ np' = [np EXCEPT ![e] = "rejected"]
                  /\ UNCHANGED <<ne, ticks, repairs, proof, cause, nb, nr, nl, retained, tomb, ever,
                                  reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
NormRestart(e) == /\ np[e] \in {"rejectPending", "reasonEffect", "rejected"}
                  /\ noticeRestarts[e] = 0 /\ noticeRestarts' = [noticeRestarts EXCEPT ![e] = 1]
                  /\ np' = [np EXCEPT ![e] = IF np[e] = "reasonEffect" \/
                              (np[e] = "rejectPending" /\ reasonPrepared[e])
                            THEN "noticeUncertain" ELSE @]
                  /\ UNCHANGED <<ne, ticks, repairs, proof, cause, nb, nr, nl, retained, tomb, ever,
                                  reasonPrepared, ni, views, enters, elapsed>>
\* Only exact history or no-effect evidence resolves an interrupted reservation.
NormHistory(e) == /\ np[e] = "noticeUncertain" /\ nr[e] = 1
                  /\ np' = [np EXCEPT ![e] = "reasonEffect"]
                  /\ UNCHANGED <<ne, ticks, repairs, proof, cause, nb, nr, nl, retained, tomb, ever,
                                  reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
NormNoEffect(e) == /\ np[e] = "noticeUncertain" /\ (nr[e] = 0 \/ Mut = "uncertainRepost")
                   /\ np' = [np EXCEPT ![e] = "rejectPending"]
                   /\ reasonPrepared' = [reasonPrepared EXCEPT ![e] = FALSE]
                   /\ UNCHANGED <<ne, ticks, repairs, proof, cause, nb, nr, nl, retained, tomb, ever,
                                   noticeRestarts, ni, views, enters, elapsed>>
NormRepeatReason(e) == /\ Mut = "reasonDuplicate" /\ np[e] = "rejected" /\ nr[e] = 1
                       /\ nr' = [nr EXCEPT ![e] = 2]
                       /\ UNCHANGED <<ne, np, ticks, repairs, proof, cause, nb, nl, retained, tomb, ever,
                                       reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
\* Only NewInput admits B; store/restart never renew A's deadline or clear its tombstone.
NormNewInput == /\ ne = 0 /\ np[0] # "normalizing"
                /\ ne' = 1 /\ proof' = [proof EXCEPT ![1] = TRUE]
                /\ cause' = [cause EXCEPT ![1] = "binding"]
                /\ tomb' = IF Mut = "revive" THEN [e \in 0..1 |-> FALSE] ELSE tomb
                /\ UNCHANGED <<np, ticks, repairs, nb, nr, nl, retained, ever, reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
\* The rollout proof makes the admission's output view: it keeps the admission's deadline and its
\* one repair, which a trust or binding repair before or after the proof spends.
NormProve(e) == /\ np[e] = "normalizing" /\ views[e] = 0
                /\ views' = [views EXCEPT ![e] = 1]
                /\ repairs' = IF Mut = "proofRenewsRepair" THEN [repairs EXCEPT ![e] = 0] ELSE repairs
                /\ ticks' = IF Mut = "proofRestartsDeadline" THEN [ticks EXCEPT ![e] = 0] ELSE ticks
                /\ UNCHANGED <<ne, np, proof, cause, nb, nr, nl, retained, tomb, ever,
                                reasonPrepared, noticeRestarts, ni, enters, elapsed>>
\* A custody kept before its reason and scrubbed after it, as resume once did.
NormScrub(e) == /\ Mut = "custodyThenScrub" /\ tomb[e] /\ retained["terminalCustody"] = 1
                /\ retained' = [retained EXCEPT !["terminalCustody"] = 0]
                /\ UNCHANGED <<ne, np, ticks, repairs, proof, cause, nb, nr, nl, tomb, ever,
                                reasonPrepared, noticeRestarts, ni, views, enters, elapsed>>
NormEpisodeNext(e) == NormRepair(e) \/ NormProve(e) \/ NormScrub(e) \/ NormTick(e) \/ NormActorInit(e) \/ NormAdopt(e) \/ NormInvalidate(e) \/ NormReject(e) \/ NormBody(e)
                     \/ NormReserveReason(e) \/ NormReason(e) \/ NormConfirm(e) \/ NormRestart(e)
                     \/ NormRepeatReason(e) \/ NormHistory(e) \/ NormNoEffect(e)
NormNext == /\ NormStable /\ ((\E e \in 0..ne : NormEpisodeNext(e)) \/ NormNewInput)
NormFair(e) == /\ WF_normVars(e <= ne /\ NormTick(e))
               /\ WF_normVars(e <= ne /\ NormRepair(e))
               /\ WF_normVars(e <= ne /\ NormAdopt(e))
               /\ WF_normVars(e <= ne /\ NormActorInit(e))
               /\ WF_normVars(e <= ne /\ NormReject(e))
               /\ WF_normVars(e <= ne /\ NormBody(e))
               /\ WF_normVars(e <= ne /\ NormReserveReason(e))
               /\ WF_normVars(e <= ne /\ NormReason(e))
               /\ WF_normVars(e <= ne /\ NormConfirm(e))
               /\ WF_normVars(e <= ne /\ NormHistory(e))
               /\ WF_normVars(e <= ne /\ NormNoEffect(e))
NormSpec == NormInit /\ [][NormNext]_normVars /\ (\A e \in 0..1 : NormFair(e))
            /\ WF_normVars(NormNewInput)
NormNoLegacy == nl = 0
NormNoRetention == \A s \in DOMAIN retained : retained[s] = 0
NormAtMostOnce == \A e \in 0..ne : nb[e] <= 1 /\ nr[e] <= 1
NormExclusiveEffects == \A e \in 0..ne : ~(nb[e] > 0 /\ nr[e] > 0)
NormNoRevival == \A e \in 0..ne : ever[e] => tomb[e]
NormSafeAdoption == \A e \in 0..ne : np[e] \in {"adopted", "posted"} => ni[e] = "proved"
NormNoActorAfterRejected == \A e \in 0..ne : tomb[e] => (ni[e] = "none" /\ nb[e] = 0)
NormOneRepair == \A e \in 0..ne : enters[e] <= 1
NormDeadlineKept == \A e \in 0..ne : np[e] = "normalizing" => elapsed[e] <= 3
NormSettledEvidence == \A e \in 0..ne : np[e] = "rejected" => nr[e] >= 1
NormEventuallyEnds == \A e \in 0..1 : []((e <= ne /\ np[e] = "normalizing") => <>(np[e] # "normalizing"))
NormOutputSettles == \A e \in 0..1 : []((e <= ne /\ np[e] = "adopted") => <>(nb[e] = 1))
NormReasonSettles == \A e \in 0..1 : []((e <= ne /\ np[e] = "rejectPending") => <>(nr[e] = 1))
NormNextInputProgresses == <>(ne = 1 /\ np[1] # "normalizing")

=============================================================================
