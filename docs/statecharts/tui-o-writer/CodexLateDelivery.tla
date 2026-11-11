--------------------------- MODULE CodexLateDelivery ---------------------------
(* Codex late takeover start point. Custody (Legacy's local hold) and delivery *)
(* (what Legacy posted) are independent. ev[s][i] is the delivery evidence an *)
(* evidence reader may meet for turn i of source s:                            *)
(*   "cur"   a record of the current tmux generation, dated inside s's bind    *)
(*           span: the delivery of that turn (it implies dlv);                 *)
(*   "bogus" a record whose bytes align to the turn but that belongs to an     *)
(*           older generation or another source of the same generation.        *)
(* P2 ships in the same PR, so after boot no TUI body is posted except where  *)
(* the environment action Evidence stands for a delivery before the gate.     *)
(*                                                                            *)
(* Rule "r3"  custody absence and turn time as evidence, previous source only. *)
(*      "d1"  first design: any aligned record, bind-time filter kept, walk    *)
(*            stops at the newest evidenced source, local unresolved set.      *)
(*      "r4"  exclude only turns at or below the current-generation floor of   *)
(*            s, no bind-time filter, walk every source of the generation and  *)
(*            the unresolved set.                                              *)
(* Mut  "genIgnore" r4 trusting bogus records, "prebindFilter" r4 with the    *)
(*      bind-time filter back, "stopAtEvidence" r4 with the d1 walk stop.     *)
EXTENDS Naturals, FiniteSets

CONSTANTS Rule, Mut, Scenario

S == 1..3
T == 1..2
Cus == {"none", "row", "pending"}
Ev == {"none", "cur", "bogus"}

VARIABLES nb, nt, ended, early, dlv, ev, prebind, ingen, cus, readable, unres, done, start

vars == <<nb, nt, ended, early, dlv, ev, prebind, ingen, cus, readable, unres, done, start>>

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
    /\ prebind \in [S -> [T -> BOOLEAN]]
    /\ ingen \in [S -> BOOLEAN]
    /\ cus \in [S -> [T -> Cus]]
    /\ readable \in [S -> BOOLEAN]
    /\ unres \subseteq S
    /\ done \in BOOLEAN
    /\ start \in [S -> 0..3]

(* One source's pre-boot history. A genuine record names a posted, ended turn; *)
(* a turn below it may be unposted (order inversion), one above it may be      *)
(* posted with no record; a bogus record says nothing about the turn.          *)
SrcOk(r, isLast) ==
    \A i \in T :
        /\ i > r.c => (~r.e[i] /\ ~r.d[i] /\ r.v[i] = "none" /\ ~r.p[i])
        /\ (i <= r.c /\ (~isLast \/ i < r.c)) => r.e[i]
        /\ r.v[i] = "cur" => (r.e[i] /\ r.d[i])
        /\ ~r.e[i] => (~r.d[i] /\ r.v[i] = "none")

SrcOpts(isLast) ==
    {r \in [c : 0..2, e : [T -> BOOLEAN], d : [T -> BOOLEAN], v : [T -> Ev], p : [T -> BOOLEAN]] :
        SrcOk(r, isLast)}

Empty == [c |-> 0, e |-> NoTurnF, d |-> NoTurnF, v |-> NoEvF, p |-> NoTurnF]

AllInit ==
    \E n \in 1..2 :
    \E r1 \in SrcOpts(n = 1) :
    \E r2 \in (IF n = 2 THEN SrcOpts(TRUE) ELSE {Empty}) :
    \E oldA \in (IF n = 2 THEN BOOLEAN ELSE {FALSE}) :
        LET R == [s \in S |-> IF s = 1 THEN r1 ELSE IF s = 2 THEN r2 ELSE Empty]
        IN /\ nb = n
           /\ nt = [s \in S |-> R[s].c]
           /\ ended = [s \in S |-> R[s].e]
           /\ dlv = [s \in S |-> R[s].d]
           /\ ev = [s \in S |-> R[s].v]
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
    /\ prebind = [s \in S |-> [i \in T |-> s = 1 /\ i = 1 /\ preA]]
    /\ ingen = [s \in S |-> TRUE]
    /\ cus = [s \in S |-> [i \in T |-> IF s = 1 /\ i = 1 THEN cusA ELSE "none"]]

RowPendingInit ==
    /\ nb = 1
    /\ nt = [s \in S |-> IF s = 1 THEN 2 ELSE 0]
    /\ ended = [s \in S |-> [i \in T |-> s = 1]]
    /\ dlv = [s \in S |-> NoTurnF]
    /\ ev = [s \in S |-> NoEvF]
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

(* ---------------------------- environment ---------------------------------- *)
EnvOK == ~done
Frozen == <<nt, ended, early, dlv, ev, prebind, cus, readable, unres, done, start>>

Bind ==
    /\ EnvOK /\ nb < 3
    /\ Scenario \in {"rotate", "moved", "all"}
    /\ nb' = nb + 1
    /\ ingen' = [ingen EXCEPT ![nb + 1] = TRUE]
    /\ UNCHANGED Frozen

NewTurn ==
    /\ EnvOK
    /\ Scenario \notin {"moved", "gen", "pending"}
    /\ nt[nb] < 2
    /\ IF nt[nb] = 0 THEN TRUE ELSE ended[nb][nt[nb]]
    /\ Scenario = "rotate" => (nb = 2 /\ nt[nb] = 0)
    /\ nt' = [nt EXCEPT ![nb] = @ + 1]
    /\ UNCHANGED <<nb, ended, early, dlv, ev, prebind, ingen, cus, readable, unres, done, start>>

EndTurn ==
    /\ EnvOK
    /\ Scenario \in {"free", "rowpending", "all"}
    /\ IF nt[nb] = 0 THEN FALSE ELSE ~ended[nb][nt[nb]]
    /\ ended' = [ended EXCEPT ![nb][nt[nb]] = TRUE]
    \* only r3 reads early; "all" checks r4, so its turns end before the baseline
    /\ early' \in {[early EXCEPT ![nb][nt[nb]] = b] : b \in IF Scenario = "all" THEN {TRUE} ELSE BOOLEAN}
    /\ UNCHANGED <<nb, nt, dlv, ev, prebind, ingen, cus, readable, unres, done, start>>

(* Legacy observes or claims a turn asynchronously, or never (r3 trajectories). *)
Observe ==
    /\ EnvOK
    /\ Scenario \in {"free", "rowpending", "rotate"}
    /\ \E s \in S, i \in T :
          /\ Exists(s, i) /\ cus[s][i] = "none" /\ ~dlv[s][i]
          /\ \E c \in {"row", "pending"} : cus' = [cus EXCEPT ![s][i] = c]
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, prebind, ingen, readable, unres, done, start>>

(* A Legacy delivery this run (before the gate): the floor moves. In "moved" *)
(* it lands on the current source B only.                                    *)
Evidence ==
    /\ EnvOK
    /\ Scenario \in {"moved", "all"}
    /\ \E s \in S, i \in T :
          /\ Exists(s, i) /\ ended[s][i] /\ ~dlv[s][i] /\ ev[s][i] = "none"
          /\ Scenario = "moved" => s = 2
          /\ dlv' = [dlv EXCEPT ![s][i] = TRUE]
          /\ ev' = [ev EXCEPT ![s][i] = "cur"]
    /\ UNCHANGED <<nb, nt, ended, early, prebind, ingen, cus, readable, unres, done, start>>

ReadOK ==
    /\ EnvOK
    /\ \E s \in S : ~readable[s] /\ readable' = [readable EXCEPT ![s] = TRUE]
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, prebind, ingen, cus, unres, done, start>>

(* ------------------------------ rules ------------------------------------- *)
CustodyIdle == \A s \in S, i \in T : Exists(s, i) => cus[s][i] = "none"
EarlyEnded(s) == {i \in 1..nt[s] : ended[s][i] /\ early[s][i]}
R3Delivered(s, i) == ended[s][i] /\ early[s][i] /\ (CustodyIdle \/ i # MaxOf(EarlyEnded(s)))
R3First(s) == MinOf({i \in 1..nt[s] : ~R3Delivered(s, i)}, nt[s] + 1)

CurFloor(s) == MaxOf({i \in 1..nt[s] : ev[s][i] = "cur"})
AnyFloor(s) == MaxOf({i \in 1..nt[s] : ev[s][i] # "none"})

\* d1: aligned records of any generation or source, the bind-time filter
D1First(s) == MinOf({i \in 1..nt[s] : i > AnyFloor(s) /\ ~prebind[s][i]}, nt[s] + 1)
\* r4 and its mutants
R4Floor(s) == IF Mut = "genIgnore" THEN AnyFloor(s) ELSE CurFloor(s)
R4First(s) == MinOf({i \in 1..nt[s] : i > R4Floor(s) /\ ~(Mut = "prebindFilter" /\ prebind[s][i])},
                    nt[s] + 1)

Evidenced(F(_)) == {s \in 1..nb : F(s) > 0}
StopAt(F(_)) == IF Evidenced(F) = {} THEN 1 ELSE MaxOf(Evidenced(F))

Walk ==
    CASE Rule = "r3" -> {s \in 1..nb : s + 1 >= nb}
      [] Rule = "d1" -> {s \in StopAt(AnyFloor)..nb : ingen[s]} \cup unres
      [] Mut = "stopAtEvidence" -> {s \in StopAt(CurFloor)..nb : ingen[s]} \cup unres
      [] OTHER -> {s \in 1..nb : ingen[s]} \cup unres

First(s) ==
    CASE Rule = "r3" -> R3First(s)
      [] Rule = "d1" -> D1First(s)
      [] OTHER -> R4First(s)

Decided == [s \in S |-> IF s \in Walk /\ (s = nb \/ First(s) <= nt[s]) THEN First(s) ELSE 0]
Named == {s \in S : Decided[s] # 0}

(* A source the attempt must read is unreadable: Retry. The sources judged so *)
(* far (the current one first, then older ones down to the unreadable one)   *)
(* join the unresolved set; r3 keeps none.                                   *)
Judged == IF ~readable[nb] THEN {nb}
          ELSE {s \in Named : \A r \in Named : (r > s) => readable[r]}

RetryJudge ==
    /\ ~done
    /\ \E s \in Named : ~readable[s]
    /\ Rule # "r3"
    /\ unres' = unres \cup Judged
    /\ unres' # unres
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, prebind, ingen, cus, readable, done, start>>

Commit ==
    /\ ~done
    /\ \A s \in Named : readable[s]
    /\ start' = Decided
    /\ done' = TRUE
    /\ UNCHANGED <<nb, nt, ended, early, dlv, ev, prebind, ingen, cus, readable, unres>>

Next ==
    \/ Bind \/ NewTurn \/ EndTurn \/ Observe \/ Evidence \/ ReadOK \/ RetryJudge \/ Commit
    \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars /\ WF_vars(Commit) /\ WF_vars(ReadOK)

(* ---------------------------- properties ---------------------------------- *)
Taken(s, i) == start[s] # 0 /\ start[s] <= i

(* Legacy-equivalence within the generation: an unposted turn above the      *)
(* current-generation floor of a source Legacy may still re-read is O's.     *)
LegacyOwes(s, i) == Exists(s, i) /\ ~dlv[s][i] /\ i > CurFloor(s) /\ ingen[s]

NoAdoptionLoss == done => \A s \in S, i \in T : LegacyOwes(s, i) => Taken(s, i)
NoRepostBelowFloor == done => \A s \in S, i \in T : (Exists(s, i) /\ i <= CurFloor(s)) => ~Taken(s, i)
\* a source once judged owed or retried is named by the init unless its floor covers it
UnresolvedKept == done => \A s \in unres : (Exists(s, 1) /\ CurFloor(s) < nt[s]) => start[s] # 0

(* Residual, informational: a turn Legacy itself abandoned below its floor,   *)
(* and one posted without a record.                                           *)
NoLossStrict == done => \A s \in S, i \in T : (Exists(s, i) /\ ingen[s] /\ ~dlv[s][i]) => Taken(s, i)
NoDupDelivered == done => \A s \in S, i \in T : (Exists(s, i) /\ dlv[s][i]) => ~Taken(s, i)

AdoptionCommits == <>done
================================================================================
