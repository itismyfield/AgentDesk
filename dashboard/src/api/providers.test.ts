import { describe, expect, it } from "vitest";

import {
  catalogLabel,
  meetingCatalogIds,
  selectableCatalogIds,
  type ProviderCatalogEntry,
} from "./providers";

function entry(
  id: string,
  extras: Partial<ProviderCatalogEntry> = {},
): ProviderCatalogEntry {
  return {
    id,
    display_name: id,
    channel_suffix: null,
    binary_name: id,
    execution_surface: id,
    supports_resume: true,
    supports_structured_output: true,
    supports_tool_stream: true,
    supports_restricted_tool_policy: true,
    supports_tui_hosting: false,
    system_prompt_transport: "native",
    context_window_tokens: null,
    ...extras,
  };
}

describe("provider catalog presentation", () => {
  it("keeps registry providers selectable and excludes legacy-only ids", () => {
    const ids = selectableCatalogIds([
      entry("claude"),
      entry("opencode"),
      entry("copilot"),
      entry("api"),
    ]);
    expect(ids).toEqual(["claude", "opencode"]);
  });

  it("keeps a legacy current id visible without adding it to create lists", () => {
    const ids = selectableCatalogIds([entry("claude"), entry("qwen")], "api");
    expect(ids).toEqual(["api", "claude", "qwen"]);
  });

  it("filters meetings by the restricted-tool-policy capability", () => {
    const ids = meetingCatalogIds([
      entry("claude"),
      entry("qwen"),
      entry("future", { supports_restricted_tool_policy: false }),
    ]);
    expect(ids).toEqual(["claude", "qwen"]);
  });

  it("prefers catalog names and falls back to themed labels", () => {
    expect(
      catalogLabel([entry("qwen", { display_name: "Qwen Code" })], "qwen"),
    ).toBe("Qwen Code");
    expect(catalogLabel([], "claude")).toBe("Claude");
  });
});
