import { describe, expect, it } from "vitest";
import type { CommandMenuGroup } from "../kit/pro/command-menu";
import { filterCommandGroups } from "./commandPaletteSearch";

const groups: CommandMenuGroup[] = [
  { id: "nav", label: "이동", commands: [
    { id: "/settings", label: "설정", keywords: ["/settings"] },
    { id: "/stats", label: "통계", keywords: ["/stats"] },
  ] },
  { id: "dept", label: "부서", commands: [{ id: "dept-platform", label: "플랫폼", keywords: ["Platform"] }] },
];
const ids = (result: CommandMenuGroup[]) => result.flatMap((group) => group.commands.map((command) => command.id));

describe("filterCommandGroups", () => {
  it("keeps the old palette's abbreviations and ranks the tighter match first", () => {
    expect(ids(filterCommandGroups(groups, "sts"))).toEqual(["/stats", "/settings"]);
  });

  it("matches a half-typed Hangul syllable and puts the matching group first", () => {
    expect(ids(filterCommandGroups(groups, "플래"))).toEqual(["dept-platform"]);
  });

  it("hands every kept row the query so the kit's substring filter keeps it", () => {
    const [nav] = filterCommandGroups(groups, " sts ");
    expect(nav.commands.every((command) => (command.keywords as string[]).includes("sts"))).toBe(true);
    expect(filterCommandGroups(groups, "zz")).toEqual([]);
    expect(filterCommandGroups(groups, " ")).toBe(groups);
  });
});
