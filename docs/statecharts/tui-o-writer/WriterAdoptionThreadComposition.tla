-------------------- MODULE WriterAdoptionThreadComposition --------------------
EXTENDS WriterThreadActive
CONSTANT AdoptionBoundary
VARIABLES memberAdoption, memberLegacy, memberO, recoveryAdoption, recoveryStore, recoveryTried, recoveryLegacy, recoveryBodySeen
memberVars == <<memberAdoption, memberLegacy, memberO>>
recoveryVars == <<recoveryAdoption, recoveryStore, recoveryTried, recoveryLegacy, recoveryBodySeen>>
allVars == <<vars, memberVars, recoveryVars>>
RecoveryModel == AdoptionBoundary \in {"recoverReleased", "recoverVerified", "recoverUnchecked", "recoverLegacy"}
RecoveryInit == /\ recoveryAdoption = IF AdoptionBoundary = "recoverReleased" THEN "Released" ELSE "Refused"
                /\ recoveryStore \in IF RecoveryModel
                     THEN {"verified", "otherKind", "otherParent", "noAuthority"} ELSE {"none"}
                /\ recoveryTried = ~RecoveryModel /\ recoveryLegacy = 0 /\ recoveryBodySeen = 0
\* Host recovery has a readable store. The current caller is authorized separately; recovery
\* must additionally match the child's checked parent and runtime before confirm_store.
RecoveryRead == /\ ~recoveryTried /\ recoveryBodySeen = 1 /\ recoveryTried' = TRUE
                /\ recoveryAdoption' = IF recoveryAdoption = "Refused" /\
                      (recoveryStore = "verified" \/
                       (AdoptionBoundary = "recoverUnchecked" /\ recoveryStore # "noAuthority"))
                     THEN "Committed" ELSE recoveryAdoption
                /\ UNCHANGED <<recoveryStore, recoveryLegacy, recoveryBodySeen, vars, memberVars>>
\* Read the real child's body gate both during refusal and after recovery. A refused known
\* child is held; a committed one is O's. Neither gives Legacy a body.
RecoveryBody == /\ RecoveryModel /\ recoveryBodySeen < 2
                /\ (recoveryBodySeen = 0 \/ recoveryTried)
                /\ recoveryBodySeen' = recoveryBodySeen + 1
                /\ recoveryLegacy' = IF AdoptionBoundary = "recoverLegacy" /\ recoveryAdoption = "Refused"
                     THEN recoveryLegacy + 1 ELSE recoveryLegacy
                /\ UNCHANGED <<recoveryAdoption, recoveryStore, recoveryTried, vars, memberVars>>
RecoveryVerifiedOnly == recoveryAdoption = "Committed" => recoveryStore = "verified"
RecoveryNoLegacy == recoveryLegacy = 0
VerifiedRecoveryEventuallyCommits == RecoveryModel /\ recoveryStore = "verified" ~> recoveryAdoption = "Committed"


\* The configured member uses the b2 adoption gate, independently of the inherited child.
\* Values: channel_gate::adopted (Deferred/Refused/Committed); child values/actions are
\* the actual main WriterThreadActive model, including boot, placement and handoff.
MemberInit == /\ memberAdoption \in {"Deferred", "Refused", "Committed"}
              /\ memberLegacy = 0 /\ memberO = 0
MemberCommit == /\ memberAdoption = "Deferred" /\ memberAdoption' = "Committed"
                /\ UNCHANGED <<memberLegacy, memberO, vars, recoveryVars>>
MemberPost == /\ memberAdoption = "Committed" /\ memberO = 0
              /\ memberO' = 1 /\ UNCHANGED <<memberAdoption, memberLegacy, vars, recoveryVars>>
MemberLeak == /\ AdoptionBoundary = "leak" /\ memberAdoption # "Committed"
              /\ memberLegacy = 0 /\ memberLegacy' = 1
              /\ UNCHANGED <<memberAdoption, memberO, vars, recoveryVars>>
ChildNext == Next /\ UNCHANGED <<memberVars, recoveryVars>>
CompositionNext == ChildNext \/ MemberCommit \/ MemberPost \/ MemberLeak \/ RecoveryRead \/ RecoveryBody
CompositionSpec == /\ Init /\ MemberInit /\ RecoveryInit /\ [][CompositionNext]_allVars
                   /\ WF_allVars(MemberCommit) /\ WF_allVars(MemberPost) /\ WF_allVars(RecoveryRead) /\ WF_allVars(RecoveryBody)
                   /\ WF_allVars(Admit /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(Resolve /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(LegacyClaim /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(LegacyDone /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(StartWriter /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(PublishFloor /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(PublishInit /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(Ready /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(Post /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(HeldVisible /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(Boot /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(Retire /\ UNCHANGED <<memberVars, recoveryVars>>)
                   /\ WF_allVars(DeploySwap /\ UNCHANGED <<memberVars, recoveryVars>>)
ConfiguredLegacyZero == memberLegacy = 0 /\ (bound => ~lSent)
\* Before exposes the incompatible universal reading; after admits only main's late turn.
NoLegacyBodyOnTui == memberLegacy = 0 /\
                    IF AdoptionBoundary = "before" THEN (parentO => ~lSent) ELSE NoLegacyBody
MemberUnadoptedNotOwned == memberO = 1 => memberAdoption = "Committed"
MemberEventuallyAnswered == memberAdoption # "Refused" ~> memberO = 1
=============================================================================
