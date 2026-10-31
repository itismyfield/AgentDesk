import { expect, it } from "vitest";
import { NO_TIES, type GraphViewSettings, type RawEdgeKey } from "./campaignGraphRouting";
import { applyTieCommand, commitTie, pruneTies, undoTie, type TieHistory } from "./campaignTieEditor";
import { tieSlots } from "./campaignTieGeometry";

const ac: RawEdgeKey = ["a", "c"];
const bd: RawEdgeKey = ["b", "d"];
const be: RawEdgeKey = ["b", "e"];

it("makes each tie command one undo step and keeps strand and via order as stored", () => {
  let history: TieHistory = { view: NO_TIES, undo: [] };
  history = commitTie(history, { kind: "create", edges: [bd, ac], position: { x: 10, y: 10 } });
  history = commitTie(history, { kind: "create", edges: [ac], position: { x: 90, y: 10 } });
  history = commitTie(history, { kind: "move", tieId: "tie-1", position: { x: 500, y: -40 } });
  expect(history.view.ties.map((tie) => [tie.id, tie.position, tie.strandOrder])).toEqual([
    ["tie-1", { x: 500, y: -40 }, [bd, ac]],
    ["tie-2", { x: 90, y: 10 }, [ac]],
  ]);
  expect(history.view.routes).toEqual([{ edge: bd, via: ["tie-1"] }, { edge: ac, via: ["tie-1", "tie-2"] }]);

  history = commitTie(history, { kind: "remove", tieId: "tie-1" });
  expect(history.view.routes).toEqual([{ edge: ac, via: ["tie-2"] }]);
  history = commitTie(history, { kind: "clear" });
  expect(history.view).toEqual(NO_TIES);
  expect(commitTie(history, { kind: "move", tieId: "tie-9", position: { x: 0, y: 0 } })).toBe(history);
  const unmoved = commitTie(history, { kind: "create", edges: [ac], position: { x: 1, y: 2 } });
  expect(commitTie(unmoved, { kind: "move", tieId: "tie-1", position: { x: 1, y: 2 } })).toBe(unmoved);

  const steps = [];
  while (history.undo.length) { history = undoTie(history); steps.push(history.view.ties.map((tie) => [tie.id, tie.position.x])); }
  expect(steps).toEqual([
    [["tie-2", 90]],
    [["tie-1", 500], ["tie-2", 90]],
    [["tie-1", 10], ["tie-2", 90]],
    [["tie-1", 10]],
    [],
  ]);
});

it("applies a command on another tab's view without reusing its tie id", () => {
  const otherTab = applyTieCommand(NO_TIES, { kind: "create", edges: [be], position: { x: 0, y: 0 } });
  const local: TieHistory = { view: NO_TIES, undo: [] };
  const merged = commitTie(local, { kind: "create", edges: [ac], position: { x: 5, y: 5 } }, otherTab);
  expect(merged.view.ties.map((tie) => [tie.id, tie.strandOrder])).toEqual([["tie-1", [be]], ["tie-2", [ac]]]);
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
