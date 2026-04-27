/**
 * Lightweight oklch perceptual-contrast guardrail.
 *
 * We do NOT parse the entire oklch color space; we just snapshot the L
 * (lightness) values that we ship in `main.css` and compare them against
 * the background L for each theme. The rule of thumb (per the spec
 * referenced in issue #1202): keep |Δ L| ≥ 0.45 between fg-text and bg
 * surfaces so AA-grade contrast holds for body copy, and ≥ 0.30 for
 * large-text / non-text accents like provider badges.
 *
 * If either side of the palette drifts (for example a future PR raises
 * `--fg-muted` toward the bg), the unit test in `oklchContrast.test.ts`
 * fails loudly before the change reaches review.
 */

export interface OklchSnapshot {
  /** Lightness 0..1 from the `oklch(L C H)` source-of-truth in main.css. */
  L: number;
  /** Optional chroma; present so the snapshot stays self-documenting. */
  C?: number;
  /** Optional hue. */
  H?: number;
}

export interface ThemePalette {
  bg: OklchSnapshot;
  bg2: OklchSnapshot;
  fg: OklchSnapshot;
  fgDim: OklchSnapshot;
  fgMuted: OklchSnapshot;
  /** Provider/semantic accents that must stay readable on the surface. */
  accents: Record<string, OklchSnapshot>;
}

export const DARK_PALETTE: ThemePalette = {
  bg: { L: 0.16, C: 0.006, H: 250 },
  bg2: { L: 0.24, C: 0.008, H: 250 },
  fg: { L: 0.96, C: 0.004, H: 250 },
  fgDim: { L: 0.82, C: 0.01, H: 250 },
  fgMuted: { L: 0.68, C: 0.009, H: 250 },
  accents: {
    claude: { L: 0.77, C: 0.16, H: 71 },
    codex: { L: 0.77, C: 0.12, H: 214 },
    gemini: { L: 0.72, C: 0.16, H: 258 },
    qwen: { L: 0.74, C: 0.18, H: 319 },
    opencode: { L: 0.76, C: 0.11, H: 195 },
    copilot: { L: 0.75, C: 0.14, H: 160 },
    antigravity: { L: 0.74, C: 0.17, H: 45 },
    ok: { L: 0.78, C: 0.17, H: 150 },
    warn: { L: 0.81, C: 0.16, H: 85 },
    err: { L: 0.67, C: 0.22, H: 22 },
    info: { L: 0.76, C: 0.12, H: 230 },
  },
};

export const LIGHT_PALETTE: ThemePalette = {
  bg: { L: 0.98, C: 0.003, H: 250 },
  bg2: { L: 1, C: 0, H: 0 },
  fg: { L: 0.21, C: 0.01, H: 250 },
  fgDim: { L: 0.36, C: 0.01, H: 250 },
  fgMuted: { L: 0.49, C: 0.012, H: 250 },
  accents: {
    claude: { L: 0.59, C: 0.18, H: 71 },
    codex: { L: 0.56, C: 0.14, H: 214 },
    gemini: { L: 0.54, C: 0.18, H: 258 },
    qwen: { L: 0.55, C: 0.2, H: 319 },
    opencode: { L: 0.56, C: 0.13, H: 195 },
    copilot: { L: 0.55, C: 0.16, H: 160 },
    antigravity: { L: 0.56, C: 0.19, H: 45 },
    ok: { L: 0.55, C: 0.17, H: 150 },
    warn: { L: 0.62, C: 0.16, H: 85 },
    err: { L: 0.55, C: 0.22, H: 22 },
    info: { L: 0.55, C: 0.13, H: 230 },
  },
};

/** Minimum lightness delta for body text against its surface (AA proxy). */
export const TEXT_CONTRAST_MIN = 0.45;
/** Minimum lightness delta for accent dots / badge fills against the surface. */
export const ACCENT_CONTRAST_MIN = 0.18;

export function lightnessDelta(
  fg: OklchSnapshot,
  bg: OklchSnapshot,
): number {
  return Math.abs(fg.L - bg.L);
}
