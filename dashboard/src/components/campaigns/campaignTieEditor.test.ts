import { expect, it } from "vitest";
import { NO_TIES, type GraphViewSettings, type RawEdgeKey } from "./campaignGraphRouting";
import { applyTieCommand, commitTie, pruneTies, undoTie, type TieHistory } from "./campaignTieEditor";
import { linePath, strokeCrosses, tieSlots } from "./campaignTieGeometry";

const ac: RawEdgeKey = ["a", "c"];
const bd: RawEdgeKey = ["b", "d"];
const be: RawEdgeKey = ["b", "e"];

it("makes each tie command one undo step and keeps strand and via order as stored", () => {
  let history: TieHistory = { view: NO_TIES, undo: [] };
  history = commitTie(history, { kind: "create", edges: [bd, ac], position: { x: 10, y: 10 } });
  const first = history.view.ties[0].id;
  history = commitTie(history, { kind: "create", edges: [ac], position: { x: 90, y: 10 } });
  const second = history.view.ties[1].id;
  expect(second).not.toBe(first);
  history = commitTie(history, { kind: "move", tieId: first, position: { x: 500, y: -40 } });
  expect(history.view.ties.map((tie) => [tie.id, tie.position, tie.strandOrder])).toEqual([
    [first, { x: 500, y: -40 }, [bd, ac]],
    [second, { x: 90, y: 10 }, [ac]],
  ]);
  expect(history.view.routes).toEqual([{ edge: bd, via: [first] }, { edge: ac, via: [first, second] }]);

  history = commitTie(history, { kind: "remove", tieId: first });
  expect(history.view.routes).toEqual([{ edge: ac, via: [second] }]);
  history = commitTie(history, { kind: "clear" });
  expect(history.view).toEqual(NO_TIES);
  expect(commitTie(history, { kind: "move", tieId: "tie-missing", position: { x: 0, y: 0 } })).toBe(history);
  const unmoved = commitTie(history, { kind: "create", edges: [ac], position: { x: 1, y: 2 } });
  expect(commitTie(unmoved, { kind: "move", tieId: unmoved.view.ties[0].id, position: { x: 1, y: 2 } })).toBe(unmoved);

  const steps = [];
  while (history.undo.length) { history = undoTie(history); steps.push(history.view.ties.map((tie) => [tie.id, tie.position.x])); }
  expect(steps).toEqual([
    [[second, 90]],
    [[first, 500], [second, 90]],
    [[first, 10], [second, 90]],
    [[first, 10]],
    [],
  ]);
});

it("applies a command on another tab's view and keeps that tab's tie", () => {
  const otherTab = applyTieCommand(NO_TIES, { kind: "create", edges: [be], position: { x: 0, y: 0 } });
  const local: TieHistory = { view: NO_TIES, undo: [] };
  const merged = commitTie(local, { kind: "create", edges: [ac], position: { x: 5, y: 5 } }, otherTab);
  expect(merged.view.ties.map((tie) => tie.strandOrder)).toEqual([[be], [ac]]);
  expect(merged.view.ties[0]).toBe(otherTab.ties[0]);
  expect(new Set(merged.view.ties.map((tie) => tie.id)).size).toBe(2);
  expect(merged.undo).toEqual([NO_TIES]);
});

it("drops only members that left the definition and ties left empty", () => {
  const view: GraphViewSettings = {
    ties: [{ id: "t1", position: { x: 0, y: 0 }, strandOrder: [ac, bd] }, { id: "t2", position: { x: 1, y: 1 }, strandOrder: [be] }],
    routes: [{ edge: ac, via: ["t1"] }, { edge: bd, via: ["t1"] }, { edge: be, via: ["t2"] }],
    showOriginal: true,
  };
  // bd is still in the definition (only hidden on screen); be was deleted.
  expect(pruneTies(view, [ac, bd])).toEqual({
    ties: [{ id: "t1", position: { x: 0, y: 0 }, strandOrder: [ac, bd] }],
    routes: [{ edge: ac, via: ["t1"] }, { edge: bd, via: ["t1"] }],
    showOriginal: true,
  });
});

it("orders strands through a tie by the stored strand order, not by where their lines are drawn", () => {
  const tie = { id: "t1", position: { x: 0, y: 0 }, strandOrder: [bd, ac] };
  const line = (id: string, edge: RawEdgeKey, y: number) => ({ id, source: `s${y}`, target: `t${y}`, via: ["t1"], edges: [edge] });
  expect(tieSlots([tie], [line("upper", ac, 0), line("lower", bd, 400)]).get("t1")).toEqual(["lower", "upper"]);
});

it("never reuses a removed tie's id, so a stale move cannot land on another tab's new tie", () => {
  const mine = applyTieCommand(NO_TIES, { kind: "create", edges: [ac], position: { x: 0, y: 0 } });
  const oldId = mine.ties[0].id;
  const otherTab = applyTieCommand(applyTieCommand(mine, { kind: "remove", tieId: oldId }), { kind: "create", edges: [bd], position: { x: 400, y: 400 } });
  expect(otherTab.ties[0].id).not.toBe(oldId);
  expect(applyTieCommand(otherTab, { kind: "move", tieId: oldId, position: { x: 50, y: 50 } }).ties.map((tie) => tie.position)).toEqual([{ x: 400, y: 400 }]);
});

it("selects a line by a stroke through any point of its drawn path, including segment joints and backward routes", () => {
  const boxes = new Map([["g1", { x: 0, y: 0, width: 244, height: 145 }], ["g2", { x: 310, y: 0, width: 244, height: 145 }], ["g4", { x: 0, y: 210, width: 244, height: 145 }]]);
  const forward = linePath({ id: "f", source: "g1", target: "g2", via: [], edges: [ac] }, boxes, new Map(), new Map());
  for (const x of [250, 277, 278, 305]) expect(strokeCrosses(forward, { x, y: 0 }, { x, y: 150 }), `x=${x}`).toBe(true);
  // A backward line leaves its source's right side before turning back, as React Flow draws it.
  const backward = linePath({ id: "b", source: "g4", target: "g1", via: [], edges: [bd] }, boxes, new Map(), new Map());
  expect(strokeCrosses(backward, { x: 255, y: 270 }, { x: 255, y: 295 })).toBe(true);
});
