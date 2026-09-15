# Provider profile fallback

Managed Discord requests automatically use other registered accounts of the same provider: Claude, Codex, Qwen, OpenCode and Grok. Accounts must already be logged in. Gemini/AGY are excluded until their launchers support isolated accounts. Optional `org.yaml` configuration:

```yaml
provider_auth_fallbacks:
  codex: {fallback_profile: codex-backup, priority: [codex-team, codex-personal], include_remaining: true, cooldown_secs: 300}
  claude: {priority: [claude-work, claude-personal]}
  qwen: {enabled: false}
```

Order: configured channel/agent/provider primary → `fallback_profile` → `priority` in list order → remaining same-provider profiles sorted by ID; duplicates are removed. Defaults: enabled, remaining accounts included, 300-second cooldown. `include_remaining: false` restricts backups to explicit entries; `enabled: false` restores fixed-account routing. A named primary never implicitly falls back to the system `default`; list it explicitly if desired. Invalid/cross-provider references reject configuration. Explicitly referenced profiles must be detached before unlinking.
Fresh usage at 100% skips an account before launch; unknown/stale/failed usage does not. Qwen/OpenCode fall back on execution errors without usage telemetry. Dispatch admission permits an alternate for an exhausted primary; pressure thresholds below 100% still apply. Classified rate-limit, idle timeout or dead/unresponsive-process errors retry anchored requests through the existing fresh-session/history path only before assistant output or tool execution. Possible side effects retain normal error/agent recovery. Each profile is attempted once per user message during routing-state lifetime; exhausted candidates stop retries. Cancellation suppresses replay and durable agent-recovery account pins take precedence.
Healthy accounts stay selected to preserve sessions; switches use existing account markers and discard foreign resume state. Selection, cooldowns and attempt history are process-local (24-hour idle retention), reset on restart and independent on other nodes. Saved primary, provider and model do not change. Scope: managed Discord launches/retries and dispatch admission; standalone one-shot calls without a channel keep existing behavior. Configure via YAML; no dashboard controls are added.
