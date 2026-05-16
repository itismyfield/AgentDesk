import type { Agent } from "../../types";
import type { AchievementCardModel } from "../achievementsModel";
import { AchievementCard } from "./AchievementCard";

type AchievementSectionKey = "earned" | "progress" | "locked";

interface AchievementSection {
  key: AchievementSectionKey;
  title: string;
  items: AchievementCardModel[];
}

interface AchievementsSectionsProps {
  sections: AchievementSection[];
  isKo: boolean;
  progressLabel: string;
  agents: Agent[];
  onOpen: (item: AchievementCardModel) => void;
}

export function AchievementsSections({
  sections,
  isKo,
  progressLabel,
  agents,
  onOpen,
}: AchievementsSectionsProps) {
  return (
    <>
      {sections.map((section) => (
        <section key={section.key} data-testid={`achievements-section-${section.key}`}>
          <div className="mb-3 flex items-center justify-between gap-3">
            <div
              className="text-[13px] font-semibold"
              style={{ color: "var(--th-text-heading, var(--fg))" }}
            >
              {section.title}{" "}
              <span style={{ color: "var(--th-text-muted, var(--fg-muted))", fontWeight: 400 }}>
                {section.items.length}
              </span>
            </div>
          </div>
          {section.items.length === 0 ? (
            <div
              className="rounded-[1.15rem] border px-4 py-5 text-sm"
              style={{
                borderColor: "var(--th-border-subtle, var(--line))",
                color: "var(--th-text-muted, var(--fg-muted))",
                background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
              }}
            >
              {section.key === "earned"
                ? isKo
                  ? "아직 집계된 업적이 없습니다."
                  : "No unlocked achievements have been recorded yet."
                : section.key === "progress"
                  ? isKo
                    ? "지금 진행 중인 업적 후보가 없습니다."
                    : "No active milestone candidates right now."
                  : isKo
                    ? "현재 잠긴 업적이 없습니다."
                    : "No locked milestones remain."}
            </div>
          ) : (
            <div className="achievements-grid" data-testid={`achievements-grid-${section.key}`}>
              {section.items.map((item) => (
                <AchievementCard
                  key={item.id}
                  item={item}
                  isKo={isKo}
                  progressLabel={progressLabel}
                  agents={agents}
                  onOpen={onOpen}
                />
              ))}
            </div>
          )}
        </section>
      ))}
    </>
  );
}
