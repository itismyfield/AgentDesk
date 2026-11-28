---------------------------- MODULE ChannelHome ----------------------------
\* One O channel whose execution node (O home) is set per channel (#8486).
\* D is the designated node; G holds the Discord gateway lease and runs the channel when D
\* cannot. The lease holder is fixed here; GatewayLease.tla covers how the lease moves.
\* Removal = TRUE: an edit may also remove the designation once set.
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
\* "nochild" never records a thread child on the designated node; "indexlag" admits from a
\* designation index that lags the PG row; "nonotice" withholds a started turn without
\* reserving its notice; "nocancel" keeps a move's cleanup when the plan returns to the owner;
\* "nonehold" plans nothing for a held input once the designation is removed; "renewrace"
\* renews during a move's cleanup and judges a plan without its owner generation;
\* "nodelivery" completes a withheld input without delivering its notice; "clearleak" lets
\* /clear drop a held input but keep the move's cleanup; "clearlocal" leaves a held input
\* of a remote or absent owner uncancelled by /clear; "heldguard" ends an old generation's
\* native only while the move's input is still held; "storeonly" retires the store but
\* keeps the writer's binding log, era entry and thread candidate; "clearclaimed" lets /clear
\* skip an input a worker claimed but has not begun; "nofresh" leaves no fresh-session mark
\* when /clear meets an offline owner's idle session; "waitdebt" renews only once the writer
\* owes nothing, also an announcement the ended native can never seal; "noresume" leaves the
\* next held input waiting on a cancelled cleanup.
\* Debt = TRUE: an answer may leave an announcement that only a later record of the live
\* native seals. Queue2 = TRUE: a second input may wait in custody behind the held head.
\* Toks: how many inputs can be in flight at once (ids are reused).
EXTENDS Naturals, FiniteSets
CONSTANTS Mode, Thread, MaxFlaps, MaxEdits, MaxClears, Removal, Debt, Queue2, Toks

Nodes == {"D", "G"}
Vals == {"none", "G", "D"}
Tok == 1..Toks

VARIABLES want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park
vars == <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>

TypeOK == /\ want \in Vals /\ yaml \in [Nodes -> Vals] /\ up \in BOOLEAN
          /\ seen \in {"online", "offline"} /\ owner \in {"none", "D", "G"}
          /\ sess \in [Nodes -> {"none", "idle", "busy"}]
          /\ stamp \in [Nodes -> {"cur", "old", "legacy"}]
          /\ run \in [Nodes -> 0..3] /\ view \in [Nodes -> BOOLEAN]
          /\ q \in {"none", "arrived", "held", "admitted", "claimed"} /\ qtok \in 0..3
          /\ tgt \in Nodes /\ proof \in Vals
          /\ clean \in {"none", "pending", "busy", "ended", "expired"}
          /\ ans \in [Tok -> 0..2] /\ fate \in [Tok -> {"free", "open", "withheld", "answered", "noticed"}]
          /\ refusals \in 0..1 /\ cutBusy \in BOOLEAN
          /\ bind \in [Nodes -> {"none", "live", "dead"}] /\ oldNat \in [Nodes -> BOOLEAN]
          /\ store \in [Nodes -> {"none", "cur", "old"}] /\ child \in BOOLEAN
          /\ note \in [Tok -> BOOLEAN] /\ qs \in BOOLEAN /\ misplaced \in BOOLEAN
          /\ cstale \in BOOLEAN /\ idx \in Vals /\ selfCut \in BOOLEAN
          /\ ndel \in [Tok -> 0..2] /\ res \in [Nodes -> {"none", "cur", "old"}]
          /\ qclr \in BOOLEAN /\ ranCleared \in BOOLEAN
          /\ fresh \in BOOLEAN /\ cleared \in [Nodes -> BOOLEAN] /\ unsealed \in [Nodes -> BOOLEAN]
          /\ q2 \in 0..3 /\ q2park \in BOOLEAN

Init == /\ want = "none" /\ yaml = [n \in Nodes |-> "none"] /\ up = TRUE /\ seen = "online"
        /\ owner = "none" /\ sess = [n \in Nodes |-> "none"]
        /\ stamp = [n \in Nodes |-> "legacy"] /\ run = [n \in Nodes |-> 0]
        /\ view = [n \in Nodes |-> FALSE] /\ q = "none" /\ qtok = 0 /\ tgt = "G" /\ proof = "none"
        /\ clean = "none" /\ ans = [t \in Tok |-> 0] /\ fate = [t \in Tok |-> "free"]
        /\ refusals = 0 /\ cutBusy = FALSE /\ flaps = 0 /\ edits = 0 /\ clears = 0
        /\ bind = [n \in Nodes |-> "none"] /\ oldNat = [n \in Nodes |-> FALSE]
        /\ store = [n \in Nodes |-> "none"] /\ child = FALSE
        /\ note = [t \in Tok |-> FALSE] /\ qs = FALSE /\ misplaced = FALSE /\ cstale = FALSE
        /\ idx = "none" /\ selfCut = FALSE /\ ndel = [t \in Tok |-> 0]
        /\ res = [n \in Nodes |-> "none"] /\ qclr = FALSE /\ ranCleared = FALSE
        /\ fresh = FALSE /\ cleared = [n \in Nodes |-> FALSE] /\ unsealed = [n \in Nodes |-> FALSE]
        /\ q2 = 0 /\ q2park = FALSE

Up(n) == n = "G" \/ up
Other(n) == IF n = "D" THEN "G" ELSE "D"
Managed == owner # "none"
Mutant(m) == Mode = m
\* The designation decisions read: the PG row, or (mutation) the gateway's own file. A thread
\* reads its parent's, or (mutation) its own, which is never set. "none" means no designation.
Designated == IF Mode = "localcfg" THEN yaml["G"]
              ELSE IF Thread /\ Mode = "noinherit" THEN "none" ELSE want
\* What admission reads to choose the entry: the PG row, or (mutation) a lagging index.
Entry == IF Mode = "indexlag" THEN idx ELSE Designated
\* Where an input that arrived under an unchanged setting must run: the designated node while
\* its heartbeat is fresh, else the gateway. It follows the PG row, whatever a node reads.
Intended == IF want = "D" /\ seen = "online" THEN "D" ELSE "G"
Placed(n) == misplaced' = (misplaced \/ (qs /\ n # Intended))
\* The one target rule: the designated node while its heartbeat is fresh, else the gateway.
Rule == IF Designated = "D" /\ seen = "online" THEN "D" ELSE "G"
\* A node may hold an execution permit: it owns the current generation, the stored native
\* evidence is not of an ended incarnation, and no unbound live session is left on it.
PermitOK(n) == /\ owner = n /\ bind[n] # "dead"
               /\ ~(bind[n] = "none" /\ sess[n] # "none")
\* Who may post now. An unmanaged channel is the gateway's, as today.
Fence(n) ==
    IF Mode = "localcfg" THEN (IF yaml[n] = "D" THEN "D" ELSE "G") = n
    ELSE IF ~Managed THEN n = "G" /\ stamp[n] = "legacy"
    ELSE IF Mode = "nofence" THEN view[n]
    ELSE owner = n /\ stamp[n] = "cur"
\* The owner row moves to node m at a new generation: every session becomes old, its native
\* evidence stays with the row it was recorded on (a session already old keeps its own), and
\* every other node's store belongs to an older generation. A renew by the same owner keeps
\* its store: only a change of executor retires it.
\* A started turn loses its execution right: its receipt is withheld and, in the same
\* transaction, its notice is reserved in the outbox (mutation "nonotice" reserves none).
Withhold(cond, t) == IF cond /\ fate[t] = "open"
                     THEN /\ fate' = [fate EXCEPT ![t] = IF Mode = "nodelivery" THEN "noticed" ELSE "withheld"]
                          /\ note' = [note EXCEPT ![t] = (Mode \notin {"nonotice", "nodelivery"})]
                     ELSE fate' = fate /\ note' = note
NewGen(m) == /\ owner' = m
             /\ stamp' = [n \in Nodes |-> IF sess[n] # "none" THEN "old" ELSE stamp[n]]
             /\ oldNat' = [n \in Nodes |-> IF sess[n] # "none" /\ stamp[n] # "old"
                                         THEN bind[n] = "live" ELSE oldNat[n]]
             /\ bind' = [n \in Nodes |-> "none"]
             /\ store' = [n \in Nodes |-> IF store[n] = "cur" /\ ~(n = m /\ owner = m)
                                         THEN "old" ELSE store[n]]
             /\ res' = [n \in Nodes |-> IF res[n] = "cur" /\ ~(n = m /\ owner = m)
                                       THEN "old" ELSE res[n]]
             /\ fresh' = FALSE


\* ---- configuration ----
\* An operator edits one node's config file; the two files may differ. Only the hub's file
\* feeds the PG row; the file-judging mutation edits both, between the two nodes only.
\* Without Removal an edit sets a node; with it an edit may also clear the designation.
EditYaml(n) == /\ edits < MaxEdits /\ edits' = edits + 1 /\ (Mode = "localcfg" \/ n = "G")
               /\ \E v \in (IF Removal /\ Mode # "localcfg" THEN Vals ELSE Nodes) \ {yaml[n]} :
                     yaml' = [yaml EXCEPT ![n] = v]
               /\ UNCHANGED <<want, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* Only the hub (G here) seeds the PG designation from its file.
Sync == /\ want # yaml["G"] /\ want' = yaml["G"] /\ qs' = FALSE
        /\ UNCHANGED <<yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* Mutation: the in-memory designation index catches up with the PG row later.
IndexRefresh == /\ Mode = "indexlag" /\ idx # want /\ idx' = want
                /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>

\* ---- the designated node D ----
DDown == /\ up /\ flaps < MaxFlaps /\ up' = FALSE /\ flaps' = flaps + 1
         /\ UNCHANGED <<want, yaml, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* D boots, reads the PG owner first, and ends a session of an older generation that its old
\* row names; an unnamed one is only kept from restoring its turn.
DUp == /\ ~up /\ up' = TRUE
       /\ view' = [view EXCEPT !["D"] = PermitOK("D")]
       /\ IF stamp["D"] = "old" /\ sess["D"] # "none"
          THEN /\ sess' = [sess EXCEPT !["D"] =
                      IF oldNat["D"] /\ (Mode # "heldguard" \/ q = "held") THEN "none" ELSE "idle"]
               /\ run' = [run EXCEPT !["D"] = 0]
          ELSE /\ sess' = sess /\ run' = run
       /\ UNCHANGED <<want, yaml, seen, owner, stamp, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
SeenOffline == /\ ~up /\ seen = "online" /\ seen' = "offline" /\ qs' = FALSE
               /\ UNCHANGED <<want, yaml, up, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
SeenOnline == /\ up /\ seen = "offline" /\ seen' = "online" /\ qs' = FALSE
              /\ UNCHANGED <<want, yaml, up, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>

\* ---- the user ----
Unreferenced(t) == qtok # t /\ q2 # t /\ \A n \in Nodes : run[n] # t
Arrive == /\ q = "none" /\ q2 = 0 /\ qs' = TRUE
          /\ \E t \in Tok : /\ fate[t] \in {"free", "answered", "noticed"} /\ Unreferenced(t) /\ ~note[t]
                            /\ fate' = [fate EXCEPT ![t] = "open"]
                            /\ ans' = [ans EXCEPT ![t] = 0] /\ qtok' = t
                            /\ ndel' = [ndel EXCEPT ![t] = 0]
          /\ q' = "arrived"
          /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, tgt, proof, clean, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, misplaced, cstale, idx, selfCut, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* /clear at the gateway, whoever owns the channel. Under the custody lock it discards every
\* waiting input that has not begun (held, admitted or claimed) with a notice and cancels the
\* move's cleanup of that generation. The owner's idle session ends where the owner is up; an
\* offline owner's row gets a fresh-session mark that its next activation honours.
ClearHeld == /\ (q \in {"held", "admitted", "claimed"} \/ q2 # 0)
             /\ ~(Mode = "clearlocal" /\ owner # "G") /\ ~(Mode = "clearclaimed" /\ q = "claimed")
Gone == ((IF q \in {"held", "admitted", "claimed"} THEN {qtok} ELSE {}) \cup {q2}) \ {0}
Clear == /\ clears < MaxClears /\ clears' = clears + 1
         /\ ~(Managed /\ sess[owner] = "busy")
         /\ (q \in {"held", "admitted", "claimed"} \/ q2 # 0 \/ (Managed /\ sess[owner] = "idle"))
         /\ IF ClearHeld
            THEN /\ q' = IF q \in {"held", "admitted", "claimed"} THEN "none" ELSE q
                 /\ qtok' = IF q \in {"held", "admitted", "claimed"} THEN 0 ELSE qtok
                 /\ qs' = FALSE /\ qclr' = FALSE /\ q2' = 0 /\ q2park' = FALSE
                 /\ fate' = [t \in Tok |-> IF t \in Gone THEN "withheld" ELSE fate[t]]
                 /\ note' = [t \in Tok |-> IF t \in Gone THEN TRUE ELSE note[t]]
                 /\ clean' = IF Mode = "clearleak" THEN clean ELSE "none"
                 /\ cstale' = IF Mode = "clearleak" THEN cstale ELSE FALSE
            ELSE /\ q' = q /\ qtok' = qtok /\ qs' = qs /\ fate' = fate /\ note' = note
                 /\ q2' = q2 /\ q2park' = q2park
                 /\ clean' = clean /\ cstale' = cstale
                 /\ qclr' = (qclr \/ q \in {"held", "admitted", "claimed"})
         /\ IF Managed /\ sess[owner] = "idle" /\ Up(owner)
            THEN /\ sess' = [sess EXCEPT ![owner] = "none"]
                 /\ bind' = [bind EXCEPT ![owner] = IF @ = "live" THEN "dead" ELSE @]
                 /\ fresh' = fresh /\ cleared' = cleared
            ELSE IF Managed /\ sess[owner] = "idle"
            THEN /\ sess' = sess /\ bind' = bind
                 /\ fresh' = (fresh \/ Mode # "nofresh")
                 /\ cleared' = [cleared EXCEPT ![owner] = TRUE]
            ELSE sess' = sess /\ bind' = bind /\ fresh' = fresh /\ cleared' = cleared
         /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, run, view, tgt, proof, ans, refusals, cutBusy, flaps, edits, oldNat, store, child, misplaced, idx, selfCut, ndel, res, ranCleared, unsealed>>
\* /cancel-queued on the held head: it leaves custody with a notice and its move's cleanup is
\* cancelled; the input behind it becomes the head (mutation "noresume": it waits on the
\* cancelled cleanup).
CancelHead == /\ Queue2 /\ clears < MaxClears /\ clears' = clears + 1 /\ q = "held"
              /\ q' = "none" /\ qtok' = 0 /\ qs' = FALSE
              /\ fate' = [fate EXCEPT ![qtok] = "withheld"] /\ note' = [note EXCEPT ![qtok] = TRUE]
              /\ clean' = "none" /\ cstale' = FALSE
              /\ q2park' = (Mode = "noresume" /\ q2 # 0 /\ clean # "none")
              /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, tgt, proof, ans, refusals, cutBusy, flaps, edits, bind, oldNat, store, child, misplaced, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2>>
\* A second message waits in custody behind the held head.
Arrive2 == /\ Queue2 /\ q = "held" /\ q2 = 0
           /\ \E t \in Tok : /\ fate[t] \in {"free", "answered", "noticed"} /\ Unreferenced(t) /\ ~note[t]
                             /\ fate' = [fate EXCEPT ![t] = "open"]
                             /\ ans' = [ans EXCEPT ![t] = 0] /\ q2' = t
                             /\ ndel' = [ndel EXCEPT ![t] = 0]
           /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, res, qclr, ranCleared, fresh, cleared, unsealed, q2park>>
\* Once the head left custody, the next waiting input becomes the head and is planned.
Promote2 == /\ q = "none" /\ q2 # 0 /\ ~q2park
            /\ q' = "held" /\ qtok' = q2 /\ q2' = 0 /\ tgt' = Rule /\ proof' = Designated /\ qs' = FALSE
            /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2park>>

\* ---- an input at the gateway ----
\* An undesignated, unmanaged channel stays on today's path (main also keeps an explicit G there).
RouteLocal == /\ q = "arrived" /\ ~Managed /\ sess["G"] # "busy"
              /\ (Entry = "none" \/ (Mode = "before" /\ Entry = "G"))
              /\ sess' = [sess EXCEPT !["G"] = "busy"] /\ stamp' = [stamp EXCEPT !["G"] = "legacy"]
              /\ run' = [run EXCEPT !["G"] = qtok] /\ q' = "none" /\ qtok' = 0 /\ qs' = FALSE
              /\ Placed("G")
              /\ UNCHANGED <<want, yaml, up, seen, owner, view, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
BeforeSticky == /\ Mode = "before" /\ q = "arrived" /\ Designated = "D" /\ sess["G"] = "idle"
                /\ sess' = [sess EXCEPT !["G"] = "busy"] /\ stamp' = [stamp EXCEPT !["G"] = "legacy"]
                /\ run' = [run EXCEPT !["G"] = qtok] /\ q' = "none" /\ qtok' = 0 /\ qs' = FALSE
                /\ Placed("G")
                /\ UNCHANGED <<want, yaml, up, seen, owner, view, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* main: with no session the placement leaves the gateway, and an O channel refuses it.
BeforeRefuse == /\ Mode = "before" /\ q = "arrived" /\ Designated = "D" /\ sess["G"] = "none"
                /\ refusals' = 1 /\ fate' = [fate EXCEPT ![qtok] = "noticed"]
                /\ ndel' = [ndel EXCEPT ![qtok] = 1]
                /\ q' = "none" /\ qtok' = 0 /\ qs' = FALSE
                /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, tgt, proof, clean, ans, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, misplaced, cstale, idx, selfCut, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* #7336 as it is: the gateway's O hold check refuses before routing; the input is retried.
HoldFirst == /\ Mode = "as7336" /\ q = "arrived" /\ owner = "D" /\ ~view["G"]
             /\ refusals = 0 /\ refusals' = 1
             /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* The input goes into shared custody. The first managed row adopts the gateway's live session;
\* this design adopts only an idle one and records its native binding for cleanup and permit.
Stage == /\ Mode # "before" /\ q = "arrived" /\ (Entry # "none" \/ Managed)
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
         /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, qtok, clean, ans, fate, refusals, cutBusy, flaps, edits, oldNat, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* The lease holder's tick plans again when the target rule or the stored designation proof no
\* longer holds, or its owner generation changed, also for an admitted input its target never
\* claimed. A plan that returns to the current owner cancels the move's cleanup; one whose
\* generation is gone drops that cleanup. A claim of an offline owner that has not begun goes
\* back to custody with it; the owner's begin then fails its fence. Mutations: "rawonly" re-plans only for availability,
\* "nonehold" plans nothing without a designation, "nocancel" keeps the cleanup, "renewrace"
\* ignores the generation.
Replan == /\ Mode # "before" /\ (q \in {"held", "admitted"} \/ (q = "claimed" /\ ~Up(owner)))
          /\ IF Mode = "rawonly" THEN tgt # Rule /\ proof = Designated
             ELSE tgt # Rule \/ proof # Designated \/ (cstale /\ Mode # "renewrace")
          /\ ~(Mode = "nonehold" /\ Designated = "none")
          /\ q' = "held" /\ tgt' = Rule /\ proof' = Designated
          /\ IF (Rule = owner /\ Mode # "nocancel") \/ (cstale /\ Mode # "renewrace")
             THEN clean' = "none" /\ cstale' = FALSE
             ELSE clean' = clean /\ cstale' = cstale
          /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, qtok, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* Every hand-over rechecks that the plan's designation proof is still the stored one.
PromoteSame == /\ q = "held" /\ clean = "none" /\ owner \in {"none", tgt} /\ proof = Designated
               /\ owner' = tgt /\ q' = "admitted"
               /\ UNCHANGED <<want, yaml, up, seen, sess, stamp, run, view, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
RequestCleanup == /\ q = "held" /\ clean = "none" /\ owner \notin {"none", tgt}
                  /\ clean' = "pending"
                  /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* The old owner answers: Busy while its turn runs, Ended once its named session is gone.
\* A live session the stored evidence does not name cannot be ended (NeedsOperator).
\* The ended native's evidence is dead; ending a session for a move to the owner itself is a
\* self-cut. Just before ending, a plan that the current setting returns to the owner cancels.
Respond == /\ clean \in {"pending", "busy"} /\ Managed /\ Up(owner)
           /\ IF sess[owner] = "busy" /\ Mode # "nowait"
              THEN /\ clean = "pending" /\ clean' = "busy"
                   /\ sess' = sess /\ run' = run /\ fate' = fate /\ cutBusy' = cutBusy
                   /\ note' = note /\ bind' = bind /\ selfCut' = selfCut
              ELSE IF sess[owner] # "none" /\ bind[owner] # "live"
              THEN /\ clean' = "expired"
                   /\ sess' = sess /\ run' = run /\ fate' = fate /\ cutBusy' = cutBusy
                   /\ note' = note /\ bind' = bind /\ selfCut' = selfCut
              ELSE IF Rule = owner /\ Mode # "nocancel"
              THEN /\ clean' = "none"
                   /\ sess' = sess /\ run' = run /\ fate' = fate /\ cutBusy' = cutBusy
                   /\ note' = note /\ bind' = bind /\ selfCut' = selfCut
              ELSE /\ clean' = "ended"
                   /\ sess' = [sess EXCEPT ![owner] = "none"]
                   /\ run' = [run EXCEPT ![owner] = 0]
                   /\ bind' = [bind EXCEPT ![owner] = IF @ = "live" THEN "dead" ELSE @]
                   /\ selfCut' = (selfCut \/ (sess[owner] # "none" /\ (tgt = owner \/ Rule = owner)))
                   /\ cutBusy' = (cutBusy \/ sess[owner] = "busy")
                   /\ Withhold(sess[owner] = "busy", run[owner])
           /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, proof, ans, refusals, flaps, edits, oldNat, store, clears, child, qs, misplaced, cstale, idx, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
Expire == /\ Managed /\ (clean = "pending" \/ (clean = "busy" /\ ~Up(owner)))
          /\ clean' = "expired"
          /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* One PG transaction moves the generation; a turn the old owner still runs is withheld with
\* a notice to the user.
Transfer == /\ q = "held" /\ clean \in {"ended", "expired"} /\ proof = Designated /\ ~cstale
            /\ NewGen(tgt) /\ clean' = "none" /\ q' = "admitted"
            /\ Withhold(sess[owner] = "busy", run[owner])
            /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, qtok, tgt, proof, ans, refusals, cutBusy, flaps, edits, clears, child, qs, misplaced, cstale, idx, selfCut, ndel, qclr, ranCleared, cleared, unsealed, q2, q2park>>
\* With no input waiting, the lease holder's tick moves a channel whose owner went offline in
\* the middle of a turn; that turn is withheld with a notice.
OrphanMove == /\ Mode \notin {"before", "as7336"} /\ owner = "D" /\ ~up /\ seen = "offline"
              /\ q = "none" /\ clean = "none" /\ sess["D"] = "busy" /\ fate[run["D"]] = "open"
              /\ NewGen("G") /\ Withhold(TRUE, run["D"])
              /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, q, qtok, tgt, proof, clean, ans, refusals, cutBusy, flaps, edits, clears, child, qs, misplaced, cstale, idx, selfCut, ndel, qclr, ranCleared, cleared, unsealed, q2, q2park>>
\* The owner whose own session ended opens a new generation for itself under the custody lock.
\* It waits while a move's cleanup owns the generation (mutation "renewrace" does not) and
\* while its writer still owes output of that generation.
Renew(n) == /\ Mode # "norenew" /\ Up(n) /\ owner = n /\ bind[n] = "dead" /\ sess[n] = "none"
            /\ (clean = "none" \/ Mode = "renewrace")
            /\ ~unsealed[n] /\ NewGen(n) /\ cstale' = (cstale \/ clean # "none")
            /\ UNCHANGED <<want, yaml, up, seen, sess, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, clears, child, note, qs, misplaced, idx, selfCut, ndel, qclr, ranCleared, cleared, unsealed, q2, q2park>>
\* The notice outbox delivers a reserved notice once; only this makes a withheld input noticed.
SendNotice(t) == /\ fate[t] = "withheld" /\ note[t]
                 /\ fate' = [fate EXCEPT ![t] = "noticed"] /\ note' = [note EXCEPT ![t] = FALSE]
                 /\ ndel' = [ndel EXCEPT ![t] = IF @ < 2 THEN @ + 1 ELSE @]
                 /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, qs, misplaced, cstale, idx, selfCut, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* A thread input admitted to D makes D record the thread under its O parent.
RecordChild == /\ Thread /\ Mode # "nochild" /\ ~child /\ Up("D")
               /\ q = "admitted" /\ tgt = "D" /\ child' = TRUE
               /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* The owner's worker claims the admitted input. Its runtime and attachments are prepared
\* before the input begins.
Claim(n) == /\ q = "admitted" /\ tgt = n /\ owner = n /\ Up(n) /\ view[n] /\ PermitOK(n)
            /\ ~(fresh /\ owner = n) /\ sess[n] # "busy" /\ (Thread /\ n = "D" => child)
            /\ q' = "claimed"
            /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* The claimed input begins under the custody lock in a session of the current generation; a
\* discarded or moved claim fails this fence. Its writer retires a store of an older
\* generation first (mutation: reuses it), with the binding log, era entry and thread candidate
\* of that tenure (mutation "storeonly": activation then refuses the old residue and the writer
\* holds). Reusing an idle session that a /clear covered continues the cleared conversation.
Begin(n) == /\ q = "claimed" /\ tgt = n /\ owner = n /\ Up(n) /\ PermitOK(n)
            /\ ~(fresh /\ owner = n) /\ sess[n] # "busy"
            /\ ~(Mode = "storeonly" /\ res[n] = "old")
            /\ sess' = [sess EXCEPT ![n] = "busy"] /\ stamp' = [stamp EXCEPT ![n] = "cur"]
            /\ bind' = [bind EXCEPT ![n] = "live"]
            /\ store' = [store EXCEPT ![n] =
                   IF Mode = "reusestore" /\ @ = "old" THEN "old" ELSE "cur"]
            /\ res' = [res EXCEPT ![n] = IF Mode = "storeonly" /\ @ = "old" THEN "old" ELSE "cur"]
            /\ run' = [run EXCEPT ![n] = qtok] /\ q' = "none" /\ qtok' = 0 /\ qs' = FALSE
            /\ Placed(n)
            /\ ranCleared' = (ranCleared \/ qclr \/ (sess[n] = "idle" /\ cleared[n]))
            /\ qclr' = FALSE /\ cleared' = [cleared EXCEPT ![n] = FALSE]
            /\ UNCHANGED <<want, yaml, up, seen, owner, view, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, oldNat, clears, child, note, cstale, idx, selfCut, ndel, fresh, unsealed, q2, q2park>>
\* The turn's answer reaches Discord; with Debt it may leave an announcement that a later
\* record of the live native seals.
Post(n) == /\ sess[n] = "busy" /\ Up(n) /\ Fence(n)
           /\ \E u \in (IF Debt THEN BOOLEAN ELSE {FALSE}) : unsealed' = [unsealed EXCEPT ![n] = u]
           /\ ans' = [ans EXCEPT ![run[n]] = IF @ < 2 THEN @ + 1 ELSE @]
           /\ fate' = IF fate[run[n]] = "open" THEN [fate EXCEPT ![run[n]] = "answered"] ELSE fate
           /\ sess' = [sess EXCEPT ![n] = "idle"] /\ run' = [run EXCEPT ![n] = 0]
           /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, proof, clean, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, q2, q2park>>
\* The turn ends but its answer is withheld by the owner check.
Withheld(n) == /\ sess[n] = "busy" /\ Up(n) /\ ~Fence(n)
               /\ sess' = [sess EXCEPT ![n] = "idle"] /\ run' = [run EXCEPT ![n] = 0]
               /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* The 2 s owner refresh on each node.
Refresh(n) == /\ Up(n) /\ view[n] # PermitOK(n) /\ view' = [view EXCEPT ![n] = PermitOK(n)]
              /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* The old owner's cleanup loop ends an idle session of an older generation its old row names.
\* Ending a native after the hand-over needs only the recorded old generation (mutation
\* "heldguard": also a still-held input).
StaleEnd(n) == /\ Up(n) /\ sess[n] = "idle" /\ stamp[n] = "old" /\ oldNat[n]
               /\ (Mode # "heldguard" \/ q = "held")
               /\ sess' = [sess EXCEPT ![n] = "none"]
               /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, unsealed, q2, q2park>>
\* The live native's next record seals the announcement.
Seal(n) == /\ unsealed[n] /\ sess[n] # "none" /\ Up(n) /\ unsealed' = [unsealed EXCEPT ![n] = FALSE]
           /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, q2, q2park>>
\* After the native ended, the writer closes what it can no longer seal with the recorded end
\* (mutation "waitdebt" waits for a seal that never comes).
EndSettle(n) == /\ Mode # "waitdebt" /\ unsealed[n] /\ sess[n] = "none" /\ Up(n)
                /\ unsealed' = [unsealed EXCEPT ![n] = FALSE]
                /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, fresh, cleared, q2, q2park>>
\* The owner's next activation honours the fresh-session mark: it ends the idle session
\* first, so the next input opens a new one.
FreshEnd(n) == /\ fresh /\ owner = n /\ Up(n) /\ sess[n] # "busy" /\ fresh' = FALSE
               /\ sess' = [sess EXCEPT ![n] = "none"] /\ cleared' = [cleared EXCEPT ![n] = FALSE]
               /\ bind' = [bind EXCEPT ![n] = IF sess[n] = "idle" /\ @ = "live" THEN "dead" ELSE @]
               /\ UNCHANGED <<want, yaml, up, seen, owner, stamp, run, view, q, qtok, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, oldNat, store, clears, child, note, qs, misplaced, cstale, idx, selfCut, ndel, res, qclr, ranCleared, unsealed, q2, q2park>>
\* Mutation: an admitted input leaves the queue with no turn, answer or notice.
DropAdmitted == /\ Mode = "drop" /\ q = "admitted" /\ q' = "none" /\ qtok' = 0 /\ qs' = FALSE
                /\ qclr' = FALSE
                /\ UNCHANGED <<want, yaml, up, seen, owner, sess, stamp, run, view, tgt, proof, clean, ans, fate, refusals, cutBusy, flaps, edits, bind, oldNat, store, clears, child, note, misplaced, cstale, idx, selfCut, ndel, res, ranCleared, fresh, cleared, unsealed, q2, q2park>>

Next == \/ \E n \in Nodes : EditYaml(n) \/ Claim(n) \/ Begin(n) \/ Post(n) \/ Withheld(n)
                            \/ Refresh(n) \/ StaleEnd(n) \/ Renew(n) \/ Seal(n)
                            \/ EndSettle(n) \/ FreshEnd(n)
        \/ Sync \/ DDown \/ DUp \/ SeenOffline \/ SeenOnline \/ Arrive \/ Clear
        \/ RouteLocal \/ BeforeSticky \/ BeforeRefuse \/ HoldFirst \/ Stage \/ Replan
        \/ PromoteSame \/ RequestCleanup \/ Respond \/ Expire \/ Transfer \/ RecordChild
        \/ DropAdmitted \/ OrphanMove \/ IndexRefresh \/ \E t \in Tok : SendNotice(t)
        \/ CancelHead \/ Arrive2 \/ Promote2

\* New messages, restarts, clears and edits are the environment and carry no fairness.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Sync) /\ WF_vars(SeenOffline) /\ WF_vars(SeenOnline)
        /\ WF_vars(RouteLocal) /\ WF_vars(BeforeSticky) /\ WF_vars(BeforeRefuse)
        /\ WF_vars(Stage) /\ WF_vars(Replan) /\ WF_vars(PromoteSame)
        /\ WF_vars(RequestCleanup) /\ WF_vars(Respond) /\ WF_vars(Expire) /\ WF_vars(Transfer)
        /\ WF_vars(RecordChild) /\ WF_vars(OrphanMove) /\ WF_vars(IndexRefresh)
        /\ WF_vars(Promote2)
        /\ \A t \in Tok : WF_vars(SendNotice(t))
        /\ \A n \in Nodes : /\ WF_vars(Claim(n)) /\ WF_vars(Begin(n)) /\ WF_vars(Post(n))
                            /\ WF_vars(Withheld(n)) /\ WF_vars(Refresh(n)) /\ WF_vars(StaleEnd(n))
                            /\ WF_vars(Renew(n)) /\ WF_vars(Seal(n)) /\ WF_vars(EndSettle(n))
                            /\ WF_vars(FreshEnd(n))

\* ---- rules ----
\* At most one node can put this channel's answer on Discord at any moment.
SingleWriter == Cardinality({n \in Nodes : Up(n) /\ sess[n] = "busy" /\ Fence(n)}) <= 1
\* No input is answered twice, and none is answered after the user was told it was withheld.
NoDuplicate == \A t \in Tok : ans[t] <= 1 /\ ~(fate[t] \in {"withheld", "noticed"} /\ ans[t] > 0)
\* Every received input is answered, notified, or still held: in the gateway's queue, in a
\* running turn, or as a reserved notice. Nothing else may leave a received input behind.
Conservation == \A t \in Tok : /\ (fate[t] = "open" => (qtok = t \/ q2 = t \/ \E n \in Nodes : run[n] = t))
                               /\ (fate[t] = "withheld" => note[t])
\* An input that arrived under an unchanged setting runs on the intended node.
FirstInputOnHome == ~misplaced
\* A move's cleanup never ends the session of the node the plan now keeps.
NoSelfCleanup == ~selfCut
\* Each input has at most one delivered effect (answer or notice), and a completed input has
\* exactly one.
EffectOnce == \A t \in Tok : /\ ans[t] + ndel[t] <= 1
                            /\ (fate[t] \in {"answered", "noticed"} => ans[t] + ndel[t] = 1)
\* An input the user cleared never starts, and the next input after a /clear never continues
\* the cleared session.
ClearHonored == ~ranCleared
\* The user never sees a refusal for an input a non-home node received.
NoRefusal == refusals = 0
\* A turn whose owner answered Busy is never cut by a move.
BusyTurnFinishes == ~cutBusy
\* A node that posts for a managed channel writes from a store of the current generation.
CurrentStore == \A n \in Nodes : (Managed /\ Up(n) /\ sess[n] = "busy" /\ Fence(n)) => store[n] = "cur"

Expected == IF want = "D" /\ up THEN "D" ELSE "G"
\* Every received input is eventually answered or delivered a notice, with or without new
\* messages.
Completes == \A t \in Tok : [](fate[t] \in {"open", "withheld"} => <>(fate[t] \in {"answered", "noticed"}))
\* No input stays held forever.
HeldResolves == [](q = "held" => <>(q # "held"))
\* No move's cleanup stays forever.
CleanupResolves == [](clean # "none" => <>(clean = "none"))
\* Every input leaves the gateway's queue.
Drains == [](q # "none" => <>(q = "none"))
\* Once restarts and edits stop, every turn that runs, runs on the expected home. It says
\* nothing while no message comes: the home moves with the next message.
ServesOnExpected == <>[](\A n \in Nodes : (Up(n) /\ sess[n] = "busy") => n = Expected)
\* A session of an older generation ends while its node is up.
OldSessionEnds == \A n \in Nodes :
    [](Up(n) /\ sess[n] # "none" /\ stamp[n] = "old" => <>(~Up(n) \/ sess[n] = "none" \/ stamp[n] # "old"))
=============================================================================
