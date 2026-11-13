-------------------------- MODULE WriterThreadPrep --------------------------
EXTENDS WriterAdoption
CONSTANT Prep
VARIABLES relation, shared, cloneSees, childCandidate, restCalls, floor, childInit,
          initFailed, rollback, retirement
childVars == <<relation, shared, cloneSees, childCandidate, restCalls, floor,
               childInit, initFailed, rollback, retirement>>
allVars == <<vars, childVars>>

\* Reader/view projection only. Production child membership remains absent in O-a.
ChildInit ==
    /\ Init /\ sel = "default" /\ site = "home" /\ root /\ exec = "none"
    /\ relation = "recorded" /\ shared = FALSE /\ cloneSees = FALSE
    /\ childCandidate = "none" /\ restCalls = 0
    /\ floor = FALSE /\ childInit = FALSE /\ initFailed = FALSE
    /\ rollback = FALSE /\ retirement = "active"

ReadRecord ==
    /\ ~shared
    /\ relation' = "verified" /\ shared' = TRUE
    /\ cloneSees' = (Prep # "unshared")
    /\ childCandidate' = IF Prep = "alias" THEN "parent" ELSE Seed("init")
    /\ restCalls' = IF Prep = "rest" THEN 1 ELSE 0
    /\ UNCHANGED <<vars, floor, childInit, initFailed, rollback, retirement>>

\* The fixture enables the future init machine, never the production child gate.
PublishFloor ==
    /\ ~floor /\ floor' = TRUE
    /\ UNCHANGED <<vars, relation, shared, cloneSees, childCandidate, restCalls,
                   childInit, initFailed, rollback, retirement>>
PublishInit ==
    /\ ~childInit /\ (floor \/ Prep = "reverse") /\ childInit' = TRUE
    /\ UNCHANGED <<vars, relation, shared, cloneSees, childCandidate, restCalls,
                   floor, initFailed, rollback, retirement>>
FailInit ==
    /\ floor /\ ~childInit /\ ~initFailed /\ initFailed' = TRUE
    /\ UNCHANGED <<vars, relation, shared, cloneSees, childCandidate, restCalls,
                   floor, childInit, rollback, retirement>>
RollbackOld ==
    /\ floor /\ Prep = "rollback" /\ rollback' = TRUE
    /\ UNCHANGED <<vars, relation, shared, cloneSees, childCandidate, restCalls,
                   floor, childInit, initFailed, retirement>>
ChildNext == ReadRecord \/ PublishFloor \/ PublishInit \/ FailInit \/ RollbackOld \/ UNCHANGED allVars
ChildSpec == ChildInit /\ [][ChildNext]_allVars /\ WF_allVars(ReadRecord)
SharedSnapshot == shared => cloneSees
OwnCandidate == shared => childCandidate = "Committed"
RecordedRestZero == restCalls = 0
FloorBeforeInit == childInit => floor
FloorSurvivesFailure == initFailed => floor
OldTargetRefused == ~rollback
RetirementIsOnlyRequest == retirement = "active"
RecordEventuallyRead == <>shared
=============================================================================
