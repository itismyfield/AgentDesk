---- MODULE HerdrBusyMarkerWindow ----
\* One person input reaching a running Herdr Claude turn: queue commit, owner busy read, hold, retry.
EXTENDS Naturals
CONSTANTS Policy, Scenario, Mutant
VARIABLES tail, arrival, phase, route, keyed, effects, plus, mailboxAdds, shown
vars == <<tail, arrival, phase, route, keyed, effects, plus, mailboxAdds, shown>>

\* The transcript tail the busy read sees. Markers are the session records Claude writes after a
\* prompt or tool result (prompt-markers) or after a finished reply (reply-markers).
Running == {"assistant", "prompt-markers"}
Reading(t) ==
    CASE t = "assistant" -> "Busy"
      [] t = "prompt-markers" -> IF Policy = "Fixed" THEN "Busy" ELSE "Unknown"
      [] t = "reply-markers" -> IF Mutant = "AllMarkersBusy" THEN "Busy" ELSE "Unknown"
      [] t = "idle" -> "Idle"
\* Before, a Herdr channel's queue commit posts the mailbox marker ahead of the owner's result.
MarkerFirst == Policy = "Before" \/ Mutant = "HerdrMarkerFirst"

Init == /\ tail = Scenario /\ arrival = "none" /\ phase = "arrive" /\ route = "none"
        /\ keyed = "none" /\ effects = 0 /\ plus = 0 /\ mailboxAdds = 0 /\ shown = FALSE

Commit == /\ phase = "arrive"
          /\ phase' = "queued" /\ arrival' = tail
          /\ mailboxAdds' = IF MarkerFirst THEN 1 ELSE 0
          /\ shown' = MarkerFirst
          /\ UNCHANGED <<tail, route, keyed, effects, plus>>
\* The native turn moves on: a reply record lands after a prompt; a finished reply ends the turn.
Progress == /\ tail \in {"prompt-markers", "reply-markers"}
            /\ tail' = IF tail = "prompt-markers" THEN "assistant" ELSE "idle"
            /\ UNCHANGED <<arrival, phase, route, keyed, effects, plus, mailboxAdds, shown>>
Key(r) == /\ phase' = "sent" /\ route' = r /\ keyed' = tail
          /\ effects' = effects + 1 /\ plus' = plus + 1 /\ shown' = FALSE
          /\ UNCHANGED mailboxAdds
\* The owner front: a Busy read pastes into the turn, an idle pane starts one, anything else holds
\* before any key and leaves the mailbox marker.
Attempt == /\ phase = "queued"
           /\ CASE Reading(tail) = "Busy" -> Key("busy")
                [] Reading(tail) = "Idle" -> Key("start")
                [] OTHER -> /\ phase' = "held" /\ UNCHANGED <<route, keyed, effects, plus>>
                            /\ IF Mutant = "NoHandback" \/ shown
                               THEN UNCHANGED <<mailboxAdds, shown>>
                               ELSE (mailboxAdds' = mailboxAdds + 1 /\ shown' = TRUE)
           /\ UNCHANGED <<tail, arrival>>
\* The held watch wakes the drive once the same read admits a key or the pane went idle.
Retry == /\ phase = "held" /\ Reading(tail) \in {"Busy", "Idle"}
         /\ phase' = "queued"
         /\ UNCHANGED <<tail, arrival, route, keyed, effects, plus, mailboxAdds, shown>>
Wait == UNCHANGED vars
Next == Commit \/ Progress \/ Attempt \/ Retry \/ Wait
Spec == Init /\ [][Next]_vars /\ WF_vars(Commit) /\ WF_vars(Progress) /\ WF_vars(Attempt)
        /\ WF_vars(Retry)

TypeOK == /\ tail \in {"assistant", "prompt-markers", "reply-markers", "idle"}
          /\ phase \in {"arrive", "queued", "held", "sent"}
          /\ route \in {"none", "busy", "start"}
AnswerAtMostOnce == effects <= 1 /\ plus <= 1
\* A busy paste only goes into a turn the transcript shows still owes a reply.
NoBusyKeyWithoutRunningTurn == route = "busy" => keyed \in Running
\* An input that arrived while the turn was observably running never shows the mailbox marker.
NoMailboxWhenObservable == arrival \in Running => mailboxAdds = 0
\* An input held before any key always shows the mailbox marker.
NoSilentHold == phase = "held" => shown
AnswerDelivered == <> (phase = "sent")
=============================================================================
