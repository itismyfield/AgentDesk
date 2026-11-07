--------------------------- MODULE WriterMissingHalt ---------------------------
EXTENDS Naturals
CONSTANTS HoldMissing, ObserveResume, Case, RetireMarkerLoss
VARIABLES phase, claim, ready, boots, evidence, consumed, sourcePresent,
          presence, nonce, binding, otherPaneLive, changed, retired,
          metadataReadable, ioSeen, orphan
vars == <<phase, claim, ready, boots, evidence, consumed, sourcePresent,
          presence, nonce, binding, otherPaneLive, changed, retired,metadataReadable,ioSeen,orphan>>
Init == /\ phase = "booting" /\ claim = TRUE /\ ready = FALSE /\ boots = 1
        /\ evidence = 0 /\ consumed = 0 /\ sourcePresent = FALSE
        /\ presence \in (IF Case = "orphan" THEN {"unknown"} ELSE {"unknown", "present"}) /\ nonce = 0 /\ binding = 0
        /\ otherPaneLive = FALSE /\ changed = FALSE /\ retired = FALSE
        /\ metadataReadable = TRUE /\ ioSeen = FALSE
        /\ orphan = IF Case = "orphan" THEN "bound" ELSE "none"
Halt == /\ phase = "booting" /\ ~sourcePresent /\ phase' = "halted"
        /\ ready' = FALSE
        /\ UNCHANGED <<claim,boots,evidence,consumed,sourcePresent,presence,
                        nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen,orphan>>
Run == /\ phase = "booting" /\ sourcePresent /\ phase' = "running"
       /\ ready' = TRUE /\ consumed' = evidence
       /\ UNCHANGED <<claim,boots,evidence,sourcePresent,presence,
                       nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen,orphan>>
Hold == /\ phase = "halted" /\ HoldMissing /\ phase' = "held"
        /\ UNCHANGED <<claim,ready,boots,evidence,consumed,sourcePresent,presence,
                        nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen,orphan>>
End == /\ phase = "halted" /\ ~HoldMissing
       /\ phase' = "ended" /\ claim' = FALSE
       /\ UNCHANGED <<ready,boots,evidence,consumed,sourcePresent,presence,
                       nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen,orphan>>
Boot == /\ phase = "ended" /\ ~claim /\ ~retired /\ boots < 3
        /\ phase' = "booting" /\ claim' = TRUE /\ boots' = boots + 1
        /\ consumed' = evidence
        /\ UNCHANGED <<ready,evidence,sourcePresent,presence,nonce,binding,
                        otherPaneLive,changed,retired,metadataReadable,ioSeen,orphan>>
Wake == /\ phase = "held" /\ ObserveResume /\ evidence > consumed /\ ~retired
        /\ metadataReadable
        /\ phase' = "booting" /\ boots' = boots + 1 /\ consumed' = evidence
        /\ UNCHANGED <<claim,ready,evidence,sourcePresent,presence,nonce,binding,
                        otherPaneLive,changed,retired,metadataReadable,ioSeen,orphan>>
NewEvidence == /\ ~changed /\ Case \notin {"stable", "orphan"} /\ ~retired
               /\ (Case # "early" \/ phase \in {"booting", "halted"})
               /\ evidence' = evidence + 1 /\ changed' = TRUE
               /\ sourcePresent' = (Case = "source" \/ Case = "early")
               /\ presence' = IF Case \in {"presence", "metadata"} THEN "absent" ELSE presence
               /\ nonce' = IF Case = "nonce" THEN 1 ELSE nonce
               /\ binding' = IF Case = "binding" THEN 1 ELSE binding
               /\ UNCHANGED <<phase,claim,ready,boots,consumed,otherPaneLive,retired,metadataReadable,ioSeen,orphan>>
OtherPane == /\ ~otherPaneLive /\ otherPaneLive' = TRUE
             /\ UNCHANGED <<phase,claim,ready,boots,evidence,consumed,sourcePresent,
                             presence,nonce,binding,changed,retired,metadataReadable,ioSeen,orphan>>
Retire == /\ ~retired /\ phase = "held" /\ Case # "orphan" /\ retired' = TRUE
          /\ phase' = "ended" /\ claim' = FALSE
          /\ UNCHANGED <<ready,boots,evidence,consumed,sourcePresent,presence,
                          nonce,binding,otherPaneLive,changed,metadataReadable,ioSeen,orphan>>
ReadFailure == /\ Case = "metadata" /\ ~ioSeen /\ phase = "held" /\ evidence > consumed
               /\ metadataReadable' = FALSE /\ ioSeen' = TRUE
               /\ UNCHANGED <<phase,claim,ready,boots,evidence,consumed,sourcePresent,
                               presence,nonce,binding,otherPaneLive,changed,retired,orphan>>
ReadRecovery == /\ ~metadataReadable /\ metadataReadable' = TRUE
                /\ UNCHANGED <<phase,claim,ready,boots,evidence,consumed,sourcePresent,
                                presence,nonce,binding,otherPaneLive,changed,retired,ioSeen,orphan>>
RetireOrphan == /\ Case = "orphan" /\ RetireMarkerLoss
                /\ phase = "held" /\ orphan = "bound" /\ orphan' = "retired"
                /\ UNCHANGED <<phase,claim,ready,boots,evidence,consumed,sourcePresent,
                                presence,nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen>>
PresenceRecovery == /\ orphan = "retired" /\ orphan' = "absent"
                    /\ presence' = "absent" /\ evidence' = evidence + 1
                    /\ UNCHANGED <<phase,claim,ready,boots,consumed,sourcePresent,
                                    nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen>>
NormalInput == /\ orphan = "absent" /\ orphan' = "launched"
               /\ nonce' = 1 /\ evidence' = evidence + 1
               /\ UNCHANGED <<phase,claim,ready,boots,consumed,sourcePresent,
                               presence,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen>>
AttachNewSource == /\ orphan = "launched" /\ orphan' = "source"
                   /\ sourcePresent' = TRUE /\ presence' = "present" /\ evidence' = evidence + 1
                   /\ UNCHANGED <<phase,claim,ready,boots,consumed,nonce,binding,
                                   otherPaneLive,changed,retired,metadataReadable,ioSeen>>
Rest == UNCHANGED vars
Next == RetireOrphan \/ PresenceRecovery \/ NormalInput \/ AttachNewSource \/ ReadFailure \/ ReadRecovery \/ Rest \/ Halt \/ Run \/ Hold \/ End \/ Boot \/ Wake \/ NewEvidence \/ OtherPane \/ Retire
Spec == Init /\ [][Next]_vars /\ WF_vars(Halt) /\ WF_vars(Run)
        /\ WF_vars(Hold) /\ WF_vars(End) /\ WF_vars(Boot) /\ WF_vars(Wake)
        /\ WF_vars(ReadRecovery) /\ WF_vars(RetireOrphan) /\ WF_vars(PresenceRecovery)
        /\ WF_vars(NormalInput) /\ WF_vars(AttachNewSource)
NoUnchangedReboot == boots <= evidence + 1
HeldOwnsClaim == phase = "held" => (claim /\ ~ready)
HaltVisible == phase \in {"held", "halted"} => ~ready
RetiredCannotBoot == retired => phase = "ended"
EvidenceResumes == (evidence > consumed /\ ~retired) ~> (consumed = evidence \/ retired)
OrphanRecovers == Case = "orphan" => <>(orphan = "source" /\ phase = "running")
=============================================================================
