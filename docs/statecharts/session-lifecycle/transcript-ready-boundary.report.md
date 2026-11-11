# Session lifecycle readiness boundary — design d1

Code SHA: 574af580b0c5d6e6ce280994f4ed243463de5d2d.
SC_RUN=mac-needed; chart is a hand-authored boundary projection.

The existing HerdrBusyHandoff close/custody/crash transitions are unchanged.
ComposerAdmitted(i) factors the equivalent Blocked/draft guard used by WakeFirst/Reserve.
Its proposed signal interpretation is refined by ../input-single-path/ClaudeReadiness.tla,
instanced by InputPath.ReadinessSpec. This does not claim product implementation or compose
the readiness and crash state spaces into a new full-system proof.

Actual supplemental TLC regression: AfterBusy, AfterFirstRefusal GREEN;
BlockedComposerBoundary remains expected RED for MessageProgress and JournalProgress.
These 3 configs ran 15 TLC checks in the final 23-cfg subset, with no unexpected violation.
Legacy InputPath Clean/BootDesign/StopGateLive also matched their expectations.
Full results/commands/logs/assumptions are in ../input-single-path/transcript-readiness.report.md.

First input changes readiness evidence and retains #8077's composer protection, typed
pre-key Held/NotSentProof/hold_not_sent/wake and post-key Unknown retention.
Reverse return /exit and Codex remain outside the refinement.
A permanently unread composer is not silently declared ready: its progress boundary stays RED.
