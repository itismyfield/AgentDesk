-------------------- MODULE NativeInputTerminal --------------------
EXTENDS NativeInputPreparation
CONSTANTS Scenario, ControlKind
VARIABLE terminal
TerminalVars == <<vars, terminal>>
StableBase == <<phase, role, pin, prepared, anyKey, original, source, restored, admitted, review>>
Interrupt == Scenario \in {"Interrupt", "StoredInterrupt", "StoredBarrierMismatch"}
Partial == Scenario = "Partial"
Raw == ~Interrupt
ScopeOK == terminal.scope = "Exact"
BeforeInterrupt == Mode \in {"BeforeInterrupt", "NoPrecedence"}
BeforeRaw == Mode = "BeforeRaw"
OverridePermitted == Interrupt /\ terminal.proofReady /\ terminal.strict /\ ScopeOK
                     /\ terminal.proofScope = "Exact" /\ Mode # "NoStoredOverride"
                     /\ (terminal.barrierRowMatches \/ Mode = "BareStoredOverride")
EffectiveBarrier == terminal.barrier /\ ~OverridePermitted
NativeMode == terminal.proofKind \in {"Interrupt", "Raw"}
TerminalInit == /\ phase = "Unknown" /\ role = "Primary" /\ pin = "Primary"
                /\ prepared = TRUE /\ anyKey = TRUE /\ original = TRUE
                /\ result = "None" /\ source = "Bound" /\ restored = TRUE
                /\ admitted = FALSE /\ review = [terminalRefinement |-> TRUE]
                /\ terminal = [
                     scope |-> IF Scenario \in {"ForeignNonce", "ForeignSource", "ForeignKey"} THEN Scenario ELSE "Exact",
                     ended |-> Scenario # "Unconfirmed", whole |-> ~Partial,
                     strict |-> Scenario # "Mention" /\ ~Partial,
                     classified |-> FALSE, probed |-> FALSE,
                     barrier |-> Scenario \in {"StoredInterrupt", "StoredBarrierMismatch"},
                     barrierRowMatches |-> Scenario # "StoredBarrierMismatch",
                     proofKind |-> "None", proofReady |-> FALSE, proofScope |-> "None",
                     version |-> 0, proofVersion |-> 0, commitVersion |-> 0,
                     captureCursor |-> 1, terminalEnd |-> 2,
                     committedKind |-> "None", committedScope |-> "None",
                     committedAuthority |-> FALSE, durable |-> FALSE, commits |-> 0,
                     row |-> TRUE, deleted |-> FALSE, deleteAuthority |-> FALSE,
                     deleteDurable |-> FALSE, crashed |-> FALSE, booted |-> TRUE,
                     changed |-> FALSE,
                     readable |-> Scenario \notin {"RetryIO", "PermanentUnread"}]

\* A strict interrupt closes its run and is never a Prompt/receipt candidate.
InterruptTerminalRow == /\ Interrupt /\ terminal.booted /\ ~terminal.classified
                        /\ result = "None"
                        /\ terminal' = [terminal EXCEPT !.classified = TRUE,
                             !.barrier = BeforeInterrupt \/ @,
                             !.proofKind = IF BeforeInterrupt THEN "None" ELSE "Interrupt",
                             !.proofReady = ~BeforeInterrupt,
                             !.proofScope = terminal.scope,
                             !.proofVersion = terminal.version]
                        /\ UNCHANGED result

\* This is a terminal-only overlay; complete capture cursor 1 never becomes raw EOF 2.
RawEOFCloser == /\ Raw /\ terminal.booted /\ terminal.readable /\ ~terminal.probed
                /\ result = "None"
                /\ LET exact == ScopeOK \/ Mode = "NoLineage"
                       eof == terminal.ended \/ Mode = "NoEndGuard"
                       parsed == (terminal.whole /\ terminal.strict) \/ Mode = "AllowPartial"
                       accepted == ~BeforeRaw /\ exact /\ eof /\ parsed
                   IN terminal' = [terminal EXCEPT !.probed = TRUE,
                        !.proofKind = IF accepted THEN "Raw" ELSE "None",
                        !.proofReady = accepted,
                        !.proofScope = terminal.scope,
                        !.proofVersion = terminal.version,
                        !.captureCursor = IF accepted /\ Mode = "AdvanceCapture" THEN 2 ELSE @]
                /\ UNCHANGED result

MetadataRace == /\ Scenario = "StampRace" /\ terminal.proofReady /\ ~terminal.changed
                /\ result = "None"
                /\ terminal' = [terminal EXCEPT !.version = 1, !.changed = TRUE]
                /\ UNCHANGED result
Reobserve == /\ terminal.booted /\ terminal.proofReady
             /\ terminal.proofVersion # terminal.version /\ Mode # "NoStampRecheck"
             /\ result = "None"
             /\ terminal' = [terminal EXCEPT !.proofReady = FALSE, !.probed = FALSE,
                  !.classified = FALSE, !.proofKind = "None"]
             /\ UNCHANGED result

NativeTerminalCommit == /\ terminal.booted /\ terminal.proofReady /\ ~EffectiveBarrier
                        /\ result = "None"
                        /\ (Scenario # "StampRace" \/ terminal.changed)
                        /\ (Scenario # "BootBefore" \/ terminal.crashed)
                        /\ (terminal.proofVersion = terminal.version \/ Mode = "NoStampRecheck")
                        /\ result' = IF terminal.proofKind = "Interrupt" THEN "Cancelled" ELSE "Executed"
                        /\ terminal' = [terminal EXCEPT !.commits = @ + 1,
                             !.committedKind = terminal.proofKind,
                             !.committedScope = terminal.proofScope,
                             !.committedAuthority = terminal.whole /\ terminal.strict /\ ScopeOK
                                /\ (terminal.proofKind = "Interrupt" \/ terminal.ended)
                                /\ (~terminal.barrier \/ terminal.barrierRowMatches),
                             !.commitVersion = terminal.version]

\* Existing scoped operator controls may win the result CAS; the overlay adds no such right.
ExistingControl == /\ ControlKind # "None" /\ ScopeOK /\ terminal.booted
                   /\ result = "None" /\ (terminal.classified \/ terminal.probed)
                   /\ result' = ControlKind
                   /\ terminal' = [terminal EXCEPT !.commits = @ + 1,
                        !.committedKind = "Control", !.committedScope = "Exact",
                        !.committedAuthority = TRUE, !.commitVersion = terminal.version]

\* An invalid tail is no native close: the existing E/no-terminal notice policy owns this branch.
ExistingEndNotice == /\ Scenario \in {"Partial", "Mention", "StoredBarrierMismatch"}
                     /\ (terminal.probed \/ terminal.classified) /\ terminal.ended /\ ScopeOK
                     /\ terminal.booted /\ terminal.readable /\ result = "None"
                     /\ (~terminal.proofReady \/ Scenario = "StoredBarrierMismatch")
                     /\ result' = IF Scenario = "StoredBarrierMismatch" THEN "ExistingBoundaryAck" ELSE "ExistingEndNotice"
                     /\ terminal' = [terminal EXCEPT !.commits = @ + 1,
                          !.committedKind = "ExistingEndNotice", !.committedScope = "Exact",
                          !.committedAuthority = TRUE, !.commitVersion = terminal.version]
FlushTerminalResult == /\ result # "None" /\ ~terminal.durable /\ terminal.booted
                       /\ terminal' = [terminal EXCEPT !.durable = TRUE]
                       /\ UNCHANGED result
CleanupTerminalRow == /\ result # "None" /\ terminal.row /\ terminal.booted /\ terminal.readable
                      /\ (terminal.durable \/ Mode = "NoDurableOrdering")
                      /\ (Scenario \notin {"BootAfter", "SavedMissing", "PermanentSavedUnread"} \/ terminal.crashed)
                      /\ terminal' = [terminal EXCEPT !.row = FALSE, !.deleted = TRUE,
                           !.deleteAuthority = terminal.committedAuthority,
                           !.deleteDurable = terminal.durable]
                      /\ UNCHANGED result

CrashAtBoundary == /\ ~terminal.crashed /\ terminal.booted
                   /\ ((Scenario = "BootBefore" /\ terminal.proofReady /\ result = "None")
                       \/ (Scenario \in {"BootAfter", "SavedMissing", "PermanentSavedUnread"} /\ terminal.durable /\ terminal.row))
                   /\ terminal' = [terminal EXCEPT !.crashed = TRUE, !.booted = FALSE,
                        !.proofReady = FALSE, !.proofKind = "None", !.probed = FALSE,
                        !.classified = FALSE,
                        !.readable = IF Scenario \in {"SavedMissing", "PermanentSavedUnread"} THEN FALSE ELSE @]
                   /\ UNCHANGED result
BootRetry == /\ terminal.crashed /\ ~terminal.booted /\ Mode # "NoBootRetry"
             /\ terminal' = [terminal EXCEPT !.booted = TRUE]
             /\ UNCHANGED result
RepairRead == /\ Scenario \in {"RetryIO", "SavedMissing"} /\ ~terminal.readable
              /\ terminal' = [terminal EXCEPT !.readable = TRUE]
              /\ UNCHANGED result

TerminalNext == /\ (InterruptTerminalRow \/ RawEOFCloser \/ MetadataRace \/ Reobserve
                   \/ NativeTerminalCommit \/ ExistingControl \/ ExistingEndNotice
                   \/ FlushTerminalResult \/ CleanupTerminalRow \/ CrashAtBoundary \/ BootRetry \/ RepairRead)
                /\ UNCHANGED StableBase
TerminalSpec == TerminalInit /\ [][TerminalNext]_TerminalVars
                /\ WF_TerminalVars(InterruptTerminalRow /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(RawEOFCloser /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(MetadataRace /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(Reobserve /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(NativeTerminalCommit /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(ExistingControl /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(ExistingEndNotice /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(FlushTerminalResult /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(CleanupTerminalRow /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(CrashAtBoundary /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(BootRetry /\ UNCHANGED StableBase)
                /\ WF_TerminalVars(RepairRead /\ UNCHANGED StableBase)

NoTerminalEvidenceFreeDelete == terminal.deleted => terminal.deleteAuthority
NoPartialTerminalRead == (terminal.proofReady /\ NativeMode) => terminal.whole /\ terminal.strict
NoForeignTerminalBarrier == ~(Interrupt /\ terminal.strict /\ terminal.classified
                              /\ terminal.barrierRowMatches /\ EffectiveBarrier)
NoForeignBarrierBypass == terminal.committedKind \in {"Interrupt", "Raw"}
                         => ~terminal.barrier \/ terminal.barrierRowMatches
NoForeignTerminalProof == terminal.proofReady => terminal.proofScope = "Exact"
NoStaleTerminalCommit == terminal.committedKind \in {"Interrupt", "Raw"}
                        => terminal.commitVersion = terminal.proofVersion
NoCaptureGuardWidening == terminal.captureCursor = 1
NoCleanupBeforeDurable == terminal.deleted => terminal.deleteDurable
AtMostOneTerminalResult == terminal.commits <= 1
ClosedExecutionEventuallySettles == (terminal.ended /\ ScopeOK)
                                   ~> ~terminal.row
=============================================================================
