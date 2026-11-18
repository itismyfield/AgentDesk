# Verified late thread ownership

Base: `ffa099d4f4a6a68c26171fb464f7b5edffca2e5f`.

A running Legacy thread can bind a source while its late adoption waits. The old
binding floor then refuses activation. Main leaves that child Released without a
body owner and repeatedly offers it to writer boot. The repair freezes the checked
parent, binding source/execution and observed turn. Only that refusal grants Legacy
ownership. A single observer reopens Deferred at turn end, session end or a new
execution bind. Existing source, delivery and intake fences still decide activation.

The bounded model makes a body, host stop, run end, boundary retry and successor
body explicit. Fairness assumes the Legacy body and run end are eventually observed;
it does not promise progress for an unverified parent or unreadable authority.
BeforeBody breaks CurrentRunAnswered; BeforeBoot breaks NoBootChurn.
ProposedBody and ProposedBoot hold their declared rules. Normalize, boot exclusion
and boundary retry mutants break their corresponding progress/safety rule.
The Rust tests separately exercise the real managed host and Legacy claim gate,
all three boundaries, both restart forms, source compaction and stale-host cleanup.

Backlog cutoff, receipt propagation/discard and unresolved/conflicting-parent
notifications are deferred. No offsets, delivered frontiers or stored replies are
rewritten by this repair. The unchanged Legacy relay supplies the current body.
