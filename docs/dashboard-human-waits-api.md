# 서버 캠페인 사람 대기 목록

`GET /api/human-waits`는 열린 서버 캠페인의 사람 대기를 한 응답에 담는다. 기존 대시보드 인증을 사용한다. 요청 본문과 쿼리 매개변수는 없다.

```json
{
  "waits": [
    {
      "source": "server",
      "campaign_id": "release",
      "node_id": "verify",
      "title": "배포 검증",
      "campaign_title": "릴리스",
      "issue_url": "https://github.com/itismyfield/AgentDesk/issues/7438",
      "human_status": "awaiting_approval",
      "wait_started_at": "2026-11-19T00:00:00Z"
    }
  ]
}
```

`title`은 노드 제목이다. 제목과 링크는 해당 캠페인을 읽을 때의 정의에서 가져온다. 정렬은 `campaign_id`, `node_id` 오름차순이다. 목록은 캠페인 상세를 따로 호출할 필요가 없으며, 내부 페이지가 여러 개여도 끝까지 읽는다. 원장 캠페인의 사람 대기는 별도 기능이다.

`human_status`는 캠페인 상세와 같은 `decision_required`, `awaiting_approval`, `awaiting_deploy`, `awaiting_start` 중 하나다. 홈 할 일에서는 판단·승인·배포 대기를 골라 표시할 수 있다. `wait_started_at`은 같은 캠페인 스냅숏의 이벤트 순번까지 현재 승인 바인딩과 일치하는 최신 게이트 개방 시각(RFC 3339)이다. 개방 이벤트나 바인딩이 없으면 `null`이다. 조회 중 동시 변경은 캠페인마다 관측 시점이 다를 수 있다.

응답은 `Cache-Control: no-store`를 사용한다. 대기가 0건이면 `200`과 `{"waits":[]}`를 반환한다. PostgreSQL 풀이 없거나 연결할 수 없으면 `503`, 기타 조회나 해석 실패는 `500`의 표준 JSON 오류를 반환한다.

서버에서 제공하는 예시는 `/api/docs/automation/campaigns`에도 있다.
