// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it } from "vitest";
import type { TokenAnalyticsResponse } from "../../types";
import { StatsBreakdown, shareLabel } from "./StatsBreakdown";
import { buildAgentItems, buildAgentSkillRows, buildModelItems, dailySeries, msg } from "./statsModel";

let container: HTMLDivElement;
let root: Root;
const t = (messages: ReturnType<typeof msg>) => messages.ko;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  const dialog = HTMLDialogElement.prototype as HTMLDialogElement & { showModal?: () => void };
  if (typeof dialog.showModal !== "function") {
    dialog.showModal = function showModal(this: HTMLDialogElement) { this.setAttribute("open", ""); };
  }
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

function analytics(): TokenAnalyticsResponse {
  return {
    period: "7d", period_label: "7d", days: 7, generated_at: "2026-11-18T00:00:00Z",
    summary: { total_tokens: 1000, total_cost: 3, cache_discount: 1, total_messages: 1, total_sessions: 1, active_days: 1, average_daily_tokens: 1000, peak_day: null },
    receipt: {
      period_label: "7d", period_start: "2026-11-12", period_end: "2026-11-18", subtotal: 4, cache_discount: 1, total: 3,
      stats: { total_messages: 1, total_sessions: 1 }, providers: [],
      models: [
        { model: "small", display_name: "Small Model", provider: "codex", input_tokens: 100, output_tokens: 100, cache_read_tokens: 0, cache_creation_tokens: 0, total_tokens: 200, cost: 0.5, cost_without_cache: 0.5 },
        { model: "big", display_name: "Big Model", provider: "claude", input_tokens: 200, output_tokens: 200, cache_read_tokens: 300, cache_creation_tokens: 100, total_tokens: 800, cost: 2.5, cost_without_cache: 3 },
      ],
      agents: [
        { agent: "qa", tokens: 250, cost: 1, percentage: 25 },
        { agent: "pm", tokens: 750, cost: 2, cost_without_cache: 2.5, input_tokens: 200, cache_read_tokens: 300, cache_creation_tokens: 100, percentage: 75 },
      ],
    },
    daily: [], heatmap: [],
  };
}

async function render(overrides: Partial<Parameters<typeof StatsBreakdown>[0]> = {}) {
  const data = analytics();
  const skills = buildAgentSkillRows({
    window: "7d", overall: [],
    byAgent: [{ agent_role_id: "pm", agent_name: "PM", skill_name: "plan-review", skill_desc_ko: "계획 검토", calls: 9, last_used_at: 0 }],
  }, [], "ko");
  await act(async () => root.render(
    <StatsBreakdown
      t={t}
      loading={false}
      failed={false}
      models={buildModelItems(data)}
      agents={buildAgentItems(data)}
      overallCacheHitRate={42}
      agentSkillRows={skills}
      series={dailySeries(t)}
      {...overrides}
    />,
  ));
}
const rows = () => Array.from(container.querySelectorAll<HTMLButtonElement>(".aui-compact-breakdown-row"));

it("lists models by tokens with their share in the kit list, and switches to agents with the overall cache hit", async () => {
  await render();
  expect(rows().map((row) => row.dataset.itemId)).toEqual(["claude-big-0", "codex-small-1"]);
  expect(rows()[0].getAttribute("aria-label")).toBe("Big Model, 800, 전체의 80%");
  expect(container.querySelector('[data-testid="stats-model-share"]')).not.toBeNull();

  const agentsTab = Array.from(container.querySelectorAll<HTMLButtonElement>('[role="tab"]')).find((tab) => tab.textContent?.includes("에이전트"))!;
  await act(async () => agentsTab.click());
  expect(container.querySelector('[data-testid="stats-agent-cache"]')?.textContent).toContain("전체 캐시 히트 42.0%");
  expect(rows().map((row) => row.dataset.itemId)).toEqual(["pm", "qa"]);
});

it("opens the selected row's detail with its facts and the agent's top skills", async () => {
  await render();
  const agentsTab = Array.from(container.querySelectorAll<HTMLButtonElement>('[role="tab"]')).find((tab) => tab.textContent?.includes("에이전트"))!;
  await act(async () => agentsTab.click());
  // A real button takes Tab, Enter and Space; the kit hands its id over and marks the open row.
  expect(rows().map((row) => [row.tagName, row.getAttribute("aria-current")])).toEqual([["BUTTON", null], ["BUTTON", null]]);
  await act(async () => rows()[0].click());
  expect(rows().map((row) => row.getAttribute("aria-current"))).toEqual(["true", null]);
  const detail = document.querySelector<HTMLElement>('[data-testid="stats-detail-body"]')!;
  expect(detail.dataset.item).toBe("pm");
  expect(detail.textContent).toContain("캐시 절감$0.50");
  expect(detail.textContent).toContain("plan-review");
  expect(detail.textContent).toContain("9 회");
});

it("keeps empty, loading and failed lists distinct", async () => {
  await render({ models: [], agents: [], loading: true });
  expect(container.querySelector('[role="status"]')?.getAttribute("aria-label")).toBe("모델 분포를 불러오는 중입니다.");
  await render({ models: [], agents: [], loading: false, failed: true });
  expect(container.textContent).toContain("분류를 불러오지 못했어요");
  await render({ models: [], agents: [], loading: false, failed: false });
  expect(container.textContent).toContain("표시할 모델 데이터가 없습니다.");
});

it("rounds list shares and marks tiny ones", () => {
  expect(shareLabel(0)).toBe("0");
  expect(shareLabel(0.4)).toBe("<1");
  expect(shareLabel(62.6)).toBe("63");
});
