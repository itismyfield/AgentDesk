---- MODULE HerdrInputHold ----
\* #8201 projection: one held original on one live Bound herdr execution, one later input.
\* Refines HerdrBusyInput's "uncertain" phase for the herdr execution-nonce input hold
\* (runtime/herdr_input_holds/<nonce>). Value ranges come from the entry-point table in
\* docs/design/8201-herdr-input-hold.design.md (section 1).
\*
\* Write   : claude/herdr_turn/first_input.rs `write` -> herdr_turn.rs `hold` before the paste.
\* Outcome : host_input.rs InputRun. Scenario names the uncertain kind:
\*   "enterRefused"   Indeterminate{confirmed:1, Refused(..)}: paste landed, Enter never sent
\*                    (the 11-16 incident, cause PaneUnverified), also Cancelled{confirmed:1}.
\*   "enterSendFail"  Indeterminate{confirmed:1, Send(..)}: Enter may have reached the pane.
\*   "pasteSendFail"  Indeterminate{confirmed:0, Send(..)}: the paste may have reached the pane.
\*   "personLeaves" / "personStays" : enterRefused, then a person also owns the composer.
\*   "unreadable"     enterRefused under a load that never lets a capture read the composer.
\* Readers : herdr_turn.rs `not_held` (Bound/Pending start), herdr_return.rs; busy injection
\*           (host_input/busy/herdr.rs) never reads the hold.
\* Release : first_input.rs composer_settled, row end / terminate / retire (execution ends).
EXTENDS Naturals
CONSTANTS Policy,      \* "before" (main 934a49997e) | "fixed" (design)
          Scenario,
          Ends,        \* the execution ends while the later input waits (row end/terminate)
          Restart,     \* one dcserver restart; the hold file survives, memory does not
          Identity,    \* "digest" (design hold v2) | "legacyPrefix" | "legacyChip" (v1 file:
                       \* only the composer's own `[User:` prefix, or a folded paste chip)
          Mutant       \* "None" or one removed rule of the fixed resolver
VARIABLE h
vars == <<h>>

EnterMay == Scenario = "enterSendFail"
PasteMay == Scenario = "pasteSendFail"

\* The uncertain write already happened; Init enumerates every outcome the scenario allows.
Init == \E landed \in (IF EnterMay THEN {"no", "pending"} ELSE {"no"}),
           pasted \in (IF PasteMay THEN {TRUE, FALSE} ELSE {TRUE}) :
          h = [composer |-> IF landed = "pending" \/ ~pasted THEN "empty" ELSE "ours",
               rendered |-> landed = "pending" \/ ~pasted,
               landed |-> landed,            \* no | pending (Enter in, no transcript) | yes
               busy |-> FALSE,               \* a turn the transcript shows is running
               hold |-> TRUE,
               readable |-> Scenario # "unreadable",
               person |-> "none",            \* none | typing | gone
               origSubmits |-> IF landed = "pending" THEN 1 ELSE 0,
               resends |-> 0,
               disposition |-> "unknown",    \* unknown | delivered | undelivered
               nxt |-> "queued",             \* queued | refused | retained | submitted
               refusals |-> 0,
               unclear |-> 0,                \* resolver passes that could not decide
               escalated |-> FALSE,
               merged |-> FALSE,
               releasedOnResidue |-> FALSE,
               ended |-> FALSE,
               up |-> TRUE, restarts |-> 0]

Live == h.up /\ ~h.ended
\* What a capture shows: an unrendered paste reads as an empty composer.
Shown == IF ~h.readable THEN "unread"
         ELSE IF h.composer = "ours" /\ ~h.rendered THEN "empty"
         ELSE h.composer
\* Settled: the hold is older than the paste-settle and receipt windows, so a landed Enter has
\* reached the transcript and a landed paste has rendered (abstracted as these two facts).
NoSettleOn == Mutant \in {"NoSettle", "ResendNoSettle"}
ResendOn == Mutant \in {"ResendUndelivered", "ResendNoSettle"}
Settled == (h.landed # "pending" /\ h.rendered) \/ NoSettleOn
PersonCan == Scenario \in {"personLeaves", "personStays"}

\* ---------------- environment ----------------
Render == /\ h.composer = "ours" /\ ~h.rendered
          /\ h' = [h EXCEPT !.rendered = TRUE]
\* Until the user record reaches the transcript a landed Enter is invisible to readiness.
TranscriptWrite == /\ h.landed = "pending"
                   /\ h' = [h EXCEPT !.landed = "yes", !.busy = TRUE]
TurnEnd == /\ h.busy /\ h.landed = "yes"
           /\ h' = [h EXCEPT !.busy = FALSE]
LoadEnds == /\ ~h.readable /\ Scenario # "unreadable"
            /\ h' = [h EXCEPT !.readable = TRUE]
LoadSpike == /\ h.readable /\ Scenario = "enterRefused" /\ h.unclear = 0 /\ h.hold
             /\ h' = [h EXCEPT !.readable = FALSE]
\* A person types into an empty composer, then submits or clears it (only "personLeaves").
PersonTypes == /\ PersonCan /\ h.person = "none" /\ h.composer = "empty"
               /\ h' = [h EXCEPT !.composer = "person", !.person = "typing"]
PersonLeaves == /\ Scenario = "personLeaves" /\ h.person = "typing"
                /\ h' = [h EXCEPT !.composer = "empty", !.person = "gone"]

\* ---------------- existing paths ----------------
\* Idle start of the later input (provider_dispatch -> herdr_turn::execute).
\* before: not_held refuses; two provider_start failures retain the input.
StartBefore == /\ Policy = "before" /\ Live /\ ~h.busy /\ h.nxt \in {"queued", "refused"}
               /\ h.hold
               /\ h' = [h EXCEPT !.refusals = @ + 1,
                                 !.nxt = IF h.nxt = "refused" THEN "retained" ELSE "refused"]
\* Without a hold the existing composer gate (composer.rs when_idle) pastes only on a shown
\* empty composer; Person/AgentDesk refuse, Unread waits.
Paste == /\ Live /\ ~h.busy /\ h.nxt \in {"queued", "refused"} /\ ~h.hold
         /\ Shown = "empty"
         /\ h' = [h EXCEPT !.nxt = "submitted",
                           !.merged = (h.composer \in {"ours", "person"}),
                           !.composer = IF h.composer = "empty" THEN "empty" ELSE "merged"]
\* Busy injection (host_input/busy/herdr.rs) reads no hold; it pastes on a busy provider.
BusyInject == /\ Live /\ h.busy /\ h.nxt = "queued" /\ Shown = "empty"
              /\ h' = [h EXCEPT !.nxt = "submitted",
                                !.merged = (h.composer \in {"ours", "person"})]
\* Row end, terminate or a confirmed retire end the execution and release its hold.
End == /\ Ends /\ h.up /\ ~h.ended /\ ~h.busy
       /\ h' = [h EXCEPT !.ended = TRUE, !.hold = FALSE]
Crash == /\ Restart /\ h.up /\ h.restarts = 0
         /\ h' = [h EXCEPT !.up = FALSE, !.restarts = 1]
\* The hold file survives; in-memory pass counters do not.
Boot == /\ ~h.up
        /\ h' = [h EXCEPT !.up = TRUE, !.unclear = 0]

\* ---------------- design: resolve the hold where it is read ----------------
\* Runs at the not_held site and after a busy injection emptied the composer; under the
\* composer mutation lock, on a gate-confirmed execution, reading the transcript then a capture.
UnclearLimit == 2
Bump == [h EXCEPT !.unclear = IF @ < UnclearLimit THEN @ + 1 ELSE @,
                  !.escalated = @ \/ (h.unclear + 1 >= UnclearLimit)]
Resolve == /\ Policy = "fixed" /\ Live /\ h.hold
           /\ CASE h.landed = "yes" /\ Mutant # "IgnoreTranscript" ->
                    \* delivered: release, never resend
                    h' = [h EXCEPT !.hold = FALSE, !.disposition = "delivered"]
                [] h.busy ->
                    h' = h                                     \* wait for the turn to end
                [] ~Settled \/ (Shown = "unread" /\ Mutant # "ReleaseOnUnread") ->
                    h' = Bump
                [] Shown \in {"empty", "unread"} ->
                    \* undelivered and nothing left in the composer: release; the original
                    \* stays retained (WriteUncertain is never resent automatically)
                    h' = [h EXCEPT !.hold = FALSE,
                                   !.disposition = "undelivered",
                                   !.releasedOnResidue = (h.composer # "empty"),
                                   !.resends = IF ResendOn THEN @ + 1 ELSE @,
                                   !.origSubmits = IF ResendOn
                                                   THEN @ + 1 ELSE @]
                [] Shown = "ours" /\ Mutant = "ReleaseOnResidue" ->
                    h' = [h EXCEPT !.hold = FALSE, !.releasedOnResidue = TRUE]
                [] Shown = "ours" /\ Identity # "legacyChip" ->
                    \* own_draft proof on the recorded digest: gated clear of our own paste
                    h' = [h EXCEPT !.composer = "empty"]
                [] OTHER ->
                    \* a person's draft, or a folded chip a v1 hold cannot prove: keep, surface
                    h' = Bump
           /\ h' # h

Stay == UNCHANGED vars

Next == Render \/ TranscriptWrite \/ TurnEnd \/ LoadEnds \/ LoadSpike
        \/ PersonTypes \/ PersonLeaves
        \/ StartBefore \/ Paste \/ BusyInject \/ End \/ Crash \/ Boot
        \/ Resolve \/ Stay

Fairness == /\ WF_vars(Render) /\ WF_vars(TranscriptWrite) /\ WF_vars(TurnEnd)
            /\ WF_vars(LoadEnds) /\ WF_vars(PersonLeaves)
            /\ WF_vars(StartBefore) /\ WF_vars(Paste) /\ WF_vars(BusyInject)
            /\ WF_vars(End) /\ WF_vars(Boot) /\ WF_vars(Resolve)
Spec == Init /\ [][Next]_vars /\ Fairness

\* ---------------- rules ----------------
TypeOK == /\ h.composer \in {"empty", "ours", "person", "merged"}
          /\ h.landed \in {"no", "pending", "yes"}
          /\ h.nxt \in {"queued", "refused", "retained", "submitted"}
          /\ h.disposition \in {"unknown", "delivered", "undelivered"}
          /\ h.origSubmits \in 0..2 /\ h.refusals \in 0..2 /\ h.restarts \in 0..1
\* Safety: the same input is submitted at most once.
OriginalAtMostOnce == h.origSubmits <= 1
\* Safety: no new input lands on text left in the composer.
NoPasteOverResidue == ~h.merged
\* Safety: the resolver never releases while our paste or a person's draft is in the composer.
NoReleaseOnResidue == ~h.releasedOnResidue
\* Safety: an input reported undelivered (the sender is asked to resend) was never submitted.
UndeliveredIsTrue == h.disposition = "undelivered" => h.landed = "no"
\* Liveness: the hold is eventually released while the execution stays alive.
HoldEventuallyReleased == <>(~h.hold)
\* Liveness (weaker, every scenario): the hold is released or surfaced to the operator.
HoldReleasedOrSurfaced == <>(~h.hold \/ h.escalated)
\* Liveness: the later input eventually starts a turn instead of being retained.
LaterInputStarts == <>(h.nxt = "submitted")
=============================================================================
