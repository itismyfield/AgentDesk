-------------------------- MODULE CodexAdmittedPrior --------------------------
\* #7651 D1b (a): an earlier rollout A when a fresh admitted rollout B is taken.
\* The launch records B's tmux generation before discovery binds B, so A's own receipts are
\* in the previous generation (o_writer_legacy.rs codex_records reads only the current one).
\* Mode "main" is the landed head (admitted init [B@0], late_adopt/codex.rs commit &[]);
\* "r10" re-judged unresolved A against the new generation; "design" attributes each earlier
\* generation's receipts to its sole bound rollout, keeps the owed basis judged before the
\* admission, and takes B without A only when A is unprovable. Two mutants follow.
EXTENDS Naturals
CONSTANTS Mode
\* Prior: "delivered" (receipt in A's generation), "undelivered" (no Legacy send of A),
\* "ambiguous" (a Legacy send of A started, no receipt). Seen: what the actor judged before B.
VARIABLES Prior, Seen, phase, initA, alarm
vars == <<Prior, Seen, phase, initA, alarm>>

Modes == {"main", "r10", "design", "designOwedAll", "designBlock"}
Design == Mode \in {"design", "designOwedAll", "designBlock"}

\* Whether A is one of the sources the admitted path judges.
Walked == CASE Mode = "main" -> FALSE
          [] Mode = "r10" -> Seen # "none"
          [] OTHER -> TRUE
\* What the walked judgement proves about A.
Verdict == CASE Mode = "r10" /\ Seen = "final" /\ Prior = "delivered" -> "end"
          [] Mode = "r10" /\ Prior = "undelivered" -> "owed"
          [] Mode = "r10" -> "legacy"
          [] Mode = "designOwedAll" -> "owed"
          [] Prior = "delivered" -> "end"
          [] Prior = "undelivered" -> "owed"
          [] OTHER -> "legacy"
Blocks == Walked /\ Verdict = "legacy" /\ Mode \in {"r10", "designBlock"}

Init == /\ Prior \in {"delivered", "undelivered", "ambiguous"} /\ Seen \in {"none", "early", "final"}
        /\ phase = "bound" /\ initA = FALSE /\ alarm = FALSE

Commit == /\ phase = "bound" /\ ~Blocks
          /\ phase' = "committed"
          /\ initA' = (Walked /\ Verdict = "owed")
          /\ alarm' = (Design /\ Walked /\ Verdict = "legacy")
          /\ UNCHANGED <<Prior, Seen>>

Retry == /\ phase = "bound" /\ Blocks /\ UNCHANGED vars

Done == phase = "committed" /\ UNCHANGED vars

Next == Commit \/ Retry \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Commit)

\* A delivered or unprovable A never holds B back.
AdmittedEventuallyCommitted == <>(phase = "committed")
\* A proven undelivered answer stays owed in B's init.
ProvenOwedKept == phase = "committed" /\ Prior = "undelivered" => initA
\* A delivered answer is not owed again.
DeliveredNotReposted == phase = "committed" /\ Prior = "delivered" => ~initA
\* Taking B without an unprovable A is visible.
UnprovableAlarmed == Design /\ phase = "committed" /\ Prior = "ambiguous" => alarm
=============================================================================
