import { describe, expect, it } from "vitest";
import { connectionItems } from "./SettingsConnectionsModel";
import {
  applyText,
  baselineText,
  catalogCounts,
  configItems,
  draftProblem,
  filterCatalog,
  appItems,
  configValue,
  formatSettingValue,
  isResetValue,
  resolveCategory,
  runtimeItems,
  withRo,
  type ConfigEntry,
} from "./SettingsModel";

const ko = (text: string) => text;
const entry = (key: string, extra: Partial<ConfigEntry> = {}): ConfigEntry => ({
  key, category: "x", value: "1", label_ko: `${key} 이름`, label_en: key, restart_behavior: "persist-live-override", ...extra,
});
const source = <T,>(data: T | undefined) => ({ data, isError: false, isPending: false });

describe("settings catalog", () => {
  it("reads the category from the URL and maps old tab ids", () => {
    expect(resolveCategory(new URLSearchParams("category=connections"))).toBe("connections");
    const old = { general: "system", voice: "system", workflow: "workflow", pipeline: "workflow", runtime: "workflow", ai: "usage", providers: "usage", connections: "connections", connectors: "connections", onboarding: "connections", machine: "system" };
    for (const [id, category] of Object.entries(old)) expect(resolveCategory(new URLSearchParams(`settingsPanel=${id}`))).toBe(category);
    expect(resolveCategory(new URLSearchParams("category=nope"))).toBe("all");
  });

  it("hides the retired deadlock key and sends unknown keys to other with no description", () => {
    const items = configItems([
      entry("deadlock_manager_channel_id"),
      entry("max_review_rounds", { value: "30", baseline: "3", baseline_source: "hardcoded", override_active: true }),
      entry("brand_new_key"),
      entry("server_port", { editable: false, restart_behavior: "config-only" }),
    ], { max_review_rounds: "10" }, true);
    expect(items.map((item) => item.key)).toEqual(["max_review_rounds", "brand_new_key", "server_port"]);
    const [rounds, unknown, port] = items;
    expect(rounds).toMatchObject({ category: "review", label: "max_review_rounds 이름", draft: "10", overridden: true });
    expect(rounds.description).toContain("사람 판단 대기");
    expect(baselineText(rounds, ko)).toBe("기본 3 · 코드");
    expect(unknown).toMatchObject({ category: "other", description: "", input: "text" });
    expect(port).toMatchObject({ input: "readonly", restartNeeded: true });
    expect(applyText(port, ko)).toBe("설정 파일에서만 · 재시작해야 반영");
  });

  it("labels runtime keys and compares saved values with defaults", () => {
    const items = runtimeItems({
      saved: { maxEntryRetries: 5, dispatchRateLimitGateEnabled: true },
      draft: { maxEntryRetries: 7, dispatchRateLimitGateEnabled: true },
      defaults: { maxEntryRetries: 3, dispatchRateLimitGateEnabled: true },
      changedKeys: new Set(["maxEntryRetries"]),
    }, true, true);
    expect(items[0]).toMatchObject({ label: "자동 대기열 재시도 횟수", saved: "5", draft: "7", overridden: true, input: "number" });
    expect(items[1]).toMatchObject({ input: "toggle", overridden: false, draft: null });
    expect(applyText(items[0], ko)).toBe("바로 · 재시작하면 기본값으로");
    expect(baselineText(items[0], ko)).toBe("기본 3 · 서버 기본값");
  });

  it("lets the Claude-only compact value go back to following the shared one", () => {
    const claude = (value: string, baseline: string | null = null) => configItems([entry("context_compact_percent_claude", { value, baseline, override_active: value !== baseline })], {}, true)[0];
    expect(claude("70")).toMatchObject({ resetTo: "", overridden: true });
    expect(claude("")).toMatchObject({ resetTo: "", overridden: false });
    expect(formatSettingValue("", claude(""), ko)).toBe("공통 값 따름");
    // With a Claude value in the config file, "" falls back to that value, not to the shared one.
    const yaml = claude("", "70");
    expect([formatSettingValue("", yaml, ko), yaml.overridden, isResetValue(yaml, "70"), isResetValue(yaml, "80")]).toEqual(["설정 파일 기본값 70", false, true, false]);
    const [floor] = configItems([entry("context_compact_lower_bound_tokens")], {}, true);
    expect(floor.description).toContain("20만 토큰 창이면 이 값이 30만이어도 13만 6천 토큰에서 압축해요");
  });

  it("counts and filters by difference, restart and search over name, key and description", () => {
    const items = configItems([
      entry("review_enabled", { override_active: true }),
      entry("server_port", { editable: false, restart_behavior: "config-only" }),
      entry("in_progress_stale_min"),
    ], {}, true);
    expect(catalogCounts(items)).toMatchObject({ total: 3, differs: 1, restart: 1, byCategory: { review: 1, system: 1, workflow: 1 } });
    const base = { category: "all" as const, query: "", differs: false, restart: false };
    expect(filterCatalog(items, { ...base, differs: true }).map((item) => item.key)).toEqual(["review_enabled"]);
    expect(filterCatalog(items, { ...base, restart: true }).map((item) => item.key)).toEqual(["server_port"]);
    expect(filterCatalog(items, { ...base, query: "활동이 없으면" }).map((item) => item.key)).toEqual(["in_progress_stale_min"]);
  });

  it("checks drafts and reads josa the way numbers sound", () => {
    const [rounds] = configItems([entry("max_review_rounds")], {}, true);
    expect(draftProblem(rounds, "0", ko)).toBe("1 이상인 숫자를 넣어 주세요.");
    expect(draftProblem(rounds, "", ko)).toBeNull();
    expect([draftProblem(rounds, "60.5", ko), draftProblem(rounds, "1e2", ko), configValue(rounds, "1e2")]).toEqual(["소수점 없는 정수를 넣어 주세요.", null, "100"]);
    expect(["3", "10", "1", "7", "켜짐", "기본"].map(withRo)).toEqual(["3으로", "10으로", "1로", "7로", "켜짐으로", "기본으로"]);
    const [language, theme] = appItems({ language: "ja", theme: "auto" }, {}, true);
    expect(language.options?.map((option) => option.label)).toEqual(["한국어", "English", "日本語", "中文"]);
    expect([formatSettingValue(theme.saved, theme, ko), theme.overridden, baselineText(language, ko)]).toEqual(["시스템 설정 따르기", true, "기본 한국어 · 코드"]);
  });

  it("turns health, retention and connectors into status rows without zero placeholders", () => {
    const items = connectionItems({
      statusRows: [],
      health: source({ status: "healthy", publicOnly: "forbidden" as const }),
      retention: source({ enabled: true, retention_days: 30, restart_required_for_config_changes: true, total_stored_bytes: 0, layer_count: 0, truncated_count: 0 } as never),
      connectors: source({ connectors: [{ id: "obsidian_skill_root", name: "Obsidian skill root", state: "missing_path", optional: true, env_var: "AGENTDESK_OBSIDIAN_SKILL_ROOT", source: "/vault/99_Skills", reason: "missing_path", detail: "", setup_actions: ["x"], capabilities: [] }] } as never),
    }, ko, true);
    const byKey = Object.fromEntries(items.map((item) => [item.key, item]));
    expect(byKey["health.status"].status?.facts.map(([label]) => label)).toEqual(["자세한 지표"]);
    expect(byKey["health.dispatch_outbox"].saved).toBe("—");
    expect(byKey.prompt_manifest_retention).toMatchObject({ category: "system", saved: "30일", restartNeeded: true });
    expect(byKey.obsidian_skill_root).toMatchObject({ label: "Obsidian 스킬 폴더", saved: "폴더 없음" });
    expect(byKey.obsidian_skill_root.status?.steps?.lines[0]).toBe("지정한 폴더를 만드세요: /vault/99_Skills");
  });
});
