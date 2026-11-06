---------------------- MODULE InputNoticeSettlement ----------------------
EXTENDS Integers, FiniteSets, TLC
CONSTANT Mode
Attempts == {1, 2}
Rows == {1, 2, 3}
Requests == {1, 2}
Source(a) == a
RowSource(r) == IF r = 2 THEN 2 ELSE 1
Offset(r) == r
Truth(r) == IF r \in Attempts THEN r ELSE 0

VARIABLES registered, rows, complete, receipt, owner, notice, writerPosts,
          route, response, effect, calls, outbox, echoed, replayed, restart,
          savedOutbox, savedPosts, lost
vars == <<registered, rows, complete, receipt, owner, notice, writerPosts,
          route, response, effect, calls, outbox, echoed, replayed, restart,
          savedOutbox, savedPosts, lost>>

Init == /\ registered = {}
        /\ rows = {}
        /\ complete = FALSE
        /\ receipt = [a \in Attempts |-> 0]
        /\ owner = [r \in Rows |-> -1]
        /\ notice = [r \in Rows |-> "Pending"]
        /\ writerPosts = [r \in Rows |-> 0]
        /\ route = [q \in Requests |-> "none"]
        /\ response = [q \in Requests |-> "pending"]
        /\ effect = {}
        /\ calls = [q \in Requests |-> 0]
        /\ outbox = {}
        /\ echoed = {}
        /\ replayed = {}
        /\ restart = 0
        /\ savedOutbox = {}
        /\ savedPosts = [r \in Rows |-> 0]
        /\ lost = {}

Register(a) == /\ a \notin registered
               /\ registered' = registered \cup {a}
               /\ UNCHANGED <<rows, complete, receipt, owner, notice, writerPosts,
                              route, response, effect, calls, outbox, echoed,
                              replayed, restart, savedOutbox, savedPosts, lost>>
Capture == /\ registered = Attempts /\ rows = {}
           /\ rows' = Rows
           /\ UNCHANGED <<registered, complete, receipt, owner, notice, writerPosts,
                          route, response, effect, calls, outbox, echoed, replayed,
                          restart, savedOutbox, savedPosts, lost>>
Census == /\ rows = Rows /\ ~complete
          /\ complete' = TRUE
          /\ UNCHANGED <<registered, rows, receipt, owner, notice, writerPosts,
                         route, response, effect, calls, outbox, echoed, replayed,
                         restart, savedOutbox, savedPosts, lost>>
ObserveReceipt == /\ 2 \in rows /\ receipt[2] = 0
                  /\ receipt' = [receipt EXCEPT ![2] = 2]
                  /\ UNCHANGED <<registered, rows, complete, owner, notice, writerPosts,
                                 route, response, effect, calls, outbox, echoed, replayed,
                                 restart, savedOutbox, savedPosts, lost>>
\* Pair in native order; an exact UUID reserves its attempt even outside a sparse row list.
Attribute(r) ==
    LET exact == {a \in registered : Source(a) = RowSource(r) /\ receipt[a] = r}
        free == {a \in registered : Source(a) = RowSource(r) /\ a <= Offset(r)
                    /\ receipt[a] = 0
                    /\ ~\E prior \in Rows : owner[prior] = a}
        candidates == IF exact # {} THEN exact ELSE free
    IN /\ complete /\ owner[r] = -1
       /\ IF Mode = "Before" THEN exact # {}
             ELSE \A prior \in rows :
                    RowSource(prior) = RowSource(r) /\ Offset(prior) < Offset(r)
                      => owner[prior] >= 0
       /\ owner' = [owner EXCEPT ![r] = IF candidates = {} THEN 0
                         ELSE CHOOSE a \in candidates : \A b \in candidates : a <= b]
       /\ UNCHANGED <<registered, rows, complete, receipt, notice, writerPosts,
                      route, response, effect, calls, outbox, echoed, replayed,
                      restart, savedOutbox, savedPosts, lost>>
Settle(r) == /\ r \in rows /\ complete /\ notice[r] = "Pending"
             /\ IF Mode = "BeforeKnown" /\ r = 3
                   THEN /\ owner[1] = 1 /\ owner[r] = 0
                        /\ notice' = [notice EXCEPT ![r] = "Excluded"]
                        /\ UNCHANGED writerPosts
                   ELSE IF owner[r] > 0
                     THEN /\ notice' = [notice EXCEPT ![r] = "Excluded"]
                          /\ UNCHANGED writerPosts
                     ELSE /\ r = 3 /\ (Mode = "Before" \/ owner[r] = 0)
                          /\ notice' = [notice EXCEPT ![r] = "Posted"]
                          /\ writerPosts' = [writerPosts EXCEPT ![r] = @ + 1]
             /\ UNCHANGED <<registered, rows, complete, receipt, owner, route,
                            response, effect, calls, outbox, echoed, replayed,
                            restart, savedOutbox, savedPosts, lost>>
Http(q, endpoint) ==
    /\ response[q] = "pending"
    /\ route' = [route EXCEPT ![q] = endpoint]
    /\ response' = [response EXCEPT ![q] = IF q = 1 THEN "Ok" ELSE "Err409"]
    /\ effect' = IF q = 1 \/ Mode = "AcceptanceGap" THEN effect \cup {q} ELSE effect
    /\ UNCHANGED <<registered, rows, complete, receipt, owner, notice, writerPosts,
                   calls, outbox, echoed, replayed, restart, savedOutbox, savedPosts, lost>>
Enqueue(q) == /\ response[q] = "Ok" /\ calls[q] = 0
              /\ q \notin lost /\ Mode # "Before"
              /\ calls' = [calls EXCEPT ![q] = @ + 1]
              /\ outbox' = outbox \cup {q}
              /\ UNCHANGED <<registered, rows, complete, receipt, owner, notice,
                             writerPosts, route, response, effect, echoed, replayed,
                             restart, savedOutbox, savedPosts, lost>>
Send(q) == /\ q \in outbox \ echoed
           /\ echoed' = echoed \cup {q}
           /\ UNCHANGED <<registered, rows, complete, receipt, owner, notice,
                          writerPosts, route, response, effect, calls, outbox,
                          replayed, restart, savedOutbox, savedPosts, lost>>
Replay(q) == /\ response[q] # "pending" /\ q \notin replayed
             /\ replayed' = replayed \cup {q}
             /\ UNCHANGED <<registered, rows, complete, receipt, owner, notice,
                            writerPosts, route, response, effect, calls, outbox,
                            echoed, restart, savedOutbox, savedPosts, lost>>
Restart == /\ restart = 0
           /\ restart' = 1
           /\ savedOutbox' = outbox
           /\ savedPosts' = writerPosts
           /\ lost' = IF Mode = "AcceptanceGap"
                         THEN {q \in effect : q \notin outbox} ELSE lost
           /\ UNCHANGED <<registered, rows, complete, receipt, owner, notice,
                          writerPosts, route, response, effect, calls, outbox,
                          echoed, replayed>>

Next == (\E a \in Attempts : Register(a)) \/ Capture \/ Census \/ ObserveReceipt
        \/ (\E r \in Rows : Attribute(r))
        \/ (\E r \in Rows : Settle(r))
        \/ (\E q \in Requests : (\E endpoint \in {"start", "deliver"} : Http(q, endpoint))
                                \/ Enqueue(q) \/ Send(q) \/ Replay(q)) \/ Restart
Spec == Init /\ [][Next]_vars
        /\ (\A a \in Attempts : WF_vars(Register(a)))
        /\ WF_vars(Capture) /\ WF_vars(Census) /\ WF_vars(ObserveReceipt)
        /\ (\A r \in Rows : WF_vars(Attribute(r)))
        /\ (\A r \in Rows : WF_vars(Settle(r)))
        /\ (\A q \in Requests :
               WF_vars(\E endpoint \in {"start", "deliver"} : Http(q, endpoint))
               /\ WF_vars(Enqueue(q)) /\ WF_vars(Send(q)))

NoRepostOwned == \A r \in Rows : Truth(r) # 0 => writerPosts[r] = 0
DirectAtMostOnce == writerPosts[3] <= 1
OwnerSameSource == \A r \in Rows : owner[r] > 0 =>
                       Source(owner[r]) = RowSource(r) /\ owner[r] <= Offset(r)
OwnerInjective == \A r, s \in Rows : owner[r] > 0 /\ owner[r] = owner[s] => r = s
SettledIsPostedOrExcluded == \A r \in Rows :
    (notice[r] # "Pending") <=> (notice[r] \in {"Posted", "Excluded"})
NoSettleWithoutProof == \A r \in Rows : notice[r] = "Excluded" =>
                                          owner[r] > 0 \/ Mode = "BeforeKnown"
NoFalseExcludeDirect == notice[3] # "Excluded"
EchoOncePerAccepted == \A q \in Requests : calls[q] <= 1 /\
                                     (q \in outbox => response[q] = "Ok")
NoDoublePublisher == writerPosts[1] = 0
RestartKeepsDurable == savedOutbox \subseteq outbox /\
                      (\A r \in Rows : savedPosts[r] <= writerPosts[r])
OwnedEventuallyExcluded == \A r \in Attempts : <>(notice[r] = "Excluded")
DirectEventuallyPosted == <>(notice[3] = "Posted")
AcceptedEventuallyEchoed == \A q \in Requests : (q \in effect) ~> (q \in echoed)
=============================================================================
