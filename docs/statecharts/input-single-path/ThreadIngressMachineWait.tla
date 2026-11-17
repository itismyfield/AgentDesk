-------------------- MODULE ThreadIngressMachineWait --------------------
EXTENDS Naturals
CONSTANTS Host, InitialRead, Mutation
VARIABLES phase, source, ended, started, native, reply, queued, personJoins
vars == <<phase, source, ended, started, native, reply, queued, personJoins>>
Init == /\ phase = "read" /\ source = InitialRead /\ ended = FALSE /\ started = FALSE
        /\ native = 0 /\ reply = 0 /\ queued = TRUE /\ personJoins = 0
Reading == IF Host = "herdr" /\ Mutation = "tmuxOnHerdr" THEN "unknown"
           ELSE IF source = "unknown" /\ Mutation = "unknownIdle" THEN "idle"
           ELSE source
Read == /\ phase = "read"
        /\ phase' = IF Reading = "idle" THEN "ready" ELSE "watch"
        /\ UNCHANGED <<source, ended, started, native, reply, queued, personJoins>>
Terminal == /\ ~ended /\ ended' = TRUE
            /\ UNCHANGED <<phase, source, started, native, reply, queued, personJoins>>
\* Exact native evidence eventually catches up after terminal under the healthy reader port.
CatchUp == /\ ended /\ source # "idle" /\ source' = "idle"
           /\ UNCHANGED <<phase, ended, started, native, reply, queued, personJoins>>
Wake == /\ phase = "watch" /\ Reading = "idle" /\ Mutation # "noWake"
        /\ phase' = "read"
        /\ UNCHANGED <<source, ended, started, native, reply, queued, personJoins>>
Begin == /\ phase = "ready"
         /\ Reading = "idle"
         /\ phase' = "running" /\ started' = TRUE /\ native' = native + 1
         /\ queued' = FALSE
         /\ UNCHANGED <<source, ended, reply, personJoins>>
Reply == /\ phase = "running" /\ phase' = "done" /\ reply' = reply + 1
         /\ UNCHANGED <<source, ended, started, native, queued, personJoins>>
PersonJoin == /\ ~ended /\ personJoins = 0 /\ personJoins' = 1
              /\ UNCHANGED <<phase, source, ended, started, native, reply, queued>>
WaitStep == /\ phase \in {"watch", "done"} /\ UNCHANGED vars
Next == Read \/ Terminal \/ CatchUp \/ Wake \/ Begin \/ Reply \/ PersonJoin \/ WaitStep
Spec == Init /\ [][Next]_vars /\ WF_vars(Read) /\ WF_vars(Terminal)
        /\ WF_vars(CatchUp) /\ WF_vars(Wake) /\ WF_vars(Begin) /\ WF_vars(Reply)
MachineWaits == started => ended
NoDuplicates == native <= 1 /\ reply <= 1 /\ personJoins <= 1
NoSilentLoss == queued \/ started
MachineOwnTurn == ended ~> started
EachReply == started ~> (reply = 1)
=============================================================================
