---- MODULE ExecutionAuthority ----
\* One O channel's execution view and its writer host under the 2 s owner refresh loop.
\* main: before #7733; fix1: deployed #7733; fix2: resume the same absent permit.
\* nowait is a mutant that skips retirement for every host.
\* Code: runtime_bootstrap/owner_execution.rs refresh, tui_o/channel_policy.rs
\* suspend/resume/install_execution, tui_o/writer/host.rs start_managed, tui_o/ownership.rs.
EXTENDS Naturals

CONSTANTS
    Fix,        \* "main" | "fix1" | "fix2" | "nowait"
    MaxFail,    \* transient PG read failures (timeout, pool, lock timeout); PG answers after them
    MaxChange   \* owner changes in PG

Owners == {"A", "B"}
MaxGate == 1 + MaxFail + MaxChange
Gates == 1..MaxGate

VARIABLES
    pg,       \* "none" (unmanaged) | owner: what a successful read returns
    vk,       \* "none" | "ok" | "err": the channel's execution view
    error,    \* "none" | "temporary" | "changed": why a view is held
    vo, vg,   \* owner and gate of the view's permit; for "err" the retained permit, or "-"/0
    gs,       \* gate id -> "owned" | "uncertain" | "closed"
    ng,       \* gates created so far (each fresh permit gets its own gate)
    hk,       \* lifecycle: "none" | "claim" | live "lease"/"permit" | "stale" claim
    ho, hg,   \* a permit host's owner and gate; hg = 0 for the lease gate
    pc,       \* "idle" | "wait": refresh waits for the host to retire before installing
    pend,     \* the observation the waiting refresh will install
    fails, changes, boot, output, misclassified

vars == <<pg, vk, error, vo, vg, gs, ng, hk, ho, hg, pc, pend, fails, changes, boot, output, misclassified>>

\* run_bot restarts, prepare reads execution authority before either gateway or worker starts.
Init ==
    /\ pg \in {"none", "A"}
    /\ fails = 0 /\ changes = 0 /\ pc = "idle" /\ pend = "none" /\ error = "none"
    /\ vk = "none" /\ vo = "-" /\ vg = 0 /\ ng = 0
    /\ gs = [g \in Gates |-> "owned"]
    /\ hk = "none" /\ ho = "-" /\ hg = 0
    /\ boot = "restart" /\ output = "pending" /\ misclassified = FALSE

\* gateway spawn and worker/standby spawn_execution converge at start_managed.
\* The parts' provider lease gate is used only without a permit; Some(Ok) replaces it.
StartWriter ==
    /\ boot = "prepared"
    /\ hk' = "claim" /\ ho' = vo /\ hg' = vg /\ boot' = "running"
    /\ UNCHANGED <<pg, vk, error, vo, vg, gs, ng, pc, pend, fails, changes, output, misclassified>>

\* The first Discord effect acquires a transaction custody lock even on an unmanaged channel.
FirstPostBegins ==
    /\ boot = "running" /\ output = "pending"
    /\ hk \in {"lease", "permit"}
    /\ IF hg = 0 THEN TRUE ELSE gs[hg] = "owned"
    /\ output' = "held"
    /\ UNCHANGED <<pg, vk, error, vo, vg, gs, ng, hk, ho, hg, pc, pend, fails, changes, boot, misclassified>>

FirstPostCompletes ==
    /\ output = "held" /\ output' = "done"
    /\ UNCHANGED <<pg, vk, error, vo, vg, gs, ng, hk, ho, hg, pc, pend, fails, changes, boot, misclassified>>

Verified == vk = "ok" \/ (vk = "err" /\ vg # 0)

\* install_execution: None removes the view; a new permit gets a fresh gate
\* (OwnershipGate::for_execution). Every caller here has already closed the old gate.
Install(obs) ==
    /\ error' = "none"
    /\ IF obs = "none"
       THEN /\ vk' = "none" /\ vo' = "-" /\ vg' = 0 /\ UNCHANGED <<gs, ng>>
       ELSE /\ ng' = ng + 1 /\ vk' = "ok" /\ vo' = obs /\ vg' = ng + 1
            /\ gs' = [gs EXCEPT ![ng + 1] = "owned"]

Prepare ==
    /\ boot = "restart"
    /\ Install(pg) /\ boot' = "prepared"
    /\ UNCHANGED <<pg, hk, ho, hg, pc, pend, fails, changes, output, misclassified>>

\* A failed read: suspend_execution(temporary). A verified permit is kept and its gate made
\* uncertain; with no prior permit the view is held with nothing retained
\* (channel_policy.rs suspend_execution).
TickFails ==
    /\ boot = "running" /\ pc = "idle" /\ fails < MaxFail /\ output = "held"
    /\ fails' = fails + 1 /\ error' = "temporary"
    /\ IF Verified
       THEN /\ gs' = [gs EXCEPT ![vg] = IF @ = "closed" THEN @ ELSE "uncertain"]
            /\ vk' = "err" /\ UNCHANGED <<vo, vg>>
       ELSE /\ vk' = "err" /\ vo' = "-" /\ vg' = 0 /\ UNCHANGED gs
    /\ UNCHANGED <<pg, ng, hk, ho, hg, pc, pend, changes, boot, output, misclassified>>

\* resume_execution: the same tuple brings back the verified permit unless its gate was closed.
Resumable(obs) == Verified /\ obs = vo /\ gs[vg] # "closed"

\* Should the refresh install now instead of waiting for the host to retire?
\* main: only when no host claims the channel. fix1: also when the host obeys the lease gate,
\* which never retires on an execution close (ownership.rs execution_retired).
SkipWait ==
    \/ hk = "none"
    \/ Fix = "nowait"
    \/ Fix \in {"fix1", "fix2"} /\ hk = "lease"

TickObserves ==
    /\ boot = "running" /\ pc = "idle" /\ output # "held"
    /\ misclassified' = (misclassified \/ (vk # "none" /\ pg = "none" /\ ~Verified /\ Fix # "fix2"))
    /\ LET obs == pg IN
       IF obs # "none" /\ Resumable(obs)
       THEN /\ vk' = "ok" /\ error' = "none" /\ gs' = [gs EXCEPT ![vg] = "owned"]
            /\ UNCHANGED <<vo, vg, ng, pc, pend>>
       ELSE IF Fix = "fix2" /\ obs = "none" /\ ~Verified
       THEN /\ Install(obs) /\ UNCHANGED <<pc, pend>>
       ELSE IF vk # "none"
       \* "execution identity changed": the verified gate closes, then wait for the host.
       THEN LET closed == IF Verified THEN [gs EXCEPT ![vg] = "closed"] ELSE gs IN
            IF SkipWait
            THEN /\ error' = "none"
                 /\ IF obs = "none"
                    THEN /\ vk' = "none" /\ vo' = "-" /\ vg' = 0 /\ gs' = closed
                         /\ UNCHANGED ng
                    ELSE /\ ng' = ng + 1 /\ vk' = "ok" /\ vo' = obs /\ vg' = ng + 1
                         /\ gs' = [closed EXCEPT ![ng + 1] = "owned"]
                 /\ UNCHANGED <<pc, pend>>
            ELSE /\ vk' = "err" /\ error' = "changed" /\ vo' = "-" /\ vg' = 0 /\ gs' = closed
                 /\ pc' = "wait" /\ pend' = obs /\ UNCHANGED ng
       ELSE /\ Install(obs) /\ UNCHANGED <<pc, pend>>
    /\ UNCHANGED <<pg, hk, ho, hg, fails, changes, boot, output>>

\* The bounded 3 s wait sees the claim released and installs.
WaitDone ==
    /\ pc = "wait" /\ hk = "none"
    /\ Install(pend) /\ pc' = "idle" /\ pend' = "none"
    /\ UNCHANGED <<pg, hk, ho, hg, fails, changes, boot, output, misclassified>>

\* The wait times out with the host still claiming the channel: `continue`, view stays held.
WaitTimeout ==
    /\ pc = "wait" /\ hk # "none"
    /\ pc' = "idle" /\ pend' = "none"
    /\ UNCHANGED <<pg, vk, error, vo, vg, gs, ng, hk, ho, hg, fails, changes, boot, output, misclassified>>

\* claim() precedes track(). During this gap hosted_under returns false although a claim exists.
TrackWriter ==
    /\ hk = "claim" /\ hk' = IF hg = 0 THEN "lease" ELSE "permit"
    /\ UNCHANGED <<pg, vk, error, vo, vg, gs, ng, ho, hg, pc, pend, fails, changes, boot, output, misclassified>>

\* An abnormal actor join marks the claim unsettled; its Drop removes Live but keeps hosted.
\* This is a real extra event, NOT an observed fact in the supplied operational sample.
HostAbnormalExit ==
    /\ hk \in {"lease", "permit"} /\ hk' = "stale"
    /\ UNCHANGED <<pg, vk, error, vo, vg, gs, ng, ho, hg, pc, pend, fails, changes, boot, output, misclassified>>

\* A permit host retires once its execution gate is closed (host.rs execution_retired select).
HostRetires ==
    /\ hk = "permit" /\ gs[hg] = "closed"
    /\ hk' = "none" /\ ho' = "-" /\ hg' = 0
    /\ UNCHANGED <<pg, vk, error, vo, vg, gs, ng, pc, pend, fails, changes, boot, output, misclassified>>

\* spawn_execution after each refresh: a channel with a verified view gets a permit host.
SpawnExecution ==
    /\ boot = "running" /\ hk = "none" /\ vk = "ok"
    /\ hk' = "permit" /\ ho' = vo /\ hg' = vg
    /\ UNCHANGED <<pg, vk, error, vo, vg, gs, ng, pc, pend, fails, changes, boot, output, misclassified>>

\* PG owner rows change. A managed channel never becomes unmanaged: its routed rows remain.
PgChanges ==
    /\ boot = "running" /\ changes < MaxChange
    /\ pg' \in IF pg = "none" THEN Owners ELSE Owners \ {pg}
    /\ changes' = changes + 1
    /\ UNCHANGED <<vk, error, vo, vg, gs, ng, hk, ho, hg, pc, pend, fails, boot, output, misclassified>>

Next ==
    \/ TickFails \/ TickObserves \/ WaitDone \/ WaitTimeout
    \/ HostRetires \/ SpawnExecution \/ PgChanges \/ TrackWriter
    \/ Prepare \/ StartWriter \/ FirstPostBegins \/ FirstPostCompletes \/ HostAbnormalExit

\* The 2 s loop keeps running; a closed permit gate retires its host; spawn_execution runs.
\* PG answering eventually is MaxFail: after it every read succeeds.
Fairness ==
    /\ WF_vars(TickObserves) /\ WF_vars(WaitDone) /\ WF_vars(WaitTimeout)
    /\ WF_vars(HostRetires) /\ WF_vars(SpawnExecution) /\ WF_vars(TrackWriter)
    /\ WF_vars(Prepare) /\ WF_vars(StartWriter) /\ WF_vars(FirstPostCompletes)

Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK ==
    /\ ng \in 0..MaxGate
    /\ vk \in {"none", "ok", "err"} /\ hk \in {"none", "claim", "lease", "permit", "stale"}
    /\ error \in {"none", "temporary", "changed"}
    /\ boot \in {"restart", "prepared", "running"}
    /\ output \in {"pending", "held", "done"}
    /\ misclassified \in BOOLEAN

\* Live input is admitted: no held view (cutover claim_for_placement) and a host whose gate is
\* owned accepts it (host.rs accepts).
Admitted ==
    /\ vk \in {"none", "ok"}
    /\ \/ hk = "lease"
       \/ hk = "permit" /\ gs[hg] = "owned"

\* Safety: another owner's permit is installed only after the old permit host stopped.
NoForeignPermitHost ==
    (hk = "permit" /\ vk = "ok") => ho = vo

\* Failed observations never open admission, including the first read after boot.
TemporaryReadHolds == error = "temporary" => (vk = "err" /\ ~Admitted)

\* A successful unmanaged read must never invent an identity change. This safety rule
\* records the wrong classification even when a healthy lease host skips the wait immediately.
SameUnmanagedReadmits == ~misclassified

\* Execution authority must recover after the same identity is successfully read, even when
\* host health independently prevents input. Actual input requires an eventually live host.
IdentityUnchanged == [](changes = 0)
AuthorityResumes == IdentityUnchanged => <>[](vk \in {"none", "ok"})
AdmissionResumes == (IdentityUnchanged /\ [](hk # "stale")) => <>[]Admitted
UnconditionalAdmissionResumes == <>[]Admitted

====
