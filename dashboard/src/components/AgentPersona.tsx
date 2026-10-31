import type { Agent } from "../types";

export type PersonaState = "idle" | "working" | "waiting" | "error" | "offline";

type PersonaAgent = Pick<Agent, "id" | "name"> &
  Partial<Pick<Agent, "name_ko" | "alias" | "avatar_emoji" | "department_color" | "status">>;

interface AgentPersonaProps {
  agent: PersonaAgent | undefined;
  /** Overrides the state derived from `agent.status` (e.g. waiting for review). */
  state?: PersonaState;
  size?: number;
  className?: string;
}

const STATE_DOT: Record<PersonaState, string> = {
  idle: "bg-[var(--th-text-muted)]",
  working: "bg-[var(--th-accent-success)]",
  waiting: "bg-[var(--th-accent-warn)] animate-persona-breathe motion-reduce:animate-none",
  error: "bg-[var(--th-accent-danger)]",
  offline: "border border-[var(--th-text-muted)] bg-[var(--th-bg-surface)]",
};

const STATE_LABEL_KO: Record<PersonaState, string> = {
  idle: "대기",
  working: "작업 중",
  waiting: "확인 필요",
  error: "오류",
  offline: "오프라인",
};

export function personaStateFromAgent(status: Agent["status"] | undefined): PersonaState {
  if (status === "working") return "working";
  if (status === "offline") return "offline";
  return "idle";
}

function initialsOf(name: string): string {
  const trimmed = name.trim();
  if (!trimmed) return "?";
  const words = trimmed.split(/[\s_-]+/).filter(Boolean);
  if (words.length > 1 && /^[A-Za-z]/.test(words[0])) {
    return (words[0][0] + words[1][0]).toUpperCase();
  }
  return Array.from(trimmed).slice(0, /^[A-Za-z]/.test(trimmed) ? 2 : 1).join("").toUpperCase();
}

// Stable fallback tone so agents without a department color stay distinguishable.
function fallbackColor(id: string): string {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  return `hsl(${hash % 360} 55% 55%)`;
}

/** Circle persona: department-tinted initials or emoji with a status dot. */
export default function AgentPersona({ agent, state, size = 32, className = "" }: AgentPersonaProps) {
  const resolvedState = state ?? personaStateFromAgent(agent?.status);
  const displayName = agent?.name_ko || agent?.alias || agent?.name || "Agent";
  const tone = agent?.department_color || (agent ? fallbackColor(agent.id) : "var(--th-text-muted)");
  const emoji = agent?.avatar_emoji?.trim();
  const dotSize = Math.max(6, Math.round(size * 0.2));
  const label = `${displayName} · ${STATE_LABEL_KO[resolvedState]}`;

  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      data-persona-state={resolvedState}
      className={`relative inline-flex shrink-0 items-center justify-center rounded-full border font-semibold ${className}`.trim()}
      style={{
        width: size,
        height: size,
        fontSize: Math.round(size * (emoji ? 0.5 : 0.38)),
        background: `color-mix(in srgb, ${tone} 12%, var(--th-bg-surface))`,
        borderColor: `color-mix(in srgb, ${tone} 35%, var(--th-border-subtle))`,
        color: `color-mix(in srgb, ${tone} 70%, var(--th-text-heading))`,
      }}
    >
      <span aria-hidden className="leading-none">{emoji || initialsOf(displayName)}</span>
      {resolvedState === "working" ? (
        <span
          aria-hidden
          className="pointer-events-none absolute -inset-px animate-[spin_1.6s_linear_infinite] rounded-full border-2 border-transparent border-t-[var(--th-accent-success)] motion-reduce:animate-none"
        />
      ) : null}
      <span
        aria-hidden
        className={`absolute right-0 bottom-0 rounded-full ring-2 ring-[var(--th-bg-surface)] ${STATE_DOT[resolvedState]}`}
        style={{ width: dotSize, height: dotSize }}
      />
    </span>
  );
}
