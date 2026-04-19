import type { MutableRefObject } from "react";
import { Container, Graphics, Sprite, Text, TextStyle, type Application, type Texture } from "pixi.js";
import type { Agent } from "../../types";
import { localeName } from "../../i18n";
import { getFontFamilyForText } from "../../lib/fonts";
import type { CallbackSnapshot, BreakAnimItem } from "./buildScene-types";
import { BREAK_ROOM_H, TARGET_CHAR_H, type RoomTheme, type WallClockVisual } from "./model";
import { BREAK_CHAT_MESSAGES, BREAK_SPOTS, LOCALE_TEXT, type SupportedLocale, pickLocale } from "./themes-locale";
import {
  blendColor,
  contrastTextColor,
  drawAmbientGlow,
  drawBunting,
  drawCeilingLight,
  drawPictureFrame,
  drawRoomAtmosphere,
  drawRug,
  drawTiledFloor,
  drawTrashCan,
  drawWallClock,
  hashStr,
} from "./drawing-core";
import { drawPlant } from "./drawing-furniture-a";
import { drawCoffeeMachine, drawCoffeeTable, drawHighTable, drawSofa, drawVendingMachine } from "./drawing-furniture-b";

interface BuildBreakRoomParams {
  app: Application;
  textures: Record<string, Texture>;
  agents: Agent[];
  spriteMap: Map<string, number>;
  activeLocale: SupportedLocale;
  breakTheme: RoomTheme;
  isDark: boolean;
  breakRoomY: number;
  OFFICE_W: number;
  cbRef: MutableRefObject<CallbackSnapshot>;
  breakAnimItemsRef: MutableRefObject<BreakAnimItem[]>;
  breakBubblesRef: MutableRefObject<Container[]>;
  breakSteamParticlesRef: MutableRefObject<Container | null>;
  breakRoomRectRef: MutableRefObject<{ x: number; y: number; w: number; h: number } | null>;
  wallClocksRef: MutableRefObject<WallClockVisual[]>;
  agentPosRef: MutableRefObject<Map<string, { x: number; y: number }>>;
}

export function buildBreakRoom({
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
}: BuildBreakRoomParams): void {
  // Show break agents AND unassigned agents (no department) in break room
  const breakAgents = agents.filter(
    (agent) => agent.status === "break" || !agent.department_id,
  );
  breakAnimItemsRef.current = [];
  breakBubblesRef.current = [];

  const breakRoom = new Container();
  const brx = 4;
  const bry = breakRoomY;
  const brw = OFFICE_W - 8;
  const brh = BREAK_ROOM_H;
  breakRoomRectRef.current = { x: brx, y: bry, w: brw, h: brh };

  const brFloor = new Graphics();
  drawTiledFloor(brFloor, brx, bry, brw, brh, breakTheme.floor1, breakTheme.floor2);
  breakRoom.addChild(brFloor);
  drawRoomAtmosphere(breakRoom, brx, bry, brw, brh, breakTheme.wall, breakTheme.accent);

  const brBorder = new Graphics();
  brBorder.roundRect(brx, bry, brw, brh, 3).stroke({ width: 2, color: breakTheme.wall });
  brBorder.roundRect(brx - 1, bry - 1, brw + 2, brh + 2, 4).stroke({ width: 1, color: breakTheme.accent, alpha: 0.25 });
  breakRoom.addChild(brBorder);

  drawAmbientGlow(breakRoom, brx + brw / 2, bry + brh / 2, brw * 0.3, breakTheme.accent, 0.05);
  drawCeilingLight(breakRoom, brx + brw / 3, bry + 6, breakTheme.accent);
  drawCeilingLight(breakRoom, brx + (brw * 2) / 3, bry + 6, breakTheme.accent);
  drawBunting(
    breakRoom,
    brx + 14,
    bry + 16,
    brw - 28,
    blendColor(0xb5d6cf, 0xffffff, 0.18),
    blendColor(0xdcb7bf, 0xffffff, 0.08),
    0.64,
  );

  const furnitureBaseX = brx + 16;
  drawCoffeeMachine(breakRoom, furnitureBaseX, bry + 20);
  drawPlant(breakRoom, furnitureBaseX + 30, bry + 38, 1);
  drawSofa(breakRoom, furnitureBaseX + 50, bry + 56, 0xc89da6);
  drawCoffeeTable(breakRoom, furnitureBaseX + 140, bry + 58);

  const furnitureRightX = brx + brw - 16;
  drawVendingMachine(breakRoom, furnitureRightX - 26, bry + 20);
  drawPlant(breakRoom, furnitureRightX - 36, bry + 38, 2);
  drawSofa(breakRoom, furnitureRightX - 120, bry + 56, 0x91bcae);
  drawHighTable(breakRoom, furnitureRightX - 170, bry + 24);

  drawPictureFrame(breakRoom, brx + brw / 2 - 8, bry + 14);
  wallClocksRef.current.push(drawWallClock(breakRoom, brx + brw / 2 + 30, bry + 18));
  drawTrashCan(breakRoom, furnitureBaseX + 24, bry + brh - 14);

  const brSignW = 84;
  const brSignBg = new Graphics();
  brSignBg.roundRect(brx + brw / 2 - brSignW / 2 + 1, bry - 3, brSignW, 18, 4).fill({ color: 0x000000, alpha: 0.12 });
  brSignBg.roundRect(brx + brw / 2 - brSignW / 2, bry - 4, brSignW, 18, 4).fill(breakTheme.accent);
  breakRoom.addChild(brSignBg);
  const breakSignTextColor = isDark ? 0xffffff : contrastTextColor(breakTheme.accent);
  const breakRoomLabel = pickLocale(activeLocale, LOCALE_TEXT.breakRoom);
  const brSignTxt = new Text({
    text: breakRoomLabel,
    style: new TextStyle({
      fontSize: 9,
      fill: breakSignTextColor,
      fontWeight: "bold",
      fontFamily: getFontFamilyForText(breakRoomLabel, "pixel"),
      dropShadow: isDark ? { alpha: 0.6, blur: 2, distance: 1, color: 0x000000 } : undefined,
    }),
  });
  brSignTxt.anchor.set(0.5, 0.5);
  brSignTxt.position.set(brx + brw / 2, bry + 5);
  breakRoom.addChild(brSignTxt);

  drawRug(breakRoom, brx + brw / 2, bry + brh / 2 + 10, brw * 0.5, brh * 0.45, breakTheme.accent);

  const steamContainer = new Container();
  breakRoom.addChild(steamContainer);
  breakSteamParticlesRef.current = steamContainer;

  const spotCount = BREAK_SPOTS.length;
  function resolveSpotPos(idx: number, seed: number) {
    const ox = (seed % 21) - 10;
    const oy = (seed % 9) - 4;
    if (idx < spotCount) {
      const s = BREAK_SPOTS[idx];
      const x = s.center
        ? brx + brw / 2 + s.x + ox
        : s.x >= 0
          ? brx + s.x + ox
          : brx + brw - 16 + s.x + ox;
      return { x, y: bry + s.y + oy, dir: s.dir };
    }
    const overflow = breakAgents.length - spotCount;
    const col = idx - spotCount;
    return {
      x: brx + 40 + ((brw - 80) * (col + 1)) / (overflow + 1) + ox,
      y: bry + 66 + oy,
      dir: "D",
    };
  }

  breakAgents.forEach((agent, index) => {
    const seed = hashStr(agent.id);
    const { x: spotX, y: spotY, dir } = resolveSpotPos(index, seed);

    agentPosRef.current.set(agent.id, { x: spotX, y: spotY });

    const spriteNum = spriteMap.get(agent.id) ?? (seed % 13) + 1;
    const charContainer = new Container();
    charContainer.position.set(spotX, spotY);
    charContainer.eventMode = "static";
    charContainer.cursor = "pointer";
    charContainer.on("pointerup", () => cbRef.current.onSelectAgent(agent));

    const dirKey = `${spriteNum}-${dir}-1`;
    const fallbackKey = `${spriteNum}-D-1`;
    const texture = textures[dirKey] || textures[fallbackKey];

    if (texture) {
      const sprite = new Sprite(texture);
      sprite.anchor.set(0.5, 1);
      const scale = (TARGET_CHAR_H * 0.85) / sprite.texture.height;
      sprite.scale.set(scale);
      charContainer.addChild(sprite);
    } else {
      const fallback = new Text({ text: agent.avatar_emoji || "🤖", style: new TextStyle({ fontSize: 20 }) });
      fallback.anchor.set(0.5, 1);
      charContainer.addChild(fallback);
    }
    breakRoom.addChild(charContainer);

    breakAnimItemsRef.current.push({
      sprite: charContainer,
      baseX: spotX,
      baseY: spotY,
    });

    const coffeeEmoji = new Text({ text: "☕", style: new TextStyle({ fontSize: 10 }) });
    coffeeEmoji.anchor.set(0.5, 0.5);
    coffeeEmoji.position.set(spotX + 14, spotY - 10);
    breakRoom.addChild(coffeeEmoji);

    const nameLabel = localeName(activeLocale, agent);
    const nameTag = new Text({
      text: nameLabel,
      style: new TextStyle({ fontSize: 6, fill: 0x4a3a2a, fontFamily: getFontFamilyForText(nameLabel, "pixel") }),
    });
    nameTag.anchor.set(0.5, 0);
    const ntW = nameTag.width + 4;
    const ntBg = new Graphics();
    ntBg.roundRect(spotX - ntW / 2, spotY + 2, ntW, 9, 2).fill({ color: 0xffffff, alpha: 0.8 });
    breakRoom.addChild(ntBg);
    nameTag.position.set(spotX, spotY + 3);
    breakRoom.addChild(nameTag);
  });

  if (breakAgents.length > 0) {
    const phase = Math.floor(Date.now() / 4000);
    const speakerCount = Math.min(2, breakAgents.length);
    for (let speakerIndex = 0; speakerIndex < speakerCount; speakerIndex++) {
      const speakerIdx = (phase + speakerIndex) % breakAgents.length;
      const agent = breakAgents[speakerIdx];
      const seed = hashStr(agent.id);
      const { x: spotX, y: spotY } = resolveSpotPos(speakerIdx, seed);

      const chatPool = BREAK_CHAT_MESSAGES[activeLocale] || BREAK_CHAT_MESSAGES.ko;
      const msg = chatPool[(seed + phase) % chatPool.length];
      const bubbleText = new Text({
        text: msg,
        style: new TextStyle({ fontSize: 7, fill: 0x333333, fontFamily: getFontFamilyForText(msg, "pixel") }),
      });
      bubbleText.anchor.set(0.5, 1);
      const bw = bubbleText.width + 10;
      const bh = bubbleText.height + 6;
      const bubbleTop = spotY - TARGET_CHAR_H * 0.85 - bh - 4;

      const bubbleG = new Graphics();
      bubbleG.roundRect(spotX - bw / 2, bubbleTop, bw, bh, 4).fill(0xfff8f0);
      bubbleG.roundRect(spotX - bw / 2, bubbleTop, bw, bh, 4).stroke({
        width: 1.2,
        color: breakTheme.accent,
        alpha: 0.5,
      });
      bubbleG
        .moveTo(spotX - 3, bubbleTop + bh)
        .lineTo(spotX, bubbleTop + bh + 4)
        .lineTo(spotX + 3, bubbleTop + bh)
        .fill(0xfff8f0);
      breakRoom.addChild(bubbleG);
      bubbleText.position.set(spotX, bubbleTop + bh - 3);
      breakRoom.addChild(bubbleText);

      const bubbleContainer = new Container();
      bubbleContainer.addChild(bubbleG);
      breakRoom.removeChild(bubbleG);
      breakRoom.removeChild(bubbleText);
      bubbleContainer.addChild(bubbleG);
      bubbleContainer.addChild(bubbleText);
      breakRoom.addChild(bubbleContainer);
      breakBubblesRef.current.push(bubbleContainer);
    }
  }

  app.stage.addChild(breakRoom);
}
