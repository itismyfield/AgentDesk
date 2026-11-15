---- MODULE CodexHerdrBootstrap ----
\* The Codex canary channel on Herdr after its Herdr row retired, across one dcserver restart.
\* AgentDesk 3fa2ab95c8068bb0c4a58bc742f65f0db80c5a71. Each action cites its code.
\* The durable tmux-era verified pin never changes: Herdr launches write no codex ownership
\* (herdr_launch.rs:740-757 source_policy None; binding_events/codex.rs:643-679 latest record).
\* Value ranges come from the design entry table (rows W, N, P, R, X).
EXTENDS Naturals

CONSTANTS
    Fix,      \* "before" | "design" | "mut_no_vacancy" | "mut_pin_drop"
    LiveInit  \* {FALSE} or {TRUE, FALSE}: whether the pin's old tmux execution may still run

VARIABLES
    row,       \* hosted row: "retired" | "pending" | "bound"                      (R)
    cleanup,   \* retired cleanup: "pending" | "done"                              (R)
    marker,    \* spawn nonce marker of the logical key: "absent" | "n2"           (N)
    tmuxLive,  \* the pinned tmux execution still runs (marker lost under it)      (P)
    writer,    \* O actor: "booting" | "blocked" | "resumed"                       (W)
    resumedBy, \* what the resume edge saw: "none" | "evidence" | "nothing"        (W)
    intake,    \* one input: "none"|"held"|"gateway"|"admitted"|"launched"|"done"|"cancelled"
    exec,      \* the new incarnation: "none" | "pending" | "bound"                (X)
    pub,       \* first source the writer published: "none" | "n2" | "old"
    fails,     \* create replies lost (bounded)
    boots      \* restarts taken (bounded)

vars == <<row, cleanup, marker, tmuxLive, writer, resumedBy, intake, exec, pub, fails, boots>>

Init ==
    /\ row = "retired" /\ cleanup \in {"pending", "done"} /\ marker = "absent"
    /\ tmuxLive \in LiveInit
    /\ writer = "booting" /\ resumedBy = "none"
    /\ intake = "none" /\ exec = "none" /\ pub = "none" /\ fails = 0 /\ boots = 0

\* codex_policy.rs:33-58 and :331-342: an absent marker on the canary pane recovers the durable
\* verified pin, so the pane is VerifiedUnavailable. A marker of a Herdr nonce reads a context
\* with no source policy, which is Legacy (codex_policy.rs:60-64). mut_pin_drop ignores the pin.
PinBlocks == marker = "absent" /\ Fix # "mut_pin_drop"
\* actor.rs:690-693 (owed_delivery_ready false) -> deliver.rs:140-148 -> codex_policy.rs:181-186.
SourcesAllowed == ~PinBlocks
\* host.rs:248-259: Owned, not stopping, resumed.
Accepts == writer = "resumed"
\* absent_open.rs:82-86 target_absent: no tmux session, no process, dead or absent pane.
Vacant == ~tmuxLive

\* Design: a Retired Herdr row whose cleanup finished, no current marker, no live new incarnation,
\* a live owned actor that is blocked only on sources, and the pinned tmux target vacant.
Eligible ==
    /\ Fix \in {"design", "mut_no_vacancy"}
    /\ writer = "blocked"
    /\ row = "retired" /\ cleanup = "done"
    /\ marker = "absent" /\ exec = "none"
    /\ (Vacant \/ Fix = "mut_no_vacancy")

Keep(v) == UNCHANGED v

\* host.rs:863-876 then actor.rs:329-356: recover, port, hosted and first_drain succeed; the
\* resume phase waits on sources.
Boot ==
    /\ writer = "booting" /\ writer' = "blocked"
    /\ Keep(<<row, cleanup, marker, tmuxLive, resumedBy, intake, exec, pub, fails, boots>>)

\* actor.rs:357-396: resume runs only while sources are allowed, then resumed=true.
Resume ==
    /\ writer = "blocked" /\ SourcesAllowed
    /\ writer' = "resumed"
    /\ resumedBy' = IF marker = "absent" THEN "nothing" ELSE "evidence"
    /\ Keep(<<row, cleanup, marker, tmuxLive, intake, exec, pub, fails, boots>>)

\* intake_route/input.rs:85-110: placement is Gateway when the writer accepts; otherwise Hold
\* (fresh admission is tmux-only: absent_open.rs:62, codex_absence.rs:20). Design adds Eligible.
InputArrives ==
    /\ intake = "none"
    /\ intake' = IF Accepts \/ Eligible THEN "gateway" ELSE "held"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, exec, pub, fails, boots>>)

\* intake_worker.rs:288 discovery and owner_input.rs:184 ready wake judge held input again.
Recheck ==
    /\ intake = "held" /\ (Accepts \/ Eligible)
    /\ intake' = "gateway"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, exec, pub, fails, boots>>)

\* turn_host.rs:295-315: o_writer_ready or OWriterNotReady; the refused front is retained.
TurnHost ==
    /\ intake = "gateway"
    /\ intake' = IF Accepts \/ Eligible THEN "admitted" ELSE "held"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, exec, pub, fails, boots>>)

\* codex/herdr_turn.rs:151-166 `_` arm -> herdr_launch.rs:432-505: the Retired row is CASed to
\* Pending by this owner, then the new nonce marker is stamped. Design rechecks vacancy under
\* the runtime hint fence before the CAS.
Launch ==
    /\ intake = "admitted" /\ row = "retired" /\ exec = "none"
    /\ (Vacant \/ Fix # "design")
    /\ row' = "pending" /\ exec' = "pending" /\ marker' = "n2" /\ intake' = "launched"
    /\ Keep(<<cleanup, tmuxLive, writer, resumedBy, pub, fails, boots>>)

LaunchRefused ==
    /\ intake = "admitted" /\ row = "retired" /\ Fix = "design" /\ ~Vacant
    /\ intake' = "held"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, exec, pub, fails, boots>>)

\* codex/herdr_turn.rs:156-160: a Pending row of this owner is reused, never created again.
AdmitPending ==
    /\ intake = "admitted" /\ row = "pending" /\ exec = "pending"
    /\ intake' = "launched"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, exec, pub, fails, boots>>)

\* codex/herdr_turn.rs:152-154: a Bound row takes a follow-up.
Followup ==
    /\ intake = "admitted" /\ row = "bound"
    /\ intake' = "done"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, exec, pub, fails, boots>>)

\* first_prompt and launch evidence bind the pane (herdr_turn.rs:436-456).
Bind ==
    /\ intake = "launched" /\ exec = "pending"
    /\ row' = "bound" /\ exec' = "bound" /\ intake' = "done"
    /\ Keep(<<cleanup, marker, tmuxLive, writer, resumedBy, pub, fails, boots>>)

\* herdr_launch.rs:588-599 retire_unsent: Pending back to Retired; the marker stays.
LaunchNotSent ==
    /\ intake = "launched" /\ exec = "pending" /\ fails = 0
    /\ row' = "retired" /\ cleanup' = "done" /\ exec' = "none" /\ intake' = "held"
    /\ fails' = 1
    /\ Keep(<<marker, tmuxLive, writer, resumedBy, pub, boots>>)

\* unplaced.rs:468: a Pending row no live launch holds is retired.
Unplaced ==
    /\ exec = "pending" /\ intake \notin {"launched", "admitted"}
    /\ row' = "retired" /\ cleanup' = "done" /\ exec' = "none"
    /\ Keep(<<marker, tmuxLive, writer, resumedBy, intake, pub, fails, boots>>)

\* Writer posts the bound incarnation's source.
Publish ==
    /\ writer = "resumed" /\ exec = "bound" /\ pub = "none"
    /\ pub' = "n2"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, intake, exec, fails, boots>>)

\* A still running pinned tmux execution grows its unproven source.
PublishOld ==
    /\ writer = "resumed" /\ tmuxLive /\ pub # "old"
    /\ pub' = "old"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, intake, exec, fails, boots>>)

\* herdr_terminate.rs:224-229, clear_teardown.rs:137 end_herdr, stop: Bound to Retired.
Retire ==
    /\ exec = "bound"
    /\ row' = "retired" /\ exec' = "none" /\ cleanup' = "pending"
    /\ Keep(<<marker, tmuxLive, writer, resumedBy, intake, pub, fails, boots>>)

\* root_replaced.rs:570-588 cleanup: runtime-kind marker, hold release, empty settlement.
CleanupDone ==
    /\ cleanup = "pending" /\ cleanup' = "done"
    /\ Keep(<<row, marker, tmuxLive, writer, resumedBy, intake, exec, pub, fails, boots>>)

\* tmux_common.rs:1222-1243 removes spawn_nonce for any cleanup keyed by the logical name.
MarkerLost ==
    /\ marker = "n2" /\ exec = "none"
    /\ marker' = "absent"
    /\ Keep(<<row, cleanup, tmuxLive, writer, resumedBy, intake, exec, pub, fails, boots>>)

TmuxEnds ==
    /\ tmuxLive /\ tmuxLive' = FALSE
    /\ Keep(<<row, cleanup, marker, writer, resumedBy, intake, exec, pub, fails, boots>>)

\* A restart loses in-memory launches; the queue keeps the input (boot custody).
Restart ==
    /\ boots = 0 /\ boots' = 1
    /\ writer' = "booting" /\ resumedBy' = "none"
    /\ intake' = IF intake \in {"gateway", "admitted", "launched"} THEN "held" ELSE intake
    /\ Keep(<<row, cleanup, marker, tmuxLive, exec, pub, fails>>)

Cancel ==
    /\ intake \in {"held", "gateway", "admitted", "launched"}
    /\ intake' = "cancelled"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, exec, pub, fails, boots>>)

\* The next person or E2E input after a turn ends.
NextInput ==
    /\ intake \in {"done", "cancelled"} /\ intake' = "none"
    /\ Keep(<<row, cleanup, marker, tmuxLive, writer, resumedBy, exec, pub, fails, boots>>)

Next ==
    \/ NextInput \/ Boot \/ Resume \/ InputArrives \/ Recheck \/ TurnHost \/ Launch \/ LaunchRefused
    \/ AdmitPending \/ Followup \/ Bind \/ LaunchNotSent \/ Unplaced \/ Publish \/ PublishOld
    \/ Retire \/ CleanupDone \/ MarkerLost \/ TmuxEnds \/ Restart \/ Cancel
    \/ UNCHANGED vars

\* The actor loop and the unplaced resolver poll again and again, so a resume or a Pending
\* retirement enabled infinitely often is taken (SF).
Spec == Init /\ [][Next]_vars
    /\ WF_vars(Boot) /\ SF_vars(Resume) /\ WF_vars(NextInput) /\ WF_vars(InputArrives) /\ WF_vars(Recheck)
    /\ WF_vars(TurnHost) /\ WF_vars(Launch) /\ WF_vars(LaunchRefused) /\ WF_vars(AdmitPending)
    /\ WF_vars(Followup) /\ WF_vars(Bind) /\ SF_vars(Unplaced) /\ WF_vars(CleanupDone)

\* INVARIANT: no new incarnation while the pinned tmux execution still runs.
NoDualExecution == ~(tmuxLive /\ exec # "none")
\* INVARIANT: the writer never publishes a source outside the current incarnation's ownership.
SourceInBoundary == pub # "old"
\* INVARIANT: source permission comes only from a current incarnation's evidence, never from
\* the admission itself or from an absent marker.
ResumedOnlyWithEvidence == resumedBy # "nothing"
\* PROPERTY: a held input on a vacant Retired channel eventually starts or is cancelled.
HeldStarts == (intake = "held") ~> (intake \in {"done", "cancelled"})
\* PROPERTY: the writer eventually becomes ready once an input is held.
WriterReady == (intake = "held") ~> (writer = "resumed" \/ intake = "cancelled")
\* PROPERTY: a Pending row that no launch holds does not stay.
NoOrphanPending == (exec = "pending" /\ intake \in {"cancelled", "held", "done", "none"})
                   ~> (exec # "pending" \/ intake = "launched")
====
