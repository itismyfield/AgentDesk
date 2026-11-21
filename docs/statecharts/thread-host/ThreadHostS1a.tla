---------------------------- MODULE ThreadHostS1a ----------------------------
(* #8118 S1a: a thread's host is judged as two facts, the parent's policy and the
   execution this process judged. Written against 94f32aeacf plus the S1a change.

   Mode "Before" is main, "Rev3" is design revision 3 taken literally (every
   synchronous reader, the absence read included, reads a process cache that is
   empty after a restart), "After" is the S1a code.

   Entries come only from the design's entry table (design-hh-3 5.1, delta 4):
     OwnBootAbsent   own boot Herdr channel, row Missing or Retired, input after restart
     HerdrThread     Bound Herdr thread under a Herdr parent (journal and busy input)
     TmuxThread      live tmux thread under a Herdr parent
     Unresolved      thread whose parent the boot view holds unresolved
     LeftChannel     channel no boot policy routes to Herdr                         *)
EXTENDS Naturals

CONSTANTS Mode, Entries

VARIABLES entry, row, turn, judged, journal, start, seen, at, restarts

vars == <<entry, row, turn, judged, journal, start, seen, at, restarts>>

Rows(e) == CASE e = "OwnBootAbsent" -> {"Missing", "Retired"}
             [] e = "HerdrThread"   -> {"Bound"}
             [] OTHER               -> {"Missing", "Legacy"}

\* thread_host.rs parent_policy: boot exact, else the resolved parent's boot entry.
Policy == CASE entry = "OwnBootAbsent" -> "HerdrOwn"
            [] entry \in {"HerdrThread", "TmuxThread"} -> "HerdrViaParent"
            [] entry = "Unresolved" -> "Unknown"
            [] OTHER -> "Tmux"

Thread == entry \in {"HerdrThread", "TmuxThread"}

Init == /\ entry \in Entries
        /\ row \in Rows(entry)
        /\ turn = "None" /\ judged = "None"
        /\ journal = IF entry = "OwnBootAbsent" THEN "None" ELSE "Held"
        /\ start = IF entry = "OwnBootAbsent" THEN "Waiting" ELSE "None"
        /\ seen = "None" /\ at = "None" /\ restarts = 0

\* turn_host.rs for_turn -> resolve (:196-215 is_live_thread_endpoint): a thread is Herdr
\* only on a Bound row at its parent's endpoint. After records the answer (record_judged).
Judge == /\ Thread
         /\ turn' = IF row = "Bound" THEN "Herdr" ELSE "Tmux"
         /\ judged' = IF Mode = "Before" THEN judged ELSE turn'
         /\ UNCHANGED <<entry, row, journal, start, seen, at, restarts>>

\* Process restart: the judged map is process memory (thread_host.rs JUDGED).
Restart == /\ Thread /\ restarts = 0 /\ turn # "None"
           /\ turn' = "None" /\ judged' = "None" /\ restarts' = 1
           /\ UNCHANGED <<entry, row, journal, start, seen, at>>

\* What a synchronous reader of the current execution answers.
\* Before: session_hosts::herdr_endpoint(channel), the channel's own boot entry only.
\* After:  thread_host.rs current_execution_cached.
Cached == IF judged = "None" THEN "Unknown" ELSE judged
Reads == CASE Mode = "Before" -> IF entry = "OwnBootAbsent" THEN "Herdr" ELSE "Tmux"
           [] Mode = "Rev3"   -> Cached
           [] OTHER -> CASE Policy = "HerdrOwn" -> "Herdr"
                         [] Policy = "Tmux"     -> "Tmux"
                         [] Policy = "Unknown"  -> "Unknown"
                         [] OTHER               -> Cached

\* Owner busy input and delivery readers: busy_inject.rs:105 (#45), inject.rs:312 (#15),
\* inject/herdr.rs:48 (#13), herdr_bound.rs:18 (#11).
BusyInput == /\ Thread /\ seen = "None"
             /\ seen' = Reads /\ at' = turn
             /\ UNCHANGED <<entry, row, turn, judged, journal, start, restarts>>

\* claude_handoff.rs:63-73 and recover :527: a journal on a channel that left Herdr is dropped.
\* Before: left = no own boot entry (:292). After: left = policy Tmux (thread_host.rs left_herdr).
Left == IF Mode = "Before" THEN entry # "OwnBootAbsent" ELSE Policy = "Tmux"
JournalCheck == /\ journal = "Held"
                /\ journal' = IF Left THEN "Dropped" ELSE "Kept"
                /\ UNCHANGED <<entry, row, turn, judged, start, seen, at, restarts>>

\* owner_input.rs:790-810 -> owner_input_start_wait.rs:40-65 -> unbound.rs:305-330.
\* Before/After read the boot entry: Missing -> HerdrFirstCreate, own Retired -> HerdrRetired,
\* presence Idle (live.rs:1042) and the start runs. Rev3 reads the empty cache: host_unresolved,
\* presence Unknown (live.rs:1044-1055), Wait::Watch observes again and nothing changes.
StartAbsent == /\ start = "Waiting" /\ Mode # "Rev3"
               /\ start' = "Started"
               /\ UNCHANGED <<entry, row, turn, judged, journal, seen, at, restarts>>
StartWatchAgain == /\ start = "Waiting" /\ Mode = "Rev3"
                   /\ UNCHANGED vars

Next == Judge \/ Restart \/ BusyInput \/ JournalCheck \/ StartAbsent \/ StartWatchAgain

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

\* A journal is dropped only on a channel whose policy is tmux; unknown is no departure.
JournalKept == journal = "Dropped" => Policy = "Tmux"
\* A reader answers what the turn host judged at the moment it read (`at`).
ConsumersAgree == (seen # "None" /\ at # "None") => seen = at
\* With nothing judged, a Bound Herdr thread is never read as tmux.
NoUnknownAsTmux == (seen # "None" /\ at = "None" /\ row = "Bound") => seen # "Tmux"
\* Known gap, closed with the activation slice: after a restart a Bound Herdr thread reads
\* Unknown until its next turn judgement; boot restore does not seed the record.
BoundThreadSeenAfterRestart == (seen # "None" /\ row = "Bound") => seen = "Herdr"
\* An own boot Herdr channel with no execution starts its next input.
BootStartResolves == (start = "Waiting") ~> (start = "Started")
=============================================================================
