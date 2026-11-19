-------------------- MODULE WriterAdoptionDebtDelivery --------------------
EXTENDS Naturals, TLC
CONSTANTS Mode, RestoredPending, LegacyBody
VARIABLES phase, meta, pending, reserved, prepared, remote, localPosted, shared,
    generation, run, grantGen, grantRun, live, locked, stopped, transferred,
    restarted, foreignSafe, freshSafe, reservationSafe, snapshot, newer, mirror, exactProof
vars == <<phase, meta, pending, reserved, prepared, remote, localPosted, shared,
    generation, run, grantGen, grantRun, live, locked, stopped, transferred,
    restarted, foreignSafe, freshSafe, reservationSafe, snapshot, newer, mirror, exactProof>>
Init == /\ phase = (IF RestoredPending THEN "uncertain" ELSE "fresh")
        /\ meta = FALSE /\ pending = RestoredPending /\ reserved = RestoredPending
        /\ prepared = RestoredPending /\ remote = (IF RestoredPending \/ Mode = "SharedFirst" THEN 1 ELSE 0)
        /\ localPosted = FALSE /\ shared = (Mode = "SharedFirst") /\ mirror = FALSE /\ exactProof = (Mode = "SharedFirst") /\ generation = 1 /\ run = 0
        /\ grantGen = 1 /\ grantRun = 0 /\ live = FALSE /\ locked = FALSE
        /\ stopped = FALSE /\ transferred = FALSE /\ restarted = FALSE
        /\ foreignSafe = TRUE /\ freshSafe = TRUE /\ reservationSafe = TRUE
        /\ snapshot = TRUE /\ newer = FALSE
Boot == /\ ~live /\ ~locked /\ run < 4
        /\ live' = TRUE /\ run' = run + 1
        /\ phase' = IF phase = "issued" /\ Mode # "Revive" THEN "fresh" ELSE phase
        /\ UNCHANGED <<meta,pending,reserved,prepared,remote,localPosted,shared,generation,
            grantGen,grantRun,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Metadata == /\ live /\ ~pending /\ ~meta /\ meta' = TRUE
        /\ UNCHANGED <<phase,pending,reserved,prepared,remote,localPosted,shared,generation,run,
            grantGen,grantRun,live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Issue == /\ Mode # "Before" /\ ~LegacyBody /\ meta /\ live /\ phase = "fresh" /\ ~shared /\ ~reserved
        /\ phase' = "issued" /\ grantRun' = run
        /\ grantGen' = (IF Mode = "StaleGeneration" THEN 0 ELSE generation)
        /\ UNCHANGED <<meta,pending,reserved,prepared,remote,localPosted,shared,generation,run,
            live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Reserve == /\ phase = "issued" /\ live /\ ~reserved
        /\ (grantGen = generation \/ Mode = "StaleGeneration")
        /\ (grantRun = run \/ Mode = "Revive")
        /\ reserved' = (Mode # "NoReserve") /\ phase' = "reserved"
        /\ UNCHANGED <<meta,pending,prepared,remote,localPosted,shared,generation,run,grantGen,
            grantRun,live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Acquire == /\ phase = "reserved" /\ live /\ ~locked
        /\ (grantGen = generation \/ Mode = "StaleGeneration")
        /\ (grantRun = run \/ Mode = "Revive")
        /\ locked' = TRUE /\ phase' = "guarded"
        /\ UNCHANGED <<meta,pending,reserved,prepared,remote,localPosted,shared,generation,run,
            grantGen,grantRun,live,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Prepare == /\ phase = "guarded" /\ live /\ prepared' = TRUE /\ phase' = "prepared"
        /\ UNCHANGED <<meta,pending,reserved,remote,localPosted,shared,generation,run,grantGen,
            grantRun,live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Http == /\ phase = "prepared" /\ live /\ remote = 0 /\ ~LegacyBody /\ Mode # "Unproved"
        /\ remote' = remote + 1 /\ phase' = "uncertain" /\ locked' = FALSE
        /\ foreignSafe' = (foreignSafe /\ grantGen = generation)
        /\ freshSafe' = (freshSafe /\ grantRun = run)
        /\ reservationSafe' = (reservationSafe /\ reserved)
        /\ UNCHANGED <<meta,pending,reserved,prepared,localPosted,shared,generation,run,grantGen,
            grantRun,live,stopped,transferred,restarted,snapshot,newer, mirror, exactProof>>
LocalReceipt == /\ remote = 1 /\ prepared /\ ~localPosted /\ localPosted' = TRUE /\ pending' = FALSE /\ exactProof' = TRUE
        /\ UNCHANGED <<phase,meta,reserved,prepared,remote,shared,generation,run,grantGen,grantRun,
            live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror>>
SharedReceipt == /\ ~shared /\ (exactProof \/ (Mode = "ReceiptFalse" /\ reserved))
        /\ shared' = TRUE /\ phase' = "confirmed"
        /\ UNCHANGED <<meta,pending,reserved,prepared,remote,localPosted,generation,run,grantGen,
            grantRun,live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Stop == /\ ~stopped /\ live /\ phase = "issued"
        /\ stopped' = TRUE /\ live' = FALSE
        /\ UNCHANGED <<phase,meta,pending,reserved,prepared,remote,localPosted,shared,generation,run,
            grantGen,grantRun,locked,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Transfer == /\ ~transferred /\ generation = 1 /\ (phase = "issued" \/ (Mode = "TransferRace" /\ locked))
        /\ generation' = 2 /\ transferred' = TRUE
        /\ live' = (IF Mode = "TransferRace" /\ locked THEN live ELSE FALSE)
        /\ phase' = (IF locked THEN phase ELSE "fresh")
        /\ UNCHANGED <<meta,pending,reserved,prepared,remote,localPosted,shared,run,grantGen,grantRun,
            locked,stopped,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Crash == /\ ~restarted /\ ((remote = 1 /\ ~shared) \/ (Mode = "Unproved" /\ phase = "prepared"))
        /\ restarted' = TRUE /\ live' = FALSE /\ locked' = FALSE
        /\ pending' = ~localPosted /\ phase' = "uncertain"
        /\ UNCHANGED <<meta,reserved,prepared,remote,localPosted,shared,generation,run,grantGen,
            grantRun,stopped,transferred,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
NewOutput == /\ ~newer /\ meta /\ live /\ ((shared /\ mirror) \/ Mode \in {"Before", "EarlyOutput"})
        /\ newer' = TRUE
        /\ UNCHANGED <<phase,meta,pending,reserved,prepared,remote,localPosted,shared,generation,run,
            grantGen,grantRun,live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot, mirror, exactProof>>
Duplicate == /\ Mode = "Duplicate" /\ shared /\ remote = 1 /\ remote' = 2
        /\ UNCHANGED <<phase,meta,pending,reserved,prepared,localPosted,shared,generation,run,grantGen,
            grantRun,live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer, mirror, exactProof>>
Gc == /\ Mode = "GcEarly" /\ reserved /\ snapshot /\ snapshot' = FALSE
        /\ UNCHANGED <<phase,meta,pending,reserved,prepared,remote,localPosted,shared,generation,run,grantGen,
            grantRun,live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,newer, mirror, exactProof>>
Mirror == /\ shared /\ ~mirror /\ mirror' = TRUE
        /\ UNCHANGED <<phase,meta,pending,reserved,prepared,remote,localPosted,shared,generation,run,
            grantGen,grantRun,live,locked,stopped,transferred,restarted,foreignSafe,freshSafe,reservationSafe,snapshot,newer,exactProof>>
Next == Mirror \/ Boot \/ Metadata \/ Issue \/ Reserve \/ Acquire \/ Prepare \/ Http \/ LocalReceipt \/ SharedReceipt
    \/ Stop \/ Transfer \/ Crash \/ NewOutput \/ Duplicate \/ Gc \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars /\ WF_vars(Boot) /\ WF_vars(Metadata) /\ WF_vars(Issue)
    /\ WF_vars(Reserve) /\ WF_vars(Acquire) /\ WF_vars(Prepare) /\ WF_vars(Http)
    /\ WF_vars(LocalReceipt) /\ WF_vars(SharedReceipt) /\ WF_vars(Mirror)
AtMostOnce == remote + (IF LegacyBody THEN 1 ELSE 0) <= 1
ReservationBeforeHttp == reservationSafe
StrictSharedReceipt == shared => exactProof /\ remote >= 1
NoForeign == foreignSafe
LiveAttachment == freshSafe
SnapshotRetained == snapshot
OldBeforeNew == newer => shared /\ mirror
UnprovedHeld == Mode = "Unproved" /\ remote = 0 => ~shared /\ ~newer
OldBodyEventuallyReceipted == <> shared
MetadataEventuallyReady == <> meta
=============================================================================
