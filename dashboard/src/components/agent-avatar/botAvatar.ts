import { examples, renderAvatarSvg, type AvatarDefinition } from "open-bot-avatar";
import { GALLERY_AVATARS } from "./galleryAvatars";

export interface BotAvatar {
  definition: AvatarDefinition;
  color: string;
  eyeColor?: string;
}

const round = (id: string, name: string, path: string, x: number, y: number): AvatarDefinition => ({
  version: 1,
  id,
  name,
  body: { path },
  eyes: { x, y, spacing: 15, scale: 1.1 },
});

// Round bodies after the "Grok Bot" mark: a plain orb whose eyes sit high and to one side.
const ROUND_AVATARS: AvatarDefinition[] = [
  round("orb", "Orb", "M50 10A40 40 0 1 1 50 90A40 40 0 1 1 50 10Z", 50, 46),
  round("orb-glance", "Glancing orb", "M50 10A40 40 0 1 1 50 90A40 40 0 1 1 50 10Z", 62, 36),
  round("pebble", "Pebble", "M50 16C73 16 92 30 92 50C92 70 73 84 50 84C27 84 8 70 8 50C8 30 27 16 50 16Z", 60, 42),
  round("egg", "Egg", "M50 8C70 8 84 34 84 58C84 78 69 92 50 92C31 92 16 78 16 58C16 34 30 8 50 8Z", 56, 44),
  round("capsule", "Capsule", "M34 22H66A28 28 0 0 1 66 78H34A28 28 0 0 1 34 22Z", 50, 47),
  round("droplet", "Droplet", "M50 8C56 20 84 42 84 60A34 34 0 0 1 16 60C16 42 44 20 50 8Z", 50, 60),
];

const r1 = (n: number) => Math.round(n * 10) / 10;

/** A rounded regular polygon fitted to the 0–100 viewBox, with the eyes at its center. */
function polygon(sides: number, flat: boolean): AvatarDefinition {
  const offset = -Math.PI / 2 + (flat ? Math.PI / sides : 0);
  const raw = Array.from({ length: sides }, (_, k) => {
    const angle = offset + (2 * Math.PI * k) / sides;
    return [Math.cos(angle), Math.sin(angle)];
  });
  const xs = raw.map(([x]) => x);
  const ys = raw.map(([, y]) => y);
  const area = (sides / 2) * Math.sin((2 * Math.PI) / sides);
  // Fit the 80-unit box, but cap the area so squares don't outweigh triangles.
  const scale = Math.min(80 / Math.max(Math.max(...xs) - Math.min(...xs), Math.max(...ys) - Math.min(...ys)), Math.sqrt(4000 / area));
  const dx = 50 - ((Math.max(...xs) + Math.min(...xs)) / 2) * scale;
  const dy = 50 - ((Math.max(...ys) + Math.min(...ys)) / 2) * scale;
  const points = raw.map(([x, y]) => [x * scale + dx, y * scale + dy]);
  // Each corner becomes a curve between points a fifth of the way along its two edges.
  const along = (from: number[], to: number[]) => `${r1(from[0] + (to[0] - from[0]) * 0.2)} ${r1(from[1] + (to[1] - from[1]) * 0.2)}`;
  const path = `${points
    .map((vertex, k) => {
      const prev = points[(k + sides - 1) % sides];
      const next = points[(k + 1) % sides];
      return `${k === 0 ? "M" : "L"}${along(vertex, prev)}Q${r1(vertex[0])} ${r1(vertex[1])} ${along(vertex, next)}`;
    })
    .join("")}Z`;
  return { version: 1, id: `polygon-${sides}${flat ? "-flat" : ""}`, name: `Polygon ${sides}`, body: { path }, eyes: { x: r1(dx), y: r1(dy), spacing: 16, scale: 1.1 } };
}

// Flipping 7 and 8 sides looks the same, so they come once.
const POLYGON_AVATARS = ([[3, false], [3, true], [4, false], [4, true], [5, false], [5, true], [6, false], [6, true], [7, false], [8, false]] as const).map(
  ([sides, flat]) => polygon(sides, flat),
);

/** Package templates, round bodies and polygons; an agent's id picks one. */
export const AVATAR_SHAPES: AvatarDefinition[] = [...examples, ...GALLERY_AVATARS, ...ROUND_AVATARS, ...POLYGON_AVATARS];

// Many colors so two agents with the same shape still look apart; deep tones take white eyes.
// No grays (offline avatars are already grayscale) and no black (lost on the dark theme).
export const AVATAR_PALETTE: [body: string, eyes?: string][] = [
  ["#f87171"], ["#fb923c"], ["#fbbf24"], ["#facc15"], ["#a3e635"], ["#4ade80"], ["#34d399"], ["#2dd4bf"], ["#22d3ee"],
  ["#38bdf8"], ["#60a5fa"], ["#818cf8"], ["#a78bfa"], ["#c084fc"], ["#e879f9"], ["#f472b6"], ["#fb7185"],
  ["#dc2626", "#ffffff"], ["#ea580c", "#ffffff"], ["#16a34a", "#ffffff"], ["#0d9488", "#ffffff"], ["#2563eb", "#ffffff"],
  ["#7c3aed", "#ffffff"], ["#c026d3", "#ffffff"], ["#be123c", "#ffffff"],
];

function hash(text: string): number {
  let value = 0x811c9dc5;
  for (let i = 0; i < text.length; i += 1) value = Math.imul(value ^ text.charCodeAt(i), 0x01000193);
  value = Math.imul(value ^ (value >>> 15), 0x2c1b3c6d);
  return (value ^ (value >>> 13)) >>> 0;
}

function look(agentId: string, colorStep = 0): BotAvatar {
  const [color, eyeColor] = AVATAR_PALETTE[(hash(`color:${agentId}`) + colorStep) % AVATAR_PALETTE.length];
  return { definition: AVATAR_SHAPES[hash(agentId) % AVATAR_SHAPES.length], color, eyeColor };
}

const rosters = new WeakMap<readonly RosterAgent[], Map<string, BotAvatar>>();
type RosterAgent = { id: string; created_at: number };

/**
 * Looks for a whole roster: shape and color come from the id, and an agent whose look an older
 * agent already has moves on to the next color, so no two agents look the same.
 */
export function avatarRoster(agents: readonly RosterAgent[]): Map<string, BotAvatar> {
  let roster = rosters.get(agents);
  if (roster) return roster;
  roster = new Map();
  const taken = new Set<string>();
  const ordered = [...agents].sort((a, b) => a.created_at - b.created_at || a.id.localeCompare(b.id));
  for (const { id } of ordered) {
    let avatar = look(id);
    for (let step = 1; taken.has(`${avatar.definition.id}|${avatar.color}`) && step < AVATAR_PALETTE.length; step += 1) avatar = look(id, step);
    taken.add(`${avatar.definition.id}|${avatar.color}`);
    roster.set(id, avatar);
  }
  rosters.set(agents, roster);
  return roster;
}

/** The agent's look from the roster, or straight from its id when it isn't in one. */
export function botAvatarFor(agentId: string, roster?: Map<string, BotAvatar>): BotAvatar {
  return roster?.get(agentId) ?? look(agentId);
}

const stills = new Map<string, string>();

/** Cached calm frame (eyes open) as a data URL; an <img> keeps each SVG's internal ids out of the page DOM. */
export function botAvatarStill({ definition, color, eyeColor }: BotAvatar): string {
  const key = `${definition.id}|${color}|${eyeColor ?? ""}`;
  let url = stills.get(key);
  if (!url) {
    url = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(renderAvatarSvg(definition, { color, eyeColor, reducedMotion: true }))}`;
    stills.set(key, url);
  }
  return url;
}
