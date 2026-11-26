---- MODULE CodexHerdrBusySource ----
\* #8426: the transcript a busy Codex owner front reads after a clear, on Herdr and on tmux.
\* Refines HerdrBusyActiveSource (#8114, Claude only) for Codex. Code map at the D1 snapshot:
\*   rowPath  = inflight row output_path. Herdr Codex intake seeds none: no tmux pane lives under
\*              the logical key (provider_isolation.rs seed_runtime); the producer stamps it only
\*              after the turn ends (codex/herdr_turn.rs read_reply -> RuntimeReady).
\*   binding  = CodexTui runtime binding of the logical key. A clear does not remove it (Herdr
\*              retire only withholds; tmux reset only cleans temp files), and the ordinary Legacy
\*              source policy lets consumer_allowed read it unchecked (codex_verified.rs).
\*   bound    = Herdr row Bound. Its attach admits only while the live binding names the new
\*              Source (tui_prompt_relay/herdr_source.rs), so Bound implies binding = Current.
\*   Select   = owner_input/busy_source.rs live_source; Registration::read_as is Absent on a
\*              Herdr row that is not Bound (claude_handoff.rs bound_herdr).
\*   Submit   = read_turn(r.transcript) gate in owner_input.rs (CodexBusy|CodexHerdrBusy).
EXTENDS Naturals
CONSTANTS Policy, Host, Scenario, Mutant
VARIABLE s
vars == <<s>>
Absent == 0
Old == 1
Current == 2

Init == s = [life |-> "running", rowPath |-> Absent, binding |-> Old, bound |-> Host = "herdr",
             knownBusy |-> TRUE, input |-> "none", queuedProven |-> FALSE,
             request |-> Absent, effects |-> 0, used |-> Absent, reason |-> "none"]

\* /clear or !clear, then the next turn's launch: a new pane lives, its row seeds no path, the
\* binding the old session left stays, and a Herdr row is Pending until its attach.
Clear == /\ s.life = "running"
         /\ s' = [s EXCEPT !.life = "window", !.rowPath = Absent, !.bound = FALSE,
                  !.knownBusy = Scenario # "unknown"]
\* The new session's hook publishes its source; a Herdr attach then binds the row. A rejected
\* hook (unverified) leaves the old binding and no attach.
Prove == /\ s.life = "window"
         /\ s' = [s EXCEPT !.life = "proven",
                  !.binding = IF Scenario = "unverified" THEN s.binding ELSE Current,
                  !.bound = Host = "herdr" /\ Scenario # "unverified"]
\* A person's input may arrive inside the clear window or after the proof.
PersonInput == /\ s.life \in {"window", "proven"} /\ s.input = "none"
               /\ s' = [s EXCEPT !.input = "queued", !.queuedProven = (s.life = "proven")]
HerdrGate == Host = "herdr" /\ ~s.bound /\ Mutant # "NoBoundGate"
Request == IF Policy = "Now" THEN s.rowPath
           ELSE IF Policy = "FixAnyHost" /\ s.binding # Absent THEN s.binding
           ELSE IF Policy = "FixHerdr" /\ Host = "herdr" /\ s.binding # Absent
                   /\ (s.bound \/ Mutant = "NoBoundGate") THEN s.binding
           ELSE s.rowPath
Select == /\ s.input = "queued"
          /\ IF ~s.knownBusy /\ Mutant # "UnknownBusy"
             THEN s' = [s EXCEPT !.input = "refused", !.reason = "turn_state_unknown"]
             ELSE IF HerdrGate
             THEN s' = [s EXCEPT !.input = "refused", !.reason = "busy_turn_unobserved"]
             ELSE s' = [s EXCEPT !.input = "captured", !.request = Request]
\* read_turn opens the path; an absent one is NotSent("busy_turn: transcript unavailable").
Submit == /\ s.input = "captured"
          /\ IF s.request = Absent
             THEN s' = [s EXCEPT !.input = "refused", !.reason = "transcript_unavailable"]
             ELSE s' = [s EXCEPT !.input = "sent", !.effects = @ + 1, !.used = s.request]
Wait == UNCHANGED vars
Next == Clear \/ Prove \/ PersonInput \/ Select \/ Submit \/ Wait
Spec == Init /\ [][Next]_vars /\ WF_vars(Clear) /\ WF_vars(Prove) /\ WF_vars(PersonInput)
        /\ WF_vars(Select) /\ WF_vars(Submit)

TypeOK == s.life \in {"running", "window", "proven"}
          /\ s.input \in {"none", "queued", "captured", "refused", "sent"}
          /\ s.effects \in 0..1
AtMostOnce == s.effects <= 1
\* A key goes only with the current session's rollout (history, append and receipt attribution).
NoForeignSourceKey == s.effects > 0 => s.used = Current
NoKeyWithoutKnownBusy == s.effects > 0 => s.knownBusy
\* Every refusal carries its reason; nothing parks without one.
RefusalExplained == s.input = "refused" => s.reason # "none"
\* The input ends delivered or refused with a reason, never held without an end.
ScenarioReached == <> (s.input \in {"refused", "sent"})
\* Rule: a Herdr input queued after the proof, with a running turn, enters that turn.
HerdrProvenInjected == [] ((s.input = "queued" /\ s.queuedProven /\ s.binding = Current
                            /\ s.knownBusy) => <> (s.input = "sent"))
\* Without a proved source or a known running turn it is refused with a reason.
UnverifiedRefused == <> (s.input = "refused" /\ s.reason # "none")
=============================================================================
