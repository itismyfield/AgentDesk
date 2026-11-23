import { describe, expect, it } from "vitest";

import { NO_DEPARTMENT, filterAgents, filterAndSortAgents, sortAgents } from "./agent-list-controls";
import type { Agent } from "../../types";
import type { HomeAgent } from "../home/homeModel";

const entry = (state: HomeAgent["state"], overrides: Partial<Agent>): HomeAgent => ({
  state,
  info: null,
  seenAt: null,
  agent: {
    id: "a1", name: "Alpha", name_ko: "알파", department_id: "dev", avatar_emoji: "🤖", personality: null,
    status: "idle", stats_tasks_done: 0, stats_tokens: 0, created_at: 0, ...overrides,
  } as Agent,
});

const entries: HomeAgent[] = [
  entry("resting", { id: "a", name: "Alpha", name_ko: "알파", department_name_ko: "개발", agentdesk_working_count: 5, stats_tasks_done: 10, created_at: 1700000000 }),
  entry("working", { id: "b", name: "Beta", name_ko: "베타", department_id: "design", department_name_ko: "디자인", status: "working", stats_tasks_done: 50, created_at: 1700000200 }),
  entry("noSession", { id: "c", name: "Charlie", name_ko: "찰리", alias: "CC", department_id: null }),
  entry("background", { id: "d-e2e", name: "Delta", name_ko: "델타" }),
  entry("working", { id: "f-e2e", name: "Foxtrot", name_ko: "폭스", status: "working" }),
];
const ids = (rows: HomeAgent[]) => rows.map((row) => row.agent.id);
const all = { deptTab: "all", statusFilter: "all", search: "" } as const;

describe("filterAgents", () => {
  it("filters by department, including agents without one", () => {
    expect(ids(filterAgents(entries, { ...all, deptTab: "design" }))).toEqual(["b"]);
    expect(ids(filterAgents(entries, { ...all, deptTab: NO_DEPARTMENT }))).toEqual(["c"]);
    // A department id that no longer exists counts as unassigned.
    expect(ids(filterAgents(entries, { ...all, deptTab: NO_DEPARTMENT, departments: new Set(["design"]) }))).toEqual(["a", "c", "f-e2e"]);
  });

  it("filters by home state and hides idle test agents unless asked, as home does", () => {
    expect(ids(filterAgents(entries, { ...all, statusFilter: "working" }))).toEqual(["b", "f-e2e"]);
    expect(ids(filterAgents(entries, all))).toEqual(["a", "b", "c", "f-e2e"]);
    expect(ids(filterAgents(entries, { ...all, includeTests: true }))).toHaveLength(5);
  });

  it("matches search against name, name_ko and alias", () => {
    expect(ids(filterAgents(entries, { ...all, search: "cc" }))).toEqual(["c"]);
    expect(ids(filterAgents(entries, { ...all, search: "베타" }))).toEqual(["b"]);
  });
});

describe("sortAgents", () => {
  it("sorts by home state, then the Korean name", () => {
    expect(ids(sortAgents(entries, "status"))).toEqual(["b", "f-e2e", "d-e2e", "a", "c"]);
  });

  it("sorts by activity, department and creation", () => {
    expect(ids(sortAgents(entries.slice(0, 3), "activity"))).toEqual(["b", "a", "c"]);
    expect(ids(sortAgents(entries.slice(0, 3), "department"))).toEqual(["c", "a", "b"]);
    expect(ids(sortAgents(entries.slice(0, 3), "created"))).toEqual(["b", "a", "c"]);
  });

  it("composes filtering and sorting", () => {
    expect(ids(filterAndSortAgents(entries, { ...all, deptTab: "dev" }, "name"))).toEqual(["a", "f-e2e"]);
  });
});
