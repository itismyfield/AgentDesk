# 판정 API 격리 수용 측정기

macOS/Linux 저장소 루트에서 실행한다. 기본 시험에는 실행되지 않는 example이다.

키 없이 먼저 빌드한다.

```sh
unset TMUX TMUX_PANE
unset GEMINI_API_KEY ADK_CARGO_CLASS
export CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none
cargo build --offline -p agentdesk --example decision_gate_acceptance
```

그다음 코디네이터가 키를 주입한 격리 프로세스에서 빌드된 바이너리를 실행한다.

```sh
target/debug/examples/decision_gate_acceptance --out /absolute/output/directory
```

키는 실행 프로세스에 주입된 `GEMINI_API_KEY`에서만 읽는다. 없거나 비어 있으면
`SKIPPED_NO_KEY`와 rc 0으로 종료하며 출력 디렉터리도 만들지 않는다.
키 조회 CLI, 운영 설정, 실제 GitHub, DB, issue 생성은 사용하지 않는다.
코디네이터가 승인된 격리 실행 환경에서 키를 주입한다. 키를 인자에 넣지 않는다.

다음 명령은 키를 읽지 않고 loopback 가짜 서버로 전체 출력 경로를 검증한다.

```sh
unset TMUX TMUX_PANE
unset GEMINI_API_KEY ADK_CARGO_CLASS
export CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none
cargo run -p agentdesk --example decision_gate_acceptance -- --self-test --out /absolute/loopback-output
```

출력은 `--out`에 지정한 디렉터리에만 기록한다. 서로 다른 실행에는 새 디렉터리를 쓴다.

| 파일 | 내용 |
| --- | --- |
| `raw-rest-body.json` | 네 질문을 한 REST 요청으로 보낸 성공 응답 JSON body |
| `comment_shape.json` | 기존 fixture와 같은 provenance/choice/noul/score/choice_255 구조 |
| `capture-manifest.json` | 모델, template, 수집 시각, 요청·fixture·body SHA256, 질문 대응 |
| `acceptance.csv` | 표본별 라벨·길이·시간·결과·확률·409/create/fallback·오탐/미탐 |
| `acceptance.md` | 같은 수용표와 0.8 집계·큰 후보 집합 시간/실패 집계 |
| `probabilities.json` | 유효한 Answer의 전체 선택지 확률표. 무응답은 빈 map |
| `summary.json` | 수용표 집계의 기계 판독본 |

원시 캡처는 30초 상한의 batch 1요청이며 누락 필드를 채우지 않는다. 표본은
각각 production 정책의 5초 deadline 안에서 한 질문·한 요청을 사용한다.
batch 지연을 표본 지연으로 합산하지 않는다. 재시도와 redirect는 없다.

`sources.rs`는 기존 공통 판정 소스와 설정 DTO·redact 구현을 그대로 포함한다.
`runtime/`의 상대 심볼릭 링크는 원본의 하위 모듈 탐색 경로를 보존한다. 비Unix
빌드는 이 harness 모듈을 제외해 기본 빌드가 심볼릭 링크 지원에 의존하지 않는다.
실제 라우트의 `issue_duplicate::check_with_fetch` 소스에 고정 후보 closure를
주입한다. 관찰 wrapper는 공통 gate의 observation을 보관하고 결과는 그대로
반환한다. 실제 표본은 원래 gate의 HTTP 경로로 직접 전송하며, 관찰 wrapper는
검증된 typed Answer만 확률표에 투영한다. loopback 중계는 원시 batch 캡처와
가짜 서버 검증에만 쓰고 오류 body·헤더를 보존하지 않는다. 실측 모드의 유일한
외부 목적지는 고정된 Gemini Decisions REST 주소다.

후보 fetch 시간은 메모리에서 고정 목록을 복제한 시간이다. GitHub 성능 측정이 아니다.
선택 확률 0.8에서의 판정은 실제 소비자 결과를 쓰며 별도로 임계값 판정을 복제하지
않는다. Off/NoAnswer는 fallback이고 정확한 신규 또는 유효한 Answer의 미탐으로
세지 않는다. 다른 중복 후보를 돌려준 경우는 `wrong_candidate`로 별도 집계한다.
실제 issue 생성·409 HTTP 응답 전송의 부수효과 시험은 기존 소비자 시험이 담당한다.

loopback 성공은 `LOOPBACK_E2E_OK`로 출력한다. `.8/.799999`, confidence 독립,
잘못된 분포, 인증 오류, 성공 응답의 비밀 sentinel이 출력에 없는지를 검증한다.
실측은 `ACCEPTANCE_MEASURED`이며 품질 PASS를 자동 선언하지 않는다.

기존 tracked `comment_shape.json`은 수정하지 않는다. 캡처 fixture의 분류는
코디네이터 대조 대기이며 fake 출력에는 `loopback_fake_only`가 남는다. 실제
REST의 값·필드와 기존 parser 시험의 고정값 단언을 코디네이터가 대조한 뒤
fixture를 교체한다. 현재 시험은 합성 표식과 정확한 관측 예시 수치를 단언하므로
형식이 같은 실제 응답이라도 수치·표식 대조 없이 그대로 통과한다고 보장하지 않는다.
