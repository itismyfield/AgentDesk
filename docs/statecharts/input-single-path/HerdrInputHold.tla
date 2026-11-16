---- MODULE HerdrInputHold ----
\* #8201 projection: one held original on one live Bound herdr execution, one later input.
\* Refines HerdrBusyInput's "uncertain" phase for the herdr execution-nonce input hold
\* (runtime/herdr_input_holds/<nonce>) and the readiness reservation the same input consumed.
\* Value ranges come from the entry-point table in docs/design/8201-herdr-input-hold.design.md.
\*
\* Write   : claude/herdr_turn/first_input.rs `write` -> herdr_turn.rs `hold` before the paste;
\*           composer.rs `when_idle` -> readiness `reserve` (Entry.pending) before the send.
\* Outcome : host_input.rs InputRun. Scenario names the uncertain kind and what happens next:
\*   "enterRefused"  Indeterminate{confirmed:1, Refused(..)}: paste landed, Enter never sent
\*                   (the 11-16 incident, cause PaneUnverified); also Cancelled{confirmed:1}.
\*   "loadSpike"     enterRefused, then the first captures fail under load and recover.
\*   "enterSendFail" Indeterminate{confirmed:1, Send(..)}: Enter may have reached the pane.
\*   "pasteSendFail" Indeterminate{confirmed:0, Send(..)}: the paste may have reached the pane.
\*   "personLeaves" / "personStays": a person types into the emptied composer, then leaves or not.
\*   "personEdits"   a person appends to our pasted text in the pane (same `[User:` head).
\*   "stashOther"    a person C-s parks our text, submits another input B, the stash returns.
\*   "unreadable"    the composer never reads.
\* Readers : herdr_turn.rs `not_held` (main) / the pre-key resolve in `write` (design);
\*           busy injection (host_input/busy/herdr.rs) never reads the hold.
\* Release : composer_settled (C1), execution end C5..C10, design `resolve_hold`.
EXTENDS Naturals
CONSTANTS Policy,    \* "before" (main 934a49997e) | "fixed" (design rev 2)
          Scenario,
          Ends,      \* the execution ends while the later input waits (row end/terminate)
          Restart,   \* one dcserver restart: the hold file survives, process memory does not
          Identity,  \* "digest" (hold v2) | "legacy" (v1 file: a time only, written pre-deploy)
          Mutant     \* "None" or one removed rule of the design
VARIABLE h
vars == <<h>>

EnterMay == Scenario = "enterSendFail"
PasteMay == Scenario = "pasteSendFail"
\* The v2 record's phase proves the last key was not sent (Refused before send, or no Enter).
EnterNotSent == Identity = "digest" /\ ~EnterMay

Init == \E landed \in (IF EnterMay THEN {"no", "pending"} ELSE {"no"}),
           pasted \in (IF PasteMay THEN {TRUE, FALSE} ELSE {TRUE}) :
          h = [composer |-> IF landed = "pending" \/ ~pasted THEN "empty" ELSE "ours",
               rendered |-> landed = "pending" \/ ~pasted,
               landed |-> landed,       \* original's Enter: no | pending (no transcript) | yes
               busy |-> FALSE,          \* the original's turn runs (transcript shows it)
               otherBusy |-> FALSE,     \* another input's turn runs
               otherUser |-> FALSE,     \* the transcript holds another user record
               stash |-> FALSE,         \* our text parked by C-s
               hold |-> TRUE,
               \* readiness Entry.pending of this input: process memory; a v1 hold is only
               \* met after the deploy restart, so its reservation is already gone.
               reserved |-> Identity = "digest",
               lock |-> FALSE,          \* composer lock held between settle and unlink
               clearedOurs |-> FALSE,   \* durable phase "cleared" in the hold record
               readable |-> Scenario # "unreadable",
               person |-> "none",
               pasteSettled |-> FALSE, receiptSettled |-> FALSE,
               origSubmits |-> IF landed = "pending" THEN 1 ELSE 0,
               disposition |-> "unknown",
               nxt |-> "queued", refusals |-> 0,
               unclear |-> 0, noticed |-> FALSE,  \* durable in the hold record
               notices |-> 0,                     \* channel notices actually sent (ghost)
               trigger |-> TRUE,                  \* a resolve pass is due (start, wake, timer)
               watch |-> FALSE,                   \* volatile evidence watch (P3)
               humanDeleted |-> FALSE, merged |-> FALSE, releasedOnResidue |-> FALSE,
               ended |-> FALSE, up |-> TRUE, restarts |-> 0]

Live == h.up /\ ~h.ended
Busy(r) == r.busy \/ r.otherBusy
\* The existing classifier: an unrendered paste reads empty; a stash does not show on Empty.
Existing(r) == IF ~r.readable THEN "unread"
               ELSE IF r.composer = "ours" /\ ~r.rendered THEN "empty"
               ELSE r.composer
\* The resolver's view: only draft_sighting Settled (no stash, normal footer) is Empty.
Shown(r) == IF r.stash /\ r.readable THEN "unread" ELSE Existing(r)

NoSettleOn == Mutant \in {"ResendNoSettle"}
PasteOK == h.pasteSettled \/ NoSettleOn \/ (Mutant = "SkipPasteSettle" /\ EnterNotSent)
ReceiptOK == h.receiptSettled \/ NoSettleOn

\* An evidence change wakes a registered watch (design: any change; mutant: Empty only).
Wake(r) == [r EXCEPT !.trigger = @ \/ (r.watch /\ (Mutant # "EmptyOnlyWake"
                                                    \/ Shown(r) = "empty"))]

\* ---------------- environment ----------------
Render == /\ h.composer = "ours" /\ ~h.rendered
          /\ h' = Wake([h EXCEPT !.rendered = TRUE])
\* Assumption handed to S1 tests: the paste window outlasts rendering, the receipt window
\* outlasts the transcript write of a landed Enter.
PasteWindow == /\ ~h.pasteSettled /\ (h.composer # "ours" \/ h.rendered)
               /\ h' = Wake([h EXCEPT !.pasteSettled = TRUE])
ReceiptWindow == /\ ~h.receiptSettled /\ h.landed # "pending"
                 /\ h' = Wake([h EXCEPT !.receiptSettled = TRUE])
TranscriptWrite == /\ h.landed = "pending"
                   /\ h' = Wake([h EXCEPT !.landed = "yes", !.busy = TRUE])
\* The turn's terminal settles the readiness reservation the natural way.
TurnEnd == /\ h.busy /\ h.landed = "yes"
           /\ h' = Wake([h EXCEPT !.busy = FALSE, !.reserved = FALSE])
LoadSpike == /\ Scenario = "loadSpike" /\ h.readable /\ h.unclear = 0 /\ h.hold
             /\ h' = [h EXCEPT !.readable = FALSE]
LoadEnds == /\ ~h.readable /\ Scenario # "unreadable"
            /\ h' = Wake([h EXCEPT !.readable = TRUE])
PersonTypes == /\ Scenario \in {"personLeaves", "personStays"} /\ h.person = "none"
               /\ h.composer = "empty"
               /\ h' = Wake([h EXCEPT !.composer = "person", !.person = "typing"])
PersonEdits == /\ Scenario = "personEdits" /\ h.person = "none"
               /\ h.composer = "ours" /\ h.rendered
               /\ h' = Wake([h EXCEPT !.composer = "edited", !.person = "typing"])
\* The person submits (or clears) their text; a submit is another user record.
PersonLeaves == /\ Scenario \in {"personLeaves", "personEdits"} /\ h.person = "typing"
                /\ h' = Wake([h EXCEPT !.composer = "empty", !.person = "gone",
                                       !.otherUser = TRUE])
\* C-s parks our text, the person submits B; Claude hands the stash back on submit.
PersonStashSubmit == /\ Scenario = "stashOther" /\ h.person = "none" /\ ~Busy(h)
                     /\ h.composer = "ours" /\ h.rendered
                     /\ h' = Wake([h EXCEPT !.composer = "empty", !.stash = TRUE,
                                            !.otherUser = TRUE, !.otherBusy = TRUE,
                                            !.person = "used"])
StashReturns == /\ h.stash /\ h.composer = "empty"
                /\ h' = Wake([h EXCEPT !.composer = "ours", !.stash = FALSE])
\* B's UserPromptSubmit replaced the pending fence; B's terminal takes it.
OtherTurnEnd == /\ h.otherBusy
                /\ h' = Wake([h EXCEPT !.otherBusy = FALSE, !.reserved = FALSE])

\* ---------------- existing paths ----------------
StartBefore == /\ Policy = "before" /\ Live /\ ~Busy(h) /\ h.nxt \in {"queued", "refused"}
               /\ h.hold
               /\ h' = [h EXCEPT !.refusals = @ + 1,
                                 !.nxt = IF h.nxt = "refused" THEN "retained" ELSE "refused"]
\* when_idle: no hold, no pending reservation, composer shown empty (Person/AgentDesk refuse).
Paste == /\ Live /\ ~Busy(h) /\ h.nxt \in {"queued", "held"}
         /\ ~h.hold /\ ~h.reserved /\ ~h.lock /\ Existing(h) = "empty"
         /\ h' = [h EXCEPT !.nxt = "submitted",
                           !.merged = @ \/ h.composer \in {"ours", "edited", "person"}]
BusyInject == /\ Live /\ h.busy /\ h.nxt = "queued" /\ Existing(h) = "empty"
              /\ h' = [h EXCEPT !.nxt = "submitted",
                                !.merged = @ \/ h.composer \in {"ours", "edited", "person"}]
End == /\ Ends /\ h.up /\ ~h.ended /\ ~Busy(h)
       /\ h' = [h EXCEPT !.ended = TRUE, !.hold = FALSE]
Crash == /\ Restart /\ h.up /\ h.restarts = 0
         /\ h' = [h EXCEPT !.up = FALSE, !.restarts = 1]
\* Memory (reservation, lock, watch) is gone; the owner's recovered front drives a new pass.
Boot == /\ ~h.up
        /\ h' = [h EXCEPT !.up = TRUE, !.reserved = FALSE, !.lock = FALSE, !.watch = FALSE,
                          !.trigger = TRUE,
                          !.unclear = IF Mutant = "VolatileCounter" THEN 0 ELSE @,
                          !.noticed = IF Mutant = "VolatileCounter" THEN FALSE ELSE @]

\* ---------------- design: resolve where the hold is read ----------------
\* Pre-key step of `write`, inside the owner effect and the composer mutation lock, on a
\* gate-confirmed execution. A kept hold answers Held (front kept, watch registered).
UnclearLimit == 2
Keep(r) == [r EXCEPT !.nxt = IF Mutant = "OpaqueFailure"
                             THEN (IF r.nxt = "queued" THEN "refused"
                                   ELSE IF r.nxt = "refused" THEN "retained" ELSE r.nxt)
                             ELSE (IF r.nxt = "queued" THEN "held" ELSE r.nxt),
                     !.refusals = IF Mutant = "OpaqueFailure" /\ r.nxt \in {"queued", "refused"}
                                  THEN @ + 1 ELSE @,
                     !.trigger = FALSE, !.watch = TRUE]
\* Undecidable: count it durably; retry by timer until the single notice is out.
Bump(r) == LET u == IF r.unclear < UnclearLimit THEN r.unclear + 1 ELSE r.unclear
               send == u >= UnclearLimit /\ ~r.noticed
           IN [Keep(r) EXCEPT !.unclear = u,
                              !.noticed = r.noticed \/ send,
                              !.notices = IF send THEN r.notices + 1 ELSE r.notices,
                              !.trigger = ~(r.noticed \/ send) /\ Mutant # "NoSelfRetry"]
\* Every hold removal goes through here; the residue flag reads the real state.
Unhold(r) == [r EXCEPT !.hold = FALSE, !.lock = FALSE,
                       !.releasedOnResidue = @ \/ r.stash
                                             \/ r.composer \in {"ours", "edited"}]
DeliveredSeen == \/ Identity = "digest" /\ h.landed = "yes" /\ Mutant # "IgnoreTranscript"
                 \/ Mutant = "TimeReceipt" /\ (h.landed = "yes" \/ h.otherUser)
UnsentProof == Identity = "digest" /\ (EnterNotSent \/ h.clearedOurs \/ Mutant = "ResendNoSettle")
\* Step 1 of the release unit: settle exactly this input's reservation, keep the lock.
Settle(disp) == [h EXCEPT !.lock = TRUE,
                          !.reserved = IF Mutant = "NoSettleReservation" THEN @ ELSE FALSE,
                          !.disposition = disp,
                          !.origSubmits = IF Mutant = "ResendNoSettle" THEN @ + 1 ELSE @]

Resolve == /\ Policy = "fixed" /\ Live /\ h.hold /\ h.trigger /\ ~h.lock
           /\ h' = CASE Busy(h) -> Keep(h)
                     [] DeliveredSeen -> Unhold([h EXCEPT !.disposition = "delivered"])
                     [] Shown(h) = "unread" /\ Mutant # "ReleaseOnUnread" -> Bump(h)
                     [] ~PasteOK -> Keep(h)
                     [] Shown(h) \in {"empty", "unread"} /\ UnsentProof ->
                            Settle("undelivered")
                     [] Shown(h) \in {"empty", "unread"} /\ Identity = "legacy" ->
                            Settle("unknown")
                     [] Shown(h) = "empty" /\ ~ReceiptOK -> Keep(h)
                     [] Shown(h) = "ours" /\ Mutant = "ReleaseOnResidue" -> Unhold(h)
                     \* exact digest only: gated clear, then the same pass re-reads
                     [] Shown(h) = "ours" /\ Identity = "digest" ->
                            [h EXCEPT !.composer = "empty", !.clearedOurs = TRUE]
                     [] Shown(h) = "edited" /\ Mutant = "ShapeDelete" ->
                            [h EXCEPT !.composer = "empty", !.clearedOurs = TRUE,
                                      !.humanDeleted = TRUE]
                     [] OTHER -> Bump(h)
\* Step 2: unlink and fsync the hold, then drop the lock.
Unlink == /\ h.lock /\ h.up
          /\ h' = Unhold(h)

Stay == UNCHANGED vars

Next == Render \/ PasteWindow \/ ReceiptWindow \/ TranscriptWrite \/ TurnEnd
        \/ LoadSpike \/ LoadEnds \/ PersonTypes \/ PersonEdits \/ PersonLeaves
        \/ PersonStashSubmit \/ StashReturns \/ OtherTurnEnd
        \/ StartBefore \/ Paste \/ BusyInject \/ End \/ Crash \/ Boot
        \/ Resolve \/ Unlink \/ Stay

Fairness == /\ WF_vars(Render) /\ WF_vars(PasteWindow) /\ WF_vars(ReceiptWindow)
            /\ WF_vars(TranscriptWrite) /\ WF_vars(TurnEnd) /\ WF_vars(LoadEnds)
            /\ WF_vars(PersonLeaves) /\ WF_vars(StashReturns) /\ WF_vars(OtherTurnEnd)
            /\ WF_vars(StartBefore) /\ WF_vars(Paste) /\ WF_vars(BusyInject)
            /\ WF_vars(End) /\ WF_vars(Boot) /\ WF_vars(Resolve) /\ WF_vars(Unlink)
Spec == Init /\ [][Next]_vars /\ Fairness

\* ---------------- rules ----------------
TypeOK == /\ h.composer \in {"empty", "ours", "edited", "person"}
          /\ h.landed \in {"no", "pending", "yes"}
          /\ h.nxt \in {"queued", "held", "refused", "retained", "submitted"}
          /\ h.disposition \in {"unknown", "delivered", "undelivered"}
          /\ h.origSubmits \in 0..2 /\ h.unclear \in 0..UnclearLimit /\ h.notices \in 0..2
\* Safety: the same input is submitted at most once.
OriginalAtMostOnce == h.origSubmits <= 1
\* Safety: no new input lands on text left in the composer.
NoPasteOverResidue == ~h.merged
\* Safety: a person's text is never deleted.
NoHumanTextDeleted == ~h.humanDeleted
\* Safety: the hold is never removed while our text sits in the composer or the stash.
NoReleaseOnResidue == ~h.releasedOnResidue
\* Safety: "undelivered" / "delivered" are told only when true of the original.
UndeliveredIsTrue == h.disposition = "undelivered" => h.landed = "no"
DeliveredIsTrue == h.disposition = "delivered" => h.landed = "yes"
\* Safety: the channel is told at most once, across restarts.
NoticeAtMostOnce == h.notices <= 1
\* Liveness: the hold is eventually released while the execution stays alive.
HoldEventuallyReleased == <>(~h.hold)
\* Liveness (every scenario): released, or the user was told once.
HoldReleasedOrNoticed == <>(~h.hold \/ h.notices >= 1)
\* Liveness: the later input starts a turn instead of being retained.
LaterInputStarts == <>(h.nxt = "submitted")
=============================================================================
