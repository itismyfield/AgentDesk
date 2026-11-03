---- MODULE ExecutionAuthority ----
\* One O channel's execution view and its writer host under the 2 s owner refresh loop.
\* AgentDesk main b66f40ba190f73d78ac1b5aa9e8f41a719af37e5 (Fix = "main") and the #7731 change
\* (Fix = "fix1"). Fix = "nowait" is a mutant that skips the retirement wait for every host.
\* Code: runtime_bootstrap/owner_execution.rs refresh, tui_o/channel_policy.rs
\* suspend/resume/install_execution, tui_o/writer/host.rs start_managed, tui_o/ownership.rs.
EXTENDS Naturals

CONSTANTS
    Fix,        \* "main" | "fix1" | "nowait"
    MaxFail,    \* transient PG read failures (timeout, pool, lock timeout); PG answers after them
    MaxChange   \* owner changes in PG

Owners == {"A", "B"}
MaxGate == 1 + MaxFail + MaxChange
Gates == 1..MaxGate

VARIABLES
    pg,       \* "none" (unmanaged) | owner: what a successful read returns
    vk,       \* "none" | "ok" | "err": the channel's execution view
    vo, vg,   \* owner and gate of the view's permit; for "err" the retained permit, or "-"/0
    gs,       \* gate id -> "owned" | "uncertain" | "closed"
    ng,       \* gates created so far (each fresh permit gets its own gate)
    hk,       \* "none" | "lease" | "permit": the running writer host and the gate it obeys
    ho, hg,   \* a permit host's owner and gate; hg = 0 for the lease gate
    pc,       \* "idle" | "wait": refresh waits for the host to retire before installing
    pend,     \* the observation the waiting refresh will install
    fails, changes

vars == <<pg, vk, vo, vg, gs, ng, hk, ho, hg, pc, pend, fails, changes>>

\* Gateway boot of an unmanaged channel hosts it under the provider lease gate
\* (host.rs start_managed: no execution permit, pg_gateway). A boot of a managed channel hosts it
\* under its permit's gate (prepare installs the permit first).
Init ==
    /\ fails = 0 /\ changes = 0 /\ pc = "idle" /\ pend = "none"
    /\ \/ /\ pg = "none" /\ vk = "none" /\ vo = "-" /\ vg = 0
          /\ gs = [g \in Gates |-> "owned"] /\ ng = 0
          /\ hk = "lease" /\ ho = "-" /\ hg = 0
       \/ /\ pg = "A" /\ vk = "ok" /\ vo = "A" /\ vg = 1
          /\ gs = [g \in Gates |-> "owned"] /\ ng = 1
          /\ hk = "permit" /\ ho = "A" /\ hg = 1

Verified == vk = "ok" \/ (vk = "err" /\ vg # 0)

\* install_execution: None removes the view; a new permit gets a fresh gate
\* (OwnershipGate::for_execution). Every caller here has already closed the old gate.
Install(obs) ==
    IF obs = "none"
    THEN /\ vk' = "none" /\ vo' = "-" /\ vg' = 0 /\ UNCHANGED <<gs, ng>>
    ELSE /\ ng' = ng + 1 /\ vk' = "ok" /\ vo' = obs /\ vg' = ng + 1
         /\ gs' = [gs EXCEPT ![ng + 1] = "owned"]

\* A failed read: suspend_execution(temporary). A verified permit is kept and its gate made
\* uncertain; with no prior permit the view is held with nothing retained
\* (channel_policy.rs suspend_execution).
TickFails ==
    /\ pc = "idle" /\ fails < MaxFail
    /\ fails' = fails + 1
    /\ IF Verified
       THEN /\ gs' = [gs EXCEPT ![vg] = IF @ = "closed" THEN @ ELSE "uncertain"]
            /\ vk' = "err" /\ UNCHANGED <<vo, vg>>
       ELSE /\ vk' = "err" /\ vo' = "-" /\ vg' = 0 /\ UNCHANGED gs
    /\ UNCHANGED <<pg, ng, hk, ho, hg, pc, pend, changes>>

\* resume_execution: the same tuple brings back the verified permit unless its gate was closed.
Resumable(obs) == Verified /\ obs = vo /\ gs[vg] # "closed"

\* Should the refresh install now instead of waiting for the host to retire?
\* main: only when no host claims the channel. fix1: also when the host obeys the lease gate,
\* which never retires on an execution close (ownership.rs execution_retired).
SkipWait ==
    \/ hk = "none"
    \/ Fix = "nowait"
    \/ Fix = "fix1" /\ hk = "lease"

TickObserves ==
    /\ pc = "idle"
    /\ LET obs == pg IN
       IF obs # "none" /\ Resumable(obs)
       THEN /\ vk' = "ok" /\ gs' = [gs EXCEPT ![vg] = "owned"]
            /\ UNCHANGED <<vo, vg, ng, pc, pend>>
       ELSE IF vk # "none"
       \* "execution identity changed": the verified gate closes, then wait for the host.
       THEN LET closed == IF Verified THEN [gs EXCEPT ![vg] = "closed"] ELSE gs IN
            IF SkipWait
            THEN /\ IF obs = "none"
                    THEN /\ vk' = "none" /\ vo' = "-" /\ vg' = 0 /\ gs' = closed
                         /\ UNCHANGED ng
                    ELSE /\ ng' = ng + 1 /\ vk' = "ok" /\ vo' = obs /\ vg' = ng + 1
                         /\ gs' = [closed EXCEPT ![ng + 1] = "owned"]
                 /\ UNCHANGED <<pc, pend>>
            ELSE /\ vk' = "err" /\ vo' = "-" /\ vg' = 0 /\ gs' = closed
                 /\ pc' = "wait" /\ pend' = obs /\ UNCHANGED ng
       ELSE /\ Install(obs) /\ UNCHANGED <<pc, pend>>
    /\ UNCHANGED <<pg, hk, ho, hg, fails, changes>>

\* The bounded 3 s wait sees the claim released and installs.
WaitDone ==
    /\ pc = "wait" /\ hk = "none"
    /\ Install(pend) /\ pc' = "idle" /\ pend' = "none"
    /\ UNCHANGED <<pg, hk, ho, hg, fails, changes>>

\* The wait times out with the host still claiming the channel: `continue`, view stays held.
WaitTimeout ==
    /\ pc = "wait" /\ hk # "none"
    /\ pc' = "idle" /\ pend' = "none"
    /\ UNCHANGED <<pg, vk, vo, vg, gs, ng, hk, ho, hg, fails, changes>>

\* A permit host retires once its execution gate is closed (host.rs execution_retired select).
HostRetires ==
    /\ hk = "permit" /\ gs[hg] = "closed"
    /\ hk' = "none" /\ ho' = "-" /\ hg' = 0
    /\ UNCHANGED <<pg, vk, vo, vg, gs, ng, pc, pend, fails, changes>>

\* spawn_execution after each refresh: a channel with a verified view gets a permit host.
SpawnExecution ==
    /\ hk = "none" /\ vk = "ok"
    /\ hk' = "permit" /\ ho' = vo /\ hg' = vg
    /\ UNCHANGED <<pg, vk, vo, vg, gs, ng, pc, pend, fails, changes>>

\* PG owner rows change. A managed channel never becomes unmanaged: its routed rows remain.
PgChanges ==
    /\ changes < MaxChange
    /\ pg' \in IF pg = "none" THEN Owners ELSE Owners \ {pg}
    /\ changes' = changes + 1
    /\ UNCHANGED <<vk, vo, vg, gs, ng, hk, ho, hg, pc, pend, fails>>

Next ==
    \/ TickFails \/ TickObserves \/ WaitDone \/ WaitTimeout
    \/ HostRetires \/ SpawnExecution \/ PgChanges

\* The 2 s loop keeps running; a closed permit gate retires its host; spawn_execution runs.
\* PG answering eventually is MaxFail: after it every read succeeds.
Fairness ==
    /\ WF_vars(TickObserves) /\ WF_vars(WaitDone) /\ WF_vars(WaitTimeout)
    /\ WF_vars(HostRetires) /\ WF_vars(SpawnExecution)

Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK ==
    /\ ng \in 0..MaxGate
    /\ vk \in {"none", "ok", "err"} /\ hk \in {"none", "lease", "permit"}

\* Live input is admitted: no held view (cutover claim_for_placement) and a host whose gate is
\* owned accepts it (host.rs accepts).
Admitted ==
    /\ vk \in {"none", "ok"}
    /\ \/ hk = "lease"
       \/ hk = "permit" /\ gs[hg] = "owned"

\* Safety: another owner's permit is installed only after the old permit host stopped.
NoForeignPermitHost ==
    (hk = "permit" /\ vk = "ok") => ho = vo

\* Liveness: once PG answers, admission resumes and stays.
AdmissionResumes == <>[]Admitted

====
