---- MODULE CodexHerdrBootstrap ----
\* The Codex canary channel on Herdr after its Herdr row retired, across one dcserver restart.
\* AgentDesk 3fa2ab95c8068bb0c4a58bc742f65f0db80c5a71. Each action cites its code.
\* The durable tmux-era verified pin never changes: Herdr launches write no codex ownership
\* (herdr_launch.rs:742-759 source_policy None; binding_events/codex.rs:643-679 latest record).
\* Value ranges come from the design entry table (rows W, N, P, R, L, X).
EXTENDS Naturals

CONSTANTS
    Fix,          \* "before" | "design1" | "design" | "mut_cond4" | "mut_no_watch"
                  \* | "mut_no_vacancy" | "mut_no_recheck" | "mut_pin_drop"
    LiveInit,     \* subset of BOOLEAN: the pin's old tmux execution may run at start
    Appear,       \* BOOLEAN: a tmux of the logical key may start before a new incarnation exists
    CleanupInit,  \* subset of {"pending", "done"}
    WriterInit,   \* subset of {"booting", "blocked"}
    FailBudget,   \* 0 or 1: marker write or create failures allowed
    BootBudget,   \* 0 or 1: restarts allowed
    NextFair      \* BOOLEAN: a later input always comes (WF on NextInput)

VARIABLES
    row,       \* hosted row: "retired" | "pending" | "bound"                       (R)
    cleanup,   \* retired hold and cleanup: "pending" | "done"                      (R)
    marker,    \* spawn nonce marker of the logical key: "absent" | "n2"            (N)
    listing,   \* in-memory Herdr listing, relative to the row's nonce:            (L)
               \* "none" | "rw" (row nonce, withheld) | "ra" (row nonce, admitted)
    tmuxLive,  \* a tmux execution of the logical key runs                          (P)
    writer,    \* O actor: "booting" | "blocked" | "resumed"                        (W)
    resumedBy, \* what the resume edge saw: "none" | "evidence" | "nothing"         (W)
    intake,    \* one front: "none" | "queued" | "held" | "gateway" | "admitted" | "cas"
               \* | "launched" | "done" | "failed" | "cancelled"
    watch,     \* a held_watch probe watches the held front                         (I)
    viaBoot,   \* the admitted front came through the bootstrap admission
    exec,      \* the new incarnation: "none" | "pending" | "bound"                 (X)
    pub,       \* first source the writer published: "none" | "n2" | "old"
    fails, boots

vars == <<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, intake, watch, viaBoot,
          exec, pub, fails, boots>>

Bootstrap == Fix # "before" /\ Fix # "mut_pin_drop"
Watching  == Fix \in {"design", "mut_cond4", "mut_no_vacancy", "mut_no_recheck"}
Recheck   == Fix \in {"design1", "design", "mut_cond4", "mut_no_watch"}
Withhold  == Fix \in {"design", "mut_cond4", "mut_no_watch", "mut_no_vacancy", "mut_no_recheck"}

Init ==
    /\ row = "retired" /\ cleanup \in CleanupInit /\ marker = "absent" /\ listing = "none"
    /\ tmuxLive \in LiveInit
    /\ writer \in WriterInit /\ resumedBy = "none"
    /\ intake = "none" /\ watch = FALSE /\ viaBoot = FALSE
    /\ exec = "none" /\ pub = "none" /\ fails = 0 /\ boots = 0

\* codex_policy.rs:33-58,330-342: an absent marker on the canary pane recovers the durable
\* verified pin (VerifiedUnavailable); a Herdr nonce's context has no source policy, so Legacy
\* (:60-64). mut_pin_drop ignores the pin.
PinBlocks == marker = "absent" /\ Fix # "mut_pin_drop"
\* actor.rs:280,690-693 -> deliver.rs:140-148 -> codex_policy.rs:193-197.
SourcesAllowed == ~PinBlocks
Accepts == writer = "resumed"           \* host.rs:248-259
Vacant == ~tmuxLive                     \* absent_open.rs:91-95, both rollout paths

\* Placement reads no row (intake_route/input.rs judge_at): a registered blocked actor
\* (fresh_input.rs:6-26), no marker, the pinned target vacant.
LocalOk == writer = "blocked" /\ marker = "absent" /\ (Vacant \/ Fix = "mut_no_vacancy")
\* Condition 4. design1: no listing of the row nonce (turn_host.rs:127-131 style read).
\* design: no listing, or the exact retired nonce withheld (adopt_skip.rs:203-294).
ListingOk == IF Fix \in {"design1", "mut_cond4"} THEN listing = "none"
             ELSE listing \in {"none", "rw"}
\* The turn host adds the row: exact Retired owner, hold released, no live new incarnation.
TurnOk == LocalOk /\ row = "retired" /\ cleanup = "done" /\ exec = "none" /\ ListingOk
\* The probe and the turn host share one predicate; a ready writer also reads Ready.
ProbeReady == Accepts \/ TurnOk

\* Design arms the probe for every writer hold on the canary Herdr channel; the probe judges
\* again at each look, so a marker lost after the hold is still seen.
Arm(next) == Watching /\ next = "held"

Keep(v) == UNCHANGED v

\* host.rs:863-876, actor.rs:329-356: recover, port, hosted, first_drain; resume waits.
\* Premise: the restart witness reads (actor.rs:338-356 Ok), see design [U3].
Boot ==
    /\ writer = "booting" /\ writer' = "blocked"
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, resumedBy, intake, watch, viaBoot,
              exec, pub, fails, boots>>)

\* actor.rs:357-396: resume runs only while sources are allowed.
Resume ==
    /\ writer = "blocked" /\ SourcesAllowed
    /\ writer' = "resumed"
    /\ resumedBy' = IF marker = "absent" THEN "nothing" ELSE "evidence"
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, intake, watch, viaBoot, exec, pub,
              fails, boots>>)

NewInput ==
    /\ intake = "none" /\ intake' = "queued"
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, watch, viaBoot,
              exec, pub, fails, boots>>)

NextInput ==
    /\ intake \in {"done", "failed", "cancelled"} /\ intake' = "none"
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, watch, viaBoot,
              exec, pub, fails, boots>>)

\* owner_input.rs:173-191: writer_boot_held -> hold_start, WRITER_HELD. Design arms the
\* bootstrap probe (held_watch::arm).
Drive ==
    /\ intake = "queued"
    /\ intake' = IF Accepts \/ (Bootstrap /\ LocalOk) THEN "gateway" ELSE "held"
    /\ watch' = Arm(intake')
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, viaBoot, exec, pub,
              fails, boots>>)

\* turn_host.rs:295-315. OWriterNotReady is not a confirmed failure (start_failure.rs:16-25);
\* design holds the front and arms the probe (owner_input.rs:964-985 turn-mode pattern).
TurnHost ==
    /\ intake = "gateway"
    /\ intake' = IF Accepts \/ (Bootstrap /\ TurnOk) THEN "admitted" ELSE "held"
    /\ viaBoot' = (~Accepts /\ Bootstrap /\ TurnOk)
    /\ watch' = Arm(intake')
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, exec, pub, fails,
              boots>>)

\* host.rs:998-1012 input_ready on the ready edge, owner_input.rs:184 boot wake.
ReadyWake ==
    /\ intake = "held" /\ Accepts
    /\ intake' = "queued" /\ watch' = FALSE
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, viaBoot, exec, pub,
              fails, boots>>)

\* held_watch.rs:81-140: each look reads the probe; the first Ready wakes the drive once.
WatchWake ==
    /\ intake = "held" /\ watch /\ ProbeReady
    /\ intake' = "queued" /\ watch' = FALSE
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, viaBoot, exec, pub,
              fails, boots>>)

\* herdr_launch.rs:484-494: Pending CAS, then the listing (nonce, false). A bootstrap
\* rechecks vacancy under the runtime hint fence first (:396, prepare at :474).
LaunchCAS ==
    /\ intake = "admitted" /\ row = "retired" /\ exec = "none"
    /\ (~viaBoot \/ Vacant \/ ~Recheck)
    /\ row' = "pending" /\ exec' = "pending" /\ listing' = "rw" /\ intake' = "cas"
    /\ Keep(<<cleanup, marker, tmuxLive, writer, resumedBy, watch, viaBoot, pub, fails, boots>>)

\* codex/herdr_turn.rs prepare: the recheck fails before the CAS; the turn ends with that error
\* through the existing launch failure path, without a bootstrap hold or watch.
LaunchRefused ==
    /\ intake = "admitted" /\ row = "retired" /\ viaBoot /\ Recheck /\ ~Vacant
    /\ intake' = "failed" /\ watch' = FALSE
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, viaBoot, exec, pub,
              fails, boots>>)

\* herdr_launch.rs:495-505 then the create: the spawn marker is published.
MarkerOk ==
    /\ intake = "cas" /\ marker' = "n2" /\ intake' = "launched"
    /\ Keep(<<row, cleanup, listing, tmuxLive, writer, resumedBy, watch, viaBoot, exec, pub,
              fails, boots>>)

\* tmux_common/spawn_markers.rs:323-352 removes the torn marker; herdr_launch.rs:508-510,590-632
\* retire_unsent: the Pending goes back to Retired, the listing (n2, false) stays, no hold.
MarkerFail ==
    /\ intake = "cas" /\ fails < FailBudget
    /\ row' = "retired" /\ exec' = "none" /\ cleanup' = "done" /\ intake' = "failed"
    /\ fails' = fails + 1
    /\ Keep(<<marker, listing, tmuxLive, writer, resumedBy, watch, viaBoot, pub, boots>>)

\* codex/herdr_turn.rs:158-162: a Pending row of this owner is reused.
AdmitPending ==
    /\ intake = "admitted" /\ row = "pending" /\ exec = "pending"
    /\ intake' = "launched"
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, watch, viaBoot, exec,
              pub, fails, boots>>)

Followup ==
    /\ intake = "admitted" /\ row = "bound"
    /\ intake' = "done"
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, watch, viaBoot, exec,
              pub, fails, boots>>)

\* First prompt and launch evidence bind the pane; herdr_source.rs:193 admits the listing.
Bind ==
    /\ intake = "launched" /\ exec = "pending"
    /\ row' = "bound" /\ exec' = "bound" /\ listing' = "ra" /\ intake' = "done"
    /\ Keep(<<cleanup, marker, tmuxLive, writer, resumedBy, watch, viaBoot, pub, fails, boots>>)

\* The create was not sent after the marker: retire_unsent, marker and listing stay.
LaunchNotSent ==
    /\ intake = "launched" /\ exec = "pending" /\ fails < FailBudget
    /\ row' = "retired" /\ cleanup' = "done" /\ exec' = "none" /\ intake' = "failed"
    /\ fails' = fails + 1
    /\ Keep(<<marker, listing, tmuxLive, writer, resumedBy, watch, viaBoot, pub, boots>>)

\* unplaced.rs:454: a Pending row no live launch holds is retired.
Unplaced ==
    /\ exec = "pending" /\ intake \notin {"cas", "launched", "admitted"}
    /\ row' = "retired" /\ cleanup' = "done" /\ exec' = "none"
    /\ Keep(<<marker, listing, tmuxLive, writer, resumedBy, intake, watch, viaBoot, pub, fails,
              boots>>)

Publish ==
    /\ writer = "resumed" /\ exec = "bound" /\ pub = "none" /\ pub' = "n2"
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, intake, watch,
              viaBoot, exec, fails, boots>>)

PublishOld ==
    /\ writer = "resumed" /\ tmuxLive /\ pub # "old" /\ pub' = "old"
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, intake, watch,
              viaBoot, exec, fails, boots>>)

\* herdr_terminate.rs:223-228 retire_confirmed (Terminate, stop, clear, row end). Design
\* withholds the exact nonce after the CAS; the base leaves an admitted listing.
Retire ==
    /\ exec = "bound"
    /\ row' = "retired" /\ exec' = "none" /\ cleanup' = "pending"
    /\ listing' = IF Withhold THEN "rw" ELSE "ra"
    /\ Keep(<<marker, tmuxLive, writer, resumedBy, intake, watch, viaBoot, pub, fails, boots>>)

\* root_replaced.rs:570-588: runtime marker, hold release, empty settlement; no owner wake.
CleanupDone ==
    /\ cleanup = "pending" /\ cleanup' = "done"
    /\ Keep(<<row, marker, listing, tmuxLive, writer, resumedBy, intake, watch, viaBoot, exec,
              pub, fails, boots>>)

\* tmux_common.rs:1222-1243 via a name-keyed cleanup.
MarkerLost ==
    /\ marker = "n2" /\ exec = "none" /\ marker' = "absent"
    /\ Keep(<<row, cleanup, listing, tmuxLive, writer, resumedBy, intake, watch, viaBoot, exec,
              pub, fails, boots>>)

TmuxEnds ==
    /\ tmuxLive /\ tmuxLive' = FALSE
    /\ Keep(<<row, cleanup, marker, listing, writer, resumedBy, intake, watch, viaBoot, exec,
              pub, fails, boots>>)

\* Out-of-band tmux of the same logical key in the bootstrap window: no new incarnation, no
\* marker, the writer not ready. Outside it the ordinary launch has no vacancy check at all
\* (herdr_launch.rs:499 before :502), a pre-existing gap left out of this model (design F1).
TmuxAppears ==
    /\ Appear /\ ~tmuxLive /\ exec = "none" /\ intake # "cas"
    /\ marker = "absent" /\ writer # "resumed"
    /\ tmuxLive' = TRUE
    /\ Keep(<<row, cleanup, marker, listing, writer, resumedBy, intake, watch, viaBoot, exec,
              pub, fails, boots>>)

\* A restart loses memory (listing, watches, launches); boot custody keeps the front.
Restart ==
    /\ boots < BootBudget /\ boots' = boots + 1
    /\ writer' = "booting" /\ resumedBy' = "none" /\ listing' = "none" /\ watch' = FALSE
    /\ viaBoot' = FALSE
    /\ intake' = IF intake \in {"held", "gateway", "admitted", "cas", "launched"}
                 THEN "queued" ELSE intake
    /\ Keep(<<row, cleanup, marker, tmuxLive, exec, pub, fails>>)

Cancel ==
    /\ intake \in {"queued", "held", "gateway", "admitted", "launched"}
    /\ intake' = "cancelled" /\ watch' = FALSE
    /\ Keep(<<row, cleanup, marker, listing, tmuxLive, writer, resumedBy, viaBoot, exec, pub,
              fails, boots>>)

Next ==
    \/ NewInput \/ NextInput \/ Boot \/ Resume \/ Drive \/ TurnHost \/ ReadyWake \/ WatchWake
    \/ LaunchCAS \/ LaunchRefused \/ MarkerOk \/ MarkerFail \/ AdmitPending \/ Followup
    \/ Bind \/ LaunchNotSent \/ Unplaced \/ Publish \/ PublishOld \/ Retire \/ CleanupDone
    \/ MarkerLost \/ TmuxEnds \/ TmuxAppears \/ Restart \/ Cancel
    \/ UNCHANGED vars

\* The actor loop, the held_watch look and the unplaced resolver read again and again, so a
\* step they enable infinitely often is taken (SF). A later input is assumed only with NextFair.
Spec == Init /\ [][Next]_vars
    /\ WF_vars(NewInput) /\ WF_vars(Boot) /\ SF_vars(Resume) /\ WF_vars(Drive)
    /\ WF_vars(TurnHost) /\ WF_vars(ReadyWake) /\ SF_vars(WatchWake) /\ WF_vars(LaunchCAS)
    /\ WF_vars(LaunchRefused) /\ WF_vars(MarkerOk) /\ WF_vars(AdmitPending)
    /\ WF_vars(Followup) /\ WF_vars(Bind) /\ SF_vars(Unplaced) /\ WF_vars(CleanupDone)
    /\ (IF NextFair THEN WF_vars(NextInput) ELSE TRUE)

\* INVARIANT: no new incarnation while a tmux execution of the logical key runs.
NoDualExecution == ~(tmuxLive /\ exec # "none")
\* INVARIANT: the writer never publishes a source outside the current incarnation's ownership.
SourceInBoundary == pub # "old"
\* INVARIANT: source permission comes only from a current incarnation's marker.
ResumedOnlyWithEvidence == resumedBy # "nothing"
\* PROPERTY: a held front eventually starts, or ends.
HeldStarts == (intake = "held") ~> (intake \in {"done", "failed", "cancelled"})
\* PROPERTY: the writer eventually becomes ready once a front is held.
WriterReady == (intake = "held") ~> (writer = "resumed" \/ intake = "cancelled")
\* PROPERTY: a Pending row that no launch holds does not stay.
NoOrphanPending == (exec = "pending" /\ intake \notin {"cas", "launched", "admitted"})
                   ~> (exec # "pending" \/ intake \in {"cas", "launched", "admitted"})
====
