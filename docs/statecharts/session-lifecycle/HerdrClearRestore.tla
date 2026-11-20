---- MODULE HerdrClearRestore ----
EXTENDS Naturals
CONSTANTS Path, Variant

VARIABLE s
vars == <<s>>

\* The phases below project the production entry points in herdr-clear-restore.report.md.
\* "staged" and "written" describe the proposed durable exact-nonce record.
Init ==
    /\ IF Path = "clear"
          THEN \/ s = [request |-> "idle", row |-> "bound", record |-> "none",
                        readable |-> TRUE, restarted |-> FALSE, endedHere |-> FALSE]
                  \/ s = [request |-> "idle", row |-> "retired", record |-> "none",
                            readable |-> FALSE, restarted |-> FALSE, endedHere |-> FALSE]
          ELSE \E authority \in {"granted", "held"}:
                 s = [server |-> 0, workspace |-> "agent", owner |-> "unobserved",
                      shell |-> 0, ticket |-> <<0, 0>>, changed |-> FALSE, unsafe |-> FALSE,
                      authority |-> authority]
    /\ Path \in {"clear", "restore"}

RequestClear ==
    /\ s.request = "idle"
    /\ s' = [s EXCEPT !.request = "started"]

LoseSource ==
    /\ s.row = "bound" /\ s.record = "none" /\ s.readable
    /\ s' = [s EXCEPT !.readable = FALSE]

StageEndRecord ==
    /\ s.request = "started" /\ s.row = "bound" /\ s.readable /\ s.record = "none"
    /\ Variant \notin {"Main", "SkipStage"}
    /\ s' = [s EXCEPT !.record = "staged"]

RefuseUnknownRecord ==
    /\ s.request = "started" /\ s.record = "none" /\ ~s.readable
    /\ Variant \notin {"Main", "SkipStage"}
    /\ s' = [s EXCEPT !.request = "refused"]

CloseExactOrObserveEnded ==
    /\ s.request = "started" /\ s.row = "bound"
    /\ (s.record = "staged" \/ Variant \in {"Main", "SkipStage"})
    /\ s' = [s EXCEPT !.row = "ended", !.endedHere = TRUE]

RetireExact ==
    /\ s.row = "ended"
    /\ s' = [s EXCEPT !.row = "retired"]

RestartDuringClear ==
    /\ s.request = "started" /\ s.row \in {"ended", "retired"} /\ ~s.restarted
    /\ s' = [s EXCEPT !.restarted = TRUE, !.readable = FALSE,
                  !.record = IF Variant = "VolatileStage" /\ @ = "staged" THEN "none" ELSE @]

CommitEndRecord ==
    /\ s.row = "retired" /\ s.record = "staged"
    /\ s' = [s EXCEPT !.record = "written"]

ReportClear ==
    /\ s.request = "started" /\ s.row = "retired"
    /\ (s.record = "written" \/ Variant \in {"Main", "SkipStage"})
    /\ s' = [s EXCEPT !.request = "reported"]

ClearNext == RequestClear \/ LoseSource \/ StageEndRecord \/ RefuseUnknownRecord
             \/ CloseExactOrObserveEnded \/ RetireExact \/ RestartDuringClear
             \/ CommitEndRecord \/ ReportClear

\* Restore observations: missing, verified lone shell, unknown ownership, or an occupied pane.
\* The second epoch stands for a restart after a current proof was read.
RestartServer ==
    /\ s.server < 2 /\ s.workspace \in {"agent", "shell", "occupied"}
    /\ \E appearance \in {"missing", "shell", "occupied"}:
         \E ownership \in {"verified", "unknown"}:
           s' = [s EXCEPT !.server = @ + 1, !.shell = @ + 1,
                         !.workspace = appearance, !.owner = ownership]

Eligible == /\ s.workspace = "shell" /\ s.owner = "verified" /\ s.authority = "granted"
CurrentTicket == s.ticket = <<s.server, s.shell>>

SurveyEmpty ==
    /\ Eligible /\ ~CurrentTicket /\ Variant \notin {"Main", "NoWake"}
    /\ s' = [s EXCEPT !.ticket = <<s.server, s.shell>>]

WorkspaceChanges ==
    /\ Eligible /\ ~s.changed
    /\ \E appearance \in {"occupied", "shell"}:
         s' = [s EXCEPT !.workspace = appearance, !.shell = @ + 1, !.changed = TRUE]

CompareAndClose ==
    /\ s.workspace \in {"shell", "occupied"} /\ s.ticket[1] # 0
    /\ ((Eligible /\ CurrentTicket) \/ Variant = "ReadThenClose")
    /\ s' = [s EXCEPT !.workspace = "closed", !.unsafe = ~(Eligible /\ CurrentTicket)]

RestoreNext == RestartServer \/ SurveyEmpty \/ WorkspaceChanges \/ CompareAndClose
SpecClear == Init /\ [][ClearNext]_vars /\ WF_vars(RetireExact) /\ WF_vars(CommitEndRecord)
SpecRestore == Init /\ [][RestoreNext]_vars /\ WF_vars(SurveyEmpty) /\ WF_vars(CompareAndClose)

NoMissingClearEndRecord ==
    IF Path # "clear" THEN TRUE
    ELSE /\ (s.request = "reported" => s.record = "written")
         /\ (s.endedHere => s.record \in {"staged", "written"})

PendingRecordDrains ==
    IF Path # "clear" THEN [](TRUE)
    ELSE (s.record = "staged" /\ s.row \in {"ended", "retired"}) ~> (s.record = "written")

NoLiveWorkspaceRemoved == IF Path # "restore" THEN TRUE ELSE ~s.unsafe
EmptyWorkspaceEventuallyCleared ==
    IF Path # "restore" THEN [](TRUE)
    ELSE Eligible ~> (s.workspace = "closed" \/ ~Eligible)
====
