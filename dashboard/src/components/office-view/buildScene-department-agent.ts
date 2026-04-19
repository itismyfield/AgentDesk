import type { MutableRefObject } from "react";
import { AnimatedSprite, Container, Graphics, Text, TextStyle, type Texture } from "pixi.js";
import { getAgentWarnings, getAgentWorkSummary } from "../../agent-insights";
import type { Agent, SubAgent, Task } from "../../types";
import { FONT_STACK_MONO, getFontFamilyForText } from "../../lib/fonts";
import type { ActiveIssueInfo, AnimItem, CallbackSnapshot, SubCloneAnimItem } from "./buildScene-types";
import {
  DESK_W,
  TARGET_CHAR_H,
  type SubCloneBurstParticle,
} from "./model";
import { hashStr } from "./drawing-core";
import { drawDesk } from "./drawing-furniture-a";

function formatElapsed(startMs: number): string {
  const ms = Date.now() - startMs;
  if (ms < 0) return "";
  const totalMin = Math.floor(ms / 60000);
  if (totalMin < 60) return `${totalMin}m`;
  const h = Math.floor(totalMin / 60);
  const m = totalMin % 60;
  if (h >= 24) {
    const d = Math.floor(h / 24);
    return `${d}d${h % 24}h`;
  }
  return m > 0 ? `${h}h${m}m` : `${h}h`;
}

interface RenderDeskAgentAndSubClonesParams {
  room: Container;
  textures: Record<string, Texture>;
  spriteMap: Map<string, number>;
  agent: Agent;
  tasks: Task[];
  subAgents: SubAgent[];
  ax: number;
  deskY: number;
  charFeetY: number;
  isWorking: boolean;
  isOffline: boolean;
  cbRef: MutableRefObject<CallbackSnapshot>;
  animItemsRef: MutableRefObject<AnimItem[]>;
  subCloneAnimItemsRef: MutableRefObject<SubCloneAnimItem[]>;
  subCloneBurstParticlesRef: MutableRefObject<SubCloneBurstParticle[]>;
  addedWorkingSubIds: Set<string>;
  nextSubSnapshot: Map<string, { parentAgentId: string; x: number; y: number }>;
  themeAccent: number;
  activeIssue?: ActiveIssueInfo;
  isBlocked?: boolean;
}

export function renderDeskAgentAndSubClones({
  room,
  textures,
  spriteMap,
  agent,
  tasks,
  subAgents,
  ax,
  deskY,
  charFeetY,
  isWorking,
  isOffline,
  cbRef,
  animItemsRef,
  subCloneAnimItemsRef,
  subCloneBurstParticlesRef,
  addedWorkingSubIds,
  nextSubSnapshot,
  themeAccent,
  activeIssue,
  isBlocked = false,
}: RenderDeskAgentAndSubClonesParams): void {
  const spriteNum = spriteMap.get(agent.id) ?? (hashStr(agent.id) % 13) + 1;
  const charContainer = new Container();
  charContainer.position.set(ax, charFeetY);
  charContainer.eventMode = "static";
  charContainer.cursor = "pointer";
  charContainer.on("pointerup", () => cbRef.current.onSelectAgent(agent));

  const frames: Texture[] = [];
  for (let frame = 1; frame <= 3; frame++) {
    const key = `${spriteNum}-D-${frame}`;
    if (textures[key]) frames.push(textures[key]);
  }

  if (frames.length > 0) {
    const animSprite = new AnimatedSprite(frames);
    animSprite.anchor.set(0.5, 1);
    const scale = TARGET_CHAR_H / animSprite.texture.height;
    animSprite.scale.set(scale);
    animSprite.gotoAndStop(0);
    if (isOffline) {
      animSprite.alpha = 0.3;
      animSprite.tint = 0x888899;
    }
    charContainer.addChild(animSprite);
  } else {
    const fallback = new Text({ text: agent.avatar_emoji || "🤖", style: new TextStyle({ fontSize: 24 }) });
    fallback.anchor.set(0.5, 1);
    charContainer.addChild(fallback);
  }
  room.addChild(charContainer);

  const deskG = drawDesk(room, ax - DESK_W / 2, deskY, isWorking);

  const bedW = TARGET_CHAR_H + 20;
  const bedH = 36;
  const bedX = ax - bedW / 2;
  const bedY = deskY;

  const bedG = new Graphics();
  bedG.roundRect(bedX, bedY, bedW, bedH, 4).fill(0x5c3d2e);
  bedG.roundRect(bedX + 1, bedY + 1, bedW - 2, bedH - 2, 3).fill(0x8b6347);
  bedG.roundRect(bedX + 3, bedY + 3, bedW - 6, bedH - 6, 2).fill(0xf0e6d3);
  bedG.roundRect(bedX - 2, bedY - 1, 6, bedH + 2, 3).fill(0x4a2e1a);
  bedG.ellipse(bedX + 16, bedY + bedH / 2, 9, 7).fill(0xfff8ee);
  bedG.ellipse(bedX + 16, bedY + bedH / 2, 9, 7).stroke({ width: 0.5, color: 0xd8d0c0 });
  bedG.ellipse(bedX + 16, bedY + bedH / 2, 5, 4).fill({ color: 0xf0e8d8, alpha: 0.6 });
  bedG.visible = false;
  room.addChild(bedG);

  const blanketG = new Graphics();
  const blanketX = bedX + bedW * 0.35;
  const blanketW = bedW * 0.62;
  blanketG.roundRect(blanketX, bedY + 2, blanketW, bedH - 4, 3).fill(0xc8d8be);
  blanketG.roundRect(blanketX, bedY + 2, blanketW, bedH - 4, 3).stroke({ width: 0.5, color: 0xa8b898 });
  blanketG
    .moveTo(blanketX + 2, bedY + bedH / 2)
    .lineTo(blanketX + blanketW - 4, bedY + bedH / 2)
    .stroke({ width: 0.4, color: 0xb0c0a0, alpha: 0.5 });
  blanketG.visible = false;
  room.addChild(blanketG);

  const particles = new Container();
  room.addChild(particles);
  animItemsRef.current.push({
    sprite: charContainer,
    status: agent.status,
    baseX: ax,
    baseY: charContainer.position.y,
    particles,
    agentId: agent.id,
    cliProvider: agent.cli_provider,
    deskG,
    bedG,
    blanketG,
    blocked: isBlocked,
  });

  if (isBlocked) {
    const haloG = new Graphics();
    haloG.circle(ax, charFeetY - TARGET_CHAR_H / 2, 22).fill({ color: 0xef4444, alpha: 0.08 });
    haloG.circle(ax, charFeetY - TARGET_CHAR_H / 2, 22).stroke({ width: 1.5, color: 0xef4444, alpha: 0.5 });
    room.addChild(haloG);
  }

  const activeTask = tasks.find((task) => task.assigned_agent_id === agent.id && task.status === "in_progress");
  const workingSubs = subAgents.filter((sub) => sub.parentAgentId === agent.id && sub.status === "working");
  const workSummary = getAgentWorkSummary(agent, {
    activeTaskTitle: activeTask?.title ?? null,
    subAgents: workingSubs,
  });

  if (isWorking && workSummary) {
    const txt = workSummary.length > 26 ? `${workSummary.slice(0, 26)}...` : workSummary;
    const bubbleLines: string[] = [];
    if (activeIssue) {
      bubbleLines.push(`🔧 #${activeIssue.number}`);
    }
    bubbleLines.push(`💬 ${txt}`);
    if (activeIssue?.startedAt) {
      bubbleLines.push(`⏱ ${formatElapsed(activeIssue.startedAt)}`);
    }
    if (workingSubs.length > 1) {
      bubbleLines.push(`+${workingSubs.length - 1} linked`);
    }
    const bubbleBody = bubbleLines.join("\n");
    const bubbleText = new Text({
      text: bubbleBody,
      style: new TextStyle({
        fontSize: 6.5,
        fill: 0x333333,
        fontFamily: getFontFamilyForText(bubbleBody, "pixel"),
        wordWrap: true,
        wordWrapWidth: 85,
      }),
    });
    bubbleText.anchor.set(0.5, 1);
    const bw = Math.min(bubbleText.width + 8, 100);
    const bh = bubbleText.height + 6;
    const bubbleTop = charFeetY - TARGET_CHAR_H - bh - 6;
    const bubbleG = new Graphics();
    bubbleG.roundRect(ax - bw / 2, bubbleTop, bw, bh, 4).fill(0xffffff);
    bubbleG.roundRect(ax - bw / 2, bubbleTop, bw, bh, 4).stroke({ width: 1.2, color: themeAccent, alpha: 0.4 });
    bubbleG
      .moveTo(ax - 3, bubbleTop + bh)
      .lineTo(ax, bubbleTop + bh + 4)
      .lineTo(ax + 3, bubbleTop + bh)
      .fill(0xffffff);
    room.addChild(bubbleG);
    bubbleText.position.set(ax, bubbleTop + bh - 3);
    room.addChild(bubbleText);

    // Make issue badge line clickable → open GitHub issue in new tab
    if (activeIssue) {
      const hitContainer = new Container();
      hitContainer.position.set(ax - bw / 2, bubbleTop);
      hitContainer.hitArea = { contains: (x: number, y: number) => x >= 0 && x <= bw && y >= 0 && y <= 12 };
      hitContainer.eventMode = "static";
      hitContainer.cursor = "pointer";
      const issueUrl = activeIssue.url;
      hitContainer.on("pointertap", () => { window.open(issueUrl, "_blank"); });
      room.addChild(hitContainer);
    }
  }

  const sceneWarnings = getAgentWarnings(agent, {
    activeTaskTitle: activeTask?.title ?? null,
    subAgents: workingSubs,
  });
  if (sceneWarnings.length > 0) {
    const warning = sceneWarnings[0];
    const badgeBg = new Graphics();
    const badgeX = ax + 18;
    const badgeY = charFeetY - TARGET_CHAR_H + 10;
    const badgeColor = warning.code === "missing_work_detail" ? 0xf59e0b : 0xef4444;
    badgeBg.circle(badgeX, badgeY, 6).fill({ color: badgeColor, alpha: 0.95 });
    badgeBg.circle(badgeX, badgeY, 6).stroke({ width: 1, color: 0xffffff, alpha: 0.5 });
    room.addChild(badgeBg);
    const badgeText = new Text({
      text: warning.code === "missing_work_detail" ? "?" : "!",
      style: new TextStyle({ fontSize: 8, fill: 0xffffff, fontWeight: "bold", fontFamily: FONT_STACK_MONO }),
    });
    badgeText.anchor.set(0.5, 0.5);
    badgeText.position.set(badgeX, badgeY);
    room.addChild(badgeText);
  }

  if (isWorking && workingSubs.length > 0) {
    const countBg = new Graphics();
    countBg.roundRect(ax + 16, deskY - 18, 20, 10, 2).fill({ color: 0x101722, alpha: 0.82 });
    room.addChild(countBg);
    const countTxt = new Text({
      text: `x${workingSubs.length}`,
      style: new TextStyle({ fontSize: 6.5, fill: 0xe2e8f8, fontWeight: "bold", fontFamily: FONT_STACK_MONO }),
    });
    countTxt.anchor.set(0.5, 0.5);
    countTxt.position.set(ax + 26, deskY - 13);
    room.addChild(countTxt);
  }

  if (isOffline) {
    const zzz = new Text({ text: "💤", style: new TextStyle({ fontSize: 12 }) });
    zzz.anchor.set(0.5, 0.5);
    zzz.position.set(ax + 20, charFeetY - TARGET_CHAR_H / 2);
    room.addChild(zzz);
  }
}
