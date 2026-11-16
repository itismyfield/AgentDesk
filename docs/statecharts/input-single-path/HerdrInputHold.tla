---- MODULE HerdrInputHold ----
\* #8201 projection: held inputs on one live Bound herdr execution (design rev 3).
\* Refines HerdrBusyInput's "uncertain" phase for the herdr execution-nonce input hold
\* (runtime/herdr_input_holds/<nonce>) and the readiness reservation the same input consumed.
\* Value ranges come from the entry-point table in docs/design/8201-herdr-input-hold.design.md.
\*
\* Write   : first_input.rs `write` -> `hold` before the paste; composer.rs `when_idle` ->
\*           readiness `reserve` (Entry.pending) before the send.
\* Outcome : host_input.rs InputRun. Scenario:
\*   "enterRefused"  Indeterminate{confirmed:1, Refused} or Cancelled{confirmed:1}: paste in,
\*                   Enter never sent (the 11-16 incident, PaneUnverified).
\*   "loadSpike"     enterRefused, then the first captures fail under load and recover.
\*   "enterSendFail" Indeterminate{confirmed:1, Send}: Enter may have reached the pane.
\*   "pasteSendFail" Indeterminate{confirmed:0, Send}: the paste may have reached the pane.
\*   "personLeaves" / "personStays": a person types into the emptied composer.
\*   "personEdits"   a person edits our pasted text (append, whitespace, any change).
\*   "stashOther"    a person C-s parks our text, submits B, the stash returns.
\*   "unreadable"    the composer never reads.
\*   "secondHold"    the first hold resolves, a later input on the same execution is held too
\*                   and never reads (two holds, one execution nonce).
\* Readers : main `not_held`; design pre-key resolve in `write`. Busy injection reads no hold.
EXTENDS Naturals
CONSTANTS Policy,    \* "before" (main 934a49997e) | "fixed" (design rev 3)
          Scenario,
          Ends,      \* the execution ends while the later input waits (row end/terminate)
          Restart,   \* one dcserver restart: hold records survive, process memory does not
          Identity,  \* "digest" (hold v2) | "legacy" (v1 file: a time only, written pre-deploy)
          EnqFail,   \* the first notice enqueue fails once
          Mutant     \* "None" or one removed rule of the design
VARIABLE h
vars == <<h>>

EnterMay == Scenario = "enterSendFail"
PasteMay == Scenario = "pasteSendFail"
Keys == {"E", "h1", "h2"}
\* Notice dedupe key: the hold identity (design) or the bare execution nonce (mutant).
Key(r) == IF Mutant = "NonceKey" THEN "E" ELSE IF r.holdId = 1 THEN "h1" ELSE "h2"

Init == \E landed \in (IF EnterMay THEN {"no", "pending"} ELSE {"no"}),
           pasted \in (IF PasteMay THEN {TRUE, FALSE} ELSE {TRUE}) :
          h = [holdId |-> 1,
               composer |-> IF landed = "pending" \/ ~pasted THEN "empty" ELSE "ours",
               rendered |-> landed = "pending" \/ ~pasted,
               landed |-> landed,
               busy |-> FALSE, otherBusy |-> FALSE, otherUser |-> FALSE, stash |-> FALSE,
               hold |-> TRUE,
               reserved |-> Identity = "digest",
               lock |-> FALSE,
               \* durable hold record: send_outcome-derived proof, kept through cleanup
               unsentProof |-> Identity = "digest" /\ ~EnterMay,
               cleanup |-> "none",           \* none | clearing | cleared (durable)
               keysSent |-> FALSE,           \* clear keys sent in this process (memory)
               readable |-> Scenario # "unreadable",
               blind |-> FALSE,
               person |-> "none",
               pasteSettled |-> FALSE, receiptSettled |-> FALSE,
               origSubmits |-> IF landed = "pending" THEN 1 ELSE 0,
               disposition |-> "unknown",
               nxt |-> "queued", refusals |-> 0,
               unclear |-> 0,                \* durable
               intent |-> FALSE,             \* durable notice intent
               enqueued |-> FALSE,           \* durable: an outbox row for this hold's key exists
               enqFails |-> 0,
               rows |-> {},                  \* outbox rows by key (persistent dedupe)
               sentFor |-> [i \in {1, 2} |-> 0],  \* rows created for each hold
               trigger |-> TRUE, watch |-> FALSE,
               humanDeleted |-> FALSE, merged |-> FALSE, releasedOnResidue |-> FALSE,
               ended |-> FALSE, up |-> TRUE, restarts |-> 0]

Live == h.up /\ ~h.ended
Busy(r) == r.busy \/ r.otherBusy
\* The existing classifier: an unrendered paste reads empty; a stash does not show on Empty.
Existing(r) == IF ~r.readable THEN "unread"
               ELSE IF r.composer = "ours" /\ ~r.rendered THEN "empty"
               ELSE r.composer
\* Resolver: readability gate first (capture, layout, stash, footer, modal); a readable text
\* goes on to the exact-body check; Empty is Composer::Empty with draft_sighting Settled.
Shown(r) == IF r.stash /\ r.readable THEN "unread" ELSE Existing(r)

NoSettleOn == Mutant = "ResendNoSettle"
PasteOK == h.pasteSettled \/ NoSettleOn \/ (Mutant = "SkipPasteSettle" /\ h.unsentProof)
ReceiptOK == h.receiptSettled \/ NoSettleOn

Wake(r) == [r EXCEPT !.trigger = @ \/ (r.watch /\ (Mutant # "EmptyOnlyWake"
                                                    \/ Shown(r) = "empty"))]

\* ---------------- environment ----------------
Render == /\ h.composer = "ours" /\ ~h.rendered
          /\ h' = Wake([h EXCEPT !.rendered = TRUE])
PasteWindow == /\ ~h.pasteSettled /\ (h.composer # "ours" \/ h.rendered)
               /\ h' = Wake([h EXCEPT !.pasteSettled = TRUE])
ReceiptWindow == /\ ~h.receiptSettled /\ h.landed # "pending"
                 /\ h' = Wake([h EXCEPT !.receiptSettled = TRUE])
TranscriptWrite == /\ h.landed = "pending"
                   /\ h' = Wake([h EXCEPT !.landed = "yes", !.busy = TRUE])
TurnEnd == /\ h.busy /\ h.landed = "yes"
           /\ h' = Wake([h EXCEPT !.busy = FALSE, !.reserved = FALSE])
LoadSpike == /\ Scenario \in {"loadSpike", "secondHold"} /\ h.holdId = 1
             /\ h.readable /\ h.unclear = 0 /\ h.hold
             /\ h' = [h EXCEPT !.readable = FALSE]
LoadEnds == /\ ~h.readable /\ Scenario # "unreadable" /\ ~h.blind
            /\ h' = Wake([h EXCEPT !.readable = TRUE])
PersonTypes == /\ Scenario \in {"personLeaves", "personStays"} /\ h.person = "none"
               /\ h.composer = "empty"
               /\ h' = Wake([h EXCEPT !.composer = "person", !.person = "typing"])
PersonEdits == /\ Scenario = "personEdits" /\ h.person = "none"
               /\ h.composer = "ours" /\ h.rendered
               /\ h' = Wake([h EXCEPT !.composer = "edited", !.person = "typing"])
PersonLeaves == /\ Scenario \in {"personLeaves", "personEdits"} /\ h.person = "typing"
                /\ h' = Wake([h EXCEPT !.composer = "empty", !.person = "gone",
                                       !.otherUser = TRUE])
PersonStashSubmit == /\ Scenario = "stashOther" /\ h.person = "none" /\ ~Busy(h)
                     /\ h.composer = "ours" /\ h.rendered
                     /\ h' = Wake([h EXCEPT !.composer = "empty", !.stash = TRUE,
                                            !.otherUser = TRUE, !.otherBusy = TRUE,
                                            !.person = "used"])
StashReturns == /\ h.stash /\ h.composer = "empty"
                /\ h' = Wake([h EXCEPT !.composer = "ours", !.stash = FALSE])
OtherTurnEnd == /\ h.otherBusy
                /\ h' = Wake([h EXCEPT !.otherBusy = FALSE, !.reserved = FALSE])
\* A later input on the same execution meets its own unclear Enter; it never reads.
SecondUncertain == /\ Scenario = "secondHold" /\ h.holdId = 1 /\ ~h.hold /\ Live
                   /\ h.nxt = "submitted" /\ ~Busy(h)
                   /\ h' = [h EXCEPT !.holdId = 2, !.hold = TRUE, !.composer = "ours",
                                     !.rendered = TRUE, !.landed = "no", !.reserved = TRUE,
                                     !.unsentProof = TRUE, !.cleanup = "none",
                                     !.keysSent = FALSE, !.readable = FALSE, !.blind = TRUE,
                                     !.disposition = "unknown", !.nxt = "queued",
                                     !.unclear = 0, !.intent = FALSE, !.enqueued = FALSE,
                                     !.trigger = TRUE, !.watch = FALSE,
                                     !.pasteSettled = TRUE, !.receiptSettled = TRUE]

\* ---------------- existing paths ----------------
StartBefore == /\ Policy = "before" /\ Live /\ ~Busy(h) /\ h.nxt \in {"queued", "refused"}
               /\ h.hold
               /\ h' = [h EXCEPT !.refusals = @ + 1,
                                 !.nxt = IF h.nxt = "refused" THEN "retained" ELSE "refused"]
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
Boot == /\ ~h.up
        /\ h' = [h EXCEPT !.up = TRUE, !.reserved = FALSE, !.lock = FALSE, !.watch = FALSE,
                          !.keysSent = FALSE, !.trigger = TRUE]

\* ---------------- design: resolve where the hold is read ----------------
UnclearLimit == 2
Keep(r) == [r EXCEPT !.nxt = IF Mutant = "OpaqueFailure"
                             THEN (IF r.nxt = "queued" THEN "refused"
                                   ELSE IF r.nxt = "refused" THEN "retained" ELSE r.nxt)
                             ELSE (IF r.nxt = "queued" THEN "held" ELSE r.nxt),
                     !.refusals = IF Mutant = "OpaqueFailure" /\ r.nxt \in {"queued", "refused"}
                                  THEN @ + 1 ELSE @,
                     !.trigger = FALSE, !.watch = TRUE]
\* Self-retry runs until this hold's outbox row is durable (design) or only until the intent
\* is recorded (mutant StopOnIntent).
Retry(r) == IF Mutant = "NoSelfRetry" THEN FALSE
            ELSE IF Mutant = "StopOnIntent" THEN ~r.intent
            ELSE ~r.enqueued
Enqueued(r) == IF Key(r) \in r.rows THEN [r EXCEPT !.enqueued = TRUE]
               ELSE [r EXCEPT !.rows = @ \cup {Key(r)},
                              !.sentFor[r.holdId] = @ + 1, !.enqueued = TRUE]
\* Undecidable: count durably; at the limit record the intent, then enqueue idempotently.
Bump(r) == LET u == IF r.unclear < UnclearLimit THEN r.unclear + 1 ELSE r.unclear
               k == [Keep(r) EXCEPT !.unclear = u, !.intent = @ \/ u >= UnclearLimit]
               tries == IF k.intent /\ ~k.enqueued
                        THEN {Enqueued(k)} \cup
                             (IF EnqFail /\ k.enqFails = 0
                              THEN {[k EXCEPT !.enqFails = 1]} ELSE {})
                        ELSE {k}
           IN {[t EXCEPT !.trigger = Retry(t)] : t \in tries}
Unhold(r) == [r EXCEPT !.hold = FALSE, !.lock = FALSE,
                       !.releasedOnResidue = @ \/ r.stash
                                             \/ r.composer \in {"ours", "edited"}]
DeliveredSeen == \/ Identity = "digest" /\ h.landed = "yes" /\ Mutant # "IgnoreTranscript"
                 \/ Mutant = "TimeReceipt" /\ (h.landed = "yes" \/ h.otherUser)
UnsentOK == Identity = "digest" /\ (h.unsentProof \/ Mutant = "ResendNoSettle")
Settle(disp) == [h EXCEPT !.lock = TRUE,
                          !.reserved = IF Mutant = "NoSettleReservation" THEN @ ELSE FALSE,
                          !.disposition = disp,
                          !.origSubmits = IF Mutant = "ResendNoSettle" THEN @ + 1 ELSE @]
\* The old rule 3 gate: any non-empty composer (draft_sighting != Settled) is held.
Gated == Shown(h) = "unread"
         \/ (Mutant = "SightingGate" /\ Shown(h) \in {"ours", "edited", "person"})
ExactOurs == Shown(h) = "ours" /\ Identity = "digest" /\ (~EnterMay \/ ReceiptOK)

Choices ==
  CASE Busy(h) -> {Keep(h)}
    [] DeliveredSeen -> {Unhold([h EXCEPT !.disposition = "delivered"])}
    [] Gated /\ Mutant # "ReleaseOnUnread" -> Bump(h)
    [] ~PasteOK -> {Keep(h)}
    \* rule 5: Settled empty with a durable unsent proof (send outcome or exact text seen)
    [] Shown(h) \in {"empty", "unread"} /\ h.cleanup = "clearing" /\ h.keysSent ->
           {[h EXCEPT !.cleanup = "cleared",
                      !.unsentProof = IF Mutant = "OverwriteProof" THEN TRUE ELSE @]}
    [] Shown(h) \in {"empty", "unread"} /\ UnsentOK -> {Settle("undelivered")}
    [] Shown(h) \in {"empty", "unread"} /\ Identity = "legacy" -> {Settle("unknown")}
    [] Shown(h) = "empty" /\ ~ReceiptOK -> {Keep(h)}
    [] Shown(h) = "ours" /\ Mutant = "ReleaseOnResidue" -> {Unhold(h)}
    \* rule 8: exact body; record clearing with the proof, then keys, then cleared
    [] ExactOurs /\ h.cleanup = "none" ->
           {[h EXCEPT !.cleanup = "clearing",
                      !.unsentProof = IF Mutant = "OverwriteProof" THEN FALSE ELSE TRUE]}
    [] ExactOurs /\ h.cleanup = "clearing" ->
           {[h EXCEPT !.composer = "empty", !.keysSent = TRUE]}
    [] Shown(h) = "edited" /\ Mutant = "ShapeDelete" ->
           {[h EXCEPT !.composer = "empty", !.unsentProof = TRUE, !.humanDeleted = TRUE]}
    [] OTHER -> Bump(h)

Resolve == /\ Policy = "fixed" /\ Live /\ h.hold /\ h.trigger /\ ~h.lock
           /\ \E n \in Choices : h' = n
Unlink == /\ h.lock /\ h.up
          /\ h' = Unhold(h)

Stay == UNCHANGED vars

Next == Render \/ PasteWindow \/ ReceiptWindow \/ TranscriptWrite \/ TurnEnd
        \/ LoadSpike \/ LoadEnds \/ PersonTypes \/ PersonEdits \/ PersonLeaves
        \/ PersonStashSubmit \/ StashReturns \/ OtherTurnEnd \/ SecondUncertain
        \/ StartBefore \/ Paste \/ BusyInject \/ End \/ Crash \/ Boot
        \/ Resolve \/ Unlink \/ Stay

Fairness == /\ WF_vars(Render) /\ WF_vars(PasteWindow) /\ WF_vars(ReceiptWindow)
            /\ WF_vars(TranscriptWrite) /\ WF_vars(TurnEnd) /\ WF_vars(LoadEnds)
            /\ WF_vars(PersonLeaves) /\ WF_vars(StashReturns) /\ WF_vars(OtherTurnEnd)
            /\ WF_vars(SecondUncertain)
            /\ WF_vars(StartBefore) /\ WF_vars(Paste) /\ WF_vars(BusyInject)
            /\ WF_vars(End) /\ WF_vars(Boot) /\ WF_vars(Resolve) /\ WF_vars(Unlink)
Spec == Init /\ [][Next]_vars /\ Fairness

\* ---------------- rules ----------------
TypeOK == /\ h.composer \in {"empty", "ours", "edited", "person"}
          /\ h.landed \in {"no", "pending", "yes"}
          /\ h.nxt \in {"queued", "held", "refused", "retained", "submitted"}
          /\ h.cleanup \in {"none", "clearing", "cleared"}
          /\ h.rows \subseteq Keys /\ h.holdId \in {1, 2}
          /\ h.origSubmits \in 0..2 /\ h.unclear \in 0..UnclearLimit
OriginalAtMostOnce == h.origSubmits <= 1
NoPasteOverResidue == ~h.merged
NoHumanTextDeleted == ~h.humanDeleted
NoReleaseOnResidue == ~h.releasedOnResidue
UndeliveredIsTrue == h.disposition = "undelivered" => h.landed = "no"
DeliveredIsTrue == h.disposition = "delivered" => h.landed = "yes"
\* Safety: at most one notice row per hold.
NoticeAtMostOnce == \A i \in {1, 2} : h.sentFor[i] <= 1
\* Liveness: the hold is eventually released while the execution stays alive.
HoldEventuallyReleased == <>(~h.hold)
\* Liveness: every hold is released or gets its own notice row.
HoldReleasedOrNoticed == h.hold ~> (~h.hold \/ h.sentFor[h.holdId] >= 1)
\* Liveness: our exact text, once readable, is cleaned up (or the composer changes).
ReadableOursCleaned == (h.hold /\ h.up /\ h.readable /\ ~h.stash /\ h.rendered
                        /\ h.composer = "ours" /\ Identity = "digest")
                       ~> (h.composer # "ours" \/ ~h.hold \/ ~h.readable)
\* Liveness: the later input starts a turn instead of being retained.
LaterInputStarts == <>(h.nxt = "submitted")
=============================================================================
