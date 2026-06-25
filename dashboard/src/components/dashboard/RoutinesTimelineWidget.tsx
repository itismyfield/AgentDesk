import { useEffect, useMemo, useState } from "react";
import { CalendarClock, ChevronRight, RefreshCw } from "lucide-react";
import {
  getRoutineRuns,
  getRoutines,
  type RoutineRecord,
  type RoutineRunRecord,
  type RoutineStatus,
} from "../../api";
import {
  SurfaceActionButton,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
  SurfaceMetricPill,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import { Drawer } from "../common/overlay/Drawer";
import type { TFunction } from "./model";
import { cx } from "./ui";

type RoutineFilter = "all" | RoutineStatus;

const FILTERS: RoutineFilter[] = ["all", "enabled", "paused", "detached"];

function parseTime(value: string | null | undefined): number | null {
  if (!value) return null;
  const time = Date.parse(value);
  return Number.isFinite(time) ? time : null;
}

function compareMaybeTime(left: number | null, right: number | null): number {
  if (left == null && right == null) return 0;
  if (left == null) return 1;
  if (right == null) return -1;
  return left - right;
}

export function sortRoutinesChronologically(
  routines: RoutineRecord[],
): RoutineRecord[] {
  return [...routines].sort((left, right) => {
    const dueCompare = compareMaybeTime(
      parseTime(left.next_due_at),
      parseTime(right.next_due_at),
    );
    if (dueCompare !== 0) return dueCompare;

    const lastRunCompare = compareMaybeTime(
      parseTime(right.last_run_at),
      parseTime(left.last_run_at),
    );
    if (lastRunCompare !== 0) return lastRunCompare;

    return left.name.localeCompare(right.name);
  });
}

function pad2(value: number): string {
  return value.toString().padStart(2, "0");
}

export function describeRoutineSchedule(
  schedule: string | null,
  language: "ko" | "en" | "ja" | "zh",
): string {
  const trimmed = schedule?.trim();
  if (!trimmed) {
    return language === "ko" ? "수동 실행" : "Manual run";
  }

  const every = trimmed.match(/^@every\s+(\d+)(ms|s|m|h|d)$/i);
  if (every) {
    const value = Number(every[1]);
    const unit = every[2].toLowerCase();
    const label =
      unit === "d"
        ? language === "ko"
          ? "일"
          : "d"
        : unit === "h"
          ? language === "ko"
            ? "시간"
            : "h"
          : unit === "m"
            ? language === "ko"
              ? "분"
              : "m"
            : unit;
    return language === "ko" ? `${value}${label}마다` : `Every ${value}${label}`;
  }

  const parts = trimmed.split(/\s+/);
  if (parts.length === 5) {
    const [minute, hour, dayOfMonth, month, dayOfWeek] = parts;
    const hourNum = Number(hour);
    const minuteNum = Number(minute);
    if (
      Number.isInteger(hourNum) &&
      Number.isInteger(minuteNum) &&
      hourNum >= 0 &&
      hourNum <= 23 &&
      minuteNum >= 0 &&
      minuteNum <= 59 &&
      dayOfMonth === "*" &&
      month === "*"
    ) {
      const clock = `${pad2(hourNum)}:${pad2(minuteNum)}`;
      if (dayOfWeek === "*") {
        return language === "ko" ? `매일 ${clock}` : `Daily ${clock}`;
      }
      if (dayOfWeek === "1-5") {
        return language === "ko" ? `평일 ${clock}` : `Weekdays ${clock}`;
      }
    }
  }

  return trimmed;
}

function statusTone(routine: RoutineRecord): "info" | "success" | "warn" | "neutral" | "danger" {
  if (routine.in_flight_run_id) return "info";
  if (routine.status === "enabled") return "success";
  if (routine.status === "paused") return "warn";
  if (routine.status === "detached") return "neutral";
  return "danger";
}

function statusLabel(routine: RoutineRecord, t: TFunction): string {
  if (routine.in_flight_run_id) {
    return t({ ko: "진행 중", en: "Running", ja: "実行中", zh: "运行中" });
  }
  if (routine.status === "enabled") {
    return t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" });
  }
  if (routine.status === "paused") {
    return t({ ko: "일시정지", en: "Paused", ja: "一時停止", zh: "已暂停" });
  }
  if (routine.status === "detached") {
    return t({ ko: "분리됨", en: "Detached", ja: "切り離し", zh: "已分离" });
  }
  return routine.status;
}

function formatDateTime(value: string | null, localeTag: string): string {
  const time = parseTime(value);
  if (time == null) return "-";
  return new Date(time).toLocaleString(localeTag, {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function formatRelative(value: string | null, localeTag: string): string | null {
  const time = parseTime(value);
  if (time == null) return null;
  const diffMs = time - Date.now();
  const absMs = Math.abs(diffMs);
  const formatter = new Intl.RelativeTimeFormat(localeTag, { numeric: "auto" });
  if (absMs < 60_000) return formatter.format(Math.round(diffMs / 1_000), "second");
  if (absMs < 3_600_000) return formatter.format(Math.round(diffMs / 60_000), "minute");
  if (absMs < 86_400_000) return formatter.format(Math.round(diffMs / 3_600_000), "hour");
  return formatter.format(Math.round(diffMs / 86_400_000), "day");
}

function filterLabel(filter: RoutineFilter, t: TFunction): string {
  switch (filter) {
    case "enabled":
      return t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" });
    case "paused":
      return t({ ko: "일시정지", en: "Paused", ja: "一時停止", zh: "已暂停" });
    case "detached":
      return t({ ko: "분리", en: "Detached", ja: "切り離し", zh: "分离" });
    case "all":
    default:
      return t({ ko: "전체", en: "All", ja: "すべて", zh: "全部" });
  }
}

function includesAny(value: string, terms: string[]): boolean {
  return terms.some((term) => value.includes(term));
}

function humanizeRoutineName(name: string): string {
  return name
    .replace(/\.(js|ts|sh)$/i, "")
    .split(/[-_/]+/)
    .filter(Boolean)
    .join(" ");
}

export function describeRoutinePurpose(routine: RoutineRecord, t: TFunction): string {
  const haystack = `${routine.name} ${routine.script_ref}`.toLowerCase();
  if (includesAny(haystack, ["family-morning-briefing"])) {
    return t({
      ko: "날씨, 캘린더, 리마인더를 모아 가족 아침 브리핑을 보냅니다.",
      en: "Sends a family morning briefing from weather, calendar, and reminders.",
      ja: "天気、カレンダー、リマインダーをまとめて朝のブリーフィングを送信します。",
      zh: "汇总天气、日历和提醒事项并发送家庭晨报。",
    });
  }
  if (includesAny(haystack, ["family-profile-probe"])) {
    return t({
      ko: "가족 프로필의 빈 정보를 찾아 DM 질문으로 보강합니다.",
      en: "Fills missing family profile details through DM prompts.",
      ja: "家族プロフィールの不足情報をDM質問で補完します。",
      zh: "通过 DM 提问补全家庭档案中的缺失信息。",
    });
  }
  if (includesAny(haystack, ["token-daily-report"])) {
    return t({
      ko: "토큰 사용량과 비용을 일일 리포트로 정리합니다.",
      en: "Builds a daily token usage and cost report.",
      ja: "トークン使用量とコストを日次レポートにまとめます。",
      zh: "生成每日 token 使用量和成本报告。",
    });
  }
  if (includesAny(haystack, ["memento-hygiene"])) {
    return t({
      ko: "Memento 기억을 위생 정리하고 오래된 파편을 관리합니다.",
      en: "Maintains Memento memory hygiene and stale fragments.",
      ja: "Mementoメモリの衛生管理と古いフラグメント整理を行います。",
      zh: "维护 Memento 记忆卫生并管理过期片段。",
    });
  }
  if (includesAny(haystack, ["memento-scope-audit"])) {
    return t({
      ko: "Memento 파편의 스코프와 고정 상태를 주기적으로 감사합니다.",
      en: "Audits Memento fragment scope and anchoring on a schedule.",
      ja: "Mementoフラグメントのスコープと固定状態を定期監査します。",
      zh: "定期审计 Memento 片段的范围和固定状态。",
    });
  }
  if (includesAny(haystack, ["worktree", "janitor", "local-worktree-gc"])) {
    return t({
      ko: "오래된 로컬 worktree와 임시 빌드 캐시를 안전 기준에 맞춰 정리합니다.",
      en: "Cleans stale local worktrees and temporary build caches with safety gates.",
      ja: "古いローカルworktreeと一時ビルドキャッシュを安全条件付きで整理します。",
      zh: "按安全规则清理过期本地 worktree 和临时构建缓存。",
    });
  }
  if (includesAny(haystack, ["dependency-update"])) {
    return t({
      ko: "Agent CLI, 핵심 crate, CVE 등 외부 의존성 변경을 추적합니다.",
      en: "Tracks external dependency updates across Agent CLIs, core crates, and CVEs.",
      ja: "Agent CLI、主要crate、CVEなど外部依存関係の更新を追跡します。",
      zh: "跟踪 Agent CLI、核心 crate 和 CVE 等外部依赖更新。",
    });
  }
  if (includesAny(haystack, ["agent-feedback-briefing"])) {
    return t({
      ko: "에이전트 피드백과 품질 신호를 모아 브리핑합니다.",
      en: "Summarizes agent feedback and quality signals.",
      ja: "エージェントのフィードバックと品質シグナルを要約します。",
      zh: "汇总智能体反馈和质量信号。",
    });
  }
  if (includesAny(haystack, ["banchan-day-reminder-prep"])) {
    return t({
      ko: "반찬데이 전날 장보기와 준비 알림을 보냅니다.",
      en: "Sends grocery and prep reminders before banchan day.",
      ja: "惣菜デー前日の買い物と準備リマインダーを送信します。",
      zh: "在配菜日前发送采购和准备提醒。",
    });
  }
  if (includesAny(haystack, ["banchan-day-reminder-cook"])) {
    return t({
      ko: "반찬데이 당일 조리 알림을 보냅니다.",
      en: "Sends cooking reminders on banchan day.",
      ja: "惣菜デー当日の調理リマインダーを送信します。",
      zh: "在配菜日当天发送烹饪提醒。",
    });
  }
  if (includesAny(haystack, ["ai-integrated-briefing"])) {
    return t({
      ko: "AI 제품/릴리즈 뉴스를 통합 브리핑으로 정리합니다.",
      en: "Builds an integrated briefing of AI product and release news.",
      ja: "AI製品とリリースニュースを統合ブリーフィングにまとめます。",
      zh: "整理 AI 产品和发布新闻的综合简报。",
    });
  }
  if (includesAny(haystack, ["cookingheart-daily-briefing"])) {
    return t({
      ko: "CookingHeart 개발 현황과 다음 액션을 일일 브리핑합니다.",
      en: "Sends a daily CookingHeart development briefing and next actions.",
      ja: "CookingHeart開発状況と次のアクションを日次ブリーフィングします。",
      zh: "发送 CookingHeart 开发状态和下一步行动日报。",
    });
  }
  if (includesAny(haystack, ["queue-stability"])) {
    return t({
      ko: "큐 안정성 신호를 점검하고 필요한 복구 작업을 묶어 실행합니다.",
      en: "Checks queue stability signals and batches needed recovery work.",
      ja: "キュー安定性シグナルを確認し、必要な復旧作業をまとめます。",
      zh: "检查队列稳定性信号并批量执行必要恢复工作。",
    });
  }
  if (includesAny(haystack, ["memory-merge"])) {
    return t({
      ko: "에이전트 메모리 파일을 정리하고 장기 기억 계층으로 분배합니다.",
      en: "Cleans agent memory files and distributes them into long-term memory layers.",
      ja: "エージェントメモリファイルを整理し長期記憶層へ分配します。",
      zh: "整理智能体记忆文件并分配到长期记忆层。",
    });
  }
  if (includesAny(haystack, ["automation-candidate"])) {
    return t({
      ko: "자동화 후보를 감지, 추천, 또는 실행하는 모니터링 루틴입니다.",
      en: "Detects, recommends, or executes automation candidates.",
      ja: "自動化候補を検出、推薦、または実行する監視ルーチンです。",
      zh: "用于检测、推荐或执行自动化候选项的监控例程。",
    });
  }
  if (includesAny(haystack, ["working-watchdog"])) {
    return t({
      ko: "작업 중인 에이전트/세션 상태를 감시하고 이상 신호를 찾습니다.",
      en: "Watches active agent and session health for anomalies.",
      ja: "稼働中のエージェントとセッション状態を監視します。",
      zh: "监控运行中的智能体和会话健康状态。",
    });
  }

  const owner = routine.agent_id ?? routine.fallback_agent_id;
  const readableName = humanizeRoutineName(routine.name || routine.script_ref);
  return owner
    ? t({
        ko: `${readableName} 작업을 ${owner}가 정해진 시간에 실행합니다.`,
        en: `${readableName} is run by ${owner} on its schedule.`,
        ja: `${readableName} を ${owner} がスケジュールに沿って実行します。`,
        zh: `${readableName} 由 ${owner} 按计划执行。`,
      })
    : t({
        ko: `${readableName} 작업을 정해진 시간에 실행합니다.`,
        en: `${readableName} runs on its schedule.`,
        ja: `${readableName} をスケジュールに沿って実行します。`,
        zh: `${readableName} 按计划执行。`,
      });
}

function previewValue(value: unknown, fallback = "-"): string {
  if (value === null || value === undefined || value === "") return fallback;
  const raw = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  if (!raw) return fallback;
  return raw.length > 520 ? `${raw.slice(0, 520)}...` : raw;
}

function DetailField({
  label,
  value,
  mono = false,
}: {
  label: string;
  value: string | number | null | undefined;
  mono?: boolean;
}) {
  return (
    <div className="min-w-0 rounded-xl border px-3 py-2" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)" }}>
      <div className="text-[10px] font-semibold uppercase tracking-[0.12em]" style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className={cx("mt-1 min-w-0 break-words text-sm", mono ? "font-mono text-xs leading-5" : "font-medium")} style={{ color: "var(--th-text-primary)" }}>
        {value ?? "-"}
      </div>
    </div>
  );
}

function RoutineDetailDrawer({
  routine,
  runs,
  runsLoading,
  runsError,
  t,
  localeTag,
  language,
  onClose,
}: {
  routine: RoutineRecord | null;
  runs: RoutineRunRecord[];
  runsLoading: boolean;
  runsError: boolean;
  t: TFunction;
  localeTag: string;
  language: "ko" | "en" | "ja" | "zh";
  onClose: () => void;
}) {
  const purpose = routine ? describeRoutinePurpose(routine, t) : "";

  return (
    <Drawer
      open={Boolean(routine)}
      onClose={onClose}
      width="min(560px, 100vw)"
      title={routine?.name ?? t({ ko: "루틴 상세", en: "Routine detail", ja: "ルーチン詳細", zh: "例程详情" })}
    >
      {routine ? (
        <div className="space-y-5">
          <section>
            <div className="flex flex-wrap items-center gap-2">
              <SurfaceMetaBadge tone={statusTone(routine)}>
                {statusLabel(routine, t)}
              </SurfaceMetaBadge>
              <SurfaceMetaBadge tone="neutral">
                {describeRoutineSchedule(routine.schedule, language)}
              </SurfaceMetaBadge>
            </div>
            <p className="mt-3 text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
              {purpose}
            </p>
          </section>

          <section className="grid gap-2 sm:grid-cols-2">
            <DetailField label={t({ ko: "다음 실행", en: "Next run", ja: "次回実行", zh: "下次运行" })} value={formatDateTime(routine.next_due_at, localeTag)} />
            <DetailField label={t({ ko: "최근 실행", en: "Last run", ja: "前回実行", zh: "上次运行" })} value={formatDateTime(routine.last_run_at, localeTag)} />
            <DetailField label={t({ ko: "담당", en: "Agent", ja: "担当", zh: "负责" })} value={routine.agent_id ?? "-"} />
            <DetailField label={t({ ko: "fallback", en: "Fallback", ja: "fallback", zh: "fallback" })} value={routine.fallback_agent_id ?? "-"} />
            <DetailField label={t({ ko: "실행 방식", en: "Strategy", ja: "実行方式", zh: "执行方式" })} value={routine.execution_strategy} />
            <DetailField label={t({ ko: "timeout", en: "Timeout", ja: "timeout", zh: "timeout" })} value={routine.timeout_secs ? `${routine.timeout_secs}s` : "-"} />
            <DetailField label={t({ ko: "재시도", en: "Retries", ja: "リトライ", zh: "重试" })} value={routine.max_retries ?? "-"} />
            <DetailField label={t({ ko: "pause reason", en: "Pause reason", ja: "pause reason", zh: "pause reason" })} value={routine.pause_reason ?? "-"} />
          </section>

          <section className="space-y-2">
            <DetailField label="script_ref" value={routine.script_ref} mono />
            <DetailField label="routine_id" value={routine.id} mono />
            <DetailField label="discord_thread_id" value={routine.discord_thread_id ?? "-"} mono />
            <DetailField label={t({ ko: "최근 결과", en: "Last result", ja: "前回結果", zh: "最近结果" })} value={previewValue(routine.last_result)} mono />
            <DetailField label="checkpoint" value={previewValue(routine.checkpoint)} mono />
          </section>

          <section>
            <h3 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {t({ ko: "최근 실행", en: "Recent runs", ja: "最近の実行", zh: "最近运行" })}
            </h3>
            <div className="mt-2 space-y-2">
              {runsLoading ? (
                <div className="rounded-xl border px-3 py-4 text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)" }}>
                  {t({ ko: "실행 기록을 불러오는 중...", en: "Loading runs...", ja: "実行履歴を読み込み中...", zh: "正在加载运行记录..." })}
                </div>
              ) : runsError ? (
                <div className="rounded-xl border px-3 py-4 text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)" }}>
                  {t({ ko: "실행 기록을 불러오지 못했습니다.", en: "Runs could not be loaded.", ja: "実行履歴を読み込めませんでした。", zh: "无法加载运行记录。" })}
                </div>
              ) : runs.length === 0 ? (
                <div className="rounded-xl border px-3 py-4 text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)" }}>
                  {t({ ko: "최근 실행 기록이 없습니다.", en: "No recent runs.", ja: "最近の実行履歴はありません。", zh: "没有最近运行记录。" })}
                </div>
              ) : (
                runs.slice(0, 8).map((run) => (
                  <div key={run.id} className="rounded-xl border px-3 py-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)" }}>
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <SurfaceMetaBadge tone={run.status === "succeeded" ? "success" : run.status === "running" ? "info" : run.status === "failed" ? "danger" : "neutral"}>
                        {run.status}
                      </SurfaceMetaBadge>
                      <span className="text-xs tabular-nums" style={{ color: "var(--th-text-muted)" }}>
                        {formatDateTime(run.started_at, localeTag)}
                      </span>
                    </div>
                    <div className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                      {run.action ? `${t({ ko: "action", en: "action", ja: "action", zh: "action" })}: ${run.action}` : null}
                      {run.finished_at ? ` · ${t({ ko: "완료", en: "finished", ja: "完了", zh: "完成" })} ${formatDateTime(run.finished_at, localeTag)}` : null}
                    </div>
                    {(run.error || run.result_json) ? (
                      <pre className="mt-2 max-h-36 overflow-auto whitespace-pre-wrap rounded-lg px-2 py-2 text-[11px] leading-5" style={{ background: "color-mix(in srgb, var(--th-overlay-medium) 82%, transparent)", color: "var(--th-text-primary)" }}>
                        {run.error ?? previewValue(run.result_json)}
                      </pre>
                    ) : null}
                  </div>
                ))
              )}
            </div>
          </section>
        </div>
      ) : null}
    </Drawer>
  );
}

interface RoutinesTimelineWidgetProps {
  t: TFunction;
  localeTag: string;
  language: "ko" | "en" | "ja" | "zh";
}

export function RoutinesTimelineWidget({
  t,
  localeTag,
  language,
}: RoutinesTimelineWidgetProps) {
  const [filter, setFilter] = useState<RoutineFilter>("all");
  const [routines, setRoutines] = useState<RoutineRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const [selectedRoutineId, setSelectedRoutineId] = useState<string | null>(null);
  const [selectedRuns, setSelectedRuns] = useState<RoutineRunRecord[]>([]);
  const [runsLoading, setRunsLoading] = useState(false);
  const [runsError, setRunsError] = useState(false);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      setError(false);
      setLoading(true);
      try {
        const next = await getRoutines(
          filter === "all" ? undefined : { status: filter },
        );
        if (!mounted) return;
        setRoutines(next);
      } catch {
        if (mounted) setError(true);
      } finally {
        if (mounted) setLoading(false);
      }
    };

    void load();
    const timer = window.setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      window.clearInterval(timer);
    };
  }, [filter, reloadKey]);

  const sortedRoutines = useMemo(
    () => sortRoutinesChronologically(routines),
    [routines],
  );
  const activeCount = useMemo(
    () => routines.filter((routine) => routine.status === "enabled").length,
    [routines],
  );
  const runningCount = useMemo(
    () => routines.filter((routine) => Boolean(routine.in_flight_run_id)).length,
    [routines],
  );
  const nextRoutine = sortedRoutines.find((routine) => routine.next_due_at);
  const selectedRoutine = selectedRoutineId
    ? routines.find((routine) => routine.id === selectedRoutineId) ?? null
    : null;

  useEffect(() => {
    if (!selectedRoutineId) {
      setSelectedRuns([]);
      setRunsError(false);
      setRunsLoading(false);
      return;
    }
    let mounted = true;
    setRunsLoading(true);
    setRunsError(false);
    void getRoutineRuns(selectedRoutineId, 8)
      .then((runs) => {
        if (mounted) setSelectedRuns(runs);
      })
      .catch(() => {
        if (mounted) setRunsError(true);
      })
      .finally(() => {
        if (mounted) setRunsLoading(false);
      });
    return () => {
      mounted = false;
    };
  }, [selectedRoutineId]);

  return (
    <>
      <SurfaceSubsection
        data-testid="routines-timeline"
        title={t({ ko: "루틴 시간표", en: "Routines Timeline", ja: "ルーチン時系列", zh: "例程时间线" })}
        description={t({
          ko: "등록된 루틴을 다음 실행 시간 기준으로 정렬해 보여줍니다.",
          en: "Registered routines are sorted by their next run time.",
          ja: "登録済みルーチンを次回実行時刻順に表示します。",
          zh: "按下一次运行时间排序显示已注册例程。",
        })}
        actions={(
          <SurfaceActionButton
            compact
            tone="neutral"
            onClick={() => setReloadKey((value) => value + 1)}
            aria-label={t({ ko: "루틴 새로고침", en: "Refresh routines", ja: "ルーチンを再読み込み", zh: "刷新例程" })}
          >
            <RefreshCw size={12} className={cx(loading ? "animate-spin" : "")} />
          </SurfaceActionButton>
        )}
        style={{
          borderColor: "color-mix(in srgb, var(--th-accent-info) 24%, var(--th-border) 76%)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)",
        }}
      >
      <div className="grid gap-3 sm:grid-cols-3">
        <SurfaceMetricPill
          label={t({ ko: "등록", en: "Registered", ja: "登録", zh: "已注册" })}
          value={routines.length}
          tone="info"
          className="w-full"
        />
        <SurfaceMetricPill
          label={t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" })}
          value={activeCount}
          tone="success"
          className="w-full"
        />
        <SurfaceMetricPill
          label={t({ ko: "다음", en: "Next", ja: "次回", zh: "下次" })}
          value={nextRoutine ? formatDateTime(nextRoutine.next_due_at, localeTag) : "-"}
          tone={runningCount > 0 ? "warn" : "neutral"}
          className="w-full"
        />
      </div>

      <div
        className="mt-4 flex gap-2 overflow-x-auto pb-1"
        data-testid="routines-filter-controls"
      >
        {FILTERS.map((item) => (
          <SurfaceSegmentButton
            key={item}
            active={filter === item}
            onClick={() => setFilter(item)}
            aria-pressed={filter === item}
          >
            {filterLabel(item, t)}
          </SurfaceSegmentButton>
        ))}
      </div>

      {error ? (
        <SurfaceEmptyState className="mt-4 px-4 py-6 text-center text-sm">
          {t({
            ko: "루틴 목록을 불러오지 못했습니다.",
            en: "Routines could not be loaded.",
            ja: "ルーチン一覧を読み込めませんでした。",
            zh: "无法加载例程列表。",
          })}
        </SurfaceEmptyState>
      ) : loading && routines.length === 0 ? (
        <div className="mt-4 space-y-2" data-testid="routines-loading">
          {Array.from({ length: 3 }).map((_, index) => (
            <div
              key={index}
              className="h-20 animate-pulse rounded-2xl border"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 62%, transparent)",
                background: "color-mix(in srgb, var(--th-card-bg) 86%, transparent)",
              }}
            />
          ))}
        </div>
      ) : sortedRoutines.length === 0 ? (
        <SurfaceEmptyState className="mt-4 px-4 py-8 text-center text-sm">
          {t({
            ko: "표시할 루틴이 없습니다.",
            en: "No routines to show.",
            ja: "表示するルーチンがありません。",
            zh: "没有可显示的例程。",
          })}
        </SurfaceEmptyState>
      ) : (
        <div className="mt-4 space-y-2" data-testid="routines-timeline-list">
          {sortedRoutines.map((routine) => {
            const relative = formatRelative(routine.next_due_at, localeTag);
            const lastRunLabel = formatDateTime(routine.last_run_at, localeTag);
            const purpose = describeRoutinePurpose(routine, t);
            return (
              <button
                key={routine.id}
                type="button"
                data-testid={`routine-row-${routine.id}`}
                className="block w-full rounded-2xl text-left transition hover:brightness-[1.02] focus:outline-none focus:ring-2 focus:ring-[var(--th-accent-primary)] focus:ring-offset-2 focus:ring-offset-[var(--th-bg-surface)]"
                onClick={() => setSelectedRoutineId(routine.id)}
                aria-label={t({
                  ko: `${routine.name} 상세 열기`,
                  en: `Open ${routine.name} details`,
                  ja: `${routine.name} の詳細を開く`,
                  zh: `打开 ${routine.name} 详情`,
                })}
              >
                <SurfaceListItem
                  tone={statusTone(routine)}
                  className="min-w-0"
                  trailing={(
                    <div className="flex min-w-[7.5rem] flex-col items-end gap-1 text-right">
                      <div
                        className="text-sm font-semibold tabular-nums"
                        style={{ color: "var(--th-text-heading)" }}
                      >
                        {formatDateTime(routine.next_due_at, localeTag)}
                      </div>
                      <div className="max-w-[9rem] truncate text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                        {relative ?? t({ ko: "수동", en: "Manual", ja: "手動", zh: "手动" })}
                      </div>
                      <ChevronRight size={14} style={{ color: "var(--th-text-muted)" }} />
                    </div>
                  )}
                >
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <CalendarClock size={14} style={{ color: "var(--th-accent-info)" }} />
                      <div
                        className="min-w-0 max-w-full truncate text-sm font-semibold"
                        style={{ color: "var(--th-text-heading)" }}
                      >
                        {routine.name}
                      </div>
                      <SurfaceMetaBadge tone={statusTone(routine)}>
                        {statusLabel(routine, t)}
                      </SurfaceMetaBadge>
                    </div>
                    <p className="mt-2 line-clamp-2 text-xs leading-5 sm:text-[13px]" style={{ color: "var(--th-text-primary)" }}>
                      {purpose}
                    </p>
                    <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                      <span className="min-w-0 max-w-full truncate">
                        {describeRoutineSchedule(routine.schedule, language)}
                      </span>
                      {routine.agent_id ? (
                        <span className="min-w-0 max-w-full truncate">
                          {t({ ko: "담당", en: "Agent", ja: "担当", zh: "负责" })} {routine.agent_id}
                        </span>
                      ) : null}
                      <span className="min-w-0 max-w-full truncate">
                        {t({ ko: "최근", en: "Last", ja: "前回", zh: "上次" })} {lastRunLabel}
                      </span>
                    </div>
                  </div>
                </SurfaceListItem>
              </button>
            );
          })}
        </div>
      )}
      </SurfaceSubsection>
      <RoutineDetailDrawer
        routine={selectedRoutine}
        runs={selectedRuns}
        runsLoading={runsLoading}
        runsError={runsError}
        t={t}
        localeTag={localeTag}
        language={language}
        onClose={() => setSelectedRoutineId(null)}
      />
    </>
  );
}
