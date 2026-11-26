---------------------------- MODULE ThreadHostS1b ----------------------------
(* #8118 S1b: commands, cleanup and input consent act on the host decided from a thread's
   execution now (host_decision.rs), and a thread's recorded execution follows the evidence
   a turn verified, not the turn's permission (turn_host.rs recorded_execution).
   Written against 2668bdeefd (S1a head) plus the S1b change.

   Mode "S1bFirst" is the first S1b head 9e28c137ae: the key holders A2/A6/A9 derived a token from
   the key (token_hash_of) and A9 read provider and key only from the slot's thread_channel_id row.
   Mode "Before" is the S1a head (A1-A9, #24 and the consent read the channel's own boot entry;
   a refused turn forgets the record). "Rev4" is design delta 4 taken literally (one Option read
   from the process record by channel only; a thread's input always asks its parent's consent).
   "After" is the S1b code. "S2Seed" is the contract the activation slice must meet: boot restore
   seeds the record and readers wait for it. "Mut*" removes one rule from After (S2Seed for
   MutSeedLate).

   Entries come only from the entry table (design-hh-5 1.2, 2.2, delta 4):
     BootHerdr   channel with its own boot Herdr entry
     BootTmux    channel no boot entry names
     LiveTmux    thread under a Herdr parent, legacy row, tmux Present
     DeadTmux    thread under a Herdr parent, legacy row, tmux Missing
     ProbeFail   thread under a Herdr parent, legacy row, tmux probe failed
     HerdrBound  thread under a Herdr parent, Bound row at the parent's endpoint (S2 only)
     Unresolved  thread whose parent the boot view holds unresolved
     SlotGc      slot thread under a Herdr parent whose sessions rows thread_gc deleted
                 (db/dispatched_sessions/thread_gc.rs:92-118); thread_id_map still names it
     LegacyKey   thread under a Herdr parent, live tmux, a legacy `host:tmux` session key that
                 SessionIdentity::parse reads with no token (domain/session_identity.rs:59-64)
   SlotGc and LegacyKey reach only the key holders: A2 session_resume.rs:408, A6
   inflight/host_recovery_guard.rs:105, A9 auto_queue/runtime.rs:487 (token_hash_of callers at
   9e28c137ae: session_resume.rs:406, host_recovery_guard.rs:104, auto_queue/runtime.rs:552). *)
EXTENDS Naturals

CONSTANTS Mode, Entries

VARIABLES entry, proof, cache, restarted, sinceRestart, restored,
          restartHost, tmuxOp, relaunch, consent, sync,
          \* S2 placement section (SpecS2); constant under Spec.
          exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle, slot,
          judged, unsent, receipt

vars == <<entry, proof, cache, restarted, sinceRestart, restored,
          restartHost, tmuxOp, relaunch, consent, sync>>
s2vars == <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle, slot,
            judged, unsent, receipt>>
allvars == <<vars, s2vars>>

Threads == {"LiveTmux", "DeadTmux", "ProbeFail", "HerdrBound", "Unresolved", "SlotGc", "LegacyKey"}
KeyHolders == {"SlotGc", "LegacyKey"}
Seeded == Mode \in {"S2Seed", "MutSeedLate"}

\* host_decision.rs decide: own boot entry, else relation, else the execution observed now
\* (row via resolve_session_row_pg + read_row, then platform/tmux.rs session_presence).
Observed == CASE entry = "BootHerdr" -> "Herdr"
              [] entry = "BootTmux" -> "Tmux"
              [] entry = "Unresolved" -> "Hold"
              [] entry = "LiveTmux" -> "Tmux"
              [] entry = "DeadTmux" -> IF Mode = "MutNoStageGate" THEN "Herdr" ELSE "Tmux"
              [] entry = "ProbeFail" -> "Hold"
              [] entry = "HerdrBound" -> "Herdr"
              \* A1 and the others read the canonical row under the runtime's token: none here.
              [] entry \in KeyHolders -> "Tmux"
\* MutCacheOnly: a thread decided from the process record instead of its execution now.
FromCache == CASE cache = "Herdr" -> "Herdr" [] cache = "Tmux" -> "Tmux" [] OTHER -> "Hold"
Decision == IF Mode = "MutCacheOnly" /\ entry \in Threads \ {"Unresolved"}
            THEN FromCache ELSE Observed

\* Before: operation_policy.rs configured_refusal = herdr_endpoint(channel), own entry only.
BootSome == entry = "BootHerdr"
\* Rev4: the same Option, Some for a thread recorded Herdr or with nothing recorded.
Rev4Some == \/ entry = "BootHerdr"
            \/ entry = "Unresolved"
            \/ (entry \in Threads /\ cache \in {"Herdr", "None"})

\* A1 restart.rs restart_channel: Herdr -> herdr::run, Tmux -> tmux restart, Hold -> refused.
RestartOutcome ==
    CASE Mode = "Before" -> IF BootSome THEN "Herdr" ELSE "Tmux"
      [] Mode = "Rev4"   -> IF Rev4Some THEN "Herdr" ELSE "Tmux"
      [] OTHER -> CASE Decision = "Herdr" -> "Herdr"
                    [] Decision = "Tmux"  -> "Tmux"
                    [] OTHER -> IF Mode = "MutRestartSome" THEN "Herdr" ELSE "Refused"
\* A2-A9 tmux_refusal: only Tmux goes on to the tmux path (its row checks are outside).
\* What the key holders A2/A6/A9 decide for SlotGc and LegacyKey.
\* S1bFirst: runtime.rs:552 token_hash_of(None key) -> host_decision.rs:119 ProviderUnknown (SlotGc);
\*   token_hash_of(legacy key) = None -> host_decision.rs:152 "no database or token" (LegacyKey).
\* After: host_decision.rs:105-117 rows read, none -> :203 Nothing -> Tmux (SlotGc);
\*   :86 key in hand -> :250 the key finds the row -> tmux probe -> Tmux (LegacyKey).
KeyHolderDecision ==
    CASE Mode = "S1bFirst" -> "Hold"
      [] Mode = "MutRowlessHold" /\ entry = "SlotGc" -> "Hold"
      [] Mode = "MutKeyDropped" /\ entry = "LegacyKey" -> "Hold"
      [] OTHER -> "Tmux"
TmuxOutcome ==
    CASE Mode = "Before" -> IF BootSome THEN "Kept" ELSE "Done"
      [] Mode = "Rev4"   -> IF Rev4Some THEN "Kept" ELSE "Done"
      [] entry \in KeyHolders -> IF KeyHolderDecision = "Tmux" THEN "Done" ELSE "Kept"
      [] OTHER -> IF Decision = "Tmux" THEN "Done" ELSE "Kept"
\* #24 provider_settings.rs claude_local_tui_relaunch_applies via R1-R4 (a Claude TUI).
RelaunchOutcome ==
    CASE Mode = "Before" -> IF BootSome THEN "Skipped" ELSE "Applied"
      [] Mode = "Rev4"   -> IF Rev4Some THEN "Skipped" ELSE "Applied"
      [] OTHER -> IF Decision = "Tmux" THEN "Applied" ELSE "Skipped"
\* W2 herdr_consent/admission.rs check: asked -> proof Pending holds, a Bound row passes
\* (assess :140-146), else the proof passes. Not asked: the input goes on unchanged.
Asked == CASE entry = "BootHerdr" -> TRUE
           [] Mode = "Before" -> FALSE
           [] Mode \in {"Rev4", "MutConsentAlways"} -> entry \in Threads \ {"Unresolved"}
           [] Mode = "MutConsentSkip" -> FALSE
           [] OTHER -> entry \in Threads /\ Decision = "Herdr"
ConsentOutcome ==
    IF ~Asked THEN "NotAsked"
    ELSE IF entry = "HerdrBound" \/ proof = "Ok" THEN "Passed" ELSE "Held"

\* S1a synchronous readers #15 (inject.rs !runs_herdr), #43 (nameless_gate probe unless Herdr),
\* #45 (busy view unless next_is_herdr): a recorded Herdr takes the Herdr path, anything else,
\* Unknown included, keeps the tmux path as the base did.
SyncOutcome == CASE cache = "Herdr" -> "HerdrPath"
                 [] cache = "None" /\ Mode = "MutUnknownSkipsTmux" -> "Skipped"
                 [] OTHER -> "TmuxPath"

\* ---- S2 placement section: definitions used by Init ----
S2Entries == {"NewThread", "FreshAfterKill", "ResumeTmux", "TmuxParentThread",
              "LiveTmuxNoSel", "SelUnknown", "RetiredNative"}
HerdrParentS2 == S2Entries \ {"TmuxParentThread"}
S2Init == /\ exec = CASE entry \in {"FreshAfterKill", "ResumeTmux"} -> "LegacyDead"
                      [] entry = "LiveTmuxNoSel" -> "Tmux"
                      [] entry = "SelUnknown" -> "Unread"
                      [] entry = "RetiredNative" -> "Retired"
                      [] OTHER -> "None"
          /\ made = FALSE /\ tabs = 0 /\ tmuxs = 0
          /\ inq = IF entry \in S2Entries THEN "Queued" ELSE "None"
          /\ submits = 0
          \* turn_host.rs:446 o_writer_ready(child, kind) at the first judgement; a child that
          \* already ran on Herdr (RetiredNative) has its writer.
          /\ oready \in (IF entry \in HerdrParentS2 \ {"RetiredNative"} THEN BOOLEAN ELSE {TRUE})
          \* Late origin (tui_o/cutover/claude_thread.rs:14-22, Deferred or parked) or an ordinary
          \* fresh adoption (channel_policy/children.rs:205-212 Adoption::Pending).
          /\ late \in (IF entry \in HerdrParentS2 THEN BOOLEAN ELSE {FALSE})
          /\ epoch = 0 /\ restarts2 = 0 /\ idle = FALSE /\ slot = "Bound"
          /\ judged = "None" /\ unsent = FALSE /\ receipt = "None"

Init == /\ entry \in Entries
        /\ proof \in {"Ok", "Pending"}
        \* A running process recorded the pane it launched; nothing else is judged yet.
        /\ cache = IF entry = "HerdrBound" THEN "Herdr" ELSE "None"
        /\ restarted = FALSE /\ sinceRestart = FALSE /\ restored = TRUE
        /\ restartHost = "None" /\ tmuxOp = "None" /\ relaunch = "None"
        /\ consent = "None" /\ sync = "None"
        /\ S2Init


CmdRestart == /\ restartHost = "None" /\ restartHost' = RestartOutcome
              /\ UNCHANGED <<entry, proof, cache, restarted, sinceRestart, restored,
                             tmuxOp, relaunch, consent, sync>>
CmdTmuxOp == /\ tmuxOp = "None" /\ tmuxOp' = TmuxOutcome
             /\ UNCHANGED <<entry, proof, cache, restarted, sinceRestart, restored,
                            restartHost, relaunch, consent, sync>>
CmdRelaunch == /\ relaunch = "None" /\ relaunch' = RelaunchOutcome
               /\ UNCHANGED <<entry, proof, cache, restarted, sinceRestart, restored,
                              restartHost, tmuxOp, consent, sync>>
Input == /\ consent = "None" /\ consent' = ConsentOutcome
         /\ UNCHANGED <<entry, proof, cache, restarted, sinceRestart, restored,
                        restartHost, tmuxOp, relaunch, sync>>

\* turn_host.rs for_turn -> record_judged. A thread at its parent's endpoint is Herdr when the
\* gate opens; a gate refusal (admission, O writer, switch) keeps the verified pane in After,
\* forgets it in Before. Legacy rows resolve to tmux. A non-child records nothing.
Turn == /\ entry \in Threads \ {"Unresolved"}
        /\ \E gate \in {"Open", "Refused"}:
             cache' = IF entry # "HerdrBound" THEN "Tmux"
                      ELSE IF gate = "Open" THEN "Herdr"
                      ELSE IF Mode \in {"Before", "MutRefusedForgets"} THEN "None"
                      ELSE "Herdr"
        \* Readers after this turn read its record: a new generation.
        /\ sinceRestart' = FALSE /\ sync' = "None"
        /\ UNCHANGED <<entry, proof, restarted, restored, restartHost, tmuxOp, relaunch,
                       consent>>

\* Process restart: the record is process memory (thread_host.rs JUDGED); readers start a new
\* generation. With a boot seed (S2) readers wait until restore completes.
Restart == /\ ~restarted
           /\ cache' = "None" /\ restarted' = TRUE /\ sinceRestart' = TRUE
           /\ restored' = ~Seeded /\ sync' = "None"
           /\ UNCHANGED <<entry, proof, restartHost, tmuxOp, relaunch, consent>>
\* S2 contract: recovery_engine/herdr_reader.rs boot Bound restore seeds the record.
BootRestore == /\ Seeded /\ ~restored
               /\ cache' = IF entry = "HerdrBound" THEN "Herdr" ELSE cache
               /\ restored' = TRUE
               /\ UNCHANGED <<entry, proof, restarted, sinceRestart, restartHost, tmuxOp,
                              relaunch, consent, sync>>
SyncRead == /\ entry \in Threads /\ sync = "None"
            /\ (Mode = "S2Seed" => restored)
            /\ sync' = SyncOutcome
            /\ UNCHANGED <<entry, proof, cache, restarted, sinceRestart, restored,
                           restartHost, tmuxOp, relaunch, consent>>

Next == CmdRestart \/ CmdTmuxOp \/ CmdRelaunch \/ Input \/ Turn \/ Restart \/ BootRestore
        \/ SyncRead

Spec == Init /\ [][Next /\ UNCHANGED s2vars]_allvars
        /\ WF_allvars(CmdRestart /\ UNCHANGED s2vars) /\ WF_allvars(CmdTmuxOp /\ UNCHANGED s2vars)
        /\ WF_allvars(CmdRelaunch /\ UNCHANGED s2vars) /\ WF_allvars(Input /\ UNCHANGED s2vars)

TmuxThread == entry \in {"LiveTmux", "DeadTmux"}

\* P1-1 (S1b review): a GC'd slot thread and a legacy-keyed thread reach the row checks; the
\* slot binding is released (auto_queue/runtime.rs:380) instead of being refused forever.
SlotRotationProceeds == entry \in KeyHolders => tmuxOp # "Kept"

\* P1-A: an undecided execution starts no host and changes no tmux session.
UnknownRunsNoHost == entry \in {"ProbeFail", "Unresolved"} =>
    /\ restartHost \in {"None", "Refused"} /\ tmuxOp # "Done" /\ relaunch # "Applied"
TmuxThreadNoHerdrOp == TmuxThread => restartHost # "Herdr"
TmuxThreadTmuxOpsAdmitted == TmuxThread => restartHost # "Refused" /\ tmuxOp # "Kept"
\* Right after a restart (nothing recorded) a live tmux thread still takes its relaunch.
LiveTmuxRelaunchApplies == entry = "LiveTmux" => relaunch # "Skipped"
HerdrNoTmuxOp == entry \in {"BootHerdr", "HerdrBound"} => restartHost # "Tmux" /\ tmuxOp # "Done"
BootUnchanged == /\ entry = "BootHerdr" => restartHost \in {"None", "Herdr"}
                 /\ entry = "BootTmux" => restartHost \in {"None", "Tmux"} /\ tmuxOp # "Kept"
                                          /\ relaunch # "Skipped" /\ consent # "Held"
\* P1-B: a tmux thread's input never waits on its parent's consent.
TmuxInputNotConsentHeld == TmuxThread => consent # "Held"
\* A Herdr thread's input is asked under its parent's consent (it passes on its Bound row).
HerdrInputUnderConsent == entry = "HerdrBound" => consent # "NotAsked"
\* Before activation every thread input's consent result is the S1a head's: not asked.
S1bChildInputKeepsMain == entry \in Threads \ {"HerdrBound"} => consent \in {"None", "NotAsked"}
\* P2-1: a reader's behaviour on Unknown, not the raw value. A live tmux thread keeps the tmux
\* path whatever is recorded.
TmuxThreadKeepsTmuxPath == entry = "LiveTmux" /\ sync # "None" => sync = "TmuxPath"
\* P2-4: only a restart (before the next turn) may leave a live Herdr pane read as tmux; a
\* refused turn does not.
RefusalKeepsLivePane == entry = "HerdrBound" /\ sync = "TmuxPath" => sinceRestart
\* P2-1 restore contract: a live Herdr pane is never read as tmux, a restart included.
RestoredBeforeRead == entry = "HerdrBound" => sync # "TmuxPath"

CommandsAnswer == <>(restartHost # "None" /\ tmuxOp # "None" /\ relaunch # "None"
                     /\ consent # "None")

\* ---- S2 placement section (SpecS2), written against main b04ea56650 ----
\* Mode "Main" is b04ea56650 (S1a and S1b landed). "S2Naive" is the activation alone: turn_host
\* sends a Herdr parent's thread without a selector to configured_turn, and nothing else changes
\* (no first launch permit, no tmux probe in turn_host, boot unplaced pass, idle cleanup, slot
\* gates and recorded_execution as in main). "S2" is the contract of design-s2-1 (refs/review/
\* i8118-s2-prep-1 input). "MutS2*" removes one rule from S2.
\* Entries (s2-prep entry table; s2-design-check for the last three):
\*   NewThread         Herdr parent, new thread, no row, no selector (first human/bot message)
\*   FreshAfterKill    Herdr parent, legacy row, tmux killed and selector cleared (11-26 18:12)
\*   ResumeTmux        Herdr parent, legacy row, tmux Missing, selector kept (existing tmux thread)
\*   TmuxParentThread  thread under a tmux parent
\*   LiveTmuxNoSel     Herdr parent, legacy row, tmux Present, no selector
\*   SelUnknown        Herdr parent, the row (selector) read fails (turn_host.rs:233-238)
\*   RetiredNative     Herdr parent, own Retired Herdr row with its native ID
\* exec: None (no row), LegacyDead, Tmux (live tmux), Unread (row unreadable), Pending (Herdr row
\* without location, herdr_launch.rs:550), Placed (location and pane adopted, not bound), Bound,
\* Retired, Gone (row deleted by thread_gc).
S2Like == Mode \in {"S2", "MutS2NoPermit", "MutS2NoBootResolve", "MutS2NoRelease",
                    "MutS2TabNoMarker", "MutS2SelectorIgnored", "MutS2ReplayResubmit",
                    "MutS2ProbeSkipped", "MutS2NoFreshSuccessor", "MutS2SeedOnAdopt",
                    "MutS2PlanSeeds", "MutS2IgnoreVerified", "MutS2SlotFuturePolicy"}
Selector == entry = "ResumeTmux"
HerdrExec == {"Pending", "Placed", "Bound"}
FreshStart == {"None", "LegacyDead", "Retired", "Gone"}

\* turn_host.rs resolve: own Retired Herdr goes back to configured_turn; main answers Tmux for every
\* thread without a Bound row at the parent's endpoint (:239-242); delta 4: a selector keeps tmux.
LaunchHost == CASE entry = "TmuxParentThread" -> "Tmux"
                [] Mode = "Main" -> "Tmux"
                [] exec = "Retired" -> "Herdr"
                [] Selector /\ Mode # "MutS2SelectorIgnored" -> "Tmux"
                [] OTHER -> "Herdr"
\* turn_host reads the row only (turn_host.rs:219-243, no tmux presence): activated alone it starts
\* Herdr beside a live tmux without a selector. S2 shares host_decision's probe (:283-289).
LiveTmuxSeenDead == entry = "LiveTmuxNoSel" /\ exec = "Tmux" /\ inq = "Queued"
                    /\ Mode \in {"S2Naive", "MutS2ProbeSkipped"}
\* Without a permit a late child's O writer is not ready: turn_host.rs:446-458 OWriterNotReady.
PermitMissing == Mode \in {"S2Naive", "MutS2NoPermit"}
\* An unplaced fresh run retired before its create (KnownNotSent) lets the next nonce take a permit.
SuccessorWithoutPermit == Mode = "MutS2NoFreshSuccessor" /\ exec = "Retired" /\ unsent
\* Boot unplaced pass: herdr_reader.rs:363 reads herdr_endpoint(owner.channel_id), own boot only.
BootResolves == S2Like /\ Mode # "MutS2NoBootResolve"
\* Idle release of a Herdr thread: session_idle_cleanup.rs:75-81 skips any Herdr decision.
HerdrIdleReleased == S2Like /\ Mode # "MutS2NoRelease"
\* Slot gate on a Retired row: runtime.rs:555-590 row_host_refusal admits a confirmed legacy row
\* only (host_teardown_gate.rs:223-251); S2 admits the exact Retired row.
RetiredSlotReleasable == S2Like
\* MutS2SlotFuturePolicy: the slot asks the next host (runtime.rs:498-507 host_decision_for_rows
\* once no execution answers Herdr) instead of the release state, so a released thread whose next
\* run is Herdr is never reset.
SlotAsksNextHost == Mode = "MutS2SlotFuturePolicy" /\ LaunchHost = "Herdr"
Released == {"LegacyDead", "Retired", "Gone"}

\* intake_turn.rs:1940 for_turn -> provider_dispatch.rs:360 drive. Tmux: claude.rs:1799 and the
\* guards tui_session_launch.rs:91-94,106-108 (own boot entry only), then claude_tui/session.rs:499.
\* Herdr: Pending row first. An ordinary fresh adoption whose writer is not ready waits in the
\* owner's WRITER_HELD hold (provider_dispatch/owner_input.rs:192-224) in every mode.
Launch == /\ UNCHANGED vars /\ entry \in S2Entries /\ inq = "Queued"
          /\ (exec \in FreshStart \/ LiveTmuxSeenDead)
          /\ ~(LaunchHost = "Herdr" /\ ~oready /\ ~late)
          /\ ~(LaunchHost = "Herdr" /\ ~oready /\ late /\ SuccessorWithoutPermit)
          /\ IF LaunchHost = "Tmux"
             THEN /\ exec' = "Tmux" /\ tmuxs' = tmuxs + 1
                  /\ inq' = "Submitted" /\ submits' = submits + 1 /\ receipt' = "Sent"
                  /\ UNCHANGED <<made, epoch, judged>>
             ELSE IF PermitMissing /\ ~oready /\ late
                  \* turn_host.rs:171-187 records the gate refusal as a current Herdr.
                  THEN /\ inq' = "Refused" /\ judged' = "Herdr"
                       /\ UNCHANGED <<exec, made, tmuxs, submits, epoch, receipt>>
                  ELSE /\ exec' = "Pending" /\ made' = FALSE /\ epoch' = restarts2
                       \* MutS2PlanSeeds: a fresh plan recorded as the current execution.
                       /\ judged' = IF Mode = "MutS2PlanSeeds" THEN "Herdr" ELSE judged
                       /\ UNCHANGED <<tmuxs, inq, submits, receipt>>
          /\ UNCHANGED <<tabs, oready, late, restarts2, idle, slot, unsent>>
\* writer/host/first_init.rs:282 activate_owner: an ordinary adoption's writer becomes ready.
OrdinaryReady == /\ UNCHANGED vars /\ entry \in HerdrParentS2 /\ ~late /\ ~oready
                 /\ oready' = TRUE
                 /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, late, epoch, restarts2,
                                idle, slot, judged, unsent, receipt>>
\* A gate refusal (admission, O writer) of an unverified thread. S2 records nothing; main and
\* MutS2IgnoreVerified record a current Herdr (turn_host.rs:171-187, no verified_bound).
GateRefused == /\ UNCHANGED vars /\ Mode = "MutS2IgnoreVerified" /\ inq = "Queued"
               /\ exec \in FreshStart /\ LaunchHost = "Herdr" /\ judged = "None"
               /\ judged' = "Herdr"
               /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch,
                              restarts2, idle, slot, unsent, receipt>>
\* The row read recovers (turn_host.rs:233-238 refuses it in main; S2 holds the input).
ReadRecovers == /\ UNCHANGED vars /\ exec = "Unread" /\ exec' = "LegacyDead"
                /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                               idle, slot, judged, unsent, receipt>>
ReadRefused == /\ UNCHANGED vars /\ exec = "Unread" /\ inq = "Queued" /\ ~S2Like
               /\ inq' = "Refused"
               /\ UNCHANGED <<exec, made, tabs, tmuxs, submits, oready, late, epoch, restarts2,
                              idle, slot, judged, unsent, receipt>>
\* At most two panes per thread keep the model finite (only MutS2TabNoMarker reaches two).
\* session_host/herdr/launch_host.rs:594-655 create in this process: answered (location recorded,
\* Placed) or answer lost (Pending).
Create == /\ UNCHANGED vars /\ exec = "Pending" /\ epoch = restarts2 /\ ~made /\ tabs < 2
          /\ made' = TRUE /\ tabs' = tabs + 1
          /\ exec' \in {"Placed", "Pending"}
          /\ UNCHANGED <<tmuxs, inq, submits, oready, late, epoch, restarts2, idle, slot, judged,
                         unsent, receipt>>
\* Unplaced resolution (herdr_launch/unplaced.rs:229-311; after a restart the boot pass): a found
\* pane is adopted with its location, not bound (Resolution::Adopted); none retires the row with
\* no create sent. MutS2TabNoMarker cannot find a tab. MutS2SeedOnAdopt seeds the adoption.
Resolve == /\ UNCHANGED vars /\ exec = "Pending"
           /\ IF epoch = restarts2 THEN made ELSE BootResolves
           /\ exec' = IF made /\ Mode # "MutS2TabNoMarker" THEN "Placed" ELSE "Retired"
           /\ unsent' = ~made
           /\ judged' = IF made /\ Mode = "MutS2SeedOnAdopt" THEN "Herdr" ELSE judged
           /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle,
                          slot, receipt>>
\* Own SessionStart and source checked, then bind_pg (tui_prompt_relay/herdr_source.rs:185,267);
\* the current execution is recorded here. After a restart only a boot that resolves the
\* parent's endpoint reaches it.
Bind == /\ UNCHANGED vars /\ exec = "Placed" /\ (epoch = restarts2 \/ BootResolves)
        /\ exec' = "Bound" /\ judged' = "Herdr"
        /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle,
                       slot, unsent, receipt>>
\* The replayed input meets the unresolved Pending row: turn_host.rs:239-241
\* HostedHerdrUnconfigured. S2 holds it until the row resolves.
Rejudge == /\ UNCHANGED vars /\ exec \in {"Pending", "Placed"} /\ epoch # restarts2
           /\ inq = "Queued" /\ ~BootResolves
           /\ inq' = "Refused"
           /\ UNCHANGED <<exec, made, tabs, tmuxs, submits, oready, late, epoch, restarts2, idle,
                          slot, judged, unsent, receipt>>
\* claude/herdr_turn/first_input.rs:269-333 submit_owned: the first input once. A live tmux takes
\* its input on the existing tmux path.
SubmitHerdr == /\ UNCHANGED vars /\ exec = "Bound" /\ inq = "Queued"
               /\ inq' = "Submitted" /\ submits' = submits + 1 /\ receipt' = "Sent"
               /\ UNCHANGED <<exec, made, tabs, tmuxs, oready, late, epoch, restarts2, idle, slot,
                              judged, unsent>>
SubmitTmux == /\ UNCHANGED vars /\ exec = "Tmux" /\ inq = "Queued" /\ ~LiveTmuxSeenDead
              /\ inq' = "Submitted" /\ submits' = submits + 1 /\ receipt' = "Sent"
              /\ UNCHANGED <<exec, made, tabs, tmuxs, oready, late, epoch, restarts2, idle, slot,
                             judged, unsent>>
\* Success removes the original with its terminal identity (queued_head/v2/retained.rs:122-146).
Settle == /\ UNCHANGED vars /\ receipt = "Sent" /\ receipt' = "Settled"
          /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                         idle, slot, judged, unsent>>
\* Process restart: rows and panes stay; a sent, unsettled input is kept Unconfirmed
\* (retained.rs:77). MutS2ReplayResubmit replays a submitted input.
Restart2 == /\ UNCHANGED vars /\ restarts2 = 0 /\ restarts2' = 1
            /\ inq' = IF Mode = "MutS2ReplayResubmit" /\ inq = "Submitted" /\ exec = "Bound"
                      THEN "Queued" ELSE inq
            /\ receipt' = IF receipt = "Sent" THEN "Unconfirmed" ELSE receipt
            \* JUDGED is process memory (thread_host.rs:87); S2 seeds a Bound row before any reader
            \* starts (design C9, runtime_bootstrap.rs:302-311).
            /\ judged' = IF S2Like /\ exec = "Bound" THEN "Herdr" ELSE "None"
            /\ UNCHANGED <<exec, made, tabs, tmuxs, submits, oready, late, epoch, idle, slot,
                           unsent>>
\* The turn ended and the thread stayed idle past thread_gc's hour (db/.../thread_gc.rs:69).
Idle == /\ UNCHANGED vars /\ exec \in {"Tmux", "Bound"} /\ inq = "Submitted" /\ ~idle
        /\ idle' = TRUE
        /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                       slot, judged, unsent, receipt>>
\* session_idle_cleanup.rs:87 skips a channel whose mailbox holds a turn: no release under a queued
\* input. A released execution is no longer recorded (thread_host.rs:90-97 forgets on None).
ReleaseIdle == /\ UNCHANGED vars /\ idle /\ inq # "Queued"
               /\ \/ exec = "Tmux" /\ exec' = "LegacyDead" /\ tabs' = tabs
                  \/ exec = "Bound" /\ HerdrIdleReleased /\ exec' = "Retired" /\ tabs' = tabs - 1
               /\ judged' = "None"
               /\ UNCHANGED <<made, tmuxs, inq, submits, oready, late, epoch, restarts2, idle,
                              slot, unsent, receipt>>
\* thread_gc deletes only a legacy or retired row with no tmux session
\* (hosted_cleanup/thread_gc.rs:78-89, migrations 0131 agentdesk_hosted_execution_deletable).
Gc == /\ UNCHANGED vars /\ idle /\ exec \in {"LegacyDead", "Retired"} /\ exec' = "Gone"
      /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle, slot,
                     judged, unsent, receipt>>
\* auto_queue/runtime.rs:498-507 host_decision_for_rows: Bound -> Herdr, Pending -> Hold: skipped.
SlotReset == /\ UNCHANGED vars /\ idle /\ slot = "Bound"
             /\ \/ exec \in {"LegacyDead", "Gone", "Tmux"}
                \/ exec = "Retired" /\ RetiredSlotReleasable
             /\ ~(exec \in Released /\ SlotAsksNextHost)
             /\ slot' = "Reset"
             /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                            idle, judged, unsent, receipt>>

NextS2 == Launch \/ OrdinaryReady \/ GateRefused \/ ReadRecovers \/ ReadRefused \/ Create
          \/ Resolve \/ Bind \/ Rejudge \/ SubmitHerdr \/ SubmitTmux \/ Settle \/ Restart2
          \/ Idle \/ ReleaseIdle \/ Gc \/ SlotReset
SpecS2 == Init /\ [][NextS2]_allvars
          /\ WF_allvars(Launch) /\ WF_allvars(OrdinaryReady) /\ WF_allvars(ReadRecovers)
          /\ WF_allvars(Create) /\ WF_allvars(Resolve) /\ WF_allvars(Bind)
          /\ WF_allvars(SubmitHerdr) /\ WF_allvars(SubmitTmux) /\ WF_allvars(Settle)
          /\ WF_allvars(Idle) /\ WF_allvars(ReleaseIdle) /\ WF_allvars(Gc) /\ WF_allvars(SlotReset)

\* A new execution under a Herdr parent is Herdr: no tmux session, after restart, thread_gc and
\* slot reset included (every relaunch goes through Launch).
NewThreadFollowsParent == entry \in {"NewThread", "FreshAfterKill"} => tmuxs = 0
ExistingTmuxStaysTmux == entry = "ResumeTmux" => tabs = 0 /\ exec \notin HerdrExec
TmuxParentStaysTmux == entry = "TmuxParentThread" => tabs = 0 /\ exec \notin HerdrExec
NoInputRefused == inq # "Refused"
SingleSubmit == submits <= 1
NoDuplicatePane == tabs <= 1
\* design-s2-1 7.2: a live tmux without a selector stays tmux; an own Retired Herdr follows Herdr;
\* the current execution is recorded only for a bound pane (a plan, an adoption or an unverified
\* refusal is not one).
LiveTmuxStaysTmux == entry = "LiveTmuxNoSel" => tabs = 0 /\ exec \notin HerdrExec
RetiredFollowsHerdr == entry = "RetiredNative" => tmuxs = 0
SeedMatchesBound == judged = "Herdr" => exec = "Bound"
\* Liveness: the first input is placed and submitted; an idle execution and its slot are released.
FirstInputResolves == <>(submits >= 1)
EventuallyReleased == [](idle => <>(exec \in Released))
SlotRotates == [](idle => <>(slot = "Reset"))
\* Replay: a sent input ends settled or kept Unconfirmed; success is not claimed without its
\* receipt, so FirstInputSettles breaks where a restart lands between the key and the receipt.
FirstInputClassified == <>(receipt \in {"Settled", "Unconfirmed"})
FirstInputSettles == <>(receipt = "Settled")
=============================================================================
