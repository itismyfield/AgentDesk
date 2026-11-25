import { describe, expect, it } from "vitest";
import { ApiRequestError } from "../../api/httpClient";
import { auditAgents, auditProviders, auditState, defaultAuditProvider, fullPromptText, isAuditTestAgent, sectionLabels, sourceShares } from "./auditModel";

const agent = (id: string, cli_provider: string | undefined, channels: Record<string, string> = {}) => ({ id, cli_provider, ...channels }) as Parameters<typeof auditProviders>[0];
const ko = (text: string) => text;
const en = (_ko: string, text: string) => text;
const configError = (status: number) => new ApiRequestError("agent binding unavailable", { status, serverCode: "config" });

describe("audit bindings", () => {
  it("reads providers from the cc, cdx/alt and legacy channel columns", () => {
    expect(auditProviders(agent("a", "claude", { discord_channel_cc: "1", discord_channel_cdx: "2" }))).toEqual(["claude", "codex"]);
    expect(auditProviders(agent("b", "codex", { discord_channel_alt: "2" }))).toEqual(["codex"]);
    expect(auditProviders(agent("c", "codex", { discord_channel_id: "3" }))).toEqual(["codex"]);
    expect(auditProviders(agent("d", "claude", { discord_channel_id: "4" }))).toEqual(["claude"]);
    expect(auditProviders(agent("e", "claude", { discord_channel_cc: "  " }))).toEqual([]);
  });

  it("defaults to the agent's own provider, else the first bound one", () => {
    expect(defaultAuditProvider(agent("a", "codex", { discord_channel_cc: "1", discord_channel_cdx: "2" }))).toBe("codex");
    expect(defaultAuditProvider(agent("b", "antigravity", { discord_channel_cc: "1", discord_channel_cdx: "2" }))).toBe("claude");
  });

  it("drops unbound agents, puts relay harness agents last, and filters e2e agents when includeTests is false", () => {
    const list = [
      agent("relay-e2e", "claude", { discord_channel_cc: "9" }),
      { ...agent("named-harness", "claude", { discord_channel_cc: "8" }), name: "E2E Harness" },
      agent("none", "claude"),
      agent("real", "claude", { discord_channel_cc: "1" }),
    ];
    expect(auditAgents(list).map((item) => item.id)).toEqual(["real", "relay-e2e", "named-harness"]);
    expect(auditAgents(list, false).map((item) => item.id)).toEqual(["real"]);
    expect(isAuditTestAgent({ id: "named-harness", name: "E2E Harness" })).toBe(true);
  });
});

describe("audit read state", () => {
  it("tells a missing binding, a failure, an empty copy and data apart", () => {
    expect(auditState(configError(404), undefined)).toBe("unbound");
    expect(auditState(configError(409), "current_session")).toBe("unbound");
    expect(auditState(configError(503), undefined)).toBe("error");
    expect(auditState(new ApiRequestError("expired", { status: 401, serverCode: "config" }), undefined)).toBe("error");
    expect(auditState(new TypeError("Failed to fetch"), undefined)).toBe("error");
    expect(auditState(null, "unavailable")).toBe("unavailable");
    expect(auditState(null, "latest_saved")).toBe("ready");
    expect(auditState(null, undefined)).toBe("loading");
  });
});

describe("prompt sections", () => {
  const sections = [
    { name: "runtime", source: "runtime" as const, chars: 30, text: "intro" },
    { name: "role_prompt", source: "canon_file" as const, chars: 50, text: "# role\n" },
    { name: "runtime", source: "runtime" as const, chars: 15, text: "\n\npeers" },
    { name: "new_layer", source: "conditional" as const, chars: 5, text: "extra" },
  ];

  it("labels by order, numbering repeated labels and keeping unknown names", () => {
    expect(sectionLabels(sections, ko)).toEqual(["런타임 문맥 1", "역할 정의", "런타임 문맥 2", "new_layer"]);
    expect(sectionLabels(sections.slice(1, 3), en)).toEqual(["Role definition", "Runtime context"]);
  });

  it("sums characters per source in a fixed order", () => {
    expect(sourceShares(sections)).toEqual([
      { source: "canon_file", chars: 50, percent: 50 },
      { source: "memento", chars: 0, percent: 0 },
      { source: "runtime", chars: 45, percent: 45 },
      { source: "conditional", chars: 5, percent: 5 },
    ]);
    expect(sourceShares([]).every((share) => share.percent === 0)).toBe(true);
  });

  it("joins the texts in order with a blank line only where they would touch", () => {
    expect(fullPromptText(sections)).toBe("intro\n\n# role\n\n\npeers\n\nextra");
  });
});
