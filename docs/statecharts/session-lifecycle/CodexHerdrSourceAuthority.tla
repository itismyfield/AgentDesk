---- MODULE CodexHerdrSourceAuthority ----
\* One Codex Herdr cold launch races one O writer poll and one intake discovery.
CONSTANTS ReadBeforeLocks, WriterDefers, RecheckMarker
VARIABLES owner, listing, lookup, marker, writer, discovery, tick
vars == <<owner, listing, lookup, marker, writer, discovery, tick>>

Init ==
    /\ owner = "ready"
    /\ listing = FALSE
    /\ lookup = "none"
    /\ marker = "expected"
    /\ writer = "ready"
    /\ discovery = "ready"
    /\ tick = FALSE

ListingHeld == listing
AuthorityHeld == owner = "authority"
DriverAvailable == writer # "blocked"

BeginLookup ==
    /\ lookup = "none"
    /\ (IF ReadBeforeLocks THEN owner = "ready" /\ ~ListingHeld
                          ELSE AuthorityHeld)
    /\ lookup' = "waiting"
    /\ UNCHANGED <<owner, listing, marker, writer, discovery, tick>>

\* Notification from the shared I/O driver wakes the blocking-pool lookup.
FinishLookup ==
    /\ lookup = "waiting"
    /\ DriverAvailable
    /\ lookup' = IF marker = "expected" THEN "pending" ELSE "changed"
    /\ UNCHANGED <<owner, listing, marker, writer, discovery, tick>>

TakeListing ==
    /\ owner = "ready"
    /\ ~ListingHeld
    /\ IF ReadBeforeLocks THEN lookup \in {"pending", "changed"} ELSE lookup = "none"
    /\ listing' = TRUE
    /\ UNCHANGED <<owner, lookup, marker, writer, discovery, tick>>

TakeAuthority ==
    /\ owner = "ready"
    /\ ListingHeld
    /\ owner' = "authority"
    /\ UNCHANGED <<listing, lookup, marker, writer, discovery, tick>>

\* While the owner is outside source authority, clear or replacement can change its marker.
ChangeMarker ==
    /\ ~AuthorityHeld
    /\ owner \notin {"submitted", "refused"}
    /\ marker = "expected"
    /\ marker' = "successor"
    /\ UNCHANGED <<owner, listing, lookup, writer, discovery, tick>>

\* Validation and restore form one synchronous section. The outcome stands for eligibility
\* to send a first key, with the existing input effect gate included in the outcome.
FinishOwner ==
    /\ AuthorityHeld
    /\ lookup \in {"pending", "changed"}
    /\ listing' = FALSE
    /\ owner' = IF lookup = "pending" /\ (~RecheckMarker \/ marker = "expected")
                 THEN "submitted" ELSE "refused"
    /\ UNCHANGED <<lookup, marker, writer, discovery, tick>>

\* Only the observed writer runs on the worker that was last driving timer and socket events.
PollWriter ==
    /\ writer = "ready"
    /\ writer' = IF ~AuthorityHeld THEN "observed"
                  ELSE IF WriterDefers THEN "deferred" ELSE "blocked"
    /\ UNCHANGED <<owner, listing, lookup, marker, discovery, tick>>

\* A deferred poll yields; it does not continue through later source/command work.
RetryWriter ==
    /\ writer = "deferred" /\ ~AuthorityHeld /\ DriverAvailable
    /\ writer' = "observed"
    /\ UNCHANGED <<owner, listing, lookup, marker, discovery, tick>>

WakeWriter ==
    /\ writer = "blocked" /\ ~AuthorityHeld
    /\ writer' = "observed"
    /\ UNCHANGED <<owner, listing, lookup, marker, discovery, tick>>

\* Discovery can wait on the same authority. Its caller already uses the blocking pool.
PollDiscovery ==
    /\ discovery = "ready"
    /\ discovery' = IF AuthorityHeld THEN "waiting" ELSE "observed"
    /\ UNCHANGED <<owner, listing, lookup, marker, writer, tick>>

WakeDiscovery ==
    /\ discovery = "waiting" /\ ~AuthorityHeld
    /\ discovery' = "observed"
    /\ UNCHANGED <<owner, listing, lookup, marker, writer, tick>>

DriverTick ==
    /\ DriverAvailable
    /\ tick' = ~tick
    /\ UNCHANGED <<owner, listing, lookup, marker, writer, discovery>>

Next ==
    \/ BeginLookup \/ FinishLookup \/ TakeListing \/ TakeAuthority \/ ChangeMarker
    \/ FinishOwner \/ PollWriter \/ RetryWriter \/ WakeWriter
    \/ PollDiscovery \/ WakeDiscovery \/ DriverTick

\* When the driver is available and a nonblocking step remains enabled, it eventually runs.
\* No assumption forces a notification while the last driver worker is blocked.
Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(BeginLookup) /\ WF_vars(FinishLookup)
    /\ WF_vars(TakeListing) /\ WF_vars(TakeAuthority) /\ WF_vars(FinishOwner)
    /\ WF_vars(PollWriter) /\ WF_vars(RetryWriter) /\ WF_vars(WakeWriter)
    /\ WF_vars(PollDiscovery) /\ WF_vars(WakeDiscovery)
    /\ WF_vars(DriverTick)

TypeOK ==
    /\ owner \in {"ready", "authority", "submitted", "refused"}
    /\ listing \in BOOLEAN
    /\ lookup \in {"none", "waiting", "pending", "changed"}
    /\ marker \in {"expected", "successor"}
    /\ writer \in {"ready", "deferred", "blocked", "observed"}
    /\ discovery \in {"ready", "waiting", "observed"}
    /\ tick \in BOOLEAN

NoDatabaseUnderPaneLocks == lookup = "waiting" => ~ListingHeld /\ ~AuthorityHeld
NoSchedulerLockWait == writer # "blocked"
AuthorityAcquiredAfterListing == AuthorityHeld => ListingHeld
FirstEffectMatchesLaunch == owner = "submitted" => lookup = "pending" /\ marker = "expected"
RuntimeKeepsTicking == []<>(tick = TRUE) /\ []<>(tick = FALSE)
LaunchSettles == <>(owner \in {"submitted", "refused"})
====
