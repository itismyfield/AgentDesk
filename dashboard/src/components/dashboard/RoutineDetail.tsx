import { lazy, Suspense, useState, type ReactNode } from "react";
import { ExternalLink, MessageSquare, Repeat2, X } from "lucide-react";
import type { RoutineRecord, RoutineRunRecord } from "../../api";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { Loading, Skeleton } from "../../kit/free/skeleton-clean";
import type { TFunction } from "./model";
import {
  badgeTone,
  compactResultText,
  describeRoutinePurpose,
  describeRoutineSchedule,
  formatDateTime,
  formatRelative,
  formatScheduleAbsolute,
  formatTimeout,
  parseTime,
  previewValue,
  runResultFactLabel,
  runResultNoteLabel,
  runStatusLabel,
  runStatusTone,
  statusLabel,
  statusTone,
  strategyLabel,
  summarizeRoutineRunResult,
} from "./routineModel";
import { RoutineSwatch } from "./RoutineScheduleRows";
import { cx } from "./ui";

const MarkdownContent = lazy(() => import("../common/MarkdownContent"));

const sectionHeading = "m-0 text-xs font-medium leading-4 text-[var(--fg-muted)]";
const card = "rounded-[var(--r-sm)] border border-[var(--line-soft)] bg-[var(--bg-1)]";
const focusRing = "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--accent)]";

function RoutineRunResultPayload({ error, result, t }: { error: string | null; result: unknown | null; t: TFunction }) {
  const hasError = Boolean(error?.trim());
  const errorText = hasError ? compactResultText(error ?? "", 520) : null;
  const summary = result === null || result === undefined ? null : summarizeRoutineRunResult(result);
  const visibleSummary = summary?.summary && summary.summary !== errorText ? summary.summary : null;
  if (!hasError && !summary) return null;

  return (
    <div className="mt-2 space-y-2">
      {hasError ? (
        <div className="rounded-[var(--r-sm)] bg-[color-mix(in_oklch,var(--err)_12%,var(--bg-2))] px-2.5 py-2 text-xs leading-5 text-[var(--fg)]">
          <div className="mb-1 text-[11px] font-semibold text-[var(--err)]">{t({ ko: "오류", en: "Error", ja: "エラー", zh: "错误" })}</div>
          <div className="whitespace-pre-wrap [overflow-wrap:anywhere]">{errorText}</div>
        </div>
      ) : null}

      {summary && summary.structured ? (
        <div className="space-y-2 rounded-[var(--r-sm)] bg-[var(--bg-2)] px-2.5 py-2 text-[var(--fg)]">
          {visibleSummary ? <p className="text-xs leading-5 [overflow-wrap:anywhere]">{visibleSummary}</p> : null}
          {summary.assistantPreview ? (
            <div className="space-y-1">
              <div className="text-[11px] font-semibold text-[var(--fg-muted)]">
                {t({ ko: "에이전트 응답", en: "Agent response", ja: "エージェント応答", zh: "智能体回复" })}
              </div>
              <p className="whitespace-pre-wrap text-xs leading-5 [overflow-wrap:anywhere]">{summary.assistantPreview}</p>
            </div>
          ) : null}
          {summary.notes.length ? (
            <div className="space-y-1">
              {summary.notes.map((note) => (
                <div key={note.key} className="text-xs leading-5 [overflow-wrap:anywhere]">
                  <span className="font-medium text-[var(--fg-muted)]">{runResultNoteLabel(note.key, t)}:</span> <span>{note.value}</span>
                </div>
              ))}
            </div>
          ) : null}
          {summary.facts.length ? (
            <div className="flex flex-wrap gap-1.5">
              {summary.facts.map((fact) => (
                <span
                  key={`${fact.key}:${fact.value}`}
                  className={cx(
                    "inline-flex min-w-0 max-w-full items-center gap-1 rounded-md bg-[var(--bg-1)] px-2 py-1 text-[11px] leading-4 text-[var(--fg)]",
                    fact.mono ? "font-mono" : "",
                  )}
                >
                  <span className="shrink-0 font-medium text-[var(--fg-muted)]">{runResultFactLabel(fact.key, t)}</span>
                  <span className="min-w-0 break-all">{fact.value}</span>
                </span>
              ))}
            </div>
          ) : null}
          {summary.rawPreview ? (
            <details className="text-[11px] leading-5 text-[var(--fg-muted)]">
              <summary className={cx("flex min-h-11 min-w-11 cursor-pointer select-none items-center rounded-md text-xs", focusRing)}>
                {t({ ko: "원본 JSON", en: "Raw JSON", ja: "元JSON", zh: "原始 JSON" })}
              </summary>
              <pre className="mt-1 max-h-28 overflow-auto whitespace-pre-wrap break-words font-mono text-[var(--fg)]">{summary.rawPreview}</pre>
            </details>
          ) : null}
        </div>
      ) : summary?.rawPreview ? (
        <pre className="max-h-36 overflow-auto whitespace-pre-wrap break-words rounded-[var(--r-sm)] bg-[var(--bg-2)] px-2.5 py-2 font-mono text-[11px] leading-5 text-[var(--fg)]">
          {summary.rawPreview}
        </pre>
      ) : null}
    </div>
  );
}

function DetailField({ label, value, mono = false }: { label: string; value: string | number | null | undefined; mono?: boolean }) {
  return (
    <div className="min-w-0 px-3 py-2">
      <dt className="text-xs leading-4 text-[var(--fg-muted)]">{label}</dt>
      <dd className={cx("m-0 mt-1 min-w-0 text-[var(--fg)] [overflow-wrap:anywhere]", mono ? "font-mono text-xs leading-5" : "text-sm font-medium")}>
        {value ?? "-"}
      </dd>
    </div>
  );
}

const RESULT_PREVIEW_CHARS = 140;

function ExpandableResult({ text, t }: { text: string; t: TFunction }) {
  const [expanded, setExpanded] = useState(false);
  const long = text.length > RESULT_PREVIEW_CHARS || text.includes("\n");
  return (
    <div data-testid="routine-last-result" className="min-w-0">
      <div
        className={cx(
          "min-w-0 border-l-2 border-[var(--line)] pl-3 text-sm leading-6 text-[var(--fg)] [overflow-wrap:anywhere]",
          long && !expanded ? "max-h-12 overflow-hidden [mask-image:linear-gradient(to_bottom,black_55%,transparent)]" : "",
        )}
      >
        <Suspense fallback={<p className="whitespace-pre-wrap">{text}</p>}>
          <MarkdownContent content={text} />
        </Suspense>
      </div>
      {long ? (
        <Button variant="ghost" size="sm" className="mt-1" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)}>
          {expanded ? t({ ko: "접기", en: "Show less", ja: "閉じる", zh: "收起" }) : t({ ko: "펼치기", en: "Show more", ja: "もっと見る", zh: "展开" })}
        </Button>
      ) : null}
    </div>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex min-w-0 flex-col gap-2">
      <h3 className={sectionHeading}>{title}</h3>
      {children}
    </section>
  );
}

export interface RoutineDetailBodyProps {
  routine: RoutineRecord;
  threadUrl: string | null;
  runs: RoutineRunRecord[];
  runsLoading: boolean;
  runsError: boolean;
  t: TFunction;
  localeTag: string;
  language: "ko" | "en" | "ja" | "zh";
  now: number;
}

/** Detail in the kit order: identity and actions, latest result, next run, settings, then run history. */
export function RoutineDetailBody({ routine, threadUrl, runs, runsLoading, runsError, t, localeTag, language, now }: RoutineDetailBodyProps) {
  const lastResult = routine.last_result?.trim() ?? "";
  const due = parseTime(routine.next_due_at);
  const paused = routine.status !== "enabled";
  const tone = statusTone(routine);
  const note = "m-0 text-sm text-[var(--fg-faint)] [word-break:keep-all]";

  return (
    <div className="flex min-w-0 flex-col gap-6" data-testid="routine-detail">
      <section className="flex min-w-0 flex-col gap-3">
        <div className="flex min-w-0 items-start gap-2.5">
          <span className="inline-flex shrink-0 pt-1.5">
            <RoutineSwatch tone={tone} outline={paused} />
          </span>
          <div className="flex min-w-0 flex-col gap-0.5">
            <span className="font-mono text-xs leading-5 text-[var(--fg-muted)] [overflow-wrap:anywhere]">{routine.name}</span>
            <span className="inline-flex flex-wrap items-center gap-x-1.5 text-xs leading-5 text-[var(--fg-muted)] [overflow-wrap:anywhere]">
              <Repeat2 size={12} aria-hidden className="shrink-0" />
              {describeRoutineSchedule(routine.schedule, language)}
              {routine.agent_id ? ` · ${routine.agent_id}` : ""}
            </span>
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Badge tone={badgeTone(tone)} dot live={tone === "info"}>
            {statusLabel(routine, t)}
          </Badge>
          {routine.last_run?.status ? (
            <Badge tone={badgeTone(runStatusTone(routine.last_run.status))}>{runStatusLabel(routine.last_run.status, t)}</Badge>
          ) : null}
        </div>
        {threadUrl ? (
          <a
            href={threadUrl}
            target="_blank"
            rel="noreferrer"
            data-testid="routine-thread-link"
            className={cx(
              "inline-flex min-h-11 w-fit max-w-full items-center gap-2 rounded-[var(--r-sm)] border border-[var(--line)] bg-[var(--bg-1)] px-4 text-sm font-medium text-[var(--fg)] transition-colors hover:bg-[var(--bg-3)]",
              focusRing,
            )}
          >
            <MessageSquare size={16} aria-hidden />
            {t({ ko: "디스코드 스레드 열기", en: "Open Discord thread", ja: "Discord スレッドを開く", zh: "打开 Discord 帖子" })}
            <ExternalLink size={14} aria-hidden />
          </a>
        ) : null}
      </section>

      {lastResult ? (
        <Section title={t({ ko: "최근 결과", en: "Latest result", ja: "前回結果", zh: "最近结果" })}>
          <ExpandableResult text={lastResult} t={t} />
        </Section>
      ) : null}

      <Section title={t({ ko: "다음 실행", en: "Next run", ja: "次回実行", zh: "下次运行" })}>
        {paused ? (
          <p className={note}>
            {routine.status === "detached"
              ? t({ ko: "분리되어 자동 실행하지 않아요.", en: "Detached, so it does not run on its own.", ja: "切り離されているため自動実行しません。", zh: "已分离，不会自动运行。" })
              : t({ ko: "일시정지되어 앞으로의 자동 실행이 멈췄어요.", en: "Paused, so upcoming automatic runs are on hold.", ja: "一時停止中のため自動実行は止まっています。", zh: "已暂停，后续自动运行已停止。" })}
          </p>
        ) : due != null ? (
          <div className={cx(card, "flex min-h-11 items-center gap-3 px-3 py-2 text-sm")}>
            <span className="whitespace-nowrap font-medium tabular-nums text-[var(--fg)]">{formatScheduleAbsolute(due, now, localeTag, t)}</span>
            <span className="min-w-0 flex-1 text-xs text-[var(--fg-faint)]">{formatRelative(due, localeTag, now)}</span>
          </div>
        ) : (
          <p className={note}>
            {t({ ko: "예정된 실행이 없어요. 수동으로만 실행해요.", en: "No run is scheduled. It only runs manually.", ja: "予定された実行はありません。手動でのみ実行します。", zh: "没有计划运行，仅手动运行。" })}
          </p>
        )}
      </Section>

      <Section title={t({ ko: "설정", en: "Settings", ja: "設定", zh: "设置" })}>
        <dl className={cx(card, "m-0 grid grid-cols-2 [&>div:nth-child(n+3)]:border-t [&>div:nth-child(n+3)]:border-[var(--line-soft)]")}>
          <DetailField label={t({ ko: "최근 실행", en: "Last run", ja: "前回実行", zh: "上次运行" })} value={formatDateTime(routine.last_run_at, localeTag)} />
          <DetailField label={t({ ko: "담당", en: "Agent", ja: "担当", zh: "负责" })} value={routine.agent_id ?? "-"} />
          <DetailField label={t({ ko: "대체 담당", en: "Backup agent", ja: "代替担当", zh: "备用负责" })} value={routine.fallback_agent_id ?? "-"} />
          <DetailField label={t({ ko: "실행 방식", en: "Session", ja: "実行方式", zh: "执行方式" })} value={strategyLabel(routine.execution_strategy, t)} />
          <DetailField label={t({ ko: "제한 시간", en: "Time limit", ja: "制限時間", zh: "时间限制" })} value={formatTimeout(routine.timeout_secs, t)} />
          <DetailField label={t({ ko: "재시도", en: "Retries", ja: "リトライ", zh: "重试" })} value={routine.max_retries ?? "-"} />
          {routine.pause_reason ? (
            <DetailField label={t({ ko: "멈춘 이유", en: "Why paused", ja: "停止理由", zh: "暂停原因" })} value={routine.pause_reason} />
          ) : null}
        </dl>
        <details className={cx(card, "px-3")}>
          <summary className={cx("flex min-h-11 cursor-pointer select-none items-center rounded-md text-sm font-medium text-[var(--fg-dim)]", focusRing)}>
            {t({ ko: "고급 정보", en: "Advanced", ja: "詳細情報", zh: "高级信息" })}
          </summary>
          <dl className="m-0 -mx-3 border-t border-[var(--line-soft)] pb-2">
            <DetailField label={t({ ko: "스크립트", en: "Script", ja: "スクリプト", zh: "脚本" })} value={routine.script_ref} mono />
            <DetailField label={t({ ko: "루틴 ID", en: "Routine ID", ja: "ルーチン ID", zh: "例程 ID" })} value={routine.id} mono />
            <DetailField label={t({ ko: "스레드 ID", en: "Thread ID", ja: "スレッド ID", zh: "帖子 ID" })} value={routine.discord_thread_id ?? "-"} mono />
            <DetailField label={t({ ko: "체크포인트", en: "Checkpoint", ja: "チェックポイント", zh: "检查点" })} value={previewValue(routine.checkpoint)} mono />
          </dl>
        </details>
      </Section>

      <Section title={t({ ko: "실행 기록", en: "Run history", ja: "実行履歴", zh: "运行记录" })}>
        {runsLoading ? (
          <Loading label={t({ ko: "실행 기록을 불러오는 중", en: "Loading runs", ja: "実行履歴を読み込み中", zh: "正在加载运行记录" })} className={cx(card, "flex flex-col gap-3 px-3 py-3")}>
            {[72, 56, 64].map((width) => (
              <span key={width} className="flex items-center justify-between gap-3">
                <Skeleton width={`${width}%`} height={10} line={20} />
                <Skeleton width={44} height={18} radius={6} />
              </span>
            ))}
          </Loading>
        ) : runsError ? (
          <p className={note}>{t({ ko: "실행 기록을 불러오지 못했어요.", en: "Runs could not be loaded.", ja: "実行履歴を読み込めませんでした。", zh: "无法加载运行记录。" })}</p>
        ) : runs.length === 0 ? (
          <p className={note}>{t({ ko: "아직 실행 기록이 없어요.", en: "No runs yet.", ja: "実行履歴はまだありません。", zh: "还没有运行记录。" })}</p>
        ) : (
          <ul className={cx(card, "m-0 list-none divide-y divide-[var(--line-soft)] px-3 py-0")} data-testid="routine-run-history">
            {runs.slice(0, 8).map((run) => (
              <li key={run.id} className="py-2.5">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <span className="min-w-0 text-xs tabular-nums text-[var(--fg-muted)]">
                    {formatDateTime(run.started_at, localeTag)}
                    {run.finished_at ? ` → ${formatDateTime(run.finished_at, localeTag)}` : ""}
                  </span>
                  <Badge tone={badgeTone(runStatusTone(run.status))}>
                    {runStatusLabel(run.status, t)?.replace(/^(최근|Last|前回|最近)\s?/, "") ?? run.status}
                  </Badge>
                </div>
                <RoutineRunResultPayload error={run.error} result={run.result_json} t={t} />
              </li>
            ))}
          </ul>
        )}
      </Section>
    </div>
  );
}

/** Title shown above the detail: the plain-language purpose. */
export function routineDetailTitle(routine: RoutineRecord | null, t: TFunction): string {
  return routine
    ? describeRoutinePurpose(routine, t)
    : t({ ko: "루틴 상세", en: "Routine detail", ja: "ルーチン詳細", zh: "例程详情" });
}

/** Wide-screen split pane: the same detail beside the list, with a close button that hands focus back. */
export function RoutineDetailPane({
  routine,
  t,
  onClose,
  children,
}: {
  routine: RoutineRecord | null;
  t: TFunction;
  onClose: () => void;
  children: ReactNode;
}) {
  return (
    <aside
      aria-label={t({ ko: "루틴 상세", en: "Routine detail", ja: "ルーチン詳細", zh: "例程详情" })}
      data-testid="routine-detail-pane"
      className="flex min-h-0 min-w-0 flex-col rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-2)]"
    >
      {routine ? (
        <>
          <header className="flex items-start justify-between gap-2 border-b border-[var(--line-soft)] py-2 pl-4 pr-2">
            <h2 className="m-0 min-w-0 py-2.5 text-[15px] font-medium leading-[22px] text-[var(--fg)] [overflow-wrap:anywhere]">
              {routineDetailTitle(routine, t)}
            </h2>
            <Button
              variant="ghost"
              size="sm"
              icon={<X size={16} aria-hidden />}
              aria-label={t({ ko: "상세 닫기", en: "Close detail", ja: "詳細を閉じる", zh: "关闭详情" })}
              className="shrink-0"
              onClick={onClose}
            />
          </header>
          <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">{children}</div>
        </>
      ) : (
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-6 py-12 text-center">
          <span className="grid h-9 w-9 place-items-center rounded-[var(--r-sm)] bg-[var(--bg-2)] text-[var(--fg-muted)]" aria-hidden>
            <Repeat2 size={18} />
          </span>
          <p className="m-0 text-sm font-medium text-[var(--fg)]">
            {t({ ko: "선택한 루틴이 없어요", en: "No routine selected", ja: "ルーチンが選択されていません", zh: "未选择例程" })}
          </p>
          <p className="m-0 max-w-[16rem] text-xs leading-5 text-[var(--fg-muted)] [word-break:keep-all]">
            {t({
              ko: "루틴을 고르면 다음 실행과 최근 결과가 보여요",
              en: "Pick a routine to see its next run and latest results",
              ja: "ルーチンを選ぶと次回実行と前回結果が表示されます",
              zh: "选择例程即可查看下次运行和最近结果",
            })}
          </p>
        </div>
      )}
    </aside>
  );
}
