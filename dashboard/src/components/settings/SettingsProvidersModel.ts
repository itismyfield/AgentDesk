export interface ProviderAuthUsageBucket {
  name?: string;
  label?: string;
  limit?: number;
  used?: number;
  remaining?: number;
  reset?: number;
  utilization?: number | null;
}

export interface ProviderAuthUsage {
  buckets?: ProviderAuthUsageBucket[];
  stale?: boolean;
  unsupported?: boolean;
  reason?: string | null;
}

export interface ProviderAuthAccount {
  id: string;
  home: string;
  bound_agents?: string[];
  bound_channels?: string[];
  usage?: ProviderAuthUsage | null;
}

export interface ProviderAuthProvider {
  id: string;
  default_home: string;
  primary_profile_id?: string;
  accounts?: ProviderAuthAccount[];
}

export interface ProviderAuthProfilesResponse {
  providers?: ProviderAuthProvider[];
  agent_profile_overrides?: Array<{
    agent_id: string;
    provider: string;
    profile_id: string | null;
  }>;
}

export interface ProviderLoginStartResponse {
  profile_id: string;
  home: string;
  tmux_session: string;
  attach?: string;
}

export interface PendingProviderLogin {
  providerId: string;
  profileId: string;
  home: string;
  tmuxSession: string;
  attach: string;
}

export function loginAttachInstruction(session: string): string {
  return `tmux attach -t ${session}`;
}

export function payloadContainsSecrets(value: unknown): boolean {
  const encoded = JSON.stringify(value ?? {}).toLowerCase();
  return (
    encoded.includes("access_token")
    || encoded.includes("api_key")
    || encoded.includes("\"token\"")
    || encoded.includes("sk-")
  );
}

export function usageBarPercent(bucket: ProviderAuthUsageBucket): number | null {
  if (typeof bucket.utilization === "number" && Number.isFinite(bucket.utilization)) {
    return Math.max(0, Math.min(100, Math.round(bucket.utilization)));
  }
  if (typeof bucket.limit === "number" && bucket.limit > 0 && typeof bucket.used === "number") {
    return Math.max(0, Math.min(100, Math.round((bucket.used / bucket.limit) * 100)));
  }
  return null;
}

export function accountIsDefault(account: ProviderAuthAccount): boolean {
  return account.id === "default";
}

export function extraAccountCount(providers: ProviderAuthProvider[]): number {
  return providers.reduce((count, provider) => {
    const extras = (provider.accounts ?? []).filter((account) => !accountIsDefault(account));
    return count + extras.length;
  }, 0);
}
