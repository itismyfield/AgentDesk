---- MODULE TmuxSocketBoot ----
\* Focus the TmuxSession T-A/T-B path on a retained row after OS reboot.
\* Code base: 75726b3b1dfd5ea53aa9164324ed54030194a7cb.
EXTENDS Naturals
CONSTANT SocketMissingIsMissing
VARIABLES phase, owner, probe, row, admitted
vars == <<phase, owner, probe, row, admitted>>
Init ==
    /\ phase = "down" /\ row = TRUE /\ admitted = 0
    /\ owner \in {"local", "mini", "wsl"}
    /\ probe \in {"namespace_missing", "other_path", "permission", "transient", "present"}
Boot ==
    /\ phase = "down" /\ phase' = "boot"
    /\ UNCHANGED <<owner, probe, row, admitted>>
\* has-session's exact namespace + ENOENT lstat witness; all other failures defer.
\* tmux.rs:109; tmux_probe.rs:103-107; restore_inflight.rs:1558-1625.
Restore ==
    /\ phase = "boot" /\ phase' = "watchers"
    /\ row' = IF owner = "local" /\ probe = "namespace_missing" /\ SocketMissingIsMissing
               THEN FALSE ELSE row
    /\ UNCHANGED <<owner, probe, admitted>>
\* No local session is adoptable in the missing-socket scenario; live/foreign stay protected.
Watchers ==
    /\ phase = "watchers" /\ phase' = "reconciled"
    /\ UNCHANGED <<owner, probe, row, admitted>>
\* recovery_flush.rs:128 opens intake only after restore; create-new refuses a held row.
OpenIntake ==
    /\ phase = "reconciled" /\ phase' = "open"
    /\ admitted' = IF row THEN 0 ELSE 1
    /\ UNCHANGED <<owner, probe, row>>
Idle == phase = "open" /\ UNCHANGED vars
Next == Boot \/ Restore \/ Watchers \/ OpenIntake \/ Idle
LocalMissingUnblocked ==
    (phase = "open" /\ owner = "local" /\ probe = "namespace_missing") => (~row /\ admitted = 1)
ForeignProtected == owner # "local" => (row /\ admitted = 0)
LiveProtected == probe = "present" => (row /\ admitted = 0)
UncertainProtected == probe \in {"other_path", "permission", "transient"} => (row /\ admitted = 0)
NoDuplicateAdmission == admitted <= 1
====
