---- MODULE ClaudeColdLaunchBinding ----
\* A Herdr Claude cold launch: the launch's startup Pending in the binding log, the first turn's
\* read of the transcript the prompt creates, and the hosted row that turns Bound on that source.
EXTENDS Naturals
CONSTANTS Fixed, BindEarly
VARIABLES log, row, file, read, hooks
vars == <<log, row, file, read, hooks>>

Init == log = "none" /\ row = "pending" /\ file = FALSE /\ read = "idle" /\ hooks = 0

\* attach_launched_herdr_source: the Stat of a transcript not written yet logs Pending(startup).
Attach == read = "idle" /\ log' = "pending" /\ read' = "submitted"
          /\ UNCHANGED <<row, file, hooks>>

\* A hook of the launch session (SessionStart, UserPromptSubmit, later events): the launch-hook
\* check promotes only an unverified registered Source, so the launch's own Pending stays.
LaunchHook == hooks < 2 /\ hooks' = hooks + 1 /\ UNCHANGED <<log, row, file, read>>

\* Claude writes the transcript with the first record of the submitted prompt.
WriteTranscript == read = "submitted" /\ ~file /\ file' = TRUE
                   /\ UNCHANGED <<log, row, read, hooks>>

\* Cancel or session death before the transcript: observe returns before any registration.
EndBeforeFile == read = "submitted" /\ ~file /\ read' = "endedNoFile"
                 /\ UNCHANGED <<log, row, file, hooks>>

\* The transcript exists, so the read starts. Fixed registers it first: the Stat resolves the
\* Pending and bind_once_logged turns the row Bound on that record.
StartRead == /\ read = "submitted" /\ file /\ read' = "reading"
             /\ IF Fixed /\ row = "pending"
                THEN log' = "resolved" /\ row' = (IF BindEarly THEN "bound" ELSE row)
                ELSE UNCHANGED <<log, row>>
             /\ UNCHANGED <<file, hooks>>

\* A read that reaches its end registers and binds; one woken by completion cleanup without a
\* terminal, or failing, returns before that.
EndRead == read = "reading" /\ read' = "ended" /\ log' = "resolved" /\ row' = "bound"
           /\ UNCHANGED <<file, hooks>>
EndEarly == read = "reading" /\ read' = "endedEarly" /\ UNCHANGED <<log, row, file, hooks>>

Next == Attach \/ LaunchHook \/ WriteTranscript \/ EndBeforeFile \/ StartRead \/ EndRead \/ EndEarly
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

TypeOK == log \in {"none", "pending", "resolved"} /\ row \in {"pending", "bound"}
          /\ file \in BOOLEAN /\ hooks \in 0..2
          /\ read \in {"idle", "submitted", "reading", "ended", "endedEarly", "endedNoFile"}
\* The first turn is read on a resolved source and a Bound row, so presence reads it Bound.
FirstTurnBound == read = "reading" => (log = "resolved" /\ row = "bound")
\* A launched binding ends resolved and Bound, or waits only because no transcript exists yet.
LaunchSettles == <>[]((log = "resolved" /\ row = "bound") \/ (read = "endedNoFile" /\ ~file))
====
