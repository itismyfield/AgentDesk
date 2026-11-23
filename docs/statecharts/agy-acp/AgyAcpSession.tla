--------------------------- MODULE AgyAcpSession ---------------------------
\* Person-driven antigravity channel over `agy acp` (design for #8339).
\* One channel, two person inputs, one AgentDesk restart, one agy exit, one stop, one clear,
\* one refused resume and one failed steer prompt. A person input that arrives while a prompt
\* is open is steered into that turn: it joins the turn, and the turn's one answer covers it.
\* A steer error of either kind (rejected before execution, or unknown outcome) never returns
\* the input to the queue: it stays in the turn, is never sent again, and the turn's one post
\* covers it with a notice.
\* The child runs in its own process group, so an AgentDesk restart can leave it orphaned;
\* boot sweeps that group before a new child is spawned.
\* The platform kill calls (Unix group signals, Windows taskkill /T and the dcserver job
\* object) are outside the model: Kill and Crash stand for both, and process tests check them.
\* Output custody (spool -> writer -> Discord receipt) is abstracted to Settle; its
\* exactly-once delivery is checked by tui-o-writer/WriterAdoption (AgySpec).
EXTENDS Naturals, FiniteSets

CONSTANTS Lifetime,    \* "resident" | "per_turn"
          AttachMode,  \* "resume" | "load_gated" | "load_leak"
          SteerMode,   \* "steer" | "early_close" | "after_close" | "error_requeue" | "error_silent"
          BootMode,    \* "fail" | "rerun_sent"
          Watchdog,    \* TRUE | FALSE
          ClearMode,   \* "detach" | "keep"
          Fence        \* TRUE: responses carry the child incarnation

Inputs == 1..2
NoGen == 0
None == 0

VARIABLES running,     \* AgentDesk process is up
          proc,        \* "none" | "up" | "dead": the channel's agy acp child
          attached,    \* session generation the live child serves (0 = none)
          stored,      \* durable session generation for the channel (0 = none)
          nextGen,     \* next fresh session generation
          clearedGen,  \* highest generation retired by clear
          loading,     \* session/load replay still streaming
          st,          \* per input: "absent" | "queued" | "sent" | "done"
          sentCount,   \* accepted session/prompt writes per input
          turnOf,      \* per input: leader input of the turn it belongs to (0 = none)
          open,        \* inputs whose session/prompt has no response yet
          done,        \* per leader: durable terminal record of the turn
          outcome,     \* per leader: "none" | "answer" | "failure" | "cancelled"
          posts,       \* per input: terminal posts that cover it
          refusedIn,   \* per input: leader of the turn whose steer failed and re-queued it (mutant)
          cancelReq, cancelSent,
          zombie,      \* a killed child's late response is still in the pipe
          wrongDone,   \* a turn was ended by another child's response
          badSend,     \* a prompt went to a session retired by clear
          served,      \* the live child has received a prompt
          steerFailed, \* per input: its steer prompt ended with an error (either kind)
          noticed,     \* per input: the post that covered it carried the steer-error notice
          orphan,      \* a child from before the restart may still run in its own group
          clearedSent, \* per input: it had been sent when /clear ran
          crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, steerFailLeft

vars == <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount, turnOf,
          open, done, outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone,
          badSend, served, steerFailed, noticed, orphan, clearedSent, crashLeft, dieLeft, stopLeft,
          clearLeft, refuseLeft, steerFailLeft>>

Queued == {i \in Inputs : st[i] = "queued"}
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
Max2(a, b) == IF a >= b THEN a ELSE b
Members(L) == {i \in Inputs : turnOf[i] = L}
Leaders == {L \in Inputs : turnOf[L] = L}
Active == {L \in Leaders : ~done[L]}
Closing == {L \in Leaders : done[L] /\ posts[L] = 0}
\* Turns with an open prompt after `o` replaces `open`, given membership `t`.
OpenTurns(o, t) == {t[i] : i \in o}

sessionVars == <<attached, stored, nextGen, clearedGen, loading>>
budgetVars == <<crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, steerFailLeft>>
extraVars == <<steerFailed, noticed, orphan, clearedSent>>

Init ==
    /\ running = TRUE /\ proc = "none" /\ attached = NoGen /\ stored = NoGen /\ nextGen = 1
    /\ clearedGen = 0 /\ loading = FALSE
    /\ st = [i \in Inputs |-> "absent"] /\ sentCount = [i \in Inputs |-> 0]
    /\ turnOf = [i \in Inputs |-> None] /\ open = {}
    /\ done = [i \in Inputs |-> FALSE] /\ outcome = [i \in Inputs |-> "none"]
    /\ posts = [i \in Inputs |-> 0] /\ refusedIn = [i \in Inputs |-> None]
    /\ cancelReq = FALSE /\ cancelSent = FALSE /\ zombie = FALSE
    /\ wrongDone = FALSE /\ badSend = FALSE /\ served = FALSE
    /\ crashLeft = 1 /\ dieLeft = 1 /\ stopLeft = 1 /\ clearLeft = 1 /\ refuseLeft = 1
    /\ steerFailLeft = 1
    /\ steerFailed = [i \in Inputs |-> FALSE] /\ noticed = [i \in Inputs |-> FALSE]
    /\ orphan = FALSE /\ clearedSent = [i \in Inputs |-> FALSE]

\* A person posts in Discord (or an authenticated iMessage arrives); the mailbox keeps it.
Arrive(i) ==
    /\ st[i] = "absent" /\ IF i = 1 THEN TRUE ELSE st[i - 1] # "absent"
    /\ st' = [st EXCEPT ![i] = "queued"]
    /\ UNCHANGED <<running, proc, sessionVars, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served, budgetVars, extraVars>>

Spawn ==
    /\ running /\ proc # "up" /\ Queued # {} /\ ~orphan
    /\ proc' = "up" /\ served' = FALSE /\ attached' = NoGen /\ loading' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, open, done,
                   outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   budgetVars, extraVars>>

NewSession ==
    /\ running /\ proc = "up" /\ attached = NoGen /\ stored = NoGen /\ nextGen <= 3
    /\ attached' = nextGen /\ stored' = nextGen /\ nextGen' = nextGen + 1
    /\ UNCHANGED <<running, proc, clearedGen, loading, st, sentCount, turnOf, open, done, outcome,
                   posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   budgetVars, extraVars>>

\* session/resume (or session/load) of the stored id.
Reattach ==
    /\ running /\ proc = "up" /\ attached = NoGen /\ stored # NoGen
    /\ attached' = stored /\ loading' = (AttachMode # "resume")
    /\ UNCHANGED <<running, proc, stored, nextGen, clearedGen, st, sentCount, turnOf, open, done,
                   outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   served, budgetVars, extraVars>>

\* agy no longer knows the stored id: a new session starts (with a context-loss notice).
ReattachRefused ==
    /\ running /\ proc = "up" /\ attached = NoGen /\ stored # NoGen /\ nextGen <= 3
    /\ refuseLeft > 0 /\ refuseLeft' = refuseLeft - 1
    /\ attached' = nextGen /\ stored' = nextGen /\ nextGen' = nextGen + 1
    /\ UNCHANGED <<running, proc, clearedGen, loading, st, sentCount, turnOf, open, done, outcome,
                   posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   crashLeft, dieLeft, stopLeft, clearLeft, steerFailLeft, extraVars>>

\* session/load replays an old answer as session/update before it responds.
ReplayOld(i) ==
    /\ running /\ loading /\ posts[i] = 1 /\ open = {}
    /\ posts' = IF AttachMode = "load_leak" THEN [posts EXCEPT ![i] = posts[i] + 1] ELSE posts
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served, budgetVars, extraVars>>

LoadAnswered ==
    /\ running /\ loading /\ loading' = FALSE
    /\ UNCHANGED <<running, proc, attached, stored, nextGen, clearedGen, st, sentCount, turnOf,
                   open, done, outcome, posts, refusedIn, cancelReq, cancelSent, zombie,
                   wrongDone, badSend, served, budgetVars, extraVars>>

\* The next queued input is written as session/prompt after its durable record (the turn's
\* origin list). While the open turn still has a prompt pending it steers that turn; otherwise
\* it leads a new turn once no prompt is open. The mutant's re-queued steer avoids its old turn.
JoinTarget ==
    IF SteerMode = "after_close" /\ Active = {} THEN Closing ELSE Active
SendPrompt ==
    /\ running /\ proc = "up" /\ attached # NoGen /\ ~loading /\ Queued # {} /\ ~cancelReq
    /\ LET i == Min(Queued)
           target == JoinTarget \ {refusedIn[i]}
           steer == target # {} /\ (SteerMode = "after_close" \/ open # {})
       IN /\ IF steer
               THEN /\ \E L \in target : turnOf' = [turnOf EXCEPT ![i] = L]
                    /\ done' = done
               ELSE /\ open = {}
                    /\ turnOf' = [turnOf EXCEPT ![i] = i]
                    /\ done' = [done EXCEPT ![i] = FALSE]
          /\ st' = [st EXCEPT ![i] = "sent"]
          /\ sentCount' = [sentCount EXCEPT ![i] = sentCount[i] + 1]
          /\ open' = open \cup {i}
          /\ badSend' = (badSend \/ attached <= clearedGen)
    /\ served' = TRUE
    /\ UNCHANGED <<running, proc, sessionVars, outcome, posts, refusedIn, cancelReq, cancelSent,
                   zombie, wrongDone, budgetVars, extraVars>>

EndCancel(o) == IF o = {} THEN FALSE ELSE cancelReq
EndCancelSent(o) == IF o = {} THEN FALSE ELSE cancelSent
Finish(L, ends) ==
    /\ done' = IF ends THEN [done EXCEPT ![L] = TRUE] ELSE done
    /\ outcome' = IF ends /\ ~done[L]
                    THEN [outcome EXCEPT ![L] = IF cancelSent THEN "cancelled" ELSE "answer"]
                    ELSE outcome

\* agy answers the oldest open prompt; a steered earlier prompt gets end_turn while the newer
\* one carries the turn on. The turn ends only when none of its prompts is still open.
AgyResponds ==
    /\ running /\ proc = "up" /\ open # {}
    /\ LET i == Min(open)
           L == turnOf[i]
           rest == open \ {i}
           ends == SteerMode = "early_close" \/ L \notin OpenTurns(rest, turnOf)
       IN /\ open' = rest
          /\ Finish(L, ends)
          /\ cancelReq' = EndCancel(rest) /\ cancelSent' = EndCancelSent(rest)
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, posts, refusedIn, zombie,
                   wrongDone, badSend, served, budgetVars, extraVars>>

\* The steer prompt fails: rejected before execution, or an unknown outcome (internal error,
\* request aborted, any other code, deadline or lost pipe after the write). Either way the
\* input stays in the turn, is never sent again, and the turn's post covers it with a notice
\* (the mutant re-queues it).
SteerErrored ==
    /\ running /\ proc = "up" /\ steerFailLeft > 0
    /\ \E i \in open :
        /\ turnOf[i] # i
        /\ LET L == turnOf[i]
               rest == open \ {i}
               requeue == SteerMode = "error_requeue"
               t2 == IF requeue THEN [turnOf EXCEPT ![i] = None] ELSE turnOf
           IN /\ open' = rest
              /\ turnOf' = t2
              /\ st' = IF requeue THEN [st EXCEPT ![i] = "queued"] ELSE st
              /\ refusedIn' = IF requeue THEN [refusedIn EXCEPT ![i] = L] ELSE refusedIn
              /\ steerFailed' = IF requeue THEN steerFailed ELSE [steerFailed EXCEPT ![i] = TRUE]
              /\ Finish(L, L \notin OpenTurns(rest, t2))
              /\ cancelReq' = EndCancel(rest) /\ cancelSent' = EndCancelSent(rest)
    /\ steerFailLeft' = steerFailLeft - 1
    /\ UNCHANGED <<running, proc, sessionVars, sentCount, posts, zombie, wrongDone, badSend,
                   served, noticed, orphan, clearedSent, crashLeft, dieLeft, stopLeft, clearLeft,
                   refuseLeft>>

\* Close every turn that still has an open prompt with the given outcome.
FailOpenTurns(kind) ==
    LET hit == OpenTurns(open, turnOf) IN
    /\ done' = [L \in Inputs |-> done[L] \/ L \in hit]
    /\ outcome' = [L \in Inputs |-> IF L \in hit /\ ~done[L] THEN kind ELSE outcome[L]]

\* The child exits on its own; stdout EOF fails every open prompt.
AgyDies ==
    /\ running /\ proc = "up" /\ dieLeft > 0
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ dieLeft' = dieLeft - 1
    /\ FailOpenTurns("failure") /\ open' = {}
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, posts, refusedIn,
                   zombie, wrongDone, badSend, served, crashLeft, stopLeft, clearLeft, refuseLeft,
                   steerFailLeft, extraVars>>

\* No update within the inactivity budget, or no cancelled answer within the cancel budget:
\* kill the child's own process group and close the open turns.
WatchdogKill ==
    /\ Watchdog /\ running /\ proc = "up" /\ open # {}
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ zombie' = TRUE
    /\ FailOpenTurns(IF cancelSent THEN "cancelled" ELSE "failure") /\ open' = {}
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, posts, refusedIn,
                   wrongDone, badSend, served, budgetVars, extraVars>>

\* The killed child's buffered response is read after a new prompt is open.
ZombieResponse ==
    /\ running /\ zombie /\ zombie' = FALSE
    /\ IF ~Fence /\ open # {}
         THEN LET j == Min(open)
                  rest == open \ {j}
              IN /\ open' = rest
                 /\ Finish(turnOf[j], turnOf[j] \notin OpenTurns(rest, turnOf))
                 /\ wrongDone' = TRUE
         ELSE UNCHANGED <<open, done, outcome, wrongDone>>
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, posts, refusedIn, cancelReq,
                   cancelSent, badSend, served, budgetVars, extraVars>>

UserStop ==
    /\ running /\ open # {} /\ stopLeft > 0 /\ ~cancelReq
    /\ cancelReq' = TRUE /\ stopLeft' = stopLeft - 1
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelSent, zombie, wrongDone, badSend, served, crashLeft, dieLeft,
                   clearLeft, refuseLeft, steerFailLeft, extraVars>>

SendCancel ==
    /\ running /\ proc = "up" /\ cancelReq /\ ~cancelSent /\ open # {}
    /\ cancelSent' = TRUE
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, zombie, wrongDone, badSend, served, budgetVars, extraVars>>

\* /clear retires the stored session; a running turn is stopped like !stop. Inputs already sent
\* stay members of their turn: its cancel or failure post covers them, also when a steer error
\* arrives after the clear.
Clear ==
    /\ running /\ clearLeft > 0 /\ clearLeft' = clearLeft - 1
    /\ clearedGen' = Max2(clearedGen, Max2(stored, attached))
    /\ stored' = NoGen
    /\ attached' = IF ClearMode = "detach" THEN NoGen ELSE attached
    /\ cancelReq' = (open # {})
    /\ clearedSent' = [i \in Inputs |-> clearedSent[i] \/ st[i] = "sent"]
    /\ UNCHANGED <<running, proc, nextGen, loading, st, sentCount, turnOf, open, done, outcome,
                   posts, refusedIn, cancelSent, zombie, wrongDone, badSend, served, crashLeft,
                   dieLeft, stopLeft, refuseLeft, steerFailLeft, steerFailed, noticed, orphan>>

\* One-turn lifetime: the child is closed after its last prompt settles.
CloseAfterTurn ==
    /\ Lifetime = "per_turn" /\ running /\ proc = "up" /\ open = {} /\ attached # NoGen /\ served
    /\ proc' = "none" /\ attached' = NoGen
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, loading, st, sentCount, turnOf, open, done,
                   outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   served, budgetVars, extraVars>>

\* The writer posts the turn's recorded disposition once; that post covers every member, and
\* carries one notice line for each member whose steer failed (the mutant leaves it out).
Settle(L) ==
    /\ running /\ L \in Leaders /\ done[L] /\ posts[L] = 0
    /\ posts' = [i \in Inputs |-> IF turnOf[i] = L THEN posts[i] + 1 ELSE posts[i]]
    /\ st' = [i \in Inputs |-> IF turnOf[i] = L THEN "done" ELSE st[i]]
    /\ noticed' = [i \in Inputs |->
                     IF turnOf[i] = L /\ SteerMode # "error_silent" THEN steerFailed[i]
                     ELSE noticed[i]]
    /\ UNCHANGED <<running, proc, sessionVars, sentCount, turnOf, open, done, outcome, refusedIn,
                   cancelReq, cancelSent, zombie, wrongDone, badSend, served, budgetVars,
                   steerFailed, orphan, clearedSent>>

\* AgentDesk restart: memory is lost. The child has its own process group, so it may outlive
\* AgentDesk as an orphan until boot sweeps it.
Crash ==
    /\ running /\ crashLeft > 0
    /\ running' = FALSE /\ crashLeft' = crashLeft - 1
    /\ proc' = IF proc = "up" THEN "dead" ELSE proc
    /\ attached' = NoGen /\ loading' = FALSE /\ open' = {} /\ zombie' = FALSE
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ orphan' = (proc = "up")
    /\ UNCHANGED <<stored, nextGen, clearedGen, st, sentCount, turnOf, done, outcome, posts,
                   refusedIn, wrongDone, badSend, served, dieLeft, stopLeft, clearLeft, refuseLeft,
                   steerFailLeft, steerFailed, noticed, clearedSent>>

\* Boot sweeps the recorded child group of the previous AgentDesk before any new spawn.
SweepOrphan ==
    /\ running /\ orphan /\ orphan' = FALSE
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   steerFailed, noticed, clearedSent, budgetVars>>

\* Boot: a turn with written prompts and no terminal record is failed with a notice (the mutant
\* re-queues its inputs and writes them again).
Boot ==
    /\ ~running /\ running' = TRUE
    /\ IF BootMode = "rerun_sent"
         THEN /\ st' = [i \in Inputs |-> IF turnOf[i] \in Active THEN "queued" ELSE st[i]]
              /\ turnOf' = [i \in Inputs |-> IF turnOf[i] \in Active THEN None ELSE turnOf[i]]
              /\ UNCHANGED <<done, outcome>>
         ELSE /\ done' = [L \in Inputs |-> done[L] \/ L \in Active]
              /\ outcome' = [L \in Inputs |-> IF L \in Active THEN "failure" ELSE outcome[L]]
              /\ UNCHANGED <<st, turnOf>>
    /\ UNCHANGED <<proc, sessionVars, sentCount, open, posts, refusedIn, cancelReq, cancelSent,
                   zombie, wrongDone, badSend, served, budgetVars, extraVars>>

AllSettled == \A i \in Inputs : st[i] = "done"
Rest == AllSettled /\ UNCHANGED vars

Next ==
    \/ \E i \in Inputs : Arrive(i) \/ ReplayOld(i) \/ Settle(i)
    \/ Spawn \/ NewSession \/ Reattach \/ ReattachRefused \/ LoadAnswered
    \/ SendPrompt \/ AgyResponds \/ SteerErrored \/ AgyDies \/ WatchdogKill \/ ZombieResponse
    \/ UserStop \/ SendCancel \/ Clear \/ CloseAfterTurn \/ Crash \/ SweepOrphan \/ Boot \/ Rest

\* agy itself is not assumed to answer (no fairness on AgyResponds or SteerErrored): only
\* AgentDesk's own steps, the watchdog included, are fair.
Fairness ==
    /\ WF_vars(Spawn) /\ WF_vars(NewSession) /\ WF_vars(Reattach) /\ WF_vars(LoadAnswered)
    /\ WF_vars(SendPrompt) /\ WF_vars(SendCancel) /\ WF_vars(WatchdogKill)
    /\ WF_vars(ZombieResponse) /\ WF_vars(Boot) /\ WF_vars(CloseAfterTurn)
    /\ WF_vars(SweepOrphan)
    /\ \A i \in Inputs : WF_vars(Settle(i))

Spec == Init /\ [][Next]_vars /\ Fairness

\* Rules
AtMostOnePost == \A i \in Inputs : posts[i] <= 1
NoDuplicateExecution == \A i \in Inputs : sentCount[i] <= 1
NoForeignResponse == ~wrongDone
NoPromptToClearedSession == ~badSend
PostOnlyWithDisposition ==
    \A i \in Inputs : posts[i] >= 1 => turnOf[i] # None /\ done[turnOf[i]]
\* A turn's answer is posted only after every steered prompt in it has been answered.
AnswerCoversWholeSteer == \A i \in Inputs : posts[i] >= 1 => i \notin open
\* A failed steer is never covered as a plain answer: the post that covers it names the failure.
NoAnswerForRefusedSteer == \A i \in Inputs : posts[i] >= 1 /\ steerFailed[i] => noticed[i]
\* A sent input always has a durable owner: the turn whose origin list holds it.
DurableOwner == \A i \in Inputs : st[i] = "sent" => turnOf[i] # None
\* An input that had been sent when /clear ran is never queued again.
NoReviveAfterClear == \A i \in Inputs : clearedSent[i] => st[i] # "queued"
\* Reachability witnesses (expected to break when checked alone): a steered input answered by
\* its turn, a failed steer covered by its turn's post, and a steer that fails after /clear.
NoSteeredAnswer ==
    ~\E i \in Inputs : turnOf[i] \notin {None, i} /\ posts[i] = 1 /\ outcome[turnOf[i]] = "answer"
NoUncertainCovered ==
    ~\E i \in Inputs : steerFailed[i] /\ posts[i] = 1
NoSteerFailAfterClear ==
    ~\E i \in Inputs : clearedSent[i] /\ steerFailed[i] /\ posts[i] = 1
\* Liveness: every accepted input is eventually covered by one terminal post
\* (answer, failure notice or cancel notice), steered inputs included.
EveryInputSettles == \A i \in Inputs : (st[i] # "absent") ~> (posts[i] >= 1)
=============================================================================
