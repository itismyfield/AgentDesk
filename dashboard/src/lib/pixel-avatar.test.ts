import { describe, expect, it } from "vitest";
import {
  PIXEL_AVATAR_PALETTES,
  buildPixelAvatarModel,
  hashAvatarSeed,
  resolveAvatarSeed,
} from "./pixel-avatar";
import { FONT_STACK_PIXEL, FONT_STACK_SANS, getFontFamilyForText } from "./fonts";

describe("pixel avatar seed helpers", () => {
  it("prefers explicit avatar_seed when present", () => {
    expect(resolveAvatarSeed({ avatar_seed: 42, id: "agent-1" })).toBe(42);
  });

  it("hashes fallback identity deterministically", () => {
    expect(hashAvatarSeed("agent-1")).toBe(hashAvatarSeed("agent-1"));
    expect(hashAvatarSeed("agent-1")).not.toBe(hashAvatarSeed("agent-2"));
  });
});

describe("buildPixelAvatarModel", () => {
  it("returns stable pixel output for the same seed", () => {
    const first = buildPixelAvatarModel(1337);
    const second = buildPixelAvatarModel(1337);
    expect(second).toEqual(first);
  });

  it("maps seeds into the 7-palette set", () => {
    const model = buildPixelAvatarModel(2026);
    expect(model.paletteIndex).toBeGreaterThanOrEqual(0);
    expect(model.paletteIndex).toBeLessThan(PIXEL_AVATAR_PALETTES.length);
    expect(model.pixels.length).toBeGreaterThan(20);
  });
});

describe("font fallback", () => {
  it("keeps pixel font for latin-only labels", () => {
    expect(getFontFamilyForText("DORO", "pixel")).toBe(FONT_STACK_PIXEL);
  });

  it("falls back to sans for Korean labels", () => {
    expect(getFontFamilyForText("도로롱", "pixel")).toBe(FONT_STACK_SANS);
  });
});
