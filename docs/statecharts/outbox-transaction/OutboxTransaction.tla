------------------------ MODULE OutboxTransaction ------------------------
\* Code SHA: 5c3ed37cddc80036fa8a274419b0219f49926704.
\* Each observation has its own linearization point. A later config change
\* does not invalidate an earlier decision; census is captured AFTER INSERT.
EXTENDS Naturals
CONSTANTS Boundary, Mutation, Cases
ASSUME Boundary \in {"current", "service"}
ASSUME Mutation \in {"none", "preobserve", "pre-source", "split-insert", "early-census", "fresh-task", "foreign-tx", "foreign-provider-connection"}
ASSUME Cases \subseteq {"completion", "no-target", "no-entry", "lifecycle",
                       "headless", "exact", "dynamic", "padded", "replay", "circuit"}
ASSUME Cases # {}
VARIABLE s
vars == <<s>>
Completion == s.case \in {"completion", "no-target", "no-entry"}
NeedsNotice == s.case \notin {"no-target", "no-entry"}
HasDisplay == s.case \in {"completion", "lifecycle", "exact"}
HasCensus == s.case \notin {"replay", "circuit"}
Headless == s.case = "headless"
StaticSource == s.case # "dynamic"
NextAfterValidation == IF Completion \/ s.case = "replay" THEN "provider-route"
                       ELSE IF s.case = "exact" THEN "provider-route" ELSE "begin"
NextAfterProvider == IF s.case = "exact" /\ ~s.active THEN "begin" ELSE "insert-route"

Init == \E c \in Cases, tag \in {"absent", "body"}, available \in BOOLEAN:
    s = [pc |-> "start", case |-> c, taskTag |-> tag,
         known |-> TRUE, default |-> "claude", member |-> FALSE, host |-> "absent",
         snapshotAvailable |-> available, bindingsTx |-> 0, bindingsConnection |-> 0,
         providerTargetTx |-> 0, providerTargetConnection |-> 0, bindingsRead |-> FALSE,
         expectedProviderTx |-> 0, expectedProviderConnection |-> 0,
         insertTargetTx |-> 0, insertTargetConnection |-> 0, insertedConnection |-> 0,
         prepared |-> "none", handoffTx |-> 0, executor |-> "db",
         drift |-> "none", restarts |-> 0, active |-> FALSE, ownerTx |-> 0, ownerConnection |-> 0,
         completePending |-> FALSE, slotPending |-> FALSE, noticePending |-> 0,
         completeDurable |-> FALSE, slotDurable |-> FALSE, noticeDurable |-> 0,
         knownUsed |-> FALSE, knownRef |-> FALSE, allowed |-> FALSE,
         default1 |-> "unread", default2 |-> "unread",
         defaultRef1 |-> "unread", defaultRef2 |-> "unread",
         earlyDefault |-> "unread", earlyMember |-> FALSE, earlySnapshot |-> FALSE,
         memberAtCensus |-> FALSE, censusUsed |-> "unread", censusRef |-> "unread",
         producerUsed |-> <<0, "absent">>, producerRef |-> <<0, "absent">>,
         censusStored |-> FALSE, noticeHint |-> "none", insertedTx |-> 0, returned |-> "none"]

Start ==
    /\ s.pc = "start"
    /\ s' = [s EXCEPT !.pc = (IF Completion \/ s.case = "replay" THEN "begin" ELSE "validate"),
             !.earlyDefault = s.default, !.earlyMember = s.member,
             !.earlySnapshot = s.snapshotAvailable]
Begin ==
    /\ s.pc = "begin"
    /\ s' = [s EXCEPT !.active = TRUE, !.ownerTx = s.restarts + 1, !.ownerConnection = s.restarts + 1,
             !.pc = IF Completion THEN "complete"
                    ELSE IF s.case = "replay" THEN "validate"
                    ELSE IF s.case = "exact" THEN "insert-route"
                    ELSE IF s.case = "dynamic" THEN "validate-inner" ELSE "provider-route"]
Complete ==
    /\ s.pc = "complete"
    /\ s' = [s EXCEPT !.pc = IF s.completeDurable THEN "rollback"
                             ELSE IF NeedsNotice THEN "release" ELSE "silent",
             !.completePending = ~s.completeDurable]
ReleaseSlot ==
    /\ s.pc \in {"release", "silent"}
    /\ s' = [s EXCEPT !.slotPending = TRUE,
             !.pc = IF s.pc = "silent" THEN "commit"
                    ELSE IF Boundary = "service" THEN "handoff" ELSE "validate"]
\* Proposed DB preparation returns the still-borrowed transaction to its
\* service driver; it never calls a service observer or commits before return.
ReturnPreparation ==
    /\ s.pc = "handoff" /\ s.active
    /\ s' = [s EXCEPT !.pc = "validate", !.prepared = "borrowed",
             !.handoffTx = s.ownerTx, !.executor = "service"]
Validate ==
    /\ s.pc = "validate"
    /\ s' = [s EXCEPT !.knownRef = s.known, !.knownUsed = s.known,
             !.allowed = (s.case # "padded" /\ (StaticSource \/ s.known)),
             !.pc = IF s.case = "padded" \/ (~StaticSource /\ ~s.known)
                    THEN "rollback" ELSE NextAfterValidation]
\* TTL/persistent pool wrappers validate again in their on-transaction core.
ValidateInner ==
    /\ s.pc = "validate-inner"
    /\ LET used == IF Mutation = "pre-source" THEN s.knownUsed ELSE s.known
       IN s' = [s EXCEPT !.knownRef = s.known, !.knownUsed = used,
                !.allowed = used, !.pc = IF used THEN "provider-route" ELSE "rollback"]
RouteProvider ==
    /\ s.pc = "provider-route"
    /\ LET tx == IF s.case = "exact" THEN 0 ELSE s.ownerTx
           connection == IF s.case = "exact" THEN 3 ELSE s.ownerConnection
       IN s' = [s EXCEPT !.expectedProviderTx = tx, !.expectedProviderConnection = connection,
                 !.providerTargetTx = tx,
                 !.providerTargetConnection = IF Mutation = "foreign-provider-connection" THEN 9 ELSE connection,
                 !.pc = IF HasDisplay THEN "bindings" ELSE NextAfterProvider]
LoadBindings ==
    /\ s.pc = "bindings"
    /\ s' = [s EXCEPT !.bindingsTx = s.providerTargetTx,
             !.bindingsConnection = s.providerTargetConnection, !.bindingsRead = TRUE,
             !.pc = "provider1"]
BindingQueryError ==
    /\ s.pc = "bindings" /\ HasDisplay
    /\ s' = [s EXCEPT !.pc = "rollback"]
\* Two matching legacy-primary rows can consult the live default separately.
ObserveProvider1 ==
    /\ s.pc = "provider1"
    /\ s' = [s EXCEPT !.defaultRef1 = IF HasDisplay THEN s.default ELSE "unread",
             !.default1 = IF HasDisplay THEN
                             (IF Mutation = "preobserve" THEN s.earlyDefault ELSE s.default)
                          ELSE "unread",
             !.pc = "provider2"]
ObserveProvider2 ==
    /\ s.pc = "provider2"
    /\ s' = [s EXCEPT !.defaultRef2 = IF HasDisplay THEN s.default ELSE "unread",
             !.default2 = IF HasDisplay THEN
                             (IF Mutation = "preobserve" THEN s.earlyDefault ELSE s.default)
                          ELSE "unread",
             !.pc = NextAfterProvider]
RouteInsert ==
    /\ s.pc = "insert-route"
    /\ s' = [s EXCEPT !.insertTargetTx = IF Mutation \in {"foreign-tx", "split-insert"} THEN 9 ELSE s.ownerTx,
             !.insertTargetConnection = IF Mutation \in {"foreign-tx", "split-insert"} THEN 9 ELSE s.ownerConnection,
             !.pc = "insert"]
Insert ==
    /\ s.pc = "insert" /\ s.active /\ s.allowed
    /\ s' = [s EXCEPT !.noticePending = 1, !.insertedTx = s.insertTargetTx, !.insertedConnection = s.insertTargetConnection,
             !.noticeHint = IF HasDisplay /\ s.default1 = s.default2 THEN s.default1 ELSE "none",
             !.noticeDurable = IF Mutation = "split-insert" THEN @ + 1 ELSE @,
             !.pc = IF HasCensus THEN "census" ELSE "commit"]
InsertError ==
    /\ s.pc = "insert"
    /\ s' = [s EXCEPT !.pc = "rollback"]
ObserveCensus ==
    /\ s.pc = "census"
    /\ LET member == IF Mutation = "early-census" THEN s.earlyMember ELSE s.member
           available == IF Mutation = "early-census" THEN s.earlySnapshot ELSE s.snapshotAvailable
           expected == IF Headless THEN s.taskTag
                       ELSE IF ~StaticSource \/ ~s.snapshotAvailable \/ s.member THEN "absent" ELSE "notice"
           used == IF Headless THEN (IF Mutation = "fresh-task" THEN "absent" ELSE s.taskTag)
                   ELSE IF ~StaticSource \/ ~available \/ member THEN "absent" ELSE "notice"
       IN s' = [s EXCEPT !.memberAtCensus = s.member, !.censusRef = expected,
                 !.censusUsed = used, !.pc = IF used = "absent" THEN "commit" ELSE "producer"]
ObserveProducer ==
    /\ s.pc = "producer"
    /\ s' = [s EXCEPT !.producerRef = <<s.restarts, s.host>>,
             !.producerUsed = <<s.restarts, s.host>>, !.pc = "savepoint"]
SavepointBegin ==
    /\ s.pc = "savepoint"
    /\ s' = [s EXCEPT !.pc = "metadata"]
SavepointBeginError ==
    /\ s.pc = "savepoint"
    /\ s' = [s EXCEPT !.pc = "rollback"]
\* UPDATE failure with successful ROLLBACK TO SAVEPOINT is optional metadata.
Metadata ==
    /\ s.pc = "metadata"
    /\ \E stored \in BOOLEAN:
         s' = [s EXCEPT !.censusStored = stored, !.pc = "commit"]
\* Failed rollback/release may poison the outer transaction; no success promise.
MetadataOuterError ==
    /\ s.pc = "metadata"
    /\ s' = [s EXCEPT !.pc = "rollback"]
Commit ==
    /\ s.pc = "commit" /\ s.active
    /\ s' = [s EXCEPT !.active = FALSE,
             !.completeDurable = s.completeDurable \/ s.completePending,
             !.slotDurable = s.slotDurable \/ s.slotPending,
             !.noticeDurable = IF Mutation = "split-insert" THEN @ ELSE @ + s.noticePending,
             !.prepared = IF s.prepared = "borrowed" THEN "spent" ELSE @,
             !.returned = "commit", !.pc = "done"]
Rollback ==
    /\ s.pc = "rollback"
    /\ s' = [s EXCEPT !.active = FALSE, !.completePending = FALSE,
             !.slotPending = FALSE, !.noticePending = 0,
             !.prepared = IF s.prepared = "borrowed" THEN "spent" ELSE @,
             !.returned = "rollback", !.pc = "done"]
\* Config is not protected by SQL transaction locks. One interleaving change
\* witnesses stale capture without an unbounded config generation counter.
ChangeSettings ==
    /\ s.pc # "done" /\ s.drift = "none"
    /\ \E field \in {"known", "default", "member", "snapshot", "host"}:
         s' = [s EXCEPT !.known = IF field = "known" THEN FALSE ELSE @,
                !.default = IF field = "default" THEN "codex" ELSE @,
                !.member = IF field = "member" THEN TRUE ELSE @,
                !.snapshotAvailable = IF field = "snapshot" THEN ~@ ELSE @,
                !.host = IF field = "host" THEN "initialized" ELSE @,
                !.drift = field]
\* Before commit the backend rolls back; after commit the terminal run is
\* re-read and does not announce again. Generic caller retry policy is outside.
Restart ==
    /\ Completion /\ s.restarts = 0 /\ s.pc # "start"
    /\ s' = [s EXCEPT !.pc = "start", !.restarts = 1, !.active = FALSE,
             !.completePending = FALSE, !.slotPending = FALSE, !.noticePending = 0,
             !.default1 = "unread", !.default2 = "unread",
             !.defaultRef1 = "unread", !.defaultRef2 = "unread",
             !.censusUsed = "unread", !.censusRef = "unread",
             !.prepared = "none", !.handoffTx = 0, !.executor = "db",
             !.censusStored = FALSE, !.insertedTx = 0, !.bindingsTx = 0, !.bindingsConnection = 0,
             !.providerTargetTx = 0, !.providerTargetConnection = 0, !.bindingsRead = FALSE,
             !.insertTargetTx = 0, !.insertTargetConnection = 0, !.insertedConnection = 0, !.returned = "none"]
Done == /\ s.pc = "done" /\ UNCHANGED vars
Next == Start \/ Begin \/ Complete \/ ReleaseSlot \/ ReturnPreparation \/ Validate \/ ValidateInner
        \/ RouteProvider \/ LoadBindings \/ BindingQueryError \/ ObserveProvider1
        \/ ObserveProvider2 \/ RouteInsert \/ Insert \/ InsertError \/ ObserveCensus \/ ObserveProducer
        \/ SavepointBegin \/ SavepointBeginError \/ Metadata \/ MetadataOuterError
        \/ Commit \/ Rollback \/ ChangeSettings \/ Restart \/ Done
Spec == Init /\ [][Next]_vars

TypeOK == /\ s.noticeDurable \in 0..2 /\ s.noticePending \in 0..1
          /\ s.restarts \in 0..1 /\ s.ownerTx \in 0..2
AtomicCompletion == Completion =>
    /\ (s.noticeDurable > 0 => s.completeDurable /\ s.slotDurable)
    /\ (s.completeDurable => s.slotDurable /\ s.noticeDurable = (IF NeedsNotice THEN 1 ELSE 0))
RollbackHasNoNewNotice == Completion /\ s.returned = "rollback" /\ ~s.completeDurable => s.noticeDurable = 0
CommittedNoticeExactlyOnce == Completion /\ s.completeDurable /\ NeedsNotice => s.noticeDurable = 1
SameTransaction == s.noticePending = 1 =>
                   s.insertedTx = s.ownerTx /\ s.insertedConnection = s.ownerConnection
PreparationKeepsBorrow == s.prepared = "borrowed" =>
                         s.active /\ s.handoffTx = s.ownerTx /\ s.executor = "service"
SameProviderConnection == s.bindingsRead =>
    s.bindingsTx = s.expectedProviderTx /\ s.bindingsConnection = s.expectedProviderConnection
SourceDecisionUsesObservation == /\ s.knownUsed = s.knownRef
                                 /\ (s.allowed => (s.case # "padded" /\ (StaticSource \/ s.knownUsed)))
ProviderObservationPointPreserved == s.default1 = s.defaultRef1 /\ s.default2 = s.defaultRef2
InsertUsesObservedDesignation == s.insertedTx > 0 =>
    s.noticeHint = (IF HasDisplay /\ s.defaultRef1 = s.defaultRef2 THEN s.defaultRef1 ELSE "none")
CensusObservationPointPreserved == s.censusUsed = s.censusRef
ProducerObservationPreserved == s.pc \in {"savepoint", "metadata", "commit", "done"} /\ s.censusUsed \notin {"absent", "unread"}
                               => s.producerUsed = s.producerRef
CensusFollowsInsert == s.censusUsed # "unread" => s.insertedTx > 0
=============================================================================
