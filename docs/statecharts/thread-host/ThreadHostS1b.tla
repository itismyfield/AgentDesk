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
          judged, unsent, receipt,
          \* design w2 (review-design-s2-1 P1 1,2,3,5,7)
          reserved, hkey, boot, other, v2, rst,
          \* design w3 (review-design-s2-2 P1 1,2)
          ran, closed, succ, archived, hitLive,
          \* design w4 (review-design-s2-3 P1 1)
          gone, gen, goneGen, lostSucc

vars == <<entry, proof, cache, restarted, sinceRestart, restored,
          restartHost, tmuxOp, relaunch, consent, sync>>
s2vars == <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle, slot,
            judged, unsent, receipt>>
w2vars == <<reserved, hkey, boot, other, v2, rst>>
w3vars == <<ran, closed, succ, archived, hitLive>>
w4vars == <<gone, gen, goneGen, lostSucc>>
allvars == <<vars, s2vars, w2vars, w3vars, w4vars>>

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
              "LiveTmuxNoSel", "SelUnknown", "RetiredNative",
              "RetryNew", "DispatchNew", "StaleBound", "RestartVacant",
              "LostCommand", "LostCommandHeld", "SlotSuccessor",
              "LostAfterKey", "LostCommandShellUsed"}
HerdrParentS2 == S2Entries \ {"TmuxParentThread"}
S2Init == /\ exec = CASE entry \in {"FreshAfterKill", "ResumeTmux"} -> "LegacyDead"
                      [] entry = "LiveTmuxNoSel" -> "Tmux"
                      [] entry = "SelUnknown" -> "Unread"
                      [] entry = "RetiredNative" -> "Retired"
                      [] entry = "StaleBound" -> "Bound"
                      [] OTHER -> "None"
          /\ made = FALSE /\ tabs = 0 /\ tmuxs = 0
          /\ inq = IF entry \in S2Entries \ {"StaleBound", "RestartVacant"} THEN "Queued" ELSE "None"
          /\ submits = 0
          \* turn_host.rs:446 o_writer_ready(child, kind) at the first judgement; a child that
          \* already ran on Herdr (RetiredNative) has its writer.
          /\ oready \in (IF entry \in HerdrParentS2 \ {"RetiredNative", "StaleBound"} THEN BOOLEAN
                         ELSE {TRUE})
          \* Late origin (tui_o/cutover/claude_thread.rs:14-22, Deferred or parked) or an ordinary
          \* fresh adoption (channel_policy/children.rs:205-212 Adoption::Pending).
          /\ late \in (IF entry \in HerdrParentS2 THEN BOOLEAN ELSE {FALSE})
          /\ epoch = 0 /\ restarts2 = 0 /\ idle = FALSE /\ slot = "Bound"
          /\ judged = "None" /\ unsent = FALSE /\ receipt = "None"
          \* w2: output reservation and first key (P1-1, P1-2); boot gate with a stale Bound and one
          \* other channel's work (P1-5); V2 owner adopted (P1-7); a dispatch reset pending (P1-3).
          /\ reserved = FALSE /\ hkey = FALSE
          /\ boot = IF entry = "StaleBound" THEN "Gate" ELSE "Started"
          /\ other = IF entry = "StaleBound" THEN "Waiting" ELSE "Served"
          /\ v2 = (entry # "RestartVacant")
          /\ rst = (entry = "DispatchNew")
          \* w3: the placed pane ran its command (P1-1); a successor in the released thread and
          \* the thread's archive (P1-2).
          /\ ran = TRUE /\ closed = "None" /\ succ = FALSE /\ archived = FALSE /\ hitLive = FALSE
          \* w4: the current execution's recorded pane is gone; execution generations; the
          \* generation whose pane was seen gone; a retire that hit a later generation.
          /\ gone = FALSE /\ gen = 0 /\ goneGen = 0 /\ lostSucc = FALSE

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

Spec == Init /\ [][Next /\ UNCHANGED <<s2vars, w2vars, w3vars, w4vars>>]_allvars
        /\ WF_allvars(CmdRestart /\ UNCHANGED <<s2vars, w2vars, w3vars, w4vars>>) /\ WF_allvars(CmdTmuxOp /\ UNCHANGED <<s2vars, w2vars, w3vars, w4vars>>)
        /\ WF_allvars(CmdRelaunch /\ UNCHANGED <<s2vars, w2vars, w3vars, w4vars>>) /\ WF_allvars(Input /\ UNCHANGED <<s2vars, w2vars, w3vars, w4vars>>)

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
\* i8118-s2-prep-1 input), refined by design w2 and w3. "MutS2*" removes one rule from S2;
\* "MutW1*" and "MutW3*" restore a design w1 or w2 gap.
\* Entries (s2-prep entry table; s2-design-check for the last three; w2 and w3 entries below):
\*   LostCommand       new thread whose first create was answered but its command input failed
\*   LostCommandHeld   the same, but the residue changed observably (another pane or tab in its
\*                     workspace, or another occupant than the lone root shell)
\*   LostCommandShellUsed  the same, the shell was used and is a lone root shell again: the
\*                     residue rule cannot tell it from an untouched one and disposes it
\*   LostAfterKey      a placed pane got its first key, then the pane went away (payload unknown)
\*   SlotSuccessor     a released slot thread that takes new work after the slot map moved on
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
                    "MutS2PlanSeeds", "MutS2IgnoreVerified", "MutS2SlotFuturePolicy",
                    "MutW1BindFirst", "MutW1RetrySkipsReserve", "MutW1ResetRefuses",
                    "MutW1BootGateBlocks", "MutW1RestartNoSeed",
                    "MutW3NoPlacedResidue", "MutW3ResidueByAbsence", "MutW3NoPristine",
                    "MutW3ArchiveAfterRelease", "MutW4PendingEarlyReturn", "MutW4MissingNoCas",
                    "MutW4UnreadAsMissing", "MutW4MissingResubmits"}
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
Launch == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w2vars /\ entry \in S2Entries /\ inq = "Queued"
          /\ (exec \in FreshStart \/ LiveTmuxSeenDead) /\ ~rst
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
          \* w4: each Herdr Pending is a new execution nonce (herdr_launch.rs:550).
          /\ gen' = IF exec' = "Pending" THEN gen + 1 ELSE gen
          /\ gone' = FALSE /\ UNCHANGED <<goneGen, lostSucc>>
\* writer/host/first_init.rs:282 activate_owner: an ordinary adoption's writer becomes ready.
OrdinaryReady == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ entry \in HerdrParentS2 /\ ~late /\ ~oready
                 /\ oready' = TRUE
                 /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, late, epoch, restarts2,
                                idle, slot, judged, unsent, receipt>>
\* A gate refusal (admission, O writer) of an unverified thread. S2 records nothing; main and
\* MutS2IgnoreVerified record a current Herdr (turn_host.rs:171-187, no verified_bound).
GateRefused == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ Mode = "MutS2IgnoreVerified" /\ inq = "Queued"
               /\ exec \in FreshStart /\ LaunchHost = "Herdr" /\ judged = "None"
               /\ judged' = "Herdr"
               /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch,
                              restarts2, idle, slot, unsent, receipt>>
\* The row read recovers (turn_host.rs:233-238 refuses it in main; S2 holds the input).
ReadRecovers == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ exec = "Unread" /\ exec' = "LegacyDead"
                /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                               idle, slot, judged, unsent, receipt>>
ReadRefused == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ exec = "Unread" /\ inq = "Queued" /\ ~S2Like
               /\ inq' = "Refused"
               /\ UNCHANGED <<exec, made, tabs, tmuxs, submits, oready, late, epoch, restarts2,
                              idle, slot, judged, unsent, receipt>>
\* At most two panes per thread keep the model finite (only MutS2TabNoMarker reaches two).
\* session_host/herdr/launch_host.rs:594-655 create in this process: answered (location recorded,
\* Placed) or answer lost (Pending).
\* w3 (P1-1): an answered create whose command input failed is CreatedUnconfirmed
\* (launch_host.rs:645-655); the placed pane may hold a bare shell. Only the LostCommand entries
\* lose the command, once (a single socket failure); the next create runs it.
LostEntries == {"LostCommand", "LostCommandHeld", "LostCommandShellUsed"}
Create == /\ UNCHANGED vars /\ UNCHANGED w2vars /\ exec = "Pending" /\ epoch = restarts2 /\ ~made /\ tabs < 2
          /\ made' = TRUE /\ tabs' = tabs + 1
          /\ \E placed \in BOOLEAN:
               /\ exec' = IF placed THEN "Placed" ELSE "Pending"
               /\ ran' \in IF placed /\ entry \in LostEntries /\ ~unsent THEN BOOLEAN ELSE {TRUE}
          /\ UNCHANGED <<closed, succ, archived, hitLive>> /\ UNCHANGED w4vars
          /\ UNCHANGED <<tmuxs, inq, submits, oready, late, epoch, restarts2, idle, slot, judged,
                         unsent, receipt>>
\* Unplaced resolution (herdr_launch/unplaced.rs:229-311; after a restart the boot pass): a found
\* pane is adopted with its location, not bound (Resolution::Adopted); none retires the row with
\* no create sent. MutS2TabNoMarker cannot find a tab. MutS2SeedOnAdopt seeds the adoption.
Resolve == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ exec = "Pending"
           /\ IF epoch = restarts2 THEN made ELSE BootResolves
           /\ exec' = IF made /\ Mode # "MutS2TabNoMarker" THEN "Placed" ELSE "Retired"
           /\ unsent' = ~made
           /\ judged' = IF made /\ Mode = "MutS2SeedOnAdopt" THEN "Herdr" ELSE judged
           /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle,
                          slot, receipt>>
\* A cold Claude start binds only after its first prompt made the transcript: the first record is
\* read, then bind_pg (claude/herdr_turn/first_input.rs:534-601, herdr_source.rs:177-190). The
\* current execution is recorded here. After a restart only a boot that resolves the parent's
\* endpoint reaches it.
Bind == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ exec = "Placed" /\ hkey
        /\ ~gone
        /\ (epoch = restarts2 \/ BootResolves)
        /\ exec' = "Bound" /\ judged' = "Herdr"
        /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle,
                       slot, unsent, receipt>>
\* The replayed input meets the unresolved Pending row: turn_host.rs:239-241
\* HostedHerdrUnconfigured. S2 holds it until the row resolves.
Rejudge == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ exec \in {"Pending", "Placed"} /\ epoch # restarts2
           /\ inq = "Queued" /\ ~BootResolves
           /\ inq' = "Refused"
           /\ UNCHANGED <<exec, made, tabs, tmuxs, submits, oready, late, epoch, restarts2, idle,
                          slot, judged, unsent, receipt>>
\* w2 order (P1-1): verified placed Pending, then the durable output reservation, then the first key.
\* The reservation sits in the common first_input write that submit and submit_owned share
\* (P1-2: an exact retry has no owner hook, herdr_turn.rs:764-773, ingress.rs:415-417).
ReserveRequired == S2Like /\ ~(Mode = "MutW1RetrySkipsReserve" /\ entry = "RetryNew")
Reserve == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ ReserveRequired /\ exec = "Placed" /\ inq = "Queued" /\ ~reserved
           /\ reserved' = TRUE
           /\ UNCHANGED <<s2vars, hkey, boot, other, v2, rst>>
\* claude/herdr_turn/first_input.rs:269-333: the first input once. MutW1BindFirst is design w1's
\* order (Bound before the first key). A replayed input (MutS2ReplayResubmit) keys a Bound pane.
SubmitHerdr == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars
               /\ IF Mode = "MutW1BindFirst" THEN exec = "Bound" ELSE exec \in {"Placed", "Bound"}
               \* attach needs the pane's own logged start (claude/herdr_turn/pending.rs:40).
               /\ inq = "Queued" /\ (ReserveRequired => reserved) /\ ran /\ ~gone
               /\ inq' = "Submitted" /\ submits' = submits + 1 /\ receipt' = "Sent" /\ hkey' = TRUE
               /\ UNCHANGED <<exec, made, tabs, tmuxs, oready, late, epoch, restarts2, idle, slot,
                              judged, unsent>>
               /\ UNCHANGED <<reserved, boot, other, v2, rst>>
\* P1-3: implementation, review and rework dispatches reset the provider first
\* (dispatch/dispatch_context.rs:205-212). A refused reset hands the input back and ends the turn
\* (intake_turn.rs:867-888) before for_turn; design w1 refuses a Herdr child there
\* (admin_host_guard.rs:96-142). w2 reads a confirmed vacant child's reset as fresh intent.
DispatchReset == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ entry = "DispatchNew" /\ rst /\ inq = "Queued"
                 /\ exec \in FreshStart
                 /\ IF LaunchHost = "Herdr" /\ Mode \in {"S2Naive", "MutW1ResetRefuses"}
                    THEN inq' = "Refused" /\ rst' = rst
                    ELSE inq' = inq /\ rst' = FALSE
                 /\ UNCHANGED <<exec, made, tabs, tmuxs, submits, oready, late, epoch, restarts2,
                                idle, slot, judged, unsent, receipt>>
                 /\ UNCHANGED <<reserved, hkey, boot, other, v2>>
\* P1-5: after a restart with Herdr restarted restore-off, a Bound row names a gone pane. Main has no
\* seed gate: consumers start (runtime_bootstrap.rs:302-311). Design w1 waits for every Bound to
\* verify before them; w2 retires rows the existing end judgement confirms dead, holds only an
\* unknown owner, then starts consumers.
BootNoGate == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ boot = "Gate" /\ ~S2Like
              /\ boot' = "Started"
              /\ UNCHANGED <<s2vars, reserved, hkey, other, v2, rst>>
BootRetireDead == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ boot = "Gate" /\ S2Like /\ Mode # "MutW1BootGateBlocks"
                  /\ exec = "Bound" /\ exec' = "Retired" /\ boot' = "Started" /\ judged' = "None"
                  /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                                 idle, slot, unsent, receipt>>
                  /\ UNCHANGED <<reserved, hkey, other, v2, rst>>
\* The relay's rehydrate pass retires a Bound row whose pane is gone
\* (tui_prompt_relay/rehydration.rs:270-274 -> recovery_engine/herdr_reader.rs:436-470).
RelayRetire == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ entry = "StaleBound" /\ boot = "Started" /\ exec = "Bound"
               /\ exec' = "Retired" /\ judged' = "None"
               /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                              idle, slot, unsent, receipt>>
               /\ UNCHANGED w2vars
\* Another channel's queued work runs once consumers started (REST worker, gateway intake).
OtherServe == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ boot = "Started" /\ other = "Waiting" /\ other' = "Served"
              /\ UNCHANGED <<s2vars, reserved, hkey, boot, v2, rst>>
\* P1-7: /restart on a vacant child (restart/herdr.rs:279-310) seeds through the V2 owner; with none
\* installed Seed::of fails (input_owner/restart_seed.rs:56-58) and nothing starts. w2 adopts the
\* local owner first. Main restarts the tmux path and starts its seed turn (restart.rs:143).
CmdRestartVacant == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ entry = "RestartVacant" /\ inq = "None"
                    /\ Mode # "MutW1RestartNoSeed"
                    /\ inq' = "Queued" /\ v2' = TRUE
                    /\ UNCHANGED <<exec, made, tabs, tmuxs, submits, oready, late, epoch,
                                   restarts2, idle, slot, judged, unsent, receipt>>
                    /\ UNCHANGED <<reserved, hkey, boot, other, rst>>
SubmitTmux == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ exec = "Tmux" /\ inq = "Queued" /\ ~LiveTmuxSeenDead
              /\ inq' = "Submitted" /\ submits' = submits + 1 /\ receipt' = "Sent"
              /\ UNCHANGED <<exec, made, tabs, tmuxs, oready, late, epoch, restarts2, idle, slot,
                             judged, unsent>>
\* Success removes the original with its terminal identity (queued_head/v2/retained.rs:122-146).
Settle == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ receipt = "Sent" /\ receipt' = "Settled"
          /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                         idle, slot, judged, unsent>>
\* Process restart: rows and panes stay; a sent, unsettled input is kept Unconfirmed
\* (retained.rs:77). MutS2ReplayResubmit replays a submitted input.
Restart2 == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ entry # "StaleBound" /\ restarts2 = 0 /\ restarts2' = 1
            /\ inq' = IF Mode = "MutS2ReplayResubmit" /\ inq = "Submitted" /\ exec = "Bound"
                      THEN "Queued" ELSE inq
            /\ receipt' = IF receipt = "Sent" THEN "Unconfirmed" ELSE receipt
            \* JUDGED is process memory (thread_host.rs:87); S2 seeds a Bound row before any reader
            \* starts (design C9, runtime_bootstrap.rs:302-311).
            /\ judged' = IF S2Like /\ exec = "Bound" THEN "Herdr" ELSE "None"
            /\ UNCHANGED <<exec, made, tabs, tmuxs, submits, oready, late, epoch, idle, slot,
                           unsent>>
\* The turn ended and the thread stayed idle past thread_gc's hour (db/.../thread_gc.rs:69).
Idle == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ exec \in {"Tmux", "Bound"} /\ inq = "Submitted" /\ ~idle
        /\ idle' = TRUE
        /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                       slot, judged, unsent, receipt>>
\* session_idle_cleanup.rs:87 skips a channel whose mailbox holds a turn: no release under a queued
\* input. A released execution is no longer recorded (thread_host.rs:90-97 forgets on None).
ReleaseIdle == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ idle /\ inq # "Queued"
               /\ \/ exec = "Tmux" /\ exec' = "LegacyDead" /\ tabs' = tabs
                  \/ exec = "Bound" /\ HerdrIdleReleased /\ exec' = "Retired" /\ tabs' = tabs - 1
               /\ judged' = "None"
               /\ UNCHANGED <<made, tmuxs, inq, submits, oready, late, epoch, restarts2, idle,
                              slot, unsent, receipt>>
\* thread_gc deletes only a legacy or retired row with no tmux session
\* (hosted_cleanup/thread_gc.rs:78-89, migrations 0131 agentdesk_hosted_execution_deletable).
Gc == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ idle /\ exec \in {"LegacyDead", "Retired"} /\ exec' = "Gone"
      /\ UNCHANGED <<made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2, idle, slot,
                     judged, unsent, receipt>>
\* auto_queue/runtime.rs:498-507 host_decision_for_rows: Bound -> Herdr, Pending -> Hold: skipped.
SlotReset == /\ UNCHANGED vars /\ UNCHANGED w3vars /\ UNCHANGED w4vars /\ UNCHANGED w2vars /\ idle /\ slot = "Bound"
             /\ \/ exec \in {"LegacyDead", "Gone", "Tmux"}
                \/ exec = "Retired" /\ RetiredSlotReleasable
             /\ ~(exec \in Released /\ SlotAsksNextHost)
             /\ slot' = "Reset"
             /\ UNCHANGED <<exec, made, tabs, tmuxs, inq, submits, oready, late, epoch, restarts2,
                            idle, judged, unsent, receipt>>

\* w3 (P1-1): a placed Pending whose command never ran is judged by the shared unplaced residue rule
\* (herdr_launch/unplaced.rs:173-203: the launch's exact marker, its lone root shell, a pristine
\* workspace, launch_host.rs:827-841). An observably changed residue (LostCommandHeld) is held; a
\* used shell that is a lone root shell again (LostCommandShellUsed) is not told apart and is
\* disposed. MutW3NoPlacedResidue has no such judgement (pending.rs:29-35 keeps PendingUnproven);
\* MutW3NoPristine skips the pristine check; MutW3ResidueByAbsence judges from a missing transcript.
ResidueJudged == CASE Mode = "MutW3ResidueByAbsence" -> TRUE
                   [] Mode = "MutW3NoPristine" -> ~ran
                   [] OTHER -> ~ran /\ entry # "LostCommandHeld"
\* w4: the close (host.dispose_residue, unplaced.rs:365-375) and the retire are two steps. A restart
\* between them, or a close applied whose answer was lost, leaves this state: the pane gone, the
\* row still the placed Pending.
ResidueClose == /\ UNCHANGED vars /\ S2Like /\ Mode # "MutW3NoPlacedResidue"
                /\ exec = "Placed" /\ ~gone /\ ~hkey /\ (epoch = restarts2 \/ BootResolves)
                /\ ResidueJudged
                /\ gone' = TRUE /\ goneGen' = gen /\ tabs' = tabs - 1
                /\ closed' = CASE ran -> "Ran"
                               [] entry = "LostCommandHeld" -> "Touched"
                               [] OTHER -> "Pristine"
                /\ UNCHANGED <<exec, made, tmuxs, inq, submits, oready, late, epoch, restarts2, idle,
                               slot, judged, unsent, receipt>>
                /\ UNCHANGED w2vars /\ UNCHANGED <<ran, succ, archived, hitLive, gen, lostSucc>>
\* The recorded pane goes away by itself, once: before any key (LostCommand) or after the first key
\* (LostAfterKey, the payload may have been delivered).
PaneVanishes == /\ UNCHANGED vars /\ UNCHANGED w2vars /\ UNCHANGED w3vars
                /\ entry \in {"LostCommand", "LostAfterKey"} /\ exec = "Placed" /\ ~gone /\ gen = 1
                /\ (entry = "LostAfterKey" => hkey)
                /\ gone' = TRUE /\ goneGen' = gen /\ tabs' = tabs - 1
                /\ UNCHANGED <<exec, made, tmuxs, inq, submits, oready, late, epoch, restarts2, idle,
                               slot, judged, unsent, receipt, gen, lostSucc>>
\* w4 (P1-1 rest): the placed resolver owns the exact Missing retire. reconnect_unless_pending
\* returns for a Pending (recovery_engine/herdr_reader.rs:425-427) before its Missing retire
\* (:449-458), so design w3 never retires this row (MutW4PendingEarlyReturn). The read is the
\* existing absence rule (session_host/herdr_gate.rs:856-876: snapshot presence Missing on the
\* launch's witness), the write the exact CAS retire_own_pending(.., Some(location))
\* (herdr_launch.rs:695-715: same Pending, nonce, location, no evidence). Missing is no resend
\* permit: only an untried original is KnownNotSent.
MissingRetireOk == CASE Mode = "MutW4MissingNoCas" -> goneGen > 0 /\ exec \in HerdrExec
                     [] Mode = "MutW4UnreadAsMissing" -> exec = "Placed" /\ ~hkey
                     [] OTHER -> exec = "Placed" /\ gone /\ goneGen = gen
PlacedMissingRetire ==
    /\ UNCHANGED vars /\ S2Like /\ Mode \notin {"MutW3NoPlacedResidue", "MutW4PendingEarlyReturn"}
    /\ (epoch = restarts2 \/ BootResolves) /\ MissingRetireOk
    /\ exec' = "Retired" /\ unsent' = ~hkey /\ judged' = "None"
    /\ inq' = IF Mode = "MutW4MissingResubmits" /\ inq = "Submitted" THEN "Queued" ELSE inq
    /\ ran' = TRUE /\ reserved' = FALSE /\ hkey' = FALSE
    /\ lostSucc' = (lostSucc \/ (goneGen > 0 /\ goneGen # gen))
    /\ UNCHANGED <<made, tabs, tmuxs, submits, oready, late, epoch, restarts2, idle, slot, receipt>>
    /\ UNCHANGED <<boot, other, v2, rst, closed, succ, archived, hitLive, gone, gen, goneGen>>
\* w3 (P1-2): after the slot map moved on, the same thread ID still takes work: a dispatch that
\* reuses the card's active_thread_id (intake_turn/adk_thread.rs:154-205, unarchived by
\* router/thread_binding.rs:77-110) or a direct Discord message starts a successor there.
Successor == /\ UNCHANGED vars /\ UNCHANGED w2vars /\ entry = "SlotSuccessor" /\ slot = "Reset"
             /\ exec \in Released /\ ~succ
             /\ succ' = TRUE /\ exec' = "Bound" /\ tabs' = tabs + 1 /\ idle' = FALSE
             /\ UNCHANGED <<made, tmuxs, inq, submits, oready, late, epoch, restarts2, slot, judged,
                            unsent, receipt>>
             /\ UNCHANGED <<ran, closed, archived, hitLive>> /\ UNCHANGED w4vars
\* The slot reset's Discord archive (auto_queue/runtime.rs:390,417-445) acts on the thread ID, not on
\* an execution. Design w2 archives once the release judgement held (MutW3ArchiveAfterRelease); w3
\* never archives a thread released from Herdr (the archive is the optional Discord side, :423-427).
ArchiveThread == /\ UNCHANGED vars /\ UNCHANGED w2vars /\ entry = "SlotSuccessor"
                 /\ Mode = "MutW3ArchiveAfterRelease" /\ slot = "Reset" /\ ~archived
                 /\ archived' = TRUE /\ hitLive' = (exec \in HerdrExec)
                 /\ UNCHANGED s2vars /\ UNCHANGED <<ran, closed, succ>> /\ UNCHANGED w4vars

NextS2 == Launch \/ OrdinaryReady \/ GateRefused \/ ReadRecovers \/ ReadRefused \/ Create
          \/ Resolve \/ Bind \/ Rejudge \/ Reserve \/ SubmitHerdr \/ SubmitTmux \/ Settle
          \/ Restart2 \/ Idle \/ ReleaseIdle \/ Gc \/ SlotReset \/ DispatchReset \/ BootNoGate
          \/ BootRetireDead \/ RelayRetire \/ OtherServe \/ CmdRestartVacant
          \/ ResidueClose \/ PaneVanishes \/ PlacedMissingRetire \/ Successor \/ ArchiveThread
SpecS2 == Init /\ [][NextS2]_allvars
          /\ WF_allvars(Launch) /\ WF_allvars(OrdinaryReady) /\ WF_allvars(ReadRecovers)
          /\ WF_allvars(Create) /\ WF_allvars(Resolve) /\ WF_allvars(Bind)
          /\ WF_allvars(SubmitHerdr) /\ WF_allvars(SubmitTmux) /\ WF_allvars(Settle)
          /\ WF_allvars(Idle) /\ WF_allvars(ReleaseIdle) /\ WF_allvars(Gc) /\ WF_allvars(SlotReset)
          /\ WF_allvars(Reserve) /\ WF_allvars(DispatchReset) /\ WF_allvars(BootNoGate)
          /\ WF_allvars(BootRetireDead) /\ WF_allvars(RelayRetire) /\ WF_allvars(OtherServe)
          /\ WF_allvars(CmdRestartVacant) /\ WF_allvars(ResidueClose)
          /\ WF_allvars(PlacedMissingRetire)

\* A new execution under a Herdr parent is Herdr: no tmux session, after restart, thread_gc and
\* slot reset included (every relaunch goes through Launch).
NewThreadFollowsParent == entry \in {"NewThread", "FreshAfterKill", "RetryNew", "DispatchNew",
                                     "RestartVacant", "LostCommand", "LostCommandHeld",
                                     "SlotSuccessor", "LostAfterKey", "LostCommandShellUsed"}
                          => tmuxs = 0
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
\* w2: no first key before its output reservation (P1-1, P1-2); a stale Bound child never stops
\* other channels' work at boot (P1-5).
KeyAfterReservation == hkey => reserved
BootServesOthers == <>(other = "Served")
\* w3: a residue close never ends a pane whose command ran, nor a touched shell (P1-1); a slot
\* release's archive never lands on a live successor in the same thread (P1-2).
ResidueNeverRan == closed # "Ran"
TouchedResidueHeld == closed # "Touched"
ArchiveSparesLive == ~hitLive
\* w4: a Missing retire ends only the generation whose pane it saw gone.
RetireOwnGeneration == ~lostSucc
=============================================================================
