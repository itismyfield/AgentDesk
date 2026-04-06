import type { MutableRefObject } from "react";
import { Container, Graphics, Text, TextStyle, type AnimatedSprite, type Sprite } from "pixi.js";
import type { MeetingPresence } from "../../types";
import {
  type Delivery,
  type RoomRect,
  type SubCloneBurstParticle,
  type WallClockVisual,
  SUB_CLONE_FIREWORK_INTERVAL,
  SUB_CLONE_FLOAT_DRIFT,
  SUB_CLONE_MOVE_X_AMPLITUDE,
  SUB_CLONE_MOVE_Y_AMPLITUDE,
  SUB_CLONE_WAVE_SPEED,
  TARGET_CHAR_H,
  destroyNode,
  emitSubCloneFireworkBurst,
} from "./model";
import { applyWallClockTime, blendColor } from "./drawing-core";
import { DEPT_THEME, DEFAULT_BREAK_THEME, DEFAULT_CEO_THEME } from "./themes-locale";
import { updateBreakRoomAndDeliveryAnimations } from "./officeTickerRoomAndDelivery";

interface AgentAnimItem {
  sprite: Container;
  status: string;
  baseX: number;
  baseY: number;
  particles: Container;
  agentId?: string;
  cliProvider?: string;
  deskG?: Graphics;
  bedG?: Graphics;
  blanketG?: Graphics;
  blocked?: boolean;
}

interface SubCloneAnimItem {
  container: Container;
  aura: Graphics;
  cloneVisual: Sprite;
  animated?: AnimatedSprite;
  frameCount: number;
  baseScale: number;
  baseX: number;
  baseY: number;
  phase: number;
  fireworkOffset: number;
}

interface BreakAnimItem {
  sprite: Container;
  baseX: number;
  baseY: number;
}

interface OfficeTickerData {
  customDeptThemes?: Record<string, { floor1: number; floor2: number; wall: number; accent: number }>;
  meetingPresence?: MeetingPresence[];
}

function hasTint(value: unknown): value is { tint: number } {
  return typeof value === "object" && value !== null && Reflect.has(value, "tint") && typeof Reflect.get(value, "tint") === "number";
}

export interface OfficeTickerContext {
  tickRef: MutableRefObject<number>;
  keysRef: MutableRefObject<Record<string, boolean>>;
  ceoPosRef: MutableRefObject<{ x: number; y: number }>;
  ceoSpriteRef: MutableRefObject<Container | null>;
  crownRef: MutableRefObject<Text | null>;
  highlightRef: MutableRefObject<Graphics | null>;
  animItemsRef: MutableRefObject<AgentAnimItem[]>;
  cliUsageRef: MutableRefObject<Record<string, { windows?: Array<{ utilization: number }> }> | null>;
  roomRectsRef: MutableRefObject<RoomRect[]>;
  deliveriesRef: MutableRefObject<Delivery[]>;
  breakAnimItemsRef: MutableRefObject<BreakAnimItem[]>;
  subCloneAnimItemsRef: MutableRefObject<SubCloneAnimItem[]>;
  subCloneBurstParticlesRef: MutableRefObject<SubCloneBurstParticle[]>;
  breakSteamParticlesRef: MutableRefObject<Container | null>;
  breakBubblesRef: MutableRefObject<Container[]>;
  wallClocksRef: MutableRefObject<WallClockVisual[]>;
  wallClockSecondRef: MutableRefObject<number>;
  themeHighlightTargetIdRef: MutableRefObject<string | null>;
  ceoOfficeRectRef: MutableRefObject<{ x: number; y: number; w: number; h: number } | null>;
  breakRoomRectRef: MutableRefObject<{ x: number; y: number; w: number; h: number } | null>;
  officeWRef: MutableRefObject<number>;
  totalHRef: MutableRefObject<number>;
  dataRef: MutableRefObject<OfficeTickerData>;
  eventBubbleQueueRef: MutableRefObject<Array<{ agentId: string; text: string; emoji: string; createdAt: number }>>;
  eventBubblesRef: MutableRefObject<Array<{ container: Container; createdAt: number; duration: number; baseY: number }>>;
  deliveryLayerRef: MutableRefObject<Container | null>;
  agentPosRef: MutableRefObject<Map<string, { x: number; y: number }>>;
  followCeoInView: () => void;
}

export function runOfficeTickerStep(ctx: OfficeTickerContext): void {
  const tick = ++ctx.tickRef.current;
  const wallClockNow = new Date();
  const wallClockSecond = wallClockNow.getHours() * 3600 + wallClockNow.getMinutes() * 60 + wallClockNow.getSeconds();

  if (ctx.wallClockSecondRef.current !== wallClockSecond) {
    ctx.wallClockSecondRef.current = wallClockSecond;
    for (const clock of ctx.wallClocksRef.current) applyWallClockTime(clock, wallClockNow);
  }

  const highlight = ctx.highlightRef.current;
  if (highlight) {
    highlight.clear();
    const activeThemeTargetId = ctx.themeHighlightTargetIdRef.current;

    if (activeThemeTargetId) {
      const pulse = 0.55 + Math.sin(tick * 0.08) * 0.2;
      let targetRect: { x: number; y: number; w: number; h: number } | null = null;
      let targetAccent = DEPT_THEME.dev.accent;

      if (activeThemeTargetId === "ceoOffice") {
        targetRect = ctx.ceoOfficeRectRef.current;
        targetAccent = ctx.dataRef.current.customDeptThemes?.ceoOffice?.accent ?? DEFAULT_CEO_THEME.accent;
      } else if (activeThemeTargetId === "breakRoom") {
        targetRect = ctx.breakRoomRectRef.current;
        targetAccent = ctx.dataRef.current.customDeptThemes?.breakRoom?.accent ?? DEFAULT_BREAK_THEME.accent;
      } else {
        const targetRoom = ctx.roomRectsRef.current.find((roomRect) => roomRect.dept.id === activeThemeTargetId);
        if (targetRoom) {
          targetRect = { x: targetRoom.x, y: targetRoom.y, w: targetRoom.w, h: targetRoom.h };
          const targetTheme =
            ctx.dataRef.current.customDeptThemes?.[activeThemeTargetId] ||
            DEPT_THEME[activeThemeTargetId] ||
            DEPT_THEME.dev;
          targetAccent = targetTheme.accent;
        }
      }

      if (targetRect) {
        highlight.roundRect(targetRect.x - 4, targetRect.y - 4, targetRect.w + 8, targetRect.h + 8, 7).stroke({
          width: 3.5,
          color: targetAccent,
          alpha: pulse,
        });
        highlight.roundRect(targetRect.x - 6, targetRect.y - 6, targetRect.w + 12, targetRect.h + 12, 9).stroke({
          width: 1.2,
          color: blendColor(targetAccent, 0xffffff, 0.22),
          alpha: 0.35 + Math.sin(tick * 0.06) * 0.08,
        });
      }
    }
  }

  for (const { sprite, status, baseX, baseY, particles, agentId, cliProvider, deskG, bedG, blanketG, blocked } of ctx
    .animItemsRef.current) {
    if (agentId) {
      const meetingNow = Date.now();
      const inMeetingPresence = (ctx.dataRef.current.meetingPresence ?? []).some((row) => {
        return row.agent_id === agentId && row.until >= meetingNow;
      });
      const inMeeting =
        inMeetingPresence || ctx.deliveriesRef.current.some((d) => d.agentId === agentId && d.holdAtSeat && d.arrived);
      sprite.visible = !inMeeting;
      if (inMeeting) continue;
    }

    sprite.position.x = baseX;
    sprite.position.y = baseY;

    if (status === "working") {
      // Stronger working FX (2~3x): more frequent spawn, bigger particles, longer life.
      if (tick % 4 === 0) {
        const particle = new Graphics();
        const colors = [0x55aaff, 0x55ff88, 0xffaa33, 0xff5577, 0xaa77ff, 0x7dd3fc, 0x34d399];
        particle.star(0, 0, 5, 2.8, 1.3, 0).fill(colors[Math.floor(Math.random() * colors.length)]);
        particle.position.set(baseX + (Math.random() - 0.5) * 30, baseY - 18 - Math.random() * 11);
        (particle as any)._vy = -0.55 - Math.random() * 0.4;
        (particle as any)._life = 0;
        particles.addChild(particle);
      }

      for (let i = particles.children.length - 1; i >= 0; i--) {
        const particle = particles.children[i] as any;
        if (particle._sweat) continue;
        particle._life++;
        particle.position.y += particle._vy ?? -0.45;
        particle.position.x += Math.sin(particle._life * 0.24) * 0.35;
        particle.alpha = Math.max(0, 1 - particle._life * 0.02);
        particle.scale.set(Math.max(0.16, 1.2 - particle._life * 0.018));
        if (particle._life > 58) {
          particles.removeChild(particle);
          particle.destroy();
        }
      }
    }

    if (cliProvider) {
      const usage = ctx.cliUsageRef.current?.[cliProvider];
      const maxUtil = usage?.windows?.reduce((max, window) => Math.max(max, window.utilization), 0) ?? 0;
      const isOfflineAgent = status === "offline";

      if (maxUtil >= 1.0) {
        const bedCenterX = baseX;
        const bedCenterY = baseY - 8 + 18;
        const headX = bedCenterX - TARGET_CHAR_H / 2 + 6;
        sprite.rotation = -Math.PI / 2;
        sprite.position.set(headX + TARGET_CHAR_H - 6, bedCenterY);
        sprite.alpha = 0.85;
        const child0 = sprite.children[0];
        if (hasTint(child0)) child0.tint = 0xff6666;
        if (deskG) deskG.visible = false;

        if (bedG) {
          bedG.visible = true;
          const room = sprite.parent;
          if (room) {
            room.removeChild(sprite);
            const bedIndex = room.children.indexOf(bedG);
            room.addChildAt(sprite, bedIndex + 1);
          }
        }

        if (blanketG) {
          blanketG.visible = true;
          const room = sprite.parent;
          if (room) {
            room.removeChild(blanketG);
            const spriteIndex = room.children.indexOf(sprite);
            room.addChildAt(blanketG, spriteIndex + 1);
          }
        }

        if (tick % 40 === 0) {
          const star = new Graphics();
          star.star(0, 0, 5, 3, 1.5, 0).fill({ color: 0xffdd44, alpha: 0.8 });
          star.position.set(headX, bedCenterY - 22);
          (star as any)._sweat = true;
          (star as any)._dizzy = true;
          (star as any)._offset = Math.random() * Math.PI * 2;
          (star as any)._life = 0;
          particles.addChild(star);
        }

        if (tick % 80 === 0) {
          const sleepy = new Text({
            text: "z",
            style: new TextStyle({ fontSize: 7 + Math.random() * 3, fill: 0xaaaacc, fontFamily: "monospace" }),
          });
          sleepy.anchor.set(0.5, 0.5);
          sleepy.position.set(headX + 6, bedCenterY - 18);
          (sleepy as any)._sweat = true;
          (sleepy as any)._life = 0;
          particles.addChild(sleepy);
        }
      } else if (maxUtil >= 0.8) {
        sprite.rotation = 0;
        sprite.alpha = 1;
        const child0 = sprite.children[0];
        if (hasTint(child0)) child0.tint = 0xff9999;
        if (deskG) deskG.visible = true;
        if (bedG) bedG.visible = false;
        if (blanketG) blanketG.visible = false;

        if (tick % 40 === 0) {
          const drop = new Graphics();
          drop
            .moveTo(0, 0)
            .lineTo(-1.8, 4)
            .quadraticCurveTo(0, 6.5, 1.8, 4)
            .lineTo(0, 0)
            .fill({ color: 0x7ec8e3, alpha: 0.85 });
          drop.circle(0, 3.8, 1.2).fill({ color: 0xbde4f4, alpha: 0.5 });
          drop.position.set(baseX + 8, baseY - 36);
          (drop as any)._sweat = true;
          (drop as any)._life = 0;
          particles.addChild(drop);
        }
      } else if (maxUtil >= 0.6) {
        sprite.rotation = 0;
        sprite.alpha = 1;
        const child0 = sprite.children[0];
        if (hasTint(child0)) child0.tint = 0xffffff;
        if (deskG) deskG.visible = true;
        if (bedG) bedG.visible = false;
        if (blanketG) blanketG.visible = false;

        if (tick % 55 === 0) {
          const drop = new Graphics();
          drop
            .moveTo(0, 0)
            .lineTo(-1.8, 4)
            .quadraticCurveTo(0, 6.5, 1.8, 4)
            .lineTo(0, 0)
            .fill({ color: 0x7ec8e3, alpha: 0.85 });
          drop.circle(0, 3.8, 1.2).fill({ color: 0xbde4f4, alpha: 0.5 });
          drop.position.set(baseX + 8, baseY - 36);
          (drop as any)._sweat = true;
          (drop as any)._life = 0;
          particles.addChild(drop);
        }
      } else if (blocked) {
        sprite.rotation = 0;
        sprite.alpha = 0.45 + Math.sin(tick * 0.06) * 0.1;
        const child0 = sprite.children[0];
        if (hasTint(child0)) child0.tint = 0xffaaaa;
        if (deskG) deskG.visible = true;
        if (bedG) bedG.visible = false;
        if (blanketG) blanketG.visible = false;
      } else {
        sprite.rotation = 0;
        sprite.alpha = isOfflineAgent ? 0.3 : 1;
        const child0 = sprite.children[0];
        if (hasTint(child0)) child0.tint = isOfflineAgent ? 0x888899 : 0xffffff;
        if (deskG) deskG.visible = true;
        if (bedG) bedG.visible = false;
        if (blanketG) blanketG.visible = false;
      }

      for (let i = particles.children.length - 1; i >= 0; i--) {
        const particle = particles.children[i] as any;
        if (!particle._sweat) continue;
        particle._life++;

        if (particle._dizzy) {
          const headPX = baseX - TARGET_CHAR_H / 2 + 10;
          const bedCenterY = baseY - 8 + 18;
          const angle = tick * 0.08 + particle._offset;
          particle.position.x = headPX + Math.cos(angle) * 14;
          particle.position.y = bedCenterY - 22 + Math.sin(angle * 0.7) * 4;
          particle.alpha = 0.7 + Math.sin(tick * 0.1) * 0.3;
        } else {
          particle.position.y += 0.45;
          particle.position.x += Math.sin(particle._life * 0.15) * 0.15;
          particle.alpha = Math.max(0, 0.85 - particle._life * 0.022);
        }

        if (particle._life > 38) {
          particles.removeChild(particle);
          particle.destroy();
        }
      }
    }
  }

  for (const clone of ctx.subCloneAnimItemsRef.current) {
    const wave = tick * SUB_CLONE_WAVE_SPEED + clone.phase;
    const driftX =
      Math.sin(wave * 0.7) * SUB_CLONE_MOVE_X_AMPLITUDE +
      Math.cos(wave * 0.38 + clone.phase * 0.6) * SUB_CLONE_FLOAT_DRIFT;
    const driftY =
      Math.sin(wave * 0.95) * SUB_CLONE_MOVE_Y_AMPLITUDE +
      Math.cos(wave * 0.52 + clone.phase) * (SUB_CLONE_FLOAT_DRIFT * 0.65);
    clone.container.position.x = clone.baseX + driftX;
    clone.container.position.y = clone.baseY + driftY;
    clone.aura.alpha = 0.1 + (Math.sin(wave * 0.9) + 1) * 0.06;
    clone.cloneVisual.alpha = 0.9 + Math.max(0, Math.sin(wave * 1.9)) * 0.08;
    clone.cloneVisual.rotation = Math.sin(wave * 1.45 + clone.phase) * 0.045;
    const scalePulse = clone.baseScale * (1 + Math.sin(wave * 1.7) * 0.01);
    clone.cloneVisual.scale.set(scalePulse);

    if (clone.animated && clone.frameCount > 1) {
      const frameFloat = (Math.sin(wave * 2.8) + 1) * 0.5 * clone.frameCount;
      const frame = Math.min(clone.frameCount - 1, Math.floor(frameFloat));
      clone.animated.gotoAndStop(frame);
    }

    if ((tick + clone.fireworkOffset) % SUB_CLONE_FIREWORK_INTERVAL === 0) {
      const room = clone.container.parent as Container | null;
      if (room) {
        emitSubCloneFireworkBurst(
          room,
          ctx.subCloneBurstParticlesRef.current,
          clone.container.position.x,
          clone.container.position.y - 24,
        );
      }
    }
  }

  const burstParticles = ctx.subCloneBurstParticlesRef.current;
  for (let i = burstParticles.length - 1; i >= 0; i--) {
    const particle = burstParticles[i];
    particle.life += 1;
    particle.node.position.x += particle.vx;
    particle.node.position.y += particle.vy;
    particle.node.rotation += particle.spin;
    particle.node.scale.set(particle.node.scale.x + particle.growth, particle.node.scale.y + particle.growth);
    particle.node.alpha = Math.max(0, 1 - particle.life / particle.maxLife);

    if (particle.life >= particle.maxLife || particle.node.destroyed) {
      destroyNode(particle.node);
      burstParticles.splice(i, 1);
    }
  }

  updateBreakRoomAndDeliveryAnimations(
    {
      breakAnimItemsRef: ctx.breakAnimItemsRef,
      breakSteamParticlesRef: ctx.breakSteamParticlesRef,
      breakRoomRectRef: ctx.breakRoomRectRef,
      breakBubblesRef: ctx.breakBubblesRef,
      deliveriesRef: ctx.deliveriesRef,
    },
    tick,
  );

  const layer = ctx.deliveryLayerRef.current;
  if (layer) {
    const queue = ctx.eventBubbleQueueRef.current;
    while (queue.length > 0) {
      const item = queue.shift()!;
      if (Date.now() - item.createdAt > 5000) continue;
      const pos = ctx.agentPosRef.current.get(item.agentId);
      if (!pos) continue;
      const bubble = new Container();
      const label = `${item.emoji} ${item.text}`;
      const bubbleText = new Text({
        text: label,
        style: new TextStyle({ fontSize: 7, fill: 0xffffff, fontFamily: "system-ui, sans-serif" }),
      });
      bubbleText.anchor.set(0.5, 0.5);
      const bubbleWidth = bubbleText.width + 10;
      const bubbleHeight = bubbleText.height + 6;
      const bubbleBg = new Graphics();
      bubbleBg.roundRect(-bubbleWidth / 2, -bubbleHeight / 2, bubbleWidth, bubbleHeight, 4).fill({ color: 0x1e293b, alpha: 0.92 });
      bubbleBg.roundRect(-bubbleWidth / 2, -bubbleHeight / 2, bubbleWidth, bubbleHeight, 4).stroke({
        width: 0.8,
        color: 0x6366f1,
        alpha: 0.5,
      });
      bubble.addChild(bubbleBg);
      bubble.addChild(bubbleText);
      bubble.position.set(pos.x, pos.y - TARGET_CHAR_H - 35);
      layer.addChild(bubble);
      ctx.eventBubblesRef.current.push({
        container: bubble,
        createdAt: item.createdAt,
        duration: 3500,
        baseY: pos.y - TARGET_CHAR_H - 35,
      });
    }

    const now = Date.now();
    const bubbles = ctx.eventBubblesRef.current;
    for (let i = bubbles.length - 1; i >= 0; i--) {
      const bubble = bubbles[i];
      if (bubble.container.destroyed || !bubble.container.parent) {
        bubbles.splice(i, 1);
        continue;
      }
      const age = now - bubble.createdAt;
      if (age >= bubble.duration) {
        bubble.container.destroy();
        bubbles.splice(i, 1);
        continue;
      }
      const progress = age / bubble.duration;
      bubble.container.alpha = progress < 0.1 ? progress / 0.1 : Math.max(0, 1 - (progress - 0.1) * 1.1);
      bubble.container.position.y = bubble.baseY - progress * 18;
    }
  }
}
