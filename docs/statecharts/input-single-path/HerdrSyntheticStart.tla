---- MODULE HerdrSyntheticStart ----
\* #8422: a synthetic start (HttpStart, Routine, Decision, EffortContinue) is judged before claim by
\* start_waits (owner_input.rs:770-822) -> start_wait::observe (owner_input_start_wait.rs:33-97).
\* A Herdr Bound row whose execution is dead reads Held (busy/herdr.rs:304-306) -> Unknown ->
\* Wait::Watch + hold_front (owner_input.rs:974-978). held_watch keeps an Unknown forever and ends a
\* Gone without a wake (held_watch.rs:60-66). Code SHA 82b96fa94b.
\* Policy "Fixed" is the design-1 repair: observe asks the shared dead-Bound retire (the #8212 proof,
\* herdr_launch/retire.rs Scope::Ended) when no turn holds the channel; a retired row reads Ready and
\* the start cold-launches; a permanent refusal goes to the typed host refusal (Observed::Refused).
EXTENDS Naturals
CONSTANTS Policy, Host, Exec0, Turn0, Foreign, Mutant

VARIABLE s
vars == <<s>>

Init == s = [front |-> "queued", awake |-> TRUE, watch |-> FALSE,
             row |-> "Bound", exec |-> Exec0, turn |-> Turn0,
             autoRetired |-> FALSE, badRetire |-> FALSE, turnRetire |-> FALSE,
             cold |-> FALSE, refusing |-> FALSE]

\* The proof retire::observed accepts: pane missing or a lone recorded root shell (ended_across).
\* Mutant RetireOnHeld retires on any Held reading, as candidate (3) without the exact proof would.
Proven == s.exec = "dead" \/ (Mutant = "RetireOnHeld" /\ s.exec = "unreadable")
\* No external inflight row reached observe; a managed turn or a captured token refuses the retire.
Guarded == s.turn = "none" \/ Mutant = "RetireUnderTurn"
CanAutoRetire == /\ Policy = "Fixed" /\ Mutant # "GoneNoRetire" /\ Host = "herdr"
                 /\ s.row = "Bound" /\ ~Foreign /\ Guarded /\ Proven

\* One Herdr observe of the start's channel execution, after any retire it asked for.
HerdrRead == IF s.row # "Bound" THEN "Ready"
             ELSE IF Foreign THEN (IF Policy = "Fixed" THEN "Refused" ELSE "Unknown")
             ELSE CASE s.exec = "idle" -> "Ready"
                    [] s.exec = "busy" -> "Blocked"
                    [] s.exec = "unreadable" -> "Unknown"
                    [] s.exec = "dead" -> IF Mutant = "GoneNoRetire" THEN "Gone" ELSE "Unknown"
\* A tmux start's native read (owner_input.rs:866-871, 880-889).
TmuxRead == CASE s.exec = "idle" -> "Ready"
              [] s.exec = "dead" -> "Gone"
              [] s.exec = "busy" -> "Blocked"
              [] s.exec = "unreadable" -> "Unknown"

Retire(t) == [t EXCEPT !.row = "Retired", !.autoRetired = TRUE,
                       !.badRetire = t.badRetire \/ t.exec # "dead",
                       !.turnRetire = t.turnRetire \/ t.turn # "none"]

\* The drive grants the front and start_idle judges it before claim.
Drive == /\ s.front = "queued" /\ s.awake
         /\ IF Host = "tmux"
            THEN \* transcript_turns: a busy channel waits for an event (owner_input.rs:802-808).
                 IF s.exec = "busy" THEN s' = [s EXCEPT !.awake = FALSE]
                 ELSE IF TmuxRead \in {"Ready", "Gone"}
                      THEN s' = [s EXCEPT !.awake = FALSE, !.front = "claimed"]
                      ELSE s' = [s EXCEPT !.awake = FALSE, !.front = "held", !.watch = TRUE]
            ELSE IF s.turn = "external"
                 \* A non-managed inflight row waits for its row end (owner_input.rs:780-783).
                 THEN s' = [s EXCEPT !.awake = FALSE]
                 ELSE LET t == IF CanAutoRetire THEN Retire(s) ELSE s
                          read == IF t.row # "Bound" THEN "Ready" ELSE HerdrRead
                      IN s' = CASE read = "Ready" ->
                                     [t EXCEPT !.awake = FALSE, !.front = "claimed"]
                                [] read = "Refused" ->
                                     [t EXCEPT !.awake = FALSE, !.front = "claimed",
                                               !.refusing = TRUE]
                                [] OTHER ->
                                     [t EXCEPT !.awake = FALSE, !.front = "held", !.watch = TRUE]

\* The claimed start runs: a typed host refusal settles it, a retired row cold-launches.
Launch == /\ s.front = "claimed"
          /\ s' = IF s.refusing THEN [s EXCEPT !.front = "rejected"]
                  ELSE IF Host = "herdr" /\ s.row = "Retired"
                  THEN [s EXCEPT !.front = "delivered", !.row = "Bound", !.exec = "idle",
                                 !.cold = TRUE]
                  ELSE IF Host = "tmux" /\ s.exec = "dead"
                  THEN [s EXCEPT !.front = "delivered", !.exec = "idle", !.cold = TRUE]
                  ELSE [s EXCEPT !.front = "delivered"]

\* held_watch: Ready wakes once, Gone ends unwoken, anything else keeps (held_watch.rs:60-66).
\* The Fixed probe is observe itself, so each look may ask the retire again.
WatchLook == /\ s.watch /\ s.front = "held"
             /\ LET t == IF CanAutoRetire THEN Retire(s) ELSE s
                    read == IF Host = "tmux" THEN TmuxRead
                            ELSE IF t.row # "Bound" THEN "Ready"
                            ELSE IF HerdrRead = "Refused" THEN "Ready"
                            ELSE HerdrRead
                IN s' = CASE read = "Ready" ->
                               [t EXCEPT !.watch = FALSE, !.front = "queued", !.awake = TRUE]
                          [] read = "Gone" -> [t EXCEPT !.watch = FALSE]
                          [] OTHER -> t

\* Any owner wake re-drives a waiting or held front.
Woken(t) == IF t.front \in {"queued", "held"}
            THEN [t EXCEPT !.front = "queued", !.awake = TRUE, !.watch = FALSE] ELSE t

\* A native turn ends; turn_presence turns idle and wakes an owned channel (supervisor.rs:691-701).
NativeEnds == /\ s.exec = "busy"
              /\ LET t == [s EXCEPT !.exec = "idle"]
                 IN s' = IF Host = "tmux" /\ Mutant # "NoPresenceWake" THEN Woken(t) ELSE t
Readable == s.exec = "unreadable" /\ s' = [s EXCEPT !.exec = "idle"]
\* The execution dies on its own; nothing is woken by it.
\* Herdr only: a tmux pane's death after an Event wait is outside this model (report: residual).
Die == /\ Host = "herdr" /\ s.exec \in {"idle", "busy", "unreadable"}
       /\ s.front \in {"queued", "held"}
       /\ s' = [s EXCEPT !.exec = "dead"]
\* A holding turn ends at its own boundary (or its row end) and wakes the owner.
TurnEnd == s.turn # "none" /\ s' = Woken([s EXCEPT !.turn = "none"])
\* Operator or HTTP force terminate: S3b-1 retired_wake wakes the owner. Never fair.
OperatorRetire == /\ Host = "herdr" /\ s.row = "Bound" /\ s.exec = "dead"
                  /\ s' = Woken([s EXCEPT !.row = "Retired"])

\* A settled start stays settled.
Done == s.front \in {"delivered", "rejected"} /\ UNCHANGED vars

Next == Done \/ Drive \/ Launch \/ WatchLook \/ NativeEnds \/ Readable \/ Die \/ TurnEnd \/ OperatorRetire
Spec == Init /\ [][Next]_vars /\ WF_vars(Drive) /\ WF_vars(Launch) /\ WF_vars(WatchLook)
        /\ WF_vars(NativeEnds) /\ WF_vars(Readable) /\ WF_vars(TurnEnd)

\* Safety: an automatic retire never ends a live or unproven execution, nor one a turn holds.
NoAutoRetireLive == ~s.badRetire
NoAutoRetireUnderTurn == ~s.turnRetire
RetiredStartIsCold == (s.front = "delivered" /\ s.autoRetired) => s.cold
TypeOK == s.front \in {"queued", "held", "claimed", "delivered", "rejected"}

\* Liveness: a synthetic start is delivered or explicitly refused.
SyntheticSettles == <>(s.front \in {"delivered", "rejected"})
====
