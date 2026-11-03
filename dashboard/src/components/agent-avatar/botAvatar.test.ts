import { describe, expect, it } from "vitest";
import { examples, validateAvatar } from "open-bot-avatar";
import { AVATAR_PALETTE, AVATAR_SHAPES, avatarRoster, botAvatarFor, botAvatarStill } from "./botAvatar";
import { GALLERY_AVATARS } from "./galleryAvatars";

const lookOf = (avatar: ReturnType<typeof botAvatarFor>) => `${avatar.definition.id}|${avatar.color}`;

describe("bot avatars", () => {
  it("mixes the package templates with round bodies and polygons, all valid and distinct", () => {
    expect(AVATAR_SHAPES).toHaveLength(examples.length + GALLERY_AVATARS.length + 6 + 10);
    for (const shape of AVATAR_SHAPES) expect(validateAvatar(shape)).toMatchObject({ ok: true });
    expect(new Set(AVATAR_SHAPES.map((shape) => shape.id)).size).toBe(AVATAR_SHAPES.length);
    expect(new Set(AVATAR_PALETTE.map(([body]) => body)).size).toBe(AVATAR_PALETTE.length);
  });

  it("picks a stable look from the id and spreads ids over every shape and color", () => {
    expect(botAvatarFor("ch-td")).toEqual(botAvatarFor("ch-td"));
    const looks = Array.from({ length: 2000 }, (_, i) => botAvatarFor(`agent-${i}`));
    expect(new Set(looks.map((avatar) => avatar.definition.id)).size).toBe(AVATAR_SHAPES.length);
    expect(new Set(looks.map((avatar) => avatar.color)).size).toBe(AVATAR_PALETTE.length);
  });

  it("gives every roster agent its own look, moving only the newer agent of a clash", () => {
    const agents = Array.from({ length: 120 }, (_, i) => ({ id: `agent-${i}`, created_at: i }));
    const roster = avatarRoster(agents);
    expect(new Set([...roster.values()].map(lookOf)).size).toBe(agents.length);

    const [older, newer] = agents
      .flatMap((a, i) => agents.slice(i + 1).filter((b) => lookOf(botAvatarFor(a.id)) === lookOf(botAvatarFor(b.id))).map((b) => [a, b]))[0];
    const pair = avatarRoster([newer, older]);
    expect(lookOf(pair.get(older.id)!)).toBe(lookOf(botAvatarFor(older.id)));
    expect(pair.get(newer.id)!.definition).toBe(botAvatarFor(newer.id).definition);
    expect(pair.get(newer.id)!.color).not.toBe(botAvatarFor(newer.id).color);
    expect(avatarRoster(agents)).toBe(roster);
    expect(botAvatarFor("not-listed", roster)).toEqual(botAvatarFor("not-listed"));
  });

  it("renders a cached, open-eyed SVG still in the avatar's colors", () => {
    const avatar = { definition: AVATAR_SHAPES[0], color: "#2563eb", eyeColor: "#ffffff" };
    const url = botAvatarStill(avatar);
    expect(url.startsWith("data:image/svg+xml;charset=utf-8,")).toBe(true);
    expect(decodeURIComponent(url)).toContain('fill="#2563eb"');
    expect(decodeURIComponent(url)).toContain("#ffffff");
    expect(botAvatarStill(avatar)).toBe(url);
  });
});
