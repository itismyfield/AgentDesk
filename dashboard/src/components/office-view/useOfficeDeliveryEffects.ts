import { useEffect, type MutableRefObject } from "react";
import { AnimatedSprite, Container, Graphics, Text, TextStyle, type Texture } from "pixi.js";
import type { Agent, CeoOfficeCall, CrossDeptDelivery, MeetingPresence } from "../../types";
import { FONT_STACK_PIXEL, getFontFamilyForText } from "../../lib/fonts";
import { hashStr } from "./drawing-core";
import { type Delivery, destroyNode, trackProcessedId } from "./model";
import {
  LOCALE_TEXT,
  paintMeetingBadge,
  pickLocale,
  resolveMeetingDecision,
  type SupportedLocale,
} from "./themes-locale";

interface MeetingPresenceSyncParams {
  meetingPresence?: MeetingPresence[];
  language: SupportedLocale;
  sceneRevision: number;
  deliveryLayerRef: MutableRefObject<Container | null>;
  texturesRef: MutableRefObject<Record<string, Texture>>;
  ceoMeetingSeatsRef: MutableRefObject<Array<{ x: number; y: number }>>;
  deliveriesRef: MutableRefObject<Delivery[]>;
  spriteMapRef: MutableRefObject<Map<string, number>>;
}

export function useMeetingPresenceSync({
  meetingPresence,
  language,
  sceneRevision,
  deliveryLayerRef,
  texturesRef,
  ceoMeetingSeatsRef,
  deliveriesRef,
  spriteMapRef,
}: MeetingPresenceSyncParams): void {
  useEffect(() => {
    const dlLayer = deliveryLayerRef.current;
    const textures = texturesRef.current;
    const seats = ceoMeetingSeatsRef.current;
    if (!dlLayer || seats.length === 0) return;

    const now = Date.now();
    const rows = (meetingPresence ?? []).filter((row) => row.until >= now);
    const activeByAgent = new Map(rows.map((row) => [row.agent_id, row]));

    for (const row of rows) {
      const seat = seats[row.seat_index % seats.length];
      if (!seat) continue;
      const decision = resolveMeetingDecision(row.phase, row.decision);

      const existing = deliveriesRef.current.find((delivery) => {
        return delivery.agentId === row.agent_id && delivery.holdAtSeat && !delivery.sprite.destroyed;
      });

      if (existing) {
        existing.meetingSeatIndex = row.seat_index;
        existing.holdUntil = row.until;
        existing.toX = seat.x;
        existing.toY = seat.y;
        existing.arrived = true;
        existing.progress = 1;
        existing.seatedPoseApplied = false;
        existing.meetingDecision = decision;
        existing.sprite.position.set(seat.x, seat.y);
        existing.sprite.alpha = 1;
        if (existing.badgeGraphics && existing.badgeText) {
          paintMeetingBadge(existing.badgeGraphics, existing.badgeText, language, row.phase, decision);
        }
        continue;
      }

      const spriteNum = spriteMapRef.current.get(row.agent_id) ?? (hashStr(row.agent_id) % 13) + 1;
      const actor = new Container();
      const frames: Texture[] = [];

      for (let frame = 1; frame <= 3; frame++) {
        const key = `${spriteNum}-D-${frame}`;
        if (textures[key]) frames.push(textures[key]);
      }

      if (frames.length > 0) {
        const animSprite = new AnimatedSprite(frames);
        animSprite.anchor.set(0.5, 1);
        const scale = 44 / animSprite.texture.height;
        animSprite.scale.set(scale);
        animSprite.gotoAndStop(0);
        actor.addChild(animSprite);
      } else {
        const fallback = new Text({ text: "🧑‍💼", style: new TextStyle({ fontSize: 20 }) });
        fallback.anchor.set(0.5, 1);
        actor.addChild(fallback);
      }

      const badge = new Graphics();
      actor.addChild(badge);
      const badgeText = new Text({
        text: "",
        style: new TextStyle({ fontSize: 7, fill: 0x111111, fontWeight: "bold", fontFamily: FONT_STACK_PIXEL }),
      });
      badgeText.anchor.set(0.5, 0.5);
      badgeText.position.set(0, 10.5);
      actor.addChild(badgeText);
      paintMeetingBadge(badge, badgeText, language, row.phase, decision);

      actor.position.set(seat.x, seat.y);
      dlLayer.addChild(actor);
      deliveriesRef.current.push({
        sprite: actor,
        fromX: seat.x,
        fromY: seat.y,
        toX: seat.x,
        toY: seat.y,
        progress: 1,
        speed: 0.0048,
        type: "walk",
        agentId: row.agent_id,
        holdAtSeat: true,
        holdUntil: row.until,
        arrived: true,
        meetingSeatIndex: row.seat_index,
        meetingDecision: decision,
        badgeGraphics: badge,
        badgeText,
      });
    }

    for (let i = deliveriesRef.current.length - 1; i >= 0; i--) {
      const delivery = deliveriesRef.current[i];
      if (!delivery.holdAtSeat || !delivery.agentId || !delivery.arrived) continue;
      if (activeByAgent.has(delivery.agentId)) continue;
      destroyNode(delivery.sprite);
      deliveriesRef.current.splice(i, 1);
    }
  }, [
    meetingPresence,
    language,
    sceneRevision,
    deliveryLayerRef,
    texturesRef,
    ceoMeetingSeatsRef,
    deliveriesRef,
    spriteMapRef,
  ]);
}

interface CrossDeptDeliveryAnimationParams {
  crossDeptDeliveries?: CrossDeptDelivery[];
  language: SupportedLocale;
  onCrossDeptDeliveryProcessed?: (id: string) => void;
  deliveryLayerRef: MutableRefObject<Container | null>;
  texturesRef: MutableRefObject<Record<string, Texture>>;
  agentPosRef: MutableRefObject<Map<string, { x: number; y: number }>>;
  spriteMapRef: MutableRefObject<Map<string, number>>;
  processedCrossDeptRef: MutableRefObject<Set<string>>;
  deliveriesRef: MutableRefObject<Delivery[]>;
}

export function useCrossDeptDeliveryAnimations({
  crossDeptDeliveries,
  language,
  onCrossDeptDeliveryProcessed,
  deliveryLayerRef,
  texturesRef,
  agentPosRef,
  spriteMapRef,
  processedCrossDeptRef,
  deliveriesRef,
}: CrossDeptDeliveryAnimationParams): void {
  useEffect(() => {
    if (!crossDeptDeliveries?.length) return;
    const dlLayer = deliveryLayerRef.current;
    const textures = texturesRef.current;
    if (!dlLayer) return;

    for (const delivery of crossDeptDeliveries) {
      if (processedCrossDeptRef.current.has(delivery.id)) continue;
      trackProcessedId(processedCrossDeptRef.current, delivery.id);

      const fromPos = agentPosRef.current.get(delivery.fromAgentId);
      const toPos = agentPosRef.current.get(delivery.toAgentId);
      if (!fromPos || !toPos) {
        onCrossDeptDeliveryProcessed?.(delivery.id);
        continue;
      }

      const actor = new Container();
      const spriteNum = spriteMapRef.current.get(delivery.fromAgentId) ?? (hashStr(delivery.fromAgentId) % 13) + 1;
      const frames: Texture[] = [];
      for (let frame = 1; frame <= 3; frame++) {
        const key = `${spriteNum}-D-${frame}`;
        if (textures[key]) frames.push(textures[key]);
      }

      if (frames.length > 0) {
        const animSprite = new AnimatedSprite(frames);
        animSprite.anchor.set(0.5, 1);
        const scale = 44 / animSprite.texture.height;
        animSprite.scale.set(scale);
        animSprite.animationSpeed = 0.12;
        animSprite.play();
        animSprite.position.set(0, 0);
        actor.addChild(animSprite);
      } else {
        const fallback = new Text({ text: "🧑‍💼", style: new TextStyle({ fontSize: 20 }) });
        fallback.anchor.set(0.5, 1);
        actor.addChild(fallback);
      }

      const docHolder = new Container();
      const docEmoji = new Text({ text: "📋", style: new TextStyle({ fontSize: 13 }) });
      docEmoji.anchor.set(0.5, 0.5);
      docHolder.addChild(docEmoji);
      docHolder.position.set(0, -50);
      actor.addChild(docHolder);

      const badge = new Graphics();
      badge.roundRect(-16, 3, 32, 13, 4).fill({ color: 0xf59e0b, alpha: 0.9 });
      badge.roundRect(-16, 3, 32, 13, 4).stroke({ width: 1, color: 0xd97706, alpha: 0.5 });
      actor.addChild(badge);

      const badgeText = new Text({
        text: pickLocale(language, LOCALE_TEXT.collabBadge),
        style: new TextStyle({
          fontSize: 7,
          fill: 0x000000,
          fontWeight: "bold",
          fontFamily: getFontFamilyForText(
            pickLocale(language, LOCALE_TEXT.collabBadge),
            "pixel",
          ),
        }),
      });
      badgeText.anchor.set(0.5, 0.5);
      badgeText.position.set(0, 9.5);
      actor.addChild(badgeText);

      actor.position.set(fromPos.x, fromPos.y);
      dlLayer.addChild(actor);

      deliveriesRef.current.push({
        sprite: actor,
        fromX: fromPos.x,
        fromY: fromPos.y,
        toX: toPos.x,
        toY: toPos.y,
        progress: 0,
        speed: 0.005,
        type: "walk",
      });

      onCrossDeptDeliveryProcessed?.(delivery.id);
    }
  }, [
    crossDeptDeliveries,
    language,
    onCrossDeptDeliveryProcessed,
    deliveryLayerRef,
    texturesRef,
    agentPosRef,
    spriteMapRef,
    processedCrossDeptRef,
    deliveriesRef,
  ]);
}

interface CeoOfficeCallAnimationParams {
  ceoOfficeCalls?: CeoOfficeCall[];
  agents: Agent[];
  language: SupportedLocale;
  onCeoOfficeCallProcessed?: (id: string) => void;
  deliveryLayerRef: MutableRefObject<Container | null>;
  texturesRef: MutableRefObject<Record<string, Texture>>;
  ceoMeetingSeatsRef: MutableRefObject<Array<{ x: number; y: number }>>;
  deliveriesRef: MutableRefObject<Delivery[]>;
  spriteMapRef: MutableRefObject<Map<string, number>>;
  agentPosRef: MutableRefObject<Map<string, { x: number; y: number }>>;
  processedCeoOfficeRef: MutableRefObject<Set<string>>;
}

export function useCeoOfficeCallAnimations({
  ceoOfficeCalls,
  agents,
  language,
  onCeoOfficeCallProcessed,
  deliveryLayerRef,
  texturesRef,
  ceoMeetingSeatsRef,
  deliveriesRef,
  spriteMapRef,
  agentPosRef,
  processedCeoOfficeRef,
}: CeoOfficeCallAnimationParams): void {
  useEffect(() => {
    if (!ceoOfficeCalls?.length) return;
    const dlLayer = deliveryLayerRef.current;
    const textures = texturesRef.current;
    if (!dlLayer) return;

    const pickLine = (call: CeoOfficeCall) => {
      const provided = call.line?.trim();
      if (provided) return provided;
      const pool =
        call.phase === "review"
          ? pickLocale(language, LOCALE_TEXT.reviewLines)
          : pickLocale(language, LOCALE_TEXT.kickoffLines);
      return pool[hashStr(`${call.fromAgentId}-${call.id}`) % pool.length];
    };

    const renderSpeechBubble = (x: number, y: number, phase: "kickoff" | "review", line: string) => {
      const bubble = new Container();
      const bubbleText = new Text({
        text: line,
        style: new TextStyle({
          fontSize: 7,
          fill: 0x2b2b2b,
          fontFamily: getFontFamilyForText(line, "pixel"),
          wordWrap: true,
          wordWrapWidth: 120,
          breakWords: true,
        }),
      });
      bubbleText.anchor.set(0.5, 1);
      const bubbleW = Math.min(bubbleText.width + 12, 122);
      const bubbleH = bubbleText.height + 8;
      const bubbleY = -62;

      const bubbleBody = new Graphics();
      bubbleBody.roundRect(-bubbleW / 2, bubbleY - bubbleH, bubbleW, bubbleH, 4).fill(0xfff8e8);
      bubbleBody.roundRect(-bubbleW / 2, bubbleY - bubbleH, bubbleW, bubbleH, 4).stroke({
        width: 1,
        color: phase === "review" ? 0x34d399 : 0xf59e0b,
        alpha: 0.6,
      });
      bubbleBody
        .moveTo(-3, bubbleY)
        .lineTo(0, bubbleY + 4)
        .lineTo(3, bubbleY)
        .fill(0xfff8e8);
      bubble.addChild(bubbleBody);
      bubbleText.position.set(0, bubbleY - 4);
      bubble.addChild(bubbleText);

      bubble.position.set(x, y - 6);
      dlLayer.addChild(bubble);

      setTimeout(() => {
        destroyNode(bubble);
      }, 2800);
    };

    for (const call of ceoOfficeCalls) {
      if (processedCeoOfficeRef.current.has(call.id)) continue;

      if (call.action === "dismiss") {
        trackProcessedId(processedCeoOfficeRef.current, call.id);
        for (let i = deliveriesRef.current.length - 1; i >= 0; i--) {
          const delivery = deliveriesRef.current[i];
          if (delivery.agentId === call.fromAgentId && delivery.holdAtSeat) {
            destroyNode(delivery.sprite);
            deliveriesRef.current.splice(i, 1);
          }
        }
        onCeoOfficeCallProcessed?.(call.id);
        continue;
      }

      const seats = ceoMeetingSeatsRef.current;
      const seat = seats.length > 0 ? seats[call.seatIndex % seats.length] : null;
      if (!seat) continue;

      if (call.action === "speak") {
        trackProcessedId(processedCeoOfficeRef.current, call.id);
        const line = pickLine(call);
        const decision = resolveMeetingDecision(call.phase, call.decision, line);
        renderSpeechBubble(seat.x, seat.y, call.phase, line);
        if (call.phase === "review") {
          const attendee = deliveriesRef.current.find((delivery) => {
            return delivery.agentId === call.fromAgentId && delivery.holdAtSeat && !delivery.sprite.destroyed;
          });
          if (attendee) {
            attendee.meetingDecision = decision;
            if (attendee.badgeGraphics && attendee.badgeText) {
              paintMeetingBadge(attendee.badgeGraphics, attendee.badgeText, language, call.phase, decision);
            }
          }
        }
        onCeoOfficeCallProcessed?.(call.id);
        continue;
      }

      const fromPos = agentPosRef.current.get(call.fromAgentId);
      if (!fromPos) continue;

      trackProcessedId(processedCeoOfficeRef.current, call.id);
      const actor = new Container();
      const spriteNum = spriteMapRef.current.get(call.fromAgentId) ?? (hashStr(call.fromAgentId) % 13) + 1;
      const frames: Texture[] = [];

      for (let frame = 1; frame <= 3; frame++) {
        const key = `${spriteNum}-D-${frame}`;
        if (textures[key]) frames.push(textures[key]);
      }

      if (frames.length > 0) {
        const animSprite = new AnimatedSprite(frames);
        animSprite.anchor.set(0.5, 1);
        const scale = 44 / animSprite.texture.height;
        animSprite.scale.set(scale);
        animSprite.animationSpeed = 0.12;
        animSprite.play();
        actor.addChild(animSprite);
      } else {
        const fallback = new Text({ text: "🧑‍💼", style: new TextStyle({ fontSize: 20 }) });
        fallback.anchor.set(0.5, 1);
        actor.addChild(fallback);
      }

      const badge = new Graphics();
      actor.addChild(badge);
      const decision = resolveMeetingDecision(call.phase, call.decision, call.line);
      const badgeText = new Text({
        text: "",
        style: new TextStyle({ fontSize: 7, fill: 0x111111, fontWeight: "bold", fontFamily: FONT_STACK_PIXEL }),
      });
      badgeText.anchor.set(0.5, 0.5);
      badgeText.position.set(0, 10.5);
      actor.addChild(badgeText);
      paintMeetingBadge(badge, badgeText, language, call.phase, decision);

      actor.position.set(fromPos.x, fromPos.y);
      dlLayer.addChild(actor);

      for (let i = deliveriesRef.current.length - 1; i >= 0; i--) {
        const delivery = deliveriesRef.current[i];
        if (delivery.agentId !== call.fromAgentId) continue;
        destroyNode(delivery.sprite);
        deliveriesRef.current.splice(i, 1);
      }

      deliveriesRef.current.push({
        sprite: actor,
        fromX: fromPos.x,
        fromY: fromPos.y,
        toX: seat.x,
        toY: seat.y,
        progress: 0,
        speed: 0.0048,
        type: "walk",
        agentId: call.fromAgentId,
        holdAtSeat: true,
        holdUntil: call.holdUntil ?? Date.now() + 600_000,
        meetingSeatIndex: call.seatIndex,
        meetingDecision: decision,
        badgeGraphics: badge,
        badgeText,
      });

      onCeoOfficeCallProcessed?.(call.id);
    }
  }, [
    ceoOfficeCalls,
    onCeoOfficeCallProcessed,
    language,
    agents,
    deliveryLayerRef,
    texturesRef,
    ceoMeetingSeatsRef,
    deliveriesRef,
    spriteMapRef,
    agentPosRef,
    processedCeoOfficeRef,
  ]);
}
