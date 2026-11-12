# S1 관리 표식 관문 모델

기존 설계의 NamingFixed cfg는 S2 승격 projection이고 S1 표식 cfg는 없었다. 별도 SessionIdentityGate는 legacy 여부, 표식 6종(absent/same/foreign/malformed/unset/probe_failed), send/paste/kill/Pane 4종에 대한 마지막 관문만 모델링한다. lifecycle·DB·서버 교체·실제 프로세스 경합은 이 모델의 증명 범위가 아니다.

| 구성 | 결과 | generated/distinct | depth | queue |
|---|---|---:|---:|---:|
| Before (guard 없음) | NoForeignMutation 반례 | 49/49 | 2 | 47 (반례로 중단) |
| After (guard 있음) | 불변식 유지·완주 | 96/96 | 2 | 0 |

2026-11-12 18:31:51 KST, TLC 1.7.4, Java UseParallelGC, workers=1. 시작~종료 1분 이내. 원시 로그·jar SHA는 lane 보고서 증거 목록에 기록한다. Rust 대상 시험이 실제 명령 경로와 pane/PID 보존을 따로 검증한다.
