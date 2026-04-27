import { describe, expect, it } from "vitest";
import {
  getProviderAccent,
  getProviderBorder,
  getProviderLabel,
  getProviderMeta,
  getProviderSeries,
  getProviderSoft,
} from "./providerTheme";

/**
 * Provider theme regression coverage for issue #1202 follow-up.
 *
 * The CSS exposes oklch tokens for every supported provider; these tests
 * pin the JS catalog to that same set so a typo in either side surfaces
 * immediately. We also verify the unknown-provider fallback uses a neutral
 * `--fg-faint` token so badges never render with `var(undefined)`.
 */

const PROVIDERS = [
  "claude",
  "codex",
  "gemini",
  "qwen",
  "opencode",
  "copilot",
  "antigravity",
  "api",
] as const;

describe("providerTheme catalog", () => {
  it.each(PROVIDERS)("maps %s to its CSS oklch tokens", (provider) => {
    expect(getProviderAccent(provider)).toBe(`var(--${provider})`);
    expect(getProviderSoft(provider)).toBe(`var(--${provider}-soft)`);
  });

  it("normalizes case and whitespace before lookup", () => {
    expect(getProviderAccent("  CLAUDE  ")).toBe("var(--claude)");
    expect(getProviderLabel(" qwen ")).toBe("Qwen");
  });

  it("falls back to a neutral foreground token for unknown providers", () => {
    expect(getProviderAccent("nonsense")).toBe("var(--fg-faint)");
    expect(getProviderAccent(null)).toBe("var(--fg-faint)");
    expect(getProviderAccent(undefined)).toBe("var(--fg-faint)");
  });

  it("returns soft fallback that anchors on the base background token", () => {
    const soft = getProviderSoft("nonsense");
    expect(soft).toContain("var(--fg-faint)");
    expect(soft).toContain("var(--bg-2)");
  });

  it("upper-cases unknown ids so the badge label still renders something", () => {
    expect(getProviderLabel("acme")).toBe("ACME");
    expect(getProviderLabel(null)).toBe("Unknown");
  });

  it("computes a border by mixing the accent against the line token", () => {
    const border = getProviderBorder("claude");
    expect(border).toContain("var(--claude)");
    expect(border).toContain("var(--line)");
  });

  it("returns a four-stop series for chart strokes", () => {
    const series = getProviderSeries("codex");
    expect(series).toHaveLength(4);
    expect(series[0]).toBe("var(--codex)");
    // Subsequent stops should keep the original token referenced via color-mix.
    series.slice(1).forEach((stop) => expect(stop).toContain("var(--codex)"));
  });

  it("packages id, label, bg, color, and border into a single meta tuple", () => {
    const meta = getProviderMeta("Gemini");
    expect(meta).toMatchObject({
      id: "gemini",
      label: "Gemini",
      color: "var(--gemini)",
      bg: "var(--gemini-soft)",
    });
    expect(meta.border).toContain("var(--gemini)");
  });
});
