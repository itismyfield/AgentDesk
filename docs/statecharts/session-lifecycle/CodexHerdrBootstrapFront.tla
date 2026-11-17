---- MODULE CodexHerdrBootstrapFront ----
\* The Codex canary channel's owner queue while the bootstrap is the writer's only way out: a
\* scheduled snapshot front S keyed Ks (no Herdr row) ahead of a later person input P keyed K (an
\* exact Retired row the bootstrap can launch). Each action cites the fixed code; FrontBefore is c1ad7d1c7b.
\* Key entry points: K = turn_session_key (adk_session.rs:95, intake_turn.rs:926); Ks = the snapshot
\* label (headless_turn.rs:148-152, entrypoints.rs:76-108); probes at owner_input.rs:179,956,988.
EXTENDS Naturals, Sequences

CONSTANTS
    Fix,          \* "before" | "design" | "mut_key" | "mut_prefer" | "mut_place_front"
    PersonComes,  \* BOOLEAN: a person input arrives behind S
    BootBudget,   \* 0 or 1: restarts allowed
    CancelBudget  \* 0 or 1: a Cancel, Terminate, clear or stop may remove a queued input

VARIABLES
    q,        \* the durable owner queue, a sequence over {"S", "P"}
    stage,    \* the front: "idle" (a drive will grant it) | "held"
    holdAt,   \* why the front is held: "none" | "place" (writer hold) | "start" (turn host)
    watch,    \* the probe the held front is watched through: "none" | "K" | "Ks"
    writer,   \* O actor: "booting" | "blocked" | "resumed"
    pState,   \* P: "none" | "queued" | "started" (its bootstrap launched) | "removed"
    sDone,    \* S settled (failed confirmed, or removed)
    futile,   \* a probe woke a front whose own judgement had not changed
    boots, cancels

vars == <<q, stage, holdAt, watch, writer, pState, sDone, futile, boots, cancels>>

FrontKey == Fix \in {"design", "mut_prefer", "mut_place_front"}
Prefer   == Fix \in {"design", "mut_key", "mut_place_front"}

Init ==
    /\ q = <<"S">> /\ stage = "idle" /\ holdAt = "none" /\ watch = "none"
    /\ writer \in {"booting", "blocked"} /\ pState = "none" /\ sDone = FALSE /\ futile = FALSE
    /\ boots = 0 /\ cancels = 0

Accepts == writer = "resumed"                        \* host.rs:248-259
\* A blocked actor with the pin, no marker and a vacant target: placement routes to the gateway
\* (intake_route/input.rs:108) and K's exact Retired row plans the bootstrap (turn_host.rs:314).
LocalOk == writer = "blocked"
\* boot_custody.rs:39-49 writer_boot_held: the placement holds the front before any start.
PlaceHold == ~Accepts /\ ~LocalOk
\* turn_host.rs:420 held_look on the probe's key. Ks has no row, so only a ready writer reads Ready.
ProbeReady(k) == Accepts \/ (k = "K" /\ LocalOk)
\* The judgement the held front stops at again on its next drive.
StillRefused(f, at) == IF at = "place" THEN PlaceHold ELSE (f = "S" /\ ~Accepts)

Front == Head(q)
PersonBehind == \E i \in 2..Len(q) : q[i] = "P"

Boot ==
    /\ writer = "booting" /\ writer' = "blocked"
    /\ UNCHANGED <<q, stage, holdAt, watch, pState, sDone, futile, boots, cancels>>

\* P's launch binds and its marker is the resume evidence (actor.rs:357-396).
Resume ==
    /\ writer = "blocked" /\ pState = "started"
    /\ writer' = "resumed"
    /\ UNCHANGED <<q, stage, holdAt, watch, pState, sDone, futile, boots, cancels>>

\* A new input wakes the drive (drive.rs:88-95); a held front is granted again.
PersonArrives ==
    /\ PersonComes /\ pState = "none"
    /\ q' = Append(q, "P") /\ pState' = "queued" /\ stage' = "idle"
    /\ UNCHANGED <<holdAt, watch, writer, sDone, futile, boots, cancels>>

\* owner_input.rs:173-195: a writer hold arms the probe on the channel's turn key K and returns
\* WRITER_HELD; no queue rotation follows it. mut_place_front reads the front's own key here.
DrivePlace ==
    /\ q # <<>> /\ stage = "idle" /\ PlaceHold
    /\ stage' = "held" /\ holdAt' = "place"
    /\ watch' = IF Fix = "mut_place_front" /\ Front = "S" THEN "Ks" ELSE "K"
    /\ UNCHANGED <<q, writer, pState, sDone, futile, boots, cancels>>

\* P starts through intake (start_owner_front); the turn host plans the bootstrap on K.
DriveP ==
    /\ q # <<>> /\ stage = "idle" /\ ~PlaceHold /\ Front = "P"
    /\ q' = Tail(q) /\ pState' = "started" /\ holdAt' = "none" /\ watch' = "none"
    /\ UNCHANGED <<stage, writer, sDone, futile, boots, cancels>>

\* S starts headless on Ks (start_owner_headless). A ready writer plans a Herdr turn the headless
\* start refuses as ExecutorNotWired, a confirmed failure (start_failure.rs). Otherwise the turn
\* host refuses OWriterNotReady on Ks: BOOTSTRAP_WAITS holds S and arms the probe
\* (owner_input.rs:1014-1020); owner_drive moves an earlier person ahead (v2.rs:125-151).
DriveS ==
    /\ q # <<>> /\ stage = "idle" /\ ~PlaceHold /\ Front = "S"
    /\ IF Accepts
       THEN /\ q' = Tail(q) /\ sDone' = TRUE /\ holdAt' = "none" /\ watch' = "none"
            /\ UNCHANGED stage
       ELSE IF Prefer /\ PersonBehind
       THEN /\ q' = <<"P", "S">> /\ holdAt' = "none" /\ watch' = "none"
            /\ UNCHANGED <<stage, sDone>>
       ELSE /\ stage' = "held" /\ holdAt' = "start"
            /\ watch' = IF FrontKey THEN "Ks" ELSE "K"
            /\ UNCHANGED <<q, sDone>>
    /\ UNCHANGED <<writer, pState, futile, boots, cancels>>

\* held_watch.rs:81-140: the first Ready look wakes the drive once.
WatchWake ==
    /\ q # <<>> /\ stage = "held" /\ watch # "none" /\ ProbeReady(watch)
    /\ futile' = (futile \/ StillRefused(Front, holdAt))
    /\ stage' = "idle" /\ watch' = "none"
    /\ UNCHANGED <<q, holdAt, writer, pState, sDone, boots, cancels>>

\* host.rs:998-1012 input_ready, owner_input.rs:189-195 boot wake: the ready edge wakes the drive.
ReadyWake ==
    /\ stage = "held" /\ Accepts
    /\ stage' = "idle" /\ watch' = "none"
    /\ UNCHANGED <<q, holdAt, writer, pState, sDone, futile, boots, cancels>>

\* A restart drops the watch and the actor; the queue and its order are durable and boot drives
\* the front again (boot custody).
Restart ==
    /\ boots < BootBudget /\ boots' = boots + 1
    /\ writer' = "booting"
    /\ stage' = "idle" /\ holdAt' = "none" /\ watch' = "none"
    /\ UNCHANGED <<q, pState, sDone, futile, cancels>>

\* Cancel, Terminate, clear or stop removes a queued input; a removed front ends its watch
\* (held_watch.rs:173-183 leads false).
Remove ==
    /\ cancels < CancelBudget /\ q # <<>> /\ cancels' = cancels + 1
    /\ \E i \in 1..Len(q) :
         /\ q' = SubSeq(q, 1, i - 1) \o SubSeq(q, i + 1, Len(q))
         /\ pState' = IF q[i] = "P" THEN "removed" ELSE pState
         /\ sDone' = (sDone \/ q[i] = "S")
         /\ IF i = 1 THEN stage' = "idle" /\ holdAt' = "none" /\ watch' = "none"
            ELSE UNCHANGED <<stage, holdAt, watch>>
    /\ UNCHANGED <<writer, futile, boots>>

Next ==
    \/ Boot \/ Resume \/ PersonArrives \/ DrivePlace \/ DriveP \/ DriveS \/ WatchWake
    \/ ReadyWake \/ Restart \/ Remove
    \/ UNCHANGED vars

Spec == Init /\ [][Next]_vars
    /\ WF_vars(Boot) /\ WF_vars(Resume) /\ WF_vars(PersonArrives) /\ WF_vars(DrivePlace)
    /\ WF_vars(DriveP) /\ WF_vars(DriveS) /\ SF_vars(WatchWake) /\ WF_vars(ReadyWake)

\* INVARIANT: no probe wakes a front whose own judgement has not changed.
NoFutileRedrive == ~futile
\* PROPERTY: a person input behind S eventually starts its turn, or is removed.
PersonStarts == (pState = "queued") ~> (pState \in {"started", "removed"})
\* PROPERTY: S eventually settles once a person input's bootstrap made the writer ready.
SnapshotSettles == (pState = "started") ~> sDone
====
