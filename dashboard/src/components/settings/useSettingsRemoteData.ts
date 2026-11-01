import { useCallback, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import * as api from "../../api";
import { getOnboardingConnectionStatus } from "../../api/settingsRuntime";
import { providerDisplayName } from "./SettingsCopy";
import type { SettingsNotificationType } from "./SettingsModel";
import type { PendingProviderLogin, ProviderAuthProvider } from "./SettingsProvidersModel";

type Tr = (ko: string, en: string) => string;
type Notify = (ko: string, en: string, type?: SettingsNotificationType) => void;

const STATUS_STALE_MS = 30_000;

/** Onboarding status and bot health, read once for the connection rows and the tab badge. */
export function useConnectionStatus() {
  const onboarding = useQuery({
    queryKey: ["settings", "onboarding-status"],
    queryFn: getOnboardingConnectionStatus,
    staleTime: STATUS_STALE_MS,
    retry: false,
  });
  const health = useQuery({
    queryKey: ["settings", "health"],
    queryFn: () => api.getHealthDetail(),
    staleTime: STATUS_STALE_MS,
    retry: false,
  });
  return { onboarding, health };
}

export function useOperatorConnectors() {
  return useQuery({
    queryKey: ["settings", "operator-connectors"],
    queryFn: () => api.getOperatorConnectors(),
    staleTime: STATUS_STALE_MS,
    retry: false,
  });
}

/** Provider accounts load only once the AI tab is opened; the endpoint can be slow. */
export function useProviderAccounts({ enabled, notify, tr }: { enabled: boolean; notify: Notify; tr: Tr }) {
  const query = useQuery({
    queryKey: ["settings", "provider-auth-profiles"],
    queryFn: () => api.getProviderAuthProfiles(),
    enabled,
    retry: false,
  });
  const [actionError, setActionError] = useState<string | null>(null);
  const [pendingLogin, setPendingLogin] = useState<PendingProviderLogin | null>(null);
  const [startingProviderId, setStartingProviderId] = useState<string | null>(null);
  const [removingAccountKey, setRemovingAccountKey] = useState<string | null>(null);
  const { refetch } = query;

  const addAccount = useCallback(async (providerId: string) => {
    const name = providerDisplayName(providerId);
    setStartingProviderId(providerId);
    setActionError(null);
    try {
      setPendingLogin(await api.startProviderAuthLogin(providerId));
      notify(`${name} 로그인을 터미널에서 마쳐 주세요.`, `Finish the ${name} sign-in in the terminal.`, "info");
    } catch {
      setActionError(tr("추가 계정 로그인을 시작하지 못했어요.", "Could not start the additional-account sign-in."));
    } finally {
      setStartingProviderId(null);
    }
  }, [notify, tr]);

  const completeLogin = useCallback(async () => {
    if (!pendingLogin) return;
    setActionError(null);
    try {
      await api.completeProviderAuthLogin(pendingLogin.providerId, pendingLogin.profileId, pendingLogin.home);
      setPendingLogin(null);
      notify("추가 계정을 연결했어요.", "Additional account connected.", "success");
      await refetch();
    } catch {
      setActionError(tr(
        "아직 로그인 정보가 보이지 않아요. 터미널에서 로그인을 마친 뒤 다시 확인해 주세요.",
        "No sign-in found yet. Finish signing in from the terminal, then check again.",
      ));
    }
  }, [notify, pendingLogin, refetch, tr]);

  const removeAccount = useCallback(async (providerId: string, profileId: string) => {
    setRemovingAccountKey(`${providerId}:${profileId}`);
    setActionError(null);
    try {
      await api.removeProviderAuthProfile(providerId, profileId);
      notify("추가 계정 연결을 해제했어요. 로그인 정보는 남아 있어요.", "Account unlinked; its sign-in data was kept.", "success");
      await refetch();
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr(
        "연결을 해제하지 못했어요. 이 계정을 쓰는 에이전트나 채널을 먼저 기본 계정으로 바꿔 주세요.",
        "Could not unlink. Move agents or channels using this account back to the default first.",
      ));
    } finally {
      setRemovingAccountKey(null);
    }
  }, [notify, refetch, tr]);

  const setPrimary = useCallback(async (providerId: string, profileId: string) => {
    const name = providerDisplayName(providerId);
    setActionError(null);
    try {
      await api.setProviderAuthPrimaryProfile(providerId, profileId);
      const label = profileId === "default" ? "기본 로그인" : profileId;
      notify(`${name} 기본 계정을 ${label}(으)로 바꿨어요.`, `Updated the ${name} primary account.`, "success");
      await refetch();
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr(
        "기본 계정을 바꾸지 못했어요.",
        "Could not change the primary account.",
      ));
    }
  }, [notify, refetch, tr]);

  const providers: ProviderAuthProvider[] = Array.isArray(query.data?.providers) ? query.data.providers : [];
  const loadError = query.isError ? tr("AI 계정 정보를 불러오지 못했어요.", "Could not load AI accounts.") : null;

  return {
    providers,
    loading: query.isFetching,
    error: actionError ?? loadError,
    pendingLogin,
    startingProviderId,
    removingAccountKey,
    reload: () => void refetch(),
    addAccount,
    completeLogin,
    removeAccount,
    setPrimary,
  };
}
