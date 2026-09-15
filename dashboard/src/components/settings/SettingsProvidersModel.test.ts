import { describe, expect, it } from "vitest";
import {
  accountIsDefault,
  extraAccountCount,
  loginAttachInstruction,
  payloadContainsSecrets,
  usageBarPercent,
  type ProviderAuthProvider,
} from "./SettingsProvidersModel";

describe("Settings Providers extra-account helpers", () => {
  it("keeps default first and counts extras separately", () => {
    const providers: ProviderAuthProvider[] = [
      {
        id: "codex",
        default_home: "~/.codex",
        accounts: [
          { id: "default", home: "~/.codex", bound_agents: ["coder"] },
          { id: "work", home: "~/.adk/profiles/codex/work", bound_agents: ["spark"] },
        ],
      },
    ];
    expect(accountIsDefault(providers[0]?.accounts?.[0] ?? { id: "", home: "" })).toBe(true);
    expect(extraAccountCount(providers)).toBe(1);
  });

  it("builds tmux attach instructions without secrets", () => {
    const attach = loginAttachInstruction("adk-login-codex-work");
    expect(attach).toBe("tmux attach -t adk-login-codex-work");
    expect(
      payloadContainsSecrets({
        profile_id: "work",
        home: "~/.adk/profiles/codex/work",
        tmux_session: "adk-login-codex-work",
        attach,
      }),
    ).toBe(false);
    expect(payloadContainsSecrets({ access_token: "secret" })).toBe(true);
  });

  it("maps usage buckets to bar percents for default and extra accounts", () => {
    expect(usageBarPercent({ name: "5h", limit: 100, used: 25, remaining: 75 })).toBe(25);
    expect(usageBarPercent({ name: "5h", utilization: 80 })).toBe(80);
    expect(usageBarPercent({ name: "5h" })).toBeNull();
  });
});
