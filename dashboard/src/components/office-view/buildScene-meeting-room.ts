import type { MutableRefObject } from "react";
import { Container, Graphics, Sprite, Text, TextStyle, type Application, type Texture } from "pixi.js";
import type { Agent, RoundTableMeeting } from "../../types";
import { localeName } from "../../i18n";
import { FONT_STACK_MONO, getFontFamilyForText } from "../../lib/fonts";
import type { CallbackSnapshot } from "./buildScene-types";
import { MEETING_ROOM_H, TARGET_CHAR_H, type RoomTheme, type WallClockVisual } from "./model";
import { LOCALE_TEXT, type SupportedLocale, pickLocale } from "./themes-locale";
import {
  contrastTextColor,
  drawAmbientGlow,
  drawCeilingLight,
  drawRoomAtmosphere,
  drawTiledFloor,
  drawWallClock,
  hashStr,
} from "./drawing-core";
import { drawPlant } from "./drawing-furniture-a";

interface BuildMeetingRoomParams {
  app: Application;
  textures: Record<string, Texture>;
  agents: Agent[];
  spriteMap: Map<string, number>;
  activeLocale: SupportedLocale;
  meetingTheme: RoomTheme;
  isDark: boolean;
  meetingRoomY: number;
  OFFICE_W: number;
  activeMeeting: RoundTableMeeting | null;
  cbRef: MutableRefObject<CallbackSnapshot>;
  wallClocksRef: MutableRefObject<WallClockVisual[]>;
  agentPosRef: MutableRefObject<Map<string, { x: number; y: number }>>;
}

// 6-seat positions relative to table center (cx, cy)
// Symmetric arrangement: 2 top, 1 right, 2 bottom, 1 left
const SEAT_OFFSETS: Array<{ dx: number; dy: number; dir: string }> = [
  { dx: -20, dy: -32, dir: "D" },   // seat 1: top-left
  { dx: 20, dy: -32, dir: "D" },    // seat 2: top-right
  { dx: 46, dy: 4, dir: "L" },      // seat 3: right-center
  { dx: 20, dy: 36, dir: "U" },     // seat 4: bottom-right
  { dx: -20, dy: 36, dir: "U" },    // seat 5: bottom-left
  { dx: -46, dy: 4, dir: "R" },     // seat 6: left-center
];

function matchParticipantToAgent(
  name: string,
  agents: Agent[],
): Agent | null {
  const lower = name.toLowerCase();
  // Extract abbreviation before parenthesis: "TD (테크니컬 디렉터)" → "td"
  const abbrev = lower.replace(/\s*\(.*$/, "").trim();

  for (const agent of agents) {
    // Match by role_id display name
    if (agent.role_id) {
      const displayName = inferDisplayNameLocal(agent.role_id).toLowerCase();
      if (displayName === lower || displayName === abbrev) return agent;
    }
    // Match by agent name fields
    const agentName = agent.name.toLowerCase();
    if (agentName === lower || agentName === abbrev) return agent;
    const agentNameKo = agent.name_ko?.toLowerCase();
    if (agentNameKo && (agentNameKo === lower || agentNameKo === abbrev)) return agent;
    const agentAlias = agent.alias?.toLowerCase();
    if (agentAlias && (agentAlias === lower || agentAlias === abbrev)) return agent;
  }
  return null;
}

// Local version of inferDisplayName to avoid server import
function inferDisplayNameLocal(roleId: string): string {
  if (roleId.startsWith("ch-")) return roleId.slice(3).toUpperCase();
  if (roleId.endsWith("-agent")) return roleId.replace(/-agent$/, "");
  return roleId;
}

export function buildMeetingRoom({
  app,
  textures,
  agents,
  spriteMap,
  activeLocale,
  meetingTheme,
  isDark,
  meetingRoomY,
  OFFICE_W,
  activeMeeting,
  cbRef,
  wallClocksRef,
  agentPosRef,
}: BuildMeetingRoomParams): void {
  const room = new Container();
  const rx = 4;
  const ry = meetingRoomY;
  const rw = OFFICE_W - 8;
  const rh = MEETING_ROOM_H;
  const isActive = activeMeeting?.status === "in_progress";

  // Floor
  const floor = new Graphics();
  drawTiledFloor(floor, rx, ry, rw, rh, meetingTheme.floor1, meetingTheme.floor2);
  room.addChild(floor);
  drawRoomAtmosphere(room, rx, ry, rw, rh, meetingTheme.wall, meetingTheme.accent);

  // Border
  const border = new Graphics();
  border.roundRect(rx, ry, rw, rh, 3).stroke({ width: 2, color: meetingTheme.wall });
  border.roundRect(rx - 1, ry - 1, rw + 2, rh + 2, 4).stroke({
    width: 1,
    color: isActive ? meetingTheme.accent : meetingTheme.wall,
    alpha: isActive ? 0.5 : 0.25,
  });
  room.addChild(border);

  // Ambient glow
  drawAmbientGlow(room, rx + rw / 2, ry + rh / 2, rw * 0.25, meetingTheme.accent, isActive ? 0.08 : 0.04);
  drawCeilingLight(room, rx + rw / 3, ry + 6, meetingTheme.accent);
  drawCeilingLight(room, rx + (rw * 2) / 3, ry + 6, meetingTheme.accent);

  // Plants
  drawPlant(room as Container, rx + 16, ry + rh - 14, 1);
  drawPlant(room as Container, rx + rw - 16, ry + rh - 14, 2);

  // Wall clock
  wallClocksRef.current.push(drawWallClock(room, rx + rw - 40, ry + 18));

  // Room sign
  const signW = isActive ? 120 : 84;
  const signBg = new Graphics();
  signBg.roundRect(rx + rw / 2 - signW / 2 + 1, ry - 3, signW, 18, 4).fill({ color: 0x000000, alpha: 0.12 });
  signBg.roundRect(rx + rw / 2 - signW / 2, ry - 4, signW, 18, 4).fill(
    isActive ? 0xdc2626 : meetingTheme.accent,
  );
  room.addChild(signBg);

  const signTextColor = isDark ? 0xffffff : contrastTextColor(isActive ? 0xdc2626 : meetingTheme.accent);
  const signLabel = isActive
    ? `🔴 ${pickLocale(activeLocale, LOCALE_TEXT.meetingInProgress)}`
    : pickLocale(activeLocale, LOCALE_TEXT.meetingRoom);
  const signTxt = new Text({
    text: signLabel,
    style: new TextStyle({
      fontSize: 9,
      fill: signTextColor,
      fontWeight: "bold",
      fontFamily: getFontFamilyForText(signLabel, "pixel"),
      dropShadow: isDark ? { alpha: 0.6, blur: 2, distance: 1, color: 0x000000 } : undefined,
    }),
  });
  signTxt.anchor.set(0.5, 0.5);
  signTxt.position.set(rx + rw / 2, ry + 5);
  room.addChild(signTxt);

  // Table center
  const cx = rx + rw / 2;
  const cy = ry + rh / 2;

  // Draw oval table
  const tableW = 60;
  const tableH = 30;
  const tableG = new Graphics();
  // Shadow
  tableG.ellipse(cx, cy + 3, tableW / 2 + 4, tableH / 2 + 4).fill({ color: 0x000000, alpha: 0.08 });
  // Table frame
  tableG.ellipse(cx, cy, tableW / 2 + 2, tableH / 2 + 2).fill(0x5c3d2e);
  // Table surface
  tableG.ellipse(cx, cy, tableW / 2, tableH / 2).fill(0xd4b478);
  // Highlight
  tableG.ellipse(cx, cy - 2, tableW / 2 - 4, tableH / 2 - 4).fill({ color: 0xe8d098, alpha: 0.4 });
  // Wood grain
  for (let i = -2; i <= 2; i++) {
    tableG.ellipse(cx, cy + i * 3, tableW / 2 - 6, 1).fill({ color: 0xc4a060, alpha: 0.15 });
  }

  if (isActive) {
    // Active meeting glow around table
    tableG.ellipse(cx, cy, tableW / 2 + 6, tableH / 2 + 6).stroke({
      width: 2,
      color: meetingTheme.accent,
      alpha: 0.4,
    });
  }
  room.addChild(tableG);

  // Draw chairs and optionally seat participants (only during active meeting)
  const participantNames = isActive ? (activeMeeting?.participant_names ?? []) : [];
  const matchedAgents: Array<Agent | null> = participantNames.map((name) =>
    matchParticipantToAgent(name, agents),
  );

  for (let i = 0; i < 6; i++) {
    const seat = SEAT_OFFSETS[i];
    const sx = cx + seat.dx;
    const sy = cy + seat.dy;

    // Chair
    const chair = new Graphics();
    const chairW = 12;
    const chairH = 12;
    chair.roundRect(sx - chairW / 2, sy - chairH / 2, chairW, chairH, 2).fill(
      i < matchedAgents.length && matchedAgents[i] ? 0x6b7c94 : 0x8899aa,
    );
    chair.roundRect(sx - chairW / 2, sy - chairH / 2, chairW, chairH, 2).stroke({
      width: 0.5,
      color: 0x4a5568,
      alpha: 0.5,
    });
    room.addChild(chair);

    const agent = i < matchedAgents.length ? matchedAgents[i] : null;
    if (!agent) continue;

    // Draw seated agent sprite
    const seed = hashStr(agent.id);
    const spriteNum = spriteMap.get(agent.id) ?? (seed % 13) + 1;

    const charContainer = new Container();
    charContainer.position.set(sx, sy - 6);
    charContainer.eventMode = "static";
    charContainer.cursor = "pointer";
    charContainer.on("pointerup", () => cbRef.current.onSelectAgent(agent));

    const dirKey = `${spriteNum}-${seat.dir}-1`;
    const fallbackKey = `${spriteNum}-D-1`;
    const texture = textures[dirKey] || textures[fallbackKey];

    if (texture) {
      const sprite = new Sprite(texture);
      sprite.anchor.set(0.5, 1);
      const scale = (TARGET_CHAR_H * 0.7) / sprite.texture.height;
      sprite.scale.set(scale);
      charContainer.addChild(sprite);
    } else {
      const fallback = new Text({ text: agent.avatar_emoji || "\u{1F916}", style: new TextStyle({ fontSize: 16 }) });
      fallback.anchor.set(0.5, 1);
      charContainer.addChild(fallback);
    }
    room.addChild(charContainer);
    agentPosRef.current.set(agent.id, { x: sx, y: sy - 6 });

    // Name tag
    const nameLabel = localeName(activeLocale, agent);
    const nameTag = new Text({
      text: nameLabel,
      style: new TextStyle({ fontSize: 5.5, fill: isDark ? 0xd0d8e8 : 0x4a3a2a, fontFamily: getFontFamilyForText(nameLabel, "pixel") }),
    });
    nameTag.anchor.set(0.5, 0);
    const ntW = nameTag.width + 4;
    const ntBg = new Graphics();
    ntBg.roundRect(sx - ntW / 2, sy + 4, ntW, 8, 2).fill({ color: isDark ? 0x1a1e28 : 0xffffff, alpha: 0.8 });
    room.addChild(ntBg);
    nameTag.position.set(sx, sy + 4.5);
    room.addChild(nameTag);

    // Current speaker indicator
    if (activeMeeting && isActive) {
      const currentSpeaker = (activeMeeting as RoundTableMeeting & { current_speaker?: string | null }).current_speaker;
      const participantName = participantNames[i];
      if (currentSpeaker && participantName && currentSpeaker.toLowerCase() === participantName.toLowerCase()) {
        // Speaking glow circle
        const glow = new Graphics();
        glow.circle(sx, sy - 6, 16).fill({ color: meetingTheme.accent, alpha: 0.2 });
        glow.circle(sx, sy - 6, 14).stroke({ width: 1.5, color: meetingTheme.accent, alpha: 0.6 });
        room.addChild(glow);

        // Speech bubble
        const bubble = new Text({
          text: "\u{1F4AC}",
          style: new TextStyle({ fontSize: 10 }),
        });
        bubble.anchor.set(0.5, 1);
        bubble.position.set(sx + 12, sy - TARGET_CHAR_H * 0.7 - 4);
        room.addChild(bubble);
      }
    }
  }

  // Round indicator (when meeting is active)
  if (activeMeeting && isActive) {
    const currentRound = (activeMeeting as RoundTableMeeting & { current_round?: number | null }).current_round;
    const totalRounds = activeMeeting.total_rounds;
    if (currentRound && totalRounds) {
      const roundLabel = `Round ${currentRound}/${totalRounds}`;
      const roundTxt = new Text({
        text: roundLabel,
        style: new TextStyle({
          fontSize: 7,
          fill: isDark ? 0xc8d0e0 : 0x4a5568,
          fontWeight: "bold",
          fontFamily: FONT_STACK_MONO,
        }),
      });
      roundTxt.anchor.set(0.5, 0);
      roundTxt.position.set(cx, cy + tableH / 2 + 8);
      room.addChild(roundTxt);
    }
  }

  // Agenda text when meeting is active
  if (activeMeeting && isActive) {
    const agendaText = activeMeeting.agenda.length > 40
      ? `${activeMeeting.agenda.slice(0, 40)}...`
      : activeMeeting.agenda;
    const agendaTxt = new Text({
      text: agendaText,
      style: new TextStyle({
        fontSize: 6,
        fill: isDark ? 0x8899aa : 0x6b7c94,
        fontFamily: getFontFamilyForText(agendaText, "pixel"),
        wordWrap: true,
        wordWrapWidth: rw * 0.6,
      }),
    });
    agendaTxt.anchor.set(0.5, 1);
    agendaTxt.position.set(cx, ry + rh - 8);
    room.addChild(agendaTxt);
  }

  app.stage.addChild(room);
}
