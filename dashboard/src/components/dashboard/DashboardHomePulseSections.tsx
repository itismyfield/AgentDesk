import { useCallback, useMemo } from "react";
import type { SkillRankingResponse } from "../../api";
import { countOpenMeetingIssues } from "../../app/meetingSummary";
import type { RoundTableMeeting } from "../../types";
import { formatProviderFlow } from "../MeetingProviderFlow";
import {
  SurfaceActionButton,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import TooltipLabel from "../common/TooltipLabel";
import type { TFunction } from "./model";

export function MeetingTimelineCard({
  meetings,
  activeCount,
  followUpCount,
  localeTag,
  t,
  onOpenMeetings,
}: {
  meetings: RoundTableMeeting[];
  activeCount: number;
  followUpCount: number;
  localeTag: string;
  t: TFunction;
  onOpenMeetings?: () => void;
}) {
  const formatter = useMemo(
    () => new Intl.DateTimeFormat(localeTag, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }),
    [localeTag],
  );

  const getMeetingStatusLabel = useCallback(
    (status: RoundTableMeeting["status"]) =>
      t({
        ko: status === "in_progress" ? "진행 중" : status === "completed" ? "완료" : "초안",
        en: status === "in_progress" ? "In Progress" : status === "completed" ? "Completed" : "Draft",
        ja: status === "in_progress" ? "進行中" : status === "completed" ? "完了" : "下書き",
        zh: status === "in_progress" ? "进行中" : status === "completed" ? "已完成" : "草稿",
      }),
    [t],
  );

  return (
    <SurfaceSubsection
      title={t({ ko: "회의 타임라인", en: "Meeting Timeline", ja: "会議タイムライン", zh: "会议时间线" })}
      description={t({
        ko: `${activeCount}개 진행 중, 후속 이슈 ${followUpCount}개 미정리`,
        en: `${activeCount} active, ${followUpCount} follow-up issues still open`,
        ja: `${activeCount}件進行中、後続イシュー ${followUpCount}件 未整理`,
        zh: `${activeCount} 个进行中，${followUpCount} 个后续 issue 未整理`,
      })}
      actions={onOpenMeetings ? (
        <SurfaceActionButton tone="success" onClick={onOpenMeetings}>
          {t({ ko: "회의록 열기", en: "Open Meetings", ja: "会議録を開く", zh: "打开会议记录" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-primary) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="space-y-2">
        {meetings.length === 0 ? (
          <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
            {t({ ko: "최근 회의가 없습니다.", en: "No recent meetings yet.", ja: "最近の会議はありません。", zh: "暂无最近会议。" })}
          </SurfaceEmptyState>
        ) : (
          meetings.map((meeting) => {
            const statusTone = meeting.status === "in_progress" ? "success" : meeting.status === "completed" ? "info" : "neutral";
            const issueCount = countOpenMeetingIssues(meeting);
            return (
              <SurfaceListItem
                key={meeting.id}
                tone={statusTone}
                trailing={(
                  <div className="text-right">
                    <div className="text-xs font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {meeting.primary_provider || meeting.reviewer_provider
                        ? formatProviderFlow(meeting.primary_provider, meeting.reviewer_provider)
                        : "RT"}
                    </div>
                    <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {t({
                        ko: `${meeting.issues_created}개 생성`,
                        en: `${meeting.issues_created} created`,
                        ja: `${meeting.issues_created}件 作成`,
                        zh: `已创建 ${meeting.issues_created} 个`,
                      })}
                    </div>
                  </div>
                )}
              >
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <SurfaceMetaBadge tone={statusTone}>{getMeetingStatusLabel(meeting.status)}</SurfaceMetaBadge>
                    <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {formatter.format(meeting.started_at || meeting.created_at)}
                    </span>
                  </div>
                  <div className="mt-1 truncate font-medium" style={{ color: "var(--th-text)" }}>
                    {meeting.agenda}
                  </div>
                  <div className="mt-2 flex flex-wrap gap-2 text-[11px]">
                    <SurfaceMetaBadge>
                      {meeting.participant_names.length} {t({ ko: "참여자", en: "participants", ja: "参加者", zh: "参与者" })}
                    </SurfaceMetaBadge>
                    <SurfaceMetaBadge>
                      {meeting.total_rounds} {t({ ko: "라운드", en: "rounds", ja: "ラウンド", zh: "轮" })}
                    </SurfaceMetaBadge>
                    {issueCount > 0 ? (
                      <SurfaceMetaBadge tone="warn">
                        {issueCount} {t({ ko: "후속 대기", en: "follow-up pending", ja: "後続待ち", zh: "后续待处理" })}
                      </SurfaceMetaBadge>
                    ) : null}
                  </div>
                </div>
              </SurfaceListItem>
            );
          })
        )}
      </div>
    </SurfaceSubsection>
  );
}

export function SkillRankingSection({
  skillRanking,
  skillWindow,
  onChangeWindow,
  numberFormatter,
  localeTag,
  lastUpdatedAt,
  refreshFailed,
  t,
}: {
  skillRanking: SkillRankingResponse | null;
  skillWindow: "7d" | "30d" | "all";
  onChangeWindow: (value: "7d" | "30d" | "all") => void;
  numberFormatter: Intl.NumberFormat;
  localeTag: string;
  lastUpdatedAt: number | null;
  refreshFailed: boolean;
  t: TFunction;
}) {
  const updatedLabel = lastUpdatedAt
    ? new Intl.DateTimeFormat(localeTag, {
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
      }).format(lastUpdatedAt)
    : null;

  return (
    <SurfaceSubsection
      title={t({ ko: "스킬 랭킹", en: "Skill Ranking", ja: "スキルランキング", zh: "技能排行" })}
      description={t({
        ko: "호출량 기준 상위 스킬과 에이전트를 같은 문법으로 정리합니다.",
        en: "Top skills and agents by call volume in the same grammar.",
        ja: "呼び出し量ベースの上位スキルとエージェントを同じ文法で整理します。",
        zh: "用统一语法整理按调用量统计的技能与代理排行。",
      })}
      actions={(
        <>
          {updatedLabel ? (
            <SurfaceMetaBadge tone={refreshFailed ? "warn" : "neutral"}>
              {refreshFailed
                ? t({
                    ko: `새로고침 실패 · 마지막 ${updatedLabel}`,
                    en: `Refresh failed · last ${updatedLabel}`,
                    ja: `更新失敗 · 最終 ${updatedLabel}`,
                    zh: `刷新失败 · 最后 ${updatedLabel}`,
                  })
                : t({
                    ko: `마지막 갱신 ${updatedLabel}`,
                    en: `Last updated ${updatedLabel}`,
                    ja: `最終更新 ${updatedLabel}`,
                    zh: `最后更新 ${updatedLabel}`,
                  })}
            </SurfaceMetaBadge>
          ) : null}
          {(["7d", "30d", "all"] as const).map((windowId) => (
            <SurfaceSegmentButton
              key={windowId}
              onClick={() => onChangeWindow(windowId)}
              active={skillWindow === windowId}
              tone="warn"
            >
              {windowId}
            </SurfaceSegmentButton>
          ))}
        </>
      )}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-warn) 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-warn) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {!skillRanking || skillRanking.overall.length === 0 ? (
        <SurfaceEmptyState className="mt-4 px-4 py-6 text-center text-sm">
          {t({ ko: "아직 집계된 스킬 호출이 없습니다.", en: "No skill usage aggregated yet.", ja: "まだ集計されたスキル呼び出しがありません。", zh: "尚无技能调用统计。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="mt-4 grid gap-4 xl:grid-cols-2">
          <SkillRankingList
            title={t({ ko: "전체 TOP 5", en: "Overall TOP 5", ja: "全体 TOP 5", zh: "全体 TOP 5" })}
            emptyLabel={t({ ko: "표시할 스킬이 없습니다.", en: "No skills to show.", ja: "表示するスキルがありません。", zh: "没有可显示的技能。" })}
            t={t}
            items={skillRanking.overall.slice(0, 5).map((row, index) => ({
              id: `${row.skill_name}-${index}`,
              leading: `${index + 1}.`,
              title: row.skill_desc_ko,
              tooltip: row.skill_name,
              trailing: numberFormatter.format(row.calls),
            }))}
          />
          <SkillRankingList
            title={t({ ko: "에이전트별 TOP 5", en: "Top by Agent", ja: "エージェント別 TOP 5", zh: "按代理 TOP 5" })}
            emptyLabel={t({ ko: "표시할 에이전트 호출이 없습니다.", en: "No agent calls to show.", ja: "表示するエージェント呼び出しがありません。", zh: "没有可显示的代理调用。" })}
            t={t}
            items={skillRanking.byAgent.slice(0, 5).map((row, index) => ({
              id: `${row.agent_role_id}-${row.skill_name}-${index}`,
              leading: `${index + 1}.`,
              title: `${row.agent_name} · ${row.skill_desc_ko}`,
              tooltip: row.skill_name,
              trailing: numberFormatter.format(row.calls),
            }))}
          />
        </div>
      )}
    </SurfaceSubsection>
  );
}

export function SkillRankingList({
  title,
  emptyLabel,
  items,
  t,
}: {
  title: string;
  emptyLabel: string;
  items: Array<{
    id: string;
    leading: string;
    title: string;
    tooltip: string;
    trailing: string;
  }>;
  t: TFunction;
}) {
  return (
    <div className="min-w-0">
      <div className="mb-2 text-sm font-medium" style={{ color: "var(--th-text-muted)" }}>
        {title}
      </div>
      {items.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {emptyLabel}
        </SurfaceEmptyState>
      ) : (
        <ul className="space-y-2">
          {items.map((item) => (
            <li key={item.id}>
              <SurfaceListItem
                tone="warn"
                trailing={(
                  <span className="text-sm font-semibold" style={{ color: "var(--th-accent-warn)" }}>
                    {item.trailing}
                  </span>
                )}
              >
                <div className="min-w-0 flex flex-1 items-start gap-2 text-sm" style={{ color: "var(--th-text)" }}>
                  <span className="inline-flex w-6 shrink-0" style={{ color: "var(--th-text-muted)" }}>
                    {item.leading}
                  </span>
                  <TooltipLabel text={item.title} tooltip={item.tooltip} className="flex-1" />
                </div>
              </SurfaceListItem>
            </li>
          ))}
        </ul>
      )}
      <div className="mt-2 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
        {t({ ko: "집계 창을 바꾸면 같은 카드 안에서 즉시 다시 계산됩니다.", en: "Changing the window recalculates in place.", ja: "ウィンドウを変えると同じカード内で再計算されます。", zh: "切换窗口后会在同一卡片内重新计算。" })}
      </div>
    </div>
  );
}
