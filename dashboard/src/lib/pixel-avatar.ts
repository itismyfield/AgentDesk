export interface AvatarSeedSource {
  avatar_seed?: number | null;
  id?: string | null;
  name?: string | null;
  name_ko?: string | null;
  avatar_emoji?: string | null;
}

export interface PixelAvatarPalette {
  background: string;
  skin: string;
  hair: string;
  outfit: string;
  accent: string;
  shadow: string;
  eye: string;
}

export interface PixelAvatarPixel {
  x: number;
  y: number;
  color: string;
}

export interface PixelAvatarModel {
  paletteIndex: number;
  palette: PixelAvatarPalette;
  pixels: PixelAvatarPixel[];
}

export const PIXEL_AVATAR_SIZE = 8;

export const PIXEL_AVATAR_PALETTES: PixelAvatarPalette[] = [
  {
    background: "#1f2331",
    skin: "#f1c8a5",
    hair: "#6f4f3d",
    outfit: "#4fd1a5",
    accent: "#f8f0c7",
    shadow: "#132238",
    eye: "#101419",
  },
  {
    background: "#2b1f35",
    skin: "#e8b894",
    hair: "#2d314a",
    outfit: "#6ea8ff",
    accent: "#ffd166",
    shadow: "#151121",
    eye: "#0d1016",
  },
  {
    background: "#162229",
    skin: "#f0d0b1",
    hair: "#205f6b",
    outfit: "#f97316",
    accent: "#fef08a",
    shadow: "#0f161c",
    eye: "#11151b",
  },
  {
    background: "#2d2219",
    skin: "#cfa580",
    hair: "#7b2c2c",
    outfit: "#c084fc",
    accent: "#fde68a",
    shadow: "#17110c",
    eye: "#0f1014",
  },
  {
    background: "#13252e",
    skin: "#e6bc96",
    hair: "#0f766e",
    outfit: "#ef4444",
    accent: "#fde68a",
    shadow: "#0b1418",
    eye: "#0b0f14",
  },
  {
    background: "#2c2f36",
    skin: "#f5cfb5",
    hair: "#3f3f46",
    outfit: "#84cc16",
    accent: "#fb7185",
    shadow: "#171a20",
    eye: "#0f1218",
  },
  {
    background: "#1f2a22",
    skin: "#ddb38a",
    hair: "#92400e",
    outfit: "#38bdf8",
    accent: "#facc15",
    shadow: "#101612",
    eye: "#0d1014",
  },
];

export function normalizeAvatarSeed(seed: number | null | undefined): number {
  const numeric = Number.isFinite(seed) ? Math.trunc(seed ?? 0) : 0;
  const normalized = numeric >>> 0;
  return normalized === 0 ? 0x9e3779b9 : normalized;
}

export function hashAvatarSeed(
  value: string | number | null | undefined,
): number {
  if (typeof value === "number" && Number.isFinite(value)) {
    return normalizeAvatarSeed(value);
  }
  const source = `${value ?? "agentdesk"}`;
  let hash = 2166136261;
  for (let i = 0; i < source.length; i += 1) {
    hash ^= source.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return normalizeAvatarSeed(hash);
}

export function resolveAvatarSeed(source: AvatarSeedSource | undefined): number {
  if (!source) return normalizeAvatarSeed(0);
  if (source.avatar_seed != null && Number.isFinite(source.avatar_seed)) {
    return normalizeAvatarSeed(source.avatar_seed);
  }
  return hashAvatarSeed(
    source.id ??
      source.name ??
      source.name_ko ??
      source.avatar_emoji ??
      "agentdesk",
  );
}

function createPrng(seed: number): () => number {
  let state = normalizeAvatarSeed(seed);
  return () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return (state >>> 0) / 0xffffffff;
  };
}

function randInt(prng: () => number, maxExclusive: number): number {
  return Math.floor(prng() * maxExclusive);
}

function setPixel(grid: string[][], x: number, y: number, color: string): void {
  if (x < 0 || x >= PIXEL_AVATAR_SIZE || y < 0 || y >= PIXEL_AVATAR_SIZE) return;
  grid[y][x] = color;
}

function setMirrorPixel(
  grid: string[][],
  x: number,
  y: number,
  color: string,
  mirror = true,
): void {
  setPixel(grid, x, y, color);
  if (mirror) setPixel(grid, PIXEL_AVATAR_SIZE - 1 - x, y, color);
}

function fillMirrorBand(
  grid: string[][],
  y: number,
  colors: Array<string | null>,
): void {
  colors.forEach((color, x) => {
    if (!color) return;
    setMirrorPixel(grid, x, y, color);
  });
}

export function buildPixelAvatarModel(seed: number): PixelAvatarModel {
  const safeSeed = normalizeAvatarSeed(seed);
  const prng = createPrng(safeSeed);
  const paletteIndex = safeSeed % PIXEL_AVATAR_PALETTES.length;
  const palette = PIXEL_AVATAR_PALETTES[paletteIndex];
  const grid = Array.from({ length: PIXEL_AVATAR_SIZE }, () =>
    Array.from({ length: PIXEL_AVATAR_SIZE }, () => palette.background),
  );

  const hairline = 1 + randInt(prng, 2);
  const fringePatterns: Array<Array<string | null>> = [
    [null, palette.hair, palette.hair, null],
    [palette.hair, null, palette.hair, null],
    [null, palette.hair, null, palette.hair],
    [palette.hair, palette.hair, null, null],
  ];
  const browPatterns: Array<Array<string | null>> = [
    [null, palette.hair, palette.hair, null],
    [palette.hair, null, null, palette.hair],
    [null, palette.hair, null, palette.hair],
  ];
  const shoulderPatterns: Array<Array<string | null>> = [
    [null, palette.outfit, palette.outfit, palette.outfit],
    [palette.outfit, palette.outfit, palette.outfit, null],
    [null, palette.outfit, palette.accent, palette.outfit],
  ];
  const collarPatterns: Array<Array<string | null>> = [
    [null, null, palette.accent, null],
    [null, palette.accent, palette.accent, null],
    [null, palette.accent, null, palette.outfit],
  ];

  fillMirrorBand(grid, 0, [null, palette.hair, palette.hair, palette.hair]);
  fillMirrorBand(grid, 1, hairline === 2 ? [palette.hair, palette.hair, palette.hair, palette.hair] : fringePatterns[randInt(prng, fringePatterns.length)]);
  fillMirrorBand(grid, 2, [palette.hair, palette.skin, palette.skin, palette.skin]);
  fillMirrorBand(grid, 3, [palette.hair, palette.skin, palette.skin, palette.skin]);
  fillMirrorBand(grid, 4, [null, palette.skin, palette.skin, palette.skin]);
  fillMirrorBand(grid, 5, shoulderPatterns[randInt(prng, shoulderPatterns.length)]);
  fillMirrorBand(grid, 6, [palette.outfit, palette.outfit, palette.outfit, palette.outfit]);
  fillMirrorBand(grid, 7, [null, palette.outfit, palette.outfit, palette.outfit]);

  fillMirrorBand(grid, 2 + hairline, browPatterns[randInt(prng, browPatterns.length)]);
  fillMirrorBand(grid, 5, collarPatterns[randInt(prng, collarPatterns.length)]);

  const eyeRow = 3;
  setMirrorPixel(grid, 2, eyeRow, palette.eye);
  setMirrorPixel(grid, 2, eyeRow + (randInt(prng, 3) === 0 ? 1 : 0), palette.eye);

  const mouthType = randInt(prng, 3);
  if (mouthType === 0) {
    setPixel(grid, 3, 4, palette.shadow);
    setPixel(grid, 4, 4, palette.shadow);
  } else if (mouthType === 1) {
    setPixel(grid, 3, 4, palette.shadow);
  } else {
    setPixel(grid, 4, 4, palette.shadow);
  }

  const accentType = randInt(prng, 4);
  if (accentType === 0) {
    setMirrorPixel(grid, 1, 1, palette.accent);
  } else if (accentType === 1) {
    setMirrorPixel(grid, 0, 3, palette.accent);
  } else if (accentType === 2) {
    setPixel(grid, 3, 6, palette.accent);
    setPixel(grid, 4, 6, palette.accent);
  } else {
    setMirrorPixel(grid, 1, 5, palette.accent);
  }

  const shadowRow = randInt(prng, 2) === 0 ? 7 : 6;
  setMirrorPixel(grid, 0, shadowRow, palette.shadow);
  setMirrorPixel(grid, 1, shadowRow, palette.shadow, false);
  setMirrorPixel(grid, 6, shadowRow, palette.shadow, false);

  const pixels: PixelAvatarPixel[] = [];
  for (let y = 0; y < grid.length; y += 1) {
    for (let x = 0; x < grid[y].length; x += 1) {
      const color = grid[y][x];
      if (color === palette.background) continue;
      pixels.push({ x, y, color });
    }
  }

  return { paletteIndex, palette, pixels };
}
