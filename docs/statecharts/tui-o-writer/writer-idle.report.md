# 출력 writer 유휴 대기

기준 코드 `b5609d74b37a3b9d9851181071b2d359fe058c46`의 `writer/actor.rs:306, 470–488`은
1초마다 본문을 실행한다. Codex 제출 mailbox는 이미 수신으로 깨어나며, 다른 깨우기
경로에는 고정 timer를 쓴다. 이 모델의 명령은 기존 timer를 기다리던 transition mailbox를 뜻한다.

| 구성 | 입력·hook·transition 명령 즉시 깨우기 | spool·transcript 최대 15초 | 들어온 변화는 결국 처리 |
| --- | --- | --- | --- |
| `WriterIdleBefore.cfg` | 위반 | 충족 | 충족 |
| `WriterIdleBackoff.cfg` | 충족 | 충족 | 충족 |
| `WriterIdleNoMaxTimer.cfg` | 충족 | 경과 시간이 멈춘 반례 | 위반 |

TLC 1.7.4에서 세 구성의 실제 위반이 `expected.tsv`와 일치했다. 백오프 구성은
`IdlePromptWake`, `IdleWithinMax`, `IdleEventuallyProcesses`를 모두 충족했다.
최대 timer를 없애면 15초 단계에서 남은 시간이 0인 채 spool 또는 transcript가
보류되어 무한히 머문다. 숫자 불변식만으로 이 정지를 검출할 수 없어 liveness를 함께 검사한다.

한 변화가 어느 timer 구간에서든 들어올 수 있다. pending 신호가 있으면 다음 동작은
처리이며, timer와 처리에는 약한 공정성을 둔다. Rust에서는 `Notify::notify_one`이
대기 future 생성 전 신호도 permit으로 보존한다. Binding watch와 mailbox는 각각
변경 버전과 큐를 보존한다. 한 차례 깨어나면 같은 채널에 함께 도착한 작업도 원래 순서로 본다.
시간 상한은 대기 구간을 모델링하며, 디스크·네트워크 동작 완료까지의 실시간 상한은 아니다.

Spool 세대는 하나의 writer가 소유한 `ChannelStore`의 성공한 spool 변경에서 전진한다.
기동 시 기존 spool 재생과 내구성 검사를 수행한다. 운영 중 별도의 spool writer를
전제하지 않는다. transcript 메타데이터는 파일 교체와 길이 감소를 구분한다.
메타데이터 외 의존성과 시간 기반 정리는 최대 15초마다 정상 본문에서 확인한다.

[모델](WriterIdle.tla) · [상태 차트](writer-idle.chart.mmd)
