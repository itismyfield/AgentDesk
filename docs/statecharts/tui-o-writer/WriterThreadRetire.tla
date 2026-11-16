------------------------- MODULE WriterThreadRetire -------------------------
EXTENDS Naturals
\* Fix: "before" (a GC success reaches no lifecycle owner) | "after" (child store retirement).
CONSTANTS Fix, Mut
VARIABLES proc,       \* "up" | "down"
          row,        \* the thread's session row: "live" | "deleted"
          life,       \* in-memory child lifecycle: "active" | "moving" | "retired"
          gen,        \* child lifecycle generation; a superseding input raises it
          epoch,      \* inputs this child has seen; any input stales an older ticket
          markGen, markRetired,   \* durable lifecycle mark; markGen = 0 is no mark
          req,        \* one retirement request, from the GC judgement to its outcome
          tGen, tEpoch,           \* what the request observed before the row delete
          writer,     \* this child's managed writer: "none" | "ready"
          wlife,      \* lifecycle generation the writer was started under
          oldw,       \* a stopped-but-unjoined writer that may still finish a POST
          owed,       \* captured child output the writer has not posted
          works,
          input,      \* the thread's newest turn: "none" | "waiting" | "posted"
          arrived, posts,
          stale, lateMark, debtDropped, badReq,   \* history of a broken rule
          gcs, crashes
mem == <<life, gen, epoch>>
mark == <<markGen, markRetired>>
tick == <<req, tGen, tEpoch>>
wr == <<writer, wlife, oldw>>
out == <<owed, works, input, arrived, posts>>
hist == <<stale, lateMark, debtDropped, badReq>>
count == <<gcs, crashes>>
vars == <<proc, row, mem, mark, tick, wr, out, hist, count>>

Terminal == {"none", "retired", "rejected", "failed", "lost"}

Init ==
    /\ proc = "up" /\ row = "live" /\ life = "active" /\ gen = 1 /\ epoch = 0
    /\ markGen = 0 /\ markRetired = FALSE /\ req = "none" /\ tGen = 0 /\ tEpoch = 0
    /\ writer = "none" /\ wlife = 0 /\ oldw = FALSE /\ owed = FALSE /\ works = 0
    /\ input = "none" /\ arrived = 0 /\ posts = 0
    /\ stale = FALSE /\ lateMark = FALSE /\ debtDropped = FALSE /\ badReq = FALSE
    /\ gcs = 0 /\ crashes = 0

\* Every producer reaches the one placement boundary; the input makes a fresh session row.
Arrive ==
    /\ proc = "up" /\ input # "waiting" /\ arrived < 2
    /\ input' = "waiting" /\ arrived' = arrived + 1 /\ posts' = 0
    /\ epoch' = epoch + 1 /\ row' = "live"
    /\ UNCHANGED <<proc, life, gen, mark, tick, wr, owed, works, hist, count>>

\* The next same-thread input replaces the retirement mark with a new active generation.
Supersede ==
    /\ proc = "up" /\ Mut # "noSupersede" /\ life = "retired" /\ input = "waiting"
    /\ life' = "active" /\ gen' = gen + 1 /\ markGen' = gen + 1 /\ markRetired' = FALSE
    /\ UNCHANGED <<proc, row, epoch, tick, wr, out, hist, count>>

\* The existing 2 s refresh hosts only an active child.
StartWriter ==
    /\ proc = "up" /\ life = "active" /\ writer = "none"
    /\ writer' = "ready" /\ wlife' = gen
    /\ UNCHANGED <<proc, row, mem, mark, tick, oldw, out, hist, count>>
Work ==
    /\ proc = "up" /\ row = "live" /\ writer = "ready" /\ ~owed /\ works < 1
    /\ owed' = TRUE /\ works' = works + 1
    /\ UNCHANGED <<proc, row, mem, mark, tick, wr, input, arrived, posts, hist, count>>
Flush ==
    /\ proc = "up" /\ writer = "ready" /\ owed /\ owed' = FALSE
    /\ stale' = (stale \/ life = "retired" \/ wlife # gen)
    /\ UNCHANGED <<proc, row, mem, mark, tick, wr, works, input, arrived, posts,
                   lateMark, debtDropped, badReq, count>>
\* Intake admits a child input only to its ready writer outside a lifecycle transition.
Post ==
    /\ proc = "up" /\ input = "waiting" /\ writer = "ready" /\ life = "active"
    /\ input' = "posted" /\ posts' = posts + 1 /\ stale' = (stale \/ wlife # gen)
    /\ UNCHANGED <<proc, row, mem, mark, tick, wr, owed, works, arrived,
                   lateMark, debtDropped, badReq, count>>
OldPost ==
    /\ proc = "up" /\ oldw /\ input = "waiting"
    /\ oldw' = FALSE /\ posts' = posts + 1 /\ stale' = TRUE
    /\ UNCHANGED <<proc, row, mem, mark, tick, writer, wlife, owed, works, input, arrived,
                   lateMark, debtDropped, badReq, count>>
OldEnd ==
    /\ proc = "up" /\ oldw /\ oldw' = FALSE
    /\ UNCHANGED <<proc, row, mem, mark, tick, writer, wlife, out, hist, count>>

\* thread_gc judges an idle row and pins the child's generation and input epoch first.
Observe ==
    /\ proc = "up" /\ req \in Terminal /\ gcs < 2 /\ row = "live" /\ input # "waiting"
    /\ life # "moving"
    /\ req' = "observed" /\ tGen' = gen /\ tEpoch' = epoch /\ gcs' = gcs + 1
    /\ UNCHANGED <<proc, row, mem, mark, wr, out, hist, crashes>>
\* Only an exact row delete makes a request; a refused delete makes none.
GcDelete ==
    /\ proc = "up" /\ req = "observed"
    /\ \/ /\ epoch = tEpoch /\ row = "live"
          /\ row' = "deleted" /\ req' = "requested" /\ UNCHANGED badReq
       \/ /\ req' = IF Mut = "noDelete" THEN "requested" ELSE "none"
          /\ badReq' = (badReq \/ Mut = "noDelete") /\ UNCHANGED row
    /\ UNCHANGED <<proc, mem, mark, tGen, tEpoch, wr, out, stale, lateMark, debtDropped,
                   count>>
\* The owner takes the child's transition only for the generation and epoch the request saw.
Begin ==
    /\ Fix = "after" /\ proc = "up" /\ req = "requested"
    /\ IF Mut = "noCas" \/ (life = "active" /\ gen = tGen /\ epoch = tEpoch)
       THEN life' = "moving" /\ req' = "begun"
       ELSE req' = "rejected" /\ UNCHANGED life
    /\ UNCHANGED <<proc, row, gen, epoch, mark, tGen, tEpoch, wr, out, hist, count>>
\* A fresh owed read from a running writer; no writer is no proof of zero debt.
Debt ==
    /\ proc = "up" /\ req = "begun"
    /\ IF writer = "ready" /\ (~owed \/ Mut = "noDebt")
       THEN req' = "clear" /\ UNCHANGED life
       ELSE req' = "failed" /\ life' = "active"
    /\ UNCHANGED <<proc, row, gen, epoch, mark, tGen, tEpoch, wr, out, hist, count>>
\* Stop and join the exact writer; skipping the join leaves it able to finish a POST.
Stop ==
    /\ proc = "up" /\ req = "clear"
    /\ req' = "stopped" /\ writer' = "none" /\ oldw' = (Mut = "noStop")
    /\ UNCHANGED <<proc, row, mem, mark, tGen, tEpoch, wlife, out, hist, count>>
StopFail ==
    /\ proc = "up" /\ req = "clear"
    /\ req' = "failed" /\ life' = "active" /\ writer' = "none"
    /\ UNCHANGED <<proc, row, gen, epoch, mark, tGen, tEpoch, wlife, oldw, out, hist, count>>
\* The mark is written outside the entry lock, so the epoch is compared again around it.
Write ==
    /\ proc = "up" /\ req = "stopped"
    /\ IF Mut = "noCas" \/ epoch = tEpoch
       THEN \/ /\ req' = "written" /\ markGen' = gen /\ markRetired' = (Mut # "noMark")
               /\ lateMark' = (lateMark \/ epoch # tEpoch)
               /\ debtDropped' = (debtDropped \/ owed) /\ UNCHANGED life
            \/ /\ req' = "failed" /\ life' = "active"
               /\ UNCHANGED <<mark, lateMark, debtDropped>>
       ELSE req' = "rejected" /\ life' = "active" /\ UNCHANGED <<mark, lateMark, debtDropped>>
    /\ UNCHANGED <<proc, row, gen, epoch, tGen, tEpoch, wr, out, stale, badReq, count>>
Confirm ==
    /\ proc = "up" /\ req = "written"
    /\ IF Mut = "noCas" \/ epoch = tEpoch
       THEN life' = "retired" /\ req' = "retired" /\ UNCHANGED <<gen, mark>>
       ELSE /\ life' = "active" /\ req' = "rejected"
            /\ gen' = gen + 1 /\ markGen' = gen + 1 /\ markRetired' = FALSE
    /\ UNCHANGED <<proc, row, epoch, tGen, tEpoch, wr, out, hist, count>>

\* A crash loses the request and every in-memory fact; the store keeps the mark and the debt.
Crash ==
    /\ proc = "up" /\ crashes < 1
    /\ proc' = "down" /\ crashes' = crashes + 1 /\ writer' = "none" /\ oldw' = FALSE
    /\ req' = IF req \in Terminal THEN req ELSE "lost"
    /\ UNCHANGED <<row, mem, mark, tGen, tEpoch, wlife, out, hist, gcs>>
Boot ==
    /\ proc = "down" /\ proc' = "up"
    /\ life' = IF markRetired /\ Mut # "noRehydrate" THEN "retired" ELSE "active"
    /\ gen' = IF markGen = 0 THEN 1 ELSE markGen
    /\ UNCHANGED <<row, epoch, mark, tick, wr, out, hist, count>>

Next == Arrive \/ Supersede \/ StartWriter \/ Work \/ Flush \/ Post \/ OldPost \/ OldEnd
        \/ Observe \/ GcDelete \/ Begin \/ Debt \/ Stop \/ StopFail \/ Write \/ Confirm
        \/ Crash \/ Boot \/ UNCHANGED vars
Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Supersede) /\ WF_vars(StartWriter) /\ WF_vars(Post) /\ WF_vars(OldEnd)
        /\ WF_vars(GcDelete) /\ WF_vars(Begin) /\ WF_vars(Debt) /\ WF_vars(Stop)
        /\ WF_vars(Write) /\ WF_vars(Confirm) /\ WF_vars(Boot)

ExactlyOnce == posts <= 1 /\ (input = "posted" => posts = 1)
NoStalePost == ~stale
RetiredNeedsMark == life = "retired" => (markRetired /\ markGen = gen)
MarkHonored == (proc = "up" /\ markRetired /\ req # "written") => life = "retired"
NoLateMark == ~lateMark
DebtKept == ~debtDropped
RequestAfterDelete == ~badReq
\* A request ends in a confirmed retirement or a visible refusal; a crash loses it.
RetireResolves == [](req = "requested" => <>(req \in Terminal))
\* An input after retirement is posted by the new generation's writer.
InputAnswered == [](input = "waiting" => <>(input = "posted"))
=============================================================================
