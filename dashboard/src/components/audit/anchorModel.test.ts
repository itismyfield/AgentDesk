import { describe, expect, it } from "vitest";
import { anchorFilterCounts, anchorGroups, anchorLimitState, anchorsToCheck, anchorTitle, ANCHOR_TITLE_MAX } from "./anchorModel";

const anchor = (chars: number, limit: number | null, overlap_candidates: Array<{ source: "anchor"; index: number } | { source: "canon_file"; path: string }> = []) => ({ chars, limit, overlap_candidates });

describe("anchor checks", () => {
  it("judges the limit only when there is one: near from 90%, over past it", () => {
    expect(anchorLimitState(anchor(400, null))).toBeNull();
    expect(anchorLimitState(anchor(269, 300))).toBeNull();
    expect(anchorLimitState(anchor(270, 300))).toBe("near");
    expect(anchorLimitState(anchor(300, 300))).toBe("near");
    expect(anchorLimitState(anchor(301, 300))).toBe("over");
  });

  it("counts each filter and the anchors worth a look once each", () => {
    const anchors = [
      anchor(290, 300, [{ source: "anchor", index: 1 }]),
      anchor(100, 300, [{ source: "canon_file", path: "agents/_shared.prompt.md" }, { source: "anchor", index: 0 }]),
      anchor(500, null),
    ];
    expect(anchorFilterCounts(anchors)).toEqual({ all: 3, limit: 1, canon: 1, mutual: 2 });
    expect(anchorsToCheck(anchors)).toBe(2);
  });
});

describe("anchor rows", () => {
  it("titles a row with the start of its first non-empty line", () => {
    expect(anchorTitle("\n  코딩 원칙: 과도한 방어 금지\n둘째 줄")).toBe("코딩 원칙: 과도한 방어 금지");
    expect(anchorTitle("가".repeat(ANCHOR_TITLE_MAX + 5))).toBe(`${"가".repeat(ANCHOR_TITLE_MAX)}…`);
  });

  it("groups by scope with unscoped rows last, or keeps one list when no scope is set", () => {
    const row = (scope: string | null, index: number) => ({ anchor: { scope }, index });
    expect(anchorGroups([row(null, 0), row(null, 1)])).toEqual([{ scope: null, rows: [row(null, 0), row(null, 1)] }]);
    expect(anchorGroups([row(null, 0), row("team", 1), row("all", 2), row("team", 3)]).map((group) => [group.scope, group.rows.map((item) => item.index)]))
      .toEqual([["team", [1, 3]], ["all", [2]], [null, [0]]]);
  });
});
