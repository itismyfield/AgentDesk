# 출력 writer 유휴 관측

고정 주기 코드 `82e9662e4ca964a3e113c885be8aaeaf20216992`는
`writer/actor/publishing.rs`에서 1초마다 다음 관측을 예약한다.
앞선 백오프의 코드 기준은 `c85323730d6a9e10730eb5ce19463f8a0274204d`다.
원래 고정 주기는 `1f61b76313ebc6219a0c4cb6117e25582aa57a1d`의
`writer/actor.rs:446, 472–488`에서 본문과 transition mailbox를 매번 처리했다.

| 구성 | 입력·hook 즉시 깨우기 | 알림 없는 변화 1초 관측 | 들어온 변화는 결국 처리 |
| --- | --- | --- | --- |
| `WriterIdleBefore.cfg` | 위반 | 충족 | 충족 |
| `WriterIdleBackoff.cfg` | 충족 | 위반 | 충족 |
| `WriterIdleFixed.cfg` | 충족 | 충족 | 충족 |
| `WriterIdleNoMaxTimer.cfg` | 충족 | 이전 15초 단계에서 정지 | 위반 |
| `WriterIdleNoTimer.cfg` | 충족 | 고정 주기의 timer 제거 시 정지 | 위반 |

TLC 1.7.4는 다섯 구성에서 예상한 규칙을 확인했다. 백오프는
`IdleWithinOne`을 깨지만 종전 15초 관측 상한은 충족한다. 두 timer 제거
구성에서는 시간 흐름도 멈춰 수치 상한만으로 실패를 검출할 수 없다.
`IdleEventuallyProcesses`는 보류한 spool·transcript·명령이 끝내 처리되지 않는 반례를 검출한다.

모델은 스탬프가 이미 찍힌 한 유휴 채널에 한 변화가 들어오는 경우를 다룬다.
입력과 hook은 대기 future 생성 전에도 `Notify::notify_one` permit 또는
binding watch 버전을 남긴다. transition 명령은 bounded mailbox에 남아
다음 기존 관측에서 소비된다. timer와 관측에는 약한 공정성을 둔다.
1초는 다음 관측까지의 대기 한도이며 디스크·네트워크 동작의 완료 시간은 별도다.

Rust는 작업 전 transcript 메타데이터와 spool 세대를 샘플링한다.
입력·hook·ownership 변화는 이전 스탬프를 무효화한다.
소유 writer의 spool 커밋을 세대로 확인하고, 파일 교체와 길이 감소는 메타데이터로 구분한다.
이전 spool 재생과 내구성 검사는 기동 때 수행한다. 미완료 binding과 원본 정리
의무가 남은 동안에는 1초 관측에서 처리 기회를 갖는다.

[모델](WriterIdle.tla) · [상태 차트](writer-idle.chart.mmd)
