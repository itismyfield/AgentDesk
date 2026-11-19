---- MODULE CustodyChildResume ----
\* One foreign custody kept for a TUI destination, resumed after one restart. held is a thread
\* child released with no Legacy run (its body Legacy must not post), allowed a late child whose
\* body Legacy may post, plain no thread child. old is a custody written before the attribution.
\* Mut "before": an old custody read as no child and the child guard ahead of the receipted
\* transport; "selfAsPlain": a self hint read as no child.
EXTENDS Naturals
CONSTANTS Mut
VARIABLES kind, shape, hint, readable, sent, restarted, placed, settled, posts
vars == <<kind, shape, hint, readable, sent, restarted, placed, settled, posts>>
\* hint is the row's logical channel: a parent, or the channel itself, which a plain channel and a
\* child whose parent read failed at intake both leave. readable: the relationship read succeeds.
Init == /\ kind \in {"held", "allowed", "plain"} /\ shape \in {"new", "old"}
        /\ hint \in {"parent", "self"} /\ (kind = "plain" => hint = "self") /\ readable \in BOOLEAN
        /\ sent = FALSE /\ restarted = FALSE /\ placed = FALSE /\ settled = FALSE /\ posts = 0
\* Before the restart Legacy may post an allowed body; every chunk's receipt is then kept.
SendBefore == /\ ~restarted /\ kind # "held" /\ ~sent
              /\ sent' = TRUE /\ posts' = posts + 1
              /\ UNCHANGED <<kind, shape, hint, readable, restarted, placed, settled>>
\* The restart lands after the last receipt and before the settlement; the boot places no child.
Restart == /\ ~restarted /\ restarted' = TRUE /\ placed' = FALSE
           /\ UNCHANGED <<kind, shape, hint, readable, sent, settled, posts>>
\* A later boot may place the child with a decided body; nothing guarantees it.
Place == /\ restarted /\ ~placed /\ kind # "plain" /\ placed' = TRUE
         /\ UNCHANGED <<kind, shape, hint, readable, sent, restarted, settled, posts>>
\* The stored attribution. An old custody is a child by a parent hint, else by a relationship read;
\* with neither it stays kept. selfAsPlain reads a self hint as no child.
Child == CASE shape = "new" -> kind # "plain"
           [] Mut \in {"oldAsFalse", "before"} -> FALSE
           [] hint = "parent" -> TRUE
           [] Mut = "selfAsPlain" -> FALSE
           [] readable -> kind # "plain"
           [] OTHER -> TRUE
Guarded == Child /\ ~placed
\* One resume: a receipted transport settles; an unplaced child's unsent body waits; a placed held
\* child is O's; anything else, an unplaced child read as no child too, is posted by Legacy.
Resume == /\ restarted /\ ~settled
          /\ IF sent /\ ~(Mut \in {"guardFirst", "before"} /\ Guarded)
                THEN settled' = TRUE /\ UNCHANGED <<posts, sent>>
             ELSE IF Guarded
                THEN UNCHANGED <<settled, posts, sent>>
             ELSE IF kind = "held" /\ placed
                THEN settled' = TRUE /\ UNCHANGED <<posts, sent>>
             ELSE settled' = TRUE /\ sent' = TRUE /\ posts' = posts + 1
          /\ UNCHANGED <<kind, shape, hint, readable, restarted, placed>>
Next == SendBefore \/ Restart \/ Place \/ Resume
Spec == Init /\ [][Next]_vars /\ WF_vars(Restart) /\ WF_vars(Resume)
\* Legacy never posts a body whose child holds no Legacy run.
NoHeldLegacyPost == kind = "held" => posts = 0
AtMostOnce == posts <= 1
\* A transport whose every chunk is receipted reaches its settlement without a placement.
SentSettles == [](sent => <>settled)
====
