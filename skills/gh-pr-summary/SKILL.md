---
description: PR 메타데이터(상태/리뷰/체크/diff 요약)를 AgentDesk 캐시 엔드포인트로 조회한다. 반복 `gh pr view` 호출을 대체하고 PR head SHA 기반으로 idempotent. 예) /gh-pr-summary itismyfield/AgentDesk 1229
---

# gh-pr-summary — PR 요약 캐시

## 목적
에이전트가 같은 PR을 반복 조회할 때 매번 `gh pr view --json …` 을 호출하면
대량 토큰 + 외부 네트워크 비용이 누적된다 (감사 보고: 메가-세션 `gh pr × 1229`).
이 스킬은 AgentDesk 컨트롤 플레인의 `/api/github/pr-summary` 엔드포인트를 호출해
동일 PR head SHA 에 대해 응답을 재사용한다.

## 입력
`$ARGUMENTS`: `<owner/repo> <pr_number> [--force] [--head-sha=<sha>]`

옵션:
- `--force` — 캐시 무시하고 GitHub에 재조회
- `--head-sha=<sha>` — 호출자가 알고 있는 현재 head SHA. 캐시 엔트리의 SHA가
  일치하지 않으면 자동 재조회
- `--field=<name>` — 응답에서 특정 필드만 추출 (예: `state`, `reviews`, `files`)

## 백엔드 호출

엔드포인트:
- `GET  /api/github/pr-summary?repo=<repo>&pr=<n>[&force_refresh=true][&expected_head_sha=<sha>]`
- `POST /api/github/pr-summary/invalidate` — 본문 `{"repo": "...", "pr": <n>}`

응답:
```json
{
  "repo": "itismyfield/agentdesk",
  "pr": 1229,
  "cache_hit": true,
  "age_seconds": 17,
  "head_sha": "abc123…",
  "view": { /* gh pr view --json 페이로드 (state, title, reviews, files, checks 등) */ }
}
```

`cache_hit=true` 인 경우 GitHub 호출 없이 메모리 응답. `false` 이면 새로 `gh` 호출 후 캐시 갱신.

## 실행 전략

### 1. 베이스 URL 확인
`http://127.0.0.1:8791` (release) 또는 `agentdesk.yaml`의 `server.host`/`port`.
필요한 인증 토큰은 `~/.adk/release/config/agentdesk.yaml`의 `server.auth_token`을 따른다.

### 2. 호출 예
```bash
curl -s "http://127.0.0.1:8791/api/github/pr-summary?repo=itismyfield/AgentDesk&pr=1229" \
  -H "Authorization: Bearer $AGENTDESK_AUTH_TOKEN" | jq '.view | {state, title, isDraft, mergeable}'
```

`--force` 가 주어지면:
```bash
curl -s ".../pr-summary?repo=…&pr=…&force_refresh=true" …
```

`--head-sha=<sha>` 가 주어지면 `expected_head_sha=<sha>` 쿼리 파라미터를 추가한다.

### 3. 결과 표시
사용자에게 `cache_hit` / `age_seconds` 디스클로저를 한 줄로 노출한다.
예: `(cache hit, 17s ago)` / `(fresh fetch from gh)`.

### 4. 무효화
PR 에 push 가 일어났거나 리뷰가 추가됐다는 신호를 다른 출처(웹훅, kanban 이벤트)에서
받았다면 무효화 호출:
```bash
curl -s -X POST ".../pr-summary/invalidate" \
  -H "Authorization: Bearer $AGENTDESK_AUTH_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"repo":"itismyfield/AgentDesk","pr":1229}'
```

## 캐시 동작 (참고)
- 키: `(repo_lowercased, pr_number)` — 케이싱 다른 입력은 같은 엔트리로 합쳐진다.
- TTL: OPEN = 60s, CLOSED/MERGED = 1h (서버 기본값).
- 용량 제한: 512 PR. 초과 시 oldest fetched_at 부터 단일 evict.
- head SHA 기반 idempotent: `expected_head_sha` 와 캐시 SHA 가 다르면 자동 재조회.
- 실패한 refresh 는 기존 엔트리를 파괴하지 않는다 (transient `gh` 실패에 강건).

## 주의
- `gh pr view` 직접 호출 금지. 본 스킬 사용. 직접 `gh` 호출 시 메가-세션 토큰 폭발 (#2654 참조).
- 비공개 레포의 경우 AgentDesk 가 실제로 `gh` 를 호출할 때 인증되어야 한다.
- 캐시는 in-memory 이므로 dcserver 재시작 시 비워진다 (이는 의도된 동작).
