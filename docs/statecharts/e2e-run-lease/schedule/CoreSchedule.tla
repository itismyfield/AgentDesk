---- MODULE CoreSchedule ----
\* Periodic core-bundle requests on the release host. The supervisor asks for one run per deployed
\* head and interval slot, starts nothing while a deploy holds the test cells, and every started
\* run ends in a result or an abandoned record. Switched off, it starts nothing and the status file
\* consumers read says so, even after a run that was in flight at the switch publishes again.
EXTENDS Naturals, FiniteSets
CONSTANTS Mut, MaxHead, MaxSlot, MaxTicks
VARIABLES head, slot, deploying, waiting, running, records, latest, clock, startedInDeploy, shadowed,
          enabled, shown, offTicked

switch == <<enabled, shown, offTicked>>
vars == <<head, slot, deploying, waiting, running, records, latest, clock, startedInDeploy, shadowed, enabled, shown, offTicked>>
None == [kind |-> "none", head |-> 0, slot |-> 0, at |-> 0]
Requests == [kind: {"deploy", "core"}, head: 1..MaxHead, slot: 0..MaxSlot, at: 0..MaxTicks]

Init == /\ head = 1 /\ slot = 0 /\ deploying = FALSE /\ waiting = {} /\ running = None
        /\ records = {} /\ latest = None /\ clock = 0 /\ startedInDeploy = FALSE /\ shadowed = FALSE
        /\ enabled = TRUE /\ shown = FALSE /\ offTicked = FALSE

\* A request id is its kind, head and slot; the tick_id mutant names core requests by the tick instead.
Id(q) == [kind |-> q.kind, head |-> q.head, slot |-> q.slot, at |-> IF Mut = "tick_id" /\ q.kind = "core" THEN q.at ELSE 0]
Seen == {Id(q): q \in waiting} \cup {Id(r.req): r \in records} \cup (IF running = None THEN {} ELSE {Id(running)})
Fresh == latest.kind = "core" /\ latest.head = head /\ latest.slot = slot

BeginDeploy == /\ ~deploying /\ head < MaxHead /\ deploying' = TRUE
               /\ UNCHANGED <<head, slot, waiting, running, records, latest, clock, startedInDeploy, shadowed, switch>>
\* The deploy hook submits the new head after the smoke, as the deploy lock is about to be released.
EndDeploy == /\ deploying /\ clock < MaxTicks /\ deploying' = FALSE /\ head' = head + 1 /\ clock' = clock + 1
             /\ waiting' = waiting \cup {[kind |-> "deploy", head |-> head + 1, slot |-> slot, at |-> clock + 1]}
             /\ UNCHANGED <<slot, running, records, latest, startedInDeploy, shadowed, switch>>
TimePasses == /\ slot < MaxSlot /\ slot' = slot + 1
              /\ UNCHANGED <<head, deploying, waiting, running, records, latest, clock, startedInDeploy, shadowed, switch>>
\* A rotation result may replace the newest summary, so freshness alone cannot stop a second request.
OtherSuite == /\ latest.kind = "core" /\ latest' = None
              /\ UNCHANGED <<head, slot, deploying, waiting, running, records, clock, startedInDeploy, shadowed, switch>>

Submit == LET q == [kind |-> "core", head |-> head, slot |-> slot, at |-> clock + 1] IN
    /\ enabled /\ running = None /\ clock < MaxTicks /\ (Mut = "ignore_deploy" \/ ~deploying) /\ ~Fresh
    /\ (Mut = "ignore_waiting" \/ ~\E w \in waiting: w.head = head)
    /\ Id(q) \notin Seen
    /\ waiting' = waiting \cup {q} /\ clock' = clock + 1
    /\ UNCHANGED <<head, slot, deploying, running, records, latest, startedInDeploy, shadowed, switch>>

\* The runner takes the newest waiting request and records the older ones as superseded.
Start == /\ enabled /\ running = None /\ waiting # {} /\ (Mut = "ignore_deploy" \/ ~deploying)
         /\ LET q == CHOOSE w \in waiting: \A o \in waiting: o.at <= w.at IN
              /\ running' = q /\ waiting' = {}
              /\ shadowed' = (shadowed \/ (q.kind = "core" /\ \E o \in waiting: o.kind = "deploy" /\ o.head = q.head))
         /\ startedInDeploy' = (startedInDeploy \/ deploying)
         /\ UNCHANGED <<head, slot, deploying, records, latest, clock, switch>>
Finish == /\ running # None /\ records' = records \cup {[req |-> running, outcome |-> "result"]}
          /\ latest' = [kind |-> "core", head |-> running.head, slot |-> slot, at |-> 0] /\ running' = None
          \* The run publishes with the settings it started under, which said enabled.
          /\ shown' = TRUE /\ offTicked' = FALSE
          /\ UNCHANGED <<head, slot, deploying, waiting, clock, startedInDeploy, shadowed, enabled>>
\* The next lock holder marks a crashed run abandoned; it is never re-executed.
Crash == /\ running # None /\ running' = None
         /\ records' = IF Mut = "drop_on_crash" THEN records ELSE records \cup {[req |-> running, outcome |-> "abandoned"]}
         /\ latest' = IF Mut = "drop_on_crash" THEN latest ELSE [kind |-> "core", head |-> running.head, slot |-> slot, at |-> 0]
         /\ UNCHANGED <<head, slot, deploying, waiting, clock, startedInDeploy, shadowed, switch>>
SwitchOff == /\ enabled /\ enabled' = FALSE
             /\ UNCHANGED <<head, slot, deploying, waiting, running, records, latest, clock, startedInDeploy, shadowed, shown, offTicked>>
\* An off tick turns a status that still shows enabled into the switched-off one.
OffTick == /\ ~enabled /\ running = None /\ ~offTicked /\ offTicked' = TRUE
           /\ shown' = (shown /\ Mut = "off_silent")
           /\ UNCHANGED <<head, slot, deploying, waiting, running, records, latest, clock, startedInDeploy, shadowed, enabled>>

Progress == BeginDeploy \/ EndDeploy \/ TimePasses \/ OtherSuite \/ Submit \/ Start \/ Finish \/ Crash \/ SwitchOff \/ OffTick
Next == Progress \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars /\ WF_vars(Finish \/ Crash)

\* The same head and slot are judged by at most one periodic run.
OneCoreRunPerHeadAndSlot == \A h \in 1..MaxHead, s \in 0..MaxSlot:
    Cardinality({r \in records: r.req.kind = "core" /\ r.req.head = h /\ r.req.slot = s}) <= 1
NoStartDuringDeploy == ~startedInDeploy
DeployRequestKeepsItsRun == ~shadowed
\* After an off tick with no run in flight, file consumers see the runner switched off.
SwitchedOffIsShown == (~enabled /\ offTicked) => ~shown
StartedRunIsRecorded == \A q \in Requests: (running = q) ~> (\E r \in records: r.req = q)
====
