# Tool-image attachments: turn attribution, obligations and retention

Code: S2a head `b2284eeebf87b956349e114b9ccc8b084d95fa30` (before) and the w3 turn repair
(after) on branch `wt/i8397-s2a`. Model: `ToolImage.tla`, 20 configs, TLC 1.7.4 through
`scripts/ci/statecharts_tlc.py` (PASS 20/20, about 61 s; slowest `ToolImageDesignC` 59 s).

## What it models

One channel's transcript records are derived into tool-image obligations, posted through the
writer's single unresolved Prepared, settled from history (Posted, NotFound, Ambiguous,
Unresolved), or omitted with a notice. Spool GC removes the oldest retained record; a restart
replays every retained record from `rstart`, not from the saved cursor.

Records: opener `o1`/`o2`, a prompt `p` inside a turn, images `h1`, over-limit images
`b1`/`b2`, a tool call `c` and its late image result `r`, native idle `i`, another turn's
named idle `x`, verified physical end `e`.

Turn attribution follows `writer/tool_images/turns.rs`:

- Row preparation (`RowPrep`): an unknown tracker restores the durable span holding the row
  before any of the row's facts, then applies a restored Closed boundary (`ClosedOffset`).
- Durable call attribution (`DurableCall`): a call still waiting for its result when its
  turn ends is recorded as `ImageCall`; a late result uses the call's turn first.
- Named idle guard (`NamedIdleGuard`): an idle naming another turn leaves the open turn.
- A physical end applies only with this run's opener evidence (`restart_end.rs`), so an
  opener stays protected until its span's Closed is durable (`GcRule = "closed"`).

## Rules

`AtMostOnce`, `NoticeAtMostOnce`, `OutcomeExclusive`, `NoSilentLoss`, `AmbigKept`,
`AmbiguousNeverOmitted`, `SinglePrepared`, `AmbigBytesKept`, `NoWrongOpened` (no span opens
inside another turn's span), `AttributionTrue` (every image key names the turn the image
belongs to) and the liveness property `EventuallyAccounted`.

`PlainUntouched` is not a model guarantee: tool results without images are owned by the
existing tui_o regression tests.

## Results

| cfg | input | broken | before → after |
|---|---|---|---|
| ToolImageDesignMidPrompt | o1 h1 p h1 i o2 h1 | - | `MutNoRowPrep` RED → GREEN |
| ToolImageMutNoRowPrep | same, restore only at the image | NoWrongOpened, AttributionTrue, AtMostOnce, AmbigBytesKept | w2 behavior |
| ToolImageDesignLateCall | o1 c h1 i r | - | `MutNoDurableCall` RED → GREEN |
| ToolImageMutNoDurableCall | same, call turn only in memory | AttributionTrue, AtMostOnce, AmbigBytesKept | w2 behavior |
| ToolImageDesignForeignIdle | o1 h1 x h1 i | - | `MutNoNamedIdleGuard` RED → GREEN |
| ToolImageMutNoNamedIdleGuard | same, any idle closes | AttributionTrue, AtMostOnce | w2 behavior |
| ToolImageDesignA/B/C/E/EFail/F, Unresolved | earlier inputs | - | GREEN |
| ToolImageAmbigSafe | Lag, safety only | - | GREEN |
| ToolImageBefore | no image path | NoSilentLoss | current product before #8397 |
| ToolImageMutGcWhenOpened | opener released by an Opened span | NoSilentLoss | mutant |
| ToolImageMutClosedOffsetNotApplied | restored Closed ignored | NoSilentLoss | mutant |
| ToolImageMutSharedPrepared | notice ignores an image Prepared | SinglePrepared | mutant |
| ToolImageMutBytesImagesOnly / MutBytesAmbig | notices or Ambiguous lose byte protection | AmbigBytesKept | mutant |

Counterexamples checked against the w2 code:

- Mid-turn prompt: `before_offset` had no span restore, so `observe(Prompt)` opened a new
  turn; `turn_for_image` restored only from Unknown and returned `Opened(mid)` inside u1,
  which the ledger's `open_span` rejects as a violation.
- Late result: `ImageTurns::calls` was memory only; after the call and opener segments were
  collected and the writer restarted, the result fell to the edge turn and a new key.
- Foreign idle: `observe` mapped every `Idle(_)` to `end`, closing the parent on a child's
  `turn_aborted`.

## Limits

Spool segments are one record each. History is truthful unless `Lag` hides one arrived post
(policy (a): a second NotFound ends as a "delivery not confirmed" notice). Group assembly of
the notice line, FIFO with bodies and the large-row capture path are covered by integration
tests, not by this model.
