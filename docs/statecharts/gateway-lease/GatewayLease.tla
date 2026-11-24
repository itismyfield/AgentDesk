---------------------------- MODULE GatewayLease ----------------------------
\* #8351: the Claude Discord gateway lease between the preferred node P (tui_o home)
\* and a non-preferred node N (tui_o Foreign). One provider, one advisory lock.
\* blocker = "busy":    boot restore is held (observed: "native control history busy").
\* blocker = "refused": preparation or the boot reaper refuses (not observed on 11-23).
\* Mode = "abandon": P's Claude runtime returns before the lease phase on either
\*        (runtime_bootstrap.rs:299-301 at b012692), and never retries.
\* Mode = "retry":   a held restore is retried every 5s until it passes, then P enters
\*        preferred-poll (#8351 round 2, native_input.rs await_native_boot); a refusal
\*        still ends the runtime. Refusable = FALSE limits P's restarts to busy holds.
\* Mode = "noyield": like "retry", but N's keepalive never hands the lease back
\*        (mutation: shows the keepalive yield is the only return path while N serves).
EXTENDS Naturals
CONSTANTS Mode, MaxRestarts, Refusable
VARIABLES lease, pRt, nRt, blocker, queued, pRestarts, nRestarts
vars == <<lease, pRt, nRt, blocker, queued, pRestarts, nRestarts>>

PStates == {"down", "booting", "dead", "waiting", "serving"}
NStates == {"down", "booting", "yielding", "serving", "standby"}

TypeOK == /\ lease \in {"free", "P", "N"} /\ pRt \in PStates /\ nRt \in NStates
          /\ blocker \in {"none", "busy", "refused"} /\ queued \in BOOLEAN

Init == /\ lease = "P" /\ pRt = "serving" /\ nRt = "standby"
        /\ blocker = "none" /\ queued = FALSE /\ pRestarts = 0 /\ nRestarts = 0

\* P advertises `discord_gateway.waiting_providers` only from acquire_as_preferred_gateway.
PWaiting == pRt = "waiting"

\* ---- preferred node P ----
\* Restart (deploy, restart skill). It may leave a native record whose boot restore refuses.
PRestart == /\ pRestarts < MaxRestarts /\ pRt # "down"
            /\ pRt' = "down" /\ pRestarts' = pRestarts + 1
            /\ lease' = IF lease = "P" THEN "free" ELSE lease
            /\ blocker' \in IF Refusable THEN {"none", "busy", "refused"} ELSE {"none", "busy"}
            /\ UNCHANGED <<nRt, queued, nRestarts>>
PBoot == /\ pRt = "down" /\ pRt' = "booting"
         /\ UNCHANGED <<lease, nRt, blocker, queued, pRestarts, nRestarts>>
\* prepare_native_boot returns true: run_bot reaches run_bot_acquire_gateway_lease.
PPrepare == /\ pRt = "booting" /\ blocker = "none" /\ pRt' = "waiting"
            /\ UNCHANGED <<lease, nRt, blocker, queued, pRestarts, nRestarts>>
\* prepare_native_boot returns false: run_bot returns with no lease line and no waiter.
\* In "retry" only a refusal gets here; a busy hold stays "booting" and is retried.
PRefuse == /\ pRt = "booting"
           /\ IF Mode = "abandon" THEN blocker # "none" ELSE blocker = "refused"
           /\ pRt' = "dead"
           /\ UNCHANGED <<lease, nRt, blocker, queued, pRestarts, nRestarts>>
\* preferred-poll: try the lock every 5s while advertising the waiter.
PAcquire == /\ PWaiting /\ lease = "free" /\ lease' = "P" /\ pRt' = "serving"
            /\ UNCHANGED <<nRt, blocker, queued, pRestarts, nRestarts>>

\* The history lock holder finishes; a refusal never clears inside the process.
ClearBlocker == /\ blocker = "busy" /\ blocker' = "none"
                /\ UNCHANGED <<lease, pRt, nRt, queued, pRestarts, nRestarts>>

\* ---- non-preferred node N ----
NRestart == /\ nRestarts < MaxRestarts /\ nRt # "down"
            /\ nRt' = "down" /\ nRestarts' = nRestarts + 1
            /\ lease' = IF lease = "N" THEN "free" ELSE lease
            /\ UNCHANGED <<pRt, blocker, queued, pRestarts>>
NBoot == /\ nRt = "down" /\ nRt' = "booting"
         /\ UNCHANGED <<lease, pRt, blocker, queued, pRestarts, nRestarts>>
\* yield_to_preferred_gateway: stand by only when P advertises the waiter.
NStart == /\ nRt = "booting"
          /\ IF PWaiting THEN nRt' = "yielding" /\ UNCHANGED lease
             ELSE IF lease = "free" THEN nRt' = "serving" /\ lease' = "N"
             ELSE nRt' = "standby" /\ UNCHANGED lease
          /\ UNCHANGED <<pRt, blocker, queued, pRestarts, nRestarts>>
\* Yield wait ends (P took it, stopped waiting, or the 90s grace ran out).
NYieldEnd == /\ nRt = "yielding"
             /\ IF lease = "free" THEN nRt' = "serving" /\ lease' = "N"
                ELSE nRt' = "standby" /\ UNCHANGED lease
             /\ UNCHANGED <<pRt, blocker, queued, pRestarts, nRestarts>>
\* spawn_standby_gateway_retry: a standby takes a freed lock.
NStandbyTake == /\ nRt = "standby" /\ lease = "free" /\ nRt' = "serving" /\ lease' = "N"
                /\ UNCHANGED <<pRt, blocker, queued, pRestarts, nRestarts>>
\* Keepalive yield (gateway_lease.rs:787-819): unlock, self-fence, launchd brings N back.
NKeepaliveYield == /\ Mode # "noyield" /\ nRt = "serving" /\ lease = "N" /\ PWaiting
                   /\ lease' = "free" /\ nRt' = "down"
                   /\ UNCHANGED <<pRt, blocker, queued, pRestarts, nRestarts>>

\* ---- an O-channel input ----
Arrive == /\ ~queued /\ queued' = TRUE
          /\ UNCHANGED <<lease, pRt, nRt, blocker, pRestarts, nRestarts>>
\* Only the O home can start the turn; on N (Foreign) the input stays queued.
Deliver == /\ queued /\ lease = "P" /\ pRt = "serving" /\ queued' = FALSE
           /\ UNCHANGED <<lease, pRt, nRt, blocker, pRestarts, nRestarts>>

Next == \/ PRestart \/ PBoot \/ PPrepare \/ PRefuse \/ PAcquire \/ ClearBlocker
        \/ NRestart \/ NBoot \/ NStart \/ NYieldEnd \/ NStandbyTake \/ NKeepaliveYield
        \/ Arrive \/ Deliver

\* PAcquire is strong-fair: the lock is free only between N's unlock and N's own retry.
Spec == Init /\ [][Next]_vars
        /\ WF_vars(PBoot) /\ WF_vars(PPrepare) /\ WF_vars(PRefuse) /\ SF_vars(PAcquire)
        /\ WF_vars(ClearBlocker) /\ WF_vars(NBoot) /\ WF_vars(NStart) /\ WF_vars(NYieldEnd)
        /\ WF_vars(NStandbyTake) /\ WF_vars(NKeepaliveYield) /\ WF_vars(Deliver)

\* ---- rules ----
LeaseMatchesRuntime == /\ (lease = "P") <=> (pRt = "serving")
                       /\ (lease = "N") <=> (nRt = "serving")

\* While the preferred node is online, the lease ends up and stays on it.
LeaseReturnsToPreferred == <>[](lease = "P")
\* An input queued on the Foreign holder does not stay queued forever.
ForeignQueueDrains == [](queued => <>~queued)
=============================================================================
