------------------------- MODULE OutboundCallerFlow -------------------------
\* Caller-level order around one outbound delivery: optional edit, fallback send,
\* transport observation, and which dedup store a send uses.
\* Caller: "routine_log" edits then may send a replacement without dedup;
\* "issue" and "outbox" send through dedup with a concurrent retry of the same intent.
\* Entry: "caller_shared" (caller passes the process deduper), "port_bound" (the
\* adapter binds it), "caller_fresh" (a caller-made deduper, the broken variant).
EXTENDS Naturals
CONSTANTS Caller, HasMessage, Entry, Mut

Reqs == IF Caller = "routine_log" THEN {1} ELSE {1, 2}
Store(r) == IF Entry = "caller_fresh" THEN r ELSE 0
Stores == {Store(r) : r \in Reqs}

VARIABLES pc, result, receipt, editFail, store, posts, unobserved
vars == <<pc, result, receipt, editFail, store, posts, unobserved>>

Init ==
    /\ pc = [r \in Reqs |-> IF Caller = "routine_log" /\ HasMessage THEN "edit" ELSE "reserve"]
    /\ result = [r \in Reqs |-> "none"]
    /\ receipt = [r \in Reqs |-> FALSE]
    /\ editFail = "none"
    /\ store = [s \in Stores |-> "free"]
    /\ posts = 0
    /\ unobserved = 0

Done(r, res, rec) ==
    /\ pc' = [pc EXCEPT ![r] = "done"]
    /\ result' = [result EXCEPT ![r] = res]
    /\ receipt' = [receipt EXCEPT ![r] = rec]

\* Each raw HTTP attempt records begin and result around the request (raw_output::request).
Observe == IF Mut = "observe_after" THEN unobserved' = unobserved + 1 ELSE UNCHANGED unobserved

\* send_or_edit_run_log: edit; authoritative failure falls through to a replacement send.
Edit(r) ==
    /\ pc[r] = "edit"
    /\ Observe
    /\ \E o \in {"ok", "transient", "permanent", "missing"}:
         CASE o = "ok" -> Done(r, "ok", TRUE) /\ UNCHANGED editFail
           [] o = "transient" /\ Mut # "fallback_on_transient" ->
                Done(r, "err", FALSE) /\ editFail' = "transient"
           [] OTHER ->
                /\ pc' = [pc EXCEPT ![r] = "reserve"]
                /\ editFail' = o
                /\ UNCHANGED <<result, receipt>>
    /\ UNCHANGED <<store, posts>>

\* Routine log sends with a zero idempotency window: no reservation.
Reserve(r) ==
    LET s == Store(r) IN
    /\ pc[r] = "reserve"
    /\ IF Caller = "routine_log"
       THEN /\ pc' = [pc EXCEPT ![r] = "send"] /\ UNCHANGED <<store, result, receipt>>
       ELSE CASE store[s] = "delivered" ->
                   /\ Done(r, "ok", TRUE) /\ UNCHANGED store
              [] store[s] = "inflight" ->
                   \* wait_for_delivery timed out: Skip
                   /\ Done(r, IF Caller = "outbox" THEN "ok_skip" ELSE "err", FALSE)
                   /\ UNCHANGED store
              [] OTHER ->
                   /\ pc' = [pc EXCEPT ![r] = "send"]
                   /\ store' = [store EXCEPT ![s] = "inflight"]
                   /\ UNCHANGED <<result, receipt>>
    /\ UNCHANGED <<editFail, posts, unobserved>>

Release(r) ==
    IF Caller = "routine_log" THEN UNCHANGED store
    ELSE store' = [store EXCEPT ![Store(r)] = "free"]

Send(r) ==
    /\ pc[r] = "send"
    /\ Observe
    /\ \E o \in {"ok", "transient", "permanent"}:
         IF o = "ok"
         THEN /\ posts' = posts + 1
              /\ Done(r, "ok", TRUE)
              /\ IF Caller = "routine_log" THEN UNCHANGED store
                 ELSE store' = [store EXCEPT ![Store(r)] = "delivered"]
         ELSE /\ Done(r, "err", FALSE) /\ Release(r) /\ UNCHANGED posts
    /\ UNCHANGED editFail

Next ==
    \/ \E r \in Reqs: Edit(r) \/ Reserve(r) \/ Send(r)
    \/ (\A r \in Reqs: pc[r] = "done") /\ UNCHANGED vars

Spec == Init /\ [][Next]_vars /\ \A r \in Reqs: WF_vars(Edit(r) \/ Reserve(r) \/ Send(r))

\* A replacement message only follows an authoritative edit failure.
ReplacementOnlyAfterAuthoritativeEditFailure ==
    (Caller = "routine_log" /\ HasMessage /\ posts > 0) => editFail \in {"permanent", "missing"}
\* Every transport attempt is observed before the caller sees its outcome.
ObservedBeforeOutcome == unobserved = 0
\* At most one created message per intent.
NoDuplicatePost == posts <= 1
\* "ok" carries a receipt; outbox's Skip-as-ok only happens while a peer holds the reservation.
OkHasReceipt == \A r \in Reqs: result[r] = "ok" => receipt[r]
SettledOutcome == \A r \in Reqs: pc[r] = "done" => result[r] # "none"
EveryRequestSettles == <>(\A r \in Reqs: pc[r] = "done")
=============================================================================
