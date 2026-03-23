import { describe, expect, it } from "vitest";
import type { GitHubComment } from "../../api";
import { parseGitHubCommentTimeline } from "./kanban-utils";

function makeComment(body: string, author = "itismyfield"): GitHubComment {
  return {
    author: { login: author },
    body,
    createdAt: "2026-03-23T09:00:00Z",
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
});
