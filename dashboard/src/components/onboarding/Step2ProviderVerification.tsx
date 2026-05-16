import type { RefObject } from "react";

import type { CommandBotEntry, ProviderStatus } from "../onboardingDraft";
import {
  providerCliName,
  providerInstallHint,
  providerLoginCommand,
  providerLoginHint,
} from "./providerConfig";
import {
  ChecklistPanel,
  type ChecklistItem,
} from "./OnboardingWizardSections";

type Tr = (ko: string, en: string) => string;

interface Step2ProviderVerificationProps {
  actionRow: string;
  borderLight: string;
  btnPrimary: string;
  btnSecondary: string;
  checkingProviders: boolean;
  commandBots: CommandBotEntry[];
  goToStep: (step: number) => void;
  isKo: boolean;
  onCheckProviders: () => Promise<void>;
  providerStatuses: Record<string, ProviderStatus>;
  providersReady: boolean;
  step2Checklist: ChecklistItem[];
  stepBox: string;
  stepHeadingRef: RefObject<HTMLHeadingElement | null>;
  tr: Tr;
}

export function Step2ProviderVerification({
  actionRow,
  borderLight,
  btnPrimary,
  btnSecondary,
  checkingProviders,
  commandBots,
  goToStep,
  isKo,
  onCheckProviders,
  providerStatuses,
  providersReady,
  step2Checklist,
  stepBox,
  stepHeadingRef,
  tr,
}: Step2ProviderVerificationProps) {
  const providers = [...new Set(commandBots.map((bot) => bot.provider))];

  return (
    <div className={stepBox} style={{ borderColor: borderLight }}>
      <div>
        <h2
          ref={stepHeadingRef}
          tabIndex={-1}
          className="text-lg font-semibold outline-none"
          style={{ color: "var(--th-text-heading)" }}
        >
          {tr("AI 프로바이더 확인", "AI Provider Verification")}
        </h2>
        <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "에이전트가 작업하려면 터미널에서 AI 프로바이더에 로그인되어 있어야 합니다.",
            "Agents need the AI provider CLI to be installed and logged in on this machine.",
          )}
        </p>
      </div>

      <div className="space-y-3">
        {providers.map((provider) => {
          const status = providerStatuses[provider];
          const name = providerCliName(provider);
          return (
            <div key={provider} className="rounded-xl p-4 border space-y-2" style={{ borderColor: borderLight }}>
              <div className="flex items-center gap-3">
                <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>{name}</span>
                {checkingProviders && (
                  <span className="text-xs animate-pulse" style={{ color: "var(--th-text-muted)" }}>
                    {tr("확인 중...", "Checking...")}
                  </span>
                )}
              </div>

              {status && !checkingProviders && (
                <div className="space-y-1">
                  <div className="flex items-center gap-2 text-sm">
                    <span>{status.installed ? "OK" : "NO"}</span>
                    <span style={{ color: status.installed ? "#86efac" : "#fca5a5" }}>
                      {status.installed
                        ? tr("설치됨", "Installed") + (status.version ? ` (${status.version})` : "")
                        : tr("설치되지 않음", "Not installed")}
                    </span>
                  </div>
                  {status.installed && (
                    <div className="flex items-center gap-2 text-sm">
                      <span>{status.logged_in ? "OK" : "!"}</span>
                      <span style={{ color: status.logged_in ? "#86efac" : "#fde68a" }}>
                        {status.logged_in
                          ? tr("로그인됨", "Logged in")
                          : tr("로그인 필요", "Login required")}
                      </span>
                    </div>
                  )}
                </div>
              )}

              {status && !status.installed && (
                <div className="rounded-lg p-3 text-xs space-y-1" style={{ backgroundColor: "rgba(251,191,36,0.08)" }}>
                  <div style={{ color: "#fde68a" }}>
                    {providerInstallHint(provider, isKo)}
                  </div>
                  <div style={{ color: "var(--th-text-muted)" }}>
                    {providerLoginHint(provider, isKo)}
                  </div>
                </div>
              )}

              {status && status.installed && !status.logged_in && (
                <div className="rounded-lg p-3 text-xs" style={{ backgroundColor: "rgba(251,191,36,0.08)" }}>
                  <div style={{ color: "#fde68a" }}>
                    {tr("터미널에서 로그인하세요:", "Login in terminal:")}
                    <code
                      className="ml-2 px-1.5 py-0.5 rounded"
                      style={{ background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)" }}
                    >
                      {providerLoginCommand(provider)}
                    </code>
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </div>

      <ChecklistPanel title={tr("Step 2 체크리스트", "Step 2 checklist")} items={step2Checklist} />

      <div className={actionRow}>
        <button type="button" onClick={() => goToStep(1)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
          {tr("이전", "Back")}
        </button>
        <button type="button" onClick={() => void onCheckProviders()} disabled={checkingProviders} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
          {tr("다시 확인", "Re-check")}
        </button>
        <button type="button"
          onClick={() => goToStep(3)}
          className={providersReady ? btnPrimary : btnSecondary}
          style={providersReady ? undefined : { borderColor: "rgba(148,163,184,0.3)" }}
        >
          {providersReady ? tr("다음", "Next") : tr("확인 없이 계속", "Continue anyway")}
        </button>
      </div>
      {!providersReady ? (
        <p className="text-xs leading-5" style={{ color: "#fde68a" }}>
          {tr(
            "CLI 설치나 로그인이 안 된 상태로 넘어가면 Step 5에서 완료할 수 없습니다. 지금은 역할 구성만 먼저 이어서 준비하는 용도입니다.",
            "If installation or login is still missing, Step 5 cannot complete. Continue only if you want to prepare the rest of the setup first.",
          )}
        </p>
      ) : null}
    </div>
  );
}
