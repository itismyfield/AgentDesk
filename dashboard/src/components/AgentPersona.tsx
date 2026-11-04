import type { Agent } from "../types";
import AnimatedBotAvatar from "./agent-avatar/AnimatedBotAvatar";
import { avatarRoster, botAvatarFor, botAvatarStill } from "./agent-avatar/botAvatar";
import { useWorkspaceAgents } from "../contexts/WorkspaceContext";

export type PersonaState = "idle" | "working" | "waiting" | "error" | "offline";

type PersonaAgent = Pick<Agent, "id" | "name"> &
  Partial<Pick<Agent, "name_ko" | "alias" | "status">>;

interface AgentPersonaProps {
  agent: PersonaAgent | undefined;
  /** Overrides the state derived from `agent.status` (e.g. waiting for review). */
  state?: PersonaState;
  size?: number;
  /** Live motion for a single prominent avatar (profile headers); lists stay still. */
  animated?: boolean;
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

/** Circle persona: the agent's bot avatar on a tint of its color, with a status dot. */
export default function AgentPersona({ agent, state, size = 32, animated = false, className = "" }: AgentPersonaProps) {
  const resolvedState = state ?? personaStateFromAgent(agent?.status);
  const displayName = agent?.name_ko || agent?.alias || agent?.name || "Agent";
  const roster = useWorkspaceAgents();
  const avatar = botAvatarFor(agent?.id ?? "agent", roster ? avatarRoster(roster) : undefined);
  const tone = avatar.color;
  const face = Math.round(size * 0.8);
  const dotSize = Math.max(6, Math.round(size * 0.2));
  const label = `${displayName} · ${STATE_LABEL_KO[resolvedState]}`;

  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      data-persona-state={resolvedState}
      data-bot-avatar={avatar.definition.id}
      className={`relative inline-flex shrink-0 items-center justify-center rounded-full border ${className}`.trim()}
      style={{
        width: size,
        height: size,
        background: `color-mix(in srgb, ${tone} 12%, var(--th-bg-surface))`,
        borderColor: `color-mix(in srgb, ${tone} 35%, var(--th-border-subtle))`,
      }}
    >
      <span aria-hidden className={resolvedState === "offline" ? "opacity-60 grayscale" : undefined}>
        {animated ? (
          <AnimatedBotAvatar
            avatar={avatar}
            mood={resolvedState === "working" ? "thinking" : "idle"}
            size={face}
          />
        ) : (
          <img
            src={botAvatarStill(avatar)}
            alt=""
            width={face}
            height={face}
            draggable={false}
            className="pointer-events-none block select-none"
          />
        )}
      </span>
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
