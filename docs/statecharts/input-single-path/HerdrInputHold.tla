---- MODULE HerdrInputHold ----
\* #8201 projection: held inputs on one live Bound herdr execution (design rev 5).
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
\*   "personSubmits" after our exact read a person presses Enter on our text before our keys.
\* Rev 4: a v2 hold starts as send_outcome=in_flight before the first key; the write records
\* the final outcome after InputRun. A crash in between leaves in_flight.
\* Rev 5: a readable empty composer with no proof either way (rule 7: in_flight or
\* enter_send_failed) releases as "unconfirmed" after this hold's notice row is secured.
\* Windows are time only; they do not know whether a transcript record is still coming.
\* Readers : main `not_held`; design pre-key resolve in `write`. Busy injection reads no hold.
EXTENDS Naturals
CONSTANTS Policy,    \* "before" (main 934a49997e) | "fixed" (design rev 5)
          Scenario,
          Ends,      \* the execution ends while the later input waits (row end/terminate)
          Restart,   \* one dcserver restart: hold records survive, process memory does not
          Identity,  \* "digest" (hold v2) | "legacy" (v1 file: a time only, written pre-deploy)
          EnqFail,   \* the first notice enqueue fails once
          CrashWindow, \* "inFlight" (before the outcome record) | "afterRecord" | "any"
                       \* | "inFlightTwice" (first crash in flight, a second one anywhere)
          Mutant     \* "None" or one removed rule of the design
VARIABLE h
vars == <<h>>

EnterMay == Scenario = "enterSendFail"
PasteMay == Scenario = "pasteSendFail"
Keys == {"E", "h1", "h2"}
\* Notice dedupe key: the hold identity (design) or the bare execution nonce (mutant).
Key(r) == IF Mutant = "NonceKey" THEN "E" ELSE IF r.holdId = 1 THEN "h1" ELSE "h2"

Legacy == Identity = "legacy"
\* The fields a recorded hold starts with; a v1 hold is only met after the deploy restart.
Recorded(landed, pasted) ==
          [holdId |-> 1,
           composer |-> IF landed = "pending" \/ ~pasted THEN "empty" ELSE "ours",
           rendered |-> landed = "pending" \/ ~pasted,
           landed |-> landed,
           sendStep |-> "recorded",          \* held | pasted | entered | recorded (durable)
           writer |-> FALSE,                 \* the write that owns the hold runs (memory)
           busy |-> FALSE, otherBusy |-> FALSE, otherUser |-> FALSE, stash |-> FALSE,
           hold |-> TRUE,
           reserved |-> FALSE,
           lock |-> FALSE,
           unsentProof |-> FALSE,            \* durable, monotonic
           cleanup |-> "none",               \* none | clearing | cleared (durable)
           keysSent |-> FALSE,               \* clear keys sent in this process (memory)
           clearReceipt |-> FALSE,           \* receipt window since the clearing record
           readable |-> Scenario # "unreadable",
           blind |-> FALSE,
           person |-> "none",
           pasteSettled |-> FALSE, receiptSettled |-> FALSE,
           origSubmits |-> IF landed = "pending" THEN 1 ELSE 0,
           disposition |-> "unknown",
           nxt |-> "queued", refusals |-> 0,
           unclear |-> 0,                    \* durable
           intent |-> FALSE,                 \* durable notice intent
           enqueued |-> FALSE,               \* durable: this hold's outbox row is recorded
           enqFails |-> 0,
           rows |-> {},                      \* outbox rows by key (persistent dedupe)
           sentFor |-> [i \in {1, 2} |-> 0],
           trigger |-> TRUE, watch |-> FALSE,
           humanDeleted |-> FALSE, merged |-> FALSE, releasedOnResidue |-> FALSE,
           overtaken |-> FALSE,              \* ghost: the original landed after a later input
           ended |-> FALSE, up |-> TRUE, restarts |-> 0]
\* v2: hold() persisted in_flight before the first key; the write is still running.
Init == IF Legacy
        THEN \E pasted \in (IF PasteMay THEN {TRUE, FALSE} ELSE {TRUE}) :
                h = Recorded("no", pasted)
        ELSE h = [Recorded("no", FALSE) EXCEPT !.sendStep = "held", !.writer = TRUE,
                                               !.composer = "empty", !.rendered = TRUE,
                                               !.reserved = TRUE]

Live == h.up /\ ~h.ended
\* The write holds the composer lock from resolve through the outcome record (A16). Without
\* it (mutant NoWriteLock) the resolver cannot tell a live write from a dead one, so it
\* measures the windows from the hold record and may judge while the write still runs.
Writing(r) == r.writer /\ r.sendStep # "recorded" /\ Mutant # "NoWriteLock"
InFlight == h.sendStep # "recorded"
\* Enter may have gone: a recorded enter_send_failed, or an outcome never recorded.
EnterMayNow == EnterMay \/ InFlight
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
\* Both windows start at the write's own key (paste, Enter); once no write runs (recorded, or
\* the writer died in a restart) nothing more is sent and the windows run from the record.
PasteWindow == /\ ~h.pasteSettled /\ (h.composer # "ours" \/ h.rendered)
               /\ (~h.writer \/ h.sendStep # "held" \/ Mutant = "NoWriteLock")
               /\ h' = Wake([h EXCEPT !.pasteSettled = TRUE])
ReceiptWindow == /\ ~h.receiptSettled
                 /\ (~h.writer \/ h.sendStep \in {"entered", "recorded"} \/ Mutant = "NoWriteLock")
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
\* After our exact read (clearing recorded) and before our keys, a person submits our text.
PersonSubmitsOurs == /\ Scenario = "personSubmits" /\ h.person = "none"
                     /\ h.composer = "ours" /\ h.cleanup = "clearing" /\ ~h.keysSent
                     /\ h' = Wake([h EXCEPT !.composer = "empty", !.landed = "pending",
                                            !.origSubmits = @ + 1, !.person = "used",
                                            !.overtaken = @ \/ h.nxt = "submitted"])
\* Rule 5b: the receipt window measured from the first empty read after the clearing record
\* (our keys, or a person's Enter). Process memory; a restart measures again (only longer).
\* Time only: it may end before a late transcript record (rev 5, review 4 P2-1).
ClearReceiptWindow == /\ h.cleanup # "none" /\ ~h.clearReceipt /\ h.composer = "empty"
                      /\ h' = Wake([h EXCEPT !.clearReceipt = TRUE])
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
                                     !.sendStep = "recorded", !.writer = FALSE,
                                     !.clearReceipt = FALSE,
                                     !.pasteSettled = TRUE, !.receiptSettled = TRUE]

\* ---------------- the write that created the hold ----------------
PasteKey == /\ h.up /\ h.writer /\ h.sendStep = "held"
            /\ \E landedPaste \in (IF PasteMay THEN {TRUE, FALSE} ELSE {TRUE}) :
                 h' = [h EXCEPT !.sendStep = "pasted",
                                !.composer = IF landedPaste THEN "ours" ELSE "empty",
                                !.rendered = ~landedPaste]
EnterKey == /\ h.up /\ h.writer /\ h.sendStep = "pasted"
            /\ \E landed \in (IF EnterMay /\ h.composer = "ours"
                               THEN {"no", "pending"} ELSE {"no"}) :
                 h' = [h EXCEPT !.sendStep = "entered", !.landed = landed,
                                !.composer = IF landed = "pending" THEN "empty" ELSE @,
                                !.rendered = IF landed = "pending" THEN TRUE ELSE @,
                                !.origSubmits = IF landed = "pending" THEN 1 ELSE @,
                                !.overtaken = @ \/ (landed = "pending" /\ h.nxt = "submitted")]
\* The final send_outcome is written atomically; an unsent outcome carries its proof.
RecordOutcome == /\ h.up /\ h.writer /\ h.sendStep = "entered"
                 /\ h' = Wake([h EXCEPT !.sendStep = "recorded", !.writer = FALSE,
                                        !.unsentProof = ~EnterMay])

\* ---------------- existing paths ----------------
StartBefore == /\ Policy = "before" /\ Live /\ ~Busy(h) /\ ~Writing(h)
               /\ h.nxt \in {"queued", "refused"}
               /\ h.hold
               /\ h' = [h EXCEPT !.refusals = @ + 1,
                                 !.nxt = IF h.nxt = "refused" THEN "retained" ELSE "refused"]
Paste == /\ Live /\ ~Busy(h) /\ ~Writing(h) /\ h.nxt \in {"queued", "held"}
         /\ ~h.hold /\ ~h.reserved /\ ~h.lock /\ Existing(h) = "empty"
         /\ h' = [h EXCEPT !.nxt = "submitted",
                           !.merged = @ \/ h.composer \in {"ours", "edited", "person"}]
BusyInject == /\ Live /\ h.busy /\ h.nxt = "queued" /\ Existing(h) = "empty"
              /\ h' = [h EXCEPT !.nxt = "submitted",
                                !.merged = @ \/ h.composer \in {"ours", "edited", "person"}]
End == /\ Ends /\ h.up /\ ~h.ended /\ ~Busy(h)
       /\ h' = [h EXCEPT !.ended = TRUE, !.hold = FALSE]
Crash == /\ Restart /\ h.up
         /\ h.restarts < (IF CrashWindow = "inFlightTwice" THEN 2 ELSE 1)
         /\ CASE CrashWindow \in {"inFlight", "inFlightTwice"} /\ h.restarts = 0 ->
                    h.sendStep # "recorded"
               [] CrashWindow = "inFlightTwice" -> TRUE
               [] CrashWindow = "afterRecord" -> h.sendStep = "recorded"
               [] OTHER -> TRUE
         /\ h' = [h EXCEPT !.up = FALSE, !.restarts = @ + 1]
Boot == /\ ~h.up
        /\ h' = [h EXCEPT !.up = TRUE, !.reserved = FALSE, !.lock = FALSE, !.watch = FALSE,
                          !.keysSent = FALSE, !.writer = FALSE, !.clearReceipt = FALSE,
                          !.trigger = TRUE]

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
\* Undecidable: count durably; at the limit record the notice intent (RecordIntent).
Bump(r) == LET u == IF r.unclear < UnclearLimit THEN r.unclear + 1 ELSE r.unclear
               k == [Keep(r) EXCEPT !.unclear = u, !.intent = @ \/ u >= UnclearLimit]
           IN {[k EXCEPT !.trigger = Retry(k)]}
Unhold(r) == [r EXCEPT !.hold = FALSE, !.lock = FALSE,
                       !.releasedOnResidue = @ \/ r.stash
                                             \/ r.composer \in {"ours", "edited"}]
DeliveredSeen == \/ Identity = "digest" /\ h.landed = "yes" /\ Mutant # "IgnoreTranscript"
                 \/ Mutant = "TimeReceipt" /\ (h.landed = "yes" \/ h.otherUser)
UnsentOK == Identity = "digest" /\ (h.unsentProof \/ Mutant = "ResendNoSettle"
                                     \/ (Mutant = "InFlightAsRefused" /\ InFlight))
\* Rule 5b's wait: after a clearing record the receipt window must pass first.
ClearWaitOK == h.cleanup = "none" \/ h.clearReceipt \/ Mutant = "SkipClearReceipt"
Settle(disp) == [h EXCEPT !.lock = TRUE,
                          !.reserved = IF Mutant = "NoSettleReservation" THEN @ ELSE FALSE,
                          !.disposition = disp,
                          !.origSubmits = IF Mutant = "ResendNoSettle" THEN @ + 1 ELSE @]
\* The old rule 3 gate: any non-empty composer (draft_sighting != Settled) is held.
Gated == Shown(h) = "unread"
         \/ (Mutant = "SightingGate" /\ Shown(h) \in {"ours", "edited", "person"})
\* An unconfirmed release: this hold's notice row first (intent -> enqueue -> record), then
\* the release unit. A crash anywhere keeps the hold file and resumes after boot.
ReleaseUnconfirmed == IF h.enqueued THEN {Settle("unconfirmed")}
                      ELSE {[h EXCEPT !.intent = TRUE, !.trigger = TRUE]}
ExactOurs == Shown(h) = "ours" /\ Identity = "digest" /\ (~EnterMayNow \/ ReceiptOK)

Choices ==
  CASE Busy(h) -> {Keep(h)}
    [] DeliveredSeen -> {Unhold([h EXCEPT !.disposition = "delivered"])}
    [] Gated /\ Mutant # "ReleaseOnUnread" -> Bump(h)
    [] ~PasteOK -> {Keep(h)}
    \* rule 5: Settled empty with a durable unsent proof (send outcome or exact text seen)
    [] Shown(h) \in {"empty", "unread"} /\ h.cleanup = "clearing" /\ h.keysSent ->
           {[h EXCEPT !.cleanup = "cleared",
                      !.unsentProof = IF Mutant = "OverwriteProof" THEN TRUE ELSE @]}
    \* 5b label: "undelivered" only when the empty composer is ours to explain (send outcome,
    \* or our confirmed keys); a clearing-phase empty may be a person's Enter -> unconfirmed.
    [] Shown(h) \in {"empty", "unread"} /\ UnsentOK /\ ClearWaitOK ->
           IF h.cleanup = "clearing" /\ Mutant # "LateTranscriptAfterClearReceipt"
           THEN ReleaseUnconfirmed ELSE {Settle("undelivered")}
    [] Shown(h) = "empty" /\ UnsentOK -> {Keep(h)}
    [] Shown(h) \in {"empty", "unread"} /\ Identity = "legacy" -> {Settle("unknown")}
    [] Shown(h) = "empty" /\ ~ReceiptOK -> {Keep(h)}
    \* rule 7 (rev 5): readable empty, no digest, no unsent proof, windows over: secure this
    \* hold's notice row first, then release as "unconfirmed". No resend, no keys.
    [] Shown(h) = "empty" /\ Identity = "digest" /\ Mutant # "InFlightKeep" ->
           ReleaseUnconfirmed
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

Resolve == /\ Policy = "fixed" /\ Live /\ h.hold /\ h.trigger /\ ~h.lock /\ ~Writing(h)
           /\ \E n \in Choices : h' = n
Unlink == /\ h.lock /\ h.up
          /\ h' = Unhold(h)
\* EnqueueNotice: part of a resolver pass; outbox dedupe by key; may fail once.
EnqueueNotice == /\ Policy = "fixed" /\ Live /\ h.hold /\ h.trigger /\ h.intent
                 /\ Key(h) \notin h.rows
                 /\ \/ h' = [h EXCEPT !.rows = @ \cup {Key(h)}, !.sentFor[h.holdId] = @ + 1]
                    \/ /\ EnqFail /\ h.enqFails = 0
                       /\ h' = LET f == [h EXCEPT !.enqFails = 1]
                              IN [f EXCEPT !.trigger = Retry(f)]
\* RecordEnqueued: the row for this hold's key exists (new or returned by dedupe). The next
\* resolver pass ends the self-retry; a pending evidence wake is kept.
RecordEnqueued == /\ Policy = "fixed" /\ Live /\ h.hold /\ h.intent /\ ~h.enqueued
                  /\ Key(h) \in h.rows
                  /\ h' = [h EXCEPT !.enqueued = TRUE]

Stay == UNCHANGED vars

Next == Render \/ PasteWindow \/ ReceiptWindow \/ TranscriptWrite \/ TurnEnd
        \/ LoadSpike \/ LoadEnds \/ PersonTypes \/ PersonEdits \/ PersonLeaves
        \/ PersonStashSubmit \/ StashReturns \/ OtherTurnEnd \/ SecondUncertain
        \/ PersonSubmitsOurs \/ ClearReceiptWindow
        \/ PasteKey \/ EnterKey \/ RecordOutcome \/ EnqueueNotice \/ RecordEnqueued
        \/ StartBefore \/ Paste \/ BusyInject \/ End \/ Crash \/ Boot
        \/ Resolve \/ Unlink \/ Stay

Fairness == /\ WF_vars(Render) /\ WF_vars(PasteWindow) /\ WF_vars(ReceiptWindow)
            /\ WF_vars(TranscriptWrite) /\ WF_vars(TurnEnd) /\ WF_vars(LoadEnds)
            /\ WF_vars(PersonLeaves) /\ WF_vars(StashReturns) /\ WF_vars(OtherTurnEnd)
            /\ WF_vars(SecondUncertain) /\ WF_vars(ClearReceiptWindow)
            /\ WF_vars(PasteKey) /\ WF_vars(EnterKey) /\ WF_vars(RecordOutcome)
            /\ WF_vars(EnqueueNotice) /\ WF_vars(RecordEnqueued)
            /\ WF_vars(StartBefore) /\ WF_vars(Paste) /\ WF_vars(BusyInject)
            /\ WF_vars(End) /\ WF_vars(Boot) /\ WF_vars(Resolve) /\ WF_vars(Unlink)
Spec == Init /\ [][Next]_vars /\ Fairness

\* ---------------- rules ----------------
TypeOK == /\ h.composer \in {"empty", "ours", "edited", "person"}
          /\ h.landed \in {"no", "pending", "yes"}
          /\ h.nxt \in {"queued", "held", "refused", "retained", "submitted"}
          /\ h.cleanup \in {"none", "clearing", "cleared"}
          /\ h.sendStep \in {"held", "pasted", "entered", "recorded"}
          /\ h.rows \subseteq Keys /\ h.holdId \in {1, 2}
          /\ h.origSubmits \in 0..2 /\ h.unclear \in 0..UnclearLimit
OriginalAtMostOnce == h.origSubmits <= 1
NoPasteOverResidue == ~h.merged
NoHumanTextDeleted == ~h.humanDeleted
NoReleaseOnResidue == ~h.releasedOnResidue
UndeliveredIsTrue == h.disposition = "undelivered" => h.landed = "no"
\* An unconfirmed release carries this hold's notice row (exactly one with NoticeAtMostOnce).
UnconfirmedNoticed == h.disposition = "unconfirmed" => h.sentFor[h.holdId] >= 1
\* A later input never goes in ahead of the original (the original never lands after it).
OriginalNotOvertaken == ~h.overtaken
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
