---- MODULE ReasonedClosure ----
\* One channel after a refusal. Rollout a holds w, x, y in that order; rollout b, bound in the same
\* tmux generation when multi, holds z at x's very byte offsets. x is the resumed turn whose body a
\* consumer discards with one reason; y is the next input's turn; w is an earlier turn no record covers.
EXTENDS Naturals
CONSTANTS Fix, Mut
VARIABLES kind, lateOn, multi, wst, xopen, adopt, xb, reason, closed, gen, restarted,
          yon, start, skipx, op, lp, rp, alarm, wr, tailIn, xt
vars == <<kind, lateOn, multi, wst, xopen, adopt, xb, reason, closed, gen, restarted,
          yon, start, skipx, op, lp, rp, alarm, wr, tailIn, xt>>
Turns == {"w", "x", "y", "z"}
\* A member's Deferred body is held; a late thread child's is Legacy's, counted and in turn order,
\* until its init.
Init == /\ kind \in {"member", "late"}
        /\ lateOn \in BOOLEAN /\ multi \in BOOLEAN /\ wst \in {"none", "unowned"} /\ xopen \in BOOLEAN
        /\ (kind = "late" => (~multi /\ wst = "none" /\ ~xopen))
        /\ (kind = "member" => lateOn)
        /\ adopt = (IF kind = "member" THEN "refused" ELSE "deferred")
        /\ xb = "held" /\ reason = FALSE /\ closed = FALSE /\ gen = 0 /\ restarted = FALSE
        /\ yon = FALSE /\ start = "none" /\ skipx = FALSE
        /\ op = [t \in Turns |-> 0] /\ lp = [t \in Turns |-> 0] /\ rp = 0 /\ alarm = FALSE
        /\ wr = FALSE /\ tailIn = FALSE /\ xt = FALSE
\* The reason intent and the closure are one durable write; twoStep writes the closure after it.
PersistReason == /\ kind = "member" /\ adopt = "refused" /\ xb = "held" /\ ~reason
                 /\ reason' = TRUE /\ closed' = (Fix # "twoStep" /\ Mut # "noClosure")
                 /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, adopt, xb, gen, restarted,
                                yon, start, skipx, op, lp, rp, alarm, wr, tailIn, xt>>
PersistClosure == /\ Fix = "twoStep" /\ reason /\ ~closed /\ xb = "held" /\ adopt # "committed"
                  /\ closed' = TRUE
                  /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, adopt, xb, reason, gen, restarted,
                                 yon, start, skipx, op, lp, rp, alarm, wr, tailIn, xt>>
\* Nothing of the body is released before its reason is durable.
Release == /\ reason /\ xb = "held" /\ (Fix = "twoStep" => closed) /\ adopt # "committed"
           /\ xb' = "discarded"
           /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, adopt, reason, closed, gen, restarted,
                          yon, start, skipx, op, lp, rp, alarm, wr, tailIn, xt>>
\* The durable intent drives its one post on its own, after a restart too.
ReasonPost == /\ reason /\ rp = 0 /\ rp' = 1
              /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, adopt, xb, reason, closed, gen, restarted,
                             yon, start, skipx, op, lp, alarm, wr, tailIn, xt>>
\* One restart before any store exists: the channel is seeded again and waits; a cold launch may
\* move the tmux generation. Rows, reasons and closures are durable.
Restart == /\ ~restarted /\ adopt \in {"refused", "deferred"}
           /\ restarted' = TRUE /\ adopt' = "deferred" /\ gen' \in {gen, 1}
           /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, xb, reason, closed,
                          yon, start, skipx, op, lp, rp, alarm, wr, tailIn, xt>>
\* x's native end record may never come; the next input's opener ends an open x.
XEnds == /\ xopen /\ xopen' = FALSE
         /\ UNCHANGED <<kind, lateOn, multi, wst, adopt, xb, reason, closed, gen, restarted,
                        yon, start, skipx, op, lp, rp, alarm, wr, tailIn, xt>>
NewInput == /\ ~yon /\ adopt \in {"deferred", "committed"}
            /\ yon' = TRUE /\ xopen' = FALSE
            /\ UNCHANGED <<kind, lateOn, multi, wst, adopt, xb, reason, closed, gen, restarted,
                           start, skipx, op, lp, rp, alarm, wr, tailIn, xt>>
\* How the takeover reads x's closure. frontier is the r1 choice: a Legacy delivered frontier, which
\* counts only for the sole rollout of the current generation and only for an ended turn.
GenScoped == Fix = "frontier" \/ Mut = "genOnly"
EndNeeded == Fix = "frontier" \/ Mut = "openIgnored"
XClosed == closed /\ (GenScoped => (~multi /\ gen = 0)) /\ (EndNeeded => ~xopen)
XDisposed == XClosed \/ lp["x"] = 1
After == IF lp["y"] = 1 THEN "end" ELSE "y"
\* One start per rollout. after: never at or before a closed turn, an uncovered turn before it is
\* left out with an alarm. first/wait/oskip are the other answers to that straddle.
Commit == /\ adopt = "deferred" /\ (kind = "late" => lateOn)
          /\ LET straddle == wst = "unowned" /\ XClosed
                 first == IF wst = "unowned" THEN "w" ELSE IF XDisposed THEN After ELSE "x"
                 leaves == straddle /\ Fix \notin {"first", "oskip"}
                 inX == XClosed /\ xopen /\ ~yon
             IN  /\ (Fix = "wait" => ~straddle)
                 /\ (Mut # "eofInit" => ~inX)
                 /\ start' = (IF leaves THEN After ELSE first)
                 /\ skipx' = (straddle /\ Fix = "oskip")
                 /\ alarm' = (leaves /\ Mut # "leaveSilent")
                 /\ wr' = (leaves /\ Fix = "closeW" /\ Mut # "noReason")
                 /\ tailIn' = inX
          /\ adopt' = "committed"
          /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, xb, reason, closed, gen, restarted,
                         yon, op, lp, rp, xt>>
\* x writes more of its open turn; an init at an EOF inside x would read that tail as owed.
AppendXTail == /\ xopen /\ ~yon /\ ~xt /\ xt' = TRUE
               /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, adopt, xb, reason, closed, gen,
                              restarted, yon, start, skipx, op, lp, rp, alarm, wr, tailIn>>
OPostTail == /\ adopt = "committed" /\ tailIn /\ xt /\ op["x"] = 0
             /\ op' = [op EXCEPT !["x"] = 1]
             /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, adopt, xb, reason, closed, gen,
                            restarted, yon, start, skipx, lp, rp, alarm, wr, tailIn, xt>>
Rank(t) == CASE t = "w" -> 0 [] t = "x" -> 1 [] t = "y" -> 2 [] OTHER -> 0
StartRank == CASE start = "w" -> 0 [] start = "x" -> 1 [] start = "y" -> 2 [] OTHER -> 3
Exists(t) == CASE t = "w" -> wst = "unowned" [] t = "x" -> TRUE [] t = "y" -> yon [] OTHER -> multi
\* O posts every turn at or after its start once; b starts at z, whose closure it never holds.
OPost(t) == /\ adopt = "committed" /\ Exists(t) /\ op[t] = 0
            /\ (t = "z" => ~(Mut = "anySource" /\ closed))
            /\ (t # "z" => Rank(t) >= StartRank)
            /\ ~(t = "x" /\ skipx)
            /\ ~(t = "y" /\ Mut = "closeNext" /\ closed)
            /\ op' = [op EXCEPT ![t] = 1]
            /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, adopt, xb, reason, closed, gen, restarted,
                           yon, start, skipx, lp, rp, alarm, wr, tailIn, xt>>
LegacyOwns == (kind = "late" /\ Mut # "lateHeld") \/ (kind = "member" /\ Mut = "memberLegacy")
LegacySend(t) == /\ adopt = "deferred" /\ LegacyOwns /\ Exists(t) /\ lp[t] = 0 /\ op[t] = 0
                 /\ (t = "x" => ~reason)
                 /\ ((t = "y" /\ kind = "late") => lp["x"] = 1)
                 /\ lp' = [lp EXCEPT ![t] = 1]
                 /\ UNCHANGED <<kind, lateOn, multi, wst, xopen, adopt, xb, reason, closed, gen, restarted,
                                yon, start, skipx, op, rp, alarm, wr, tailIn, xt>>
Next == PersistReason \/ PersistClosure \/ Release \/ ReasonPost \/ Restart \/ XEnds \/ NewInput
        \/ Commit \/ AppendXTail \/ OPostTail \/ (\E t \in Turns : OPost(t)) \/ (\E t \in {"x", "y"} : LegacySend(t))
Fair == /\ WF_vars(PersistReason) /\ WF_vars(PersistClosure) /\ WF_vars(Release) /\ WF_vars(ReasonPost)
        /\ WF_vars(Commit) /\ (\A t \in Turns : WF_vars(OPost(t)))
        /\ (\A t \in {"x", "y"} : WF_vars(LegacySend(t)))
Spec == Init /\ [][Next]_vars /\ Fair
\* A body whose episode holds a reason is never posted, by O or by Legacy.
ReasonedNeverPosted == reason => (op["x"] = 0 /\ lp["x"] = 0)
AtMostOnce == (\A t \in Turns : op[t] + lp[t] <= 1) /\ rp <= 1
NoLegacyOnMember == kind = "member" => \A t \in Turns : lp[t] = 0
LeftOutVisible == (adopt = "committed" /\ wst = "unowned" /\ start # "w") => alarm
\* The next input's turn is posted exactly once (with AtMostOnce).
NewInputPosted == [](yon => <>(op["y"] + lp["y"] = 1))
\* A late child's resumed response in Deferred is posted once, by Legacy or by its own init.
DeferredResumeSettles == (kind = "late") => <>(op["x"] + lp["x"] = 1)
OtherSourcePosted == []((adopt = "committed" /\ multi) => <>(op["z"] = 1))
\* Not a rule of the design: what leaving the uncovered turn out costs, and what oskip keeps.
EarlierPosted == []((adopt = "committed" /\ wst = "unowned") => <>(op["w"] = 1))
\* An uncovered earlier turn is never started past silently: the init starts at it or it holds its
\* own reason.
EarlierAccounted == (adopt = "committed" /\ wst = "unowned") => (start = "w" \/ wr)
\* The hold answer: an uncovered earlier turn is never started past.
EarlierKept == (adopt = "committed" /\ wst = "unowned") => start = "w"
\* The next input's turn is posted once unless an uncovered turn precedes a closed one.
NewInputPostedOffStraddle == [](yon /\ ~(wst = "unowned" /\ closed) => <>(op["y"] + lp["y"] = 1))
====
