import { type Container, Graphics } from "pixi.js";
import { buildSpriteMap } from "../AgentAvatar";
import {
  BREAK_ROOM_GAP,
  BREAK_ROOM_H,
  CEO_ZONE_H,
  COLS_PER_ROW,
  HALLWAY_H,
  MEETING_ROOM_H,
  ROOM_PAD,
  SLOT_H,
  SLOT_W,
  detachNode,
} from "./model";
import { DEFAULT_BREAK_THEME, DEFAULT_MEETING_THEME, applyOfficeThemeMode } from "./themes-locale";
import type { BuildOfficeSceneContext } from "./buildScene-types";
import { buildCeoAndHallway } from "./buildScene-ceo-hallway";
import { buildDepartmentRooms } from "./buildScene-departments";
import { buildMeetingRoom } from "./buildScene-meeting-room";
import { buildBreakRoom } from "./buildScene-break-room";
import { buildFinalLayers } from "./buildScene-final-layers";

export function buildOfficeScene(context: BuildOfficeSceneContext): void {
  const {
    appRef,
    texturesRef,
    dataRef,
    cbRef,
    activeMeetingTaskIdRef,
    meetingMinutesOpenRef,
    localeRef,
    themeRef,
    animItemsRef,
    roomRectsRef,
    deliveriesRef,
    deliveryLayerRef,
    eventBubblesRef,
    prevAssignRef,
    agentPosRef,
    spriteMapRef,
    ceoMeetingSeatsRef,
    totalHRef,
    officeWRef,
    ceoPosRef,
    highlightRef,
    ceoOfficeRectRef,
    breakRoomRectRef,
    breakAnimItemsRef,
    subCloneAnimItemsRef,
    subCloneBurstParticlesRef,
    subCloneSnapshotRef,
    breakSteamParticlesRef,
    breakBubblesRef,
    wallClocksRef,
    wallClockSecondRef,
    setSceneRevision,
  } = context;

  const app = appRef.current;
  const textures = texturesRef.current;
  if (!app) return;

  const preservedDeliverySprites = new Set<Container>();
  for (const delivery of deliveriesRef.current) {
    if (delivery.sprite.destroyed) continue;
    preservedDeliverySprites.add(delivery.sprite);
    detachNode(delivery.sprite);
  }

  const oldChildren = app.stage.removeChildren();
  for (const child of oldChildren) {
    if (preservedDeliverySprites.has(child)) continue;
    if (!child.destroyed) child.destroy({ children: true });
  }

  animItemsRef.current = [];
  roomRectsRef.current = [];
  agentPosRef.current.clear();
  eventBubblesRef.current = [];
  breakAnimItemsRef.current = [];
  subCloneAnimItemsRef.current = [];
  subCloneBurstParticlesRef.current = [];
  breakBubblesRef.current = [];
  breakSteamParticlesRef.current = null;
  wallClocksRef.current = [];
  wallClockSecondRef.current = -1;
  ceoOfficeRectRef.current = null;
  breakRoomRectRef.current = null;
  ceoMeetingSeatsRef.current = [];

  const {
    departments,
    agents,
    tasks,
    subAgents,
    unreadAgentIds: unread,
    customDeptThemes: customThemes,
    activeMeeting,
    activeIssueByAgent,
    blockedAgentIds,
  } = dataRef.current;

  const previousSubSnapshot = subCloneSnapshotRef.current;
  const currentWorkingSubIds = new Set(subAgents.filter((sub) => sub.status === "working").map((sub) => sub.id));
  const addedWorkingSubIds = new Set<string>();
  for (const sub of subAgents) {
    if (sub.status !== "working") continue;
    if (!previousSubSnapshot.has(sub.id)) addedWorkingSubIds.add(sub.id);
  }

  const removedSubBurstsByParent = new Map<string, Array<{ x: number; y: number }>>();
  for (const [subId, prev] of previousSubSnapshot.entries()) {
    if (currentWorkingSubIds.has(subId)) continue;
    const list = removedSubBurstsByParent.get(prev.parentAgentId) ?? [];
    list.push({ x: prev.x, y: prev.y });
    removedSubBurstsByParent.set(prev.parentAgentId, list);
  }
  const nextSubSnapshot = new Map<string, { parentAgentId: string; x: number; y: number }>();

  const activeLocale = localeRef.current;
  const isDark = themeRef.current === "dark";
  applyOfficeThemeMode(isDark);
  const breakTheme = customThemes?.breakRoom ?? DEFAULT_BREAK_THEME;

  const spriteMap = buildSpriteMap(agents);
  spriteMapRef.current = spriteMap;

  const OFFICE_W = officeWRef.current;
  const deptCount = departments.length;
  const roomGap = 12;
  const layoutMargin = 12;
  const availableW = OFFICE_W - layoutMargin * 2;
  const minRoomW = 1 * SLOT_W + ROOM_PAD * 2;

  // Filter out departments with no agents assigned
  const agentsPerDeptRaw = departments.map((dept) => agents.filter((agent) => agent.department_id === dept.id));
  const nonEmptyIndices = agentsPerDeptRaw.map((da, i) => (da.length > 0 ? i : -1)).filter((i) => i >= 0);
  const filteredDepartments = nonEmptyIndices.map((i) => departments[i]);
  const agentsPerDept = nonEmptyIndices.map((i) => agentsPerDeptRaw[i]);
  // Override deptCount and departments reference for layout
  const deptCountFiltered = filteredDepartments.length;

  const meetingTheme = DEFAULT_MEETING_THEME;

  if (deptCountFiltered === 0) {
    const meetingRoomY = CEO_ZONE_H + HALLWAY_H;
    const breakRoomY = meetingRoomY + MEETING_ROOM_H + BREAK_ROOM_GAP;
    const totalH = breakRoomY + BREAK_ROOM_H + 30;
    totalHRef.current = totalH;
    app.renderer.resize(OFFICE_W, totalH);

    buildCeoAndHallway({
      app,
      OFFICE_W,
      totalH,
      meetingRoomY,
      breakRoomY,
      isDark,
    });

    buildMeetingRoom({
      app,
      textures,
      agents,
      spriteMap,
      activeLocale,
      meetingTheme,
      isDark,
      meetingRoomY,
      OFFICE_W,
      activeMeeting: activeMeeting ?? null,
      cbRef,
      wallClocksRef,
      agentPosRef,
    });

    buildBreakRoom({
      app,
      textures,
      agents,
      spriteMap,
      activeLocale,
      breakTheme,
      isDark,
      breakRoomY,
      OFFICE_W,
      cbRef,
      breakAnimItemsRef,
      breakBubblesRef,
      breakSteamParticlesRef,
      breakRoomRectRef,
      wallClocksRef,
      agentPosRef,
    });

    buildFinalLayers({
      app,
      tasks,
      ceoPosRef,
      agentPosRef,
      deliveriesRef,
      deliveryLayerRef,
      highlightRef,
      prevAssignRef,
      setSceneRevision,
    });

    return;
  }

  // Each department's ideal width based on actual agent count
  const deptWidths = agentsPerDept.map((da) => {
    const neededCols = Math.min(COLS_PER_ROW, Math.max(1, da.length));
    return Math.max(minRoomW, neededCols * SLOT_W + ROOM_PAD * 2);
  });

  // Flow layout: greedily pack departments into rows
  const flowRows: number[][] = [[]];
  let curRowW = 0;
  for (let i = 0; i < deptCountFiltered; i++) {
    const w = deptWidths[i];
    const gap = flowRows[flowRows.length - 1].length > 0 ? roomGap : 0;
    if (curRowW + gap + w > availableW && flowRows[flowRows.length - 1].length > 0) {
      flowRows.push([i]);
      curRowW = w;
    } else {
      flowRows[flowRows.length - 1].push(i);
      curRowW += gap + w;
    }
  }

  const deptStartY = CEO_ZONE_H + HALLWAY_H;

  // Per-row height based on tallest department in that row
  const rowHeights = flowRows.map((rowIndices) => {
    let maxInRow = 0;
    for (const idx of rowIndices) maxInRow = Math.max(maxInRow, agentsPerDept[idx].length);
    const rowAgentRows = Math.ceil(Math.max(1, maxInRow) / COLS_PER_ROW);
    return Math.max(170, rowAgentRows * SLOT_H + 44);
  });

  // Position each department
  const deptLayouts: Array<{ rx: number; ry: number; rw: number; rh: number; deptAgentRows: number }> = [];
  let curY = deptStartY;
  for (let r = 0; r < flowRows.length; r++) {
    const rowIndices = flowRows[r];
    const rowWidths = rowIndices.map((idx) => deptWidths[idx]);
    const totalRowW = rowWidths.reduce((s, w) => s + w, 0) + (rowIndices.length - 1) * roomGap;
    const rowStartX = (OFFICE_W - totalRowW) / 2;
    const rh = rowHeights[r];

    let curX = rowStartX;
    for (let i = 0; i < rowIndices.length; i++) {
      const deptIdx = rowIndices[i];
      const rw = rowWidths[i];
      const deptAgentRows = Math.ceil(Math.max(1, agentsPerDept[deptIdx].length) / COLS_PER_ROW);
      deptLayouts[deptIdx] = { rx: curX, ry: curY, rw, rh, deptAgentRows };
      curX += rw + roomGap;
    }
    curY += rh + roomGap;
  }

  const lastRowY = curY - roomGap;
  const meetingRoomY = lastRowY + HALLWAY_H;
  const breakRoomY = meetingRoomY + MEETING_ROOM_H + BREAK_ROOM_GAP;
  const totalH = breakRoomY + BREAK_ROOM_H + 30;
  totalHRef.current = totalH;

  app.renderer.resize(OFFICE_W, totalH);

  buildCeoAndHallway({
    app,
    OFFICE_W,
    totalH,
    meetingRoomY,
    breakRoomY,
    isDark,
  });

  buildDepartmentRooms({
    app,
    textures,
    departments: filteredDepartments,
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
  });
  subCloneSnapshotRef.current = nextSubSnapshot;

  buildMeetingRoom({
    app,
    textures,
    agents,
    spriteMap,
    activeLocale,
    meetingTheme,
    isDark,
    meetingRoomY,
    OFFICE_W,
    activeMeeting: activeMeeting ?? null,
    cbRef,
    wallClocksRef,
    agentPosRef,
  });

  buildBreakRoom({
    app,
    textures,
    agents,
    spriteMap,
    activeLocale,
    breakTheme,
    isDark,
    breakRoomY,
    OFFICE_W,
    cbRef,
    breakAnimItemsRef,
    breakBubblesRef,
    breakSteamParticlesRef,
    breakRoomRectRef,
    wallClocksRef,
    agentPosRef,
  });

  buildFinalLayers({
    app,
    tasks,
    ceoPosRef,
    agentPosRef,
    deliveriesRef,
    deliveryLayerRef,
    highlightRef,
    prevAssignRef,
    setSceneRevision,
  });

  // E4: Time-of-day atmosphere overlay
  {
    const hour = new Date().getHours();
    let tintColor = 0x000000;
    let tintAlpha = 0;
    if (hour >= 22 || hour < 5) {
      // Night: deep blue tint
      tintColor = 0x0a1628;
      tintAlpha = isDark ? 0.12 : 0.08;
    } else if (hour >= 5 && hour < 7) {
      // Dawn: warm orange
      tintColor = 0xffa040;
      tintAlpha = isDark ? 0.04 : 0.03;
    } else if (hour >= 17 && hour < 20) {
      // Sunset: amber
      tintColor = 0xff8c30;
      tintAlpha = isDark ? 0.05 : 0.04;
    } else if (hour >= 20 && hour < 22) {
      // Evening: deep dusk blue
      tintColor = 0x1a1a40;
      tintAlpha = isDark ? 0.08 : 0.05;
    }
    if (tintAlpha > 0) {
      const overlay = new Graphics();
      overlay.rect(0, 0, OFFICE_W, totalH).fill({ color: tintColor, alpha: tintAlpha });
      overlay.eventMode = "none";
      app.stage.addChild(overlay);
    }
  }
}
