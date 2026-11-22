--------------------------- MODULE AgyAcpSession ---------------------------
\* Human agy channel driven through `agy acp` (design for #8339).
\* One channel, two person inputs, one AgentDesk restart, one agy death, one stop, one clear.
\* Output custody (spool -> writer -> Discord receipt) is abstracted to Settle; its
\* exactly-once delivery is checked by tui-o-writer/WriterAdoption (AgySpec).
EXTENDS Naturals, FiniteSets

CONSTANTS Lifetime,    \* "resident" | "per_turn"
          AttachMode,  \* "resume" | "load_gated" | "load_leak"
          InputMode,   \* "queue" | "steer" | "steer_drop"
          BootMode,    \* "fail" | "rerun_sent"
          Watchdog,    \* TRUE | FALSE
          ClearMode,   \* "detach" | "keep"
          Fence        \* TRUE: responses carry the process incarnation

Inputs == 1..2
NoGen == 0

VARIABLES running,     \* AgentDesk process is up
          proc,        \* "none" | "up" | "dead": the channel's agy acp child
          attached,    \* session generation the live child serves (0 = none)
          stored,      \* durable session generation for the channel (0 = none)
          nextGen,     \* next fresh session generation
          clearedGen,  \* highest generation retired by clear
          loading,     \* session/load replay still streaming
          st,          \* per input: "absent" | "queued" | "sent" | "done"
          sentCount,   \* session/prompt writes per input
          usedGen,     \* generation the prompt was written to
          open,        \* inputs with a pending session/prompt response
          done,        \* durable terminal disposition recorded (spool done/failed)
          outcome,     \* "none" | "answer" | "failure" | "cancelled"
          posts,       \* Discord terminal posts per input
          cancelReq, cancelSent,
          zombie,      \* a killed child's late response is still in the pipe
          wrongDone,   \* an input was settled by another prompt's response
          badSend,     \* a prompt went to a session retired by clear
          served,      \* the live child has received a prompt
          crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft

vars == <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount, usedGen,
          open, done, outcome, posts, cancelReq, cancelSent, zombie, wrongDone, badSend,
          served, crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft>>

Queued == {i \in Inputs : st[i] = "queued"}
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
Max2(a, b) == IF a >= b THEN a ELSE b

Init ==
    /\ running = TRUE /\ proc = "none" /\ attached = NoGen /\ stored = NoGen /\ nextGen = 1
    /\ clearedGen = 0 /\ loading = FALSE
    /\ st = [i \in Inputs |-> "absent"] /\ sentCount = [i \in Inputs |-> 0]
    /\ usedGen = [i \in Inputs |-> 0] /\ open = {}
    /\ done = [i \in Inputs |-> FALSE] /\ outcome = [i \in Inputs |-> "none"]
    /\ posts = [i \in Inputs |-> 0]
    /\ cancelReq = FALSE /\ cancelSent = FALSE /\ zombie = FALSE
    /\ wrongDone = FALSE /\ badSend = FALSE
    /\ served = FALSE /\ crashLeft = 1 /\ dieLeft = 1 /\ stopLeft = 1 /\ clearLeft = 1 /\ refuseLeft = 1

\* A person posts in Discord (or an authenticated iMessage arrives); the mailbox keeps it durably.
Arrive(i) ==
    /\ st[i] = "absent" /\ IF i = 1 THEN TRUE ELSE st[i - 1] # "absent"
    /\ st' = [st EXCEPT ![i] = "queued"]
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, loading, sentCount, usedGen,
                   open, done, outcome, posts, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

Spawn ==
    /\ running /\ proc # "up" /\ Queued # {}
    /\ proc' = "up" /\ served' = FALSE /\ attached' = NoGen /\ loading' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, usedGen, open, done, outcome,
                   posts, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft>>

NewSession ==
    /\ running /\ proc = "up" /\ attached = NoGen /\ stored = NoGen /\ nextGen <= 3
    /\ attached' = nextGen /\ stored' = nextGen /\ nextGen' = nextGen + 1
    /\ UNCHANGED <<running, proc, clearedGen, loading, st, sentCount, usedGen, open, done, outcome,
                   posts, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

\* session/resume (or session/load) of the stored id; agy may no longer know it.
Reattach ==
    /\ running /\ proc = "up" /\ attached = NoGen /\ stored # NoGen
    /\ attached' = stored
    /\ loading' = (AttachMode # "resume")
    /\ UNCHANGED <<running, proc, stored, nextGen, clearedGen, st, sentCount, usedGen, open, done,
                   outcome, posts, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

ReattachRefused ==
    /\ running /\ proc = "up" /\ attached = NoGen /\ stored # NoGen /\ nextGen <= 3
    /\ refuseLeft > 0 /\ refuseLeft' = refuseLeft - 1
    /\ attached' = nextGen /\ stored' = nextGen /\ nextGen' = nextGen + 1
    /\ UNCHANGED <<running, proc, clearedGen, loading, st, sentCount, usedGen, open, done, outcome,
                   posts, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, served>>

\* session/load replays old history as session/update before it answers.
ReplayOld(i) ==
    /\ running /\ loading /\ posts[i] = 1 /\ open = {}
    /\ posts' = IF AttachMode = "load_leak" THEN [posts EXCEPT ![i] = posts[i] + 1] ELSE posts
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount,
                   usedGen, open, done, outcome, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

LoadAnswered ==
    /\ running /\ loading /\ loading' = FALSE
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, st, sentCount, usedGen,
                   open, done, outcome, posts, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

\* The durable sent mark is written before session/prompt goes to stdin.
SendPrompt ==
    /\ running /\ proc = "up" /\ attached # NoGen /\ ~loading /\ Queued # {} /\ ~cancelReq
    /\ \/ open = {}
       \/ InputMode \in {"steer", "steer_drop"} /\ Cardinality(open) = 1
    /\ LET i == Min(Queued) IN
        /\ st' = [st EXCEPT ![i] = "sent"]
        /\ sentCount' = [sentCount EXCEPT ![i] = sentCount[i] + 1]
        /\ usedGen' = [usedGen EXCEPT ![i] = attached]
        /\ open' = open \cup {i}
        /\ served' = TRUE
        /\ badSend' = (badSend \/ attached <= clearedGen)
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, loading, done, outcome,
                   posts, cancelReq, cancelSent, zombie, wrongDone, crashLeft, dieLeft, stopLeft,
                   clearLeft, refuseLeft>>

EndCancel(o) == IF o = {} THEN FALSE ELSE cancelReq
EndCancelSent(o) == IF o = {} THEN FALSE ELSE cancelSent

\* agy answers the oldest open prompt: end_turn, or cancelled after session/cancel.
\* A steered first prompt is answered with end_turn while the newer prompt keeps running.
AgyResponds ==
    /\ running /\ proc = "up" /\ open # {}
    /\ LET i == Min(open)
           dropped == InputMode = "steer_drop" /\ Cardinality(open) > 1
       IN /\ open' = open \ {i}
          /\ done' = IF dropped THEN done ELSE [done EXCEPT ![i] = TRUE]
          /\ outcome' = IF dropped THEN outcome
                        ELSE [outcome EXCEPT ![i] = IF cancelSent THEN "cancelled" ELSE "answer"]
          /\ cancelReq' = EndCancel(open \ {i})
          /\ cancelSent' = EndCancelSent(open \ {i})
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount,
                   usedGen, posts, zombie, wrongDone, badSend, crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

FailOpen(kind) == [i \in Inputs |-> IF i \in open THEN kind ELSE outcome[i]]
DoneOpen == [i \in Inputs |-> done[i] \/ i \in open]

\* The child exits on its own; stdout EOF fails every pending prompt.
AgyDies ==
    /\ running /\ proc = "up" /\ dieLeft > 0
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ dieLeft' = dieLeft - 1
    /\ done' = DoneOpen /\ outcome' = FailOpen("failure") /\ open' = {}
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, usedGen, posts, zombie,
                   wrongDone, badSend, crashLeft, stopLeft, clearLeft, refuseLeft, served>>

\* No update for the inactivity budget, or no cancelled answer within the cancel budget:
\* kill the process group and close the open prompts.
WatchdogKill ==
    /\ Watchdog /\ running /\ proc = "up" /\ open # {}
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ zombie' = TRUE
    /\ done' = DoneOpen
    /\ outcome' = FailOpen(IF cancelSent THEN "cancelled" ELSE "failure")
    /\ open' = {} /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, usedGen, posts, wrongDone,
                   badSend, crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

\* The killed child's buffered response is read after a new prompt is open.
ZombieResponse ==
    /\ running /\ zombie
    /\ zombie' = FALSE
    /\ IF ~Fence /\ open # {}
         THEN LET j == Min(open) IN
              /\ open' = open \ {j}
              /\ done' = [done EXCEPT ![j] = TRUE]
              /\ outcome' = [outcome EXCEPT ![j] = "answer"]
              /\ wrongDone' = TRUE
         ELSE UNCHANGED <<open, done, outcome, wrongDone>>
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount,
                   usedGen, posts, cancelReq, cancelSent, badSend, crashLeft, dieLeft, stopLeft,
                   clearLeft, refuseLeft, served>>

UserStop ==
    /\ running /\ open # {} /\ stopLeft > 0 /\ ~cancelReq
    /\ cancelReq' = TRUE /\ stopLeft' = stopLeft - 1
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount,
                   usedGen, open, done, outcome, posts, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, clearLeft, refuseLeft, served>>

SendCancel ==
    /\ running /\ proc = "up" /\ cancelReq /\ ~cancelSent /\ open # {}
    /\ cancelSent' = TRUE
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount,
                   usedGen, open, done, outcome, posts, cancelReq, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

\* /clear retires the stored session; a running prompt is stopped like !stop.
Clear ==
    /\ running /\ clearLeft > 0
    /\ clearLeft' = clearLeft - 1
    /\ clearedGen' = Max2(clearedGen, Max2(stored, attached))
    /\ stored' = NoGen
    /\ attached' = IF ClearMode = "detach" THEN NoGen ELSE attached
    /\ cancelReq' = (open # {})
    /\ UNCHANGED <<running, proc, nextGen, loading, st, sentCount, usedGen, open, done, outcome,
                   posts, cancelSent, zombie, wrongDone, badSend, crashLeft, dieLeft, stopLeft, refuseLeft, served>>

\* One-turn lifetime: the child is closed after its last prompt settles.
CloseAfterTurn ==
    /\ Lifetime = "per_turn" /\ running /\ proc = "up" /\ open = {} /\ attached # NoGen /\ served
    /\ proc' = "none" /\ attached' = NoGen
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, loading, st, sentCount, usedGen, open, done,
                   outcome, posts, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

\* The writer posts the recorded disposition once (receipt-guarded).
Settle(i) ==
    /\ running /\ done[i] /\ posts[i] = 0
    /\ posts' = [posts EXCEPT ![i] = 1] /\ st' = [st EXCEPT ![i] = "done"]
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, loading, sentCount, usedGen,
                   open, done, outcome, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

\* AgentDesk restart: the child dies with its process group; memory is lost.
Crash ==
    /\ running /\ crashLeft > 0
    /\ running' = FALSE /\ crashLeft' = crashLeft - 1
    /\ proc' = IF proc = "up" THEN "dead" ELSE proc
    /\ attached' = NoGen /\ loading' = FALSE /\ open' = {} /\ zombie' = FALSE
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ UNCHANGED <<stored, nextGen, clearedGen, st, sentCount, usedGen, done, outcome, posts,
                   wrongDone, badSend, dieLeft, stopLeft, clearLeft, refuseLeft, served>>

\* Boot: a sent input without a terminal record is failed (or, in the mutant, sent again).
Boot ==
    /\ ~running /\ running' = TRUE
    /\ LET orphan == {i \in Inputs : st[i] = "sent" /\ ~done[i]} IN
        IF BootMode = "rerun_sent"
          THEN /\ st' = [i \in Inputs |-> IF i \in orphan THEN "queued" ELSE st[i]]
               /\ UNCHANGED <<done, outcome>>
          ELSE /\ done' = [i \in Inputs |-> done[i] \/ i \in orphan]
               /\ outcome' = [i \in Inputs |-> IF i \in orphan THEN "failure" ELSE outcome[i]]
               /\ UNCHANGED st
    /\ UNCHANGED <<proc, attached, stored, nextGen, clearedGen, loading, sentCount, usedGen, open,
                   posts, cancelReq, cancelSent, zombie, wrongDone, badSend, crashLeft, dieLeft,
                   stopLeft, clearLeft, refuseLeft, served>>

AllSettled == \A i \in Inputs : st[i] = "done"
Rest == AllSettled /\ UNCHANGED vars

Next ==
    \/ \E i \in Inputs : Arrive(i) \/ ReplayOld(i) \/ Settle(i)
    \/ Spawn \/ NewSession \/ Reattach \/ ReattachRefused \/ LoadAnswered
    \/ SendPrompt \/ AgyResponds \/ AgyDies \/ WatchdogKill \/ ZombieResponse
    \/ UserStop \/ SendCancel \/ Clear \/ CloseAfterTurn \/ Crash \/ Boot \/ Rest

\* agy itself is not assumed to answer (no fairness on AgyResponds): only AgentDesk's own
\* steps, including the watchdog, are fair.
Fairness ==
    /\ WF_vars(Spawn) /\ WF_vars(NewSession) /\ WF_vars(Reattach) /\ WF_vars(LoadAnswered)
    /\ WF_vars(SendPrompt) /\ WF_vars(SendCancel) /\ WF_vars(WatchdogKill)
    /\ WF_vars(ZombieResponse) /\ WF_vars(Boot) /\ WF_vars(CloseAfterTurn)
    /\ \A i \in Inputs : WF_vars(Settle(i))

Spec == Init /\ [][Next]_vars /\ Fairness

\* Rules
AtMostOnePost == \A i \in Inputs : posts[i] <= 1
NoDuplicateExecution == \A i \in Inputs : sentCount[i] <= 1
NoForeignResponse == ~wrongDone
NoPromptToClearedSession == ~badSend
PostOnlyWithDisposition == \A i \in Inputs : posts[i] >= 1 => done[i]
\* Liveness: every accepted input gets its one terminal post (answer, failure or cancel notice).
EveryInputSettles == \A i \in Inputs : (st[i] # "absent") ~> (posts[i] >= 1)
=============================================================================
