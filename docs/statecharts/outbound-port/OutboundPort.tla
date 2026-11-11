---------------------------- MODULE OutboundPort ----------------------------
\* Outbound delivery: intent -> reservation -> nonce -> post -> receipt.
\* Conv says where the dedup key is built from the target ids: "serenity" is the
\* key built from Serenity ids, "domain" is the key built from value ids after the
\* adapter converts them, "lossy" is a conversion that drops the thread parent.
EXTENDS Naturals, FiniteSets
CONSTANTS Conv, Nonce, Ambiguous, Mut, MaxRestarts

Intents == {"a", "b"}            \* Thread{p1,t} and Thread{p2,t}, same event ids
Tries == {1, 2}                  \* a second request for the same intent
Reqs == Intents \X Tries
NoReq == <<"none", 0>>
Key(i) == IF Conv = "lossy" THEN "t" ELSE i
Keys == {Key(i) : i \in Intents}
Outcomes == {"none", "sent", "duplicate", "skip", "transient", "permanent"}

VARIABLES pc, outcome, shown, retried, dedup, holder, store, posts, nonceSeen, restarts
vars == <<pc, outcome, shown, retried, dedup, holder, store, posts, nonceSeen, restarts>>

Init ==
    /\ pc = [r \in Reqs |-> "idle"]
    /\ outcome = [r \in Reqs |-> "none"]
    /\ shown = [r \in Reqs |-> "none"]
    /\ retried = [r \in Reqs |-> FALSE]
    /\ dedup = [k \in Keys |-> "free"]
    /\ holder = [k \in Keys |-> NoReq]
    /\ store = [k \in Keys |-> "none"]
    /\ posts = [i \in Intents |-> 0]
    /\ nonceSeen = [i \in Intents |-> FALSE]
    /\ restarts = 0

TypeOK ==
    /\ pc \in [Reqs -> {"idle", "waiting", "sending", "done"}]
    /\ outcome \in [Reqs -> Outcomes]
    /\ dedup \in [Keys -> {"free", "inflight", "delivered"}]
    /\ posts \in [Intents -> 0..4]

Finish(r, o, s) ==
    /\ pc' = [pc EXCEPT ![r] = "done"]
    /\ outcome' = [outcome EXCEPT ![r] = o]
    /\ shown' = [shown EXCEPT ![r] = s]

\* deliver_outbound_with_overrides: reserve the key, or replay a stored receipt.
Reserve(r) ==
    LET i == r[1] k == Key(i) IN
    /\ pc[r] = "idle"
    /\ IF Mut = "no_reserve"
       THEN /\ pc' = [pc EXCEPT ![r] = "sending"]
            /\ UNCHANGED <<outcome, shown, dedup, holder, store>>
       ELSE CASE dedup[k] = "delivered" ->
                    /\ Finish(r, "duplicate", store[k])
                    /\ UNCHANGED <<dedup, holder, store>>
              [] dedup[k] = "inflight" ->
                    /\ pc' = [pc EXCEPT ![r] = "waiting"]
                    /\ UNCHANGED <<outcome, shown, dedup, holder, store>>
              [] OTHER ->
                    /\ pc' = [pc EXCEPT ![r] = "sending"]
                    /\ holder' = [holder EXCEPT ![k] = r]
                    /\ IF Mut = "record_before_post"
                       THEN /\ dedup' = [dedup EXCEPT ![k] = "delivered"]
                            /\ store' = [store EXCEPT ![k] = i]
                       ELSE /\ dedup' = [dedup EXCEPT ![k] = "inflight"]
                            /\ UNCHANGED store
                    /\ UNCHANGED <<outcome, shown>>
    /\ UNCHANGED <<retried, posts, nonceSeen, restarts>>

\* wait_for_delivery: Delivered, Released (loop again) or TimedOut (Skip).
Wait(r) ==
    LET k == Key(r[1]) IN
    /\ pc[r] = "waiting"
    /\ CASE dedup[k] = "delivered" ->
              /\ Finish(r, "duplicate", store[k])
         [] dedup[k] = "free" ->
              /\ pc' = [pc EXCEPT ![r] = "idle"]
              /\ UNCHANGED <<outcome, shown>>
         [] OTHER -> Finish(r, "skip", "none")
    /\ UNCHANGED <<retried, dedup, holder, store, posts, nonceSeen, restarts>>

\* Discord creates a message unless an enforced nonce was already seen.
Created(i) ==
    /\ posts' = [posts EXCEPT ![i] = IF Nonce = "enforced" /\ nonceSeen[i] THEN @ ELSE @ + 1]
    /\ nonceSeen' = [nonceSeen EXCEPT ![i] = TRUE]

Release(k, r) ==
    IF holder[k] = r /\ dedup[k] = "inflight"
    THEN /\ dedup' = [dedup EXCEPT ![k] = "free"]
         /\ holder' = [holder EXCEPT ![k] = NoReq]
    ELSE UNCHANGED <<dedup, holder>>

SendOk(r) ==
    LET i == r[1] k == Key(i) IN
    /\ pc[r] = "sending"
    /\ Created(i)
    /\ dedup' = [dedup EXCEPT ![k] = "delivered"]
    /\ store' = [store EXCEPT ![k] = i]
    /\ holder' = [holder EXCEPT ![k] = NoReq]
    /\ Finish(r, "sent", i)
    /\ UNCHANGED <<retried, restarts>>

SendFail(r) ==
    LET i == r[1] k == Key(i) IN
    /\ pc[r] = "sending"
    /\ \E o \in {"transient", "permanent"}: Finish(r, o, "none")
    /\ Release(k, r)
    /\ UNCHANGED <<retried, store, posts, nonceSeen, restarts>>

\* No HTTP response although Discord created the message.
SendAmbiguous(r) ==
    LET i == r[1] k == Key(i) IN
    /\ Ambiguous
    /\ pc[r] = "sending"
    /\ Created(i)
    /\ Finish(r, "transient", "none")
    /\ Release(k, r)
    /\ UNCHANGED <<retried, store, restarts>>

\* TransientFailure keeps retry authority with the durable caller.
Retry(r) ==
    /\ pc[r] = "done" /\ outcome[r] = "transient" /\ ~retried[r]
    /\ pc' = [pc EXCEPT ![r] = "idle"]
    /\ outcome' = [outcome EXCEPT ![r] = "none"]
    /\ retried' = [retried EXCEPT ![r] = TRUE]
    /\ UNCHANGED <<shown, dedup, holder, store, posts, nonceSeen, restarts>>

\* The in-memory deduper is lost; the durable caller drives open requests again.
Restart ==
    /\ restarts < MaxRestarts
    /\ restarts' = restarts + 1
    /\ dedup' = [k \in Keys |-> "free"]
    /\ holder' = [k \in Keys |-> NoReq]
    /\ store' = [k \in Keys |-> "none"]
    /\ pc' = [r \in Reqs |-> IF pc[r] = "done" THEN "done" ELSE "idle"]
    /\ UNCHANGED <<outcome, shown, retried, posts, nonceSeen>>

Next ==
    \/ \E r \in Reqs: Reserve(r) \/ Wait(r) \/ SendOk(r) \/ SendFail(r)
                      \/ SendAmbiguous(r) \/ Retry(r)
    \/ Restart
    \/ (\A r \in Reqs: pc[r] = "done") /\ UNCHANGED vars

Spec == Init /\ [][Next]_vars
        /\ \A r \in Reqs: WF_vars(Reserve(r)) /\ WF_vars(Wait(r))
                          /\ WF_vars(SendOk(r) \/ SendFail(r) \/ SendAmbiguous(r))

NoDuplicatePost == \A i \in Intents: posts[i] <= 1
NoSuccessWithoutReceipt ==
    \A r \in Reqs: outcome[r] \in {"sent", "duplicate"} => shown[r] = r[1] /\ posts[r[1]] >= 1
NoSilentLoss ==
    \A i \in Intents:
        (\A n \in Tries: pc[<<i, n>>] = "done")
            => posts[i] >= 1 \/ \E n \in Tries: outcome[<<i, n>>] \in {"transient", "permanent"}
EveryRequestSettles == <>(\A r \in Reqs: pc[r] = "done")
=============================================================================
