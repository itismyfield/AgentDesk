import { describe, expect, it } from "vitest";
import type { Campaign } from "../../api/campaigns";
import { makeCampaignNode as node } from "./campaignTestFixtures";
import {
  SCOPE_LIMIT, adoptLatest, applyEdits, editCounts, editField, fieldErrors, findCycles, rebaseEdits, scopeTasks,
} from "./relationsEditorModel";
import { campaignLocationQuery, parseCampaignLocation } from "./serverCampaignModel";

// A → B → C, B → D (D's group differs); E stands alone.
const nodes = [
  node("A", { group: "G" }), node("B", { group: "G", dependencies: ["A"] }), node("C", { group: "G", dependencies: ["B"] }),
  node("D", { group: "H", dependencies: ["B"] }), node("E", { group: "G" }),
];

describe("relations editor scope", () => {
  it("draws a group, or one task's dependencies and dependents n steps out without siblings, up to the limit", () => {
    expect(scopeTasks(nodes, { kind: "group", key: "G" })).toEqual({ ids: ["A", "B", "C", "E"], total: 4 });
    expect(scopeTasks(nodes, { kind: "task", id: "C", depth: 1 }).ids).toEqual(["C", "B"]);
    expect(scopeTasks(nodes, { kind: "task", id: "C", depth: 2 }).ids).toEqual(["C", "B", "A"]);
    expect(scopeTasks(nodes, { kind: "task", id: "B", depth: 1 }).ids).toEqual(["B", "A", "C", "D"]);
    const many = Array.from({ length: SCOPE_LIMIT + 5 }, (_, index) => node(`n${index}`, { group: "big" }));
    expect(scopeTasks(many, { kind: "group", key: "big" })).toMatchObject({ total: SCOPE_LIMIT + 5 });
    // S ↔ A, A → B: A, reached upstream through the loop, is still walked downstream to B; the limit holds on the union.
    const loop = [node("S", { dependencies: ["A"] }), node("A", { dependencies: ["S"] }), node("B", { dependencies: ["A"] })];
    expect(scopeTasks(loop, { kind: "task", id: "S", depth: 2 }).ids).toEqual(["S", "A", "B"]);
    const hub = scopeTasks([node("H"), ...many.map((value) => ({ ...value, dependencies: ["H"] }))], { kind: "task", id: "H", depth: 1 });
    expect([hub.ids.length, hub.total]).toEqual([SCOPE_LIMIT, SCOPE_LIMIT + 6]);
    expect(scopeTasks(many, { kind: "group", key: "big" }).ids).toHaveLength(SCOPE_LIMIT);
  });

  it("keeps the scope in the address", () => {
    const query = campaignLocationQuery({ mode: "legacy", campaign: "c", node: null, edit: { kind: "task", id: "B", depth: 9 } });
    expect(parseCampaignLocation(query)).toEqual({ mode: "legacy", campaign: "c", node: null, edit: { kind: "task", id: "B", depth: 3 } });
    expect(parseCampaignLocation("?mode=legacy&campaign=c&edit=relations&scope=group:")?.edit).toEqual({ kind: "group", key: "" });
  });
});

describe("relations editor checks", () => {
  it("finds a cycle through tasks outside the drawn scope, and a task depending on itself", () => {
    const looped = applyEdits(nodes, { A: { dependencies: ["D"] }, E: { dependencies: ["E"] } });
    expect(scopeTasks(looped, { kind: "group", key: "G" }).ids).not.toContain("D");
    expect(findCycles(looped)).toEqual([["A", "B", "D", "A"], ["E", "E"]]);
    expect(findCycles(nodes)).toEqual([]);
  });

  it("counts a field once and drops it when it is back at the base value", () => {
    let edits = editField({}, nodes[1], "B", "dependencies", ["A", "E"]);
    edits = editField(edits, nodes[1], "B", "title", "  ");
    expect(editCounts(edits)).toEqual({ dependencies: 1, title: 1, issue_url: 0 });
    expect(fieldErrors(applyEdits(nodes, edits)[1], edits.B)).toEqual(new Set(["title"]));
    expect(fieldErrors({ ...nodes[0], issue_url: "javascript:alert(1)" }, { issue_url: "javascript:alert(1)" })).toEqual(new Set(["issue_url"]));
    // Only changed fields are judged: a link the ledger already holds in its own short form stays as it is.
    expect(fieldErrors({ ...nodes[0], issue_url: "owner/repo/issues/9" }, { title: "A" })).toEqual(new Set());
    expect(editField(editField(edits, nodes[1], "B", "title", "B"), nodes[1], "B", "dependencies", ["A"])).toEqual({});
  });
});

describe("putting edits on the newest copy", () => {
  const edits = { B: { dependencies: ["A", "E"] }, C: { title: "New C" } };

  it("moves each edited field while the server still holds its base value and keeps the server's other changes", () => {
    const latest = nodes.map((value) => value.id === "B" ? { ...value, status: "running" as const, title: "Server B" } : value);
    const result = rebaseEdits(nodes, edits, latest);
    expect("nodes" in result && result.nodes.find((value) => value.id === "B")).toMatchObject({ status: "running", title: "Server B", dependencies: ["A", "E"] });
    expect("nodes" in result && result.nodes.find((value) => value.id === "C")?.title).toBe("New C");
    const same = nodes.map((value) => value.id === "B" ? { ...value, dependencies: ["E", "A"] } : value);
    expect(rebaseEdits(nodes, edits, same)).toHaveProperty("nodes");
  });

  it("refuses a field the server changed too, a task that is gone or now locked, and a dependency on a task that is gone", () => {
    const changed = nodes.map((value) => value.id === "C" ? { ...value, title: "Server C" } : value);
    expect(rebaseEdits(nodes, edits, changed)).toEqual({ conflicts: [{ id: "C", field: "title" }] });
    expect(rebaseEdits(nodes, edits, nodes.filter((value) => value.id !== "C"))).toEqual({ conflicts: [{ id: "C", field: "missing" }] });
    const done = nodes.map((value) => value.id === "B" ? { ...value, status: "skipped" as const } : value);
    expect(rebaseEdits(nodes, edits, done)).toEqual({ conflicts: [{ id: "B", field: "locked" }] });
    expect(rebaseEdits(nodes, edits, nodes.filter((value) => value.id !== "E"))).toEqual({ conflicts: [{ id: "B", field: "dependencies" }] });
  });

  it("takes the newest copy as the base, dropping only the conflicting edits", () => {
    const latest = { id: "c", nodes: nodes.map((value) => value.id === "C" ? { ...value, title: "Server C" } : value) } as Campaign;
    expect(adoptLatest(latest, edits, [{ id: "C", field: "title" }])).toEqual({ B: { dependencies: ["A", "E"] } });
  });
});
