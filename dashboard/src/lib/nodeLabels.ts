type Translator = (ko: string, en: string) => string;

// Presentation labels are independent of the persisted role/profile identifiers.
export function nodeRoleLabel(role: string | null | undefined, tr: Translator): string {
  switch (role) {
    case "hub":
    case "leader": return tr("허브", "Hub");
    case "runner":
    case "worker": return tr("실행 노드", "Runner");
    case "auto": return tr("자동 선택", "Automatic");
    case "standby": return tr("대기", "Standby");
    default: return tr("역할 미확인", "Unknown role");
  }
}

export function runtimeModeLabel(profile: string | null | undefined, tr: Translator): string {
  switch (profile) {
    case "full": return tr("전체 기능", "Full features");
    case "runner":
    case "worker": return tr("실행 전용", "Execution only");
    default: return tr("기능 모드 미확인", "Unknown feature mode");
  }
}

export function nodePlatformLabel(platform: string | null | undefined, tr: Translator): string {
  switch (platform) {
    case "macos":
    case "darwin": return "macOS";
    case "windows": return "Windows";
    case "linux": return "Linux";
    default: return platform && platform !== "unknown" ? platform : tr("OS 미확인", "Unknown OS");
  }
}
