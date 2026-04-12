import { describe, expect, it } from "vitest";
import type { GitHubComment } from "../../api";
import type { KanbanCard } from "../../types";
import {
  coalesceGitHubCommentTimeline,
  getCardDwellBadge,
  getCardStateEnteredAt,
  parseGitHubCommentTimeline,
} from "./kanban-utils";

function makeComment(
  body: string,
  author = "itismyfield",
  createdAt = "2026-03-23T09:00:00Z",
): GitHubComment {
  return {
    author: { login: author },
    body,
    createdAt,
  };
}

function makeCard(overrides: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id: "card-1",
    title: "Test card",
    description: null,
    status: "in_progress",
    github_repo: "itismyfield/AgentDesk",
    owner_agent_id: null,
    requester_agent_id: null,
    assignee_agent_id: "agent-1",
    parent_card_id: null,
    latest_dispatch_id: "dispatch-1",
    sort_order: 10,
    priority: "medium",
    depth: 0,
    blocked_reason: null,
    review_notes: null,
    github_issue_number: 540,
    github_issue_url: null,
    metadata_json: null,
    pipeline_stage_id: null,
    review_status: null,
    created_at: 1_710_000_000_000,
    updated_at: 1_710_000_000_000,
    started_at: 1_710_000_000_000,
    requested_at: 1_709_999_700_000,
    review_entered_at: null,
    completed_at: null,
    latest_dispatch_status: "in_progress",
    latest_dispatch_title: "dispatch",
    latest_dispatch_type: "implementation",
    latest_dispatch_result_summary: null,
    latest_dispatch_chain_depth: 0,
    child_count: 0,
    ...overrides,
  };
}

describe("parseGitHubCommentTimeline", () => {
  it("리뷰 진행 마커 코멘트를 review 이벤트로 파싱한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment("🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)"),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "reviewing",
      title: "리뷰 진행",
    });
  });

  it("리뷰 피드백 코멘트에서 첫 지적 사항을 요약으로 추출한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`코드 리뷰 결과입니다.

1. **High** — 첫 번째 문제
2. **Medium** — 두 번째 문제`),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "changes_requested",
      summary: "High — 첫 번째 문제",
      body: `코드 리뷰 결과입니다.

1. **High** — 첫 번째 문제
2. **Medium** — 두 번째 문제`,
    });
    expect(entry.details).toContain("Medium — 두 번째 문제");
  });

  it("리뷰 통과 코멘트를 pass 이벤트로 파싱한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment("추가 blocking finding은 없습니다. 현재 diff 기준으로 머지를 막을 추가 결함은 확인하지 못했습니다."),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "passed",
      title: "리뷰 통과",
    });
  });

  it("재검토 pass 코멘트도 review passed로 유지한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment("라운드 2 재검토 결과 추가 blocking finding은 없습니다. 현재 diff 기준으로 머지를 막을 추가 결함은 확인하지 못했습니다."),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "passed",
      title: "리뷰 통과",
    });
  });

  it("완료 보고 코멘트를 작업 이력 이벤트로 파싱한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`## #68 완료 보고

### 변경 요약
- something

### 검증
- tests

### DoD
- [x] item`),
    ]);

    expect(entry).toMatchObject({
      kind: "work",
      status: "completed",
      title: "#68 완료 보고",
    });
    expect(entry.details).toEqual(["변경 요약", "검증", "DoD"]);
  });

  it("미분류 코멘트를 general 타입으로 반환한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment("이건 그냥 일반 코멘트입니다."),
    ]);

    expect(entry).toMatchObject({
      kind: "general",
      status: "comment",
      title: "이건 그냥 일반 코멘트입니다.",
      author: "itismyfield",
    });
  });

  it("빈 코멘트는 무시한다", () => {
    const result = parseGitHubCommentTimeline([makeComment("")]);
    expect(result).toHaveLength(0);
  });

  it("긴 코멘트의 summary를 200자로 잘라낸다", () => {
    const longBody = "A".repeat(300);
    const [entry] = parseGitHubCommentTimeline([makeComment(longBody)]);

    expect(entry.kind).toBe("general");
    expect(entry.summary!.length).toBeLessThanOrEqual(201); // 200 + "…"
  });

  it("PM 결정 코멘트를 pm 타입으로 파싱한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`## PM 결정

- 이 방향으로 진행
- 리스크 수용`),
    ]);

    expect(entry).toMatchObject({
      kind: "pm",
      status: "decision",
      title: "PM 결정",
    });
  });

  it("영문 PM Decision 헤더도 pm 타입으로 파싱한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`## PM Decision: ✅ Accept

- proceed`),
    ]);

    expect(entry).toMatchObject({
      kind: "pm",
      status: "decision",
      title: "PM Decision: ✅ Accept",
    });
  });

  it("실사용 리뷰 피드백 코멘트를 review 타입으로 분류한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`리뷰했습니다. 확인된 이슈 3건 남깁니다.

1. 첫 번째 이슈
2. 두 번째 이슈`),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "changes_requested",
      title: "리뷰 피드백",
    });
  });

  it("인용된 pass 문구는 리뷰 통과로 오인하지 않는다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`추가 리뷰했습니다. blocking finding 2건입니다.

> 추가 결함은 확인하지 못했습니다

1. 첫 번째 문제
2. 두 번째 문제`),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "changes_requested",
      title: "리뷰 피드백",
    });
  });

  it("재확인 blocking 코멘트도 review 타입으로 유지한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`재확인했습니다. 현재 코드 기준으로도 blocking 2건 남아 있습니다.

1. 첫 번째 문제
2. 두 번째 문제`),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "changes_requested",
      title: "리뷰 피드백",
    });
  });

  it("본문 중간의 PM 결정 문자열만으로 pm 타입으로 분류하지 않는다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`## #65 완료 보고

- 리뷰 / PM 결정 / 작업 이력 타임라인 추가
- 회귀 테스트 완료`),
    ]);

    expect(entry).toMatchObject({
      kind: "work",
      status: "completed",
      title: "#65 완료 보고",
    });
  });

  it("리뷰 키워드 + 번호 매긴 코드 참조가 있으면 review로 분류한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`리뷰를 확인했습니다. 아래 항목 참고 부탁합니다.

1. \`src/server/routes/kanban.rs:1114-1159\`
assign_issue 경로가 description을 metadata에만 저장합니다.

2. \`src/github/sync.rs:78-82\`
동기화 주기가 너무 긴 것 같습니다.`),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "changes_requested",
      title: "리뷰 피드백",
    });
  });

  it("영문 review 키워드 + 코드 참조를 review로 분류한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`Reviewed the changes. Issues found:

1. \`dashboard/src/api/client.ts:438\` — missing error handling
2. \`src/db/schema.rs:44\` — migration may fail`),
    ]);

    expect(entry).toMatchObject({
      kind: "review",
      status: "changes_requested",
      title: "리뷰 피드백",
    });
  });

  it("리뷰 키워드만 있고 코드 참조가 없으면 general로 유지한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment("코드 리뷰 진행 상황 궁금합니다."),
    ]);

    expect(entry).toMatchObject({
      kind: "general",
      status: "comment",
    });
  });

  it("완료 보고에 리뷰 키워드 + 코드 참조가 있어도 work로 유지한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`## #65 완료 보고

코드 리뷰 반영 완료했습니다.

1. \`src/server/routes/kanban.rs:1114-1159\` 수정
2. \`dashboard/src/api/client.ts:438\` 에러 핸들링 추가`),
    ]);

    expect(entry).toMatchObject({
      kind: "work",
      status: "completed",
      title: "#65 완료 보고",
    });
  });

  it("이슈 번호 작업 완료 헤더를 work 타입으로 파싱한다", () => {
    const [entry] = parseGitHubCommentTimeline([
      makeComment(`## #53 작업 완료

### 변경 요약
- 타임라인 분류 확장`),
    ]);

    expect(entry).toMatchObject({
      kind: "work",
      status: "completed",
      title: "#53 작업 완료",
    });
  });
});

describe("coalesceGitHubCommentTimeline", () => {
  it("같은 작성자의 연속 일반 변경 이벤트를 2분 윈도우로 합산한다", () => {
    const parsed = parseGitHubCommentTimeline([
      makeComment("상태 변경: ready → in_progress", "alice", "2026-03-23T09:00:00Z"),
      makeComment("메타데이터 업데이트: priority=high", "alice", "2026-03-23T09:01:10Z"),
      makeComment("라벨 변경: bug, urgent", "alice", "2026-03-23T09:01:40Z"),
    ]);

    const coalesced = coalesceGitHubCommentTimeline(parsed);

    expect(coalesced).toHaveLength(1);
    expect(coalesced[0]).toMatchObject({
      author: "alice",
      coalesced: true,
    });
    expect(coalesced[0]?.entries).toHaveLength(3);
  });

  it("에이전트 할당 변경 같은 중요 이벤트는 합산하지 않는다", () => {
    const parsed = parseGitHubCommentTimeline([
      makeComment("상태 변경: ready → in_progress", "alice", "2026-03-23T09:00:00Z"),
      makeComment("에이전트 할당 변경: alice → bob", "alice", "2026-03-23T09:00:40Z"),
      makeComment("메타데이터 업데이트: priority=high", "alice", "2026-03-23T09:01:20Z"),
    ]);

    const coalesced = coalesceGitHubCommentTimeline(parsed);

    expect(coalesced).toHaveLength(3);
    expect(coalesced.every((entry) => !entry.coalesced)).toBe(true);
  });

  it("2분 윈도우를 넘기면 같은 작성자여도 새 그룹으로 분리한다", () => {
    const parsed = parseGitHubCommentTimeline([
      makeComment("상태 변경: ready → in_progress", "alice", "2026-03-23T09:00:00Z"),
      makeComment("메타데이터 업데이트: priority=high", "alice", "2026-03-23T09:02:30Z"),
    ]);

    const coalesced = coalesceGitHubCommentTimeline(parsed);

    expect(coalesced).toHaveLength(2);
    expect(coalesced.every((entry) => !entry.coalesced)).toBe(true);
  });
});

describe("getCardStateEnteredAt", () => {
  it("review 상태에서는 review_entered_at 문자열을 우선 사용한다", () => {
    const card = makeCard({
      status: "review",
      started_at: 1_710_000_000_000,
      updated_at: 1_710_000_600_000,
      review_entered_at: "2026-04-13T00:15:00Z",
    });

    expect(getCardStateEnteredAt(card)).toBe(new Date("2026-04-13T00:15:00Z").getTime());
  });
});

describe("getCardDwellBadge", () => {
  const tr = (ko: string, _en: string) => ko;

  it("requested 카드는 15분 이후 warm 톤으로 바뀐다", () => {
    const enteredAt = Date.UTC(2026, 3, 13, 0, 0, 0);
    const card = makeCard({
      status: "requested",
      requested_at: enteredAt,
      updated_at: enteredAt,
      started_at: null,
    });

    const badge = getCardDwellBadge(card, enteredAt + 16 * 60_000, tr);

    expect(badge).toMatchObject({
      label: "체류",
      tone: "warm",
      detail: "16분",
    });
  });

  it("review 카드는 review_entered_at 기준으로 stale 판정한다", () => {
    const enteredAt = Date.UTC(2026, 3, 13, 0, 0, 0);
    const card = makeCard({
      status: "review",
      started_at: enteredAt - 3 * 60 * 60_000,
      updated_at: enteredAt - 30 * 60_000,
      review_entered_at: new Date(enteredAt).toISOString(),
    });

    const badge = getCardDwellBadge(card, enteredAt + 121 * 60_000, tr);

    expect(badge).toMatchObject({
      tone: "stale",
      detail: "2시간",
    });
  });
});
