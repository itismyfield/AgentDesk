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
\* ROLE: this model checks only these things:
\*   - which envelope each writer can open (format change);
\*   - whether the recorded attempt stays visible to, and usable by, the scope-free proof
\*     (proof visibility: the ledger record, and the transcript binding `mend` needs);
\*   - a stale overwrite by a read-modify-write outside the shared lock;
\*   - with Deploy # "none", the slice 5 ops procedure: intake close, the stop, the final check,
\*     the session replacement, and a rollback after the new binary has written.
\* C4/C5 (keyed/keyless reconciliation, native-row uniqueness) are still to be proven by the
\* implementation's concurrent-transaction tests. RecordedAtMostOnce holds by construction.
\* OwnerProgress means every input settles; it does not mean the deploy finished (DeployReached).
\*
\* Ranges come from the entry census only:
\*   fmt      none | v1 | v2          file missing / `{"entries":..}` / `{"version":2|3,..}`
\*   Writer   v1 | v2                 out-of-scope register: V1 update (now) or V2 transaction (5b)
\*   Migrate  BOOLEAN                 `v2.rs:299 migration::restore` of a V1 file (now TRUE)
\*   View     v1 | v2                 proof read of the binary that is not 5b: V1 `read()` (main
\*                                    before slice 4, `history.rs:342,894` there) or `readable()`
\*                                    (slice 4 repair 1, `history.rs:343,895`)
\*   Boot     none | v1 | v2          ledger file on disk when the first binary starts
\*   Locked   BOOLEAN                 out-of-scope read-modify-write under the shared `.lock`
\*                                    (`history.rs:181-188`, `v2.rs:270-277`)
\*   Restart  BOOLEAN                 a daemon restart between paste and settle leaves "x" Unknown,
\*                                    proven later by `queued_reconcile::mend` (`:81`, `nonce_in :335`)
\*                                    against the session's current transcript binding
\*                                    (`queued_reconcile.rs:166-183,242-249,382-386`)
\*   Deploy   none | precheck | guarded | replaceonv2 | skipretry | skipkeyless | skipe3 | keylessall
\*            | nonceabsent | anydecision
\*            none: one binary, Writer/Migrate/View as given.
\*            Otherwise the old binary (V1 writer, restore, View) runs and is replaced by the 5b
\*            binary (V2 writer, no restore, V2 view):
\*            precheck     rev 0: a live format check, then stop, replace the session and start
\*            guarded      rev 2: close intake, settle on the original binding, stop, and only if
\*                         the stopped ledger is V2/V3/missing and no Unknown head remains,
\*                         replace the session and start; otherwise hold (old binary, same binding)
\*            replaceonv2  mutant: the final check replaces the session whenever the format is V2
\*            skipretry    mutant: the final check ignores tui_o retry debt
\*            skipkeyless  mutant: the final check ignores an unsettled keyless attempt
\*            skipe3       mutant: the final check does not require a start binary with repair 1
\*            keylessall   mutant: every keyless record, settled history included, counts as debt
\*            nonceabsent  mutant: a keyless attempt whose native row is not in the transcript passes
\*            anydecision  mutant: any non-null append decision (HandedPending included) passes
\*            Rev 4 O6 is fail-closed: a keyless attempt passes only with a terminal state in the
\*            persisted record (Codex `decision == Joined`, `append.rs:494`); Reserved, EffectPossible,
\*            Uncertain, Admitted without a result and HandedPending stay obligations, whether or not
\*            the native row reached the transcript (`kst`, `krow`).
\*            The guarded check reads only persisted state after the stop (rev 3 E2): Unknown heads,
\*            retry debt (Body and InputNotice), unsettled keyless attempts, and E3 (View = "v2").
\*            retryDebt and a keyless attempt can appear while the old producer still runs, also after the
\*            live pre-check; they settle only on the original binding. A settled historical keyless
\*            record (KeylessHist) exists in every procedure config and is not debt.
\*   Rollback none | binary | ledger  after the 5b binary has written: back to the old binary with
\*                                    the current ledger (binary) or with the pre-deploy copy (ledger)
\* A 5b binary booted on a V1 file stops its Claude runtime before intake
\* (`native.rs:199` -> `native_input.rs:15-20` -> `runtime_bootstrap.rs:299-301`).
EXTENDS Naturals

CONSTANTS Writer, Migrate, View, Boot, Locked, Restart, Deploy, Rollback

Inputs == {"o", "x"}

VARIABLES fmt, rec, st, held, copy, bin, plan, bootfmt, binding, xbind, snap,
          retryDebt, kst, krow, made

Obl == <<retryDebt, kst, krow, made>>
vars == <<fmt, rec, st, held, copy, bin, plan, bootfmt, binding, xbind, snap, retryDebt, kst, krow, made>>

\* A Codex keyless attempt's persisted admission/decision (`append.rs:6-19`): none, or one of
\* Reserved, EffectPossible, Uncertain, Admitted, HandedPending, Joined; krow: its native row is in
\* the transcript.
KStates == {"none", "reserved", "effect", "uncertain", "admitted", "handed", "joined"}
KeylessObl == kst \notin {"none", "joined"}

States == {"queued", "owner", "pasted", "unknown", "done"}
Bins == {"run", "old", "checked", "closing", "stopped", "new", "rolled"}
Formats == {"none", "v1", "v2"}

TypeOK ==
    /\ fmt \in Formats
    /\ rec \in [Inputs -> 0..3]
    /\ st \in [Inputs -> States]
    /\ held \in BOOLEAN
    /\ copy \in [f : Formats, r : [Inputs -> 0..3]]
    /\ bin \in Bins
    /\ plan \in {"go", "hold"}
    /\ bootfmt \in Formats
    /\ binding \in {"original", "replaced"}
    /\ xbind \in {"original", "replaced"}
    /\ snap \in [f : Formats, r : [Inputs -> 0..3]]
    /\ retryDebt \in BOOLEAN
    /\ kst \in KStates
    /\ krow \in BOOLEAN
    /\ made \subseteq {"retry", "keyless"}

Init ==
    /\ fmt = Boot
    /\ rec = [i \in Inputs |-> 0]
    /\ st = [i \in Inputs |-> IF i = "o" THEN "owner" ELSE "queued"]
    /\ held = FALSE
    /\ copy = [f |-> Boot, r |-> [i \in Inputs |-> 0]]
    /\ bin = IF Deploy = "none" THEN "run" ELSE "old"
    /\ plan = "go"
    /\ bootfmt = Boot
    /\ binding = "original"
    /\ xbind = "original"
    /\ snap = [f |-> Boot, r |-> [i \in Inputs |-> 0]]
    /\ retryDebt = FALSE
    /\ kst = "none"
    /\ krow = FALSE
    /\ made = {}

New == bin = "new"
\* A settled keyless record from earlier history; it is never an obligation.
KeylessHist == Deploy # "none"
EffWriter == IF Deploy = "none" THEN Writer ELSE IF New THEN "v2" ELSE "v1"
EffMigrate == IF Deploy = "none" THEN Migrate ELSE ~New
EffView == IF New THEN "v2" ELSE View

\* A process that writes the ledger is up; a 5b binary booted on V1 never opens intake.
Running == bin \in {"run", "old", "checked", "closing", "new", "rolled"} /\ ~(New /\ bootfmt = "v1")
\* New external input is accepted (not while the procedure has closed intake).
Intake == Running /\ bin # "closing"

\* A V2 transaction can open the file: missing (fresh snapshot), V2, or V1 only through restore.
V2Opens(f) == f \in {"none", "v2"} \/ (f = "v1" /\ EffMigrate)

\* V1 update refuses a V2 file (`compat.rs:98 read_legacy`); a V2 transaction refuses what V2Opens refuses.
XAccepts(f) == IF EffWriter = "v1" THEN f \in {"none", "v1"} ELSE V2Opens(f)
XFormat(f) == IF EffWriter = "v1" THEN "v1" ELSE "v2"

Frozen == UNCHANGED <<bin, plan, bootfmt, binding, snap>> /\ UNCHANGED Obl

\* --- out-of-scope writer, one atomic step under the shared lock -------------------------------
XRegister ==
    /\ Intake
    /\ Locked
    /\ st["x"] = "queued"
    /\ IF XAccepts(fmt)
          THEN /\ fmt' = XFormat(fmt)
               /\ rec' = [rec EXCEPT !["x"] = @ + 1]
               /\ st' = [st EXCEPT !["x"] = "pasted"]
               /\ xbind' = binding
          ELSE /\ st' = [st EXCEPT !["x"] = "owner"]   \* NotSent before any key: handed back
               /\ UNCHANGED <<fmt, rec, xbind>>
    /\ UNCHANGED <<held, copy>>
    /\ Frozen

\* --- mutant: the same writer reads, then writes its copy back outside the shared lock --------
XRead ==
    /\ Intake
    /\ ~Locked
    /\ st["x"] = "queued"
    /\ ~held
    /\ held' = TRUE
    /\ copy' = [f |-> fmt, r |-> rec]
    /\ UNCHANGED <<fmt, rec, st, xbind>>
    /\ Frozen

XWrite ==
    /\ Intake
    /\ ~Locked
    /\ st["x"] = "queued"
    /\ held
    /\ IF XAccepts(copy.f)
          THEN /\ fmt' = XFormat(copy.f)
               /\ rec' = [copy.r EXCEPT !["x"] = @ + 1]
               /\ st' = [st EXCEPT !["x"] = "pasted"]
               /\ xbind' = binding
          ELSE /\ st' = [st EXCEPT !["x"] = "owner"]
               /\ UNCHANGED <<fmt, rec, xbind>>
    /\ held' = FALSE
    /\ UNCHANGED copy
    /\ Frozen

\* --- the V2 owner: prepare_history registers, then the leaf pastes --------------------------
\* A transaction that cannot open the file returns Err and writes nothing (`v2.rs:299`), so the
\* owner's attempt stays NotSent and is retried; the step is not enabled then. Inputs already
\* queued keep running while intake is closed.
OwnerPrepare(i) ==
    /\ Running
    /\ st[i] = "owner"
    /\ V2Opens(fmt)
    /\ fmt' = "v2"
    /\ rec' = [rec EXCEPT ![i] = @ + 1]
    /\ st' = [st EXCEPT ![i] = "pasted"]
    \* A handed-back "x" is recorded under the sessions current when the owner takes it.
    /\ xbind' = IF i = "x" THEN binding ELSE xbind
    /\ UNCHANGED <<held, copy>>
    /\ Frozen

Settle(i) ==
    /\ Running
    /\ st[i] = "pasted"
    /\ st' = [st EXCEPT ![i] = "done"]
    /\ UNCHANGED <<fmt, rec, held, copy, xbind>>
    /\ Frozen

\* Only the out-of-scope queued head is settled by transcript proof after a restart.
Crash ==
    /\ Running
    /\ Restart
    /\ st["x"] = "pasted"
    /\ st' = [st EXCEPT !["x"] = "unknown"]
    /\ UNCHANGED <<fmt, rec, held, copy, xbind>>
    /\ Frozen

\* The proof finds the recorded attempt through a view that reads the file's format, and only
\* in the transcript the attempt was bound to (`Recorded::matches`, `queued_reconcile.rs:382-386`).
Visible(i) == rec[i] >= 1 /\ (EffView = "v2" \/ fmt \in {"none", "v1"})

Mend ==
    /\ Running
    /\ st["x"] = "unknown"
    /\ Visible("x")
    /\ xbind = binding
    /\ st' = [st EXCEPT !["x"] = "done"]
    /\ UNCHANGED <<fmt, rec, held, copy, xbind>>
    /\ Frozen

\* --- the ops procedure (Deploy # "none") ----------------------------------------------------
\* Rev 0: the format is checked while the old producer still runs and intake stays open.
PreCheck ==
    /\ Deploy = "precheck"
    /\ bin = "old"
    /\ bin' = "checked"
    /\ plan' = IF fmt = "v1" THEN "hold" ELSE "go"
    /\ UNCHANGED <<fmt, rec, st, held, copy, bootfmt, binding, xbind, snap>>
    /\ UNCHANGED Obl

Guards == {"guarded", "replaceonv2", "skipretry", "skipkeyless", "skipe3", "keylessall",
           "nonceabsent", "anydecision"}

\* While the old producer runs (also after the live check), tui_o can record a retry debt and a
\* keyless paste can still await its receipt; each settles only on the original binding.
Appear(k) ==
    /\ Deploy # "none"
    /\ bin \in {"old", "checked", "closing"}
    /\ k \notin made
    /\ made' = made \cup {k}
    /\ IF k = "retry" THEN retryDebt' = TRUE /\ UNCHANGED <<kst, krow>>
                       ELSE /\ kst' \in KStates \ {"none", "joined"}
                            /\ krow' \in BOOLEAN
                            /\ UNCHANGED retryDebt
    /\ UNCHANGED <<fmt, rec, st, held, copy, bin, plan, bootfmt, binding, xbind, snap>>

SettleObl(k) ==
    /\ Running
    /\ binding = "original"
    /\ IF k = "retry" THEN retryDebt /\ retryDebt' = FALSE /\ UNCHANGED <<kst, krow>>
                       ELSE KeylessObl /\ kst' = "joined" /\ krow' = TRUE /\ UNCHANGED retryDebt
    /\ UNCHANGED <<fmt, rec, st, held, copy, bin, plan, bootfmt, binding, xbind, snap, made>>

\* Rev 2 step 1: close new intake; queued inputs keep settling on the original binding.
Close ==
    /\ Deploy \in Guards
    /\ bin = "old"
    /\ bin' = "closing"
    /\ UNCHANGED <<fmt, rec, st, held, copy, plan, bootfmt, binding, xbind, snap>>
    /\ UNCHANGED Obl

\* Stopping dcserver: a paste the old binary had not settled stays an Unknown queued head.
Stop ==
    /\ bin \in {"checked", "closing"}
    /\ bin' = "stopped"
    /\ st' = [i \in Inputs |-> IF i = "x" /\ st[i] = "pasted" THEN "unknown" ELSE st[i]]
    /\ UNCHANGED <<fmt, rec, held, copy, plan, bootfmt, binding, xbind, snap>>
    /\ UNCHANGED Obl

\* Replace the sessions (new transcript binding) and start the 5b binary on the stopped ledger.
Deployed ==
    /\ bin' = "new"
    /\ binding' = "replaced"
    /\ bootfmt' = fmt
    /\ snap' = [f |-> fmt, r |-> rec]

StartAsPlanned ==
    /\ Deploy = "precheck"
    /\ bin = "stopped"
    /\ IF plan = "go" THEN Deployed ELSE /\ bin' = "old" /\ UNCHANGED <<binding, bootfmt, snap>>
    /\ UNCHANGED <<fmt, rec, st, held, copy, plan, xbind>>
    /\ UNCHANGED Obl

\* Rev 3 step 4: the stopped persisted state decides (E1, E2, E3); nothing is moved.
Ready ==
    /\ fmt # "v1"
    /\ (Deploy = "replaceonv2" \/ st["x"] # "unknown")
    /\ (Deploy = "skipretry" \/ ~retryDebt)
    /\ (Deploy = "skipkeyless" \/ ~KeylessObl
        \/ (Deploy = "nonceabsent" /\ ~krow)
        \/ (Deploy = "anydecision" /\ kst = "handed"))
    /\ (Deploy = "skipe3" \/ View = "v2")
    /\ (Deploy # "keylessall" \/ ~KeylessHist)

FinalCheck ==
    /\ Deploy \in Guards
    /\ bin = "stopped"
    /\ IF Ready
          THEN Deployed
          \* hold: the old binary comes back on the same sessions with intake open again; the
          \* operator settles and the procedure starts over from step 1
          ELSE /\ bin' = "old"
               /\ UNCHANGED <<binding, bootfmt, snap>>
    /\ UNCHANGED <<fmt, rec, st, held, copy, plan, xbind>>
    /\ UNCHANGED Obl

\* After the 5b binary has written, the binary goes back; the sessions stay.
RollbackStep ==
    /\ Rollback # "none"
    /\ bin = "new"
    /\ (fmt # snap.f \/ rec # snap.r)
    /\ bin' = "rolled"
    /\ IF Rollback = "ledger"
          THEN /\ fmt' = snap.f /\ rec' = snap.r
          ELSE UNCHANGED <<fmt, rec>>
    /\ UNCHANGED <<st, held, copy, plan, bootfmt, binding, xbind, snap>>
    /\ UNCHANGED Obl

Idle == UNCHANGED vars

Next ==
    \/ XRegister \/ XRead \/ XWrite
    \/ \E i \in Inputs : OwnerPrepare(i) \/ Settle(i)
    \/ Crash \/ Mend
    \/ \E k \in {"retry", "keyless"} : Appear(k) \/ SettleObl(k)
    \/ PreCheck \/ Close \/ Stop \/ StartAsPlanned \/ FinalCheck \/ RollbackStep
    \/ Idle

Spec ==
    /\ Init /\ [][Next]_vars
    \* Work steps are strongly fair: the procedure may stop and restart the binary in between.
    /\ SF_vars(XRegister) /\ SF_vars(XRead) /\ SF_vars(XWrite)
    /\ \A i \in Inputs : SF_vars(OwnerPrepare(i)) /\ SF_vars(Settle(i))
    /\ SF_vars(Mend)
    /\ \A k \in {"retry", "keyless"} : SF_vars(SettleObl(k))
    /\ WF_vars(PreCheck) /\ WF_vars(Close) /\ WF_vars(Stop) /\ WF_vars(StartAsPlanned)
    /\ WF_vars(FinalCheck) /\ WF_vars(RollbackStep)

\* INVARIANT: a pasted input has exactly one record in the live ledger.
RecordedExactlyOnce ==
    \A i \in Inputs : st[i] \in {"pasted", "unknown", "done"} => rec[i] = 1

\* INVARIANT: no input is ever recorded twice (by construction; see ROLE).
RecordedAtMostOnce == \A i \in Inputs : rec[i] <= 1

\* INVARIANT: an Unknown head keeps its proof evidence in the live ledger.
ProofKept == \A i \in Inputs : st[i] = "unknown" => rec[i] = 1

\* INVARIANT: an Unknown head keeps the transcript binding its proof needs.
BindingKept == st["x"] = "unknown" => xbind = binding

\* INVARIANT: the slice 5b binary never boots on a V1 ledger.
NewBootSupported == New => bootfmt # "v1"

\* INVARIANT: the sessions are never replaced while a debt that needs them remains (E2).
NoDebtAcrossReplace == binding = "replaced" => ~retryDebt /\ ~KeylessObl

\* INVARIANT: the 5b binary starts only after a start binary with repair 1 (E3), in every run.
E3Enforced == bin \in {"new", "rolled"} => View = "v2"

\* PROPERTY (liveness): every retry debt and keyless obligation eventually settles.
ObligationsSettled == <>[](~retryDebt /\ ~KeylessObl)

\* PROPERTY (liveness): every input is eventually done.
OwnerProgress == <>(\A i \in Inputs : st[i] = "done")

\* PROPERTY (liveness, procedure configs): the 5b binary is eventually started.
DeployReached == <>(bin \in {"new", "rolled"})
=============================================================================
