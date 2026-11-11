-------------------------- MODULE IntakeConsent --------------------------
\* Code SHA: 08e2336e47986d70d0cbe95e56215858377aed5c.
\* Current/Proposed preserve Issue, Take and by-value handoff/rollback timing.
\* Proposed represents the successful spawned CAS by an opaque, linear ticket.
EXTENDS Naturals
CONSTANTS Boundary, Mutant
ASSUME Boundary \in {"current", "proposed"}
ASSUME Mutant \in {"none", "forge", "reuse", "stale", "bookkeeping", "owner-only"}
Workers == 1..2
EmptyRow == [status |-> "pending", payload |-> "original", source |-> "original",
             claim |-> 0, generation |-> 1, marker |-> "none",
             retry |-> 3, completed |-> 7, created |-> 11]
VARIABLES row, live, pc, snap, ticket, proof, spend, op, observed, cas, attempted,
          ownerLock, rowLock, txn, changed, bad, rearms, releases,
          drift, restarted, fs
vars == <<row, live, pc, snap, ticket, proof, spend, op, observed, cas, attempted,
          ownerLock, rowLock, txn, changed, bad, rearms, releases,
          drift, restarted, fs>>
Init ==
    /\ row = EmptyRow /\ live = TRUE
    /\ pc = [w \in Workers |-> "idle"] /\ snap = [w \in Workers |-> EmptyRow]
    /\ ticket = [w \in Workers |-> FALSE]
    /\ cas = [w \in Workers |-> FALSE] /\ attempted = [w \in Workers |-> FALSE]
    /\ proof = [w \in Workers |-> "unissued"] /\ spend = [w \in Workers |-> 0]
    /\ op = [w \in Workers |-> "none"] /\ observed = [w \in Workers |-> FALSE]
    /\ ownerLock = 0 /\ rowLock = 0 /\ txn = [w \in Workers |-> EmptyRow]
    /\ changed = [w \in Workers |-> FALSE]
    /\ bad = [forge |-> FALSE, stale |-> FALSE, rollback |-> FALSE]
    /\ rearms = 0 /\ releases = 0 /\ drift = FALSE /\ restarted = FALSE /\ fs = TRUE
Claim(w) ==
    /\ pc[w] = "idle" /\ row.status = "pending" /\ live /\ rowLock = 0 /\ ownerLock = 0
    /\ row' = [row EXCEPT !.status = "claimed", !.claim = w]
    /\ snap' = [snap EXCEPT ![w] = row'] /\ pc' = [pc EXCEPT ![w] = "claimed"]
    /\ UNCHANGED <<cas, attempted, live, ticket, proof, spend, op, observed, ownerLock, rowLock,
                   txn, changed, bad, rearms, releases, drift, restarted, fs>>
SameStamp(w) == row.claim = snap[w].claim /\ row.generation = snap[w].generation
AdvanceStatus(w, from, dest) ==
    /\ pc[w] = from /\ row.status = from /\ SameStamp(w) /\ live
    /\ ownerLock = 0 /\ rowLock = 0
    /\ row' = [row EXCEPT !.status = dest] /\ pc' = [pc EXCEPT ![w] = dest]
    /\ ticket' = [ticket EXCEPT ![w] = (dest = "spawned")]
    /\ cas' = [cas EXCEPT ![w] = @ \/ (dest = "spawned")]
    /\ UNCHANGED <<attempted, live, snap, proof, spend, op, observed, ownerLock, rowLock,
                   txn, changed, bad, rearms, releases, drift, restarted, fs>>
AbortStale(w) ==
    /\ pc[w] \in {"claimed", "accepted"}
    /\ ~(row.status = pc[w] /\ SameStamp(w) /\ live)
    /\ pc' = [pc EXCEPT ![w] = "done"]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, proof, spend, op, observed,
                   ownerLock, rowLock, txn, changed, bad, rearms, releases,
                   drift, restarted, fs>>
Issue(w) ==
    /\ pc[w] = "spawned" /\ ticket[w] /\ proof[w] = "unissued"
    /\ pc' = [pc EXCEPT ![w] = "scope"] /\ proof' = [proof EXCEPT ![w] = "slot"]
    /\ ticket' = [ticket EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<cas, attempted, row, live, snap, spend, op, observed, ownerLock, rowLock,
                   txn, changed, bad, rearms, releases, drift, restarted, fs>>
Take(w, matches) ==
    /\ pc[w] = "scope" /\ proof[w] = "slot"
    /\ pc' = [pc EXCEPT ![w] = IF matches THEN "entry" ELSE "done"]
    /\ proof' = [proof EXCEPT ![w] = IF matches THEN "held" ELSE "spent"]
    /\ spend' = [spend EXCEPT ![w] = @ + IF matches THEN 0 ELSE 1]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, op, observed, ownerLock, rowLock,
                   txn, changed, bad, rearms, releases, drift, restarted, fs>>
DropNormal(w) ==
    /\ pc[w] = "entry" /\ proof[w] = "held"
    /\ pc' = [pc EXCEPT ![w] = "done"] /\ proof' = [proof EXCEPT ![w] = "spent"]
    /\ spend' = [spend EXCEPT ![w] = @ + 1]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, op, observed, ownerLock, rowLock,
                   txn, changed, bad, rearms, releases, drift, restarted, fs>>
RecordAttempt(w) ==
    /\ pc[w] = "entry" /\ proof[w] = "held"
    /\ pc' = [pc EXCEPT ![w] = "recording"] /\ op' = [op EXCEPT ![w] = "hold"]
    /\ attempted' = [attempted EXCEPT ![w] = TRUE]
    /\ UNCHANGED <<cas, row, live, snap, ticket, proof, spend, observed, ownerLock,
                   rowLock, txn, changed, bad, rearms, releases, drift, restarted, fs>>
\* A record attempt may already have published. Even load failure cannot roll it back.
RecordError(w) ==
    /\ pc[w] = "recording" /\ proof[w] = "held"
    /\ pc' = [pc EXCEPT ![w] = "done"] /\ proof' = [proof EXCEPT ![w] = "spent"]
    /\ spend' = [spend EXCEPT ![w] = @ + 1]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, op, observed, ownerLock,
                   rowLock, txn, changed, bad, rearms, releases, drift, restarted, fs>>
ObserveAndMove(w) ==
    /\ pc[w] = "recording" /\ proof[w] = "held"
    /\ observed' = [observed EXCEPT ![w] = fs]
    /\ proof' = [proof EXCEPT ![w] = "spent"] /\ spend' = [spend EXCEPT ![w] = @ + 1]
    /\ pc' = [pc EXCEPT ![w] = IF fs THEN "wantLock" ELSE "done"]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, op, ownerLock, rowLock, txn, changed,
                   bad, rearms, releases, drift, restarted, fs>>
RollbackMove(w) ==
    /\ pc[w] = "entry" /\ proof[w] = "held"
    /\ op' = [op EXCEPT ![w] = "rollback"] /\ pc' = [pc EXCEPT ![w] = "wantLock"]
    /\ proof' = [proof EXCEPT ![w] = "spent"] /\ spend' = [spend EXCEPT ![w] = @ + 1]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, observed, ownerLock, rowLock,
                   txn, changed, bad, rearms, releases, drift, restarted, fs>>
LockOwner(w) ==
    /\ pc[w] = "wantLock" /\ ownerLock = 0
    /\ ownerLock' = w /\ pc' = [pc EXCEPT ![w] = "ownerLocked"]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, proof, spend, op, observed, rowLock,
                   txn, changed, bad, rearms, releases, drift, restarted, fs>>
ReadForUpdate(w) ==
    /\ pc[w] = "ownerLocked" /\ ownerLock = w /\ rowLock = 0
    /\ rowLock' = w /\ pc' = [pc EXCEPT ![w] = "read"]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, proof, spend, op, observed, ownerLock,
                   txn, changed, bad, rearms, releases, drift, restarted, fs>>
Exact(w) == row.status = "spawned" /\ SameStamp(w)
            /\ row.payload = snap[w].payload /\ row.source = snap[w].source
Guard(w) ==
    /\ (Mutant = "stale" \/ Exact(w))
    /\ IF op[w] = "rollback"
       THEN row.marker = "none" /\ live
       ELSE observed[w] /\ row.marker \in {"none", "rearm"}

PrepareCAS(w) ==
    /\ pc[w] = "read" /\ rowLock = w /\ ownerLock = w
    /\ changed' = [changed EXCEPT ![w] = Guard(w)]
    /\ txn' = [txn EXCEPT ![w] = IF ~Guard(w) THEN row
              ELSE IF op[w] = "hold" THEN [row EXCEPT !.marker = "held"]
              ELSE [row EXCEPT !.status = "pending", !.claim = 0, !.marker = "rearm",
                    !.retry = IF Mutant = "bookkeeping" THEN @ + 1 ELSE @]]
    /\ pc' = [pc EXCEPT ![w] = "commit"]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, proof, spend, op, observed, ownerLock,
                   rowLock, bad, rearms, releases, drift, restarted, fs>>
Commit(w) ==
    /\ pc[w] = "commit" /\ rowLock = w /\ ownerLock = w
    /\ bad' = [bad EXCEPT !.stale = @ \/ (changed[w] /\ ~(Exact(w) /\ live)),
               !.rollback = @ \/ (changed[w] /\ op[w] = "rollback" /\
                   (txn[w].retry # row.retry \/ txn[w].completed # row.completed \/
                    txn[w].created # row.created \/ row.marker # "none" \/ attempted[w]))]
    /\ row' = txn[w] /\ ownerLock' = 0 /\ rowLock' = 0
    /\ rearms' = rearms + IF changed[w] /\ op[w] = "rollback" THEN 1 ELSE 0
    /\ pc' = [pc EXCEPT ![w] = "done"]
    /\ UNCHANGED <<cas, attempted, live, snap, ticket, proof, spend, op, observed, txn, changed,
                   releases, drift, restarted, fs>>
\* Receipt identity, exact held payload/stamp and live owner are abstracted together.
Release ==
    /\ row.marker = "held" /\ row.status = "spawned" /\ live
    /\ ownerLock = 0 /\ rowLock = 0
    /\ \E w \in Workers : row.claim = snap[w].claim /\ row.payload = snap[w].payload
                          /\ row.source = snap[w].source /\ row.generation = snap[w].generation
    /\ row' = [row EXCEPT !.status = "pending", !.claim = 0, !.marker = "released"]
    /\ releases' = releases + 1
    /\ UNCHANGED <<cas, attempted, live, pc, snap, ticket, proof, spend, op, observed, ownerLock,
                   rowLock, txn, changed, bad, rearms, drift, restarted, fs>>
\* Existing promote retires open rows before owner transfer in the SAME transaction.
\* owner-only removes that coupling; other interference is bounded to one event.
Drift(kind) ==
    /\ ~drift /\ ownerLock = 0 /\ rowLock = 0
    /\ row.status \in {"claimed", "accepted", "spawned"}
    /\ row' = CASE kind = "retire" /\ Mutant # "owner-only" -> [row EXCEPT !.status = "failed"]
                   [] kind = "marker" -> [row EXCEPT !.marker = "foreign"]
                   [] kind = "payload" -> [row EXCEPT !.payload = "changed"]
                   [] kind = "source" -> [row EXCEPT !.source = "changed"]
                   [] kind = "claim" -> [row EXCEPT !.claim = 3]
                   [] kind = "generation" -> [row EXCEPT !.generation = 2]
                   [] kind = "forceFail" -> [row EXCEPT !.status = "failed"]
                   [] OTHER -> row
    /\ live' = (IF kind = "retire" THEN FALSE ELSE live)
    /\ drift' = TRUE
    /\ UNCHANGED <<cas, attempted, pc, snap, ticket, proof, spend, op, observed, ownerLock,
                   rowLock, txn, changed, bad, rearms, releases, restarted, fs>>
ChangeConsent ==
    /\ fs /\ fs' = FALSE
    /\ UNCHANGED <<cas, attempted, row, live, pc, snap, ticket, proof, spend, op, observed,
                   ownerLock, rowLock, txn, changed, bad, rearms, releases, drift, restarted>>
\* Process loss aborts uncommitted tx, drops in-memory authority, preserves durable marker.
Restart ==
    /\ ~restarted /\ restarted' = TRUE
    /\ pc' = [w \in Workers |-> IF pc[w] = "idle" THEN "idle" ELSE "done"]
    /\ spend' = [w \in Workers |-> spend[w] + IF proof[w] \in {"slot", "held"} THEN 1 ELSE 0]
    /\ proof' = [w \in Workers |-> IF proof[w] \in {"slot", "held"} THEN "spent" ELSE proof[w]]
    /\ ticket' = [w \in Workers |-> FALSE] /\ ownerLock' = 0 /\ rowLock' = 0
    /\ UNCHANGED <<cas, attempted, row, live, snap, op, observed, txn, changed, bad, rearms, releases, drift, fs>>
Forge ==
    /\ Mutant = "forge" /\ proof[1] = "unissued" /\ ~cas[1]
    /\ proof' = [proof EXCEPT ![1] = "slot"] /\ pc' = [pc EXCEPT ![1] = "scope"]
    /\ UNCHANGED <<cas, attempted, row, live, snap, ticket, spend, op, observed,
                   ownerLock, rowLock, txn, changed, bad, rearms, releases, drift, restarted, fs>>
Reuse(w) ==
    /\ Mutant = "reuse" /\ proof[w] = "spent" /\ spend[w] = 1
    /\ spend' = [spend EXCEPT ![w] = 2]
    /\ UNCHANGED <<cas, attempted, row, live, pc, snap, ticket, proof, op, observed,
                   ownerLock, rowLock, txn, changed, bad, rearms, releases, drift, restarted, fs>>
Advance(w) == AdvanceStatus(w, "claimed", "accepted") \/ AdvanceStatus(w, "accepted", "spawned")
              \/ AbortStale(w) \/ Issue(w) \/ Take(w, TRUE) \/ Take(w, FALSE)
              \/ DropNormal(w) \/ RecordAttempt(w) \/ RecordError(w) \/ ObserveAndMove(w) \/ RollbackMove(w)
              \/ LockOwner(w) \/ ReadForUpdate(w) \/ PrepareCAS(w) \/ Commit(w)
Next == (\E w \in Workers : Claim(w) \/ Advance(w) \/ Reuse(w)) \/ Release \/ Restart
        \/ (\E k \in {"payload", "source", "claim", "generation", "forceFail", "retire", "marker"} : Drift(k))
        \/ ChangeConsent \/ Forge
Spec == Init /\ [][Next]_vars /\ (\A w \in Workers : WF_vars(Advance(w)))
        /\ WF_vars(Release)
TypeOK == row.status \in {"pending", "claimed", "accepted", "spawned", "failed"}
          /\ ownerLock \in 0..2 /\ rowLock \in 0..2 /\ spend \in [Workers -> 0..2]
NoUnauthorizedIssue == \A w \in Workers : proof[w] # "unissued" => cas[w]
ProofConsumedOnce == \A w \in Workers : spend[w] <= 1
    /\ (proof[w] = "spent" => spend[w] = 1)
    /\ (proof[w] \in {"slot", "held"} => spend[w] = 0)
NoStaleProofWrite == ~bad.stale
RollbackExactAndBookkeeping == ~bad.rollback /\ rearms <= 1
ReleaseOnce == releases <= 1
ProofEventuallySettles == \A w \in Workers : (proof[w] \in {"slot", "held"}) ~> (proof[w] = "spent")
TransactionEventuallyEnds == \A w \in Workers : (pc[w] \in {"wantLock", "ownerLocked", "read", "commit"}) ~> (pc[w] = "done")
=============================================================================
