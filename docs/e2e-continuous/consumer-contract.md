# Continuous E2E 소비자와 배포 요청

저장소 루트에서 실행하거나 `PYTHONPATH`에 저장소 루트를 지정한다.
소비자는 상태와 요약만 읽으며, 설정·인증·운영 API를 읽지 않는다.

```sh
python3 -m scripts.e2e.continuous.consumer --status-file /mirror/status.json --head <full-sha>
```

`--status-file` 기본값은 `BC_E2E_STATUS_FILE`, `--head` 기본값은 `BC_E2E_HEAD`다.
표준 status schema1 파일은 같은 디렉터리의 `summary.json`을 읽는다.
`--summary-file`로 다른 요약 파일을 명시할 수 있다.
원격 호스트에서 원자적으로 전달할 단일 파일은 다음 envelope를 사용한다.

```json
{"schema_version": 1, "status": {"schema_version": 1}, "summary": null}
```

`status`는 supervisor가 발행한 전체 status, `summary`는 전체 continuous schema1 요약이다.
위 예시는 envelope 모양만 보이며 유효한 실행 증거가 아니다.
판정은 `summary.consume`, 출력은 `summary.format_lines`가 담당한다.
표준 출력은 `E2E_CHECK`와 `E2E_MONITOR` 두 줄이며, 읽기 실패도 종료 코드 0이다.

- 상태·요약 모두 미도착: `NO_DATA`.
- 명시 off: `DISABLED`.
- 호스트/권한/읽기 오류: `UNKNOWN`, `HOST_UNAVAILABLE`.
- 활성 상태에서 비교할 head가 없거나 full SHA가 아님: `UNKNOWN`, `HEAD_REQUIRED`.
- JSON null·형식 오류·미지원 schema·미래 시각·오래된 head는 PASS가 아니다.

배포 companion은 다음 명령으로 비밀 없는 요청만 남긴다.

```sh
python3 -m scripts.e2e.continuous.companion --state-dir /test/state --request-id <safe-id> --deploy-sha <full-sha>
```

이 명령은 `inbox.submit`을 재사용한다. 파일에는 schema version·request ID·deploy SHA·접수 시각만 담는다.
같은 ID의 재전달은 원래 요청을 보존하며 provider 턴이나 runner를 직접 실행하지 않는다.
요청 거절·전달 실패는 JSON `REJECTED`와 종료 코드 0으로 남겨 기존 배포 결과를 보존한다.
실제 시험 호스트로의 전달과 배포 후 호출은 coordinator 배선에서 수행한다.
