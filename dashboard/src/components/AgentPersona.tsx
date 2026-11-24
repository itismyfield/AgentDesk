import { useEffect, useRef, useState } from "react";
import type { Agent } from "../types";
import AnimatedBotAvatar from "./agent-avatar/AnimatedBotAvatar";
import { avatarRoster, botAvatarBlinking, botAvatarFor, botAvatarStill } from "./agent-avatar/botAvatar";
import { useWorkspaceAgents } from "../contexts/WorkspaceContext";

export type PersonaState = "idle" | "working" | "waiting" | "error" | "offline";

type PersonaAgent = Pick<Agent, "id" | "name"> &
  Partial<Pick<Agent, "name_ko" | "alias" | "status">>;

interface AgentPersonaProps {
  agent: PersonaAgent | undefined;
  /** Overrides the state derived from `agent.status` (e.g. waiting for review). */
  state?: PersonaState;
  size?: number;
  /**
   * `true` forces the live 3D scene (profile headers); `false` forces a static still.
   * When omitted, hybrid mode runs: `working` and pointer-hovered avatars mount the live
   * 3D scene, resting avatars blink via zero-JS SVG CSS, and `offline` stays still.
   */
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
export default function AgentPersona({ agent, state, size = 32, animated, className = "" }: AgentPersonaProps) {
  const rootRef = useRef<HTMLSpanElement>(null);
  const [hovered, setHovered] = useState(false);
  const resolvedState = state ?? personaStateFromAgent(agent?.status);
  const agentKey = agent?.id ?? "agent";
  const displayName = agent?.name_ko || agent?.alias || agent?.name || "Agent";
  const roster = useWorkspaceAgents();
  const avatar = botAvatarFor(agentKey, roster ? avatarRoster(roster) : undefined);
  const tone = avatar.color;
  const face = Math.round(size * 0.8);
  const dotSize = Math.max(6, Math.round(size * 0.2));
  const label = `${displayName} · ${STATE_LABEL_KO[resolvedState]}`;

  useEffect(() => {
    if (animated !== undefined || resolvedState === "offline") return;
    const el = rootRef.current;
    if (!el) return;
    const target = el.closest<HTMLElement>("button, a, [role='button'], [role='option']") ?? el;
    const onEnter = (event: PointerEvent) => {
      if (event.pointerType === "touch" || window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
      setHovered(true);
    };
    const onLeave = () => setHovered(false);
    target.addEventListener("pointerenter", onEnter);
    target.addEventListener("pointerleave", onLeave);
    return () => {
      setHovered(false);
      target.removeEventListener("pointerenter", onEnter);
      target.removeEventListener("pointerleave", onLeave);
    };
  }, [animated, resolvedState]);

  const isLive = animated ?? (resolvedState !== "offline" && (resolvedState === "working" || hovered));
  const isStill = !isLive && (resolvedState === "offline" || animated === false);
  const motionMode = isLive ? "live" : isStill ? "still" : "blink";

  return (
    <span
      ref={rootRef}
      role="img"
      aria-label={label}
      title={label}
      data-persona-state={resolvedState}
      data-persona-motion={motionMode}
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
        {isLive ? (
          <AnimatedBotAvatar
            avatar={avatar}
            mood={resolvedState === "working" ? "thinking" : "idle"}
            size={face}
            followPointer={hovered || animated === true}
          />
        ) : (
          <img
            src={isStill ? botAvatarStill(avatar) : botAvatarBlinking(avatar, agentKey)}
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
