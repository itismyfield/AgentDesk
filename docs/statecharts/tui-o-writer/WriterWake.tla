---------------------------- MODULE WriterWake ----------------------------
EXTENDS Naturals
CONSTANTS ParkRule, Case
VARIABLES presence, loss, drain, unsealed, successor, writer, parked, alarm,
          boots, restarted, input, newSource, posted, oldDebt
vars == <<presence, loss, drain, unsealed, successor, writer, parked, alarm,
          boots, restarted, input, newSource, posted, oldDebt>>

\* Source baseline 7a0c819e12; park-and-run contract, issue 7845.
\* One unretired missing cursor, no live capture; retained spool debt may remain.
\* drain is unread capture debt; unsealed is unfinished derivation, not posted debt.
\* family has no EOF witness; superseded models durable successor/link seq.
Init == /\ presence \in IF Case = "active" THEN {"present", "unknown"}
                        ELSE IF Case = "superseded" THEN {"present"}
                        ELSE {"absent"}
        /\ loss \in IF Case = "unsafe" THEN {"missing", "replaced", "denied", "shrunk"}
                    ELSE {"missing"}
        /\ drain \in IF Case = "unsafe" THEN {TRUE, FALSE} ELSE {FALSE}
        /\ unsealed \in IF Case = "unsafe" THEN {TRUE, FALSE} ELSE {FALSE}
        /\ successor = (Case = "superseded")
        /\ writer = "booting" /\ parked = FALSE /\ alarm \in {FALSE, TRUE} /\ boots = 1
        /\ restarted = FALSE
        /\ input = (IF Case = "superseded" THEN "launched" ELSE "waiting")
        /\ newSource = (Case = "superseded") /\ posted = 0 /\ oldDebt = 1

CanPark == loss = "missing" /\ ~drain
           /\ (successor \/ presence = "absent")
           /\ (successor \/ ~unsealed)
Min2(n) == IF n > 2 THEN 2 ELSE n

\* Every gateway, execution/standby and owner_cleanup entry reaches this resume.
ResumePark == /\ writer = "booting" /\ ParkRule /\ CanPark
              /\ writer' = "running" /\ parked' = TRUE /\ alarm' = FALSE
              /\ UNCHANGED <<presence,loss,drain,unsealed,successor,boots,
                              restarted,input,newSource,posted,oldDebt>>
ResumeHalt == /\ writer = "booting" /\ ~(ParkRule /\ CanPark)
              /\ writer' = "halted" /\ alarm' = TRUE
              /\ UNCHANGED <<presence,loss,drain,unsealed,successor,parked,boots,
                              restarted,input,newSource,posted,oldDebt>>
HostEnds == /\ writer = "halted" /\ writer' = "ended"
            /\ UNCHANGED <<presence,loss,drain,unsealed,successor,parked,alarm,
                            boots,restarted,input,newSource,posted,oldDebt>>
PeriodicBoot == /\ writer = "ended" /\ writer' = "booting"
                /\ boots' = Min2(boots + 1)
                /\ UNCHANGED <<presence,loss,drain,unsealed,successor,parked,alarm,
                                restarted,input,newSource,posted,oldDebt>>

\* Pending input cannot create a session until the unchanged readiness gate admits it.
InputArrives == /\ input = "waiting" /\ input' = "held"
                /\ UNCHANGED <<presence,loss,drain,unsealed,successor,writer,
                                parked,alarm,boots,restarted,newSource,posted,oldDebt>>
AdmitInput == /\ input = "held" /\ writer = "running"
              /\ input' = "accepted"
              /\ UNCHANGED <<presence,loss,drain,unsealed,successor,writer,
                              parked,alarm,boots,restarted,newSource,posted,oldDebt>>
\* FreshSubstitution chooses a fresh UUID/resume=false; startup creates Owed{0}.
NewSessionSource == /\ input = "accepted" /\ writer = "running"
                    /\ input' = "launched" /\ presence' = "present"
                    /\ newSource' = TRUE /\ successor' = TRUE
                    /\ UNCHANGED <<loss,drain,unsealed,writer,parked,alarm,
                                    boots,restarted,posted,oldDebt>>
PostFirstReply == /\ input \in {"launched", "done"} /\ newSource /\ writer = "running"
                  /\ posted = 0 /\ posted' = Min2(posted + 1) /\ input' = "done"
                  /\ UNCHANGED <<presence,loss,drain,unsealed,successor,writer,
                                  parked,alarm,boots,restarted,newSource,oldDebt>>
\* Redundant bind and periodic refresh cannot post a second durable reply.
RepeatNotification == /\ writer = "running" /\ input = "done" /\ UNCHANGED vars
\* One bounded process restart resets memory, retaining the new binding/source.
Restart == /\ ~restarted /\ writer = "running"
           /\ restarted' = TRUE /\ writer' = "booting" /\ parked' = FALSE /\ boots' = 1
           /\ UNCHANGED <<presence,loss,drain,unsealed,successor,alarm,input,
                           newSource,posted,oldDebt>>
\* Resume replays all retained records; parked source debt is still delivered.
DeliverRetainedSpool == /\ writer = "running" /\ oldDebt = 1 /\ oldDebt' = 0
                       /\ UNCHANGED <<presence,loss,drain,unsealed,successor,writer,
                                       parked,alarm,boots,restarted,input,newSource,posted>>
Rest == UNCHANGED vars
Next == ResumePark \/ ResumeHalt \/ HostEnds \/ PeriodicBoot \/ InputArrives \/
        AdmitInput \/ NewSessionSource \/ PostFirstReply \/ RepeatNotification \/
        Restart \/ DeliverRetainedSpool \/ Rest
Spec == Init /\ [][Next]_vars
        /\ WF_vars(ResumePark) /\ WF_vars(ResumeHalt) /\ WF_vars(HostEnds)
        /\ WF_vars(PeriodicBoot) /\ WF_vars(InputArrives) /\ WF_vars(AdmitInput)
        /\ WF_vars(NewSessionSource) /\ WF_vars(PostFirstReply)
        /\ WF_vars(DeliverRetainedSpool)

NoQuietReboot == (presence = "absent" /\ loss = "missing" /\ ~drain /\ ~unsealed)
                 => boots <= 1
ParkOnlySafe == parked => CanPark
ActiveLossVisible == (~CanPark /\ writer \in {"halted", "ended"}) => alarm
ReadyHasNoStaleHalt == writer = "running" => ~alarm
FirstReplyOnce == posted <= 1
FirstReplyNeedsAdmission == posted = 1 => newSource /\ input = "done"
ParkedKeepsHost == (writer = "running" /\ parked) => boots = 1
NewSourceGetsFirstReply == (input = "launched") ~> (posted = 1)
RetainedSpoolDelivered == (writer = "running" /\ oldDebt = 1) ~> (oldDebt = 0)
WaitingInputGetsFirstReply == (input = "held") ~> (posted = 1)
=============================================================================
