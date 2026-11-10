# 입력 처리 퍼징·속성 테스트 발견

프로덕션 코드는 고치지 않았다. 아래는 속성 테스트가 최소화한 입력으로 확인한 동작이다. cargo-fuzz 결과는 이 파일 맨 아래에 이어서 적는다.

## 1. `tail_with_ellipsis`가 길이 0을 한 글자로 넘긴다

- 위치: `src/utils/format.rs:62`
- 입력: `text = "a"`, `max_chars = 0`
- 관찰: 반환값은 `"…"`이고, 문자 수는 1이다. 함수 주석은 결과를 `max_chars` 글자 이내로 자른다고 적혀 있다.
- 원인: `max_chars <= 1`이면 본문 예산 없이 바로 `"…"`를 반환한다. 예산이 1일 때는 맞지만, 0일 때는 한 글자를 초과한다.
- 영향: 현재 호출부 `recovery_engine/state_extractors.rs`는 160을 넘긴다. 예산 0을 넣는 경로가 생기면 상태 표시가 요청한 빈 문자열 대신 말줄임표를 남긴다.
- 제안: `max_chars == 0`이면 빈 문자열을 반환한다. `max_chars == 1`만 `"…"`로 둔다.

재현 바이트: `artifacts/inputs/tail_ellipsis_zero.txt` (`a`) 와 인수 `0`.

## 2. ANSI 스트립이 멱등이 아니다. CR이 ESC와 `[` 사이에 있으면 두 번째 호출이 CSI를 지운다

- 위치: `src/services/tui_input/ansi.rs:29` (CSI 인식), `src/services/tui_input/ansi.rs:67` (CR 제거)
- 입력: `"\u{1b}\r["` (바이트 `1B 0D 5B`)
- 관찰: 첫 호출은 `"\u{1b}["`를 남긴다. 그 결과에 같은 함수를 다시 적용하면 `""`가 된다.
- 원인: CSI는 ESC 바로 다음의 `[`만 본다. 그 사이의 CR은 텍스트로 남긴 뒤, 마지막에만 걸러 낸다. 그래서 첫 결과는 진짜 CSI처럼 보이고, 두 번째 통과가 그 뒤를 통째로 삼킨다. 닫히지 않은 CSI는 나머지 캡처를 버린다 (`ansi.rs:32`).
- 영향: 캡처를 두 번 씻거나, 이미 씻은 문자열을 다시 넣으면 프롬프트·본문이 사라질 수 있다. 한 번만 호출하는 경로에서는 첫 결과의 ESC가 그대로 남는다.
- 제안: CSI/OSC를 보기 전에 CR을 건너뛰거나, CR을 제거한 뒤 한 번만 이스케이프를 인식한다. 인식 결과에는 ESC가 남지 않게 해서 함수가 자기 출력에 대해 고정점이 되게 한다.

재현 파일: `artifacts/inputs/ansi_cr_before_csi.bin`

## 3. `normalize_empty_lines`가 빈 줄 하나를 빈 줄 두 개로 만든다

- 위치: `src/services/discord/formatting/tool_markdown.rs:242`
- 입력: `"\n "` (개행 하나, 그 다음 공백)
- 관찰: 반환값은 `"\n\n "`이다. `lines()`로 나누면 빈 줄, 빈 줄, `" "`이다. 주석은 연속 빈 줄을 최대 한 줄로 줄인다고 적혀 있다. 같은 함수를 한 번 더 호출해도 `"\n\n "`라서, 잘못된 결과가 그대로 고정된다.
- 원인: 빈 줄 분기는 개행을 하나 넣고, 다음 비어 있지 않은 줄은 버퍼가 비어 있지 않다는 이유로 개행을 한 번 더 넣는다.
- 영향: 도구 출력·Discord 본문의 선행 빈 줄이 두 줄이 된다. 포매터가 이 함수를 거치면 단락 간격이 늘어난다.
- 제안: 빈 줄에서 이미 구분자를 썼으면, 다음 내용 줄에서는 개행을 다시 넣지 않는다.

재현 파일: `artifacts/inputs/normalize_empty_lines.txt`

## 4. 코드 펜스 안의 CR 한 줄이 `lines()`와 `join("\n")` 사이에서 사라진다

- 위치: `src/services/discord/formatting/tool_markdown.rs:530` (`convert_markdown_tables`), 이어서 `src/services/discord/formatting/streaming_status.rs:271` (`format_for_discord`)
- 입력: `"\r\r"`. 펜스로 감싸면 `"```\n\r\r\n```"`
- 관찰: `format_for_discord` 결과는 `"```\n\n```"`이다. 가운데에 있던 CR 줄이 없다.
- 원인: `str::lines()`는 `"```\n\r\r\n```"`의 가운데 줄을 `"\r"`로 본다 (`\r\n`만 줄 끝). `convert_markdown_tables`가 그 줄을 `"\n"`으로 이어 붙이면 `"```\n\r\n```"`가 되고, 다음 `lines()`는 그 `\r\n`을 줄 끝 하나로 먹어 가운데 줄을 삭제한다.
- 영향: 터미널 캡처의 CR이 코드 블록 한 줄을 지운다. 표 변환과 본문 포맷이 둘 다 같은 패턴이라 한 번 더 지나면 내용이 줄어든다.
- 제안: 줄을 이어 붙이기 전에 각 줄의 `\r`을 제거한다. CR만 있는 줄은 빈 줄로 남기고, 다음 단계가 그 줄을 줄 끝으로 오인하지 않게 한다.

재현 파일: `artifacts/inputs/cr_line_in_fence.txt`

## 5. `format_for_discord`가 끝의 빈 줄과 CR을 한 번만 남긴다

- 위치: `src/services/discord/formatting/streaming_status.rs:302` (빈 줄 접기). 퍼저가 멈춘 단언은 `src/services/discord/formatting/input_properties.rs`의 `check_format`.
- 입력: 바이트 `67 0a 0a 0d`, 문자열 `g\n\n\r`. libFuzzer 아티팩트 Base64는 `ZwoKDQ==`.
- 관찰: 첫 `format_for_discord`는 `"g\n"`을 반환한다. 그 결과에 같은 함수를 다시 적용하면 `"g"`가 된다. 실행 80회 만에 타깃 `discord_message_text`가 종료됐다 (exit 77).
- 원인: `"\r"`만 있는 줄은 `trim()`이 비어 있다고 본다. 접기는 그 줄을 개행 하나로 줄여 `"g\n"`을 만든다. `lines()`는 문자열이 개행 하나로 끝나면 빈 줄을 돌려주지 않으므로, 두 번째 호출은 그 개행을 지운다.
- 영향: 포맷을 두 번 거치면 끝의 개행이 사라진다. 본문 `g`는 남는다. 앞의 4번과 같이 CR을 줄 내용으로 보면 다음 `lines()`가 줄을 먹는다.
- 제안: 결과를 만들기 전에 각 줄의 `\r`을 제거하고, 끝의 빈 줄은 항상 버리거나 항상 남겨서 한 번의 출력이 다음 입력의 고정점이 되게 한다.

재현 파일: `artifacts/inputs/format_not_idempotent.bin`

## cargo-fuzz 실행

`cargo fuzz build --sanitizer none`은 기본 `-Ccodegen-units=1`과 libFuzzer C++ 빌드에서 메모리 한도와 `libstdc++` 링크에 막혔다. 같은 libFuzzer 하네스를 커버리지 계측(`sancov`, inline 8-bit counters, pc-table)과 `--cfg fuzzing`으로 직접 빌드한 뒤 각 바이너리를 180초씩 돌렸다. 주소 새니타이저는 쓰지 않았다. 입력 상한은 libFuzzer 기본 4096바이트, 케이스 타임아웃 8초, RSS 상한 2048MB.

| 타깃 | 시간 | 실행 수 | 결과 |
| --- | --- | --- | --- |
| discord_message_text | 즉시 중단 | 80 | 발견 5. `g\n\n\r`에서 포맷 멱등 단언 실패 |
| discord_commands | 180초 | 15,370,606 | 크래시 없음 |
| ansi_and_frames | 180초 | 9,325,583 | 크래시 없음 |
| prompt_extract | 180초 | 22,462,499 | 크래시 없음 |
| rollout_and_codecs | 180초 | 11,806,015 | 크래시 없음 |
| config_yaml | 180초 | 9,253,733 | 크래시 없음 |
| redact_and_fences | 180초 | 1,388,941 | 크래시 없음 |
| outbound_and_chunks | 180초 | 28,993,192 | 크래시 없음 |

`discord_message_text`는 발견 5 때문에 나머지 검사를 오래 돌리지 못했다. 그 검사들은 프로퍼티 테스트 2,000케이스에서 통과했다. 나머지 일곱 타깃은 3분 동안 새 크래시가 없었다.

프로퍼티 테스트는 `PROPTEST_CASES=2000`으로 `cargo test --lib input_properties`를 돌렸고, 22개 테스트가 27.81초에 통과했다. 그 전에 더 짧은 실행과 위의 퍼저 실행이 발견 1–5를 최소화했다.
