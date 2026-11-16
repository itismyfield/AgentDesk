-------------------- MODULE WriterAdoptionThreadComposition --------------------
EXTENDS WriterThreadActive
CONSTANT AdoptionBoundary
VARIABLES memberAdoption, memberLegacy, memberO
memberVars == <<memberAdoption, memberLegacy, memberO>>
allVars == <<vars, memberVars>>

\* The configured member uses the b2 adoption gate, independently of the inherited child.
\* Values: channel_gate::adopted (Deferred/Refused/Committed); child values/actions are
\* the actual main WriterThreadActive model, including boot, placement and handoff.
MemberInit == /\ memberAdoption \in {"Deferred", "Refused", "Committed"}
              /\ memberLegacy = 0 /\ memberO = 0
MemberCommit == /\ memberAdoption = "Deferred" /\ memberAdoption' = "Committed"
                /\ UNCHANGED <<memberLegacy, memberO, vars>>
MemberPost == /\ memberAdoption = "Committed" /\ memberO = 0
              /\ memberO' = 1 /\ UNCHANGED <<memberAdoption, memberLegacy, vars>>
MemberLeak == /\ AdoptionBoundary = "leak" /\ memberAdoption # "Committed"
              /\ memberLegacy = 0 /\ memberLegacy' = 1
              /\ UNCHANGED <<memberAdoption, memberO, vars>>
ChildNext == Next /\ UNCHANGED memberVars
CompositionNext == ChildNext \/ MemberCommit \/ MemberPost \/ MemberLeak
CompositionSpec == /\ Init /\ MemberInit /\ [][CompositionNext]_allVars
                   /\ WF_allVars(MemberCommit) /\ WF_allVars(MemberPost)
                   /\ WF_allVars(Admit /\ UNCHANGED memberVars)
                   /\ WF_allVars(Resolve /\ UNCHANGED memberVars)
                   /\ WF_allVars(LegacyClaim /\ UNCHANGED memberVars)
                   /\ WF_allVars(LegacyDone /\ UNCHANGED memberVars)
                   /\ WF_allVars(StartWriter /\ UNCHANGED memberVars)
                   /\ WF_allVars(PublishFloor /\ UNCHANGED memberVars)
                   /\ WF_allVars(PublishInit /\ UNCHANGED memberVars)
                   /\ WF_allVars(Ready /\ UNCHANGED memberVars)
                   /\ WF_allVars(Post /\ UNCHANGED memberVars)
                   /\ WF_allVars(HeldVisible /\ UNCHANGED memberVars)
                   /\ WF_allVars(Boot /\ UNCHANGED memberVars)
                   /\ WF_allVars(Retire /\ UNCHANGED memberVars)
                   /\ WF_allVars(DeploySwap /\ UNCHANGED memberVars)
ConfiguredLegacyZero == memberLegacy = 0 /\ (bound => ~lSent)
\* Before exposes the incompatible universal reading; after admits only main's late turn.
NoLegacyBodyOnTui == memberLegacy = 0 /\
                    IF AdoptionBoundary = "before" THEN (parentO => ~lSent) ELSE NoLegacyBody
MemberUnadoptedNotOwned == memberO = 1 => memberAdoption = "Committed"
MemberEventuallyAnswered == memberAdoption # "Refused" ~> memberO = 1
=============================================================================
