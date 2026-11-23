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
\* A steered input's origin is written ahead into the turn's durable row, but the bridge's
\* in-memory copy learns it only at its next stream tick. A stop, /clear or restart can come
\* first, so the completion receipt merges the latest durable origins before it commits.
\* The platform kill calls (Unix group signals, Windows taskkill /T and the dcserver job
\* object) are outside the model: Kill and Crash stand for both, and process tests check them.
\* A forced stop (force-kill or session cleanup) records its intent before it publishes its
\* cancel, and ends the child even after a person's session/cancel went out or agy's answer
\* closed the turn first. Each child's record names its incarnation; a stopped child's
\* exit report reaches AgentDesk late, after the next child may have written its own record.
\* Output custody (spool -> writer -> Discord receipt) is abstracted to Settle; its
\* exactly-once delivery is checked by tui-o-writer/WriterAdoption (AgySpec).
EXTENDS Naturals, FiniteSets

CONSTANTS Lifetime,    \* "resident" | "per_turn"
          AttachMode,  \* "resume" | "load_gated" | "load_leak"
          SteerMode,   \* "steer" | "early_close" | "after_close" | "error_requeue" | "error_silent"
          BootMode,    \* "fail" | "rerun_sent"
          Watchdog,    \* TRUE | FALSE
          ClearMode,   \* "detach" | "keep"
          Fence,       \* TRUE: responses carry the child incarnation
          ReceiptMode, \* "merge": the receipt merges the durable origins | "memory"
          ForceMode,   \* "promote": a forced stop follows a sent cancel | "first_only" | "off"
          RecordMode,  \* "owner": an exit removes only its own record | "waiter" | "off"
          ForceOrder   \* "intent_first": intent before the cancel | "cancel_first"

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
          settlerView, \* per input: the receipt writer's state already holds its origin
          receiptHas,  \* per input: the committed receipt names its origin
          merged,      \* per input: only the durable merge put it in the receipt
          forceReq,    \* a forced stop's intent is recorded and its child still runs
          forceAsked,  \* a forced stop was asked for and its child still runs
          intentDue,   \* cancel_first: the cancel is out, the intent not yet recorded
          recorded,    \* the latest spawned child's record is on disk
          curExit,     \* the latest child ended and its exit report has not arrived yet
          oldExits,    \* exit reports of earlier children that have not arrived yet
          crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, steerFailLeft, forceLeft

vars == <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount, turnOf,
          open, done, outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone,
          badSend, served, steerFailed, noticed, orphan, clearedSent, settlerView, receiptHas,
          merged, forceReq, forceAsked, intentDue, recorded, curExit, oldExits, crashLeft, dieLeft, stopLeft, clearLeft,
          refuseLeft, steerFailLeft, forceLeft>>

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
budgetVars == <<crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, steerFailLeft, forceLeft>>
receiptVars == <<settlerView, receiptHas, merged>>
childVars == <<forceReq, forceAsked, intentDue, recorded, curExit, oldExits>>
extraVars == <<steerFailed, noticed, orphan, clearedSent, receiptVars, childVars>>

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
    /\ settlerView = [i \in Inputs |-> FALSE] /\ receiptHas = [i \in Inputs |-> FALSE]
    /\ merged = [i \in Inputs |-> FALSE]
    /\ forceReq = FALSE /\ forceAsked = FALSE /\ intentDue = FALSE /\ recorded = FALSE /\ curExit = FALSE /\ oldExits = 0 /\ forceLeft = 1

\* A person posts in Discord (or an authenticated iMessage arrives); the mailbox keeps it.
Arrive(i) ==
    /\ st[i] = "absent" /\ IF i = 1 THEN TRUE ELSE st[i - 1] # "absent"
    /\ st' = [st EXCEPT ![i] = "queued"]
    /\ UNCHANGED <<running, proc, sessionVars, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served, budgetVars, extraVars>>

Spawn ==
    /\ running /\ proc # "up" /\ Queued # {} /\ ~orphan
    /\ proc' = "up" /\ served' = FALSE /\ attached' = NoGen /\ loading' = FALSE
    /\ recorded' = TRUE /\ curExit' = FALSE /\ oldExits' = oldExits + (IF curExit THEN 1 ELSE 0)
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, open, done,
                   outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   budgetVars, steerFailed, noticed, orphan, clearedSent, receiptVars, forceReq,
                   forceAsked, intentDue>>

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
                   crashLeft, dieLeft, stopLeft, clearLeft, steerFailLeft, forceLeft, extraVars>>

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
    /\ ~forceReq
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
          /\ settlerView' = [settlerView EXCEPT ![i] = ~steer]
    /\ served' = TRUE
    /\ UNCHANGED <<running, proc, sessionVars, outcome, posts, refusedIn, cancelReq, cancelSent,
                   zombie, wrongDone, budgetVars, steerFailed, noticed, orphan, clearedSent,
                   receiptHas, merged, childVars>>

\* The bridge's stream tick reads the durable row and learns a steered origin. Not fair: a
\* stop, /clear or restart may come before it.
StreamTick(i) ==
    /\ running /\ turnOf[i] # None /\ ~settlerView[i] /\ posts[i] = 0
    /\ settlerView' = [settlerView EXCEPT ![i] = TRUE]
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   budgetVars, steerFailed, noticed, orphan, clearedSent, receiptHas, merged,
                   childVars>>

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
                   wrongDone, badSend, served, budgetVars, steerFailed, noticed, orphan,
                   clearedSent, receiptVars, childVars>>

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
                   served, noticed, orphan, clearedSent, receiptVars, crashLeft, dieLeft, stopLeft,
                   clearLeft, refuseLeft, forceLeft, childVars>>

\* Close every turn that still has an open prompt with the given outcome.
FailOpenTurns(kind) ==
    LET hit == OpenTurns(open, turnOf) IN
    /\ done' = [L \in Inputs |-> done[L] \/ L \in hit]
    /\ outcome' = [L \in Inputs |-> IF L \in hit /\ ~done[L] THEN kind ELSE outcome[L]]

\* The child exits on its own; stdout EOF fails every open prompt (a turn being cancelled ends
\* as cancelled).
AgyDies ==
    /\ running /\ proc = "up" /\ dieLeft > 0
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ dieLeft' = dieLeft - 1
    /\ FailOpenTurns(IF cancelSent THEN "cancelled" ELSE "failure") /\ open' = {}
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE /\ forceReq' = FALSE
    /\ forceAsked' = FALSE /\ curExit' = (RecordMode # "off")
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, posts, refusedIn,
                   zombie, wrongDone, badSend, served, crashLeft, stopLeft, clearLeft, refuseLeft,
                   steerFailLeft, forceLeft, steerFailed, noticed, orphan, clearedSent,
                   receiptVars, intentDue, recorded, oldExits>>

\* No update within the inactivity budget, or no cancelled answer within the cancel budget:
\* kill the child's own process group and close the open turns.
WatchdogKill ==
    /\ Watchdog /\ running /\ proc = "up" /\ open # {}
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ zombie' = TRUE
    /\ FailOpenTurns(IF cancelSent THEN "cancelled" ELSE "failure") /\ open' = {}
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE /\ forceReq' = FALSE
    /\ forceAsked' = FALSE /\ curExit' = (RecordMode # "off")
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, posts, refusedIn,
                   wrongDone, badSend, served, budgetVars, steerFailed, noticed, orphan,
                   clearedSent, receiptVars, intentDue, recorded, oldExits>>

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
                   clearLeft, refuseLeft, steerFailLeft, forceLeft, extraVars>>

\* A force-kill or session cleanup of the channel's turn; it may follow a person's stop. Its
\* intent is recorded before its cancel is published (the mutant publishes the cancel first).
ForceStop ==
    /\ ForceMode # "off" /\ running /\ open # {} /\ forceLeft > 0 /\ ~forceAsked
    /\ forceReq' = (ForceOrder = "intent_first") /\ intentDue' = (ForceOrder = "cancel_first")
    /\ forceAsked' = TRUE /\ cancelReq' = TRUE /\ forceLeft' = forceLeft - 1
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelSent, zombie, wrongDone, badSend, served, crashLeft, dieLeft,
                   stopLeft, clearLeft, refuseLeft, steerFailLeft, steerFailed, noticed, orphan,
                   clearedSent, receiptVars, recorded, curExit, oldExits>>

\* cancel_first: the intent lands only after the cancel; a turn that agy's answer already
\* closed has no drive left to read it, so its child keeps running.
ForceIntent ==
    /\ running /\ intentDue /\ intentDue' = FALSE /\ forceReq' = (open # {})
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   budgetVars, steerFailed, noticed, orphan, clearedSent, receiptVars, forceAsked,
                   recorded, curExit, oldExits>>

\* The turn sends Terminate: the child's own group is killed and its open turns end cancelled,
\* also after agy's answer closed the turn. The mutant sends it only when no session/cancel
\* went out first.
SendTerminate ==
    /\ running /\ proc = "up" /\ forceReq
    /\ ForceMode = "promote" \/ ~cancelSent
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ zombie' = TRUE
    /\ FailOpenTurns("cancelled") /\ open' = {}
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE /\ forceReq' = FALSE
    /\ forceAsked' = FALSE /\ curExit' = (RecordMode # "off")
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, posts, refusedIn,
                   wrongDone, badSend, served, budgetVars, steerFailed, noticed, orphan,
                   clearedSent, receiptVars, intentDue, recorded, oldExits>>

\* A stopped child's exit report: the record goes only if it is that child's (the mutant removes
\* whatever record is on disk). A report from an earlier child arrives after the next spawn.
ExitReported(latest) ==
    /\ running /\ IF latest THEN curExit ELSE oldExits > 0
    /\ curExit' = (curExit /\ ~latest) /\ oldExits' = IF latest THEN oldExits ELSE oldExits - 1
    /\ recorded' = (recorded /\ ~latest /\ RecordMode = "owner")
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   budgetVars, steerFailed, noticed, orphan, clearedSent, receiptVars, forceReq,
                   forceAsked, intentDue>>

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
                   dieLeft, stopLeft, refuseLeft, steerFailLeft, forceLeft, steerFailed, noticed,
                   orphan, receiptVars, childVars>>

\* One-turn lifetime: the child is closed after its last prompt settles.
CloseAfterTurn ==
    /\ Lifetime = "per_turn" /\ running /\ proc = "up" /\ open = {} /\ attached # NoGen /\ served
    /\ proc' = "none" /\ attached' = NoGen /\ curExit' = (RecordMode # "off")
    /\ forceReq' = FALSE /\ forceAsked' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, loading, st, sentCount, turnOf, open, done,
                   outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   served, budgetVars, steerFailed, noticed, orphan, clearedSent, receiptVars,
                   intentDue, recorded, oldExits>>

\* The writer posts the turn's recorded disposition once; that post covers every member, and
\* carries one notice line for each member whose steer failed (the mutant leaves it out). The
\* receipt commits with it: the writer's own origins merged with the turn's durable row (the
\* mutant uses only the writer's in-memory origins).
Settle(L) ==
    /\ running /\ L \in Leaders /\ done[L] /\ posts[L] = 0
    /\ posts' = [i \in Inputs |-> IF turnOf[i] = L THEN posts[i] + 1 ELSE posts[i]]
    /\ st' = [i \in Inputs |-> IF turnOf[i] = L THEN "done" ELSE st[i]]
    /\ noticed' = [i \in Inputs |->
                     IF turnOf[i] = L /\ SteerMode # "error_silent" THEN steerFailed[i]
                     ELSE noticed[i]]
    /\ receiptHas' = [i \in Inputs |->
                        IF turnOf[i] = L THEN ReceiptMode = "merge" \/ settlerView[i]
                        ELSE receiptHas[i]]
    /\ merged' = [i \in Inputs |->
                    IF turnOf[i] = L THEN ReceiptMode = "merge" /\ ~settlerView[i]
                    ELSE merged[i]]
    /\ UNCHANGED <<running, proc, sessionVars, sentCount, turnOf, open, done, outcome, refusedIn,
                   cancelReq, cancelSent, zombie, wrongDone, badSend, served, budgetVars,
                   steerFailed, orphan, clearedSent, settlerView, childVars>>

\* AgentDesk restart: memory is lost. The child has its own process group, so it may outlive
\* AgentDesk as an orphan until boot sweeps it.
Crash ==
    /\ running /\ crashLeft > 0
    /\ running' = FALSE /\ crashLeft' = crashLeft - 1
    /\ proc' = IF proc = "up" THEN "dead" ELSE proc
    /\ attached' = NoGen /\ loading' = FALSE /\ open' = {} /\ zombie' = FALSE
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE /\ forceReq' = FALSE
    /\ forceAsked' = FALSE /\ intentDue' = FALSE /\ curExit' = FALSE /\ oldExits' = 0
    /\ orphan' = (proc = "up") /\ settlerView' = [i \in Inputs |-> FALSE]
    /\ UNCHANGED <<stored, nextGen, clearedGen, st, sentCount, turnOf, done, outcome, posts,
                   refusedIn, wrongDone, badSend, served, dieLeft, stopLeft, clearLeft, refuseLeft,
                   steerFailLeft, forceLeft, steerFailed, noticed, clearedSent, receiptHas, merged,
                   recorded>>

\* Boot sweeps the recorded child group of the previous AgentDesk before any new spawn.
SweepOrphan ==
    /\ running /\ orphan /\ orphan' = FALSE /\ recorded' = FALSE
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   steerFailed, noticed, clearedSent, receiptVars, budgetVars, forceReq, forceAsked,
                   intentDue, curExit, oldExits>>

\* Boot: a turn with written prompts and no terminal record is failed with a notice (the mutant
\* re-queues its inputs and writes them again). Boot settles from the durable row, so its view
\* holds every member's origin.
Boot ==
    /\ ~running /\ running' = TRUE
    /\ IF BootMode = "rerun_sent"
         THEN /\ st' = [i \in Inputs |-> IF turnOf[i] \in Active THEN "queued" ELSE st[i]]
              /\ turnOf' = [i \in Inputs |-> IF turnOf[i] \in Active THEN None ELSE turnOf[i]]
              /\ UNCHANGED <<done, outcome>>
         ELSE /\ done' = [L \in Inputs |-> done[L] \/ L \in Active]
              /\ outcome' = [L \in Inputs |-> IF L \in Active THEN "failure" ELSE outcome[L]]
              /\ UNCHANGED <<st, turnOf>>
    /\ settlerView' = [i \in Inputs |-> turnOf[i] # None]
    /\ UNCHANGED <<proc, sessionVars, sentCount, open, posts, refusedIn, cancelReq, cancelSent,
                   zombie, wrongDone, badSend, served, budgetVars, steerFailed, noticed, orphan,
                   clearedSent, receiptHas, merged, childVars>>

AllSettled == \A i \in Inputs : st[i] = "done"
Rest == AllSettled /\ UNCHANGED vars

Next ==
    \/ \E i \in Inputs : Arrive(i) \/ ReplayOld(i) \/ Settle(i) \/ StreamTick(i)
    \/ Spawn \/ NewSession \/ Reattach \/ ReattachRefused \/ LoadAnswered
    \/ SendPrompt \/ AgyResponds \/ SteerErrored \/ AgyDies \/ WatchdogKill \/ ZombieResponse
    \/ UserStop \/ SendCancel \/ Clear \/ CloseAfterTurn \/ Crash \/ SweepOrphan \/ Boot \/ Rest
    \/ ForceStop \/ ForceIntent \/ SendTerminate \/ \E latest \in BOOLEAN : ExitReported(latest)

\* agy itself is not assumed to answer (no fairness on AgyResponds or SteerErrored): only
\* AgentDesk's own steps, the watchdog included, are fair.
Fairness ==
    /\ WF_vars(Spawn) /\ WF_vars(NewSession) /\ WF_vars(Reattach) /\ WF_vars(LoadAnswered)
    /\ WF_vars(SendPrompt) /\ WF_vars(SendCancel) /\ WF_vars(WatchdogKill)
    /\ WF_vars(ZombieResponse) /\ WF_vars(Boot) /\ WF_vars(CloseAfterTurn)
    /\ WF_vars(SweepOrphan) /\ WF_vars(SendTerminate) /\ WF_vars(ForceIntent)
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
\* Every input a post covered, steered ones included, is named by that turn's receipt.
NoMissingOrigin == \A i \in Inputs : posts[i] >= 1 => receiptHas[i]
\* A running child, or one a restart left running, always has its own record for the sweep.
ChildRecorded == (proc = "up" \/ orphan) => recorded
\* Reachability witnesses (expected to break when checked alone): a steered input answered by
\* its turn, a failed steer covered by its turn's post, a steer that fails after /clear, and a
\* cancelled receipt that needed the durable merge.
NoSteeredAnswer ==
    ~\E i \in Inputs : turnOf[i] \notin {None, i} /\ posts[i] = 1 /\ outcome[turnOf[i]] = "answer"
NoUncertainCovered ==
    ~\E i \in Inputs : steerFailed[i] /\ posts[i] = 1
NoSteerFailAfterClear ==
    ~\E i \in Inputs : clearedSent[i] /\ steerFailed[i] /\ posts[i] = 1
\* Witness: a cancelled turn's receipt names a steered input only through the durable merge.
NoMergeOnCancel ==
    ~\E i \in Inputs : merged[i] /\ turnOf[i] # None /\ outcome[turnOf[i]] = "cancelled"
\* Witnesses: a forced stop arrives after session/cancel went out, and an earlier child's exit
\* report is still due while the next child runs.
NoPromotion == ~(forceReq /\ cancelSent /\ proc = "up")
NoLateExit == ~(oldExits > 0 /\ proc = "up" /\ recorded)
\* Liveness: every accepted input is eventually covered by one terminal post
\* (answer, failure notice or cancel notice), steered inputs included.
EveryInputSettles == \A i \in Inputs : (st[i] # "absent") ~> (posts[i] >= 1)
\* Liveness: a forced stop ends the open turn even if agy never answers its cancel.
ForceEndsTurn == forceReq ~> (open = {})
\* Liveness: a forced stop ends its child, also when agy's answer closed the turn first.
ForceEndsChild == forceAsked ~> (proc # "up")
=============================================================================
