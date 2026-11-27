---------------------------- MODULE ChannelHome ----------------------------
\* #8486: one O channel whose execution node (O home) is set per channel.
\* D is the designated node; G is the node holding the Discord gateway lease, which is
\* also the fallback ("leader") when D cannot run the channel. GatewayLease.tla covers
\* who holds the lease; this model takes exactly one lease holder as given.
\* Mode = "before":   main adeb172ff4. A designation never moves a live session, and
\*                    without one the gateway refuses to place an O channel off itself.
\* Mode = "as7336":   the #7336 fallback path switched on as it is in main: the gateway's
\*                    O hold check runs before routing, and a busy unmanaged turn is adopted.
\* Mode = "design":   this design.
\* Mode = "nofence":  mutation, a node posts on its cached permit without the PG owner check.
\* Mode = "localcfg": mutation, each node judges the home from its own config file.
\* Mode = "nowait":   mutation, the old owner ends a running turn instead of answering Busy.
EXTENDS Naturals, FiniteSets
CONSTANTS Mode, MaxFlaps, MaxEdits

Nodes == {"D", "G"}
Tok == 1..3

VARIABLES want, yaml, up, seen, owner, sess, stamp, run, view,
          q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits
vars == <<want, yaml, up, seen, owner, sess, stamp, run, view,
          q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits>>

TypeOK == /\ want \in Nodes /\ yaml \in [Nodes -> Nodes] /\ up \in BOOLEAN
          /\ seen \in {"online", "offline"} /\ owner \in {"none", "D", "G"}
          /\ sess \in [Nodes -> {"none", "idle", "busy"}]
          /\ stamp \in [Nodes -> {"cur", "old", "legacy"}]
          /\ run \in [Nodes -> 0..3] /\ view \in [Nodes -> BOOLEAN]
          /\ q \in {"none", "arrived", "held", "admitted"} /\ qtok \in 0..3
          /\ tgt \in Nodes /\ proof \in Nodes /\ clean \in {"none", "pending", "busy", "ended", "expired"}
          /\ ans \in [Tok -> 0..2]
          /\ fate \in [Tok -> {"free", "open", "answered", "noticed", "lost"}]
          /\ refusals \in 0..1 /\ cutBusy \in BOOLEAN

Init == /\ want = "G" /\ yaml = [n \in Nodes |-> "G"] /\ up = TRUE /\ seen = "online"
        /\ owner = "none" /\ sess = [n \in Nodes |-> "none"]
        /\ stamp = [n \in Nodes |-> "legacy"] /\ run = [n \in Nodes |-> 0]
        /\ view = [n \in Nodes |-> FALSE] /\ q = "none" /\ qtok = 0 /\ tgt = "G" /\ proof = "G"
        /\ clean = "none" /\ ans = [t \in Tok |-> 0] /\ fate = [t \in Tok |-> "free"]
        /\ refusals = 0 /\ cutBusy = FALSE /\ flaps = 0 /\ edits = 0

Up(n) == n = "G" \/ up
Other(n) == IF n = "D" THEN "G" ELSE "D"
Managed == owner # "none"
\* The designation every decision reads: the PG row, or (mutation) the gateway's own file.
Designated == IF Mode = "localcfg" THEN yaml["G"] ELSE want
\* The one target rule: the designated node while its heartbeat is fresh, else the gateway.
Rule == IF Designated = "D" /\ seen = "online" THEN "D" ELSE "G"
\* Who may post now. An unmanaged channel is the gateway's, as today.
Fence(n) ==
    IF Mode = "localcfg" THEN yaml[n] = n
    ELSE IF ~Managed THEN n = "G" /\ stamp[n] = "legacy"
    ELSE IF Mode = "nofence" THEN view[n]
    ELSE owner = n /\ stamp[n] = "cur"

\* ---- configuration ----
\* An operator edits one node's config file; the two files may differ.
EditYaml(n) == /\ edits < MaxEdits /\ edits' = edits + 1
               /\ yaml' = [yaml EXCEPT ![n] = Other(yaml[n])]
               /\ UNCHANGED <<proof, want, up, seen, owner, sess, stamp, run, view, q, qtok, tgt,
                              clean, ans, fate, refusals, cutBusy, flaps>>
\* Only the hub (G here) seeds the PG designation from its file.
Sync == /\ want # yaml["G"] /\ want' = yaml["G"]
        /\ UNCHANGED <<proof, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt,
                       clean, ans, fate, refusals, cutBusy, flaps, edits>>

\* ---- the designated node D ----
\* The dcserver on D stops; its native session is left as it is.
DDown == /\ up /\ flaps < MaxFlaps /\ up' = FALSE /\ flaps' = flaps + 1
         /\ UNCHANGED <<proof, want, yaml, seen, owner, sess, stamp, run, view, q, qtok, tgt,
                        clean, ans, fate, refusals, cutBusy, edits>>
\* D boots: it reads the PG owner first, and a session from an older generation is ended
\* before any restore.
DUp == /\ ~up /\ up' = TRUE
       /\ view' = [view EXCEPT !["D"] = (owner = "D")]
       /\ IF stamp["D"] = "old" /\ sess["D"] # "none"
          THEN /\ sess' = [sess EXCEPT !["D"] = "none"] /\ run' = [run EXCEPT !["D"] = 0]
               /\ fate' = IF run["D"] # 0 /\ fate[run["D"]] = "open"
                          THEN [fate EXCEPT ![run["D"]] = "lost"] ELSE fate
          ELSE UNCHANGED <<proof, sess, run, fate>>
       /\ UNCHANGED <<proof, want, yaml, seen, owner, stamp, q, qtok, tgt, clean, ans,
                      refusals, cutBusy, flaps, edits>>
\* The heartbeat row follows the process with a delay in both directions.
SeenOffline == /\ ~up /\ seen = "online" /\ seen' = "offline"
               /\ UNCHANGED <<proof, want, yaml, up, owner, sess, stamp, run, view, q, qtok, tgt,
                              clean, ans, fate, refusals, cutBusy, flaps, edits>>
SeenOnline == /\ up /\ seen = "offline" /\ seen' = "online"
              /\ UNCHANGED <<proof, want, yaml, up, owner, sess, stamp, run, view, q, qtok, tgt,
                             clean, ans, fate, refusals, cutBusy, flaps, edits>>

\* ---- an input at the gateway ----
Unreferenced(t) == qtok # t /\ \A n \in Nodes : run[n] # t
Arrive == /\ q = "none"
          /\ \E t \in Tok : /\ fate[t] \in {"free", "answered", "noticed"} /\ Unreferenced(t)
                            /\ fate' = [fate EXCEPT ![t] = "open"]
                            /\ ans' = [ans EXCEPT ![t] = 0] /\ qtok' = t
          /\ q' = "arrived"
          /\ UNCHANGED <<proof, want, yaml, up, seen, owner, sess, stamp, run, view, tgt, clean,
                         refusals, cutBusy, flaps, edits>>

\* No designation and no owner row: the gateway runs it as today.
RunLocal == /\ sess["G"] # "busy"
            /\ sess' = [sess EXCEPT !["G"] = "busy"] /\ stamp' = [stamp EXCEPT !["G"] = "legacy"]
            /\ run' = [run EXCEPT !["G"] = qtok] /\ q' = "none" /\ qtok' = 0
            /\ UNCHANGED <<proof, want, yaml, up, seen, owner, view, tgt, clean, ans, fate,
                           refusals, cutBusy, flaps, edits>>
RouteLocal == /\ q = "arrived" /\ ~Managed /\ Designated = "G" /\ RunLocal
\* main: a live local session keeps every later input (live_owner_local).
BeforeSticky == /\ Mode = "before" /\ q = "arrived" /\ Designated = "D"
                /\ sess["G"] # "none" /\ RunLocal
\* main: with no session the placement leaves the gateway, and an O channel refuses that.
BeforeRefuse == /\ Mode = "before" /\ q = "arrived" /\ Designated = "D" /\ sess["G"] = "none"
                /\ refusals' = 1 /\ fate' = [fate EXCEPT ![qtok] = "lost"]
                /\ q' = "none" /\ qtok' = 0
                /\ UNCHANGED <<proof, want, yaml, up, seen, owner, sess, stamp, run, view, tgt,
                               clean, ans, cutBusy, flaps, edits>>

\* #7336 as it is: the gateway's O hold check sees another node's permit and refuses
\* before the routing hook runs. The input is kept and retried, and refused again.
HoldFirst == /\ Mode = "as7336" /\ q = "arrived" /\ owner = "D" /\ ~view["G"]
             /\ refusals = 0 /\ refusals' = 1
             /\ UNCHANGED <<proof, want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok,
                            tgt, clean, ans, fate, cutBusy, flaps, edits>>
\* The input goes into shared custody with a planned target. The first managed row adopts
\* the gateway's live session; this design adopts only an idle one.
Stage == /\ Mode # "before" /\ q = "arrived" /\ (Designated = "D" \/ Managed)
         /\ ~(Mode = "as7336" /\ owner = "D" /\ ~view["G"])
         /\ (Mode = "as7336" \/ Managed \/ sess["G"] # "busy")
         /\ IF ~Managed /\ sess["G"] # "none"
            THEN /\ owner' = "G"
                 /\ stamp' = [stamp EXCEPT !["G"] = IF sess["G"] = "idle" THEN "cur" ELSE "legacy"]
            ELSE UNCHANGED <<owner, stamp>>
         /\ q' = "held" /\ tgt' = Rule /\ proof' = Designated
         /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, qtok, clean, ans, fate,
                        refusals, cutBusy, flaps, edits>>
\* The lease holder's tick plans again when the designation or D's heartbeat changed,
\* also for an admitted input its target never claimed.
Replan == /\ Mode # "before" /\ q \in {"held", "admitted"}
          /\ (tgt # Rule \/ proof # Designated)
          /\ q' = "held" /\ tgt' = Rule /\ proof' = Designated
          /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, qtok, clean,
                         ans, fate, refusals, cutBusy, flaps, edits>>
\* The target already owns the channel, or nobody does. Every hand-over rechecks that the
\* designation the plan was made from is still the stored one.
PromoteSame == /\ q = "held" /\ clean = "none" /\ owner \in {"none", tgt}
               /\ proof = Designated
               /\ owner' = tgt /\ q' = "admitted"
               /\ UNCHANGED <<proof, want, yaml, up, seen, sess, stamp, run, view, qtok, tgt, clean,
                              ans, fate, refusals, cutBusy, flaps, edits>>
\* Another node owns it: ask that owner to end its session.
RequestCleanup == /\ q = "held" /\ clean = "none" /\ owner \notin {"none", tgt}
                  /\ clean' = "pending"
                  /\ UNCHANGED <<proof, want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok,
                                 tgt, ans, fate, refusals, cutBusy, flaps, edits>>
\* The old owner answers: Busy while its turn runs, Ended once it is idle.
Respond == /\ clean \in {"pending", "busy"} /\ Managed /\ Up(owner)
           /\ IF sess[owner] = "busy" /\ Mode # "nowait"
              THEN /\ clean = "pending" /\ clean' = "busy"
                   /\ UNCHANGED <<proof, sess, run, fate, cutBusy>>
              ELSE /\ clean' = "ended"
                   /\ sess' = [sess EXCEPT ![owner] = "none"]
                   /\ run' = [run EXCEPT ![owner] = 0]
                   /\ cutBusy' = (cutBusy \/ sess[owner] = "busy")
                   /\ fate' = IF sess[owner] = "busy" /\ fate[run[owner]] = "open"
                              THEN [fate EXCEPT ![run[owner]] = "noticed"] ELSE fate
           /\ UNCHANGED <<proof, want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, ans,
                          refusals, flaps, edits>>
\* No answer inside the control deadline, or the owner stopped answering.
Expire == /\ Managed /\ (clean = "pending" \/ (clean = "busy" /\ ~Up(owner)))
          /\ clean' = "expired"
          /\ UNCHANGED <<proof, want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt,
                         ans, fate, refusals, cutBusy, flaps, edits>>
\* One PG transaction: the generation moves, every older session is stale from here, and a
\* turn the old owner still runs is withheld with a notice to the user.
Transfer == /\ q = "held" /\ clean \in {"ended", "expired"} /\ proof = Designated
            /\ owner' = tgt /\ clean' = "none" /\ q' = "admitted"
            /\ stamp' = [n \in Nodes |-> IF sess[n] # "none" THEN "old" ELSE stamp[n]]
            /\ fate' = IF sess[owner] = "busy" /\ fate[run[owner]] = "open"
                       THEN [fate EXCEPT ![run[owner]] = "noticed"] ELSE fate
            /\ UNCHANGED <<proof, want, yaml, up, seen, sess, run, view, qtok, tgt, ans, refusals,
                           cutBusy, flaps, edits>>
\* The owner's worker claims the admitted input and starts a turn in a session of the
\* current generation (a new session when none of this generation exists).
Start(n) == /\ q = "admitted" /\ tgt = n /\ owner = n /\ Up(n) /\ view[n]
            /\ sess[n] # "busy"
            /\ sess' = [sess EXCEPT ![n] = "busy"] /\ stamp' = [stamp EXCEPT ![n] = "cur"]
            /\ run' = [run EXCEPT ![n] = qtok] /\ q' = "none" /\ qtok' = 0
            /\ UNCHANGED <<proof, want, yaml, up, seen, owner, view, tgt, clean, ans, fate,
                           refusals, cutBusy, flaps, edits>>
\* The turn's answer reaches Discord.
Post(n) == /\ sess[n] = "busy" /\ Up(n) /\ Fence(n)
           /\ ans' = [ans EXCEPT ![run[n]] = IF @ < 2 THEN @ + 1 ELSE @]
           /\ fate' = IF fate[run[n]] = "open" THEN [fate EXCEPT ![run[n]] = "answered"] ELSE fate
           /\ sess' = [sess EXCEPT ![n] = "idle"] /\ run' = [run EXCEPT ![n] = 0]
           /\ UNCHANGED <<proof, want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, clean,
                          refusals, cutBusy, flaps, edits>>
\* The turn ends but its answer is withheld by the owner check.
Withheld(n) == /\ sess[n] = "busy" /\ Up(n) /\ ~Fence(n)
               /\ fate' = IF fate[run[n]] = "open" THEN [fate EXCEPT ![run[n]] = "lost"] ELSE fate
               /\ sess' = [sess EXCEPT ![n] = "idle"] /\ run' = [run EXCEPT ![n] = 0]
               /\ UNCHANGED <<proof, want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, clean,
                              ans, refusals, cutBusy, flaps, edits>>
\* The 2 s owner refresh on each node.
Refresh(n) == /\ Up(n) /\ view[n] # (owner = n)
              /\ view' = [view EXCEPT ![n] = (owner = n)]
              /\ UNCHANGED <<proof, want, yaml, up, seen, owner, sess, stamp, run, q, qtok, tgt,
                             clean, ans, fate, refusals, cutBusy, flaps, edits>>
\* The old owner's own cleanup loop ends an idle session of an older generation.
StaleEnd(n) == /\ Up(n) /\ sess[n] = "idle" /\ stamp[n] = "old"
               /\ sess' = [sess EXCEPT ![n] = "none"]
               /\ UNCHANGED <<proof, want, yaml, up, seen, owner, stamp, run, view, q, qtok, tgt,
                              clean, ans, fate, refusals, cutBusy, flaps, edits>>

Next == \/ \E n \in Nodes : EditYaml(n) \/ Start(n) \/ Post(n) \/ Withheld(n)
                            \/ Refresh(n) \/ StaleEnd(n)
        \/ Sync \/ DDown \/ DUp \/ SeenOffline \/ SeenOnline \/ Arrive
        \/ RouteLocal \/ BeforeSticky \/ BeforeRefuse \/ HoldFirst \/ Stage \/ Replan
        \/ PromoteSame \/ RequestCleanup \/ Respond \/ Expire \/ Transfer

\* Restarts and edits are the environment; every tick and every started turn is fair.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Sync) /\ WF_vars(SeenOffline) /\ WF_vars(SeenOnline) /\ WF_vars(Arrive)
        /\ WF_vars(RouteLocal) /\ WF_vars(BeforeSticky) /\ WF_vars(BeforeRefuse)
        /\ WF_vars(Stage) /\ WF_vars(Replan) /\ WF_vars(PromoteSame)
        /\ WF_vars(RequestCleanup) /\ WF_vars(Respond) /\ WF_vars(Expire) /\ WF_vars(Transfer)
        /\ \A n \in Nodes : /\ WF_vars(Start(n)) /\ WF_vars(Post(n)) /\ WF_vars(Withheld(n))
                            /\ WF_vars(Refresh(n)) /\ WF_vars(StaleEnd(n))

\* ---- rules ----
\* At most one node can put this channel's answer on Discord at any moment.
SingleWriter == Cardinality({n \in Nodes : Up(n) /\ sess[n] = "busy" /\ Fence(n)}) <= 1
\* No input is answered twice, and none is answered after the user was told it was withheld.
NoDuplicate == \A t \in Tok : ans[t] <= 1 /\ ~(fate[t] = "noticed" /\ ans[t] > 0)
\* No input disappears without an answer or a notice.
NoSilentLoss == \A t \in Tok : fate[t] # "lost"
\* The user never sees a refusal for an input a non-home node received.
NoRefusal == refusals = 0
\* A turn whose owner answered Busy is never cut by a move.
BusyTurnFinishes == ~cutBusy

Home == IF Managed THEN owner ELSE "G"
\* Once restarts and edits stop: the designated node is the home while it is up, the
\* gateway otherwise, and it stays so.
HomeFollowsDesignation == <>[](Home = IF want = "D" /\ up THEN "D" ELSE "G")
\* Every input leaves the gateway's queue.
Drains == [](q # "none" => <>(q = "none"))
=============================================================================
