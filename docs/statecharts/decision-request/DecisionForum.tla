-------------------------- MODULE DecisionForum --------------------------
EXTENDS Naturals, TLC
CONSTANT Mode, Existing
Callers == {"a", "b"}
VARIABLES stages, lock, mapping, pending, forums, crashLeft
vars == <<stages, lock, mapping, pending, forums, crashLeft>>
Init == /\ stages = [c \in Callers |-> "idle"] /\ lock = "none"
        /\ mapping = "none" /\ pending = {} /\ forums = IF Existing THEN 1 ELSE 0
        /\ crashLeft = 1
Acquire(c) == /\ stages[c] = "idle" /\ (lock = "none" \/ Mode = "NoLock")
              /\ lock' = c /\ stages' = [stages EXCEPT ![c] = "locked"]
              /\ UNCHANGED <<mapping, pending, forums, crashLeft>>
Lookup(c) == /\ stages[c] = "locked" /\ (lock = c \/ Mode = "NoLock")
             /\ IF forums > 0
                   THEN /\ mapping' = "ready" /\ stages' = [stages EXCEPT ![c] = "done"] /\ lock' = "none"
                   ELSE IF mapping = "unknown" /\ Mode # "RetryUnknown"
                     THEN /\ UNCHANGED mapping /\ stages' = [stages EXCEPT ![c] = "failed"] /\ lock' = "none"
                     ELSE /\ UNCHANGED <<lock, mapping>> /\ stages' = [stages EXCEPT ![c] = "looked"]
             /\ UNCHANGED <<pending, forums, crashLeft>>
Reserve(c) == /\ stages[c] = "looked"
              /\ (lock = c /\ mapping # "unknown") \/ Mode \in {"NoLock", "RetryUnknown"}
              /\ mapping' = "unknown" /\ stages' = [stages EXCEPT ![c] = "reserved"]
              /\ UNCHANGED <<lock, pending, forums, crashLeft>>
Dispatch(c) == /\ stages[c] = "reserved" /\ stages' = [stages EXCEPT ![c] = "posted"]
               /\ pending' = pending \cup {c}
               /\ UNCHANGED <<lock, mapping, forums, crashLeft>>
RemoteEffect(c) == /\ c \in pending /\ forums < 2 /\ forums' = forums + 1
                   /\ pending' = pending \ {c}
                   /\ UNCHANGED <<stages, lock, mapping, crashLeft>>
Reply(c) == /\ stages[c] = "posted" /\ c \notin pending /\ forums > 0
            /\ mapping' = "ready" /\ stages' = [stages EXCEPT ![c] = "done"] /\ lock' = "none"
            /\ UNCHANGED <<pending, forums, crashLeft>>
Crash(c) == /\ lock = c /\ crashLeft = 1 /\ crashLeft' = 0
            /\ stages' = [stages EXCEPT ![c] = "failed"] /\ lock' = "none"
            /\ UNCHANGED <<mapping, pending, forums>>
Next == \E c \in Callers: Acquire(c) \/ Lookup(c) \/ Reserve(c) \/ Dispatch(c) \/ RemoteEffect(c) \/ Reply(c) \/ Crash(c)
Spec == Init /\ [][Next]_vars
NoDuplicateForum == forums <= 1
=============================================================================
