------------------------- MODULE WriterThreadActive -------------------------
EXTENDS Naturals
\* Fix: "before" (main: a child is no O member) | "after" (complete activation).
CONSTANTS Fix, Mut
VARIABLES proc,      \* "new" | "down" | "old": the binary serving the store
          relation,  \* shared child entry: "none" | "unresolved" | "verified"
          cand,      \* child's own adoption: "none" | "held" | "late" | "pending" | "committed" | "parent"
          floor, init, tomb, tombGen,   \* durable store facts
          writer,    \* "none" | "starting" | "ready" | "stopped"
          gen,       \* child lifecycle generation; a superseding input raises it
          input,     \* the thread's newest turn: "none" | "waiting" | "posted" | "failed"
          knock,     \* a producer boundary has not classified the newest input yet
          arrived, oSent, lSent, via,
          row,       \* thread session row: "none" | "live" | "deleted"
          retire,    \* generation a retirement request targets, 0 for none
          deploy,    \* rollback attempt: "idle" | "checked" | "stopped" | "refused" | "swapped"
          booted, crashes
vars == <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, input, knock,
          arrived, oSent, lSent, via, row, retire, deploy, booted, crashes>>

Init ==
    /\ proc = "new" /\ relation = "none" /\ cand = "none"
    /\ floor = FALSE /\ init = FALSE /\ tomb = FALSE /\ tombGen = 0
    /\ writer = "none" /\ gen = 1 /\ input = "none" /\ knock = FALSE
    /\ arrived = 0 /\ oSent = FALSE /\ lSent = FALSE /\ via = "none"
    /\ row = "none" /\ retire = 0 /\ deploy = "idle" /\ booted = "ok" /\ crashes = 0

Seed == IF Mut = "alias" THEN "parent"
        ELSE IF init THEN "committed"
        ELSE IF Mut = "pending" THEN "pending" ELSE "late"

\* Discord intake, dispatch, routine TUI and terminal input all reach one placement boundary.
Arrive ==
    /\ proc = "new" /\ input \in {"none", "posted", "failed"} /\ arrived < 2
    /\ input' = "waiting" /\ knock' = TRUE /\ arrived' = arrived + 1
    /\ oSent' = FALSE /\ lSent' = FALSE /\ via' = "none" /\ row' = "live"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen,
                   retire, deploy, booted, crashes>>

\* The checked parent read either verifies the child or leaves a held, unresolved entry.
Admit ==
    /\ Fix = "after" /\ proc = "new" /\ knock /\ knock' = FALSE
    /\ \/ /\ relation = "none"
          /\ \E ok \in BOOLEAN :
                /\ relation' = IF ok THEN "verified" ELSE "unresolved"
                /\ cand' = IF ok THEN Seed ELSE "held"
       \/ /\ relation # "none" /\ UNCHANGED <<relation, cand>>
    /\ tomb' = FALSE
    /\ gen' = IF tomb \/ retire # 0 THEN gen + 1 ELSE gen
    /\ UNCHANGED <<proc, floor, init, tombGen, writer, input, arrived, oSent, lSent, via,
                   row, retire, deploy, booted, crashes>>

\* The existing 2 s owner refresh resolves a held entry before any provider branch.
Resolve ==
    /\ Mut # "noresolve" /\ proc = "new" /\ relation = "unresolved"
    /\ relation' = "verified" /\ cand' = Seed
    /\ UNCHANGED <<proc, floor, init, tomb, tombGen, writer, gen, input, knock, arrived,
                   oSent, lSent, via, row, retire, deploy, booted, crashes>>

\* Legacy posts a body only where the gate reads the thread as a non-member.
LegacyOpen ==
    \/ Fix = "before"
    \/ (relation = "none" /\ ~knock)
    \/ cand = "pending"
    \/ (Mut = "direct" /\ relation # "none")
LegacyBody ==
    /\ proc = "new" /\ input = "waiting" /\ ~lSent /\ LegacyOpen
    /\ lSent' = TRUE
    /\ input' = IF relation = "none" \/ cand = "pending" THEN "posted" ELSE input
    /\ cand' = IF cand = "pending" THEN "held" ELSE cand
    /\ UNCHANGED <<proc, relation, floor, init, tomb, tombGen, writer, gen, knock, arrived,
                   oSent, via, row, retire, deploy, booted, crashes>>

StartWriter ==
    /\ proc = "new" /\ relation = "verified" /\ cand \in {"late", "committed", "parent"}
    /\ writer \in {"none", "stopped"} /\ ~tomb /\ retire = 0
    /\ writer' = "starting"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes>>
PublishFloor ==
    /\ proc = "new" /\ writer = "starting" /\ ~init /\ ~floor /\ floor' = TRUE
    /\ UNCHANGED <<proc, relation, cand, init, tomb, tombGen, writer, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes>>
PublishInit ==
    /\ proc = "new" /\ writer = "starting" /\ ~init /\ (floor \/ Mut = "reverse")
    /\ cand \in {"late", "parent"}
    /\ init' = TRUE /\ cand' = IF cand = "parent" THEN "parent" ELSE "committed"
    /\ UNCHANGED <<proc, relation, floor, tomb, tombGen, writer, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes>>
\* A failed init keeps the floor and is a visible halt, never a silent Legacy fallback.
InitFail ==
    /\ proc = "new" /\ writer = "starting" /\ floor /\ ~init /\ cand = "late"
    /\ cand' = "held" /\ writer' = "none"
    /\ input' = IF input = "waiting" THEN "failed" ELSE input
    /\ UNCHANGED <<proc, relation, floor, init, tomb, tombGen, gen, knock, arrived,
                   oSent, lSent, via, row, retire, deploy, booted, crashes>>
HeldVisible ==
    /\ proc = "new" /\ relation = "verified" /\ cand = "held" /\ input = "waiting" /\ ~knock
    /\ input' = "failed"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes>>
Ready ==
    /\ proc = "new" /\ writer = "starting"
    /\ \/ (init /\ cand \in {"committed", "parent"})
       \/ Mut = "readyEarly"
    /\ writer' = "ready"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes>>
Post ==
    /\ proc = "new" /\ input = "waiting" /\ ~oSent /\ ~knock
    /\ \/ writer = "ready"
       \/ (Mut = "parentReady" /\ relation = "verified")
    /\ oSent' = TRUE /\ input' = "posted"
    /\ via' = IF writer = "ready" /\ cand # "parent" THEN "child" ELSE "parent"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, knock,
                   arrived, lSent, row, retire, deploy, booted, crashes>>

Down == /\ relation' = "none" /\ cand' = "none" /\ writer' = "none" /\ retire' = 0
Crash ==
    /\ proc = "new" /\ crashes < 1 /\ deploy \in {"idle", "refused"}
    /\ proc' = "down" /\ crashes' = crashes + 1 /\ Down
    /\ UNCHANGED <<floor, init, tomb, tombGen, gen, input, knock, arrived, oSent, lSent,
                   via, row, deploy, booted>>
\* Boot reads recorded children without REST and holds known unrecorded ones.
Boot ==
    /\ proc = "down" /\ deploy \in {"idle", "refused"} /\ proc' = "new"
    /\ relation' = IF init THEN "verified"
                   ELSE IF input = "waiting" /\ Mut # "noBootScan" /\ Fix = "after"
                        THEN "unresolved" ELSE "none"
    /\ cand' = IF relation' = "verified" THEN "committed"
               ELSE IF relation' = "unresolved" THEN "held" ELSE "none"
    /\ UNCHANGED <<floor, init, tomb, tombGen, writer, gen, input, knock, arrived, oSent,
                   lSent, via, row, retire, deploy, booted, crashes>>

\* A rollback to a binary without the child capability, judged before the service stops.
DeployCheck ==
    /\ deploy = "idle" /\ proc = "new"
    /\ deploy' = IF floor /\ Mut # "noguard" THEN "refused" ELSE "checked"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, input,
                   knock, arrived, oSent, lSent, via, row, retire, booted, crashes>>
DeployStop ==
    /\ deploy = "checked" /\ proc = "new" /\ proc' = "down" /\ deploy' = "stopped" /\ Down
    /\ UNCHANGED <<floor, init, tomb, tombGen, gen, input, knock, arrived, oSent, lSent,
                   via, row, booted, crashes>>
\* The floor is judged again once no writer can create it, before the binary is replaced.
DeploySwap ==
    /\ deploy = "stopped"
    /\ IF floor /\ Mut \notin {"norecheck", "noguard"}
       THEN deploy' = "refused" /\ UNCHANGED <<proc, booted>>
       ELSE /\ deploy' = "swapped" /\ proc' = "old"
            /\ booted' = IF init THEN "aborted" ELSE "ok"
    /\ UNCHANGED <<relation, cand, floor, init, tomb, tombGen, writer, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, crashes>>

\* thread_gc deleted the exact idle row; the request names the generation it judged.
GcDelete ==
    /\ proc = "new" /\ row = "live" /\ input \in {"posted", "failed"} /\ ~knock
    /\ relation = "verified" /\ retire = 0 /\ ~tomb
    /\ row' = "deleted" /\ retire' = gen
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, input,
                   knock, arrived, oSent, lSent, via, deploy, booted, crashes>>
Exact == retire = gen /\ row = "deleted" /\ input # "waiting" /\ ~knock
StopWriter ==
    /\ proc = "new" /\ retire # 0 /\ Exact /\ writer \in {"starting", "ready"}
    /\ writer' = "stopped"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes>>
Retire ==
    /\ proc = "new" /\ retire # 0
    /\ \/ /\ Mut = "lateTomb" \/ (Exact /\ writer \in {"none", "stopped"})
          /\ tomb' = TRUE /\ tombGen' = retire /\ retire' = 0
       \/ /\ Mut # "lateTomb" /\ ~Exact
          /\ retire' = 0 /\ UNCHANGED <<tomb, tombGen>>
    /\ UNCHANGED <<proc, relation, cand, floor, init, writer, gen, input, knock, arrived,
                   oSent, lSent, via, row, deploy, booted, crashes>>

Next == Arrive \/ Admit \/ Resolve \/ LegacyBody \/ StartWriter \/ PublishFloor
        \/ PublishInit \/ InitFail \/ HeldVisible \/ Ready \/ Post \/ Crash \/ Boot
        \/ DeployCheck \/ DeployStop \/ DeploySwap \/ GcDelete \/ StopWriter \/ Retire
        \/ UNCHANGED vars
Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Admit) /\ WF_vars(Resolve) /\ WF_vars(LegacyBody) /\ WF_vars(StartWriter)
        /\ WF_vars(PublishFloor) /\ WF_vars(PublishInit) /\ WF_vars(Ready) /\ WF_vars(Post)
        /\ WF_vars(HeldVisible) /\ WF_vars(Boot) /\ WF_vars(Retire) /\ WF_vars(DeploySwap)

NoLegacyBody == ~lSent
ExactlyOnce == ~(oSent /\ lSent)
OwnAuthority == cand # "parent" /\ via # "parent"
FloorBeforeInit == init => floor
ReadyNeedsInit == writer = "ready" => (init /\ cand = "committed")
OldReaderBoots == booted = "ok"
TombExact == tomb => tombGen = gen
\* An admitted child input is posted by its own ready writer or fails visibly.
ChildInputAnswered == [](input = "waiting" => <>(input # "waiting" \/ proc = "old"))
=============================================================================
