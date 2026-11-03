import { examples, renderAvatarSvg, type AvatarDefinition } from "open-bot-avatar";
import { CUSTOM_AVATARS } from "./customAvatars";
import { GALLERY_AVATARS } from "./galleryAvatars";

export interface BotAvatar {
  definition: AvatarDefinition;
  /** Saved to `avatar_emoji` when picked, so emoji-only surfaces (Discord, CLI) keep a matching icon. */
  emoji: string;
  nameKo: string;
}

// Picker order: role shapes first, then general ones.
const CATALOG: [id: string, emoji: string, nameKo: string][] = [
  ["robot", "🤖", "로봇"],
  ["hard-hat", "🏗️", "안전모"],
  ["factory", "🏭", "공장"],
  ["bar-chart", "📊", "막대 차트"],
  ["flask", "🧪", "플라스크"],
  ["target", "🎯", "과녁"],
  ["wrench", "🔧", "렌치"],
  ["set-square", "📐", "삼각자"],
  ["theater-mask", "🎭", "가면"],
  ["palette", "🎨", "팔레트"],
  ["paint-brush", "🖌️", "붓"],
  ["magnifier", "🔍", "돋보기"],
  ["book", "📖", "책"],
  ["chef-hat", "👨‍🍳", "요리사 모자"],
  ["tent", "⛺", "텐트"],
  ["house", "🏠", "집"],
  ["cottage", "🏡", "정원 집"],
  ["briefcase", "💼", "서류 가방"],
  ["chat", "💬", "말풍선"],
  ["lightbulb", "💡", "전구"],
  ["controller", "🎮", "게임 패드"],
  ["rocket", "🚀", "로켓"],
  ["brain", "🧠", "두뇌"],
  ["shield", "🛡️", "방패"],
  ["star", "⭐", "별"],
  ["cloud", "☁️", "구름"],
  ["camera", "📷", "카메라"],
  ["mug", "☕", "머그컵"],
  ["sprout", "🌱", "새싹"],
  ["barbell", "🏋️", "바벨"],
  ["cat", "🐱", "고양이"],
  ["bunny", "🐰", "토끼"],
  ["bull", "🐂", "황소"],
  ["ghost", "👻", "유령"],
  ["witch-hat", "🧙", "마법사 모자"],
];

// Emoji agents already use that share another emoji's shape.
const ALIASES: Record<string, string> = {
  "✍️": "book", "📝": "book", "📚": "book",
  "🧑‍💼": "briefcase", "👩‍💼": "briefcase", "👨‍💼": "briefcase",
  "👷": "hard-hat", "🧑‍🍳": "chef-hat", "👩‍🍳": "chef-hat",
  "🛠️": "wrench", "⚙️": "wrench", "📡": "cloud", "🔎": "magnifier",
  "📈": "bar-chart", "📉": "bar-chart", "🕹️": "controller", "🐈": "cat", "🐇": "bunny",
};

const definitions = new Map([...examples, ...GALLERY_AVATARS, ...CUSTOM_AVATARS].map((d) => [d.id, d]));

export const BOT_AVATARS: BotAvatar[] = CATALOG.flatMap(([id, emoji, nameKo]) => {
  const definition = definitions.get(id);
  return definition ? [{ definition, emoji, nameKo }] : [];
});

// Emoji differ only by the variation selector across keyboards ("🖌" vs "🖌️").
const emojiKey = (emoji: string) => emoji.replace(/️/g, "").trim();
const byEmoji = new Map(BOT_AVATARS.map((avatar) => [emojiKey(avatar.emoji), avatar]));
for (const [emoji, id] of Object.entries(ALIASES)) {
  const avatar = BOT_AVATARS.find((entry) => entry.definition.id === id);
  if (avatar) byEmoji.set(emojiKey(emoji), avatar);
}

function hash(text: string): number {
  let value = 0;
  for (let i = 0; i < text.length; i += 1) value = (value * 31 + text.charCodeAt(i)) >>> 0;
  return value;
}

/** The agent's shape: the one its emoji names, otherwise a stable pick from its id. */
export function botAvatarFor(agent: { id: string; avatar_emoji?: string | null }): BotAvatar {
  return byEmoji.get(emojiKey(agent.avatar_emoji ?? "")) ?? BOT_AVATARS[hash(agent.id) % BOT_AVATARS.length];
}

function hslToHex(h: number, s: number, l: number): string {
  const a = (s / 100) * Math.min(l / 100, 1 - l / 100);
  const channel = (n: number) => {
    const k = (n + h / 30) % 12;
    const value = l / 100 - a * Math.max(-1, Math.min(k - 3, 9 - k, 1));
    return Math.round(value * 255).toString(16).padStart(2, "0");
  };
  return `#${channel(0)}${channel(8)}${channel(4)}`;
}

const HEX_COLOR = /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i;

/** Department color when the renderer accepts it (#rgb/#rrggbb), otherwise a stable hue from the id. */
export function botAvatarColor(color: string | null | undefined, id: string): string {
  const trimmed = color?.trim();
  return trimmed && HEX_COLOR.test(trimmed) ? trimmed : hslToHex(hash(id) % 360, 55, 55);
}

const stills = new Map<string, string>();

/** Cached still frame as a data URL; an <img> keeps each SVG's internal ids out of the page DOM. */
export function botAvatarStill(definition: AvatarDefinition, color: string): string {
  const key = `${definition.id}|${color}`;
  let url = stills.get(key);
  if (!url) {
    url = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(renderAvatarSvg(definition, { color }))}`;
    stills.set(key, url);
  }
  return url;
}
