import { describe, expect, it } from "vitest";
import {
  ACCENT_CONTRAST_MIN,
  DARK_PALETTE,
  LIGHT_PALETTE,
  TEXT_CONTRAST_MIN,
  lightnessDelta,
} from "./oklchContrast";

/**
 * These tests are guardrails, not exact WCAG calculators. If a future PR
 * shifts the L of any token in `main.css`, update the snapshot in
 * `oklchContrast.ts` and the assertions below will tell you whether the
 * change preserves enough perceptual contrast for the dashboard surface.
 */

describe("oklch contrast guardrails", () => {
  it.each([
    ["dark", DARK_PALETTE],
    ["light", LIGHT_PALETTE],
  ] as const)("body text on %s surface clears the AA proxy", (_label, palette) => {
    expect(lightnessDelta(palette.fg, palette.bg)).toBeGreaterThanOrEqual(
      TEXT_CONTRAST_MIN,
    );
    expect(lightnessDelta(palette.fg, palette.bg2)).toBeGreaterThanOrEqual(
      TEXT_CONTRAST_MIN,
    );
  });

  it.each([
    ["dark", DARK_PALETTE],
    ["light", LIGHT_PALETTE],
  ] as const)("dim and muted text stay readable on %s bg", (_label, palette) => {
    // Secondary text only needs to be clearly distinct from the surface and
    // from primary text — we check delta vs bg only because muted text is
    // intentionally lower-contrast for hierarchy.
    expect(lightnessDelta(palette.fgDim, palette.bg)).toBeGreaterThanOrEqual(0.4);
    expect(lightnessDelta(palette.fgMuted, palette.bg)).toBeGreaterThanOrEqual(
      0.16,
    );
  });

  it.each([
    ["dark", DARK_PALETTE],
    ["light", LIGHT_PALETTE],
  ] as const)("provider accents stand out on the %s surface", (_label, palette) => {
    Object.entries(palette.accents).forEach(([id, accent]) => {
      const delta = lightnessDelta(accent, palette.bg);
      // Non-text accents (provider dots, badge fills, chart strokes) only need
      // perceptual separation from the surface, not the full text threshold.
      expect(
        delta,
        `${id} L=${accent.L} too close to bg L=${palette.bg.L}`,
      ).toBeGreaterThanOrEqual(ACCENT_CONTRAST_MIN);
    });
  });

  it("light palette pulls accents toward darker L than dark palette", () => {
    // Per issue #1202 follow-up, light-mode provider colors should drop in
    // L (and slightly bump C) so they stay legible on the near-white bg.
    Object.keys(DARK_PALETTE.accents).forEach((id) => {
      const dark = DARK_PALETTE.accents[id];
      const light = LIGHT_PALETTE.accents[id];
      expect(
        light.L,
        `${id} light L=${light.L} should be lower than dark L=${dark.L}`,
      ).toBeLessThan(dark.L);
    });
  });
});
