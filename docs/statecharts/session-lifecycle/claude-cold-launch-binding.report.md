# Herdr Claude cold launch and first-turn queue injection

The model follows one Herdr Claude launch whose SessionStart and UserPromptSubmit arrive before the transcript. During the first read there is no later hook. The binding log and the hosted PG row are distinct. A busy queue attempt needs a Bound row and a verified pin on the current source.

## Result

TLC 1.7.4 matched all four scoped configs in 15.0 seconds. The statechart wrapper reached 13 states for each config.

| Config | Result | Behavior |
|---|---|---|
| Before · `356ee704f9` | RED · `FirstTurnQueueEligible` | Early Stat resolves the binding and PG binds, then the busy attempt is held for a missing pin. [Chart](claude-cold-launch-binding-before.chart.mmd) |
| After | GREEN | The first complete record and the retained launch hook verify the pin. The busy attempt can pass its source gate. [Chart](claude-cold-launch-binding-after.chart.mmd) |
| NoRegistration | RED · `FirstTurnBound`, `FirstTurnQueueEligible`, `LaunchSettles` | Skipping early registration and bind leaves the log and row Pending until handoff, which an early read exit can skip. |
| NoBind | RED · `FirstTurnBound`, `FirstTurnQueueEligible`, `LaunchSettles` | Early registration verifies the pin. The busy route still requires a Bound row, and an early exit can leave the row Pending. |

- `FirstTurnBound` requires the log to name the current source and the row to be Bound while the first turn is read.
- `FirstTurnQueueEligible` requires a queue attempt during that read to pass the source gate.
- `LaunchSettles` requires the log and row eventually to settle as Resolved and Bound unless the turn ends before the transcript exists.

## Counterexample against the code

The Before chart's shortest path for `FirstTurnQueueEligible` has six steps.

1. SessionStart arrives for the launch session.
2. `attach_launched_herdr_source` sees no transcript and records Pending(startup).
3. UserPromptSubmit arrives for the same session while the transcript is still absent.
4. Claude writes the transcript with the first complete JSONL record.
5. `first_input::observe` sees the file. Its early registration records Resolved by Stat, and `bind_once_logged` binds the row. The source remains unverified.
6. A busy queue attempt sees the Bound row. `Context::current_source` requires the current pin and holds the attempt.

The fresh launch path waits for SessionStart before attach in `herdr_turn/handoff.rs:138-152`. The two registration mutants break `FirstTurnBound` at step 5 and `FirstTurnQueueEligible` at step 6. Their `LaunchSettles` counterexample adds an early read exit before handoff at step 7.

| Model boundary | Code |
|---|---|
| Attach and initial Pending | `herdr_source.rs` · `attach_launched_herdr_source` |
| Same-session launch hooks | `observation_ingress::observe_binding_hook` same-session branch and `adoption_retry::observe_launch_hook` |
| Source resolution and PG bind | `first_input::observe` and `herdr_turn::bind_once_logged` |
| Pin verification | `runtime_binding/claude_source.rs` · `Candidate::promote_registered` |
| Bound route and verified source | `queue_io/input_owner/claude_handoff.rs` · `observe_busy` and `host_input/busy/herdr.rs` · `Context::current_source` |

The model assumes the PG bind succeeds when called for the matching nonce and that the first complete record belongs to the attached file. It abstracts file creation and the first complete record into one step, and checks the queue at the first observable progress. File replacement, DB failures, contention, in-pane `/clear`, and the pane's response after the source gate are outside its scope. The charts retain all modeled transitions.

## Reproduce

Use the statechart wrapper with the model and view in a scratch directory. Set `JAVA` and `TLA2TOOLS` to Java and the repository's pinned TLC 1.7.4 jar. With `statechart` on PATH, run the following from the repository root.

```sh
scratch=$(mktemp -d)
mkdir "$scratch/session-lifecycle"
cp docs/statecharts/session-lifecycle/ClaudeColdLaunchBinding* \
  docs/statecharts/session-lifecycle/claude-cold-launch-binding.view.mjs \
  "$scratch/session-lifecycle/"
rg '^session-lifecycle/ClaudeColdLaunchBinding' docs/statecharts/expected.tsv > "$scratch/expected.tsv"
python3 scripts/ci/statecharts_tlc.py --root "$scratch" --java "$JAVA" --jar "$TLA2TOOLS"
model="$scratch/session-lifecycle/ClaudeColdLaunchBinding"
for config in Before After NoRegistration NoBind; do
  statechart "$model.tla" --config "$model$config.cfg" \
    --view "$scratch/session-lifecycle/claude-cold-launch-binding.view.mjs" \
    --out "$scratch/chart-$config"
done
```

The wrapper exits 0 for After and 1 for each expected red config. The scoped TLC runner checks that every broken rule matches `expected.tsv`.
