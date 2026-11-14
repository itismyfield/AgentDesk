# Session lifecycle Idle readiness boundary — design d2

Production code SHA: 574af580b0c5d6e6ce280994f4ed243463de5d2d.
Document baseline: 0b0f39c582a9073fdb20b62512bc7794a18d1281.
SC_RUN=mac-needed; chart is a hand-authored boundary projection.

Existing active/native Running busy injection stays on its own path with the same paste/Enter/native queue/receipt.
The Idle first-input boundary alone uses current SessionStart/approved transcript terminal plus existing composer veto.
Suppressed Stop's soft summary/timer read cannot authorize that boundary or compact until continuation finishes.
The source/turn-scoped latch does not change shared transcript classification, busy policy or #8077 custody.

HerdrBusyHandoff model bytes are unchanged from d1; its equivalent ComposerAdmitted abstraction stays intact.
Actual rerun in the d2 changed-model subset: AfterBusy and AfterFirstRefusal GREEN;
BlockedComposerBoundary expected RED (MessageProgress, JournalProgress), 15 reported TLC runs across 3 cfgs.
The InputPath refinement adds explicit BusyInject/BusyAbsorb/BusyConsume and continuation actions.
These are separate bounded refinements, not a new full-system composition proof.
The independent Restart cfg does not prove suppression-history-loss progress.

Full final results: ../input-single-path/transcript-readiness.report.md.
Exact close, existing draft protection, typed pre-key Held/NotSentProof/hold_not_sent/wake,
post-key Unknown retention, reverse return /exit and Codex contracts remain unchanged.
