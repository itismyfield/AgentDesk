import type { CliProvider } from "../../types";
import type { DeptForm, FormData } from "./types";

export const CLI_PROVIDERS: CliProvider[] = ["claude", "codex", "gemini", "opencode", "qwen", "copilot", "antigravity", "api"];

export const STATUS_DOT: Record<string, string> = {
  working: "bg-emerald-400 shadow-emerald-400/50 shadow-sm",
  break: "bg-amber-400",
  offline: "bg-red-400",
  archived: "bg-zinc-400",
  idle: "bg-slate-500",
};

export const ICON_SPRITE_POOL = Array.from({ length: 20 }, (_, i) => i + 1);

export const EMOJI_GROUPS: { label: string; labelEn: string; emojis: string[] }[] = [
  {
    label: "부서/업무",
    labelEn: "Work",
    emojis: ["📊", "💻", "🎨", "🔍", "🛡️", "⚙️", "📁", "🏢", "📋", "📈", "💼", "🗂️", "📌", "🎯", "🔧", "🧪"],
  },
  {
    label: "사람/표정",
    labelEn: "People",
    emojis: ["🤖", "👤", "👥", "😊", "😎", "🤓", "🧑‍💻", "👨‍🔬", "👩‍🎨", "🧑‍🏫", "🦸", "🦊", "🐱", "🐶", "🐻", "🐼"],
  },
  {
    label: "사물/기호",
    labelEn: "Objects",
    emojis: ["💡", "🚀", "⚡", "🔥", "💎", "🏆", "🎵", "🎮", "📱", "💾", "🖥️", "📡", "🔑", "🛠️", "📦", "🧩"],
  },
  {
    label: "자연/색상",
    labelEn: "Nature",
    emojis: ["🌟", "⭐", "🌈", "🌊", "🌸", "🍀", "🌙", "☀️", "❄️", "🔵", "🟢", "🟡", "🔴", "🟣", "🟠", "⚪"],
  },
];

export const BLANK: FormData = {
  name: "",
  name_ko: "",
  name_ja: "",
  name_zh: "",
  department_id: "",
  cli_provider: "claude",
  avatar_emoji: "🤖",
  sprite_number: null,
  personality: "",
  prompt_content: "",
  auto_commit: false,
};

export const DEPT_COLORS = [
  "#3b82f6",
  "#ef4444",
  "#f59e0b",
  "#10b981",
  "#14b8a6",
  "#f97316",
  "#ec4899",
  "#06b6d4",
  "#6b7280",
];

export const DEPT_BLANK: DeptForm = {
  id: "",
  name: "",
  name_ko: "",
  name_ja: "",
  name_zh: "",
  icon: "📁",
  color: "#3b82f6",
  description: "",
  prompt: "",
};
