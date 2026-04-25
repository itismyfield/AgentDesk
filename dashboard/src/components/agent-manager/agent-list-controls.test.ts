import { describe, expect, it } from "vitest";

import {
  filterAgents,
  filterAndSortAgents,
  sortAgents,
} from "./agent-list-controls";
import type { Agent } from "../../types";

function makeAgent(overrides: Partial<Agent>): Agent {
  return {
    id: "a1",
    name: "Alpha",
    name_ko: "알파",
    department_id: "dev",
    avatar_emoji: "🤖",
    personality: null,
    status: "idle",
    stats_tasks_done: 0,
    stats_xp: 0,
    stats_tokens: 0,
    created_at: 0,
    ...overrides,
  } as Agent;
}

const agents: Agent[] = [
  makeAgent({
    id: "a",
    name: "Alpha",
    name_ko: "알파",
    department_id: "dev",
    department_name_ko: "개발",
    status: "idle",
    stats_xp: 1000,
    agentdesk_working_count: 5,
    stats_tasks_done: 10,
    created_at: 1700000000,
    archived_at: null,
  }),
  makeAgent({
    id: "b",
    name: "Beta",
    name_ko: "베타",
    department_id: "design",
    department_name_ko: "디자인",
    status: "working",
    stats_xp: 5000,
    agentdesk_working_count: 1,
    stats_tasks_done: 50,
    created_at: 1700000200,
  }),
  makeAgent({
    id: "c",
    name: "Charlie",
    name_ko: "찰리",
    alias: "CC",
    department_id: "dev",
    department_name_ko: "개발",
    status: "archived",
    stats_xp: 200,
    archived_at: 1700000500,
    archive_reason: "old",
  }),
];

describe("filterAgents", () => {
  it("filters by department tab", () => {
    const result = filterAgents(agents, {
      deptTab: "design",
      statusFilter: "all",
      search: "",
    });
    expect(result.map((a) => a.id)).toEqual(["b"]);
  });

  it("filters by status", () => {
    const result = filterAgents(agents, {
      deptTab: "all",
      statusFilter: "archived",
      search: "",
    });
    expect(result.map((a) => a.id)).toEqual(["c"]);
  });

  it("matches search against name, name_ko, alias, and emoji", () => {
    expect(
      filterAgents(agents, {
        deptTab: "all",
        statusFilter: "all",
        search: "cc",
      }).map((a) => a.id),
    ).toEqual(["c"]);
    expect(
      filterAgents(agents, {
        deptTab: "all",
        statusFilter: "all",
        search: "베타",
      }).map((a) => a.id),
    ).toEqual(["b"]);
  });
});

describe("sortAgents", () => {
  it("sorts by status with working first, archived last", () => {
    expect(sortAgents(agents, "status").map((a) => a.id)).toEqual([
      "b",
      "a",
      "c",
    ]);
  });

  it("sorts by xp descending", () => {
    expect(sortAgents(agents, "xp").map((a) => a.id)).toEqual(["b", "a", "c"]);
  });

  it("sorts by activity (working_count + tasks_done) descending", () => {
    expect(sortAgents(agents, "activity").map((a) => a.id)).toEqual([
      "b",
      "a",
      "c",
    ]);
  });

  it("sorts by department name then name", () => {
    expect(sortAgents(agents, "department").map((a) => a.id)).toEqual([
      "a",
      "c",
      "b",
    ]);
  });

  it("sorts by created descending", () => {
    expect(sortAgents(agents, "created").map((a) => a.id)).toEqual([
      "b",
      "a",
      "c",
    ]);
  });

  it("sorts by archived descending", () => {
    expect(sortAgents(agents, "archived").map((a) => a.id)).toEqual([
      "c",
      "a",
      "b",
    ]);
  });
});

describe("filterAndSortAgents", () => {
  it("composes filtering and sorting", () => {
    const result = filterAndSortAgents(
      agents,
      { deptTab: "dev", statusFilter: "all", search: "" },
      "xp",
    );
    expect(result.map((a) => a.id)).toEqual(["a", "c"]);
  });
});
