# Codex busy-turn injection canary

Codex busy-turn injection uses the existing `ADK_BUSY_INJECT_DISCORD_CHANNELS`
gate. The capability constant is enabled, and the requested channel must pass
that gate. No additional configuration file or environment key is used.

The gate is cached once per process. Changing its environment value requires
replacing the release process through the existing release procedure. There is
no hot reload or instantaneous revocation of an already pasted input.

## Channel scope

- An unset, empty or invalid list disables the gate. `*` admits every channel
  that otherwise meets the existing gate checks.
- For Codex capability, the gate receives the requested channel and no parent.
  Listing a parent admits its threads to Discord intake, but a Codex thread
  must independently pass the capability gate.
- Windows and Herdr exclusions remain in the existing gate. Native turn,
  full-access context, draft, modal, queue and holder checks still apply.
- Claude and the Claude-only queued-head capability keep their existing paths.
- External input modes do not bypass the Codex channel gate.

## 즉시 활성화 — 카나리 생략

사용자 결정(2026-10-14)에 따라 구현 착지 즉시 활성화하며 카나리는 생략한다.
이미 목록에 있는 Codex 채널은 C4 바이너리가 뜨는 순간 활성화된다.

1. 첫 C4 프로세스를 실행하기 전에 각 실행 노드의 기존 목록과 그 목록이
   허용하는 실제 Codex 채널 전체를 대조한다. `*`와 기존 등록 채널도 포함한다.
2. 정본 `~/.adk/release/config/launchd.env`에서 기존 ID를 보존하면서 승인된
   채널만 추가한다. `ADK_BUSY_INJECT`는 변경하지 않는다.
3. 정본에서 release 서비스 정의를 재생성하고 기존의 안전한 release 절차로
   프로세스를 교체한다. 환경 파일 수정만으로는 실행 중인 gate가 바뀌지 않는다.
4. 새 SHA·PID·시작 시각·유효 목록을 확인한 뒤 활성화 완료로 판정한다.
   미확정 입력은 재전송하지 않고 아래 rollback 절차를 따른다.

아래 카나리·n≥10·J8 절은 선택적인 측정 절차이며 활성화의 선행조건이 아니다.
실행하지 않은 측정은 PASS로 기록하지 않는다.

## Optional measurement: before a live canary

These are deployment checks for the coordinator; implementation tests do not
establish their live values.

1. Record the deployed SHA, process PID, start time and execution node.
2. Confirm the effective existing gate on every execution node. A one-channel
   canary requires a list other than `*` and no other active direct Codex
   channel covered by that list. Preserve existing channel IDs.
3. Confirm the target is direct tmux Codex with full-access native context,
   and record its session ID, native rollout path and starting offset.
4. Preserve the initial Discord snapshot and inspect health, lease and queue.
5. Require independently validated J8 measurement tooling before claiming a
   live J8 result. Unit and PG fixtures alone cannot establish that result.

## Optional measurement: sample and verdict

Start A through real Discord input, confirm its native model turn is busy,
then submit B once through Discord. Preserve native and Discord evidence for
every submitted B across at least ten samples.

`Injected` can also mean B opened a new turn after A ended. It does not prove
that B joined A. A J8 pass requires every sample to show `joined=true`, the
observed turn equal to the target, one accepted original input, one complete
final answer per target, the B marker once, and no duplicate answer, input
echo, synthetic placeholder or missing delivery. The final state must be
idle with an empty queue and no unconfirmed or unfinished sample.

Missing logs, native ranges or Discord pages make the result incomplete.
An observed `joined=false` is a failed join sample. Do not exclude vetoed,
new-turn or unconfirmed samples from the submitted sample set.

## Stop and rollback

Stop submitting inputs and preserve their nonce/turn mapping, native ranges,
logs and Discord snapshots. Do not resend an uncertain B.

Remove only the canary channel from the existing gate through the approved
release configuration procedure, preserve the other IDs, regenerate the
release service definition and replace the process. Confirm the new PID,
effective gate and unlisted-channel refusal. A restart that retains the old
loaded environment does not establish revocation.

The new process blocks further unlisted attempts before pane I/O. It does
not cancel an earlier paste or convert `Unconfirmed` into a resend. Returning
the capability constant to false is a separate code rollback if needed.
