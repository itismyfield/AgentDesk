import { describe, expect, it } from "vitest";
import { NO_TIES, campaignRawEdges, rawEdgeId, routeEdges, type CableTie, type GraphViewSettings, type RawEdgeKey } from "./campaignGraphRouting";

const e1: RawEdgeKey = ["a1", "b1"];
const e2: RawEdgeKey = ["a2", "b1"];
const e3: RawEdgeKey = ["a3", "b2"];
const back: RawEdgeKey = ["b1", "a4"];
const groups = new Map([["a1", "A"], ["a2", "A"], ["a3", "A"], ["a4", "A"], ["b1", "B"], ["b2", "B"]]);
const tie = (id: string, strandOrder: RawEdgeKey[], x = 0): CableTie => ({ id, position: { x, y: 0 }, strandOrder });
const tied = (ties: CableTie[], routes: Array<[RawEdgeKey, string[]]>): GraphViewSettings =>
  ({ ties, routes: routes.map(([edge, via]) => ({ edge, via })), showOriginal: false });
function deepFreeze<T>(value: T): T {
  if (value && typeof value === "object" && !Object.isFrozen(value)) {
    Object.freeze(value);
    for (const child of value instanceof Map ? [...value.entries()].flat() : Object.values(value)) deepFreeze(child);
  }
  return value;
}

describe("routeEdges", () => {
  it("keeps the raw edges behind each aggregate line, each in its own direction", () => {
    const routed = routeEdges({ edges: [e1, e2, back], displayOf: groups }, NO_TIES);
    // A -> B and B -> A are two lines; a round trip between groups is not a task cycle.
    expect(routed.display.map(({ source, target, via, edges }) => ({ source, target, via, edges }))).toEqual([
      { source: "A", target: "B", via: [], edges: [e1, e2] },
      { source: "B", target: "A", via: [], edges: [back] },
    ]);
    expect(routed.routes).toEqual([{ edge: e1, via: [] }, { edge: e2, via: [] }, { edge: back, via: [] }]);
    expect(Object.keys(routed.display[0]).sort()).toEqual(["edges", "id", "source", "target", "via"]);
  });

  it("does not pull an edge added after a tie into that tie", () => {
    const view = tied([tie("t1", [e1, e2])], [[e1, ["t1"]], [e2, ["t1"]]]);
    const e4: RawEdgeKey = ["a3", "b1"];
    const routed = routeEdges({ edges: [e1, e2, e4], displayOf: groups }, view);
    expect(routed.display.map(({ via, edges }) => ({ via, edges }))).toEqual([
      { via: ["t1"], edges: [e1, e2] },
      { via: [], edges: [e4] },
    ]);
    expect(routed.routes.at(-1)).toEqual({ edge: e4, via: [] });
    expect(routed.settings.ties[0].strandOrder).toEqual([e1, e2]);
  });

  it("keeps ties and routes of edges hidden from the drawing and clears only edges gone from the definition", () => {
    const view = tied([tie("t1", [e1, e2, e3])], [[e1, ["t1"]], [e2, ["t1"]], [e3, ["t1"]]]);
    const filtered = new Map(groups);
    filtered.delete("a2");
    // e2 is filtered out of the drawing; e3 was deleted from the definition.
    const routed = routeEdges({ edges: [e1, e2], displayOf: filtered }, view);
    expect(routed.hidden).toEqual([e2]);
    expect(routed.display.map(({ edges }) => edges)).toEqual([[e1]]);
    expect(routed.settings.ties[0].strandOrder).toEqual([e1, e2]);
    expect(routed.settings.routes).toEqual([{ edge: e1, via: ["t1"] }, { edge: e2, via: ["t1"] }]);
    expect(routed.removed).toEqual([e3]);
  });

  it("keeps each edge's tie order and each tie's strand order as stored, not as positions suggest", () => {
    const view = tied([tie("t1", [e2, e1], 400), tie("t2", [e1, e2], 100)], [[e1, ["t2", "t1"]], [e2, ["t1", "t2"]]]);
    const routed = routeEdges({ edges: [e1, e2], displayOf: groups }, view);
    expect(routed.routes).toEqual([{ edge: e1, via: ["t2", "t1"] }, { edge: e2, via: ["t1", "t2"] }]);
    // Same groups, different paths: two lines.
    expect(routed.display.map(({ via, edges }) => ({ via, edges }))).toEqual([
      { via: ["t2", "t1"], edges: [e1] },
      { via: ["t1", "t2"], edges: [e2] },
    ]);
    expect(routed.settings.ties.map((entry) => entry.strandOrder)).toEqual([[e2, e1], [e1, e2]]);
  });

  it("identifies raw edges by a collision-free form rather than a display string", () => {
    const left: RawEdgeKey = ["a:b", "c"];
    const right: RawEdgeKey = ["a", "b:c"];
    expect(rawEdgeId(left)).not.toBe(rawEdgeId(right));
    const displayOf = new Map([["a:b", "X"], ["a", "X"], ["c", "Y"], ["b:c", "Y"]]);
    const routed = routeEdges({ edges: [left, right], displayOf }, tied([tie("t1", [left])], [[left, ["t1"]]]));
    expect(routed.routes).toEqual([{ edge: left, via: ["t1"] }, { edge: right, via: [] }]);
    expect(new Set(routed.display.map((line) => line.id)).size).toBe(2);
  });

  it("draws one line per raw edge when the original edges are shown", () => {
    const routed = routeEdges({ edges: [e1, e2], displayOf: groups }, { ...NO_TIES, showOriginal: true });
    expect(routed.display.map(({ edges }) => edges)).toEqual([[e1], [e2]]);
  });

  it("summarizes edges inside one drawn node instead of drawing them", () => {
    const routed = routeEdges({ edges: [["a1", "a2"], e1], displayOf: groups }, NO_TIES);
    expect(routed.internal).toEqual([["a1", "a2"]]);
    expect(routed.display).toHaveLength(1);
  });

  it("leaves the projection and the stored settings untouched", () => {
    const projection = deepFreeze({ edges: [e1, e2, e3], displayOf: new Map(groups) });
    const view = deepFreeze(tied([tie("t1", [e1, e2])], [[e1, ["t1"]], [e2, ["t1"]], [["gone", "b1"], ["t1"]]]));
    const before = JSON.stringify(view);
    expect(() => routeEdges(projection, view)).not.toThrow();
    expect(JSON.stringify(view)).toBe(before);
  });
});

describe("campaignRawEdges", () => {
  it("lists each dependency between existing tasks once, dependency first", () => {
    expect(campaignRawEdges([
      { id: "a", dependencies: [] },
      { id: "b", dependencies: ["a", "a", "missing"] },
      { id: "c", dependencies: ["b", "a"] },
    ])).toEqual([["a", "b"], ["b", "c"], ["a", "c"]]);
  });
});
