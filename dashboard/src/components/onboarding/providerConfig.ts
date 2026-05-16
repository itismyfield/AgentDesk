import type { CommandBotEntry } from "../onboardingDraft";

export const COMMAND_PROVIDERS = ["claude", "codex", "gemini", "opencode", "qwen"] as const;

const PROVIDER_CONFIG = {
  claude: {
    suffix: "cc",
    label: "Claude",
    cliName: "Claude Code",
    installHintKo: "설치: npm install -g @anthropic-ai/claude-code",
    installHintEn: "Install: npm install -g @anthropic-ai/claude-code",
    loginHintKo: "로그인: claude login",
    loginHintEn: "Login: claude login",
    loginCommand: "claude login",
  },
  codex: {
    suffix: "cdx",
    label: "Codex",
    cliName: "Codex CLI",
    installHintKo: "설치: npm install -g @openai/codex",
    installHintEn: "Install: npm install -g @openai/codex",
    loginHintKo: "로그인: codex login",
    loginHintEn: "Login: codex login",
    loginCommand: "codex login",
  },
  gemini: {
    suffix: "gm",
    label: "Gemini",
    cliName: "Gemini CLI",
    installHintKo: "설치: npm install -g @google/gemini-cli",
    installHintEn: "Install: npm install -g @google/gemini-cli",
    loginHintKo: "로그인: gemini",
    loginHintEn: "Login: gemini",
    loginCommand: "gemini",
  },
  opencode: {
    suffix: "oc",
    label: "OpenCode",
    cliName: "OpenCode",
    installHintKo: "설치: npm install -g opencode-ai",
    installHintEn: "Install: npm install -g opencode-ai",
    loginHintKo: "로그인: opencode 실행 후 provider 인증 확인",
    loginHintEn: "Login: run opencode, then verify provider auth",
    loginCommand: "opencode",
  },
  qwen: {
    suffix: "qw",
    label: "Qwen",
    cliName: "Qwen Code",
    installHintKo: "설치: npm install -g @qwen-code/qwen-code@latest",
    installHintEn: "Install: npm install -g @qwen-code/qwen-code@latest",
    loginHintKo: "로그인: qwen 실행 후 /auth",
    loginHintEn: "Login: run qwen, then /auth",
    loginCommand: "qwen -> /auth",
  },
} as const satisfies Record<CommandBotEntry["provider"], {
  suffix: string;
  label: string;
  cliName: string;
  installHintKo: string;
  installHintEn: string;
  loginHintKo: string;
  loginHintEn: string;
  loginCommand: string;
}>;

export function providerSuffix(provider: CommandBotEntry["provider"]) {
  return PROVIDER_CONFIG[provider].suffix;
}

export function providerLabel(provider: CommandBotEntry["provider"]) {
  return PROVIDER_CONFIG[provider].label;
}

export function providerCliName(provider: CommandBotEntry["provider"]) {
  return PROVIDER_CONFIG[provider].cliName;
}

export function providerInstallHint(provider: CommandBotEntry["provider"], isKo: boolean) {
  const config = PROVIDER_CONFIG[provider];
  return isKo ? config.installHintKo : config.installHintEn;
}

export function providerLoginHint(provider: CommandBotEntry["provider"], isKo: boolean) {
  const config = PROVIDER_CONFIG[provider];
  return isKo ? config.loginHintKo : config.loginHintEn;
}

export function providerLoginCommand(provider: CommandBotEntry["provider"]) {
  return PROVIDER_CONFIG[provider].loginCommand;
}
