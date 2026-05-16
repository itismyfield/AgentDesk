import type { Dispatch, RefObject, SetStateAction } from "react";

import type { BotInfo, CommandBotEntry } from "../onboardingDraft";
import {
  COMMAND_PROVIDERS,
  providerLabel,
} from "./providerConfig";
import {
  ChecklistPanel,
  Tip,
  type ChecklistItem,
} from "./OnboardingWizardSections";

type Tr = (ko: string, en: string) => string;

interface Step1BotConnectionProps {
  actionRow: string;
  announceBotInfo: BotInfo | null;
  announceReady: boolean;
  announceToken: string;
  borderInput: string;
  borderLight: string;
  btnPrimary: string;
  btnSecondary: string;
  btnSmall: string;
  commandBots: CommandBotEntry[];
  commandBotsReady: boolean;
  inputStyle: string;
  makeInviteUrl: (botId: string, permissions: string) => string;
  notifyBotInfo: BotInfo | null;
  notifyToken: string;
  permissions: {
    announce: string;
    command: string;
    notify: string;
  };
  setAnnounceToken: Dispatch<SetStateAction<string>>;
  setCommandBots: Dispatch<SetStateAction<CommandBotEntry[]>>;
  setNotifyToken: Dispatch<SetStateAction<string>>;
  step1Checklist: ChecklistItem[];
  stepBox: string;
  stepHeadingRef: RefObject<HTMLHeadingElement | null>;
  tr: Tr;
  validating: boolean;
  validateStep1: () => Promise<void>;
  goToStep: (step: number) => void;
}

export function Step1BotConnection({
  actionRow,
  announceBotInfo,
  announceReady,
  announceToken,
  borderInput,
  borderLight,
  btnPrimary,
  btnSecondary,
  btnSmall,
  commandBots,
  commandBotsReady,
  inputStyle,
  makeInviteUrl,
  notifyBotInfo,
  notifyToken,
  permissions,
  setAnnounceToken,
  setCommandBots,
  setNotifyToken,
  step1Checklist,
  stepBox,
  stepHeadingRef,
  tr,
  validating,
  validateStep1,
  goToStep,
}: Step1BotConnectionProps) {
  return (
    <div className={stepBox} style={{ borderColor: borderLight }}>
      <div>
        <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
          {tr("Discord 봇 연결", "Connect Discord Bots")}
        </h2>
        <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "AgentDesk는 Discord 봇을 통해 AI 에이전트를 운영합니다. 각 봇의 역할이 다르므로 최소 2개(실행 봇 + 통신 봇)가 필요합니다.",
            "AgentDesk runs AI agents through Discord bots. You need at least 2 bots (Command + Communication).",
          )}
        </p>
      </div>

      <div
        className="rounded-xl p-4 text-sm space-y-2"
        style={{
          background: "color-mix(in srgb, var(--th-accent-primary-soft) 72%, transparent)",
          border: "1px solid color-mix(in srgb, var(--th-accent-primary) 24%, var(--th-border) 76%)",
        }}
      >
        <div className="font-medium" style={{ color: "var(--th-text-primary)" }}>
          {tr("봇 토큰을 얻는 방법", "How to get bot tokens")}
        </div>
        <ol className="list-decimal list-inside space-y-1" style={{ color: "var(--th-text-secondary)" }}>
          <li>
            <a href="https://discord.com/developers/applications" target="_blank" rel="noopener noreferrer" className="text-emerald-300 hover:text-emerald-200 underline">
              Discord Developer Portal
            </a>
            {tr("에서 New Application 클릭", " -> Click New Application")}
          </li>
          <li>{tr("왼쪽 Bot 탭 -> Reset Token -> 토큰 복사", "Left Bot tab -> Reset Token -> Copy token")}</li>
          <li>
            {tr(
              "같은 Bot 탭 -> Privileged Gateway Intents에서 MESSAGE CONTENT Intent를 활성화",
              "On the same Bot tab -> Privileged Gateway Intents -> enable MESSAGE CONTENT Intent",
            )}
            <span className="block ml-4 mt-0.5" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "이 설정이 없으면 봇이 메시지 내용을 읽지 못해 정상 동작하지 않습니다",
                "Without this, the bot cannot read message content and will not function properly",
              )}
            </span>
          </li>
          <li>{tr("아래에 토큰을 붙여넣고 검증하면, 서버 초대 링크가 자동 생성됩니다", "Paste tokens below and validate - invite links are generated automatically")}</li>
        </ol>
      </div>

      <div className="space-y-3">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
            {tr("실행 봇", "Command Bot")}
          </span>
          <Tip text={tr(
            "에이전트의 AI 세션을 실행하는 봇입니다.\nDiscord에서 메시지를 받으면 이 봇이\nClaude Code, Codex CLI, Gemini CLI, OpenCode, 또는 Qwen Code를 실행하여\n에이전트가 작업합니다.",
            "Runs AI sessions for agents.\nWhen a message arrives, this bot\nlaunches Claude Code, Codex CLI, Gemini CLI, OpenCode, or Qwen Code.",
          )} />
        </div>

        {commandBots.map((bot, i) => (
          <div key={i} className="rounded-xl p-4 border space-y-2" style={{ borderColor: borderLight }}>
            <div className="flex items-center gap-3">
              <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {tr(`실행 봇 ${i + 1}`, `Command Bot ${i + 1}`)}
              </span>
              <div className="flex rounded-lg overflow-hidden border" style={{ borderColor: "rgba(148,163,184,0.3)" }}>
                {COMMAND_PROVIDERS.map((p) => (
                  <button type="button"
                    key={p}
                    onClick={() => {
                      setCommandBots((prev) => {
                        const copy = [...prev];
                        copy[i] = { ...copy[i], provider: p };
                        return copy;
                      });
                    }}
                    className="px-3 py-1 text-xs transition-colors"
                    style={{
                      backgroundColor: bot.provider === p ? "color-mix(in srgb, var(--th-accent-primary-soft) 84%, transparent)" : "transparent",
                      color: bot.provider === p ? "var(--th-text-primary)" : "var(--th-text-muted)",
                    }}
                  >
                    {providerLabel(p)}
                  </button>
                ))}
              </div>
              {commandBots.length > 1 && (
                <button type="button"
                  onClick={() => setCommandBots((prev) => prev.filter((_, j) => j !== i))}
                  className="ml-auto text-xs text-red-400 hover:text-red-300"
                >
                  {tr("제거", "Remove")}
                </button>
              )}
            </div>
            <input
              type="password"
              placeholder={tr("봇 토큰 붙여넣기", "Paste bot token")}
              value={bot.token}
              onChange={(event) => {
                setCommandBots((prev) => {
                  const copy = [...prev];
                  copy[i] = { ...copy[i], token: event.target.value };
                  return copy;
                });
              }}
              className={inputStyle}
              style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
            />
            {bot.botInfo?.valid && (
              <div className="flex items-center gap-2">
                <span className="text-xs text-emerald-400">OK {bot.botInfo.bot_name}</span>
                <a
                  href={makeInviteUrl(bot.botInfo.bot_id!, permissions.command)}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="text-xs px-2 py-0.5 rounded-md border transition-colors hover:opacity-100"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
                    background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                    color: "var(--th-text-primary)",
                  }}
                >
                  {tr("서버에 초대 ->", "Invite to server ->")}
                </a>
              </div>
            )}
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "자동 설정 권한: Send Messages, Read Message History, Manage Messages, Create Public Threads, Send Messages in Threads",
                "Auto-configured: Send Messages, Read Message History, Manage Messages, Create Public Threads, Send Messages in Threads",
              )}
            </div>
          </div>
        ))}

        {commandBots.length < 2 && (
          <button type="button"
            onClick={() => {
              const used = new Set(commandBots.map((bot) => bot.provider));
              const other = COMMAND_PROVIDERS.find((provider) => !used.has(provider)) ?? "claude";
              setCommandBots((prev) => [...prev, { provider: other, token: "", botInfo: null }]);
            }}
            className={btnSmall}
            style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-secondary)" }}
          >
            + {tr("두 번째 실행 봇 추가 (듀얼 프로바이더)", "Add second command bot (dual provider)")}
          </button>
        )}
      </div>

      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
            {tr("통신 봇", "Communication Bot")}
          </span>
          <Tip text={tr(
            "에이전트들이 서로 메시지를 보낼 때 사용하는 봇입니다.\n에이전트 A가 에이전트 B에게 작업을 요청하거나\n결과를 회신할 때 이 봇을 통해 전송합니다.\n\n또한 온보딩 시 Discord 채널을 자동 생성하고,\n다른 봇들의 채널 접근 권한을 설정합니다.\n(별도의 봇이어야 메시지 충돌이 방지됩니다)",
            "Used for agent-to-agent communication.\nAgent A sends tasks to Agent B through this bot.\n\nAlso creates Discord channels during onboarding\nand manages channel permissions for other bots.\n(Must be a separate bot to prevent conflicts)",
          )} />
          <span className="text-xs px-1.5 py-0.5 rounded bg-red-500/20 text-red-300 font-medium">
            {tr("필수", "Required")}
          </span>
        </div>
        <input
          type="password"
          placeholder={tr("통신 봇 토큰 붙여넣기", "Paste communication bot token")}
          value={announceToken}
          onChange={(event) => setAnnounceToken(event.target.value)}
          className={inputStyle}
          style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
        />
        {announceBotInfo?.valid && (
          <div className="flex items-center gap-2">
            <span className="text-xs text-emerald-400">OK {announceBotInfo.bot_name}</span>
            <a
              href={makeInviteUrl(announceBotInfo.bot_id!, permissions.announce)}
              target="_blank"
              rel="noopener noreferrer"
              className="text-xs px-2 py-0.5 rounded-md border transition-colors hover:opacity-100"
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
                background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                color: "var(--th-text-primary)",
              }}
            >
              {tr("서버에 초대 (관리자 권한) ->", "Invite to server (Admin) ->")}
            </a>
          </div>
        )}
        <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "관리자(Administrator) 권한으로 초대됩니다. 채널 생성, 봇 권한 설정 등을 자동으로 처리합니다.",
            "Invited with Administrator permission. Handles channel creation and bot permission setup automatically.",
          )}
        </div>
      </div>

      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
            {tr("알림 봇", "Notification Bot")}
          </span>
          <Tip text={tr(
            "시스템 상태, 오류, 경고 등 정보 전달에만 사용됩니다.\n이 봇의 메시지에는 에이전트가 반응하지 않습니다.\n없어도 기본 기능에 지장은 없습니다.",
            "Only for system status and error notifications.\nAgents don't respond to this bot's messages.\nOptional - core features work without it.",
          )} />
          <span
            className="text-xs px-1.5 py-0.5 rounded font-medium"
            style={{
              color: "var(--th-text-muted)",
              background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
            }}
          >
            {tr("선택", "Optional")}
          </span>
        </div>
        <input
          type="password"
          placeholder={tr("알림 봇 토큰 (선택)", "Notification bot token (optional)")}
          value={notifyToken}
          onChange={(event) => setNotifyToken(event.target.value)}
          className={inputStyle}
          style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
        />
        {notifyBotInfo?.valid && (
          <div className="flex items-center gap-2">
            <span className="text-xs text-emerald-400">OK {notifyBotInfo.bot_name}</span>
            <a
              href={makeInviteUrl(notifyBotInfo.bot_id!, permissions.notify)}
              target="_blank"
              rel="noopener noreferrer"
              className="text-xs px-2 py-0.5 rounded-md border transition-colors hover:opacity-100"
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
                background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                color: "var(--th-text-primary)",
              }}
            >
              {tr("서버에 초대 ->", "Invite to server ->")}
            </a>
          </div>
        )}
        <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {tr("자동 설정 권한: Send Messages", "Auto-configured: Send Messages")}
        </div>
      </div>

      <ChecklistPanel title={tr("Step 1 체크리스트", "Step 1 checklist")} items={step1Checklist} />

      <div className={actionRow}>
        <button type="button"
          onClick={() => void validateStep1()}
          disabled={!commandBots[0]?.token || !announceToken || validating}
          className={btnSecondary}
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)",
            color: "var(--th-text-primary)",
          }}
        >
          {validating ? tr("검증 중...", "Validating...") : tr("토큰 검증", "Validate Tokens")}
        </button>
        {commandBots[0]?.botInfo?.valid && announceBotInfo?.valid ? (
          <button type="button" onClick={() => goToStep(2)} className={btnPrimary}>
            {tr("다음", "Next")}
          </button>
        ) : (
          <button type="button" onClick={() => goToStep(2)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
            {tr("나중에 입력", "Skip for now")}
          </button>
        )}
      </div>
      {!commandBotsReady || !announceReady ? (
        <p className="text-xs leading-5" style={{ color: "#fde68a" }}>
          {tr(
            "이 단계 검증을 건너뛰면 Step 5에서 완료가 막힙니다. 최소한 실행 봇과 통신 봇 검증, 서버 초대까지 끝내는 편이 안전합니다.",
            "Skipping validation here will block completion in Step 5. It is safer to finish command/communication bot validation and server invites first.",
          )}
        </p>
      ) : null}
      <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
        {tr("토큰은 나중에 설정 파일에서 직접 입력할 수 있습니다: ", "Tokens can be set later in: ")}
        <code
          className="text-xs px-1 py-0.5 rounded"
          style={{ background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)" }}
        >
          ~/.adk/release/agentdesk.yaml
        </code>
      </p>
    </div>
  );
}
