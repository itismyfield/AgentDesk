---------------------------- MODULE ChannelHome ----------------------------
\* One O channel whose execution node (O home) is set per channel (#8486).
\* D is the designated node; G holds the Discord gateway lease and runs the channel when D
\* cannot. The lease holder is fixed here; GatewayLease.tla covers how the lease moves.
\* Thread = TRUE: the channel is a thread child; its designation is its parent's and D must
\* record the child before its writer can start.
\* Mode = "before":    main 5f0f2803e8. A designation never moves a live session, and without
\*                     one the gateway refuses to place an O channel off itself.
\* Mode = "as7336":    the #7336 fallback path switched on as it is in main.
\* Mode = "design":    this design.
\* Mutations of the design: "nofence" posts on the cached permit without the PG owner check;
\* "localcfg" judges the home from each node's own config file; "nowait" ends a running turn
\* instead of answering Busy; "drop" lets an admitted input leave custody unanswered;
\* "noadoptnative" keeps the adopted session's native binding away from cleanup and permit;
\* "norenew" never opens a new generation after the owner's own session ended;
\* "reusestore" lets a writer reuse a store of an older generation; "rawonly" re-plans a held
\* input only when the raw revision changed; "noinherit" reads a thread's own designation;
\* "nochild" never records a thread child on the designated node.
EXTENDS Naturals, FiniteSets
CONSTANTS Mode, Thread, MaxFlaps, MaxEdits, MaxClears

Nodes == {"D", "G"}
Tok == 1..3

VARIABLES want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child
vars == <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>

TypeOK == /\ want \in Nodes /\ yaml \in [Nodes -> Nodes] /\ up \in BOOLEAN
          /\ seen \in {"online", "offline"} /\ owner \in {"none", "D", "G"}
          /\ sess \in [Nodes -> {"none", "idle", "busy"}]
          /\ stamp \in [Nodes -> {"cur", "old", "legacy"}]
          /\ run \in [Nodes -> 0..3] /\ view \in [Nodes -> BOOLEAN]
          /\ q \in {"none", "arrived", "held", "admitted"} /\ qtok \in 0..3
          /\ tgt \in Nodes /\ proof \in Nodes
          /\ clean \in {"none", "pending", "busy", "ended", "expired"}
          /\ ans \in [Tok -> 0..2] /\ fate \in [Tok -> {"free", "open", "answered", "noticed"}]
          /\ refusals \in 0..1 /\ cutBusy \in BOOLEAN
          /\ bind \in [Nodes -> {"none", "live", "dead"}] /\ oldNat \in [Nodes -> BOOLEAN]
          /\ store \in [Nodes -> {"none", "cur", "old"}] /\ child \in BOOLEAN

Init == /\ want = "G" /\ yaml = [n \in Nodes |-> "G"] /\ up = TRUE /\ seen = "online"
        /\ owner = "none" /\ sess = [n \in Nodes |-> "none"]
        /\ stamp = [n \in Nodes |-> "legacy"] /\ run = [n \in Nodes |-> 0]
        /\ view = [n \in Nodes |-> FALSE] /\ q = "none" /\ qtok = 0 /\ tgt = "G" /\ proof = "G"
        /\ clean = "none" /\ ans = [t \in Tok |-> 0] /\ fate = [t \in Tok |-> "free"]
        /\ refusals = 0 /\ cutBusy = FALSE /\ flaps = 0 /\ edits = 0 /\ clears = 0
        /\ bind = [n \in Nodes |-> "none"] /\ oldNat = [n \in Nodes |-> FALSE]
        /\ store = [n \in Nodes |-> "none"] /\ child = FALSE

Up(n) == n = "G" \/ up
Other(n) == IF n = "D" THEN "G" ELSE "D"
Managed == owner # "none"
Mutant(m) == Mode = m
\* The designation decisions read: the PG row, or (mutation) the gateway's own file. A thread
\* reads its parent's, or (mutation) its own, which is never set.
Designated == IF Mode = "localcfg" THEN yaml["G"]
              ELSE IF Thread /\ Mode = "noinherit" THEN "G" ELSE want
\* The one target rule: the designated node while its heartbeat is fresh, else the gateway.
Rule == IF Designated = "D" /\ seen = "online" THEN "D" ELSE "G"
\* A node may hold an execution permit: it owns the current generation, the stored native
\* evidence is not of an ended incarnation, and no unbound live session is left on it.
PermitOK(n) == /\ owner = n /\ bind[n] # "dead"
               /\ ~(bind[n] = "none" /\ sess[n] # "none")
\* Who may post now. An unmanaged channel is the gateway's, as today.
Fence(n) ==
    IF Mode = "localcfg" THEN yaml[n] = n
    ELSE IF ~Managed THEN n = "G" /\ stamp[n] = "legacy"
    ELSE IF Mode = "nofence" THEN view[n]
    ELSE owner = n /\ stamp[n] = "cur"
\* The owner row moves to node m at a new generation: every session becomes old, its native
\* evidence stays with the row it was recorded on (a session already old keeps its own), and
\* every store belongs to an older generation.
NewGen(m) == /\ owner' = m
             /\ stamp' = [n \in Nodes |-> IF sess[n] # "none" THEN "old" ELSE stamp[n]]
             /\ oldNat' = [n \in Nodes |-> IF sess[n] # "none" /\ stamp[n] # "old"
                                         THEN bind[n] = "live" ELSE oldNat[n]]
             /\ bind' = [n \in Nodes |-> "none"]
             /\ store' = [n \in Nodes |-> IF store[n] = "cur" THEN "old" ELSE store[n]]


\* ---- configuration ----
\* An operator edits one node's config file; the two files may differ.
EditYaml(n) == /\ edits < MaxEdits /\ edits' = edits + 1
               /\ yaml' = [yaml EXCEPT ![n] = Other(yaml[n])]
               /\ UNCHANGED <<want, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, bind, oldNat, store, clears, child>>
\* Only the hub (G here) seeds the PG designation from its file.
Sync == /\ want # yaml["G"] /\ want' = yaml["G"]
        /\ UNCHANGED <<yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>

\* ---- the designated node D ----
DDown == /\ up /\ flaps < MaxFlaps /\ up' = FALSE /\ flaps' = flaps + 1
         /\ UNCHANGED <<want, yaml, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, edits, bind, oldNat, store, clears, child>>
\* D boots, reads the PG owner first, and ends a session of an older generation that its old
\* row names; an unnamed one is only kept from restoring its turn.
DUp == /\ ~up /\ up' = TRUE
       /\ view' = [view EXCEPT !["D"] = PermitOK("D")]
       /\ IF stamp["D"] = "old" /\ sess["D"] # "none"
          THEN /\ sess' = [sess EXCEPT !["D"] = IF oldNat["D"] THEN "none" ELSE "idle"]
               /\ run' = [run EXCEPT !["D"] = 0]
          ELSE /\ sess' = sess /\ run' = run
       /\ UNCHANGED <<want, yaml, seen, owner, stamp, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
SeenOffline == /\ ~up /\ seen = "online" /\ seen' = "offline"
               /\ UNCHANGED <<want, yaml, up, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
SeenOnline == /\ up /\ seen = "offline" /\ seen' = "online"
              /\ UNCHANGED <<want, yaml, up, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>

\* ---- the user ----
Unreferenced(t) == qtok # t /\ \A n \in Nodes : run[n] # t
Arrive == /\ q = "none"
          /\ \E t \in Tok : /\ fate[t] \in {"free", "answered", "noticed"} /\ Unreferenced(t)
                            /\ fate' = [fate EXCEPT ![t] = "open"]
                            /\ ans' = [ans EXCEPT ![t] = 0] /\ qtok' = t
          /\ q' = "arrived"
          /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, tgt, proof, clean, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* The owner clears its idle session: the native incarnation ends.
Clear(n) == /\ clears < MaxClears /\ clears' = clears + 1 /\ Managed /\ owner = n /\ Up(n)
            /\ sess[n] = "idle" /\ sess' = [sess EXCEPT ![n] = "none"]
            /\ bind' = [bind EXCEPT ![n] = IF @ = "live" THEN "dead" ELSE @]
            /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, oldNat, store, child>>

\* ---- an input at the gateway ----
RouteLocal == /\ q = "arrived" /\ ~Managed /\ Designated = "G" /\ sess["G"] # "busy"
              /\ sess' = [sess EXCEPT !["G"] = "busy"] /\ stamp' = [stamp EXCEPT !["G"] = "legacy"]
              /\ run' = [run EXCEPT !["G"] = qtok] /\ q' = "none" /\ qtok' = 0
              /\ UNCHANGED <<want, yaml, up, seen, owner, view, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
BeforeSticky == /\ Mode = "before" /\ q = "arrived" /\ Designated = "D" /\ sess["G"] = "idle"
                /\ sess' = [sess EXCEPT !["G"] = "busy"] /\ stamp' = [stamp EXCEPT !["G"] = "legacy"]
                /\ run' = [run EXCEPT !["G"] = qtok] /\ q' = "none" /\ qtok' = 0
                /\ UNCHANGED <<want, yaml, up, seen, owner, view, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* main: with no session the placement leaves the gateway, and an O channel refuses it.
BeforeRefuse == /\ Mode = "before" /\ q = "arrived" /\ Designated = "D" /\ sess["G"] = "none"
                /\ refusals' = 1 /\ fate' = [fate EXCEPT ![qtok] = "noticed"]
                /\ q' = "none" /\ qtok' = 0
                /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, tgt, proof, clean, ans, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* #7336 as it is: the gateway's O hold check refuses before routing; the input is retried.
HoldFirst == /\ Mode = "as7336" /\ q = "arrived" /\ owner = "D" /\ ~view["G"]
             /\ refusals = 0 /\ refusals' = 1
             /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* The input goes into shared custody. The first managed row adopts the gateway's live session;
\* this design adopts only an idle one and records its native binding for cleanup and permit.
Stage == /\ Mode # "before" /\ q = "arrived" /\ (Designated = "D" \/ Managed)
         /\ ~(Mode = "as7336" /\ owner = "D" /\ ~view["G"])
         /\ (Mode = "as7336" \/ Managed \/ sess["G"] # "busy")
         /\ IF ~Managed /\ sess["G"] # "none"
            THEN /\ owner' = "G"
                 /\ stamp' = [stamp EXCEPT !["G"] = IF sess["G"] = "idle" THEN "cur" ELSE "legacy"]
                 /\ bind' = [bind EXCEPT !["G"] =
                        IF Mode \in {"as7336", "noadoptnative"} THEN "none" ELSE "live"]
                 /\ store' = [store EXCEPT !["G"] = "cur"]
            ELSE /\ owner' = owner /\ stamp' = stamp /\ bind' = bind /\ store' = store
         /\ q' = "held" /\ tgt' = Rule /\ proof' = Designated
         /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, qtok, clean, ans, fate, refusals, cutBusy, flaps, edits, oldNat, clears, child>>
\* The lease holder's tick plans again when the target rule or the stored designation proof no
\* longer holds, also for an admitted input its target never claimed. The mutation re-plans
\* only for availability, as a raw-revision-only query misses an agent or parent change.
Replan == /\ Mode # "before" /\ q \in {"held", "admitted"}
          /\ IF Mode = "rawonly" THEN tgt # Rule /\ proof = Designated
             ELSE tgt # Rule \/ proof # Designated
          /\ q' = "held" /\ tgt' = Rule /\ proof' = Designated
          /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, qtok, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* Every hand-over rechecks that the plan's designation proof is still the stored one.
PromoteSame == /\ q = "held" /\ clean = "none" /\ owner \in {"none", tgt} /\ proof = Designated
               /\ owner' = tgt /\ q' = "admitted"
               /\ UNCHANGED <<want, yaml, up, seen, sess, stamp, run, view, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
RequestCleanup == /\ q = "held" /\ clean = "none" /\ owner \notin {"none", tgt}
                  /\ clean' = "pending"
                  /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* The old owner answers: Busy while its turn runs, Ended once its named session is gone.
\* A live session the stored evidence does not name cannot be ended (NeedsOperator).
Respond == /\ clean \in {"pending", "busy"} /\ Managed /\ Up(owner)
           /\ IF sess[owner] = "busy" /\ Mode # "nowait"
              THEN /\ clean = "pending" /\ clean' = "busy"
                   /\ sess' = sess /\ run' = run /\ fate' = fate /\ cutBusy' = cutBusy
              ELSE IF sess[owner] # "none" /\ bind[owner] # "live"
              THEN /\ clean' = "expired"
                   /\ sess' = sess /\ run' = run /\ fate' = fate /\ cutBusy' = cutBusy
              ELSE /\ clean' = "ended"
                   /\ sess' = [sess EXCEPT ![owner] = "none"]
                   /\ run' = [run EXCEPT ![owner] = 0]
                   /\ cutBusy' = (cutBusy \/ sess[owner] = "busy")
                   /\ fate' = IF sess[owner] = "busy" /\ fate[run[owner]] = "open"
                              THEN [fate EXCEPT ![run[owner]] = "noticed"] ELSE fate
           /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, proof, ans, refusals, flaps, edits, bind, oldNat, store, clears, child>>
Expire == /\ Managed /\ (clean = "pending" \/ (clean = "busy" /\ ~Up(owner)))
          /\ clean' = "expired"
          /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* One PG transaction moves the generation; a turn the old owner still runs is withheld with
\* a notice to the user.
Transfer == /\ q = "held" /\ clean \in {"ended", "expired"} /\ proof = Designated
            /\ NewGen(tgt) /\ clean' = "none" /\ q' = "admitted"
            /\ fate' = IF sess[owner] = "busy" /\ fate[run[owner]] = "open"
                       THEN [fate EXCEPT ![run[owner]] = "noticed"] ELSE fate
            /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, qtok, tgt, proof, ans, refusals, cutBusy, flaps, edits, clears, child>>
\* With no input waiting, the lease holder's tick moves a channel whose owner went offline in
\* the middle of a turn; that turn is withheld with a notice.
OrphanMove == /\ Mode \notin {"before", "as7336"} /\ owner = "D" /\ ~up /\ seen = "offline"
              /\ q = "none" /\ clean = "none" /\ sess["D"] = "busy" /\ fate[run["D"]] = "open"
              /\ NewGen("G") /\ fate' = [fate EXCEPT ![run["D"]] = "noticed"]
              /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, q, qtok, tgt, proof, clean, ans, refusals, cutBusy, flaps, edits, clears, child>>
\* The owner whose own session ended opens a new generation for itself under the custody lock.
Renew(n) == /\ Mode # "norenew" /\ Up(n) /\ owner = n /\ bind[n] = "dead" /\ sess[n] = "none"
            /\ NewGen(n)
            /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, clears, child>>
\* A thread input admitted to D makes D record the thread under its O parent.
RecordChild == /\ Thread /\ Mode # "nochild" /\ ~child /\ Up("D")
               /\ q = "admitted" /\ tgt = "D" /\ child' = TRUE
               /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears>>
\* The owner's worker claims the admitted input and starts a turn in a session of the current
\* generation. Its writer retires a store of an older generation first (mutation: reuses it).
Start(n) == /\ q = "admitted" /\ tgt = n /\ owner = n /\ Up(n) /\ view[n] /\ PermitOK(n)
            /\ sess[n] # "busy" /\ (Thread /\ n = "D" => child)
            /\ sess' = [sess EXCEPT ![n] = "busy"] /\ stamp' = [stamp EXCEPT ![n] = "cur"]
            /\ bind' = [bind EXCEPT ![n] = "live"]
            /\ store' = [store EXCEPT ![n] =
                   IF Mode = "reusestore" /\ @ = "old" THEN "old" ELSE "cur"]
            /\ run' = [run EXCEPT ![n] = qtok] /\ q' = "none" /\ qtok' = 0
            /\ UNCHANGED <<want, yaml, up, seen, owner, view, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, oldNat, clears, child>>
\* The turn's answer reaches Discord.
Post(n) == /\ sess[n] = "busy" /\ Up(n) /\ Fence(n)
           /\ ans' = [ans EXCEPT ![run[n]] = IF @ < 2 THEN @ + 1 ELSE @]
           /\ fate' = IF fate[run[n]] = "open" THEN [fate EXCEPT ![run[n]] = "answered"] ELSE fate
           /\ sess' = [sess EXCEPT ![n] = "idle"] /\ run' = [run EXCEPT ![n] = 0]
           /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, proof, clean, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* The turn ends but its answer is withheld by the owner check.
Withheld(n) == /\ sess[n] = "busy" /\ Up(n) /\ ~Fence(n)
               /\ sess' = [sess EXCEPT ![n] = "idle"] /\ run' = [run EXCEPT ![n] = 0]
               /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* The 2 s owner refresh on each node.
Refresh(n) == /\ Up(n) /\ view[n] # PermitOK(n) /\ view' = [view EXCEPT ![n] = PermitOK(n)]
              /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* The old owner's cleanup loop ends an idle session of an older generation its old row names.
StaleEnd(n) == /\ Up(n) /\ sess[n] = "idle" /\ stamp[n] = "old" /\ oldNat[n]
               /\ sess' = [sess EXCEPT ![n] = "none"]
               /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>
\* Mutation: an admitted input leaves the queue with no turn, answer or notice.
DropAdmitted == /\ Mode = "drop" /\ q = "admitted" /\ q' = "none" /\ qtok' = 0
                /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child>>

Next == \/ \E n \in Nodes : EditYaml(n) \/ Start(n) \/ Post(n) \/ Withheld(n)
                            \/ Refresh(n) \/ StaleEnd(n) \/ Clear(n) \/ Renew(n)
        \/ Sync \/ DDown \/ DUp \/ SeenOffline \/ SeenOnline \/ Arrive
        \/ RouteLocal \/ BeforeSticky \/ BeforeRefuse \/ HoldFirst \/ Stage \/ Replan
        \/ PromoteSame \/ RequestCleanup \/ Respond \/ Expire \/ Transfer \/ RecordChild
        \/ DropAdmitted \/ OrphanMove

\* New messages, restarts, clears and edits are the environment and carry no fairness.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Sync) /\ WF_vars(SeenOffline) /\ WF_vars(SeenOnline)
        /\ WF_vars(RouteLocal) /\ WF_vars(BeforeSticky) /\ WF_vars(BeforeRefuse)
        /\ WF_vars(Stage) /\ WF_vars(Replan) /\ WF_vars(PromoteSame)
        /\ WF_vars(RequestCleanup) /\ WF_vars(Respond) /\ WF_vars(Expire) /\ WF_vars(Transfer)
        /\ WF_vars(RecordChild) /\ WF_vars(OrphanMove)
        /\ \A n \in Nodes : /\ WF_vars(Start(n)) /\ WF_vars(Post(n)) /\ WF_vars(Withheld(n))
                            /\ WF_vars(Refresh(n)) /\ WF_vars(StaleEnd(n)) /\ WF_vars(Renew(n))

\* ---- rules ----
\* At most one node can put this channel's answer on Discord at any moment.
SingleWriter == Cardinality({n \in Nodes : Up(n) /\ sess[n] = "busy" /\ Fence(n)}) <= 1
\* No input is answered twice, and none is answered after the user was told it was withheld.
NoDuplicate == \A t \in Tok : ans[t] <= 1 /\ ~(fate[t] = "noticed" /\ ans[t] > 0)
\* Every received input is answered, notified, or still held: in the gateway's queue or in
\* a running turn. Nothing else may leave a received input behind.
Conservation == \A t \in Tok : fate[t] = "open" => (qtok = t \/ \E n \in Nodes : run[n] = t)
\* The user never sees a refusal for an input a non-home node received.
NoRefusal == refusals = 0
\* A turn whose owner answered Busy is never cut by a move.
BusyTurnFinishes == ~cutBusy
\* A node that posts for a managed channel writes from a store of the current generation.
CurrentStore == \A n \in Nodes : (Managed /\ Up(n) /\ sess[n] = "busy" /\ Fence(n)) => store[n] = "cur"

Expected == IF want = "D" /\ up THEN "D" ELSE "G"
\* Every received input is eventually answered or notified, with or without new messages.
Completes == \A t \in Tok : [](fate[t] = "open" => <>(fate[t] # "open"))
\* Every input leaves the gateway's queue.
Drains == [](q # "none" => <>(q = "none"))
\* Once restarts and edits stop, every turn that runs, runs on the expected home. It says
\* nothing while no message comes: the home moves with the next message.
ServesOnExpected == <>[](\A n \in Nodes : (Up(n) /\ sess[n] = "busy") => n = Expected)
\* A session of an older generation ends while its node is up.
OldSessionEnds == \A n \in Nodes :
    [](Up(n) /\ sess[n] # "none" /\ stamp[n] = "old" => <>(~Up(n) \/ sess[n] = "none" \/ stamp[n] # "old"))
=============================================================================
