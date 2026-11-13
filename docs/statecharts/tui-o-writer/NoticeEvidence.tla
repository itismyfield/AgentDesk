--------------------------- MODULE NoticeEvidence ---------------------------
EXTENDS WriterWake, FiniteSets
CONSTANTS NoticeMode, InputClass, EvidenceReturns, SessionBoundary
VARIABLE n
all == <<bootAll, n>>

\* One retained record; classes and evidence availability come from the entrypoint table.
NoticeInit == n = [phase |-> "absent", proof |-> "unknown", closed |-> FALSE,
    reason |-> "authority_busy", logged |-> FALSE, logs |-> 0, bootLogs |-> 0,
    posts |-> 0, prepared |-> FALSE, effect |-> FALSE, crashed |-> FALSE,
    savedTerminal |-> FALSE]
Terminal == n.phase \in {"posted", "excluded", "expired"}
Exempt == n.proof \in {"owned", "policy"}
ParentHold == RouteOwnerHold /\ UNCHANGED n
ParentRecheck == RouteSelfWakeRecheck /\ UNCHANGED n
ParentOpens == RouteGatewayOpens /\ UNCHANGED n
ParentWake == RouteExistingOwnerWake /\ UNCHANGED n
ParentExecute == RouteExecute /\ UNCHANGED n

Materialize == /\ n.phase = "absent" /\ bootReady
               /\ n' = [n EXCEPT !.phase = "pending"]
               /\ UNCHANGED bootAll
\* Current's mismatched registration cannot supply the new source/UUID-bound proof.
RecoverProof == /\ n.phase = "pending" /\ n.proof = "unknown" /\ EvidenceReturns
                /\ (NoticeMode # "before" \/ InputClass = "direct")
                /\ n' = [n EXCEPT !.proof = InputClass]
                /\ UNCHANGED bootAll
CloseSession == /\ SessionBoundary /\ n.phase # "absent" /\ ~n.closed
                /\ n' = [n EXCEPT !.closed = TRUE]
                /\ UNCHANGED bootAll
Defer == /\ n.phase = "pending"
         /\ \E reason \in {"authority_busy", "native_census_unavailable",
                            "registration_source_mismatch", "history_snapshot_changed"} :
            n' = [n EXCEPT !.reason = reason, !.logged = TRUE,
                !.logs = IF ~n.logged \/
                            (NoticeMode = "reason-log" /\ reason # n.reason)
                         THEN Min2(@ + 1) ELSE @,
                !.bootLogs = IF ~n.logged \/
                            (NoticeMode = "reason-log" /\ reason # n.reason)
                         THEN Min2(@ + 1) ELSE @]
         /\ UNCHANGED bootAll
Settle == /\ n.phase = "pending" /\ Exempt /\ ~n.prepared
          /\ NoticeMode \notin {"before", "no-settle"}
          /\ (NoticeMode # "wait-boundary" \/ n.closed)
          /\ n' = [n EXCEPT !.phase = IF n.closed THEN "expired" ELSE "excluded"]
          /\ UNCHANGED bootAll
\* The mutant treats a session boundary as sufficient to discard an unknown direct input.
ExpireUnproven == /\ NoticeMode = "expire-unproven" /\ n.phase = "pending"
                  /\ n.closed /\ n.proof = "unknown" /\ ~n.prepared
                  /\ n' = [n EXCEPT !.phase = "expired"]
                  /\ UNCHANGED bootAll
Prepare == /\ n.phase = "pending" /\ n.proof = "direct" /\ ~n.prepared
           /\ bootReady /\ routeGateway
           /\ n' = [n EXCEPT !.prepared = TRUE]
           /\ UNCHANGED bootAll
\* Existing Prepared/history confirmation is abstracted; an uncertain effect is never retried blind.
Post == /\ n.prepared /\ ~n.effect /\ n.phase = "pending"
        /\ n' = [n EXCEPT !.effect = TRUE, !.posts = @ + 1]
        /\ UNCHANGED bootAll
Confirm == /\ n.prepared /\ n.effect /\ n.phase = "pending"
           /\ n' = [n EXCEPT !.phase = "posted"]
           /\ UNCHANGED bootAll
Crash == /\ n.phase # "absent" /\ ~n.crashed
         /\ n' = [n EXCEPT !.crashed = TRUE, !.savedTerminal = Terminal,
             !.phase = IF NoticeMode = "drop-replay" /\ Terminal THEN "pending" ELSE @,
             !.logged = FALSE, !.bootLogs = 0]
         /\ UNCHANGED bootAll

NoticeNext == ParentHold \/ ParentRecheck \/ ParentOpens \/ ParentWake \/ ParentExecute
              \/ Materialize \/ RecoverProof \/ CloseSession \/ Defer \/ Settle
              \/ ExpireUnproven \/ Prepare \/ Post \/ Confirm \/ Crash
NoticeSpec == RouteInit /\ NoticeInit /\ [][NoticeNext]_all
    /\ WF_all(ParentHold) /\ WF_all(ParentRecheck) /\ WF_all(ParentOpens)
    /\ WF_all(ParentWake) /\ WF_all(ParentExecute)
    /\ WF_all(Materialize) /\ WF_all(RecoverProof)
    /\ WF_all(Settle) /\ WF_all(Prepare) /\ WF_all(Post) /\ WF_all(Confirm)

NoticeType == n.phase \in {"absent", "pending", "excluded", "expired", "posted"}
              /\ n.proof \in {"unknown", "owned", "policy", "direct"}
              /\ n.posts \in 0..1 /\ n.logs \in 0..2
NoticeOnce == n.posts <= 1 /\ (n.phase = "posted" => n.posts = 1)
NoQuietLoss == n.phase \in {"excluded", "expired"} => Exempt
NoExpireOwedDirect == InputClass = "direct" => n.phase \notin {"excluded", "expired"}
NoPostExempt == InputClass # "direct" => n.posts = 0
PreparedKept == n.prepared => n.phase \in {"pending", "posted"}
ReplayKeepsTerminal == n.savedTerminal => Terminal
PendingLoggedOnce == n.bootLogs <= 1 /\ n.logs <= 2
EveryPendingTerminates == (n.phase = "pending") ~> Terminal
DirectEventuallyPosted == (n.phase = "pending" /\ InputClass = "direct") ~> (n.phase = "posted")
BoundaryHappens == <>n.closed
BoundarySpec == NoticeSpec /\ WF_all(CloseSession)
=============================================================================
