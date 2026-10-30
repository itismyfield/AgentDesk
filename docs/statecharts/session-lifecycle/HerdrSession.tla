---- MODULE HerdrSession ----
\* One Discord channel's Claude execution on a Herdr pane (HostKind::Herdr), across one dcserver
\* crash. AgentDesk main e8d94425a04867279ced4ec967e9d93df0d7ec0a.
\* `rec` is the channel's hosted_execution record (Postgres); pane ids are not derived from the
\* channel, so a pane nobody recorded cannot be found again. Each action cites its code.
EXTENDS Naturals

VARIABLES
    srv,      \* "up" | "down"
    rec,      \* "legacy" | "pending" (no pane) | "placed" (pane, no evidence) | "evidenced" | "bound" | "retired"
    pane,     \* the recorded pane: "none" | "agent" (provider runs) | "shell" (provider exited) | "closed"
    fresh,    \* a pane this launch created and holds only in memory
    stray,    \* panes that may run an agent and that no record names
    launch,   \* in-memory launch: "idle" | "creating" | "placing" | "evidence"
    input,    \* "idle" | "pinned": the input gate judged the pane; the send follows
    landed,   \* "none" | "agent" | "shell": where the prompt of the step just taken landed
    closing,  \* operator closed the pane; the row is not retired yet
    crashes, lost, exits

vars == <<srv, rec, pane, fresh, stray, launch, input, landed, closing, crashes, lost, exits>>

Init ==
    /\ srv = "up" /\ rec = "legacy" /\ pane = "none" /\ fresh = FALSE /\ stray = 0
    /\ launch = "idle" /\ input = "idle" /\ landed = "none" /\ closing = FALSE
    /\ crashes = 0 /\ lost = 0 /\ exits = 0

\* Durable Pending first, by CAS over a Legacy or Retired row only; a Pending, Bound or unreadable
\* row refuses the launch as Occupied (herdr_launch.rs:374-421, hosted_execution.rs:455-475).
\* A retired predecessor's pane is no longer named by the row.
StartLaunch ==
    /\ srv = "up" /\ launch = "idle" /\ input = "idle" /\ ~closing
    /\ rec \in {"legacy", "retired"}
    /\ rec' = "pending" /\ launch' = "creating" /\ pane' = "none"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, fresh, stray, input, closing, crashes, lost, exits>>

\* The create reply names the new pane (herdr_launch.rs:453-456).
CreateOk ==
    /\ launch = "creating"
    /\ fresh' = TRUE /\ launch' = "placing"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, rec, pane, stray, input, closing, crashes, lost, exits>>

\* The server may have acted with no usable reply: Pending stays, nothing is resent, adopted or
\* killed (herdr_launch.rs:187-191, 471-472).
CreateLost ==
    /\ launch = "creating" /\ lost < 1
    /\ stray' \in {stray, stray + 1} /\ launch' = "idle" /\ lost' = lost + 1
    /\ landed' = "none"
    /\ UNCHANGED <<srv, rec, pane, fresh, input, closing, crashes, exits>>

\* The request never left: no pane, and the launch retires its own Pending (herdr_launch.rs:467-469, 519-544).
CreateNotSent ==
    /\ launch = "creating"
    /\ rec' = "retired" /\ launch' = "idle"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, pane, fresh, stray, input, closing, crashes, lost, exits>>

\* Re-read the row, then record the pane on this nonce's Pending (herdr_launch.rs:482-494).
RecordPane ==
    /\ launch = "placing"
    /\ rec' = "placed" /\ pane' = "agent" /\ fresh' = FALSE /\ launch' = "evidence"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, stray, input, closing, crashes, lost, exits>>

\* The re-read or the CAS fails: the launch ends Indeterminate and the pane is not recorded
\* (herdr_launch.rs:483-494).
RecordPaneFails ==
    /\ launch = "placing" /\ lost < 1
    /\ fresh' = FALSE /\ stray' = stray + 1 /\ launch' = "idle" /\ lost' = lost + 1
    /\ landed' = "none"
    /\ UNCHANGED <<srv, rec, pane, input, closing, crashes, exits>>

\* Probe the pane's root shell and provider and store them as expected (herdr_launch.rs:502-514, 592-610).
RecordEvidence ==
    /\ launch = "evidence" /\ pane = "agent"
    /\ rec' = "evidenced" /\ launch' = "idle"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, pane, fresh, stray, input, closing, crashes, lost, exits>>

\* The probe finds no provider evidence: the launch still returns Launched with the gap, and the
\* row keeps its pane without evidence (herdr_launch.rs:509-514, 592-610).
EvidenceGap ==
    /\ launch = "evidence" /\ pane # "agent"
    /\ launch' = "idle"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, rec, pane, fresh, stray, input, closing, crashes, lost, exits>>

\* A later launch on a Pending that has a pane but no evidence only probes again; it never
\* creates a pane (herdr_launch.rs:344-373, claude/herdr_turn/never_attached.rs:11-27).
ProbeAgain ==
    /\ srv = "up" /\ launch = "idle" /\ rec = "placed" /\ pane = "agent"
    /\ rec' = "evidenced"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, pane, fresh, stray, launch, input, closing, crashes, lost, exits>>

\* The first attach binds the execution (claude/herdr_turn.rs:783 bind_pg; hosted_execution.rs:429-437).
Bind ==
    /\ srv = "up" /\ launch = "idle" /\ rec = "evidenced" /\ pane = "agent"
    /\ rec' = "bound"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, pane, fresh, stray, launch, input, closing, crashes, lost, exits>>

\* The input gate checks the exact execution: root shell and provider process
\* (claude/herdr_turn.rs:575-579 gate; session_host/herdr_gate.rs:411-431, 827-837, 901-937).
Pin ==
    /\ srv = "up" /\ launch = "idle" /\ input = "idle" /\ ~closing
    /\ rec = "bound" /\ pane = "agent"
    /\ input' = "pinned"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, rec, pane, fresh, stray, launch, closing, crashes, lost, exits>>

\* The send uses the pinned judgment and checks only the pane id and the server witness
\* (herdr_gate.rs:538-571, 940-960). A closed pane takes nothing.
Send ==
    /\ input = "pinned"
    /\ landed' = IF pane \in {"agent", "shell"} THEN pane ELSE "none"
    /\ input' = "idle"
    /\ UNCHANGED <<srv, rec, pane, fresh, stray, launch, closing, crashes, lost, exits>>

\* Claude exits; the pane stays open at its root shell (the command was typed into a shell,
\* herdr_launch.rs:298-311).
ProviderExits ==
    /\ pane = "agent" /\ exits < 1
    /\ pane' = "shell" /\ exits' = exits + 1
    /\ landed' = "none"
    /\ UNCHANGED <<srv, rec, fresh, stray, launch, input, closing, crashes, lost>>

\* The row ends once its execution is seen gone (recovery_engine/herdr_row_end.rs:1-3, 393-;
\* herdr_terminate.rs:184-198 retire_confirmed).
RowEnd ==
    /\ srv = "up" /\ launch = "idle" /\ input = "idle" /\ ~closing
    /\ rec \in {"placed", "evidenced", "bound"} /\ pane \in {"shell", "closed"}
    /\ rec' = "retired"
    /\ landed' = "none"
    /\ UNCHANGED <<srv, pane, fresh, stray, launch, input, closing, crashes, lost, exits>>

\* Operator terminate: one fresh judgment and one close; then the row is retired
\* (termination_audit/host_terminate.rs:116-133, herdr_gate.rs:451-491, herdr_terminate.rs:184-198).
OperatorClose ==
    /\ srv = "up" /\ launch = "idle" /\ input = "idle" /\ ~closing
    /\ rec = "bound" /\ pane \in {"agent", "shell"}
    /\ pane' = "closed" /\ closing' = TRUE
    /\ landed' = "none"
    /\ UNCHANGED <<srv, rec, fresh, stray, launch, input, crashes, lost, exits>>

OperatorRetire ==
    /\ closing
    /\ rec' = "retired" /\ closing' = FALSE
    /\ landed' = "none"
    /\ UNCHANGED <<srv, pane, fresh, stray, launch, input, crashes, lost, exits>>

\* dcserver dies: the launch, a pinned judgment and the pane id it held in memory are lost.
\* A create in flight may still have made a pane. Herdr panes and the row survive.
Crash ==
    /\ srv = "up" /\ crashes < 1
    /\ srv' = "down" /\ crashes' = crashes + 1
    /\ launch' = "idle" /\ input' = "idle" /\ closing' = FALSE /\ fresh' = FALSE
    /\ stray' \in IF fresh THEN {stray + 1}
                  ELSE IF launch = "creating" THEN {stray, stray + 1} ELSE {stray}
    /\ landed' = "none"
    /\ UNCHANGED <<rec, pane, lost, exits>>

\* Restart reconcile only reads: it compares the stored record with the exact pane and never
\* writes, adopts, relaunches or closes (recovery_engine/host_reconcile.rs:1-2, 94-99).
Boot ==
    /\ srv = "down"
    /\ srv' = "up"
    /\ landed' = "none"
    /\ UNCHANGED <<rec, pane, fresh, stray, launch, input, closing, crashes, lost, exits>>

Next ==
    \/ StartLaunch \/ CreateOk \/ CreateLost \/ CreateNotSent \/ RecordPane \/ RecordPaneFails
    \/ RecordEvidence \/ EvidenceGap \/ ProbeAgain \/ Bind \/ Pin \/ Send \/ ProviderExits \/ RowEnd
    \/ OperatorClose \/ OperatorRetire \/ Crash \/ Boot

Spec == Init /\ [][Next]_vars

\* Rules the reader agreed to.
\* A prompt reaches only the recorded provider, never the shell left in its pane.
PromptOnlyToAgent == landed \in {"none", "agent"}
\* Every pane that may run an agent for the channel is the one the row names, or one a running
\* launch is about to record.
NoOrphanPane == stray = 0
\* The channel never has two agent panes.
NoDuplicatePane == (IF pane = "agent" THEN 1 ELSE 0) + (IF fresh THEN 1 ELSE 0) + stray <= 1
\* Once no launch or terminate is running, the row never names a closed pane, and never sits
\* Pending without a pane.
RowMatchesPane ==
    (srv = "up" /\ launch = "idle" /\ ~closing) =>
        /\ rec # "pending"
        /\ (rec \in {"placed", "evidenced", "bound"} => pane # "closed")
====
