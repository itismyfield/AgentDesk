---- MODULE HerdrSyntheticStart ----
\* #8422: a synthetic start (HttpStart, Routine, Decision, EffortContinue) is judged before claim by
\* start_waits (owner_input.rs:770-822) -> start_wait::observe (owner_input_start_wait.rs:33-97).
\* A Herdr Bound row whose execution is dead reads Held (Claude: busy/herdr.rs:304-306) or
\* Activity::Unknown (Codex: owner_input_start_wait.rs:67-76, unbound.rs:328) -> Wait::Watch +
\* hold_front (owner_input.rs:974-978). held_watch keeps an Unknown forever and ends a Gone without
\* a wake (held_watch.rs:65-71). Code SHA 82b96fa94b.
\* Policy "Fixed" is design rev2: observe asks the shared dead-Bound recovery for every provider
\* (the CLI retire::observed(Scope::Ended) proof, owner compared with the asking provider); the
\* recovery holds the channel transition lease from its last no-turn read to the CAS (borrowed by a
\* person's input that already holds it); a retired row reads Ready and the start cold-launches; a
\* permanent refusal is settled Failed with its reason.
\* A person's input reaches the same recovery after claim (owned_busy.rs:99-339); "Before" for a
\* person is #8212 (Scope::Missing only).
EXTENDS Naturals
CONSTANTS Policy, Host, Provider, Origin, Exec0, Turn0, Inflight0, Foreign, Race, InflightRace,
          Mutant

VARIABLE s
vars == <<s>>

Init == s = [front |-> "queued", awake |-> TRUE, watch |-> FALSE,
             row |-> "Bound", exec |-> Exec0, turn |-> Turn0, inflight |-> Inflight0,
             lease |-> FALSE, turnStarted |-> FALSE, inflightStarted |-> FALSE,
             autoRetired |-> FALSE, badRetire |-> FALSE, turnRetire |-> FALSE,
             cold |-> FALSE, refusing |-> FALSE, outcome |-> "none"]

Dead == s.exec \in {"missing", "shell"}

\* Whether the recovery is reached from this start's judgement at all.
Reached == \/ /\ Policy = "Fixed"
               /\ ~(Provider = "codex" /\ Mutant = "codexEarlyReturn")
           \/ /\ Policy = "Before" /\ Origin = "person" /\ Provider = "claude"

\* The exact ended proof: Fixed uses Scope::Ended (missing pane or lone recorded root shell);
\* #8212 used Scope::Missing. Mutant RetireOnHeld retires on any unclear read.
Proven == \/ (Policy = "Fixed" /\ Dead)
          \/ (Policy = "Before" /\ s.exec = "missing")
          \/ (Mutant = "RetireOnHeld" /\ s.exec = "unreadable")

\* The owner check; mutant ownerClaudeConst keeps the "claude" constant.
OwnerDiffers == Foreign \/ (Mutant = "ownerClaudeConst" /\ Provider = "codex")

\* No turn holds the channel: no token, and the inflight read is exactly Ok(None).
\* Mutant okFlatten reads a failed inflight read as no row.
NoTurn == /\ (s.turn = "none" \/ Mutant = "RetireUnderTurn")
          /\ (s.inflight = "none" \/ (Mutant = "okFlatten" /\ s.inflight = "unreadable"))
\* The lease: try_acquire for a synthetic start, borrow for a person already in transition.
LeaseOk == ~(Origin = "person" /\ Mutant = "noBorrow")

\* One Herdr read of the channel execution (Claude busy reader or Codex activity).
HerdrRead == IF s.row # "Bound" THEN "Ready"
             ELSE CASE s.exec = "idle" -> "Ready"
                    [] s.exec = "busy" -> "Blocked"
                    [] s.exec = "unreadable" -> "Unknown"
                    [] Dead -> IF Mutant = "GoneNoRetire" THEN "Gone" ELSE "Unknown"
TmuxRead == CASE s.exec = "idle" -> "Ready"
              [] s.exec = "busy" -> "Blocked"
              [] s.exec = "unreadable" -> "Unknown"
              [] Dead -> "Gone"

\* The recovery's verdict on an Unknown Bound read.
Recovery == IF ~Reached \/ Mutant = "GoneNoRetire" THEN "none"
            ELSE IF OwnerDiffers THEN "refused"
            ELSE IF Proven /\ NoTurn /\ LeaseOk THEN "begin"
            ELSE "reobserve"

\* Judge: a granted (drive) or watched (watch) front reads its execution.
Verdict == IF Host = "tmux" THEN TmuxRead
           ELSE IF HerdrRead = "Unknown" /\ Recovery = "begin" THEN "begin"
           ELSE IF HerdrRead = "Unknown" /\ Recovery = "refused" THEN "refused"
           ELSE HerdrRead

Begin(t) == [t EXCEPT !.front = "retiring", !.watch = FALSE, !.awake = FALSE,
                      !.lease = (Mutant # "noLease")]
\* Mutant inflightNoLease keeps the lease for token registrations only.

Drive == /\ s.front = "queued" /\ s.awake
         /\ IF Host = "tmux" /\ s.exec = "busy"
            \* transcript_turns: a busy channel waits for an event (owner_input.rs:802-808).
            THEN s' = [s EXCEPT !.awake = FALSE]
            ELSE IF Host = "herdr" /\ Origin = "synthetic"
                    /\ (s.turn = "external" \/ s.inflight = "row")
            \* A non-managed inflight row waits for its row end (owner_input.rs:780-783).
            THEN s' = [s EXCEPT !.awake = FALSE]
            ELSE s' = CASE Verdict \in {"Ready", "Gone"} /\ (Host = "tmux" \/ Verdict = "Ready") ->
                             [s EXCEPT !.awake = FALSE, !.front = "claimed"]
                        [] Verdict = "begin" -> Begin(s)
                        [] Verdict = "refused" ->
                             [s EXCEPT !.awake = FALSE, !.front = "claimed", !.refusing = TRUE]
                        [] OTHER -> [s EXCEPT !.awake = FALSE, !.front = "held", !.watch = TRUE]

\* held_watch probe = the same judgement; Ready (or a refusal) wakes once, Gone ends unwoken.
\* A begun recovery runs under its own lease whatever the front's later lead check says.
WatchLook == /\ s.watch /\ s.front = "held"
             /\ s' = CASE Verdict \in {"Ready", "refused"} ->
                            [s EXCEPT !.watch = FALSE, !.front = "queued", !.awake = TRUE]
                       [] Verdict = "begin" -> Begin(s)
                       [] Verdict = "Gone" -> [s EXCEPT !.watch = FALSE]
                       [] OTHER -> s

\* The CAS ends the recovery: the row retires, the lease drops, the start goes on to claim.
Cas == /\ s.front = "retiring"
       /\ s' = [s EXCEPT !.row = "Retired", !.autoRetired = TRUE, !.lease = FALSE,
                         !.front = "claimed",
                         !.badRetire = s.badRetire \/ ~Dead,
                         !.turnRetire = s.turnRetire \/ s.turn # "none" \/ s.inflight # "none"]

\* The claimed start runs: a typed refusal settles Failed, a retired row cold-launches.
Launch == /\ s.front = "claimed"
          /\ s' = IF s.refusing
                  THEN [s EXCEPT !.front = "rejected",
                                 !.outcome = IF Mutant = "rejectAsCompleted"
                                             THEN "completed" ELSE "failed"]
                  ELSE IF Host = "herdr" /\ s.row = "Retired"
                  THEN [s EXCEPT !.front = "delivered", !.row = "Bound", !.exec = "idle",
                                 !.cold = TRUE, !.outcome = "completed"]
                  ELSE IF Host = "tmux" /\ Dead
                  THEN [s EXCEPT !.front = "delivered", !.exec = "idle", !.cold = TRUE,
                                 !.outcome = "completed"]
                  ELSE [s EXCEPT !.front = "delivered", !.outcome = "completed"]

Woken(t) == IF t.front \in {"queued", "held"}
            THEN [t EXCEPT !.front = "queued", !.awake = TRUE, !.watch = FALSE] ELSE t

\* A native turn ends; turn_presence turns idle and wakes an owned channel (supervisor.rs:691-701).
NativeEnds == /\ s.exec = "busy"
              /\ LET t == [s EXCEPT !.exec = "idle"]
                 IN s' = IF Host = "tmux" /\ Mutant # "NoPresenceWake" THEN Woken(t) ELSE t
Readable == s.exec = "unreadable" /\ s' = [s EXCEPT !.exec = "idle"]
InflightRead == s.inflight = "unreadable" /\ s' = [s EXCEPT !.inflight = "none"]
\* The execution dies on its own, leaving a missing pane or a lone root shell; nothing wakes.
Die == /\ Host = "herdr" /\ s.exec \in {"idle", "busy", "unreadable"}
       /\ s.front \in {"queued", "held"}
       /\ \E e \in {"missing", "shell"} : s' = [s EXCEPT !.exec = e]
\* Another start registers its token; it takes the transition lock first (turn_start.rs:151).
TurnStart == /\ Race /\ Host = "herdr" /\ ~s.turnStarted /\ s.turn = "none" /\ ~s.lease
             /\ s.front \in {"queued", "held", "retiring"}
             /\ s' = [s EXCEPT !.turn = "managed", !.turnStarted = TRUE]
TurnEnd == s.turn # "none" /\ s' = Woken([s EXCEPT !.turn = "none"])
\* A watcher creates an inflight row alone (liveness.rs:473-529 reacquire, monitor auto turn) from
\* a stream it saw before the execution ended; implementation: it takes the transition lease.
InflightStart == /\ InflightRace /\ Host = "herdr" /\ ~s.inflightStarted /\ s.inflight = "none"
                 /\ (~s.lease \/ Mutant = "inflightNoLease")
                 /\ s.front \in {"queued", "held", "retiring"}
                 /\ s' = [s EXCEPT !.inflight = "row", !.inflightStarted = TRUE]
\* Its row ends (herdr_row_end) and wakes the owner.
InflightEnd == s.inflight = "row" /\ s' = Woken([s EXCEPT !.inflight = "none"])
\* Operator or HTTP force terminate: S3b-1 retired_wake wakes the owner. Never fair.
OperatorRetire == /\ Host = "herdr" /\ s.row = "Bound" /\ Dead /\ s.front # "retiring"
                  /\ s' = Woken([s EXCEPT !.row = "Retired"])
Done == s.front \in {"delivered", "rejected"} /\ UNCHANGED vars

Next == Done \/ Drive \/ WatchLook \/ Cas \/ Launch \/ NativeEnds \/ Readable \/ InflightRead
        \/ Die \/ TurnStart \/ TurnEnd \/ InflightStart \/ InflightEnd \/ OperatorRetire
Spec == Init /\ [][Next]_vars /\ WF_vars(Drive) /\ WF_vars(WatchLook) /\ WF_vars(Cas)
        /\ WF_vars(Launch) /\ WF_vars(NativeEnds) /\ WF_vars(Readable)
        /\ WF_vars(InflightRead) /\ WF_vars(TurnEnd) /\ WF_vars(InflightEnd)

\* Safety.
NoAutoRetireLive == ~s.badRetire
\* A channel holding a turn token (or an unproven inflight read) never has its Bound retired.
NoAutoRetireUnderTurn == ~s.turnRetire
RetiredStartIsCold == (s.front = "delivered" /\ s.autoRetired) => s.cold
RejectedIsFailed == s.front = "rejected" => s.outcome = "failed"
TypeOK == s.front \in {"queued", "held", "retiring", "claimed", "delivered", "rejected"}

\* Liveness: the start is delivered or explicitly refused; a start whose owner matches is delivered.
SyntheticSettles == <>(s.front \in {"delivered", "rejected"})
OwnedStartDelivered == <>(s.front = "delivered" \/ (Foreign /\ s.front = "rejected"))
====
