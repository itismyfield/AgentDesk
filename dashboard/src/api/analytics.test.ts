import { afterEach, describe, expect, it, vi } from "vitest";

import { formatAchievementDate } from "../components/achievementsModel";
import { getAchievements } from "./analytics";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("achievement date normalization", () => {
  it("preserves missing dates for numeric zero and the legacy epoch timestamp", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(
          JSON.stringify({
            achievements: [
              { id: "numeric-zero", earned_at: 0 },
              { id: "legacy-epoch", earned_at: "1970-01-01T00:00:00Z" },
            ],
            daily_missions: [],
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        ),
      ),
    );

    const response = await getAchievements();

    expect(response.achievements.map((achievement) => achievement.earned_at)).toEqual([
      0,
      0,
    ]);
    for (const achievement of response.achievements) {
      expect(formatAchievementDate(achievement.earned_at, "ko-KR")).toBeUndefined();
    }
  });
});
