import type { MutableRefObject } from "react";
import { Container, Graphics, Text, TextStyle, type Application, type Texture } from "pixi.js";
import type { Agent, Department, SubAgent, Task } from "../../types";
import { localeName } from "../../i18n";
import { FONT_STACK_MONO, getFontFamilyForText } from "../../lib/fonts";
import type { CallbackSnapshot, AnimItem, SubCloneAnimItem } from "./buildScene-types";
import {
  COLS_PER_ROW,
  DESK_W,
  ROOM_PAD,
  SLOT_H,
  SLOT_W,
  TARGET_CHAR_H,
  type RoomRect,
  type SubCloneBurstParticle,
  type WallClockVisual,
  emitSubCloneSmokeBurst,
} from "./model";
import { DEPT_THEME, LOCALE_TEXT, type SupportedLocale, pickLocale } from "./themes-locale";
import {
  blendColor,
  contrastTextColor,
  drawAmbientGlow,
  drawBunting,
  drawCeilingLight,
  drawPictureFrame,
  drawRug,
  drawRoomAtmosphere,
  drawTiledFloor,
  drawTrashCan,
  drawWallClock,
  drawWindow,
} from "./drawing-core";
import { drawChair, drawDesk, drawPlant, drawWhiteboard } from "./drawing-furniture-a";
import { drawBookshelf } from "./drawing-furniture-b";
import { renderDeskAgentAndSubClones } from "./buildScene-department-agent";
import type { ActiveIssueInfo } from "./buildScene-types";

export interface DeptLayout {
  rx: number;
  ry: number;
  rw: number;
  rh: number;
  deptAgentRows: number;
}

interface BuildDepartmentRoomsParams {
  app: Application;
  textures: Record<string, Texture>;
  departments: Department[];
  agents: Agent[];
  tasks: Task[];
  subAgents: SubAgent[];
  unread?: Set<string>;
  customThemes?: Record<string, { floor1: number; floor2: number; wall: number; accent: number }>;
  activeLocale: SupportedLocale;
  deptLayouts: DeptLayout[];
  spriteMap: Map<string, number>;
  cbRef: MutableRefObject<CallbackSnapshot>;
  roomRectsRef: MutableRefObject<RoomRect[]>;
  agentPosRef: MutableRefObject<Map<string, { x: number; y: number }>>;
  animItemsRef: MutableRefObject<AnimItem[]>;
  subCloneAnimItemsRef: MutableRefObject<SubCloneAnimItem[]>;
  subCloneBurstParticlesRef: MutableRefObject<SubCloneBurstParticle[]>;
  wallClocksRef: MutableRefObject<WallClockVisual[]>;
  removedSubBurstsByParent: Map<string, Array<{ x: number; y: number }>>;
  addedWorkingSubIds: Set<string>;
  nextSubSnapshot: Map<string, { parentAgentId: string; x: number; y: number }>;
  activeIssueByAgent?: Map<string, ActiveIssueInfo>;
  blockedAgentIds?: Set<string>;
}

export function buildDepartmentRooms({
  app,
  textures,
  departments,
  agents,
  tasks,
  subAgents,
  unread,
  customThemes,
  activeLocale,
  deptLayouts,
  spriteMap,
  cbRef,
  roomRectsRef,
  agentPosRef,
  animItemsRef,
  subCloneAnimItemsRef,
  subCloneBurstParticlesRef,
  wallClocksRef,
  removedSubBurstsByParent,
  addedWorkingSubIds,
  nextSubSnapshot,
  activeIssueByAgent,
  blockedAgentIds,
}: BuildDepartmentRoomsParams): void {
  departments.forEach((dept, deptIdx) => {
    const { rx, ry, rw: roomW, rh: roomH, deptAgentRows } = deptLayouts[deptIdx];
    const theme = customThemes?.[dept.id] || DEPT_THEME[dept.id] || DEPT_THEME.dev;
    const deptAgents = agents.filter((agent) => agent.department_id === dept.id);
    if (deptAgents.length === 0) return;
    roomRectsRef.current.push({ dept, x: rx, y: ry, w: roomW, h: roomH });

    const room = new Container();

    const floorG = new Graphics();
    drawTiledFloor(floorG, rx, ry, roomW, roomH, theme.floor1, theme.floor2);
    room.addChild(floorG);
    drawRoomAtmosphere(room, rx, ry, roomW, roomH, theme.wall, theme.accent);

    const wallG = new Graphics();
    wallG.roundRect(rx, ry, roomW, roomH, 3).stroke({ width: 2.5, color: theme.wall });
    room.addChild(wallG);

    const doorG = new Graphics();
    doorG.rect(rx + roomW / 2 - 16, ry - 2, 32, 5).fill(0xf5f0e8);
    room.addChild(doorG);

    const signW = 84;
    const signBg = new Graphics();
    signBg.roundRect(rx + roomW / 2 - signW / 2 + 1, ry - 3, signW, 18, 4).fill({ color: 0x000000, alpha: 0.12 });
    signBg.roundRect(rx + roomW / 2 - signW / 2, ry - 4, signW, 18, 4).fill(theme.accent);
    signBg.eventMode = "static";
    signBg.cursor = "pointer";
    signBg.on("pointerup", () => cbRef.current.onSelectDepartment(dept));
    room.addChild(signBg);
    const signLabel = `${dept.icon || "🏢"} ${localeName(activeLocale, dept)}`;
    const signTxt = new Text({
      text: signLabel,
      style: new TextStyle({
        fontSize: 9,
        fill: 0xffffff,
        fontWeight: "bold",
        fontFamily: getFontFamilyForText(signLabel, "pixel"),
        dropShadow: { alpha: 0.2, distance: 1, color: 0x000000 },
      }),
    });
    signTxt.anchor.set(0.5, 0.5);
    signTxt.position.set(rx + roomW / 2, ry + 5);
    room.addChild(signTxt);

    // E3: Department working ratio progress bar
    {
      const workingCount = deptAgents.filter((a) => a.status === "working").length;
      const totalCount = deptAgents.length;
      const ratio = totalCount > 0 ? workingCount / totalCount : 0;
      const barW = Math.min(signW - 4, 60);
      const barH = 3;
      const barX = rx + roomW / 2 - barW / 2;
      const barY = ry + 14;
      const barG = new Graphics();
      barG.roundRect(barX, barY, barW, barH, 1.5).fill({ color: 0x000000, alpha: 0.15 });
      if (ratio > 0) {
        const fillW = Math.max(2, barW * ratio);
        const fillColor = ratio >= 0.7 ? 0x34d399 : ratio >= 0.3 ? 0xfbbf24 : 0xf87171;
        barG.roundRect(barX, barY, fillW, barH, 1.5).fill(fillColor);
      }
      room.addChild(barG);
      const ratioTxt = new Text({
        text: `${workingCount}/${totalCount}`,
        style: new TextStyle({ fontSize: 6, fill: 0xffffff, fontFamily: FONT_STACK_MONO, dropShadow: { alpha: 0.3, distance: 1, color: 0x000000 } }),
      });
      ratioTxt.anchor.set(0.5, 0);
      ratioTxt.position.set(rx + roomW / 2, barY + barH + 1);
      room.addChild(ratioTxt);
    }

    drawCeilingAndDecor(room, rx, ry, roomW, roomH, theme, deptIdx, wallClocksRef);

    if (deptAgents.length > 0) {
      drawRug(
        room,
        rx + roomW / 2,
        ry + 38 + (Math.min(deptAgentRows, 2) * SLOT_H) / 2,
        roomW - 40,
        Math.min(deptAgentRows, 2) * SLOT_H - 10,
        theme.accent,
      );
    }

    if (deptAgents.length === 0) {
      const emptyLabel = pickLocale(activeLocale, LOCALE_TEXT.noAssignedAgent);
      const emptyText = new Text({
        text: emptyLabel,
        style: new TextStyle({ fontSize: 10, fill: 0x9a8a7a, fontFamily: getFontFamilyForText(emptyLabel, "pixel") }),
      });
      emptyText.anchor.set(0.5, 0.5);
      emptyText.position.set(rx + roomW / 2, ry + roomH / 2);
      room.addChild(emptyText);
    }

    deptAgents.forEach((agent, agentIdx) => {
      const acol = agentIdx % COLS_PER_ROW;
      const arow = Math.floor(agentIdx / COLS_PER_ROW);
      const ax = rx + ROOM_PAD + acol * SLOT_W + SLOT_W / 2;
      const ay = ry + 38 + arow * SLOT_H;
      const isWorking = agent.status === "working";
      const isOffline = agent.status === "offline";
      const isBreak = agent.status === "break";

      const nameY = ay;
      const charFeetY = nameY + 24 + TARGET_CHAR_H;
      const deskY = charFeetY - 8;

      agentPosRef.current.set(agent.id, { x: ax, y: deskY });

      renderAgentHeader(room, ax, nameY, agent, theme.accent, unread, activeLocale);
      drawChair(room, ax, charFeetY - TARGET_CHAR_H * 0.18, theme.accent);

      const removedBursts = removedSubBurstsByParent.get(agent.id);
      if (removedBursts && removedBursts.length > 0) {
        for (const burst of removedBursts) {
          emitSubCloneSmokeBurst(room, subCloneBurstParticlesRef.current, burst.x, burst.y, "despawn");
        }
        removedSubBurstsByParent.delete(agent.id);
      }

      if (isBreak) {
        drawBreakAwayTag(room, ax, deskY, charFeetY, activeLocale, theme.accent);
      } else {
        renderDeskAgentAndSubClones({
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
          themeAccent: theme.accent,
          activeIssue: activeIssueByAgent?.get(agent.id),
          isBlocked: blockedAgentIds?.has(agent.id) ?? false,
        });
      }
    });

    app.stage.addChild(room);
  });
}

function drawCeilingAndDecor(
  room: Container,
  rx: number,
  ry: number,
  roomW: number,
  roomH: number,
  theme: { accent: number; wall: number },
  deptIdx: number,
  wallClocksRef: MutableRefObject<WallClockVisual[]>,
): void {
  drawCeilingLight(room, rx + roomW / 2, ry + 14, theme.accent);
  drawAmbientGlow(room, rx + roomW / 2, ry + roomH / 2, roomW * 0.4, theme.accent, 0.04);
  drawBunting(
    room,
    rx + 12,
    ry + 16,
    roomW - 24,
    blendColor(theme.accent, 0xffffff, 0.2),
    blendColor(theme.wall, 0xffffff, 0.4),
    0.52,
  );

  drawWhiteboard(room, rx + roomW - 48, ry + 18);
  drawBookshelf(room, rx + 6, ry + 18);
  wallClocksRef.current.push(drawWallClock(room, rx + roomW - 16, ry + 12));
  drawWindow(room, rx + roomW / 2 - 12, ry + 16);
  if (roomW > 240) {
    drawWindow(room, rx + roomW / 2 - 40, ry + 16, 20, 16);
    drawWindow(room, rx + roomW / 2 + 20, ry + 16, 20, 16);
  }
  if (roomW > 200) {
    drawPictureFrame(room, rx + 40, ry + 20);
  }

  drawPlant(room, rx + 8, ry + roomH - 14, deptIdx);
  drawPlant(room, rx + roomW - 12, ry + roomH - 14, deptIdx + 1);
  drawTrashCan(room, rx + roomW - 14, ry + roomH - 26);
}

function renderAgentHeader(
  room: Container,
  ax: number,
  nameY: number,
  agent: Agent,
  accent: number,
  unread: Set<string> | undefined,
  activeLocale: SupportedLocale,
): void {
  const label = localeName(activeLocale, agent);
  const nameText = new Text({
    text: label,
    style: new TextStyle({
      fontSize: 7,
      fill: 0x3a3a4a,
      fontWeight: "bold",
      fontFamily: getFontFamilyForText(label, "pixel"),
    }),
  });
  nameText.anchor.set(0.5, 0);
  const nameTagW = nameText.width + 6;
  const nameTagBg = new Graphics();
  nameTagBg.roundRect(ax - nameTagW / 2, nameY, nameTagW, 12, 3).fill({ color: 0xffffff, alpha: 0.85 });
  room.addChild(nameTagBg);
  nameText.position.set(ax, nameY + 2);
  room.addChild(nameText);

  if (unread?.has(agent.id)) {
    const bangBg = new Graphics();
    const bangX = ax + nameTagW / 2 + 2;
    bangBg.circle(bangX, nameY + 6, 6).fill(0xff3333);
    bangBg.circle(bangX, nameY + 6, 6).stroke({ width: 1, color: 0xff0000, alpha: 0.6 });
    room.addChild(bangBg);
    const bangTxt = new Text({
      text: "!",
      style: new TextStyle({ fontSize: 8, fill: 0xffffff, fontWeight: "bold", fontFamily: FONT_STACK_MONO }),
    });
    bangTxt.anchor.set(0.5, 0.5);
    bangTxt.position.set(bangX, nameY + 6);
    room.addChild(bangTxt);
  }

}

function drawBreakAwayTag(
  room: Container,
  ax: number,
  deskY: number,
  charFeetY: number,
  activeLocale: SupportedLocale,
  accent: number,
): void {
  drawDesk(room, ax - DESK_W / 2, deskY, false);
  const awayTagY = charFeetY - TARGET_CHAR_H / 2;
  const awayTagBgColor = blendColor(accent, 0x101826, 0.78);
  const awayLabel = pickLocale(activeLocale, LOCALE_TEXT.breakRoom);
  const awayTag = new Text({
    text: awayLabel,
    style: new TextStyle({
      fontSize: 8,
      fill: contrastTextColor(awayTagBgColor),
      fontWeight: "bold",
      fontFamily: getFontFamilyForText(awayLabel, "pixel"),
    }),
  });
  awayTag.anchor.set(0.5, 0.5);
  const awayTagW = awayTag.width + 10;
  const awayTagH = awayTag.height + 4;
  const awayTagBg = new Graphics();
  awayTagBg
    .roundRect(ax - awayTagW / 2, awayTagY - awayTagH / 2, awayTagW, awayTagH, 3)
    .fill({ color: awayTagBgColor, alpha: 0.9 });
  awayTagBg
    .roundRect(ax - awayTagW / 2, awayTagY - awayTagH / 2, awayTagW, awayTagH, 3)
    .stroke({ width: 1, color: blendColor(accent, 0xffffff, 0.2), alpha: 0.85 });
  room.addChild(awayTagBg);
  awayTag.position.set(ax, awayTagY + 0.5);
  room.addChild(awayTag);
}
