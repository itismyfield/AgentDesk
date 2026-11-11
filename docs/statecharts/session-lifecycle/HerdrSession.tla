---- MODULE HerdrSession ----
\* One Discord channel's Claude execution on a Herdr pane (HostKind::Herdr), across one dcserver
\* crash. AgentDesk main e8d94425a04867279ced4ec967e9d93df0d7ec0a.
\* `rec` is the channel's hosted_execution record (Postgres); pane ids are not derived from the
\* channel, so a pane nobody recorded cannot be found again. Each action cites its code.
EXTENDS Naturals

CONSTANTS HandoffPolicy, HandoffCases
VARIABLE handoff

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

sessionVars == <<srv, rec, pane, fresh, stray, launch, input, landed, closing, crashes, lost, exits>>
vars == <<sessionVars, handoff>>

SessionInit ==
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

SessionNext ==
    \/ StartLaunch \/ CreateOk \/ CreateLost \/ CreateNotSent \/ RecordPane \/ RecordPaneFails
    \/ RecordEvidence \/ EvidenceGap \/ ProbeAgain \/ Bind \/ Pin \/ Send \/ ProviderExits \/ RowEnd
    \/ OperatorClose \/ OperatorRetire \/ Crash \/ Boot


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

\* Codex direct-TUI handoff, design contract on main 289f41b1. Identity abstracts the
\* server generation, pane, provider/owner, process birth and launch nonce tuple.
HInitial(c) ==
    [case |-> c, origin |-> IF c \in {"hint", "marker_changed"} THEN "start" ELSE "person",
     busy |-> c = "queue", step |-> "admit", identity |-> "old", pin |-> "none",
     legacy |-> IF c \in {"hint", "person", "marker_changed"} THEN "absent"
                ELSE IF c = "unknown" THEN "unknown"
                ELSE IF c = "shell" THEN "shell" ELSE "live",
     hint |-> TRUE, marker |-> "old", snapshot |-> "old",
     queue |-> "queued", raw |-> "queue", attempt |-> "none", result |-> "none",
     literal |-> 0, enter |-> 0, draft |-> "empty", edited |-> FALSE,
     wrong |-> FALSE, wrongDraft |-> FALSE, literalDraft |-> FALSE, busyWrite |-> FALSE, afterCancel |-> FALSE,
     swapped |-> FALSE, superseded |-> FALSE, ended |-> FALSE,
     failures |-> 0, wakes |-> 0, creates |-> 0, effects |-> 0,
     notified |-> FALSE, foreignCleanup |-> FALSE]

HInit == \E c \in HandoffCases : handoff = HInitial(c)
HCurrent == handoff.identity = handoff.pin /\ ~handoff.superseded
HWriteReady == HCurrent /\ handoff.legacy = "live" /\ ~handoff.busy
HLiteralReady == HWriteReady /\ handoff.draft = "empty"
HEnterReady == HWriteReady /\ handoff.draft = "/exit"

\* owner_input.rs:859-864: only non-person kept starts consult start_waits.
HAdmission ==
    /\ srv = "up" /\ handoff.step = "admit"
    /\ handoff' = [handoff EXCEPT
        !.step = IF HandoffPolicy = "r1" /\ handoff.hint /\ handoff.origin = "start"
                 THEN "stuck"
                 ELSE IF handoff.legacy = "absent" THEN "clean" ELSE "observe",
        !.pin = handoff.identity,
        !.ended = handoff.legacy = "absent"]

\* Two Opaque failures leave raw custody retained, but lose automatic resumption.
\* queued_head/v2/start_failure.rs:81; this is not a claim that the bytes disappear.
HObserve ==
    /\ srv = "up" /\ handoff.step = "observe"
    /\ handoff' = IF handoff.case \in {"unsupported", "unknown", "shell"}
       THEN [handoff EXCEPT !.step = "notice", !.queue = "refused", !.raw = "retained"]
       ELSE IF handoff.busy
       THEN IF HandoffPolicy = "r1"
            THEN [handoff EXCEPT !.failures = @ + 1,
                  !.step = IF handoff.failures = 1 THEN "wait" ELSE "observe",
                  !.queue = IF handoff.failures = 1 THEN "manual_retry" ELSE "queued",
                  !.raw = IF handoff.failures = 1 THEN "retained" ELSE "queue"]
            ELSE [handoff EXCEPT !.step = "wait", !.queue = "waiting"]
       ELSE [handoff EXCEPT !.step = "literal", !.attempt = "literal_intent"]

\* Proposed compare-and-write port: check and physical write are one abstract step.
\* Implementation must prove that fence; LegacyTmuxGate supplies no such authority.
HLiteral ==
    /\ srv = "up" /\ handoff.step = "literal"
    /\ HandoffPolicy = "r1" \/ HLiteralReady
    /\ handoff' = [handoff EXCEPT !.literal = @ + 1, !.draft = IF @ = "empty" THEN "/exit" ELSE "human/exit",
         !.wrong = @ \/ ~HCurrent \/ handoff.legacy # "live",
         !.literalDraft = @ \/ handoff.draft # "empty", !.busyWrite = @ \/ handoff.busy, !.afterCancel = @ \/ handoff.superseded,
         !.attempt = "literal_written", !.step = "arm_enter"]

HArmEnter ==
    /\ srv = "up" /\ handoff.step = "arm_enter"
    /\ HandoffPolicy = "r1" \/ HEnterReady
    /\ handoff' = [handoff EXCEPT !.attempt = "enter_intent", !.step = "enter"]

HEnter ==
    /\ srv = "up" /\ handoff.step = "enter"
    /\ HandoffPolicy = "r1" \/ HEnterReady
    /\ handoff' = [handoff EXCEPT !.enter = @ + 1,
         !.wrong = @ \/ ~HCurrent \/ handoff.legacy # "live",
         !.busyWrite = @ \/ handoff.busy, !.wrongDraft = @ \/ handoff.draft # "/exit",
         !.afterCancel = @ \/ handoff.superseded, !.attempt = "entered",
         !.result = "unconfirmed", !.step = "wait", !.queue = "waiting"]

\* A same-name replacement can occur before either write or before successor creation.
HReplace ==
    /\ srv = "up" /\ handoff.case = "target" /\ ~handoff.swapped
    /\ handoff.step \in {"literal", "arm_enter", "enter", "clean", "wake", "create"}
    /\ handoff' = [handoff EXCEPT !.identity = "other", !.legacy = "live", !.swapped = TRUE]

HDraftChanges ==
    /\ srv = "up" /\ handoff.case = "target" /\ ~handoff.edited
    /\ handoff.step \in {"literal", "arm_enter", "enter"}
    /\ handoff' = [handoff EXCEPT !.draft = "human", !.edited = TRUE]

HBusyAppears ==
    /\ srv = "up" /\ handoff.case = "target" /\ ~handoff.edited /\ ~handoff.busy
    /\ handoff.step \in {"literal", "arm_enter", "enter"}
    /\ handoff' = [handoff EXCEPT !.busy = TRUE, !.edited = TRUE]

HMarkerChanges ==
    /\ srv = "up" /\ handoff.case = "marker_changed" /\ handoff.step = "clean"
    /\ handoff.marker = "old"
    /\ handoff' = [handoff EXCEPT !.marker = "other"]

HCancel ==
    /\ srv = "up" /\ handoff.case = "cancel" /\ ~handoff.superseded
    /\ handoff.step \in {"observe", "literal", "arm_enter", "enter", "wait", "clean", "wake", "create"}
    /\ handoff' = [handoff EXCEPT !.superseded = TRUE]

HHalt ==
    /\ srv = "up" /\ HandoffPolicy = "r2"
    /\ handoff.step \notin {"done", "notice", "recover", "created", "placing", "evidence", "binding", "deliver"}
    /\ handoff.superseded
       \/ (handoff.step = "literal" /\ ~HLiteralReady)
       \/ (handoff.step \in {"arm_enter", "enter"} /\ ~HEnterReady)
       \/ (handoff.step \in {"clean", "wake", "create"} /\ ~HCurrent)
       \/ (handoff.step = "clean" /\ handoff.marker # handoff.snapshot)
    /\ handoff' = [handoff EXCEPT !.step = "notice", !.queue = "refused", !.raw = "retained",
         !.result = IF handoff.literal > 0 THEN "unclear" ELSE "untouched"]

\* Environment assumption for progress: this exact old execution eventually ends.
\* In restart-after-intent/no-write cases this requires an external end, not a replay.
HLegacyEnds ==
    /\ srv = "up" /\ handoff.step = "wait" /\ handoff.legacy = "live"
    /\ handoff.identity = handoff.pin /\ ~handoff.superseded
    /\ handoff' = [handoff EXCEPT !.legacy = "absent"]

HBusyClears ==
    /\ srv = "up" /\ handoff.case = "queue" /\ handoff.step = "wait" /\ handoff.busy
    /\ handoff' = [handoff EXCEPT !.busy = FALSE]
HReadyAgain ==
    /\ srv = "up" /\ handoff.step = "wait" /\ handoff.queue = "waiting"
    /\ handoff.attempt = "none" /\ HWriteReady
    /\ handoff' = [handoff EXCEPT !.step = "observe"]

HConfirmEnded ==
    /\ srv = "up" /\ handoff.step = "wait" /\ handoff.legacy = "absent"
    /\ handoff.queue = "waiting" /\ HCurrent
    /\ handoff' = [handoff EXCEPT !.ended = TRUE, !.step = "clean"]

\* Absent here means session Missing plus no process registry hit, not a dead pane.
\* Marker equality abstracts canonical+legacy file identity/bytes and owner witnesses.
HCleanHint ==
    /\ srv = "up" /\ handoff.step = "clean" /\ HCurrent
    /\ handoff.legacy = "absent" /\ handoff.marker = handoff.snapshot
    /\ handoff' = [handoff EXCEPT
         !.hint = IF HandoffPolicy = "r2" THEN FALSE ELSE @,
         !.foreignCleanup = @ \/ (HandoffPolicy = "r2" /\ (handoff.marker # handoff.snapshot \/ ~HCurrent)),
         !.step = IF HandoffPolicy = "r2" THEN "wake" ELSE "create"]

\* One durable logical wake token; recovery may redeliver its idempotent notification.
HWake ==
    /\ srv = "up" /\ handoff.step = "wake" /\ HCurrent
    /\ handoff' = [handoff EXCEPT !.wakes = 1, !.queue = "queued", !.step = "create"]

HNotice ==
    /\ srv = "up" /\ handoff.step = "notice"
    /\ handoff' = [handoff EXCEPT !.notified = TRUE, !.step = "done"]

\* Reuse the existing durable Pending/create/record/bind actions after handoff only.
HCreate ==
    /\ handoff.step = "create" /\ handoff.ended /\ HCurrent
    /\ handoff.legacy = "absent" /\ (~handoff.hint \/ HandoffPolicy = "r1")
    /\ StartLaunch
    /\ handoff' = [handoff EXCEPT !.creates = @ + 1, !.hint = FALSE, !.step = "created"]
HCreated == handoff.step = "created" /\ CreateOk /\ handoff' = [handoff EXCEPT !.step = "placing"]
HPlaced == handoff.step = "placing" /\ RecordPane /\ handoff' = [handoff EXCEPT !.step = "evidence"]
HEvidenced == handoff.step = "evidence" /\ RecordEvidence /\ handoff' = [handoff EXCEPT !.step = "binding"]
HBound == handoff.step = "binding" /\ Bind /\ handoff' = [handoff EXCEPT !.step = "deliver"]
HDeliver ==
    /\ srv = "up" /\ handoff.step = "deliver" /\ rec = "bound" /\ pane = "agent"
    /\ handoff' = [handoff EXCEPT !.effects = @ + 1, !.queue = "delivered", !.raw = "delivered", !.step = "done"]

HCrash ==
    /\ handoff.case = "restart" /\ rec = "legacy"
    /\ handoff.step \in {"literal", "arm_enter", "enter", "wait", "clean", "wake", "create"}
    /\ Crash
    /\ handoff' = [handoff EXCEPT !.step = "recover", !.queue = "waiting"]
HBoot ==
    /\ handoff.step = "recover" /\ Boot
    /\ handoff' = [handoff EXCEPT
         !.step = IF handoff.ended THEN "clean" ELSE IF handoff.attempt = "none" THEN "admit" ELSE "wait"]

HSteps == HAdmission \/ HObserve \/ HLiteral \/ HArmEnter \/ HEnter \/ HReplace
       \/ HDraftChanges \/ HBusyAppears \/ HMarkerChanges \/ HCancel \/ HHalt \/ HLegacyEnds
       \/ HBusyClears \/ HReadyAgain \/ HConfirmEnded \/ HCleanHint \/ HWake \/ HNotice \/ HDeliver
HandoffNext == (HSteps /\ UNCHANGED sessionVars)
       \/ HCreate \/ HCreated \/ HPlaced \/ HEvidenced \/ HBound \/ HCrash \/ HBoot
Init == SessionInit /\ HInit
Next == IF HandoffPolicy = "off" THEN SessionNext /\ UNCHANGED handoff ELSE HandoffNext
Spec == Init /\ [][Next]_vars
HandoffSpec == Init /\ [][HandoffNext]_vars
       /\ WF_vars(HandoffNext) /\ WF_vars(HLegacyEnds /\ UNCHANGED sessionVars)

NoWrongExecutionExit == ~handoff.wrong
NoUnexpectedEnter == ~handoff.wrongDraft
NoLiteralIntoDraft == ~handoff.literalDraft
NoBusyExit == ~handoff.busyWrite
NoExitReplay == handoff.literal <= 1 /\ handoff.enter <= 1
NoAfterCancel == ~handoff.afterCancel
NoForeignHintCleanup == ~handoff.foreignCleanup
NoDuplicateExecution ==
    (IF handoff.legacy = "live" THEN 1 ELSE 0)
    + (IF pane = "agent" THEN 1 ELSE 0) + (IF fresh THEN 1 ELSE 0) + stray <= 1
NoPrematureSuccessor == handoff.creates > 0 => handoff.ended /\ handoff.legacy = "absent"
NoDuplicateOriginal == handoff.creates <= 1 /\ handoff.effects <= 1 /\ handoff.wakes <= 1
NoSilentLoss ==
    /\ handoff.raw \in {"queue", "retained", "delivered"}
    /\ (handoff.raw = "delivered" => handoff.effects = 1)
    /\ (handoff.queue = "refused" => handoff.raw = "retained")
    /\ (handoff.step = "done" => handoff.queue = "delivered" \/ (handoff.queue = "refused" /\ handoff.notified))
HandoffProgress == <> (handoff.queue = "delivered" \/ (handoff.queue = "refused" /\ handoff.notified))


\* Marker-only cleanup uses the existing handoff variable, independently of the
\* deferred exit/journal specification. Lookup kind is evidence, not a Boolean.
AInitial(c) ==
    [case |-> c, phase |-> IF c = "outbox_only" THEN "boot" ELSE "observe",
     lookup |-> IF c = "missing" THEN "Missing" ELSE IF c = "retired" THEN "Retired" ELSE "Legacy",
     owner |-> c # "owner_unknown", probe |-> c # "probe_error",
     canonical |-> c # "outbox_only", alias |-> c # "outbox_only",
     snapshot |-> TRUE, front |-> TRUE, epoch |-> TRUE, fence |-> FALSE,
     intent |-> FALSE, committed |-> c = "outbox_only",
     outbox |-> c = "outbox_only", acknowledged |-> FALSE,
     up |-> c # "outbox_only", disturbed |-> FALSE, faulted |-> FALSE,
     restarted |-> FALSE, contested |-> FALSE, proof |-> FALSE,
     raw |-> IF c = "outbox_only" THEN "settled" ELSE "queued",
     notified |-> FALSE, deletes |-> 0, unsafeDeletes |-> 0, wakes |-> 0,
     grants |-> 0, creates |-> 0, prompts |-> 0]
AInit == \E c \in HandoffCases : handoff = AInitial(c)
AOwned == handoff.owner /\ handoff.probe /\ handoff.snapshot
AFront == handoff.front /\ handoff.epoch
AValid == AOwned /\ AFront /\ handoff.lookup \in {"Missing", "Legacy"}
AHoldsInput == handoff.raw = "queued" \/ handoff.intent \/ handoff.outbox

AObserve ==
    /\ handoff.up /\ handoff.phase = "observe"
    /\ handoff' = [handoff EXCEPT
         !.phase = IF AValid THEN "intent" ELSE "notice",
         !.fence = AValid]
\* The queue intent is committed before the first unlink. Unlink is idempotent
\* only for this intent's recorded paths and exact unchanged witnesses.
AIntent ==
    /\ handoff.phase = "intent" /\ handoff.up /\ handoff.fence
    /\ handoff' = [handoff EXCEPT !.intent = TRUE, !.fence = FALSE, !.phase = "recheck"]
ARecheck ==
    /\ handoff.up /\ handoff.phase = "recheck" /\ handoff.intent
    /\ handoff' = [handoff EXCEPT !.fence = AValid,
         !.phase = IF AValid THEN "unlink_canonical" ELSE "notice"]
AUnlinkCanonical ==
    /\ handoff.up /\ handoff.phase = "unlink_canonical" /\ handoff.fence /\ AValid
    /\ handoff' = [handoff EXCEPT !.canonical = FALSE,
         !.deletes = @ + (IF handoff.canonical THEN 1 ELSE 0),
         !.unsafeDeletes = @ + (IF AValid /\ handoff.fence THEN 0 ELSE 1), !.phase = "unlink_alias"]
AUnlinkAlias ==
    /\ handoff.up /\ handoff.phase = "unlink_alias" /\ handoff.fence /\ AValid
    /\ handoff' = [handoff EXCEPT !.alias = FALSE,
         !.deletes = @ + (IF handoff.alias THEN 1 ELSE 0),
         !.unsafeDeletes = @ + (IF AValid /\ handoff.fence THEN 0 ELSE 1), !.phase = "commit"]
APartialUnlink ==
    /\ handoff.up /\ handoff.case = "partial" /\ ~handoff.faulted
    /\ handoff.phase = "unlink_alias"
    /\ handoff' = [handoff EXCEPT !.faulted = TRUE, !.fence = FALSE, !.phase = "recover"]
ACommit ==
    /\ handoff.up /\ handoff.phase = "commit" /\ AValid
    /\ ~handoff.canonical /\ ~handoff.alias
    /\ handoff' = [handoff EXCEPT !.committed = TRUE, !.fence = FALSE, !.phase = "outbox"]
\* Receipt commit and outbox creation are separate recoverable steps. The receipt
\* itself keeps boot discovery live until the outbox is durably acknowledged.
AOutbox ==
    /\ handoff.up /\ handoff.phase = "outbox" /\ handoff.committed
    /\ handoff' = [handoff EXCEPT !.outbox = TRUE, !.phase = "wake"]
AWake ==
    /\ handoff.up /\ handoff.outbox /\ handoff.phase = "wake"
    /\ handoff' = [handoff EXCEPT !.wakes = @ + 1, !.phase = "admit"]
\* Read-only admission re-probes absence and compares the exact Legacy row,
\* owner, paths, front and epoch; clearing a hint alone never grants authority.
AAdmission ==
    /\ handoff.up /\ handoff.phase = "admit"
    /\ handoff' = IF handoff.raw = "settled"
       THEN [handoff EXCEPT !.phase = "ack"]
       ELSE IF ~AValid
       THEN [handoff EXCEPT !.phase = "notice"]
       ELSE IF HandoffPolicy = "a-before" /\ handoff.lookup = "Legacy"
       THEN [handoff EXCEPT !.phase = "wait"]
       ELSE [handoff EXCEPT !.proof = TRUE, !.fence = TRUE, !.phase = "grant"]
AGrant ==
    /\ handoff.up /\ handoff.phase = "grant" /\ handoff.proof /\ handoff.fence
    /\ (IF HandoffPolicy = "a-no-front-check" THEN AOwned ELSE AValid)
    /\ ~handoff.canonical /\ ~handoff.alias /\ handoff.committed
    /\ handoff' = [handoff EXCEPT !.grants = @ + 1, !.phase = "create"]
ACreate ==
    /\ handoff.up /\ handoff.phase = "create"
    /\ handoff' = [handoff EXCEPT !.creates = @ + 1, !.fence = FALSE, !.phase = "prompt"]
APrompt ==
    /\ handoff.up /\ handoff.phase = "prompt"
    /\ handoff' = [handoff EXCEPT !.prompts = @ + 1, !.raw = "delivered", !.phase = "ack"]
AAck ==
    /\ handoff.up /\ handoff.phase = "ack"
    /\ handoff' = [handoff EXCEPT !.acknowledged = TRUE, !.outbox = FALSE,
         !.intent = FALSE, !.phase = "done"]
ARedeliver ==
    /\ handoff.up /\ handoff.case = "redelivery" /\ handoff.wakes = 1
    /\ handoff.phase \in {"admit", "grant", "create", "prompt", "ack", "done"}
    /\ handoff' = [handoff EXCEPT !.wakes = 2]
ACrash ==
    /\ handoff.up /\ handoff.case = "crash" /\ ~handoff.restarted
    /\ handoff.phase \in {"commit", "outbox", "wake", "admit"}
    /\ handoff' = [handoff EXCEPT !.up = FALSE, !.restarted = TRUE,
         !.fence = FALSE, !.proof = FALSE, !.phase = "boot"]
ABoot ==
    /\ ~handoff.up /\ handoff.phase = "boot"
    /\ (IF HandoffPolicy = "a-no-outbox-discovery" THEN handoff.raw = "queued" ELSE AHoldsInput)
    /\ handoff' = [handoff EXCEPT !.up = TRUE,
         !.phase = IF handoff.committed THEN "outbox" ELSE "recover"]
ARecover ==
    /\ handoff.up /\ handoff.phase = "recover" /\ handoff.intent
    /\ handoff' = [handoff EXCEPT !.fence = AValid,
         !.phase = IF AValid THEN "unlink_canonical" ELSE "notice"]
\* Queue-front and owner-epoch changes invalidate the receipt's consumer. A
\* changed file before the mutation fence is acquired invalidates its snapshot.
AChange ==
    /\ handoff.up /\ ~handoff.disturbed
    /\ ((handoff.case \in {"front", "epoch"} /\ handoff.phase \in {"commit", "outbox", "wake", "admit", "grant"})
        \/ (handoff.case = "file_replaced" /\ handoff.phase \in {"observe", "recheck"})
        \/ (handoff.case \in {"post_cleanup_replace", "row_changed"} /\ handoff.phase \in {"wake", "admit"}))
    /\ handoff' = [handoff EXCEPT !.disturbed = TRUE,
         !.front = IF handoff.case = "front" THEN FALSE ELSE @,
         !.epoch = IF handoff.case = "epoch" THEN FALSE ELSE @,
         !.snapshot = IF handoff.case \in {"file_replaced", "post_cleanup_replace"} THEN FALSE ELSE @,
         !.lookup = IF handoff.case = "row_changed" THEN "Retired" ELSE @]
AFenceContender ==
    /\ handoff.up /\ handoff.case = "fence" /\ handoff.fence /\ ~handoff.contested
    /\ handoff' = [handoff EXCEPT !.contested = TRUE]
AStale ==
    /\ handoff.up /\ ~AValid
    /\ handoff.phase \in {"unlink_canonical", "unlink_alias", "commit", "outbox", "wake", "grant"}
    /\ handoff' = [handoff EXCEPT !.fence = FALSE, !.phase = "notice"]
ANotice ==
    /\ handoff.up /\ handoff.phase = "notice"
    /\ handoff' = [handoff EXCEPT !.raw = "retained", !.notified = TRUE,
         !.outbox = FALSE, !.intent = FALSE, !.acknowledged = TRUE, !.phase = "done"]
ASteps == AObserve \/ AIntent \/ ARecheck \/ AUnlinkCanonical \/ AUnlinkAlias \/ APartialUnlink
       \/ ACommit \/ AOutbox \/ AWake \/ AAdmission \/ AGrant \/ ACreate \/ APrompt
       \/ AAck \/ ARedeliver \/ ACrash \/ ABoot \/ ARecover \/ AChange
       \/ AFenceContender \/ AStale \/ ANotice
AHintNext == ASteps /\ UNCHANGED sessionVars
AHintSpec == SessionInit /\ AInit /\ [][AHintNext]_vars /\ WF_vars(AHintNext)
ANoForeignMutation ==
    /\ handoff.unsafeDeletes = 0
    /\ ((~handoff.owner \/ ~handoff.probe) => handoff.deletes = 0)
ANoDuplicateEffects == handoff.grants <= 1 /\ handoff.creates <= 1 /\ handoff.prompts <= 1
ANoUnprovedGrant == handoff.grants > 0 => handoff.proof /\ handoff.committed
ANoStaleGrant == handoff.grants > 0 => AValid
ANoSilentLoss ==
    /\ handoff.raw \in {"queued", "retained", "delivered", "settled"}
    /\ (handoff.raw = "delivered" => handoff.prompts = 1)
    /\ (handoff.raw = "retained" => handoff.notified)
    /\ (handoff.phase = "done" => handoff.raw # "queued")
ANoEarlyUnlink == handoff.deletes > 0 => handoff.intent \/ handoff.acknowledged
AProgress == <> (handoff.phase = "done")

====
