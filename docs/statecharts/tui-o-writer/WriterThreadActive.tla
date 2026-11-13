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
          booted, crashes,
          parentO,   \* the parent is an O writer channel; otherwise its threads stay Legacy's
          dormant,   \* the parent is dormant; its children inherit that reader mode
          wmode,     \* the running child writer's reader: "active" | "dormant"
          lSending,  \* a Legacy body send is claimed and not finished
          lateTurn,  \* the open input is the late reservation's own Legacy turn
          oTurn      \* the open input arrived after the child's init committed (handoff)
vars == <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, input, knock,
          arrived, oSent, lSent, via, row, retire, deploy, booted, crashes,
          parentO, dormant, wmode, lSending, lateTurn, oTurn>>

Init ==
    /\ proc = "new" /\ relation = "none" /\ cand = "none"
    /\ floor = FALSE /\ init = FALSE /\ tomb = FALSE /\ tombGen = 0
    /\ writer = "none" /\ gen = 1 /\ input = "none" /\ knock = FALSE
    /\ arrived = 0 /\ oSent = FALSE /\ lSent = FALSE /\ via = "none"
    /\ row = "none" /\ retire = 0 /\ deploy = "idle" /\ booted = "ok" /\ crashes = 0
    /\ parentO \in BOOLEAN /\ dormant \in BOOLEAN /\ (dormant => parentO)
    /\ wmode = "active" /\ lSending = FALSE /\ lateTurn = FALSE /\ oTurn = FALSE

Seed == IF ~parentO THEN "legacy"
        ELSE IF Mut = "alias" THEN "parent"
        ELSE IF init THEN "committed"
        ELSE IF Mut = "pending" THEN "pending" ELSE "late"
Fixed == <<parentO, dormant>>

\* Discord intake, dispatch, routine TUI and terminal input all reach one placement boundary.
Arrive ==
    /\ proc = "new" /\ input \in {"none", "posted", "failed"} /\ arrived < 2 /\ ~lSending
    /\ input' = "waiting" /\ knock' = TRUE /\ arrived' = arrived + 1
    /\ oSent' = FALSE /\ lSent' = FALSE /\ via' = "none" /\ row' = "live"
    /\ lateTurn' = FALSE /\ oTurn' = init
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen,
                   retire, deploy, booted, crashes, Fixed, wmode, lSending>>

\* The placement's checked parent read either records the child or holds the input itself at
\* placement, before it reaches the session, so no body exists yet to hold or lose.
Admit ==
    /\ Fix = "after" /\ proc = "new" /\ knock
    /\ \/ /\ relation = "none"
          /\ \E ok \in BOOLEAN :
                /\ relation' = IF ok THEN "verified" ELSE "unresolved"
                /\ cand' = IF ok THEN Seed ELSE "held"
                /\ knock' = ~ok
                /\ input' = IF ~ok /\ Mut = "dropHeld" THEN "lost" ELSE input
       \/ /\ relation = "verified" /\ knock' = FALSE /\ UNCHANGED <<relation, cand, input>>
    /\ tomb' = FALSE
    /\ gen' = IF tomb \/ retire # 0 THEN gen + 1 ELSE gen
    /\ UNCHANGED <<proc, floor, init, tombGen, writer, arrived, oSent, lSent, via,
                   row, retire, deploy, booted, crashes, Fixed, wmode, lSending, lateTurn,
                   oTurn>>

\* The next input's placement can record a boot-reserved child while Legacy still sends its
\* open turn; that turn stays Legacy's until it ends.
EarlyPlace ==
    /\ Fix = "after" /\ proc = "new" /\ relation = "none" /\ lateTurn /\ input = "waiting"
    /\ ~knock /\ relation' = "verified" /\ cand' = Seed
    /\ UNCHANGED <<proc, floor, init, tomb, tombGen, writer, gen, input, knock, arrived,
                   oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed, wmode,
                   lSending, lateTurn, oTurn>>
\* The held placement is tried again until its checked read succeeds. A child of a Legacy
\* parent becomes Legacy's; the input then goes on as any other.
Resolve ==
    /\ Mut # "noresolve" /\ proc = "new" /\ relation = "unresolved"
    /\ relation' = "verified" /\ cand' = Seed /\ knock' = FALSE
    /\ UNCHANGED <<proc, floor, init, tomb, tombGen, writer, gen, input, arrived,
                   oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed, wmode,
                   lSending, lateTurn, oTurn>>

\* Legacy posts a body where the gate reads the thread as a non-member, a Legacy parent's
\* child, or the late reservation's own turn before the handoff.
LegacyOpen ==
    \/ Fix = "before"
    \/ (relation = "none" /\ ~knock)
    \/ cand \in {"pending", "legacy"}
    \/ (cand = "late" /\ ~init /\ lateTurn)
    \/ (Mut = "direct" /\ relation # "none")
\* The claim is counted until the send ends; the handoff waits on it.
LegacyClaim ==
    /\ proc = "new" /\ input = "waiting" /\ ~lSent /\ ~lSending /\ LegacyOpen
    /\ lSending' = TRUE
    /\ cand' = IF cand = "pending" THEN "held" ELSE cand
    /\ UNCHANGED <<proc, relation, floor, init, tomb, tombGen, writer, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed,
                   wmode, lateTurn, oTurn>>
LegacyDone ==
    /\ proc = "new" /\ lSending
    /\ lSending' = FALSE /\ lSent' = TRUE
    /\ input' = IF input = "waiting" /\ (relation = "none" \/ cand \in {"held", "legacy", "late"})
                 THEN "posted" ELSE input
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, knock,
                   arrived, oSent, via, row, retire, deploy, booted, crashes, Fixed, wmode,
                   lateTurn, oTurn>>

\* A child writer reads in its parent's mode: a dormant parent's child is never active.
StartWriter ==
    /\ proc = "new" /\ relation = "verified" /\ cand \in {"late", "committed", "parent"}
    /\ writer \in {"none", "stopped"} /\ ~tomb /\ retire = 0
    /\ writer' = "starting"
    /\ wmode' = IF dormant /\ Mut # "noInherit" THEN "dormant" ELSE "active"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed,
                   lSending, lateTurn, oTurn>>
PublishFloor ==
    /\ proc = "new" /\ writer = "starting" /\ ~init /\ ~floor /\ floor' = TRUE
    /\ UNCHANGED <<proc, relation, cand, init, tomb, tombGen, writer, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed,
                   wmode, lSending, lateTurn, oTurn>>
\* The handoff: init commits only after the late turn's Legacy send settled, so O starts at
\* the next turn and never re-reads a body Legacy is still sending.
Settled == Mut = "noWait" \/ (~lSending /\ ~(input = "waiting" /\ lateTurn))
PublishInit ==
    /\ proc = "new" /\ writer = "starting" /\ ~init /\ (floor \/ Mut = "reverse")
    /\ cand \in {"late", "parent"} /\ Settled
    /\ init' = TRUE /\ cand' = IF cand = "parent" THEN "parent" ELSE "committed"
    /\ UNCHANGED <<proc, relation, floor, tomb, tombGen, writer, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed,
                   wmode, lSending, lateTurn, oTurn>>
\* A failed init keeps the floor and is a visible halt, never a silent Legacy fallback.
InitFail ==
    /\ proc = "new" /\ writer = "starting" /\ floor /\ ~init /\ cand = "late"
    /\ cand' = "held" /\ writer' = "none"
    /\ input' = IF input = "waiting" /\ ~lateTurn THEN "failed" ELSE input
    /\ UNCHANGED <<proc, relation, floor, init, tomb, tombGen, gen, knock, arrived,
                   oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed,
                   wmode, lSending, lateTurn, oTurn>>
HeldVisible ==
    /\ proc = "new" /\ relation = "verified" /\ cand = "held" /\ input = "waiting" /\ ~knock
    /\ ~lSending
    /\ input' = "failed"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed,
                   wmode, lSending, lateTurn, oTurn>>
Ready ==
    /\ proc = "new" /\ writer = "starting"
    /\ \/ (init /\ cand \in {"committed", "parent"})
       \/ Mut = "readyEarly"
    /\ writer' = "ready"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed,
                   wmode, lSending, lateTurn, oTurn>>
Post ==
    /\ proc = "new" /\ input = "waiting" /\ ~oSent /\ ~knock
    /\ \/ writer = "ready"
       \/ (Mut = "parentReady" /\ relation = "verified")
    /\ oSent' = TRUE /\ input' = "posted"
    /\ via' = IF writer = "ready" /\ cand # "parent" THEN "child" ELSE "parent"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, knock,
                   arrived, lSent, row, retire, deploy, booted, crashes, Fixed,
                   wmode, lSending, lateTurn, oTurn>>

Down == /\ relation' = "none" /\ cand' = "none" /\ writer' = "none" /\ retire' = 0
        /\ lSending' = FALSE
Crash ==
    /\ proc = "new" /\ crashes < 1 /\ deploy \in {"idle", "refused"}
    /\ proc' = "down" /\ crashes' = crashes + 1 /\ Down
    /\ UNCHANGED <<floor, init, tomb, tombGen, gen, input, knock, arrived, oSent, lSent,
                   via, row, deploy, booted, Fixed, wmode, lateTurn, oTurn>>
\* Boot reads recorded children without REST. A known unrecorded child is reserved late: its
\* open turn stays Legacy's, and the next placement records it.
Boot ==
    /\ proc = "down" /\ deploy \in {"idle", "refused"} /\ proc' = "new"
    /\ relation' = IF init THEN "verified" ELSE "none"
    /\ cand' = IF init THEN "committed" ELSE "none"
    /\ lateTurn' = (lateTurn \/ (Fix = "after" /\ Mut # "noBootScan" /\ ~init
                                   /\ input = "waiting" /\ ~knock))
    /\ UNCHANGED <<floor, init, tomb, tombGen, writer, gen, input, knock, arrived, oSent,
                   lSent, via, row, retire, deploy, booted, crashes, Fixed, wmode, lSending,
                   oTurn>>

\* A rollback to a binary without the child capability, judged before the service stops.
DeployCheck ==
    /\ deploy = "idle" /\ proc = "new"
    /\ deploy' = IF floor /\ Mut # "noguard" THEN "refused" ELSE "checked"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, input,
                   knock, arrived, oSent, lSent, via, row, retire, booted, crashes, Fixed,
                   wmode, lSending, lateTurn, oTurn>>
DeployStop ==
    /\ deploy = "checked" /\ proc = "new" /\ proc' = "down" /\ deploy' = "stopped" /\ Down
    /\ UNCHANGED <<floor, init, tomb, tombGen, gen, input, knock, arrived, oSent, lSent,
                   via, row, booted, crashes, Fixed, wmode, lateTurn, oTurn>>
\* The floor is judged again once no writer can create it, before the binary is replaced.
DeploySwap ==
    /\ deploy = "stopped"
    /\ IF floor /\ Mut \notin {"norecheck", "noguard"}
       THEN deploy' = "refused" /\ UNCHANGED <<proc, booted>>
       ELSE /\ deploy' = "swapped" /\ proc' = "old"
            /\ booted' = IF init THEN "aborted" ELSE "ok"
    /\ UNCHANGED <<relation, cand, floor, init, tomb, tombGen, writer, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, crashes, Fixed, wmode, lSending,
                   lateTurn, oTurn>>

\* thread_gc deleted the exact idle row; the request names the generation it judged.
GcDelete ==
    /\ proc = "new" /\ row = "live" /\ input \in {"posted", "failed"} /\ ~knock
    /\ relation = "verified" /\ parentO /\ retire = 0 /\ ~tomb
    /\ row' = "deleted" /\ retire' = gen
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, writer, gen, input,
                   knock, arrived, oSent, lSent, via, deploy, booted, crashes, Fixed, wmode,
                   lSending, lateTurn, oTurn>>
Exact == retire = gen /\ row = "deleted" /\ input # "waiting" /\ ~knock
StopWriter ==
    /\ proc = "new" /\ retire # 0 /\ Exact /\ writer \in {"starting", "ready"}
    /\ writer' = "stopped"
    /\ UNCHANGED <<proc, relation, cand, floor, init, tomb, tombGen, gen, input, knock,
                   arrived, oSent, lSent, via, row, retire, deploy, booted, crashes, Fixed,
                   wmode, lSending, lateTurn, oTurn>>
Retire ==
    /\ proc = "new" /\ retire # 0
    /\ \/ /\ Mut = "lateTomb" \/ (Exact /\ writer \in {"none", "stopped"})
          /\ tomb' = TRUE /\ tombGen' = retire /\ retire' = 0
       \/ /\ Mut # "lateTomb" /\ ~Exact
          /\ retire' = 0 /\ UNCHANGED <<tomb, tombGen>>
    /\ UNCHANGED <<proc, relation, cand, floor, init, writer, gen, input, knock, arrived,
                   oSent, lSent, via, row, deploy, booted, crashes, Fixed, wmode, lSending,
                   lateTurn, oTurn>>

Next == Arrive \/ Admit \/ EarlyPlace \/ Resolve \/ LegacyClaim \/ LegacyDone \/ StartWriter
        \/ PublishFloor
        \/ PublishInit \/ InitFail \/ HeldVisible \/ Ready \/ Post \/ Crash \/ Boot
        \/ DeployCheck \/ DeployStop \/ DeploySwap \/ GcDelete \/ StopWriter \/ Retire
        \/ UNCHANGED vars
Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Admit) /\ WF_vars(Resolve) /\ WF_vars(LegacyClaim) /\ WF_vars(LegacyDone)
        /\ WF_vars(StartWriter)
        /\ WF_vars(PublishFloor) /\ WF_vars(PublishInit) /\ WF_vars(Ready) /\ WF_vars(Post)
        /\ WF_vars(HeldVisible) /\ WF_vars(Boot) /\ WF_vars(Retire) /\ WF_vars(DeploySwap)

\* An O parent's child body is O's except the late reservation's turn before the handoff.
NoLegacyBody == lSent => (~parentO \/ (lateTurn /\ ~oTurn))
ExactlyOnce == ~(oSent /\ lSent)
OwnAuthority == cand # "parent" /\ via # "parent"
FloorBeforeInit == init => floor
ReadyNeedsInit == writer = "ready" => (init /\ cand = "committed")
OldReaderBoots == booted = "ok"
TombExact == tomb => tombGen = gen
\* An input held at placement for an unresolved child is never dropped.
NoSilentLoss == input # "lost"
InheritDormant == writer \in {"starting", "ready"} => ((wmode = "dormant") = dormant)
\* An admitted child input is posted by its own ready writer or fails visibly.
ChildInputAnswered == [](input = "waiting" => <>(input # "waiting" \/ proc = "old"))
\* An input held at placement is resolved while its process runs.
HeldResolves == [](relation = "unresolved" => <>(relation # "unresolved" \/ proc # "new"))
=============================================================================
