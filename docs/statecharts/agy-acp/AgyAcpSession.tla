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
\* Every input, the turn's first one included, is written ahead into the durable row before its
\* prompt goes on the wire; a restart can fall between the two. Before the bridge looks up the
\* receipt it seals the turn's steer acceptance; an input after the seal waits for the next turn.
\* A lookup that cannot read the row keeps the row; its mark is retried at the next admission.
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
          Fence,       \* TRUE: responses carry the child incarnation
          ReceiptMode, \* "merge": the receipt merges the durable origins | "memory"
          SealMode,    \* "seal": steer acceptance is sealed before the receipt lookup | "off"
          LeaderOrigin, \* "save": the first input's origin is written ahead too | "off"
          MarkRetry,   \* "retry": an unmarked kept row is marked again at admission | "off"
          ReadFails    \* how many receipt lookups may fail to read the row (0 | 1)

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
          st,          \* per input: "absent" | "queued" | "accepted" | "saved" | "sent" | "unsent"
                       \* | "done"
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
          looked,      \* per leader: "no" | "ok" | "fail": the bridge's receipt lookup
          held,        \* leader whose row stays after its post: the receipt is not committed
          marked,      \* the held row carries its terminal_receipt mark
          readFailLeft,
          crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, steerFailLeft

vars == <<running, proc, attached, stored, nextGen, clearedGen, loading, st, sentCount, turnOf,
          open, done, outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone,
          badSend, served, steerFailed, noticed, orphan, clearedSent, settlerView, receiptHas,
          looked, held, marked, readFailLeft, crashLeft,
          dieLeft, stopLeft, clearLeft, refuseLeft, steerFailLeft>>

Queued == {i \in Inputs : st[i] = "queued"}
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
Max2(a, b) == IF a >= b THEN a ELSE b
Members(L) == {i \in Inputs : turnOf[i] = L}
Leaders == {L \in Inputs : turnOf[L] = L}
Active == {L \in Leaders : ~done[L]}
Closing == {L \in Leaders : done[L] /\ posts[L] = 0}
\* Turns with an open prompt after `o` replaces `open`, given membership `t`.
OpenTurns(o, t) == {t[i] : i \in o}
\* Inputs the actor accepted whose prompt it has not yet written or dropped.
Saving == {i \in Inputs : st[i] \in {"accepted", "saved"}}
ActorFree == Saving = {}
InTurn(i) == st[i] \in {"saved", "sent", "unsent"}
\* The lookup seals the turn in the same step: the actor's seal answer comes first.
SealedTurns == IF SealMode = "seal" THEN {L \in Inputs : looked[L] # "no"} ELSE {}
\* Its origin is in the turn's durable row: every member's, the first input's only when saved.
Durable(i) ==
    turnOf[i] # None /\ st[i] # "accepted" /\ (turnOf[i] # i \/ LeaderOrigin = "save")
\* The bridge closes a turn when the actor recorded its end, or at once on a stop.
Closable(L) ==
    L \in Leaders /\ posts[L] = 0 /\ (done[L] \/ (cancelReq /\ L \in OpenTurns(open, turnOf)))

sessionVars == <<attached, stored, nextGen, clearedGen, loading>>
budgetVars == <<crashLeft, dieLeft, stopLeft, clearLeft, refuseLeft, steerFailLeft>>
receiptVars == <<settlerView, receiptHas>>
settleVars == <<looked, held, marked, readFailLeft>>
extraVars == <<steerFailed, noticed, orphan, clearedSent, receiptVars, settleVars>>

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
    /\ looked = [i \in Inputs |-> "no"]
    /\ held = None /\ marked = FALSE /\ readFailLeft = ReadFails

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

\* The actor accepts the next queued input. While the open, unsealed turn still has a prompt
\* pending the input steers that turn; otherwise it leads a new turn once no prompt is open and
\* no row is kept. The actor handles one input at a time. The mutant's re-queued steer avoids
\* its old turn.
JoinTarget ==
    (IF SteerMode = "after_close" /\ Active = {} THEN Closing ELSE Active) \ SealedTurns
Accept ==
    /\ running /\ proc = "up" /\ attached # NoGen /\ ~loading /\ Queued # {} /\ ~cancelReq
    /\ ActorFree
    /\ LET i == Min(Queued)
           target == JoinTarget \ {refusedIn[i]}
           steer == target # {} /\ (SteerMode = "after_close" \/ open # {})
       IN /\ IF steer
               THEN /\ \E L \in target : turnOf' = [turnOf EXCEPT ![i] = L]
                    /\ UNCHANGED <<done, looked>>
               ELSE /\ open = {} /\ held = None
                    /\ turnOf' = [turnOf EXCEPT ![i] = i]
                    /\ done' = [done EXCEPT ![i] = FALSE]
                    /\ looked' = [looked EXCEPT ![i] = "no"]
          /\ st' = [st EXCEPT ![i] = "accepted"]
    /\ UNCHANGED <<running, proc, sessionVars, sentCount, open, outcome, posts, refusedIn,
                   cancelReq, cancelSent, zombie, wrongDone, badSend, served, budgetVars,
                   steerFailed, noticed, orphan, clearedSent, receiptVars, held, marked,
                   readFailLeft>>

\* The accepted input's origin is written ahead into the turn's durable row (the first input's
\* too). A stop or seal that came meanwhile does not undo it.
Save ==
    /\ running /\ \E i \in Inputs : st[i] = "accepted" /\ st' = [st EXCEPT ![i] = "saved"]
    /\ UNCHANGED <<running, proc, sessionVars, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   budgetVars, extraVars>>

\* The actor writes the saved input's session/prompt. A steer whose turn is stopping or sealed
\* is not written: it stays in the turn as a failed steer, covered with a notice.
Wire ==
    /\ running
    /\ \E i \in Saving : st[i] = "saved" /\
        LET L == turnOf[i]
            drop == i # L /\ (cancelReq \/ L \in SealedTurns)
        IN IF drop
             THEN /\ st' = [st EXCEPT ![i] = "unsent"]
                  /\ steerFailed' = [steerFailed EXCEPT ![i] = TRUE]
                  /\ UNCHANGED <<sentCount, open, badSend, served>>
             ELSE /\ proc = "up" /\ attached # NoGen /\ ~loading
                  /\ st' = [st EXCEPT ![i] = "sent"]
                  /\ sentCount' = [sentCount EXCEPT ![i] = sentCount[i] + 1]
                  /\ open' = open \cup {i}
                  /\ badSend' = (badSend \/ attached <= clearedGen)
                  /\ served' = TRUE
                  /\ UNCHANGED steerFailed
    /\ UNCHANGED <<running, proc, sessionVars, turnOf, done, outcome, posts, refusedIn, cancelReq,
                   cancelSent, zombie, wrongDone, budgetVars, noticed, orphan, clearedSent,
                   receiptVars, settleVars>>

\* The bridge's stream tick reads the durable row and learns an origin written ahead. Not fair:
\* a stop, /clear or restart may come before it. With the merge the tick cannot change a receipt,
\* so only the memory mutant explores it.
StreamTick(i) ==
    /\ ReceiptMode = "memory" /\ running /\ Durable(i) /\ ~settlerView[i] /\ posts[i] = 0
    /\ settlerView' = [settlerView EXCEPT ![i] = TRUE]
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   budgetVars, steerFailed, noticed, orphan, clearedSent, receiptHas, settleVars>>

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
    /\ running /\ proc = "up" /\ open # {} /\ ActorFree
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
    /\ running /\ proc = "up" /\ steerFailLeft > 0 /\ ActorFree
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
                   served, noticed, orphan, clearedSent, receiptVars, settleVars, crashLeft,
                   dieLeft, stopLeft, clearLeft, refuseLeft>>

\* Close every turn that still has an open prompt with the given outcome.
FailOpenTurns(kind) ==
    LET hit == OpenTurns(open, turnOf) IN
    /\ done' = [L \in Inputs |-> done[L] \/ L \in hit]
    /\ outcome' = [L \in Inputs |-> IF L \in hit /\ ~done[L] THEN kind ELSE outcome[L]]

\* The child exits on its own; stdout EOF fails every open prompt.
AgyDies ==
    /\ running /\ proc = "up" /\ dieLeft > 0 /\ ActorFree
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ dieLeft' = dieLeft - 1
    /\ FailOpenTurns("failure") /\ open' = {}
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, posts, refusedIn,
                   zombie, wrongDone, badSend, served, crashLeft, stopLeft, clearLeft, refuseLeft,
                   steerFailLeft, extraVars>>

\* No update within the inactivity budget, or no cancelled answer within the cancel budget:
\* kill the child's own process group and close the open turns.
WatchdogKill ==
    /\ Watchdog /\ running /\ proc = "up" /\ open # {} /\ ActorFree
    /\ proc' = "dead" /\ attached' = NoGen /\ loading' = FALSE /\ zombie' = TRUE
    /\ FailOpenTurns(IF cancelSent THEN "cancelled" ELSE "failure") /\ open' = {}
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, st, sentCount, turnOf, posts, refusedIn,
                   wrongDone, badSend, served, budgetVars, extraVars>>

\* The killed child's buffered response is read after a new prompt is open.
ZombieResponse ==
    /\ running /\ zombie /\ zombie' = FALSE /\ ActorFree
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
    /\ running /\ proc = "up" /\ cancelReq /\ ~cancelSent /\ open # {} /\ ActorFree
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
    /\ clearedSent' = [i \in Inputs |-> clearedSent[i] \/ InTurn(i)]
    /\ UNCHANGED <<running, proc, nextGen, loading, st, sentCount, turnOf, open, done, outcome,
                   posts, refusedIn, cancelSent, zombie, wrongDone, badSend, served, crashLeft,
                   dieLeft, stopLeft, refuseLeft, steerFailLeft, steerFailed, noticed, orphan,
                   receiptVars, settleVars>>

\* One-turn lifetime: the child is closed after its last prompt settles.
CloseAfterTurn ==
    /\ Lifetime = "per_turn" /\ running /\ proc = "up" /\ open = {} /\ attached # NoGen /\ served
    /\ ActorFree
    /\ proc' = "none" /\ attached' = NoGen
    /\ UNCHANGED <<running, stored, nextGen, clearedGen, loading, st, sentCount, turnOf, open, done,
                   outcome, posts, refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend,
                   served, budgetVars, extraVars>>

\* The bridge first seals the turn's steer acceptance; the actor answers once any write-ahead it
\* already accepted is resolved, without waiting for agy (the mutant skips the seal). Then the
\* receipt lookup merges the bridge's own origins with the turn's durable row (the mutant uses
\* only the in-memory origins). A read that fails records nothing.
LookupNow(L) ==
    \/ /\ looked' = [looked EXCEPT ![L] = "ok"]
       /\ receiptHas' = [i \in Inputs |->
                           IF turnOf[i] = L
                             THEN settlerView[i] \/ (ReceiptMode = "merge" /\ Durable(i))
                             ELSE receiptHas[i]]
       /\ UNCHANGED readFailLeft
    \/ /\ readFailLeft > 0 /\ readFailLeft' = readFailLeft - 1
       /\ looked' = [looked EXCEPT ![L] = "fail"]
       /\ UNCHANGED receiptHas

\* The seal answers only once no input of the turn is accepted but unsaved (the mutant does not
\* wait).
SealWaits(L) == SealMode = "seal" => \A i \in Inputs : turnOf[i] = L => st[i] # "accepted"

\* On a stop the bridge leaves its loop at once and looks up the receipt before agy answers the
\* cancel; the post waits for the turn's end. The seal waits for an accepted input's save.
EarlyLookup(L) ==
    /\ running /\ L \in Leaders /\ posts[L] = 0 /\ ~done[L] /\ cancelReq
    /\ L \in OpenTurns(open, turnOf) /\ looked[L] = "no" /\ LookupNow(L) /\ SealWaits(L)
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   budgetVars, steerFailed, noticed, orphan, clearedSent, settlerView, held,
                   marked>>

\* The writer posts the turn's recorded disposition once, after the seal and lookup when no stop
\* already did them; that post covers every member, and carries one notice line for each member
\* whose steer failed (the mutant leaves it out). A failed lookup keeps the row: its mark may or
\* may not be written.
Settle(L) ==
    /\ running /\ L \in Leaders /\ done[L] /\ posts[L] = 0
    /\ posts' = [i \in Inputs |-> IF turnOf[i] = L THEN posts[i] + 1 ELSE posts[i]]
    /\ st' = [i \in Inputs |-> IF turnOf[i] = L THEN "done" ELSE st[i]]
    /\ noticed' = [i \in Inputs |->
                     IF turnOf[i] = L /\ SteerMode # "error_silent" THEN steerFailed[i]
                     ELSE noticed[i]]
    /\ \/ /\ looked[L] = "no" /\ SealWaits(L) /\ LookupNow(L)
          /\ IF looked'[L] = "fail" THEN held' = L /\ marked' \in BOOLEAN
             ELSE UNCHANGED <<held, marked>>
       \/ /\ looked[L] = "ok" /\ UNCHANGED <<looked, receiptHas, readFailLeft, held, marked>>
       \/ /\ looked[L] = "fail" /\ held' = L /\ marked' \in BOOLEAN
          /\ UNCHANGED <<looked, receiptHas, readFailLeft>>
    /\ UNCHANGED <<running, proc, sessionVars, sentCount, turnOf, open, done, outcome, refusedIn,
                   cancelReq, cancelSent, zombie, wrongDone, badSend, served, budgetVars,
                   steerFailed, orphan, clearedSent, settlerView>>

\* The next admission records a kept row from its durable origins and frees the channel. An
\* unmarked row is marked again first (the mutant waits for a mark that never comes).
AdmitSettle ==
    /\ running /\ held # None /\ Queued # {} /\ (marked \/ MarkRetry = "retry")
    /\ receiptHas' = [i \in Inputs |->
                        IF turnOf[i] = held THEN receiptHas[i] \/ Durable(i) ELSE receiptHas[i]]
    /\ held' = None /\ marked' = FALSE
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   budgetVars, steerFailed, noticed, orphan, clearedSent, settlerView, looked,
                   readFailLeft>>

\* AgentDesk restart: memory is lost. The child has its own process group, so it may outlive
\* AgentDesk as an orphan until boot sweeps it. An input written ahead but not yet on the wire
\* is never written: it stays in its turn, which boot fails. An input accepted but not yet
\* saved was never recorded: it is still in the mailbox and comes again.
Crash ==
    /\ running /\ crashLeft > 0
    /\ running' = FALSE /\ crashLeft' = crashLeft - 1
    /\ proc' = IF proc = "up" THEN "dead" ELSE proc
    /\ attached' = NoGen /\ loading' = FALSE /\ open' = {} /\ zombie' = FALSE
    /\ cancelReq' = FALSE /\ cancelSent' = FALSE
    /\ orphan' = (proc = "up") /\ settlerView' = [i \in Inputs |-> FALSE]
    /\ looked' = [L \in Inputs |-> IF posts[L] = 0 THEN "no" ELSE looked[L]]
    /\ st' = [i \in Inputs |-> CASE st[i] = "saved" -> "unsent"
                                   [] st[i] = "accepted" -> "queued"
                                   [] OTHER -> st[i]]
    /\ turnOf' = [i \in Inputs |-> IF st[i] = "accepted" THEN None ELSE turnOf[i]]
    /\ UNCHANGED <<stored, nextGen, clearedGen, sentCount, done, outcome, posts,
                   refusedIn, wrongDone, badSend, served, dieLeft, stopLeft, clearLeft, refuseLeft,
                   steerFailLeft, steerFailed, noticed, clearedSent, receiptHas, held, marked,
                   readFailLeft>>

\* Boot sweeps the recorded child group of the previous AgentDesk before any new spawn.
SweepOrphan ==
    /\ running /\ orphan /\ orphan' = FALSE
    /\ UNCHANGED <<running, proc, sessionVars, st, sentCount, turnOf, open, done, outcome, posts,
                   refusedIn, cancelReq, cancelSent, zombie, wrongDone, badSend, served,
                   steerFailed, noticed, clearedSent, receiptVars, settleVars, budgetVars>>

\* Boot: a turn with written prompts and no terminal record is failed with a notice (the mutant
\* re-queues its inputs and writes them again). Boot settles from the durable row, so its view
\* holds every origin written ahead, and it records a kept row.
Boot ==
    /\ ~running /\ running' = TRUE
    /\ IF BootMode = "rerun_sent"
         THEN /\ st' = [i \in Inputs |-> IF turnOf[i] \in Active THEN "queued" ELSE st[i]]
              /\ turnOf' = [i \in Inputs |-> IF turnOf[i] \in Active THEN None ELSE turnOf[i]]
              /\ UNCHANGED <<done, outcome>>
         ELSE /\ done' = [L \in Inputs |-> done[L] \/ L \in Active]
              /\ outcome' = [L \in Inputs |-> IF L \in Active THEN "failure" ELSE outcome[L]]
              /\ UNCHANGED <<st, turnOf>>
    /\ settlerView' = [i \in Inputs |-> Durable(i)]
    /\ receiptHas' = [i \in Inputs |->
                        IF held # None /\ turnOf[i] = held THEN receiptHas[i] \/ Durable(i)
                        ELSE receiptHas[i]]
    /\ held' = None /\ marked' = FALSE
    /\ UNCHANGED <<proc, sessionVars, sentCount, open, posts, refusedIn, cancelReq, cancelSent,
                   zombie, wrongDone, badSend, served, budgetVars, steerFailed, noticed, orphan,
                   clearedSent, looked, readFailLeft>>

AllSettled == \A i \in Inputs : st[i] = "done"
Rest == AllSettled /\ UNCHANGED vars

Next ==
    \/ \E i \in Inputs : Arrive(i) \/ ReplayOld(i) \/ Settle(i) \/ StreamTick(i)
                        \/ EarlyLookup(i)
    \/ Spawn \/ NewSession \/ Reattach \/ ReattachRefused \/ LoadAnswered \/ AdmitSettle
    \/ Accept \/ Save \/ Wire \/ AgyResponds \/ SteerErrored \/ AgyDies \/ WatchdogKill \/ ZombieResponse
    \/ UserStop \/ SendCancel \/ Clear \/ CloseAfterTurn \/ Crash \/ SweepOrphan \/ Boot \/ Rest

\* agy itself is not assumed to answer (no fairness on AgyResponds or SteerErrored): only
\* AgentDesk's own steps, the watchdog included, are fair.
Fairness ==
    /\ WF_vars(Spawn) /\ WF_vars(NewSession) /\ WF_vars(Reattach) /\ WF_vars(LoadAnswered)
    /\ WF_vars(Accept) /\ WF_vars(Save) /\ WF_vars(Wire) /\ WF_vars(SendCancel) /\ WF_vars(WatchdogKill)
    /\ WF_vars(ZombieResponse) /\ WF_vars(Boot) /\ WF_vars(CloseAfterTurn)
    /\ WF_vars(SweepOrphan) /\ WF_vars(AdmitSettle)
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
\* An input written ahead always has a durable owner: the turn whose origin list holds it.
DurableOwner == \A i \in Inputs : InTurn(i) => turnOf[i] # None
\* An input that had been sent when /clear ran is never queued again.
NoReviveAfterClear == \A i \in Inputs : clearedSent[i] => st[i] # "queued"
\* Every input a post covered, the first and steered ones included, is named by that turn's
\* receipt, or its turn's row is still kept for the receipt.
NoMissingOrigin ==
    \A i \in Inputs : posts[i] >= 1 => receiptHas[i] \/ (turnOf[i] # None /\ held = turnOf[i])
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
    ~\E i \in Inputs : receiptHas[i] /\ ~settlerView[i] /\ turnOf[i] \notin {None, i}
                     /\ outcome[turnOf[i]] = "cancelled"
\* Witnesses: a restart between an input's write-ahead and its prompt, a steer accepted but not
\* written because its turn was stopping, and a kept row without its mark that an admission
\* waits on (the retry marks it).
NoCrashAfterSave == ~\E i \in Inputs : ~running /\ st[i] = "unsent" /\ ~steerFailed[i]
NoUnsentSteer == ~\E i \in Inputs : steerFailed[i] /\ sentCount[i] = 0 /\ posts[i] = 1
NoRetrySettle == ~(held # None /\ ~marked /\ Queued # {})
\* Liveness: every accepted input is eventually covered by one terminal post
\* (answer, failure notice or cancel notice), steered inputs included.
EveryInputSettles == \A i \in Inputs : (st[i] # "absent") ~> (posts[i] >= 1)
=============================================================================
