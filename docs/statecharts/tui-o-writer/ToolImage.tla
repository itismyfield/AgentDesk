---- MODULE ToolImage ----
\* Tool-image attachments in the tui_o writer ledger: turn attribution across GC and restart,
\* durable spans and call attribution, omission notices, shared Prepared, confirm outcomes.
\* Report: tool-image.report.md (code SHA there). Mode "before" = no image path at all.
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Rec,            \* records: "o1"/"o2" opener, "p" prompt (ignored inside a turn), "h1" image,
                          \* "b1"/"b2" over-limit images (same length), "c" tool call, "r" its late
                          \* image result (h2), "s" its late result carrying h1, "i" native idle,
                          \* "x" another turn's named idle,
                          \* "e" physical end
          Mode,           \* "before" | "design"
          SpanRestore,    \* durable turn spans used on replay (FALSE = mutant)
          PhysEndForward, \* end_projection closes the image tracker (FALSE = mutant)
          NoticeRetry,    \* selector re-prepares a NotFound notice (FALSE = mutant)
          DigestKept,     \* big images keep decoded sha256 identity (FALSE = length identity)
          AmbigEnds, UnresolvedEnds, \* mutants: these outcomes end the obligation
          Lag,            \* history may miss an arrived post once (policy-(a) limit)
          ClosedOffset,   \* replay applies a restored span's Closed end_offset first (FALSE = mutant)
          GcRule,         \* "closed" (opener kept until a durable Closed) | "opened" (d3 rule, mutant)
                          \* | "span" (S2b w1: only a turn with an image span keeps its opener)
                          \* | "persist" (no opener kept; GC first records a live turn's span)
          SharedPrepared, \* notices wait for an image Prepared and vice versa (FALSE = mutant)
          BytesRule,      \* "all" (every open obligation keeps bytes) | "images" (d3 NeedsBytes, mutant)
          RowPrep,        \* row preparation restores a durable span before the row's facts (FALSE = S2a w2)
          DurableCall,    \* a call pending at its turn's end is attributed durably (FALSE = S2a w2)
          NamedIdleGuard, \* a named idle of another turn leaves the open turn (FALSE = S2a w2)
          MissedEndCloses, \* S2b: a verified end without this run's opener closes the prepared image turn
          BodyRule,       \* "none" (no body after an Unresolved attempt) | "w4" (re-settlement counts that
                          \* body as an altered post) | "owned" (it skips posts other serials own)
          SelectSettle,   \* "none" (no restored body Prepared) | "always" (each image poll settles it, w5)
                          \* | "due" (only a due image step settles it)
          MaxRestarts, MaxFail, MaxHFail

N == Len(Rec)
Imgs == {"h1", "h2", "b1", "b2"}
IsImg(i) == Rec[i] \in Imgs \/ Rec[i] \in {"r", "s"}
ImgOf(i) == CASE Rec[i] = "r" -> "h2" [] Rec[i] = "s" -> "h1" [] OTHER -> Rec[i]
Big(h) == h \in {"b1", "b2"}
RECURSIVE FirstOpener(_, _)
FirstOpener(o, j) == IF j > N THEN 0 ELSE IF Rec[j] = o THEN j ELSE FirstOpener(o, j + 1)
CallPos == FirstOpener("c", 1)
\* Ground truth: a prompt or another turn's idle leaves the turn; a late result belongs to its call.
RECURSIVE TT(_)
TT(i) == IF i = 0 THEN "none"
         ELSE CASE Rec[i] = "o1" -> "t1" [] Rec[i] = "o2" -> "t2"
                [] Rec[i] \in {"r", "s"} -> TT(CallPos)
                [] Rec[i] \in {"i", "e"} -> "none" [] OTHER -> TT(i - 1)
Turns == {"t1", "t2", "tp"}
Tids == Turns \cup {"edge"}
HK == Imgs \cup {"L"}
Keys == Tids \X HK
Groups == {i \in 1..N : IsImg(i)}
PairOf(i) == <<TT(i), ImgOf(i)>>
Pairs == ({"t1", "t2", "tp", "none"}) \X Imgs
ReqPairs == {PairOf(i) : i \in Groups}
HKey(h) == IF Big(h) /\ ~DigestKept THEN "L" ELSE h

VARIABLES cursor, saved, rstart, ts, openPos, spanFrom, spanTo,
          st, unknown, att, keyRec, ns, arrived, nArr, hidden,
          restarts, fails, hfails, retired, callTurn, pending, dcall, viol, behind, bodyOpen, early
vars == <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec,
          ns, arrived, nArr, hidden, restarts, fails, hfails, retired, callTurn, pending, dcall, viol, behind, bodyOpen, early>>

\* st: none ready retry prepared posted omitted unres ambig dropped
\* ns: none pending prepared nunknown nf posted
Init ==
  /\ cursor = 0 /\ saved = 0 /\ rstart = 1 /\ ts = "idle" /\ openPos = 0
  /\ spanFrom = [t \in Turns |-> 0] /\ spanTo = [t \in Turns |-> 0]
  /\ st = [k \in Keys |-> "none"] /\ unknown = [k \in Keys |-> FALSE]
  /\ att = [k \in Keys |-> 0] /\ keyRec = [k \in Keys |-> 0]
  /\ ns = [g \in Groups |-> "none"]
  /\ arrived = [p \in Pairs |-> 0] /\ nArr = [g \in Groups |-> 0]
  /\ hidden = [k \in Keys |-> 0]
  /\ restarts = 0 /\ fails = 0 /\ hfails = 0 /\ retired = FALSE
  /\ callTurn = "none" /\ pending = FALSE /\ dcall = "none" /\ viol = FALSE
  /\ behind = [k \in Keys |-> FALSE]
  /\ bodyOpen = (SelectSettle # "none") /\ early = FALSE

Covering(i) == {t \in Turns : spanFrom[t] > 0 /\ spanFrom[t] <= i /\ (spanTo[t] = 0 \/ i < spanTo[t])}
Close(t, i) == IF t \in Turns /\ spanFrom[t] > 0 /\ spanTo[t] = 0
               THEN spanTo' = [spanTo EXCEPT ![t] = i] ELSE UNCHANGED spanTo
Fixed == <<saved, rstart, unknown, att, arrived, nArr, hidden, restarts, fails, hfails, retired, behind, bodyOpen, early>>

\* Row preparation: an unknown tracker restores the durable span holding this row first.
RowTs(i) == IF RowPrep /\ ts = "unknown" /\ Covering(i) # {} THEN CHOOSE t \in Covering(i) : TRUE ELSE ts
\* A span-restored Open(t) whose durable Closed end_offset is at or before this record is Idle first.
PreTs(i) == LET t == RowTs(i) IN
            IF ClosedOffset /\ t \in Turns /\ spanTo[t] > 0 /\ i >= spanTo[t] THEN "idle" ELSE t
OpenerOf(r) == CASE r = "o1" -> "t1" [] r = "o2" -> "t2" [] OTHER -> "tp"
Inside(t, at) == \E u \in Turns \ {t} :
                   spanFrom[u] > 0 /\ spanFrom[u] <= at /\ (spanTo[u] = 0 \/ at < spanTo[u])
KeepCalls == UNCHANGED <<callTurn, pending, dcall, viol>>

Derive ==
  /\ ~retired /\ cursor < N
  /\ LET i == cursor + 1  r == Rec[i]  ts0 == PreTs(i) IN
     /\ cursor' = i
     /\ CASE r \in {"o1", "o2", "p"} ->
              \* A prompt inside an open turn never reopens it (seal.rs:101).
              /\ IF ts0 \in Turns THEN /\ ts' = ts0 /\ UNCHANGED openPos
                 ELSE /\ ts' = OpenerOf(r) /\ openPos' = i
              /\ UNCHANGED <<spanFrom, spanTo, st, keyRec, ns>> /\ KeepCalls
          \* restart_end projection applies a physical end only with this run's opener evidence.
          \* The S2b cut runs after row preparation, so it sees a turn restored from its span.
          [] r = "i" \/ (r = "e" /\ PhysEndForward /\ (openPos > 0 \/ (MissedEndCloses /\ ts0 \in Turns)))
                     \/ (r = "x" /\ ~NamedIdleGuard) ->
              /\ ts' = "idle" /\ Close(ts0, i)
              \* A call still waiting for its result is attributed durably at its turn's end.
              /\ dcall' = IF DurableCall /\ pending /\ callTurn \in Turns /\ dcall = "none"
                          THEN callTurn ELSE dcall
              /\ UNCHANGED <<openPos, spanFrom, st, keyRec, ns, callTurn, pending, viol>>
          [] r = "c" ->
              /\ ts' = ts0 /\ callTurn' = (IF ts0 \in Turns THEN ts0 ELSE callTurn) /\ pending' = TRUE
              /\ UNCHANGED <<openPos, spanFrom, spanTo, st, keyRec, ns, dcall, viol>>
          [] IsImg(i) /\ Mode = "design" ->
              LET h == ImgOf(i)
                  cov == Covering(i)
                  callT == IF r \notin {"r", "s"} THEN "none"
                           ELSE IF callTurn \in Turns THEN callTurn
                           ELSE IF DurableCall /\ dcall \in Turns THEN dcall ELSE "none"
                  tid == IF callT \in Turns THEN callT
                         ELSE IF ts0 \in Turns THEN ts0
                         ELSE IF SpanRestore /\ cov # {} THEN CHOOSE t \in cov : TRUE
                         ELSE "edge"
                  k == <<tid, HKey(h)>>
                  opens == ts0 \in Turns /\ tid = ts0 /\ spanFrom[ts0] = 0 IN
              /\ ts' = IF ts0 \in Turns \/ callT \in Turns THEN ts0
                       ELSE IF tid \in Turns THEN tid ELSE ts0
              /\ UNCHANGED openPos
              \* First image of a live turn persists its span; inside another span it is a violation.
              /\ IF opens THEN spanFrom' = [spanFrom EXCEPT ![ts0] = openPos] ELSE UNCHANGED spanFrom
              /\ viol' = (viol \/ (opens /\ Inside(ts0, openPos)))
              /\ pending' = IF r \in {"r", "s"} THEN FALSE ELSE pending
              /\ UNCHANGED <<spanTo, callTurn, dcall>>
              /\ IF st[k] # "none" THEN UNCHANGED <<st, keyRec, ns>>
                 ELSE /\ keyRec' = [keyRec EXCEPT ![k] = i]
                      /\ IF Big(h)
                         THEN st' = [st EXCEPT ![k] = "omitted"] /\ ns' = [ns EXCEPT ![i] = "pending"]
                         ELSE st' = [st EXCEPT ![k] = "ready"] /\ UNCHANGED ns
          [] OTHER -> /\ ts' = ts0 /\ UNCHANGED <<openPos, spanFrom, spanTo, st, keyRec, ns>> /\ KeepCalls
  /\ UNCHANGED Fixed

SaveCursor ==
  /\ ~retired /\ saved < cursor /\ \A k \in Keys : st[k] # "ready"
  /\ saved' = cursor
  /\ UNCHANGED <<cursor, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec, ns,
                 arrived, nArr, hidden, restarts, fails, hfails, retired, callTurn, pending, dcall, viol, behind, bodyOpen, early>>

ImgOpen == IF BytesRule = "all" THEN {"ready", "retry", "prepared", "unres", "ambig"}
           ELSE {"ready", "retry", "prepared", "unres"}
NeedsBytes(i) == (\E k \in Keys : keyRec[k] = i /\ st[k] \in ImgOpen)
                 \/ (BytesRule = "all" /\ i \in Groups /\ ns[i] \in {"pending", "prepared", "nunknown", "nf"})
OpenerPos(t) == FirstOpener(CASE t = "t1" -> "o1" [] t = "t2" -> "o2" [] OTHER -> "p", 1)
OpenerProtected(r) ==
  \E t \in Turns : r = OpenerPos(t) /\
     CASE GcRule = "closed" -> ts = t \/ (spanFrom[t] > 0 /\ spanTo[t] = 0)
       [] GcRule = "span" -> spanFrom[t] > 0 /\ spanTo[t] = 0
       [] GcRule = "persist" -> FALSE
       [] OTHER -> ts = t /\ spanFrom[t] = 0
\* GC removes the oldest retained record (one record = one segment here).
Gc ==
  /\ ~retired /\ rstart <= saved
  /\ ~OpenerProtected(rstart)
  /\ ~NeedsBytes(rstart)
  /\ rstart' = rstart + 1
  \* "persist": collecting a live turn's opener first records its span, and collecting a
  \* pending call first records the call's turn, so neither needs the collected bytes again.
  /\ IF GcRule = "persist" /\ ts \in Turns /\ OpenerPos(ts) = rstart /\ spanFrom[ts] = 0
     THEN spanFrom' = [spanFrom EXCEPT ![ts] = rstart] ELSE UNCHANGED spanFrom
  /\ IF GcRule = "persist" /\ pending /\ callTurn \in Turns /\ CallPos = rstart /\ dcall = "none"
     THEN dcall' = callTurn ELSE UNCHANGED dcall
  /\ UNCHANGED <<cursor, saved, ts, openPos, spanTo, st, unknown, att, keyRec, ns,
                 arrived, nArr, hidden, restarts, fails, hfails, retired, callTurn, pending, viol, behind, bodyOpen, early>>

OthersV == <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, keyRec, ns, nArr, restarts, retired, callTurn, pending, dcall, viol, bodyOpen, early>>

NoticeOpen == {g \in Groups : ns[g] \in {"prepared", "nunknown"}}
Prepare(k) ==
  /\ ~retired /\ st[k] \in {"ready", "retry"} /\ \A j \in Keys : st[j] # "prepared" /\ ~bodyOpen
  /\ (SharedPrepared => NoticeOpen = {})
  /\ st' = [st EXCEPT ![k] = "prepared"] /\ att' = [att EXCEPT ![k] = @ + 1]
  /\ behind' = [behind EXCEPT ![k] = FALSE]       \* a new attempt takes the newest anchor
  /\ UNCHANGED <<unknown, arrived, hidden, fails, hfails>> /\ UNCHANGED OthersV

Arr(k) == arrived' = [arrived EXCEPT ![PairOf(keyRec[k])] = @ + 1]
Post(k) ==
  /\ ~retired /\ st[k] = "prepared" /\ ~unknown[k]
  /\ \/ Arr(k) /\ st' = [st EXCEPT ![k] = "posted"] /\ UNCHANGED <<unknown, fails, hidden, att>>
     \/ /\ fails < MaxFail /\ fails' = fails + 1 /\ Arr(k) /\ unknown' = [unknown EXCEPT ![k] = TRUE]
        /\ \E hd \in (IF Lag THEN {0, 1} ELSE {0}) : hidden' = [hidden EXCEPT ![k] = hd]
        /\ UNCHANGED <<st, att>>
     \/ /\ fails < MaxFail /\ fails' = fails + 1 /\ unknown' = [unknown EXCEPT ![k] = TRUE]
        /\ UNCHANGED <<st, arrived, hidden, att>>
     \/ /\ fails < MaxFail /\ fails' = fails + 1     \* Withheld -> NotSent: does not count
        /\ st' = [st EXCEPT ![k] = "retry"] /\ att' = [att EXCEPT ![k] = @ - 1]
        /\ UNCHANGED <<unknown, arrived, hidden>>
  /\ UNCHANGED <<hfails, behind, bodyOpen, early>> /\ UNCHANGED OthersV

\* History read of one attempt, with its own saved payload/anchor.
Seen(k) == arrived[PairOf(keyRec[k])] - hidden[k]
Judge(k) ==
  CASE Seen(k) = 1 -> st' = [st EXCEPT ![k] = "posted"] /\ UNCHANGED ns
    [] Seen(k) >= 2 /\ ~AmbigEnds -> st' = [st EXCEPT ![k] = "ambig"] /\ UNCHANGED ns
    \* Mutant: Ambiguous handled as NotFound -> ends as an omission.
    [] Seen(k) >= 2 -> st' = [st EXCEPT ![k] = "omitted"] /\ ns' = [ns EXCEPT ![keyRec[k]] = "pending"]
    [] OTHER -> IF att[k] < 2
                THEN st' = [st EXCEPT ![k] = "retry"] /\ UNCHANGED ns
                ELSE /\ st' = [st EXCEPT ![k] = "omitted"]              \* not_confirmed
                     /\ ns' = [ns EXCEPT ![keyRec[k]] = "pending"]

Settle(k) ==
  /\ ~retired /\ st[k] = "prepared" /\ unknown[k]
  /\ unknown' = [unknown EXCEPT ![k] = FALSE] /\ hidden' = [hidden EXCEPT ![k] = 0]
  /\ \/ Judge(k) /\ UNCHANGED hfails
     \/ /\ hfails < MaxHFail /\ hfails' = hfails + 1
        /\ st' = [st EXCEPT ![k] = IF UnresolvedEnds THEN "dropped" ELSE "unres"] /\ UNCHANGED ns
  /\ UNCHANGED <<att, arrived, fails, behind, bodyOpen, early>>
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, keyRec, nArr, restarts, retired, callTurn, pending, dcall, viol>>

\* Selector: re-settle a recorded Unresolved attempt; never a new POST. Under "w4" a later body
\* with no exact match reads as an altered post and the attempt stays Unresolved.
Reconfirm(k) ==
  /\ ~retired /\ st[k] = "unres"
  /\ IF BodyRule = "w4" /\ behind[k] /\ Seen(k) = 0 THEN UNCHANGED <<st, ns>> ELSE Judge(k)
  /\ hidden' = [hidden EXCEPT ![k] = 0]
  /\ UNCHANGED <<unknown, att, arrived, fails, hfails, behind, bodyOpen, early>>
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, keyRec, nArr, restarts, retired, callTurn, pending, dcall, viol>>

\* FIFO goes on after an Unresolved result: another serial's body posts past the attempt's anchor.
BodyAfter(k) ==
  /\ ~retired /\ BodyRule # "none" /\ st[k] = "unres" /\ ~behind[k]
  /\ behind' = [behind EXCEPT ![k] = TRUE]
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec,
                 ns, arrived, nArr, hidden, restarts, fails, hfails, retired, callTurn, pending, dcall, viol,
                 bodyOpen, early>>

\* A due image step: what the selector or a marker delivery would take this poll.
ImageDue == (\E k \in Keys : st[k] \in {"ready", "retry", "unres"}) \/ (\E g \in Groups : ns[g] = "pending")
\* The image poll settles a restored body Prepared; "early" marks a settlement no image step needed.
SelectSettleBody ==
  /\ ~retired /\ bodyOpen /\ (SelectSettle = "always" \/ ImageDue)
  /\ bodyOpen' = FALSE /\ early' = (early \/ ~ImageDue)
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec,
                 ns, arrived, nArr, hidden, restarts, fails, hfails, retired, callTurn, pending, dcall, viol, behind>>

NoticePrepare(g) ==
  /\ ~retired /\ ns[g] = "pending" /\ ~bodyOpen
  /\ NoticeOpen = {}
  /\ (SharedPrepared => \A k \in Keys : st[k] # "prepared")
  /\ ns' = [ns EXCEPT ![g] = "prepared"]
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec,
                 arrived, nArr, hidden, restarts, fails, hfails, retired, callTurn, pending, dcall, viol, behind, bodyOpen, early>>
NoticePost(g) ==
  /\ ~retired /\ ns[g] = "prepared"
  /\ \/ nArr' = [nArr EXCEPT ![g] = @ + 1] /\ ns' = [ns EXCEPT ![g] = "posted"] /\ UNCHANGED fails
     \/ /\ fails < MaxFail /\ fails' = fails + 1     \* arrived, crash/response lost before the result
        /\ nArr' = [nArr EXCEPT ![g] = @ + 1] /\ ns' = [ns EXCEPT ![g] = "nunknown"]
     \/ /\ fails < MaxFail /\ fails' = fails + 1 /\ ns' = [ns EXCEPT ![g] = "nunknown"] /\ UNCHANGED nArr
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec,
                 arrived, hidden, restarts, hfails, retired, callTurn, pending, dcall, viol, behind, bodyOpen, early>>
NoticeSettle(g) ==
  /\ ~retired /\ ns[g] = "nunknown"
  /\ ns' = [ns EXCEPT ![g] = IF nArr[g] >= 1 THEN "posted" ELSE "nf"]
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec,
                 arrived, nArr, hidden, restarts, fails, hfails, retired, callTurn, pending, dcall, viol, behind, bodyOpen, early>>
\* Selector: a notice whose latest attempt is NotFound gets a new attempt (bypasses Settled).
NoticeRedo(g) ==
  /\ ~retired /\ NoticeRetry /\ ns[g] = "nf"
  /\ ns' = [ns EXCEPT ![g] = "pending"]
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec,
                 arrived, nArr, hidden, restarts, fails, hfails, retired, callTurn, pending, dcall, viol, behind, bodyOpen, early>>

Restart ==
  /\ ~retired /\ restarts < MaxRestarts
  /\ restarts' = restarts + 1
  /\ cursor' = rstart - 1                        \* replay every retained frame
  /\ ts' = (IF rstart = 1 THEN "idle" ELSE "unknown") /\ openPos' = 0
  /\ st' = [k \in Keys |-> IF st[k] = "ready" THEN "none" ELSE st[k]]
  /\ keyRec' = [k \in Keys |-> IF st[k] = "ready" THEN 0 ELSE keyRec[k]]
  /\ unknown' = [k \in Keys |-> st[k] = "prepared" \/ (unknown[k] /\ st[k] \notin {"ready", "retry"})]
  /\ ns' = [g \in Groups |-> IF ns[g] = "prepared" THEN "nunknown" ELSE ns[g]]
  /\ callTurn' = "none" /\ pending' = FALSE
  /\ UNCHANGED <<saved, rstart, spanFrom, spanTo, att, arrived, nArr, hidden, fails, hfails, retired, dcall, viol, behind, bodyOpen, early>>

OpenWork == cursor < N \/ (\E k \in Keys : st[k] \in {"ready", "retry", "prepared", "unres", "ambig"})
            \/ (\E g \in Groups : ns[g] \in {"pending", "prepared", "nunknown", "nf"})
Retire ==
  /\ ~retired /\ ~OpenWork /\ retired' = TRUE
  /\ UNCHANGED <<cursor, saved, rstart, ts, openPos, spanFrom, spanTo, st, unknown, att, keyRec,
                 ns, arrived, nArr, hidden, restarts, fails, hfails, callTurn, pending, dcall, viol, behind, bodyOpen, early>>

Next == Derive \/ SaveCursor \/ Gc \/ Restart \/ Retire \/ SelectSettleBody
        \/ (\E k \in Keys : Prepare(k) \/ Post(k) \/ Settle(k) \/ Reconfirm(k) \/ BodyAfter(k))
        \/ (\E g \in Groups : NoticePrepare(g) \/ NoticePost(g) \/ NoticeSettle(g) \/ NoticeRedo(g))

Spec == Init /\ [][Next]_vars /\ WF_vars(Derive) /\ WF_vars(SaveCursor) /\ WF_vars(SelectSettleBody)
        /\ \A k \in Keys : WF_vars(Prepare(k)) /\ WF_vars(Post(k)) /\ WF_vars(Settle(k)) /\ WF_vars(Reconfirm(k))
        /\ \A g \in Groups : WF_vars(NoticePrepare(g)) /\ WF_vars(NoticePost(g))
                             /\ WF_vars(NoticeSettle(g)) /\ WF_vars(NoticeRedo(g))

Noticed(p) == \E g \in Groups : PairOf(g) = p /\ nArr[g] >= 1
Accounted(p) == arrived[p] >= 1 \/ Noticed(p)   \* duplicates are AtMostOnce's job
Quiescent == retired \/ ~OpenWork

AtMostOnce == \A p \in Pairs : arrived[p] <= 1
NoticeAtMostOnce == \A g \in Groups : nArr[g] <= 1
OutcomeExclusive == \A g \in Groups : ~(arrived[PairOf(g)] >= 1 /\ nArr[g] >= 1)
NoSilentLoss == Quiescent => \A p \in ReqPairs : Accounted(p)
PlainUntouched == \A k \in Keys : k[2] \notin Imgs \cup {"L"} => st[k] = "none"
AmbigKept == \A k \in Keys : st[k] = "ambig" => ~retired
\* INVARIANT: evidence of two posts never ends as an omission notice.
AmbiguousNeverOmitted == \A g \in Groups : ~(arrived[PairOf(g)] >= 2 /\ nArr[g] >= 1)
\* INVARIANT: images and notices share the ledger's single unresolved Prepared.
SinglePrepared == Cardinality({k \in Keys : st[k] = "prepared"}) + Cardinality(NoticeOpen) <= 1
\* INVARIANT: an open obligation's source record is still retained.
AmbigBytesKept == /\ \A k \in Keys : st[k] \in {"ready", "retry", "prepared", "unres", "ambig"} => keyRec[k] >= rstart
                  /\ \A g \in Groups : ns[g] \in {"pending", "prepared", "nunknown", "nf"} => g >= rstart
\* INVARIANT: no image derivation opens a span inside another turn's span (ledger violation).
NoWrongOpened == ~viol
\* INVARIANT: every image key names the turn the image really belongs to.
AttributionTrue == \A k \in Keys : st[k] # "none" => k[1] = TT(keyRec[k])
\* INVARIANT: an image-less poll never settles a body's open Prepared before its own delivery.
NoImagelessSettle == ~early
NeverAmbig == \A k \in Keys : st[k] # "ambig"
EventuallyAccounted == <>[](\A p \in ReqPairs : Accounted(p))

RecA == <<"o1", "h1", "h1", "i", "o2", "h1">>
RecB == <<"o1", "h1", "e", "o2", "h1", "h1">>
RecC == <<"o1", "b1", "b2", "h1", "i">>
RecD == <<"o1", "h1", "i">>
RecE == <<"o1", "h1", "h1", "e", "o2", "h1", "h1">>
RecF == <<"o1", "b1", "h1", "e", "o2">>
RecMid == <<"o1", "h1", "p", "h1", "i", "o2", "h1">>
RecLate == <<"o1", "c", "h1", "i", "r">>
\* S2b w4: the call's turn ends without images; the next turn posts h1 first, then the late
\* result "s" of the old call carries the same h1.
RecLateSame == <<"o1", "c", "i", "o2", "h1", "s">>
RecForeign == <<"o1", "h1", "x", "h1", "i">>
====
