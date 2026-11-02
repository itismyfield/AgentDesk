# InputOwnership — who answers a Discord input pasted into a Claude pane (#7676 A2+D)

Code: a1c `0b0fbcbfbe` (`Fix = "main"`, `InputOwnershipMain.cfg`) and the A2+D change on top of it
(`Fix = "a2d"`, `InputOwnership.cfg`). One Discord input, one Claude tmux channel in turn mode or
legacy mode (chosen at Init), mailbox idle/active and transcript idle/busy in every combination,
a native turn AgentDesk did not start (its end may not wake the owner drive), the receipt thread's
3 s window, and the relay reading the input's row once.

Run (exit 1 means a rule breaks):

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart InputOwnership.tla --config InputOwnershipMain.cfg --view ownership.view.mjs --out /tmp/sc-own-main
~/.claude/skills/statechart/scripts/statechart InputOwnership.tla --config InputOwnership.cfg --view ownership.view.mjs --out /tmp/sc-own-a2d
```

## Rules

| Rule | Reader's sentence | main | a2d |
|---|---|---|---|
| NoDuplicate | The input is pasted at most once. | holds | holds |
| NoRetryRequired | No input ends asking the person to send it again. | breaks (1 step) | holds |
| AnswerOnce | An answered input was posted once: O writer only on turn mode, the legacy relay or the declared reader on legacy. | breaks (5 steps) | holds |
| HandedHasRow | Time alone never makes the input Handed. | holds | holds |
| NoSilentLoss | Once nothing else happens, the input was answered. | breaks (3 steps) | breaks (3 steps, same path) |

`ownership.chart.mmd` is the main chart (bug paths only; 748 states).

## Counterexamples, replayed against the code

1. **NoRetryRequired, main** — mailbox active, transcript idle: `hand_busy` → `inject_owned` was
   `BusyOnly` (`claude_tui/busy_inject/submit.rs:30`), `NotBusy` (`busy_inject.rs:457-461`) is not
   `MODAL`, so `effect::run` calls `retain_not_sent` (`input_owner/effect.rs:66-74`). Code-confirmed.
   Fixed: the owned entry is `AnyTurn`.
2. **AnswerOnce, main (legacy)** — the busy paste is queued, the turn ends, the queued prompt opens
   its own turn; the frame is known to history, so `claim_injected_steer` returned
   `Joined { first: false }` for every Claude frame (`tui_prompt_dedupe/injected_steer.rs:131`),
   `observe_inner` returns `InjectedSteer` before any owner is chosen. Code-confirmed up to the
   claim; the zero relay is shown by `claude_input_answerer_is_read_from_its_row` failing on main
   (legacy busy cells publish 0). Fixed by the row-based Claude claim.
3. **NoSilentLoss, main and a2d** — idle pane whose observer cannot be built: `take_owned` refuses
   `idle_unobserved` (`provider_dispatch/owner_input.rs`), the front stays, and a terminal turn
   AgentDesk did not start may end without waking the drive. The Claude observer is absent only
   when the transcript cannot be resolved (`claude_tui/hosting/warm_followup.rs:567-573`), so A2+D
   does not paste there (no source proves the target). Code-confirmed as the remaining event-wake
   gap of the A1 review (P2-1), owned by slice C. Not an A2+D acceptance test.

## Mutants of the a2d model (all break a rule besides the known NoSilentLoss gap)

| Mut | What it removes | Breaks |
|---|---|---|
| pendOnBusy | the owned busy entry declares an Answer nobody reads | AnswerOnce |
| blanketJoin | Claude frames joined whatever the row | AnswerOnce |
| receiptKind | ownership read from the 3 s receipt kind | AnswerOnce |
| timeHanded | Handed after the window with only an enqueue | HandedHasRow, AnswerOnce |
| turnLegacy | the legacy relay also answers on turn mode | AnswerOnce |

Left out: Codex and Herdr panes (A2 keeps their hold), modal/capture holds (slice C wakes),
restart replay of the claim ledger, two inputs racing one turn.
