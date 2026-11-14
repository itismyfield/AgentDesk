--------------------------- MODULE CodexLateDelivery ---------------------------
(* Codex late takeover start point. Custody (Legacy's local hold) and delivery *)
(* (what Legacy posted) are independent. ev[s][i] is the delivery evidence an *)
(* evidence reader may meet for turn i of source s:                            *)
(*   "cur"   a record of the current tmux generation, dated inside s's bind    *)
(*           span: the delivery of that turn (it implies dlv);                 *)
(*   "bogus" a record whose bytes align to the turn but that belongs to an     *)
(*           older generation or another source of the same generation.        *)
(*   "hist"  a Historical receipt of an earlier source, dated inside this      *)
(*           source's bind span: time alone names this source.                 *)
(*   "edit"  a later source's short answer edited into its placeholder: the   *)
(*           receipt keeps the placeholder's id, dated before its own bind,    *)
(*           at bytes where this (first) source's turn ends.                   *)
(* early2[s][i]: the record is dated before the generation's second source.    *)
(* P2 ships in the same PR, so after boot no TUI body is posted except where  *)
(* the environment action Evidence stands for a delivery before the gate.     *)
(*                                                                            *)
(* Rule "r3"  custody absence and turn time as evidence, previous source only. *)
(*      "d1"  first design: any aligned record, bind-time filter kept, walk    *)
(*            stops at the newest evidenced source, local unresolved set.      *)
(*      "r4"  exclude only turns at or below the current-generation floor of   *)
(*            s, records placed by date, no bind-time filter, walk every       *)
(*            source of the generation and the unresolved set; the current     *)
(*            source is remembered only on its own Retry.                      *)
(*      "r5"  r4, but a record counts only while the generation bound one      *)
(*            source, or for the first source when dated before the second;    *)
(*            a current source judged owed on the warm path is remembered.     *)
(*      "r6"  a record counts only while the generation bound one source; the  *)
(*            current source is remembered on the fresh path too.              *)
(* Mut  "genIgnore" r6 trusting bogus records, "prebindFilter" r6 with the    *)
(*      bind-time filter back.                                                 *)
EXTENDS Naturals, FiniteSets

CONSTANTS Rule, Mut, Scenario

S == 1..3
T == 1..2
Cus == {"none", "row", "pending"}
Ev == {"none", "cur", "bogus", "hist", "edit"}

VARIABLES nb, nt, ended, early, dlv, ev, early2, prebind, ingen, cus, readable, unres, done, start,
          blocked, judged

vars == <<nb, nt, ended, early, dlv, ev, early2, prebind, ingen, cus, readable, unres, done, start,
          blocked, judged>>

Exists(s, i) == s <= nb /\ i <= nt[s]
NoTurnF == [i \in T |-> FALSE]
NoCusF == [i \in T |-> "none"]
NoEvF == [i \in T |-> "none"]
MaxOf(X) == IF X = {} THEN 0 ELSE CHOOSE x \in X : \A y \in X : y <= x
MinOf(X, d) == IF X = {} THEN d ELSE CHOOSE x \in X : \A y \in X : x <= y

TypeOK ==
    /\ nb \in S
    /\ nt \in [S -> 0..2]
    /\ ended \in [S -> [T -> BOOLEAN]]
    /\ early \in [S -> [T -> BOOLEAN]]
    /\ dlv \in [S -> [T -> BOOLEAN]]
    /\ ev \in [S -> [T -> Ev]]
    /\ early2 \in [S -> [T -> BOOLEAN]]
    /\ prebind \in [S -> [T -> BOOLEAN]]
    /\ ingen \in [S -> BOOLEAN]
    /\ cus \in [S -> [T -> Cus]]
    /\ readable \in [S -> BOOLEAN]
    /\ unres \subseteq S
    /\ done \in BOOLEAN
    /\ start \in [S -> 0..3]
    /\ blocked \in BOOLEAN
    /\ judged \subseteq S

(* One source's pre-boot history. A genuine record names a posted, ended turn; *)
(* a turn below it may be unposted (order inversion), one above it may be      *)
(* posted with no record; a bogus record says nothing about the turn.          *)
SrcOk(r, isLast) ==
    \A i \in T :
        /\ i > r.c => (~r.e[i] /\ ~r.d[i] /\ r.v[i] = "none" /\ ~r.p[i])
        /\ (i <= r.c /\ (~isLast \/ i < r.c)) => r.e[i]
        /\ r.v[i] = "cur" => (r.e[i] /\ r.d[i])
        /\ r.v[i] \notin {"hist", "edit"}
        /\ r.w[i] => r.v[i] = "cur"
        /\ ~r.e[i] => (~r.d[i] /\ r.v[i] = "none")

SrcOpts(isLast) ==
    {r \in [c : 0..2, e : [T -> BOOLEAN], d : [T -> BOOLEAN], v : [T -> Ev], p : [T -> BOOLEAN],
             w : [T -> BOOLEAN]] :
        SrcOk(r, isLast)}

Empty == [c |-> 0, e |-> NoTurnF, d |-> NoTurnF, v |-> NoEvF, p |-> NoTurnF, w |-> NoTurnF]

AllInit ==
    \E n \in 1..2 :
    \E r1 \in SrcOpts(n = 1) :
    \E r2 \in (IF n = 2 THEN {r \in SrcOpts(TRUE) : r.w = NoTurnF} ELSE {Empty}) :
    \E oldA \in (IF n = 2 THEN BOOLEAN ELSE {FALSE}) :
        LET R == [s \in S |-> IF s = 1 THEN r1 ELSE IF s = 2 THEN r2 ELSE Empty]
        IN /\ nb = n
           /\ nt = [s \in S |-> R[s].c]
           /\ ended = [s \in S |-> R[s].e]
           /\ dlv = [s \in S |-> R[s].d]
           /\ ev = [s \in S |-> R[s].v]
           /\ early2 = [s \in S |-> R[s].w]
           /\ prebind = [s \in S |-> R[s].p]
           /\ ingen = [s \in S |-> ~(s = 1 /\ oldA)]
           /\ cus = [s \in S |-> NoCusF]
           /\ readable = [s \in S |-> TRUE]

OneTurn(evA, preA, cusA) ==
    /\ nb = 1
    /\ nt = [s \in S |-> IF s = 1 THEN 1 ELSE 0]
    /\ ended = [s \in S |-> [i \in T |-> s = 1 /\ i = 1]]
    /\ dlv = [s \in S |-> NoTurnF]
    /\ ev = [s \in S |-> [i \in T |-> IF s = 1 /\ i = 1 THEN evA ELSE "none"]]
    /\ early2 = [s \in S |-> NoTurnF]
    /\ prebind = [s \in S |-> [i \in T |-> s = 1 /\ i = 1 /\ preA]]
    /\ ingen = [s \in S |-> TRUE]
    /\ cus = [s \in S |-> [i \in T |-> IF s = 1 /\ i = 1 THEN cusA ELSE "none"]]

RowPendingInit ==
    /\ nb = 1
    /\ nt = [s \in S |-> IF s = 1 THEN 2 ELSE 0]
    /\ ended = [s \in S |-> [i \in T |-> s = 1]]
    /\ dlv = [s \in S |-> NoTurnF]
    /\ ev = [s \in S |-> NoEvF]
    /\ early2 = [s \in S |-> NoTurnF]
    /\ prebind = [s \in S |-> NoTurnF]
    /\ ingen = [s \in S |-> TRUE]
    /\ cus = [s \in S |-> IF s = 1 THEN [i \in T |-> IF i = 1 THEN "row" ELSE "pending"]
                                   ELSE NoCusF]

(* A and B each hold one ended unposted turn; B is the current source. *)
MovedInit ==
    /\ nb = 2
    /\ nt = [s \in S |-> IF s <= 2 THEN 1 ELSE 0]
    /\ ended = [s \in S |-> [i \in T |-> s <= 2 /\ i = 1]]
    /\ dlv = [s \in S |-> NoTurnF]
    /\ ev = [s \in S |-> NoEvF]
    /\ early2 = [s \in S |-> NoTurnF]
    /\ prebind = [s \in S |-> NoTurnF]
    /\ ingen = [s \in S |-> TRUE]
    /\ cus = [s \in S |-> NoCusF]

(* Trajectory genfresh: A holds only its prompt (an open turn) when judged. *)
FreshInit ==
    /\ nb = 1
    /\ nt = [s \in S |-> IF s = 1 THEN 1 ELSE 0]
    /\ ended = [s \in S |-> NoTurnF]
    /\ dlv = [s \in S |-> NoTurnF]
    /\ ev = [s \in S |-> NoEvF]
    /\ early2 = [s \in S |-> NoTurnF]
    /\ prebind = [s \in S |-> NoTurnF]
    /\ ingen = [s \in S |-> TRUE]
    /\ cus = [s \in S |-> NoCusF]

Init ==
    /\ CASE Scenario = "free" -> OneTurn("none", FALSE, "none")
         [] Scenario = "rowpending" -> RowPendingInit
         [] Scenario = "rotate" -> OneTurn("none", FALSE, "pending")
         [] Scenario = "gen" -> OneTurn("bogus", FALSE, "none")
         [] Scenario = "pending" -> OneTurn("none", TRUE, "none")
         [] Scenario = "moved" -> MovedInit
         [] Scenario = "hist" -> OneTurn("none", FALSE, "none")
         [] Scenario = "genmove" -> OneTurn("none", FALSE, "none")
         [] Scenario = "edit" -> OneTurn("none", FALSE, "none")
         [] Scenario = "genfresh" -> FreshInit
         [] OTHER -> AllInit
    \* every pre-boot turn that ended did so before the baseline sampled after boot
    /\ early = ended
    /\ readable \in CASE Scenario = "rotate" -> {[s \in S |-> s > 1 \/ b] : b \in BOOLEAN}
                      [] Scenario = "moved" -> {[s \in S |-> IF s = 1 THEN a ELSE IF s = 2 THEN b ELSE TRUE] :
                                                 a \in BOOLEAN, b \in BOOLEAN}
                      [] Scenario = "all" -> {readable}
                      [] OTHER -> {[s \in S |-> TRUE]}
    /\ unres = {}
    /\ done = FALSE
    /\ start = [s \in S |-> 0]
    \* custody holding the commit: the genmove trajectory starts held
    /\ blocked = (Scenario \in {"genmove", "genfresh"})
    /\ judged = {}

(* ---------------------------- environment ---------------------------------- *)
EnvOK == ~done
InGen == {s \in 1..nb : ingen[s]}
FirstInGen == MinOf(InGen, 1)

Bind ==
    /\ EnvOK /\ nb < 3
    /\ Scenario \in {"rotate", "moved", "all", "hist", "genmove", "edit", "genfresh"}
    /\ nb' = nb + 1
    /\ ingen' = [ingen EXCEPT ![nb + 1] = TRUE]
    /\ UNCHANGED <<nt, ended, early, dlv, ev, early2, prebind, cus, readable, unres, done, start,
                   blocked, judged>>

NewTurn ==
    /\ EnvOK
    /\ Scenario \notin {"moved", "gen", "pending", "genmove", "edit", "genfresh"}
    /\ nt[nb] < 2
    /\ IF nt[nb] = 0 THEN TRUE ELSE ended[nb][nt[nb]]
    /\ Scenario \in {"rotate", "hist"} => (nb = 2 /\ nt[nb] = 0)
    /\ nt' = [nt EXCEPT ![nb] = @ + 1]
    /\ UNCHANGED <<nb, ended, early, dlv, ev, early2, prebind, ingen, cus, readable, unres, done,
                   start, blocked, judged>>

EndTurn ==
    /\ EnvOK
    /\ Scenario \in {"free", "rowpending", "all", "hist", "genfresh"}
    /\ IF nt[nb] = 0 THEN FALSE ELSE ~ended[nb][nt[nb]]
    /\ ended' = [ended EXCEPT ![nb][nt[nb]] = TRUE]
    \* only r3 reads early; the other scenarios end their turns before the baseline
    /\ early' \in {[early EXCEPT ![nb][nt[nb]] = b] :
                     b \in IF Scenario \in {"free", "rowpending"} THEN BOOLEAN ELSE {TRUE}}
    /\ UNCHANGED <<nb, nt, dlv, ev, early2, prebind, ingen, cus, readable, unres, done, start,
                   blocked, judged>>

(* Legacy observes or claims a turn asynchronously, or never (r3 trajectories). *)
Observe ==
    /\ EnvOK
    /\ Scenario \in {"free", "rowpending", "rotate"}
    /\ \E s \in S, i \in T :
          /\ Exists(s, i) /\ cus[s][i] = "none" /\ ~dlv[s][i]
          /\ \E c \in {"row", "pending"} : cus' = [cus EXCEPT ![s][i] = c]
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, early2, prebind, ingen, readable, unres, done,
                   start, blocked, judged>>

(* A Legacy delivery this run (before the gate): the floor moves, on the     *)
(* current source B of the "moved" trajectory.                              *)
Evidence ==
    /\ EnvOK
    /\ Scenario = "moved"
    /\ \E s \in S, i \in T :
          /\ Exists(s, i) /\ ended[s][i] /\ ~dlv[s][i] /\ ev[s][i] = "none"
          /\ Scenario = "moved" => s = 2
          /\ dlv' = [dlv EXCEPT ![s][i] = TRUE]
          /\ ev' = [ev EXCEPT ![s][i] = "cur"]
          /\ early2' = [early2 EXCEPT ![s][i] = (Cardinality(InGen) = 1)]
    /\ UNCHANGED <<nb, nt, ended, early, prebind, ingen, cus, readable, unres, done, start,
                   blocked, judged>>

(* An earlier source's long delivery finishes after a later source bound: its *)
(* Historical receipt is dated in the later span, at bytes a turn there ends. *)
Historical ==
    /\ EnvOK
    /\ Scenario \in {"hist", "all"}
    /\ \E s \in S, i \in T :
          /\ Exists(s, i) /\ s # FirstInGen /\ ingen[s]
          /\ ended[s][i] /\ ~dlv[s][i] /\ ev[s][i] = "none"
          /\ ev' = [ev EXCEPT ![s][i] = "hist"]
    /\ UNCHANGED <<nb, nt, ended, early, dlv, early2, prebind, ingen, cus, readable, unres, done,
                   start, blocked, judged>>

(* A later source's answer edits its placeholder: the receipt is dated before  *)
(* that source's bind and lands on bytes where the first source's turn ends.  *)
Edit ==
    /\ EnvOK
    /\ Scenario = "edit"
    /\ Cardinality(InGen) >= 2
    /\ \E i \in T :
          /\ Exists(FirstInGen, i) /\ ended[FirstInGen][i] /\ ~dlv[FirstInGen][i]
          /\ ev[FirstInGen][i] = "none"
          /\ ev' = [ev EXCEPT ![FirstInGen][i] = "edit"]
          /\ early2' = [early2 EXCEPT ![FirstInGen][i] = TRUE]
    /\ UNCHANGED <<nb, nt, ended, early, dlv, prebind, ingen, cus, readable, unres, done, start,
                   blocked, judged>>

(* The generation's start moves past the oldest older source's span. *)
GenMove ==
    /\ EnvOK
    /\ Scenario \in {"genmove", "genfresh"}
    /\ \E s \in InGen : s < nb /\ s = FirstInGen /\ ingen' = [ingen EXCEPT ![s] = FALSE]
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, early2, prebind, cus, readable, unres, done,
                   start, blocked, judged>>

ReadOK ==
    /\ EnvOK
    /\ \E s \in S : ~readable[s] /\ readable' = [readable EXCEPT ![s] = TRUE]
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, early2, prebind, ingen, cus, unres, done, start,
                   blocked, judged>>

Unblock ==
    /\ EnvOK /\ blocked
    /\ blocked' = FALSE
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, early2, prebind, ingen, cus, readable, unres,
                   done, start, judged>>

(* ------------------------------ rules ------------------------------------- *)
CustodyIdle == \A s \in S, i \in T : Exists(s, i) => cus[s][i] = "none"
EarlyEnded(s) == {i \in 1..nt[s] : ended[s][i] /\ early[s][i]}
R3Delivered(s, i) == ended[s][i] /\ early[s][i] /\ (CustodyIdle \/ i # MaxOf(EarlyEnded(s)))
R3First(s) == MinOf({i \in 1..nt[s] : ~R3Delivered(s, i)}, nt[s] + 1)

\* the turns Legacy truly delivered and recorded for s in this generation
CurFloor(s) == MaxOf({i \in 1..nt[s] : ev[s][i] = "cur"})
AnyFloor(s) == MaxOf({i \in 1..nt[s] : ev[s][i] # "none"})
\* r4 places a record by its date: a Historical receipt passes as this source's
DatedFloor(s) == MaxOf({i \in 1..nt[s] : ev[s][i] \in {"cur", "hist"}})
\* r5 trusts a date before the second bind; an edited placeholder's receipt passes too
R5Proven(s, i) ==
    ev[s][i] \in {"cur", "edit"} /\ (Cardinality(InGen) = 1 \/ (s = FirstInGen /\ early2[s][i]))
R5Floor(s) == MaxOf({i \in 1..nt[s] : R5Proven(s, i)})
\* r6 counts a record only while the generation bound one source
Proven(s, i) == ev[s][i] \in {"cur", "edit"} /\ Cardinality(InGen) = 1
ProvenFloor(s) == MaxOf({i \in 1..nt[s] : Proven(s, i)})

\* d1: aligned records of any generation or source, the bind-time filter
D1First(s) == MinOf({i \in 1..nt[s] : i > AnyFloor(s) /\ ~prebind[s][i]}, nt[s] + 1)
\* r4, r5 and the r5 mutants
NewFloor(s) ==
    CASE Mut = "genIgnore" -> AnyFloor(s)
      [] Rule = "r4" -> DatedFloor(s)
      [] Rule = "r5" -> R5Floor(s)
      [] OTHER -> ProvenFloor(s)
NewFirst(s) == MinOf({i \in 1..nt[s] : i > NewFloor(s) /\ ~(Mut = "prebindFilter" /\ prebind[s][i])},
                     nt[s] + 1)

Evidenced(F(_)) == {s \in 1..nb : F(s) > 0}
StopAt(F(_)) == IF Evidenced(F) = {} THEN 1 ELSE MaxOf(Evidenced(F))

Walk ==
    CASE Rule = "r3" -> {s \in 1..nb : s + 1 >= nb}
      [] Rule = "d1" -> {s \in StopAt(AnyFloor)..nb : ingen[s]} \cup unres
      [] OTHER -> InGen \cup unres

First(s) ==
    CASE Rule = "r3" -> R3First(s)
      [] Rule = "d1" -> D1First(s)
      [] OTHER -> NewFirst(s)

Decided == [s \in S |-> IF s \in Walk /\ (s = nb \/ First(s) <= nt[s]) THEN First(s) ELSE 0]
Named == {s \in S : Decided[s] # 0}
Owed(s) == Decided[s] # 0 /\ First(s) <= nt[s]

(* A Retry: a source to read is unreadable, or custody holds the commit. The  *)
(* sources judged so far join the unresolved set; r3 keeps none, and d1 and r4 *)
(* keep the current source only on its own read failure.                       *)
ReadJudged == IF ~readable[nb] THEN {nb}
              ELSE {s \in Named : \A r \in Named : (r > s) => readable[r]}
\* the current source still holds only its prompt: the fresh path judges it
FreshCur == nt[nb] = 1 /\ ~ended[nb][1]
RetryJudged ==
    IF \E s \in Named : ~readable[s] THEN ReadJudged
    ELSE IF Rule = "r6" THEN {s \in Named : Owed(s)}
    ELSE IF Rule = "r5" THEN {s \in Named : Owed(s) /\ ~(s = nb /\ FreshCur)}
    ELSE {s \in Named : Owed(s) /\ s # nb}

RetryJudge ==
    /\ ~done
    /\ blocked \/ \E s \in Named : ~readable[s]
    /\ Rule # "r3"
    /\ judged' = judged \cup (IF \E s \in Named : ~readable[s] THEN ReadJudged
                              ELSE {s \in Named : Owed(s)})
    /\ unres' = unres \cup RetryJudged
    /\ <<unres', judged'>> # <<unres, judged>>
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, early2, prebind, ingen, cus, readable, done, start,
                   blocked>>

Commit ==
    /\ ~done /\ ~blocked
    /\ \A s \in Named : readable[s]
    /\ start' = Decided
    /\ done' = TRUE
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, early2, prebind, ingen, cus, readable, unres,
                   blocked, judged>>

Next ==
    \/ Bind \/ NewTurn \/ EndTurn \/ Observe \/ Evidence \/ Historical \/ Edit \/ GenMove \/ ReadOK
    \/ Unblock \/ RetryJudge \/ Commit
    \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars /\ WF_vars(Commit) /\ WF_vars(ReadOK) /\ WF_vars(Unblock)

(* ---------------------------- properties ---------------------------------- *)
Taken(s, i) == start[s] # 0 /\ start[s] <= i

(* Legacy-equivalence within the generation: an unposted turn above the      *)
(* current-generation floor of a source Legacy may still re-read is O's.     *)
LegacyOwes(s, i) == Exists(s, i) /\ ~dlv[s][i] /\ i > CurFloor(s) /\ ingen[s]

NoAdoptionLoss == done => \A s \in S, i \in T : LegacyOwes(s, i) => Taken(s, i)
\* a turn whose recorded delivery is proven for its source is not posted again
NoRepostBelowFloor == done => \A s \in S, i \in T :
    (Exists(s, i) /\ i <= ProvenFloor(s)) => ~Taken(s, i)
\* a source once judged owed or retried is named by the init unless its floor covers it
UnresolvedKept == done => \A s \in unres : (Exists(s, 1) /\ CurFloor(s) < nt[s]) => start[s] # 0
\* acceptance: a source an attempt judged owed keeps its unposted turns, generation or not
JudgedOwedKept == done => \A s \in judged, i \in T :
    (Exists(s, i) /\ ~dlv[s][i] /\ i > CurFloor(s)) => Taken(s, i)

(* Residual, informational: a turn Legacy itself abandoned below its floor,   *)
(* and one posted without a record.                                           *)
NoLossStrict == done => \A s \in S, i \in T : (Exists(s, i) /\ ingen[s] /\ ~dlv[s][i]) => Taken(s, i)
NoDupDelivered == done => \A s \in S, i \in T : (Exists(s, i) /\ dlv[s][i]) => ~Taken(s, i)

AdoptionCommits == <>done
================================================================================
