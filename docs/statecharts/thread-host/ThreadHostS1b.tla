---------------------------- MODULE ThreadHostS1b ----------------------------
(* #8118 S1b: commands, cleanup and input consent act on the host decided from a thread's
   execution now (host_decision.rs), and a thread's recorded execution follows the evidence
   a turn verified, not the turn's permission (turn_host.rs recorded_execution).
   Written against 2668bdeefd (S1a head) plus the S1b change.

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
     Unresolved  thread whose parent the boot view holds unresolved                       *)
EXTENDS Naturals

CONSTANTS Mode, Entries

VARIABLES entry, proof, cache, restarted, sinceRestart, restored,
          restartHost, tmuxOp, relaunch, consent, sync

vars == <<entry, proof, cache, restarted, sinceRestart, restored,
          restartHost, tmuxOp, relaunch, consent, sync>>

Threads == {"LiveTmux", "DeadTmux", "ProbeFail", "HerdrBound", "Unresolved"}
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
TmuxOutcome ==
    CASE Mode = "Before" -> IF BootSome THEN "Kept" ELSE "Done"
      [] Mode = "Rev4"   -> IF Rev4Some THEN "Kept" ELSE "Done"
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

Init == /\ entry \in Entries
        /\ proof \in {"Ok", "Pending"}
        \* A running process recorded the pane it launched; nothing else is judged yet.
        /\ cache = IF entry = "HerdrBound" THEN "Herdr" ELSE "None"
        /\ restarted = FALSE /\ sinceRestart = FALSE /\ restored = TRUE
        /\ restartHost = "None" /\ tmuxOp = "None" /\ relaunch = "None"
        /\ consent = "None" /\ sync = "None"


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

Spec == Init /\ [][Next]_vars /\ WF_vars(CmdRestart) /\ WF_vars(CmdTmuxOp)
        /\ WF_vars(CmdRelaunch) /\ WF_vars(Input)

TmuxThread == entry \in {"LiveTmux", "DeadTmux"}

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
=============================================================================
