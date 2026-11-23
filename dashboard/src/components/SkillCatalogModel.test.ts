import { describe, expect, it } from "vitest";
import type { SkillCatalogEntry } from "../types";
import { categorizeSkill, fillSkillTrend, plainSkillText, skillUsers, topSkillItems, trendDayLabel } from "./SkillCatalogModel";

const skill = (name: string, description: string): SkillCatalogEntry => ({ name, description, description_ko: description, total_calls: 0, last_used_at: null });
const ranked = (name: string, calls: number) => ({ skill_name: name, skill_desc_ko: name, calls, last_used_at: 0 });

describe("SkillCatalogModel", () => {
  it("matches English category terms as whole words", () => {
    // "pr" used to catch "profiles" and "Prefer", filing both under GitHub.
    expect(categorizeSkill(skill("family-profile-probe", "Keep family profiles fresh without spamming"))).toBe("knowledge");
    expect(categorizeSkill(skill("playwright", "Drive a real browser. Prefer the CLI."))).toBe("workflow");
    expect(categorizeSkill(skill("create-issue", "에이전트가 GitHub Issue를 만들 때 써요"))).toBe("github");
    expect(categorizeSkill(skill("agentdesk-restart", "빠른 재시작 전용"))).toBe("ops");
    expect(categorizeSkill(skill("family-morning-briefing", "모닝 브리핑"))).toBe("meetings");
  });

  it("fills every UTC date of the window and keeps the server's counts", () => {
    const now = Date.UTC(2026, 10, 23, 1, 0);
    const days = fillSkillTrend([{ day: "2026-10-24", count: 1 }, { day: "2026-11-23", count: 3 }], 30, now);
    expect(days).toHaveLength(31);
    expect(days[0]).toEqual({ day: "2026-10-24", count: 1 });
    expect(days[1]).toEqual({ day: "2026-10-25", count: 0 });
    expect(days.at(-1)).toEqual({ day: "2026-11-23", count: 3 });
    expect(trendDayLabel("2026-11-03", true)).toBe("11/3");
    expect(trendDayLabel("2026-11-03", true, true)).toBe("11월 3일");
  });

  it("ranks the five most called skills and counts every called one", () => {
    const rows = [9, 7, 3, 3, 2, 1, 0].map((calls, index) => ranked(`s${index}`, calls));
    const { items, called } = topSkillItems(rows);
    expect(items.map((item) => [item.id, item.value])).toEqual([["s0", 9], ["s1", 7], ["s2", 3], ["s3", 3], ["s4", 2]]);
    expect(called).toBe(6);
  });

  it("lists a skill's agents by calls and finds them by role id or id", () => {
    const byAgent = [
      { agent_role_id: "ops-role", agent_name: "Ops", skill_name: "hub", skill_desc_ko: "", calls: 1, last_used_at: 0 },
      { agent_role_id: "agent-b", agent_name: "B", skill_name: "hub", skill_desc_ko: "", calls: 5, last_used_at: 0 },
      { agent_role_id: "gone", agent_name: "Gone", skill_name: "hub", skill_desc_ko: "", calls: 2, last_used_at: 0 },
      { agent_role_id: "agent-b", agent_name: "B", skill_name: "other", skill_desc_ko: "", calls: 9, last_used_at: 0 },
    ];
    const agents = [{ id: "agent-a", role_id: "ops-role" }, { id: "agent-b", role_id: null }];
    expect(skillUsers(byAgent, "hub", agents).map(({ row, agent }) => [row.agent_name, agent?.id ?? null])).toEqual([
      ["B", "agent-b"],
      ["Gone", null],
      ["Ops", "agent-a"],
    ]);
  });

  it("reads a description as one plain line", () => {
    expect(plainSkillText("정책 버전: **v2.18**.\n> 🚨 `relay` 확인\n```bash\nrun")).toBe("정책 버전: v2.18. 🚨 relay 확인 run");
    // A lone star is content, as in a tool pattern.
    expect(plainSkillText("allowed-tools: Bash(agent-browser:*)")).toBe("allowed-tools: Bash(agent-browser:*)");
  });
});
