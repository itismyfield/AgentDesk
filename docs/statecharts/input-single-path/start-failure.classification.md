Fixed classification at owner settlement:

| 결과 | 분류 |
|---|---|
| reserve commit 이후의 모든 결과 | EffectOwned |
| drive handoff 대기(owner_input.rs:167–193), claim 실패(:514–523), owner_front_not_admitted 전체, headless claim 실패(headless_turn.rs:214–224), 비allowlist HerdrRefusal, wrapper 안의 NotSent 결과 | SchedulingHeld — 횟수 미포함, 슬롯 미소비. 오늘 동작 그대로 |
| allowlist 4종(host_refusal.rs:28–37에서 typed 보존) | ConfirmedKind, 첫 실패에서 보관 |
| wrapper 생성 뒤 reserve 전의 비NotSent Err(Herdr gate 미확인, context I/O, tmux 생성 실패 등), panic, (Err(_), Ok(_)) 무응답 | StartFailedBeforeInput — 2회 예산 |
| Forwarded / ConsumedToHold / SkippedDuplicate | 다른 경로가 이미 소비했을 수 있으므로 KnownNotSent 보관 금지 |

Allowlist: ProviderUnsupported, EndpointNotLocal, actual ExecutorNotWired after switch-on,
HostedHerdrUnconfigured { state: Some(Pending | Bound) } after automatic Return.
Preclaim NotSent("herdr_root_recovery_held") is SchedulingHeld; S2 owns its completion wake.
Inside-start Herdr bound-gate failure is StartFailedBeforeInput regardless of its string.
The S2 case below emulates only its NotSent frame, not the preclaim recovery itself; its live integration is covered by S2 tests.
