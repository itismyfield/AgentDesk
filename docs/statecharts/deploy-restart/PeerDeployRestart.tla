------------------------- MODULE PeerDeployRestart -------------------------
\* A non-preferred peer that just restarted itself, and one cluster deploy request to it.
\* Mode "main": the peer's 90s preferred-gateway yield wait spawns no marker poller and ignores
\* restart_pending. "fixed": the wait stands down to standby when a request is pending.
\* "anyproof" (mutant of fixed): the deploy accepts any persisted proof, not its own nonce's.
EXTENDS Naturals
CONSTANTS Mode, Y, B, MaxSelf
VARIABLES peer, yieldLeft, marker, proof, dphase, budget, selfLeft, binary
vars == <<peer, yieldLeft, marker, proof, dphase, budget, selfLeft, binary>>

\* peer: yield = boot wait before the lease outcome (no poller); up = standby or serving with
\* its marker poller; exiting = quick exit after publishing restart_persisted.<nonce>.
\* marker: owner of restart_pending (deploy nonce, or a UUID from promotion/self-fence).
Accept == IF Mode = "anyproof" THEN proof # {} ELSE "deploy" \in proof

Init == /\ peer = "yield" /\ yieldLeft = Y
        /\ marker = "none" /\ proof = {}
        /\ dphase = "idle" /\ budget = 0
        /\ selfLeft = MaxSelf /\ binary = "old"

\* Deploy phase 1 creates the marker only when no other request holds it.
DeployRequest == /\ dphase = "idle" /\ marker = "none"
                 /\ marker' = "deploy" /\ dphase' = "drain" /\ budget' = B
                 /\ UNCHANGED <<peer, yieldLeft, proof, selfLeft, binary>>
\* Standby promotion or a self-fence handback publishes its own UUID request.
SelfRequest == /\ peer = "up" /\ marker = "none" /\ selfLeft > 0
               /\ marker' = "self" /\ selfLeft' = selfLeft - 1
               /\ UNCHANGED <<peer, yieldLeft, proof, dphase, budget, binary>>
YieldEnds == /\ peer = "yield" /\ yieldLeft = 0 /\ peer' = "up"
             /\ UNCHANGED <<yieldLeft, marker, proof, dphase, budget, selfLeft, binary>>
YieldStandsDown == /\ Mode # "main" /\ peer = "yield" /\ marker # "none" /\ peer' = "up"
                   /\ UNCHANGED <<yieldLeft, marker, proof, dphase, budget, selfLeft, binary>>
\* The final provider's poller publishes the proof for the nonce it read, disposes it, exits.
PollerConsumes == /\ peer = "up" /\ marker # "none"
                  /\ proof' = proof \cup {marker} /\ marker' = "none" /\ peer' = "exiting"
                  /\ UNCHANGED <<yieldLeft, dphase, budget, selfLeft, binary>>
\* launchd brings the same installed binary back; the deploy owns the binary swap.
Relaunch == /\ peer = "exiting" /\ dphase # "acked"
            /\ peer' = "yield" /\ yieldLeft' = Y
            /\ UNCHANGED <<marker, proof, dphase, budget, selfLeft, binary>>
Ack == /\ dphase = "drain" /\ Accept /\ dphase' = "acked"
       /\ UNCHANGED <<peer, yieldLeft, marker, proof, budget, selfLeft, binary>>
\* The 20s phase-1 timeout disposes only its own marker; phase 2 can then never see a proof.
Timeout == /\ dphase = "drain" /\ budget = 0 /\ ~Accept
           /\ dphase' = "failed" /\ marker' = IF marker = "deploy" THEN "none" ELSE marker
           /\ UNCHANGED <<peer, yieldLeft, proof, budget, selfLeft, binary>>
\* Bootout and bootstrap of the staged binary after this request's own proof.
Swap == /\ dphase = "acked"
        /\ dphase' = "done" /\ peer' = "yield" /\ yieldLeft' = Y /\ binary' = "new"
        /\ UNCHANGED <<marker, proof, budget, selfLeft>>

Urgent == ENABLED (YieldEnds \/ YieldStandsDown \/ PollerConsumes \/ Relaunch \/ Ack \/ Timeout \/ Swap)
\* One tick (~10s) of the yield wait or the deploy budget passes when nothing faster is due.
Tick == /\ ~Urgent
        /\ \/ dphase = "drain" /\ budget > 0
           \/ peer = "yield" /\ yieldLeft > 0
        /\ budget' = IF dphase = "drain" /\ budget > 0 THEN budget - 1 ELSE budget
        /\ yieldLeft' = IF peer = "yield" /\ yieldLeft > 0 THEN yieldLeft - 1 ELSE yieldLeft
        /\ UNCHANGED <<peer, marker, proof, dphase, selfLeft, binary>>

Settled == ~ENABLED (DeployRequest \/ SelfRequest \/ Tick) /\ ~Urgent /\ UNCHANGED vars

Next == DeployRequest \/ SelfRequest \/ YieldEnds \/ YieldStandsDown \/ PollerConsumes
        \/ Relaunch \/ Ack \/ Timeout \/ Swap \/ Tick \/ Settled
Spec == Init /\ [][Next]_vars
        /\ WF_vars(YieldEnds) /\ WF_vars(YieldStandsDown) /\ WF_vars(PollerConsumes)
        /\ WF_vars(Relaunch) /\ WF_vars(Ack) /\ WF_vars(Timeout) /\ WF_vars(Swap) /\ WF_vars(Tick)

\* The deploy counts only the proof its own nonce produced.
AckRequiresOwnProof == dphase \in {"acked", "done"} => "deploy" \in proof
\* A finished deploy left the peer on the staged binary.
DoneRunsNewBinary == dphase = "done" => binary = "new"
\* An issued deploy request is acknowledged, whatever restart the peer was in.
DeployConfirmed == dphase = "drain" ~> dphase = "acked"
\* Restarts stop: eventually no request is pending and the peer is not exiting.
RestartsSettle == <>[](marker = "none" /\ peer # "exiting")
=============================================================================
