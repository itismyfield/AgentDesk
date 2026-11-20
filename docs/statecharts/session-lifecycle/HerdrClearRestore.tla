---- MODULE HerdrClearRestore ----
EXTENDS Naturals
CONSTANTS Path, Variant

VARIABLE s
vars == <<s>>

Init ==
    IF Path = "clear"
    THEN \E original \in {"bound", "pending", "retired", "unknown"},
            store \in {"ok", "unavailable"}:
        s = [original |-> original, row |-> original, request |-> "idle",
             source |-> IF original = "bound" THEN "verified" ELSE "missing",
             store |-> store, record |-> "none"]
    ELSE \E capability \in {"confirmed", "unknown"}:
        s = [server |-> 0, workspace |-> "agent", lineage |-> "unknown",
             activity |-> "unknown", work |-> "busy", revision |-> 0,
             ticket |-> <<0, 0>>, capability |-> capability, unsafe |-> FALSE]

RequestClear ==
    /\ s.request = "idle"
    /\ s' = [s EXCEPT !.request = "started"]

LoseSource ==
    /\ s.request = "started" /\ s.row = "bound" /\ s.source = "verified"
    /\ s' = [s EXCEPT !.source = "missing"]

HasExactTarget == s.original \in {"bound", "pending"}
RecordEligible == s.original = "bound" /\ s.source = "verified"
RecordGate == Variant = "RecordGate" /\ s.row = "bound" /\ s.source = "missing"

RefuseUnknownRecord ==
    /\ s.request = "started" /\ RecordGate
    /\ s' = [s EXCEPT !.request = "refused"]

RefuseUnknownTarget ==
    /\ s.request = "started" /\ s.row = "unknown"
    /\ s' = [s EXCEPT !.request = "refused"]

CloseExactOrObserveEnded ==
    /\ s.request = "started" /\ s.row \in {"bound", "pending"} /\ ~RecordGate
    /\ s' = [s EXCEPT !.row = "ended"]

RetireExact ==
    /\ s.row = "ended"
    /\ s' = [s EXCEPT !.row = "retired"]

RecordAfterEnd ==
    /\ s.request = "started" /\ s.row = "retired" /\ s.original = "bound" /\ s.record = "none"
    /\ Variant \notin {"Main", "SkipRecord"}
    /\ s' = [s EXCEPT !.record = IF RecordEligible /\ s.store = "ok" THEN "written" ELSE "warned"]

ReportClear ==
    /\ s.request = "started" /\ s.row = "retired"
    /\ (s.original # "bound" \/ s.record # "none" \/ Variant \in {"Main", "SkipRecord"})
    /\ s' = [s EXCEPT !.request = "reported"]

InterruptPostlude ==
    /\ s.request = "started" /\ s.row = "retired" /\ HasExactTarget
    /\ s.record = "none"
    /\ s' = [s EXCEPT !.request = "interrupted"]

ClearNext == RequestClear \/ LoseSource \/ RefuseUnknownRecord \/ RefuseUnknownTarget
             \/ CloseExactOrObserveEnded \/ RetireExact \/ RecordAfterEnd
             \/ ReportClear \/ InterruptPostlude

\* A restored shell can have unknown history or background work even when foreground looks idle.
RestartServer ==
    /\ s.server < 2 /\ s.workspace \in {"agent", "shell", "occupied"}
    /\ IF s.server = 0
          THEN \E look \in {"missing", "shell", "occupied"},
                  lineage \in {"verified", "unknown"},
                  history \in {"unused", "unknown"},
                  work \in {"empty", "busy", "unknown"}:
                s' = [s EXCEPT !.server = 1, !.workspace = look, !.lineage = lineage,
                              !.activity = history, !.work = work]
          ELSE s' = [s EXCEPT !.server = 2, !.revision = 0,
                             !.workspace = IF s.activity = "used" THEN "shell" ELSE @,
                             !.work = IF s.activity = "used" THEN "unknown" ELSE @]

UseRestoredShell ==
    /\ s.workspace = "shell" /\ s.activity = "unused"
    /\ s' = [s EXCEPT !.workspace = "occupied", !.activity = "used", !.work = "busy",
                  !.revision = @ + 1]

ReturnToShell ==
    /\ s.workspace = "occupied" /\ s.activity = "used"
    /\ \E result \in {"empty", "busy", "unknown"}:
         s' = [s EXCEPT !.workspace = "shell", !.work = result, !.revision = @ + 1]

LoseLineageEvidence ==
    /\ s.server # 0 /\ s.lineage = "verified" /\ s.workspace \in {"shell", "occupied"}
    /\ s' = [s EXCEPT !.lineage = "unknown", !.revision = @ + 1]

LoseNoWorkProof ==
    /\ s.workspace = "shell" /\ s.work = "empty"
    /\ \E work \in {"busy", "unknown"}:
         s' = [s EXCEPT !.work = work, !.revision = @ + 1]

PotentialTarget == s.workspace = "shell" /\ s.capability = "confirmed"
Eligible == /\ PotentialTarget /\ s.lineage = "verified" /\ s.activity = "unused" /\ s.work = "empty"
Allowed ==
    /\ PotentialTarget
    /\ (s.lineage = "verified" \/ Variant = "NoLineageGate")
    /\ (s.activity = "unused" \/ (Variant = "NoActivityGate" /\ s.activity = "used"))
    /\ (s.work = "empty" \/ Variant = "NoWorkGate")

HasTicket == s.ticket[1] # 0
CurrentTicket == s.ticket = <<s.server, s.revision>>

SurveyEmpty ==
    /\ Allowed /\ ~CurrentTicket /\ Variant \notin {"Main", "NoWake"}
    /\ s' = [s EXCEPT !.ticket = <<s.server, s.revision>>]

CompareAndClose ==
    /\ s.workspace \in {"shell", "occupied"} /\ HasTicket
    /\ ((Allowed /\ CurrentTicket) \/ Variant = "ReadThenClose")
    /\ s' = [s EXCEPT !.workspace = "closed", !.unsafe = ~(Eligible /\ CurrentTicket)]

RestoreNext == RestartServer \/ UseRestoredShell \/ ReturnToShell
               \/ LoseLineageEvidence \/ LoseNoWorkProof \/ SurveyEmpty \/ CompareAndClose

SpecClear == Init /\ [][ClearNext]_vars
             /\ WF_vars(CloseExactOrObserveEnded) /\ WF_vars(RetireExact) /\ WF_vars(RefuseUnknownRecord)
SpecRestore == Init /\ [][RestoreNext]_vars /\ WF_vars(SurveyEmpty) /\ WF_vars(CompareAndClose)

NoRecordRefusesAuthorizedClear == HasExactTarget => s.request # "refused"
NoUnprovenEndRecord ==
    s.record = "written" => s.row = "retired" /\ RecordEligible /\ s.store = "ok"
NoMissingClearEndRecord ==
    (s.request = "reported" /\ s.original = "bound")
        => s.record = (IF RecordEligible /\ s.store = "ok" THEN "written" ELSE "warned")
AuthorizedClearEnds == (HasExactTarget /\ s.request = "started") ~> (s.row = "retired")

NoUnprovenWorkspaceRemoved == ~s.unsafe
EmptyWorkspaceEventuallyCleared == Eligible ~> (s.workspace = "closed" \/ ~Eligible)
====
