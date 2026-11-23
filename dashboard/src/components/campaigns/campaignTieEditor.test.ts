import { expect, it } from "vitest";
import { NO_TIES, type GraphViewSettings, type RawEdgeKey } from "./campaignGraphRouting";
import { applyTieCommand, pruneTies } from "./campaignTieEditor";

const ac: RawEdgeKey = ["a", "c"];
const bd: RawEdgeKey = ["b", "d"];
const be: RawEdgeKey = ["b", "e"];

it("keeps strand and via order as stored through create, move, detach, remove and clear, and returns the same view for a no-op", () => {
  let view = applyTieCommand(NO_TIES, { kind: "create", edges: [bd, ac], position: { x: 10, y: 10 } });
  const first = view.ties[0].id;
  view = applyTieCommand(view, { kind: "create", edges: [ac], position: { x: 90, y: 10 } });
  const second = view.ties[1].id;
  expect(second).not.toBe(first);
  view = applyTieCommand(view, { kind: "move", tieId: first, position: { x: 500, y: -40 } });
  expect(view.ties.map((tie) => [tie.id, tie.position, tie.strandOrder])).toEqual([[first, { x: 500, y: -40 }, [bd, ac]], [second, { x: 90, y: 10 }, [ac]]]);
  expect(view.routes).toEqual([{ edge: bd, via: [first] }, { edge: ac, via: [first, second] }]);
  // One line out of a tie takes only that tie off its route; the last line out takes the tie with it.
  const detached = applyTieCommand(view, { kind: "detach", tieId: first, edge: ac });
  expect([detached.ties.map((tie) => tie.strandOrder), detached.routes]).toEqual([[[bd], [ac]], [{ edge: bd, via: [first] }, { edge: ac, via: [second] }]]);
  expect(applyTieCommand(detached, { kind: "detach", tieId: second, edge: ac }).ties.map((tie) => tie.id)).toEqual([first]);
  expect(applyTieCommand(detached, { kind: "detach", tieId: second, edge: bd })).toBe(detached);

  view = applyTieCommand(view, { kind: "remove", tieId: first });
  expect(view.routes).toEqual([{ edge: ac, via: [second] }]);
  expect(applyTieCommand(view, { kind: "clear" })).toEqual(NO_TIES);
  expect(applyTieCommand(view, { kind: "move", tieId: "tie-missing", position: { x: 0, y: 0 } })).toBe(view);
  expect(applyTieCommand(view, { kind: "move", tieId: second, position: { x: 90, y: 10 } })).toBe(view);
  expect(applyTieCommand(view, { kind: "create", edges: [], position: { x: 0, y: 0 } })).toBe(view);
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

it("never reuses a removed tie's id, so a stale move cannot land on another tab's new tie", () => {
  const mine = applyTieCommand(NO_TIES, { kind: "create", edges: [ac], position: { x: 0, y: 0 } });
  const oldId = mine.ties[0].id;
  const otherTab = applyTieCommand(applyTieCommand(mine, { kind: "remove", tieId: oldId }), { kind: "create", edges: [bd], position: { x: 400, y: 400 } });
  expect(otherTab.ties[0].id).not.toBe(oldId);
  expect(applyTieCommand(otherTab, { kind: "move", tieId: oldId, position: { x: 50, y: 50 } }).ties.map((tie) => tie.position)).toEqual([{ x: 400, y: 400 }]);
});
