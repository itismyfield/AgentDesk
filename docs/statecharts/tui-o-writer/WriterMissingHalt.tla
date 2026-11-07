--------------------------- MODULE WriterMissingHalt ---------------------------
EXTENDS Naturals
CONSTANTS HoldMissing, ObserveResume, Case
VARIABLES phase, claim, ready, boots, evidence, consumed, sourcePresent,
          presence, nonce, binding, otherPaneLive, changed, retired,
          metadataReadable, ioSeen
vars == <<phase, claim, ready, boots, evidence, consumed, sourcePresent,
          presence, nonce, binding, otherPaneLive, changed, retired,metadataReadable,ioSeen>>
Init == /\ phase = "booting" /\ claim = TRUE /\ ready = FALSE /\ boots = 1
        /\ evidence = 0 /\ consumed = 0 /\ sourcePresent = FALSE
        /\ presence \in {"unknown", "present"} /\ nonce = 0 /\ binding = 0
        /\ otherPaneLive = FALSE /\ changed = FALSE /\ retired = FALSE
        /\ metadataReadable = TRUE /\ ioSeen = FALSE
Halt == /\ phase = "booting" /\ ~sourcePresent /\ phase' = "halted"
        /\ ready' = FALSE
        /\ UNCHANGED <<claim,boots,evidence,consumed,sourcePresent,presence,
                        nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen>>
Run == /\ phase = "booting" /\ sourcePresent /\ phase' = "running"
       /\ ready' = TRUE /\ consumed' = evidence
       /\ UNCHANGED <<claim,boots,evidence,sourcePresent,presence,
                       nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen>>
Hold == /\ phase = "halted" /\ HoldMissing /\ phase' = "held"
        /\ UNCHANGED <<claim,ready,boots,evidence,consumed,sourcePresent,presence,
                        nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen>>
End == /\ phase = "halted" /\ ~HoldMissing
       /\ phase' = "ended" /\ claim' = FALSE
       /\ UNCHANGED <<ready,boots,evidence,consumed,sourcePresent,presence,
                       nonce,binding,otherPaneLive,changed,retired,metadataReadable,ioSeen>>
Boot == /\ phase = "ended" /\ ~claim /\ ~retired /\ boots < 3
        /\ phase' = "booting" /\ claim' = TRUE /\ boots' = boots + 1
        /\ consumed' = evidence
        /\ UNCHANGED <<ready,evidence,sourcePresent,presence,nonce,binding,
                        otherPaneLive,changed,retired,metadataReadable,ioSeen>>
Wake == /\ phase = "held" /\ ObserveResume /\ evidence > consumed /\ ~retired
        /\ metadataReadable
        /\ phase' = "booting" /\ boots' = boots + 1 /\ consumed' = evidence
        /\ UNCHANGED <<claim,ready,evidence,sourcePresent,presence,nonce,binding,
                        otherPaneLive,changed,retired,metadataReadable,ioSeen>>
NewEvidence == /\ ~changed /\ Case # "stable" /\ ~retired
               /\ (Case # "early" \/ phase \in {"booting", "halted"})
               /\ evidence' = evidence + 1 /\ changed' = TRUE
               /\ sourcePresent' = (Case = "source" \/ Case = "early")
               /\ presence' = IF Case \in {"presence", "metadata"} THEN "absent" ELSE presence
               /\ nonce' = IF Case = "nonce" THEN 1 ELSE nonce
               /\ binding' = IF Case = "binding" THEN 1 ELSE binding
               /\ UNCHANGED <<phase,claim,ready,boots,consumed,otherPaneLive,retired,metadataReadable,ioSeen>>
OtherPane == /\ ~otherPaneLive /\ otherPaneLive' = TRUE
             /\ UNCHANGED <<phase,claim,ready,boots,evidence,consumed,sourcePresent,
                             presence,nonce,binding,changed,retired,metadataReadable,ioSeen>>
Retire == /\ ~retired /\ phase = "held" /\ retired' = TRUE
          /\ phase' = "ended" /\ claim' = FALSE
          /\ UNCHANGED <<ready,boots,evidence,consumed,sourcePresent,presence,
                          nonce,binding,otherPaneLive,changed,metadataReadable,ioSeen>>
ReadFailure == /\ Case = "metadata" /\ ~ioSeen /\ phase = "held" /\ evidence > consumed
               /\ metadataReadable' = FALSE /\ ioSeen' = TRUE
               /\ UNCHANGED <<phase,claim,ready,boots,evidence,consumed,sourcePresent,
                               presence,nonce,binding,otherPaneLive,changed,retired>>
ReadRecovery == /\ ~metadataReadable /\ metadataReadable' = TRUE
                /\ UNCHANGED <<phase,claim,ready,boots,evidence,consumed,sourcePresent,
                                presence,nonce,binding,otherPaneLive,changed,retired,ioSeen>>
Rest == UNCHANGED vars
Next == ReadFailure \/ ReadRecovery \/ Rest \/ Halt \/ Run \/ Hold \/ End \/ Boot \/ Wake \/ NewEvidence \/ OtherPane \/ Retire
Spec == Init /\ [][Next]_vars /\ WF_vars(Halt) /\ WF_vars(Run)
        /\ WF_vars(Hold) /\ WF_vars(End) /\ WF_vars(Boot) /\ WF_vars(Wake)
        /\ WF_vars(ReadRecovery)
NoUnchangedReboot == boots <= evidence + 1
HeldOwnsClaim == phase = "held" => (claim /\ ~ready)
HaltVisible == phase \in {"held", "halted"} => ~ready
RetiredCannotBoot == retired => phase = "ended"
EvidenceResumes == (evidence > consumed /\ ~retired) ~> (consumed = evidence \/ retired)
=============================================================================
