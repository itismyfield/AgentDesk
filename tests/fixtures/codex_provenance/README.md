`0162_metadata_turn.jsonl` is reconstructed from the Codex CLI 0.162.0 observations
in `review-design-p4-1b-r1.out.md` (§P2-B) and `review-p4-1a-r1.out.md` (§P2-1),
under the campaign's `i5845-cdx-p4` lane-run directory. It is not a captured rollout.
The repository's existing busy-inject fixtures contain abbreviated metadata;
the review supplies the three observed top-level schemas and record ordering.

IDs, text, state values and token counters are placeholders. Tests prepend their
own matching parent `session_meta` and calculate offsets from the resulting bytes.
The acceptance contract is deliberately the review's minimum confirmed schema:
nonblank token `turn_id`, world object without `turn_id` and optional bool `full`,
and bool `trigger_turn`. Tests vary trigger and token identity to exercise rejection.
