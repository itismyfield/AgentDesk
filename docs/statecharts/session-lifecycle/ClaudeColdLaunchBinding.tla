---- MODULE ClaudeColdLaunchBinding ----
\* A single Herdr Claude cold launch with no tool, notification, or Stop hook during the first read.
CONSTANTS RegisterEarly, BindEarly, PinFirstRecord
VARIABLES log, row, file, read, hooks, pin, queue
vars == <<log, row, file, read, hooks, pin, queue>>

Init == /\ log = "none" /\ row = "pending" /\ file = FALSE /\ read = "idle"
        /\ hooks = "none" /\ pin = FALSE /\ queue = "waiting"

SessionStart == read = "idle" /\ hooks = "none" /\ hooks' = "start"
                /\ UNCHANGED <<log, row, file, read, pin, queue>>

\* attach_launched_herdr_source: the Stat of a transcript not written yet logs Pending(startup).
Attach == read = "idle" /\ hooks = "start" /\ log' = "pending" /\ read' = "submitted"
          /\ UNCHANGED <<row, file, hooks, pin, queue>>

UserPromptSubmit == read = "submitted" /\ hooks = "start" /\ ~file /\ hooks' = "prompt"
                    /\ UNCHANGED <<log, row, file, read, pin, queue>>

\* Claude writes the transcript with the first complete record of the submitted prompt.
WriteTranscript == read = "submitted" /\ hooks = "prompt" /\ ~file /\ file' = TRUE
                   /\ UNCHANGED <<log, row, read, hooks, pin, queue>>

\* Cancel or session death before the transcript: observe returns before any registration.
EndBeforeFile == read = "submitted" /\ ~file /\ read' = "endedNoFile"
                 /\ UNCHANGED <<log, row, file, hooks, pin, queue>>

\* At the first observable progress, Stat can resolve Pending, PG can bind, and the opened
\* record can validate the source against the retained launch hook.
StartRead == /\ read = "submitted" /\ file /\ read' = "reading"
             /\ IF RegisterEarly
                THEN /\ log' = "resolved"
                     /\ row' = (IF BindEarly THEN "bound" ELSE row)
                     /\ pin' = PinFirstRecord
                ELSE UNCHANGED <<log, row, pin>>
             /\ UNCHANGED <<file, hooks, queue>>

\* The busy route needs both a Bound row and a current verified source.
TryQueueInjection == /\ read = "reading" /\ queue = "waiting"
                     /\ queue' = IF row # "bound" THEN "unbound"
                                 ELSE IF ~pin THEN "noPin" ELSE "eligible"
                     /\ UNCHANGED <<log, row, file, read, hooks, pin>>

\* A read that reaches its end registers and binds; one woken by completion cleanup without a
\* terminal, or failing, returns before that.
EndRead == read = "reading" /\ read' = "ended" /\ log' = "resolved" /\ row' = "bound"
           /\ UNCHANGED <<file, hooks, pin, queue>>
EndEarly == read = "reading" /\ read' = "endedEarly"
            /\ UNCHANGED <<log, row, file, hooks, pin, queue>>

Next == SessionStart \/ Attach \/ UserPromptSubmit \/ WriteTranscript \/ EndBeforeFile
        \/ StartRead \/ TryQueueInjection \/ EndRead \/ EndEarly
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

TypeOK == /\ log \in {"none", "pending", "resolved"} /\ row \in {"pending", "bound"}
          /\ file \in BOOLEAN /\ hooks \in {"none", "start", "prompt"} /\ pin \in BOOLEAN
          /\ read \in {"idle", "submitted", "reading", "ended", "endedEarly", "endedNoFile"}
          /\ queue \in {"waiting", "unbound", "noPin", "eligible"}
\* The first turn is read on a resolved source and a Bound row, so presence reads it Bound.
FirstTurnBound == read = "reading" => (log = "resolved" /\ row = "bound")
\* A queue attempt during the first read can pass the source gate without a later hook.
FirstTurnQueueEligible == read = "reading" => queue \in {"waiting", "eligible"}
\* A launched binding ends resolved and Bound, or waits only because no transcript exists yet.
LaunchSettles == <>[]((log = "resolved" /\ row = "bound") \/ (read = "endedNoFile" /\ ~file))
====
