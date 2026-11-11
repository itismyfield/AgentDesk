---- MODULE HerdrPendingRegistration ----
\* Specializes HerdrSession's durable Pending/evidence, Crash, Boot, Send and Bind for Codex.
EXTENDS HerdrSession
CONSTANTS RestoreRegistration, InitialRecord, Scenario
VARIABLES registry, marker, phase, sends, startupSeen, held, otherInstalled,
          restores, restoredPending
extra == <<registry, marker, phase, sends, startupSeen, held, otherInstalled,
           restores, restoredPending>>
reuseVars == <<vars, extra>>

\* evidenced is still HostedState::Pending with expected evidence (herdr_launch.rs:502-514).
ReuseInit ==
    /\ srv = "up" /\ rec = InitialRecord /\ pane = IF rec = "pending" THEN "none" ELSE "agent"
    /\ fresh = FALSE /\ stray = 0 /\ launch = "idle" /\ input = "idle"
    /\ landed = "none" /\ closing = FALSE /\ crashes = 0 /\ lost = 0 /\ exits = 0
    /\ registry = 1 /\ marker = 1 /\ phase = "beforeRestart" /\ sends = 0
    /\ startupSeen = FALSE /\ held = FALSE /\ otherInstalled = FALSE
    /\ restores = 0 /\ restoredPending = TRUE

\* The row, pane and marker survive; only the process registry is lost.
Restart ==
    /\ phase = "beforeRestart" /\ Crash
    /\ registry' = 0 /\ phase' = "afterCrash"
    /\ UNCHANGED <<marker, sends, startupSeen, held, otherInstalled, restores, restoredPending>>
RestartBoot ==
    /\ phase = "afterCrash" /\ Boot /\ phase' = "waiting"
    /\ UNCHANGED <<registry, marker, sends, startupSeen, held, otherInstalled,
                    restores, restoredPending>>

\* Exact unplaced resolution records the existing pane/evidence; no new provider is created.
ResolveUnplaced ==
    /\ phase = "waiting" /\ srv = "up" /\ rec = "pending"
    /\ rec' = "evidenced" /\ pane' = "agent"
    /\ UNCHANGED <<srv, fresh, stray, launch, input, landed, closing, crashes, lost, exits, extra>>
ResolveEvidence ==
    /\ phase = "waiting" /\ ProbeAgain /\ UNCHANGED extra

\* A competing registration, marker replacement or row change precedes this input's check.
InstallOther ==
    /\ phase = "waiting" /\ Scenario = "other" /\ ~otherInstalled
    /\ registry' = 2 /\ otherInstalled' = TRUE
    /\ UNCHANGED <<vars, marker, phase, sends, startupSeen, held, restores, restoredPending>>
ReplaceMarker ==
    /\ phase = "waiting" /\ Scenario = "marker" /\ marker = 1 /\ marker' = 2
    /\ UNCHANGED <<vars, registry, phase, sends, startupSeen, held, otherInstalled,
                    restores, restoredPending>>
ReplaceRow ==
    /\ phase = "waiting" /\ Scenario = "bound" /\ rec = "evidenced" /\ rec' = "bound"
    /\ UNCHANGED <<srv, pane, fresh, stray, launch, input, landed, closing,
                    crashes, lost, exits, extra>>

ReadyToCheck ==
    /\ phase = "waiting" /\ srv = "up" /\ rec \in {"evidenced", "bound"}
    /\ (Scenario = "other" => otherInstalled)
    /\ (Scenario = "marker" => marker = 2)
    /\ (Scenario = "bound" => rec = "bound")
ValidPending == rec = "evidenced" /\ marker = 1 /\ registry \in {0, 1}

\* The source authority makes check-and-fill one action; an existing different nonce is refused.
PrepareInput ==
    /\ ReadyToCheck
    /\ LET allowed == ~RestoreRegistration \/ ValidPending IN
       /\ phase' = IF allowed THEN "ready" ELSE "refused"
       /\ input' = IF allowed THEN "pinned" ELSE "idle"
       /\ registry' = IF RestoreRegistration /\ ValidPending THEN 1 ELSE registry
       /\ restores' = restores + IF RestoreRegistration /\ ValidPending /\ registry = 0 THEN 1 ELSE 0
       /\ restoredPending' = IF RestoreRegistration /\ ValidPending /\ registry = 0
                             THEN rec = "evidenced" ELSE restoredPending
    /\ UNCHANGED <<srv, rec, pane, fresh, stray, launch, landed, closing, crashes, lost, exits,
                    marker, sends, startupSeen, held, otherInstalled>>
FirstInput ==
    /\ phase = "ready" /\ Send /\ phase' = "awaitStartup"
    /\ sends' = sends + 1 /\ held' = TRUE
    /\ UNCHANGED <<registry, marker, startupSeen, otherInstalled, restores, restoredPending>>

\* A delivered native-UUID Startup reaches record() only for the registered execution.
Startup ==
    /\ phase = "awaitStartup" /\ ~startupSeen
    /\ startupSeen' = TRUE /\ phase' = IF registry = 1 THEN "recorded" ELSE "held"
    /\ UNCHANGED <<vars, registry, marker, sends, held, otherInstalled, restores, restoredPending>>
Finish ==
    /\ phase = "recorded" /\ Bind /\ phase' = "done" /\ held' = FALSE
    /\ UNCHANGED <<registry, marker, sends, startupSeen, otherInstalled, restores, restoredPending>>
RetryHeld == phase = "held" /\ held /\ UNCHANGED reuseVars
ReuseNext == Restart \/ RestartBoot \/ ResolveUnplaced \/ ResolveEvidence
             \/ InstallOther \/ ReplaceMarker \/ ReplaceRow \/ PrepareInput \/ FirstInput
             \/ Startup \/ Finish \/ RetryHeld
ReuseSpec == ReuseInit /\ [][ReuseNext]_reuseVars
             /\ WF_reuseVars(Restart) /\ WF_reuseVars(RestartBoot)
             /\ WF_reuseVars(ResolveUnplaced) /\ WF_reuseVars(ResolveEvidence)
             /\ WF_reuseVars(InstallOther) /\ WF_reuseVars(ReplaceMarker) /\ WF_reuseVars(ReplaceRow)
             /\ WF_reuseVars(PrepareInput) /\ WF_reuseVars(FirstInput)
             /\ WF_reuseVars(Startup) /\ WF_reuseVars(Finish)
PendingReuseBound == (phase = "ready") ~> (rec = "bound")
NoOtherNonceOverwrite == otherInstalled => registry = 2
NoDuplicateInput == sends <= 1
RestoreOnlyPending == restoredPending
InvalidPendingWritesNothing == Scenario \in {"other", "marker", "bound"} => sends = 0
====
