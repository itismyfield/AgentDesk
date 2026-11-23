---------------------------- MODULE InputLedgerFormat ----------------------------
\* Production base ab2d0dcb81 (#8343 slice 4), assuming slice 4 repair 1 (proof reads the V2 view).
\* One shared frame ledger file (`runtime/injected_frames.json`) written by two kinds of writer:
\*   - the V2 input owner (`first_input.rs:101 prepare_history` -> `v2.rs:260 transaction`), and
\*   - a writer outside any V2 attempt scope (path A `inject.rs:491` / path B `queued_owner.rs`),
\*     which today registers through V1 `history.rs:177 update()`.
\* Inputs: "o" enters through the V2 owner; "x" enters through the out-of-scope writer.
\* "x" refused before any paste (`NotSent(load_failed)`) is handed back to the queue front
\* (`inject.rs:708-731`) and then delivered by the owner like "o".
\*
\* Ranges come from the entry census only:
\*   fmt      none | v1 | v2          file missing / `{"entries":..}` / `{"version":2|3,..}`
\*   Writer   v1 | v2                 out-of-scope register: V1 update (now) or V2 transaction (5b)
\*   Migrate  BOOLEAN                 `v2.rs:299 migration::restore` of a V1 file (now TRUE)
\*   View     v1 | v2                 mend proof read: `history.rs:342,894 read()` (before repair 1)
\*                                    or `readable()` -> `v2::unscoped_store` -> `owned()` (after)
\*   Boot     none | v1 | v2          ledger file left on disk when this binary starts
\*   Locked   BOOLEAN                 out-of-scope read-modify-write under the shared `.lock`
\*                                    (`history.rs:181-188`, `v2.rs:270-277`)
\*   Restart  BOOLEAN                 a daemon restart between paste and settle leaves "x" Unknown,
\*                                    proven later by `queued_reconcile::mend` (`:81`, `nonce_in :335`)
EXTENDS Naturals

CONSTANTS Writer, Migrate, View, Boot, Locked, Restart

Inputs == {"o", "x"}

VARIABLES fmt, rec, st, held, copy

vars == <<fmt, rec, st, held, copy>>

States == {"queued", "owner", "pasted", "unknown", "done"}

TypeOK ==
    /\ fmt \in {"none", "v1", "v2"}
    /\ rec \in [Inputs -> 0..3]
    /\ st \in [Inputs -> States]
    /\ held \in BOOLEAN
    /\ copy \in [f : {"none", "v1", "v2"}, r : [Inputs -> 0..3]]

Init ==
    /\ fmt = Boot
    /\ rec = [i \in Inputs |-> 0]
    /\ st = [i \in Inputs |-> IF i = "o" THEN "owner" ELSE "queued"]
    /\ held = FALSE
    /\ copy = [f |-> Boot, r |-> [i \in Inputs |-> 0]]

\* A V2 transaction can open the file: missing (fresh snapshot), V2, or V1 only through restore.
V2Opens(f) == f \in {"none", "v2"} \/ (f = "v1" /\ Migrate)

\* What the out-of-scope writer does on the file it read, as (format after, registered?).
\* V1 update refuses a V2 file (`compat.rs:98 read_legacy`); a V2 transaction refuses what V2Opens refuses.
XAccepts(f) == IF Writer = "v1" THEN f \in {"none", "v1"} ELSE V2Opens(f)
XFormat(f) == IF Writer = "v1" THEN "v1" ELSE "v2"

\* --- out-of-scope writer, one atomic step under the shared lock -------------------------------
XRegister ==
    /\ Locked
    /\ st["x"] = "queued"
    /\ IF XAccepts(fmt)
          THEN /\ fmt' = XFormat(fmt)
               /\ rec' = [rec EXCEPT !["x"] = @ + 1]
               /\ st' = [st EXCEPT !["x"] = "pasted"]
          ELSE /\ st' = [st EXCEPT !["x"] = "owner"]   \* NotSent before any key: handed back
               /\ UNCHANGED <<fmt, rec>>
    /\ UNCHANGED <<held, copy>>

\* --- mutant: the same writer reads, then writes its copy back outside the shared lock --------
XRead ==
    /\ ~Locked
    /\ st["x"] = "queued"
    /\ ~held
    /\ held' = TRUE
    /\ copy' = [f |-> fmt, r |-> rec]
    /\ UNCHANGED <<fmt, rec, st>>

XWrite ==
    /\ ~Locked
    /\ st["x"] = "queued"
    /\ held
    /\ IF XAccepts(copy.f)
          THEN /\ fmt' = XFormat(copy.f)
               /\ rec' = [copy.r EXCEPT !["x"] = @ + 1]
               /\ st' = [st EXCEPT !["x"] = "pasted"]
          ELSE /\ st' = [st EXCEPT !["x"] = "owner"]
               /\ UNCHANGED <<fmt, rec>>
    /\ held' = FALSE
    /\ UNCHANGED copy

\* --- the V2 owner: prepare_history registers, then the leaf pastes --------------------------
\* A transaction that cannot open the file returns Err and writes nothing (`v2.rs:299`), so the
\* owner's attempt stays NotSent and is retried; the step is not enabled then.
OwnerPrepare(i) ==
    /\ st[i] = "owner"
    /\ V2Opens(fmt)
    /\ fmt' = "v2"
    /\ rec' = [rec EXCEPT ![i] = @ + 1]
    /\ st' = [st EXCEPT ![i] = "pasted"]
    /\ UNCHANGED <<held, copy>>

Settle(i) ==
    /\ st[i] = "pasted"
    /\ st' = [st EXCEPT ![i] = "done"]
    /\ UNCHANGED <<fmt, rec, held, copy>>

\* Only the out-of-scope queued head is settled by transcript proof after a restart.
Crash ==
    /\ Restart
    /\ st["x"] = "pasted"
    /\ st' = [st EXCEPT !["x"] = "unknown"]
    /\ UNCHANGED <<fmt, rec, held, copy>>

\* The proof finds the recorded attempt only through a view that reads the file's format.
Visible(i) == rec[i] >= 1 /\ (View = "v2" \/ fmt \in {"none", "v1"})

Mend ==
    /\ st["x"] = "unknown"
    /\ Visible("x")
    /\ st' = [st EXCEPT !["x"] = "done"]
    /\ UNCHANGED <<fmt, rec, held, copy>>

Idle == UNCHANGED vars

Next ==
    \/ XRegister \/ XRead \/ XWrite
    \/ \E i \in Inputs : OwnerPrepare(i) \/ Settle(i)
    \/ Crash \/ Mend
    \/ Idle

Spec ==
    /\ Init /\ [][Next]_vars
    /\ WF_vars(XRegister) /\ WF_vars(XRead) /\ WF_vars(XWrite)
    /\ \A i \in Inputs : WF_vars(OwnerPrepare(i)) /\ WF_vars(Settle(i))
    /\ WF_vars(Mend)

\* INVARIANT: a pasted input has exactly one record; nothing pasted is left without its record.
RecordedExactlyOnce ==
    \A i \in Inputs : st[i] \in {"pasted", "unknown", "done"} => rec[i] = 1

\* INVARIANT: no input is ever recorded twice.
RecordedAtMostOnce == \A i \in Inputs : rec[i] <= 1

\* PROPERTY (liveness): the owner never stalls; every input is eventually done.
OwnerProgress == <>(\A i \in Inputs : st[i] = "done")
=============================================================================
