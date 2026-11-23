---------------------------- MODULE InputLedgerFormat ----------------------------
\* Production base 6b91594c5c (#8343 slice 4 repair 1: the scope-free proof reads the V2 view).
\* One shared frame ledger file (`runtime/injected_frames.json`) written by two kinds of writer:
\*   - the V2 input owner (`first_input.rs:101 prepare_history` -> `v2.rs:260 transaction`), and
\*   - a writer outside any V2 attempt scope (path A `inject.rs:491` / path B `queued_owner.rs`),
\*     which today registers through V1 `history.rs:177 update()`.
\* Inputs: "o" enters through the V2 owner; "x" enters through the out-of-scope writer.
\* "x" refused before any paste (`NotSent(load_failed)`) is handed back and then delivered by
\* the owner like "o" (an adoption hold that keeps it in the legacy mailbox is not modeled).
\*
\* ROLE: this model checks three things only - which envelope each writer can open (format
\* change), whether the recorded attempt stays visible to the scope-free proof (proof
\* visibility), and a stale overwrite by a read-modify-write outside the shared lock. With
\* Deploy # "none" it also checks the slice 5 ops procedure: where the final format check sits
\* relative to stopping the producers. It does NOT check C4/C5 (keyed/keyless reconciliation,
\* native-row uniqueness); those are proven by concurrent-transaction tests in the implementation.
\* RecordedAtMostOnce holds because each input registers once by construction, not by dedupe.
\*
\* Ranges come from the entry census only:
\*   fmt      none | v1 | v2          file missing / `{"entries":..}` / `{"version":2|3,..}`
\*   Writer   v1 | v2                 out-of-scope register: V1 update (now) or V2 transaction (5b)
\*   Migrate  BOOLEAN                 `v2.rs:299 migration::restore` of a V1 file (now TRUE)
\*   View     v1 | v2                 mend proof read: V1 `read()` (before repair 1) or
\*                                    `readable()` -> `v2::unscoped_store` -> `owned()` (`history.rs:343,895`)
\*   Boot     none | v1 | v2          ledger file on disk when the (first) binary starts
\*   Locked   BOOLEAN                 out-of-scope read-modify-write under the shared `.lock`
\*                                    (`history.rs:181-188`, `v2.rs:270-277`)
\*   Restart  BOOLEAN                 a daemon restart between paste and settle leaves "x" Unknown,
\*                                    proven later by `queued_reconcile::mend` (`:81`, `nonce_in :335`)
\*   Deploy   none | precheck | stopcheck | bakunchecked
\*            none: one binary, Writer/Migrate as given.
\*            Otherwise the old binary (V1 writer, restore, View) runs, is checked, stopped and
\*            replaced by the slice 5b binary (V2 writer, no restore, V2 view):
\*            precheck     the format is checked while the old producer still runs (design rev 0 §7)
\*            stopcheck    final check after the stop; a V1 file with an open proof obligation
\*                         goes back to the old binary, otherwise it is archived (.bak)
\*            bakunchecked mutant: the final check archives a V1 file without the obligation check
\*            A 5b binary that boots on a V1 file stops its Claude runtime before intake
\*            (`native.rs:199` -> `native_input.rs:15-20` -> `runtime_bootstrap.rs:299-301`).
EXTENDS Naturals

CONSTANTS Writer, Migrate, View, Boot, Locked, Restart, Deploy

Inputs == {"o", "x"}

VARIABLES fmt, rec, arch, st, held, copy, bin, plan, bootfmt

vars == <<fmt, rec, arch, st, held, copy, bin, plan, bootfmt>>

States == {"queued", "owner", "pasted", "unknown", "done"}
Bins == {"run", "old", "checked", "stopped", "final", "new"}

TypeOK ==
    /\ fmt \in {"none", "v1", "v2"}
    /\ rec \in [Inputs -> 0..3]
    /\ arch \in [Inputs -> 0..3]
    /\ st \in [Inputs -> States]
    /\ held \in BOOLEAN
    /\ copy \in [f : {"none", "v1", "v2"}, r : [Inputs -> 0..3]]
    /\ bin \in Bins
    /\ plan \in {"keep", "bak"}
    /\ bootfmt \in {"none", "v1", "v2"}

Init ==
    /\ fmt = Boot
    /\ rec = [i \in Inputs |-> 0]
    /\ arch = [i \in Inputs |-> 0]
    /\ st = [i \in Inputs |-> IF i = "o" THEN "owner" ELSE "queued"]
    /\ held = FALSE
    /\ copy = [f |-> Boot, r |-> [i \in Inputs |-> 0]]
    /\ bin = IF Deploy = "none" THEN "run" ELSE "old"
    /\ plan = "keep"
    /\ bootfmt = Boot

New == bin = "new"
EffWriter == IF Deploy = "none" THEN Writer ELSE IF New THEN "v2" ELSE "v1"
EffMigrate == IF Deploy = "none" THEN Migrate ELSE ~New
EffView == IF Deploy # "none" /\ New THEN "v2" ELSE View

\* A process that writes the ledger is up; a 5b binary booted on V1 never opens intake.
Running == bin \in {"run", "old", "checked", "new"} /\ ~(New /\ bootfmt = "v1")

\* A V2 transaction can open the file: missing (fresh snapshot), V2, or V1 only through restore.
V2Opens(f) == f \in {"none", "v2"} \/ (f = "v1" /\ EffMigrate)

\* V1 update refuses a V2 file (`compat.rs:98 read_legacy`); a V2 transaction refuses what V2Opens refuses.
XAccepts(f) == IF EffWriter = "v1" THEN f \in {"none", "v1"} ELSE V2Opens(f)
XFormat(f) == IF EffWriter = "v1" THEN "v1" ELSE "v2"

Frozen == UNCHANGED <<arch, bin, plan, bootfmt>>

\* --- out-of-scope writer, one atomic step under the shared lock -------------------------------
XRegister ==
    /\ Running
    /\ Locked
    /\ st["x"] = "queued"
    /\ IF XAccepts(fmt)
          THEN /\ fmt' = XFormat(fmt)
               /\ rec' = [rec EXCEPT !["x"] = @ + 1]
               /\ st' = [st EXCEPT !["x"] = "pasted"]
          ELSE /\ st' = [st EXCEPT !["x"] = "owner"]   \* NotSent before any key: handed back
               /\ UNCHANGED <<fmt, rec>>
    /\ UNCHANGED <<held, copy>>
    /\ Frozen

\* --- mutant: the same writer reads, then writes its copy back outside the shared lock --------
XRead ==
    /\ Running
    /\ ~Locked
    /\ st["x"] = "queued"
    /\ ~held
    /\ held' = TRUE
    /\ copy' = [f |-> fmt, r |-> rec]
    /\ UNCHANGED <<fmt, rec, st>>
    /\ Frozen

XWrite ==
    /\ Running
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
    /\ Frozen

\* --- the V2 owner: prepare_history registers, then the leaf pastes --------------------------
\* A transaction that cannot open the file returns Err and writes nothing (`v2.rs:299`), so the
\* owner's attempt stays NotSent and is retried; the step is not enabled then.
OwnerPrepare(i) ==
    /\ Running
    /\ st[i] = "owner"
    /\ V2Opens(fmt)
    /\ fmt' = "v2"
    /\ rec' = [rec EXCEPT ![i] = @ + 1]
    /\ st' = [st EXCEPT ![i] = "pasted"]
    /\ UNCHANGED <<held, copy>>
    /\ Frozen

Settle(i) ==
    /\ Running
    /\ st[i] = "pasted"
    /\ st' = [st EXCEPT ![i] = "done"]
    /\ UNCHANGED <<fmt, rec, held, copy>>
    /\ Frozen

\* Only the out-of-scope queued head is settled by transcript proof after a restart.
Crash ==
    /\ Running
    /\ Restart
    /\ st["x"] = "pasted"
    /\ st' = [st EXCEPT !["x"] = "unknown"]
    /\ UNCHANGED <<fmt, rec, held, copy>>
    /\ Frozen

\* The proof finds the recorded attempt only through a view that reads the file's format.
Visible(i) == rec[i] >= 1 /\ (EffView = "v2" \/ fmt \in {"none", "v1"})

Mend ==
    /\ Running
    /\ st["x"] = "unknown"
    /\ Visible("x")
    /\ st' = [st EXCEPT !["x"] = "done"]
    /\ UNCHANGED <<fmt, rec, held, copy>>
    /\ Frozen

\* --- the ops procedure (Deploy # "none") ----------------------------------------------------
\* Live format check: the operator plans what to do with the file at the stop.
PreCheck ==
    /\ Deploy # "none"
    /\ bin = "old"
    /\ bin' = "checked"
    /\ plan' = IF fmt = "v1" THEN "bak" ELSE "keep"
    /\ UNCHANGED <<fmt, rec, arch, st, held, copy, bootfmt>>

\* Stopping dcserver: a paste the old binary had not settled stays an Unknown queued head.
Stop ==
    /\ bin = "checked"
    /\ bin' = "stopped"
    /\ st' = [i \in Inputs |-> IF i = "x" /\ st[i] = "pasted" THEN "unknown" ELSE st[i]]
    /\ UNCHANGED <<fmt, rec, arch, held, copy, plan, bootfmt>>

Archive == /\ fmt' = "none"
           /\ arch' = [i \in Inputs |-> arch[i] + rec[i]]
           /\ rec' = [i \in Inputs |-> 0]

\* Rev 0 procedure: the plan made while running is applied at the stop; the new binary starts.
StartAsPlanned ==
    /\ Deploy = "precheck"
    /\ bin = "stopped"
    /\ IF plan = "bak" /\ fmt = "v1" THEN Archive ELSE UNCHANGED <<fmt, rec, arch>>
    /\ bin' = "new"
    /\ bootfmt' = IF plan = "bak" /\ fmt = "v1" THEN "none" ELSE fmt
    /\ UNCHANGED <<st, held, copy, plan>>

\* An Unknown head whose proof lives in this V1 file is an open obligation.
Obligation == st["x"] = "unknown" /\ rec["x"] >= 1

\* Rev 1 procedure: the final check runs with every producer stopped.
FinalCheck ==
    /\ Deploy \in {"stopcheck", "bakunchecked"}
    /\ bin = "stopped"
    /\ CASE fmt # "v1" ->
                /\ bin' = "final" /\ UNCHANGED <<fmt, rec, arch>>
         [] Deploy = "stopcheck" /\ Obligation ->
                /\ bin' = "old" /\ UNCHANGED <<fmt, rec, arch>>   \* settle on the old binary first
         [] OTHER ->
                /\ bin' = "final" /\ Archive
    /\ UNCHANGED <<st, held, copy, plan, bootfmt>>

Start ==
    /\ bin = "final"
    /\ bin' = "new"
    /\ bootfmt' = fmt
    /\ UNCHANGED <<fmt, rec, arch, st, held, copy, plan>>

Idle == UNCHANGED vars

Next ==
    \/ XRegister \/ XRead \/ XWrite
    \/ \E i \in Inputs : OwnerPrepare(i) \/ Settle(i)
    \/ Crash \/ Mend
    \/ PreCheck \/ Stop \/ StartAsPlanned \/ FinalCheck \/ Start
    \/ Idle

Spec ==
    /\ Init /\ [][Next]_vars
    \* Work steps are strongly fair: the procedure may stop and restart the binary in between.
    /\ SF_vars(XRegister) /\ SF_vars(XRead) /\ SF_vars(XWrite)
    /\ \A i \in Inputs : SF_vars(OwnerPrepare(i)) /\ SF_vars(Settle(i))
    /\ SF_vars(Mend)
    /\ WF_vars(PreCheck) /\ WF_vars(Stop) /\ WF_vars(StartAsPlanned)
    /\ WF_vars(FinalCheck) /\ WF_vars(Start)

\* INVARIANT: a pasted input has exactly one record, in the ledger or in its archived copy.
RecordedExactlyOnce ==
    \A i \in Inputs : st[i] \in {"pasted", "unknown", "done"} => rec[i] + arch[i] = 1

\* INVARIANT: no input is ever recorded twice (by construction; see ROLE).
RecordedAtMostOnce == \A i \in Inputs : rec[i] + arch[i] <= 1

\* INVARIANT: an Unknown head keeps its proof evidence in the live ledger.
ProofKept == \A i \in Inputs : st[i] = "unknown" => rec[i] = 1

\* INVARIANT: the slice 5b binary never boots on a V1 ledger.
NewBootSupported == New => bootfmt # "v1"

\* PROPERTY (liveness): the owner never stalls; every input is eventually done.
OwnerProgress == <>(\A i \in Inputs : st[i] = "done")
=============================================================================
