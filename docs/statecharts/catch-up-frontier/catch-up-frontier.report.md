# Catch-up cursor over a V2 queue channel

Code SHA: `5918c79bb34d39357b16d16ef32c1e2d081f99b9` (main) and the fix branch `fix/i8045-catchup-cursor-v2-terminal`.

The model follows three inputs of one channel whose queue file is V2, in message order, and the durable
phase-1 cursor (`.sweep.json` `after`). An input is waiting (V2 `items`), being sent (`head_injection`),
owed after an unclear send (`retained_inputs`), delivered (`terminal_identities`), or dropped before it was
sent, which leaves no source holding it. A scan recovers dropped inputs after the cursor and moves the cursor
to the end of the run of inputs it may pass.

`Merge` picks how the scan reads the V2 file:

- `main`: every id the file holds reads as queued, so nothing passes (`merge_v2_held_sources` gave all of
  them `Queued`, and the later dispatch evidence could not replace it).
- `fixed`: an id only the settled history holds passes; queued, attempted and retained ids stay open.
- `pendingpass` (mutant): queued ids pass as well.

## Rules

- `NoUndeliveredBehindCursor` (INVARIANT): the cursor never sits past an input that is not delivered.
- `DeliveredInputsAdvanceCursor` (PROPERTY): once every input is delivered, the cursor reaches the newest.

## Results

| Config | Breaks | Path |
|---|---|---|
| `CatchUpFrontierMain.cfg` | `DeliveredInputsAdvanceCursor` | all three inputs are delivered, the cursor stays at 0 forever |
| `CatchUpFrontier.cfg` | nothing | 156 states |
| `CatchUpFrontierPendingPass.cfg` | `NoUndeliveredBehindCursor`, `NoDeadlock` | one scan passes three waiting inputs; dropping them later leaves nothing to recover them |

The chart is the `fixed` run. Labels name the cursor and the input right after it.

## Left out

- Live mailbox arms (`intervention_queue`, active turn, dequeue reservation) read before the V2 file; they win
  over it, so a V2 settled id the live queue still holds stays open.
- The 24-hour pruning of settled identities: a pruned id falls back to the ledger and age classification.
- Paging: one scan here reads every input; the code reads one page on a scan without retry state.
