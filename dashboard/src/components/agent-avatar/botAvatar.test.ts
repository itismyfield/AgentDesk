import { describe, expect, it } from "vitest";
import { validateAvatar } from "open-bot-avatar";
import { BOT_AVATARS, botAvatarColor, botAvatarFor, botAvatarStill } from "./botAvatar";

describe("bot avatar catalog", () => {
  it("resolves every listed shape to a valid definition with its own emoji", () => {
    expect(BOT_AVATARS).toHaveLength(35);
    for (const avatar of BOT_AVATARS) expect(validateAvatar(avatar.definition)).toMatchObject({ ok: true });
    expect(new Set(BOT_AVATARS.map((a) => a.definition.id)).size).toBe(BOT_AVATARS.length);
    expect(new Set(BOT_AVATARS.map((a) => a.emoji)).size).toBe(BOT_AVATARS.length);
  });

  it("maps the emoji agents use today to their shapes", () => {
    const shapeOf = (emoji: string) => botAvatarFor({ id: "agent", avatar_emoji: emoji }).definition.id;
    expect({
      "🧪": shapeOf("🧪"), "📊": shapeOf("📊"), "🎯": shapeOf("🎯"), "🎭": shapeOf("🎭"), "🎨": shapeOf("🎨"),
      "📐": shapeOf("📐"), "🔍": shapeOf("🔍"), "✍️": shapeOf("✍️"), "🖌️": shapeOf("🖌️"), "🔧": shapeOf("🔧"),
      "👨‍🍳": shapeOf("👨‍🍳"), "⛺": shapeOf("⛺"), "🏡": shapeOf("🏡"), "🏠": shapeOf("🏠"), "🧑‍💼": shapeOf("🧑‍💼"),
      "👩‍💼": shapeOf("👩‍💼"), "🏗️": shapeOf("🏗️"), "🏭": shapeOf("🏭"), "🤖": shapeOf("🤖"), "📡": shapeOf("📡"),
    }).toEqual({
      "🧪": "flask", "📊": "bar-chart", "🎯": "target", "🎭": "theater-mask", "🎨": "palette",
      "📐": "set-square", "🔍": "magnifier", "✍️": "book", "🖌️": "paint-brush", "🔧": "wrench",
      "👨‍🍳": "chef-hat", "⛺": "tent", "🏡": "cottage", "🏠": "house", "🧑‍💼": "briefcase",
      "👩‍💼": "briefcase", "🏗️": "hard-hat", "🏭": "factory", "🤖": "robot", "📡": "cloud",
    });
    // Same emoji typed without the variation selector.
    expect(shapeOf("🖌")).toBe("paint-brush");
  });

  it("gives an unknown or empty emoji a stable shape from the agent id", () => {
    const first = botAvatarFor({ id: "ops-bot", avatar_emoji: "🦄" });
    expect(botAvatarFor({ id: "ops-bot", avatar_emoji: "" })).toBe(first);
    expect(BOT_AVATARS).toContain(first);
  });

  it("keeps hex department colors and derives a hex color otherwise", () => {
    expect(botAvatarColor("#f97316", "a")).toBe("#f97316");
    const derived = botAvatarColor("hsl(10 50% 50%)", "ch-ad");
    expect(derived).toMatch(/^#[0-9a-f]{6}$/);
    expect(botAvatarColor(null, "ch-ad")).toBe(derived);
  });

  it("renders a cached SVG still", () => {
    const { definition } = botAvatarFor({ id: "a", avatar_emoji: "🧪" });
    const url = botAvatarStill(definition, "#f97316");
    expect(url.startsWith("data:image/svg+xml;charset=utf-8,")).toBe(true);
    expect(decodeURIComponent(url)).toContain('fill="#f97316"');
    expect(botAvatarStill(definition, "#f97316")).toBe(url);
  });
});
