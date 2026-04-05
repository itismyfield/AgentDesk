import { useMemo } from "react";
import type { CSSProperties } from "react";
import type { Agent } from "../types";

/** Map agent IDs to sprite numbers (stable order, same as OfficeView) */
export function buildSpriteMap(agents: Agent[]): Map<string, number> {
  const map = new Map<string, number>();
  // 1) sprite_number가 DB에 지정된 에이전트 우선
  for (const a of agents) {
    if (a.sprite_number != null && a.sprite_number > 0) map.set(a.id, a.sprite_number);
  }
  // 2) DORO fallback (sprite_number 미지정시)
  const doro = agents.find((a) => a.name === "DORO");
  if (doro && !map.has(doro.id)) map.set(doro.id, 13);
  // 3) 나머지: 자동 할당 (1-12 순환)
  const rest = [...agents].filter((a) => !map.has(a.id)).sort((a, b) => a.id.localeCompare(b.id));
  rest.forEach((a, i) => map.set(a.id, (i % 12) + 1));
  return map;
}

/** Hook: memoized sprite map from agents array */
export function useSpriteMap(agents: Agent[]): Map<string, number> {
  return useMemo(() => buildSpriteMap(agents), [agents]);
}

/** Get the sprite number for an agent by ID */
export function getSpriteNum(agents: Agent[], agentId: string): number | undefined {
  return buildSpriteMap(agents).get(agentId);
}

function hashIdToSprite(id: string): number {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) {
    hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  }
  return (hash % 12) + 1;
}

function resolveSpriteNum(agent: Agent | undefined, spriteMap: Map<string, number>): number | undefined {
  if (!agent) return undefined;
  if (agent.sprite_number != null && agent.sprite_number > 0) return agent.sprite_number;
  const mapped = spriteMap.get(agent.id);
  if (mapped != null && mapped > 0) return mapped;
  if (agent.name === "DORO") return 13;
  return hashIdToSprite(agent.id);
}

interface AgentAvatarProps {
  agent: Agent | undefined;
  agents?: Agent[];
  spriteMap?: Map<string, number>;
  size?: number;
  className?: string;
  rounded?: "full" | "xl" | "2xl";
  imageFit?: "cover" | "contain";
  imagePosition?: CSSProperties["objectPosition"];
}

/** Sprite-based avatar — pass either `agents` or `spriteMap` */
export default function AgentAvatar({
  agent,
  agents,
  spriteMap,
  size = 28,
  className = "",
  rounded = "full",
  imageFit = "cover",
  imagePosition = "center",
}: AgentAvatarProps) {
  const map = spriteMap ?? (agents ? buildSpriteMap(agents) : new Map());
  const spriteNum = resolveSpriteNum(agent, map);

  const roundedClass = rounded === "full" ? "rounded-full" : rounded === "xl" ? "rounded-xl" : "rounded-2xl";

  if (spriteNum) {
    return (
      <div
        className={`${roundedClass} overflow-hidden bg-th-bg-surface flex-shrink-0 ${className}`}
        style={{ width: size, height: size }}
      >
        <img
          src={`/sprites/${spriteNum}-D-1.png`}
          alt={agent?.name ?? ""}
          className={`w-full h-full ${imageFit === "contain" ? "object-contain" : "object-cover"}`}
          style={{ imageRendering: "pixelated", objectPosition: imagePosition }}
        />
      </div>
    );
  }
  return (
    <div
      className={`${roundedClass} bg-th-bg-surface flex items-center justify-center flex-shrink-0 ${className}`}
      style={{ width: size, height: size, fontSize: size * 0.6 }}
    >
      {agent?.avatar_emoji ?? "🤖"}
    </div>
  );
}
