-------------------------- MODULE TurnPresence --------------------------
EXTENDS Naturals, TLC
CONSTANTS AdmissionMode, FinalMode, Grace
VARIABLES running, process, phase, identity, progress, epoch, row, changed, bumped, before, after,
          pinned, retry, accepted, busy, unknownTicks, notice, published,
          wrongAdmission, wrongPublication, publishedInput, absence
vars == <<running, process, phase, identity, progress, epoch, row, changed, bumped, before, after,
          pinned, retry, accepted, busy, unknownTicks, notice, published,
          wrongAdmission, wrongPublication, publishedInput, absence>>
\* Identity 0 is the original; 1 changes session, 2..6 owner/nonce/turn/source/core.
\* Identity never wraps while an old proof exists; progress may cycle forever.
Snapshot == [boot |-> process, id |-> identity, tick |-> progress, ownerEpoch |-> epoch, row |-> row]
HealthyOpen == running /\ row = "present"
Same(a, b, mode) ==
    /\ a.row = "present" /\ b.row = "present"
    /\ a.boot = b.boot /\ a.ownerEpoch = b.ownerEpoch
    /\ IF mode = "name" THEN (a.id = 1) = (b.id = 1) ELSE a.id = b.id
    /\ (mode # "strict" \/ a.tick = b.tick)
Init ==
    /\ running = TRUE /\ process = 0
    /\ phase = "begin" /\ identity = 0 /\ progress = 0 /\ epoch = 0
    /\ row = "present" /\ changed = FALSE /\ bumped = FALSE
    /\ before = Snapshot /\ after = Snapshot /\ pinned = Snapshot
    /\ retry = 0 /\ accepted = FALSE /\ busy = FALSE
    /\ unknownTicks = 0 /\ notice = FALSE /\ published = FALSE
    /\ wrongAdmission = FALSE /\ wrongPublication = FALSE /\ publishedInput = Snapshot /\ absence = FALSE
Progress == /\ progress' = 1 - progress
            /\ UNCHANGED <<phase, identity, epoch, row, changed, bumped, before, after,
                 pinned, retry, accepted, busy, unknownTicks, notice, published,
                 wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

ChangeIdentity ==
    /\ ~changed /\ identity' \in 1..6 /\ changed' = TRUE
    /\ UNCHANGED <<phase, progress, epoch, row, bumped, before, after, pinned, retry,
         accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

UnrelatedEpochBump ==
    /\ ~bumped /\ epoch' = 1 /\ bumped' = TRUE
    /\ UNCHANGED <<phase, identity, progress, row, changed, before, after, pinned, retry,
         accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

ReadFail ==
    /\ row = "present" /\ ~changed /\ changed' = TRUE /\ row' \in {"unreadable", "missing"}
    /\ UNCHANGED <<phase, identity, progress, epoch, bumped, before, after,
         pinned, retry, accepted, busy, unknownTicks, notice, published,
         wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

ReadRecover ==
    /\ phase = "begin" /\ row # "present" /\ row' = "present"
    /\ UNCHANGED <<phase, identity, progress, epoch, changed, bumped, before, after,
         pinned, retry, accepted, busy, unknownTicks, notice, published,
         wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

Begin ==
    /\ phase = "begin"
    /\ phase' = ("collect-before")
    /\ retry' = (0)
    /\ published' = (FALSE)
    /\ absence' = FALSE
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, accepted, busy, unknownTicks, notice, wrongAdmission, wrongPublication>>
    /\ UNCHANGED publishedInput
    /\ UNCHANGED <<running, process>>

CollectBefore ==
    /\ phase = "collect-before"
    /\ before' = (Snapshot)
    /\ phase' = ("collect-after")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, after, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

CollectAfter ==
    /\ phase = "collect-after"
    /\ after' = (Snapshot)
    /\ phase' = ("admit")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

Retry ==
    /\ phase = "admit" /\ ~Same(before, after, AdmissionMode) /\ retry = 0 /\ before.row = "present" /\ after.row = "present"
    /\ phase' = ("collect-before")
    /\ retry' = (1)
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

Admit ==
    /\ phase = "admit" /\ Same(before, after, AdmissionMode)
    /\ accepted' = (TRUE)
    /\ pinned' = (before)
    /\ phase' = ("read-open")
    /\ wrongAdmission' = (wrongAdmission \/ before.id # after.id)
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, retry, busy, unknownTicks, notice, published, wrongPublication, absence>>
    /\ UNCHANGED publishedInput
    /\ UNCHANGED <<running, process>>

RejectAdmission ==
    /\ phase = "admit" /\ ~Same(before, after, AdmissionMode) /\ (retry = 1 \/ before.row # "present" \/ after.row # "present")
    /\ accepted' = (FALSE)
    /\ phase' = ("unbound-before")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

CollectUnboundBefore ==
    /\ phase = "unbound-before"
    /\ before' = (Snapshot)
    /\ phase' = ("unbound-after")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, after, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

CollectUnboundAfter ==
    /\ phase = "unbound-after"
    /\ after' = (Snapshot)
    /\ phase' = ("unbound")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

ObserveUnknown ==
    /\ phase = "unbound"
    /\ busy' = (FALSE)
    /\ unknownTicks' = (IF unknownTicks < Grace THEN unknownTicks + 1 ELSE Grace)
    /\ notice' = (unknownTicks + 1 >= Grace)
    /\ absence' = (before.row = "missing" /\ after.row = "missing" /\ row = "missing")
    /\ phase' = ("begin")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, accepted, published, wrongAdmission, wrongPublication>>
    /\ UNCHANGED publishedInput
    /\ UNCHANGED <<running, process>>

ReadOpen ==
    /\ phase = "read-open"
    /\ busy' = (TRUE)
    /\ unknownTicks' = (0)
    /\ notice' = (FALSE)
    /\ absence' = (FALSE)
    /\ phase' = ("publication-before")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, accepted, published, wrongAdmission, wrongPublication>>
    /\ UNCHANGED publishedInput
    /\ UNCHANGED <<running, process>>

PublicationBefore ==
    /\ phase = "publication-before"
    /\ before' = (Snapshot)
    /\ phase' = ("publication-after")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, after, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

PublicationAfter ==
    /\ phase = "publication-after"
    /\ after' = (Snapshot)
    /\ phase' = ("publication-pair")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

PublicationPair ==
    /\ phase = "publication-pair"
    /\ phase' = (IF Same(before, after, IF AdmissionMode = "name" THEN "name" ELSE "projection") /\ Same(pinned, after, IF AdmissionMode = "name" THEN "name" ELSE "projection") THEN "publication-final" ELSE "begin")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, accepted, busy, unknownTicks, notice, published, wrongAdmission, wrongPublication, publishedInput, absence>>
    /\ UNCHANGED <<running, process>>

FinalCheckAndPublish ==
    /\ phase = "publication-final"
    /\ publishedInput' = Snapshot
    /\ published' = (Same(pinned, Snapshot, FinalMode))
    /\ wrongPublication' = (wrongPublication \/ (Same(pinned, Snapshot, FinalMode) /\ pinned.id # identity))
    /\ phase' = ("begin")
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after, pinned, retry, accepted, busy, unknownTicks, notice, wrongAdmission, absence>>
    /\ UNCHANGED <<running, process>>

\* Restart discards the in-memory reading; a new process collects the surviving row again.
Crash ==
    /\ running /\ ~changed
    /\ running' = FALSE /\ changed' = TRUE /\ phase' = "down"
    /\ accepted' = FALSE /\ busy' = FALSE /\ published' = FALSE /\ absence' = FALSE
    /\ unknownTicks' = 0 /\ notice' = FALSE
    /\ UNCHANGED <<process, identity, progress, epoch, row, bumped, before, after,
         pinned, retry, wrongAdmission, wrongPublication, publishedInput>>
Boot ==
    /\ ~running /\ phase = "down"
    /\ running' = TRUE /\ process' = 1 /\ phase' = "begin"
    /\ UNCHANGED <<identity, progress, epoch, row, changed, bumped, before, after,
         pinned, retry, accepted, busy, unknownTicks, notice, published,
         wrongAdmission, wrongPublication, publishedInput, absence>>

Work == Boot \/ Begin \/ CollectBefore \/ CollectAfter \/ Retry \/ Admit \/ RejectAdmission
        \/ CollectUnboundBefore \/ CollectUnboundAfter \/ ObserveUnknown \/ ReadOpen
        \/ PublicationBefore \/ PublicationAfter \/ PublicationPair \/ FinalCheckAndPublish
Next == Work \/ Crash \/ Progress \/ ChangeIdentity \/ UnrelatedEpochBump \/ ReadFail \/ ReadRecover
Spec == Init /\ [][Next]_vars /\ WF_vars(Work)
NoWrongAdmission == ~wrongAdmission
NoWrongPublication == ~wrongPublication
NoOldProofAfterObservedBoundary == published =>
    publishedInput.id = pinned.id /\ publishedInput.boot = pinned.boot /\ publishedInput.row = "present" /\ publishedInput.ownerEpoch = pinned.ownerEpoch
\* Check only the original healthy epoch: true boundary/read faults retain ordinary Unknown.
NoProgressIdentityUnknown == (identity = 0 /\ row = "present" /\ ~bumped /\ ~changed) => unknownTicks = 0
NoProgressIdentityNotice == (identity = 0 /\ row = "present" /\ ~bumped /\ ~changed) => ~notice
NoAbsenceFromPresentOrUnreadRow == absence => before.row = "missing" /\ after.row = "missing"
EventuallyStableBusy == (<>[] HealthyOpen) => <>[] (accepted /\ busy /\ ~notice)
PublicationContinues == (<>[] HealthyOpen) => []<> published
=============================================================================
