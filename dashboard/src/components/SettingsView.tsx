import { AlertTriangle, Check, RefreshCw, RotateCcw, Search } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { useSearchParams } from "react-router";
import { PageFrame } from "../app/PageFrame";
import { MOBILE_LAYOUT_MEDIA_QUERY } from "../app/breakpoints";
import { Button } from "../kit/free/button-clean";
import ClusterNodesPanel from "./cluster/ClusterNodesPanel";
import { Modal } from "./common/overlay/Modal";
import { connectionItems, connectionStatusRows } from "./settings/SettingsConnectionsModel";
import { SETTINGS_CATEGORIES, type SettingsCategory } from "./settings/SettingsCopy";
import { SettingsMachinePanel } from "./settings/SettingsMachinePanel";
import {
  SETTINGS_CATEGORY_QUERY_KEY,
  catalogCounts,
  configItems,
  draftProblem,
  filterCatalog,
  languageItem,
  resetsOnRestart,
  resolveCategory,
  runtimeItems,
  runtimeValue,
  sortCatalog,
  type CatalogItem,
  type CategoryFilter,
  type PendingDangerousSave,
  type SettingsNotificationType,
} from "./settings/SettingsModel";
import { SettingsEditSheet, SettingsPhoneCatalog } from "./settings/SettingsNavigation";
import { SettingsOnboardingOverlay } from "./settings/SettingsOnboardingOverlay";
import { SettingsPipelineEditorSection } from "./settings/SettingsPipelineEditorSection";
import { SETTINGS_TRAY_CLASS, SettingsAction, SettingsNotice, SettingsSection, SettingsTextField } from "./settings/SettingsPrimitives";
import { SettingsProvidersPanel } from "./settings/SettingsProvidersPanel";
import { SettingsSaveBar } from "./settings/SettingsSaveBar";
import { SettingsDesktopCatalog, type CatalogGroup } from "./settings/SettingsViewLayout";
import { getDangerousConfigKeys, getDangerousConfigLabel } from "./settings/settingsDangerousConfig";
import { useConfigEntriesDraft, useRuntimeConfigDraft } from "./settings/useSettingsDrafts";
import { useConnectionStatus, useOperatorConnectors, usePromptRetention, useProviderAccounts } from "./settings/useSettingsRemoteData";

interface SettingsViewProps {
  isKo: boolean;
  /** Saved dashboard language and the shell's /api/settings save path. */
  language: string;
  onSaveSettings: (patch: Record<string, unknown>) => Promise<void>;
  onNotify?: (message: string, type?: SettingsNotificationType) => string | void;
}

const WIDE_LAYOUT_QUERY = "(min-width: 1280px)";

function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (onChange) => {
      const media = window.matchMedia(query);
      media.addEventListener("change", onChange);
      return () => media.removeEventListener("change", onChange);
    },
    () => window.matchMedia(query).matches,
    () => false,
  );
}

/** "/" jumps to the search field unless the operator is already typing somewhere. */
function useSlashFocus(target: React.RefObject<HTMLInputElement | null>) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const element = event.target as HTMLElement | null;
      if (event.key !== "/" || event.metaKey || event.ctrlKey || event.altKey) return;
      if (element?.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(element?.tagName ?? "")) return;
      event.preventDefault();
      target.current?.focus();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [target]);
}

/** Settings catalog: config, runtime-config and connection status in one searchable list (v3 stage 2). */
export default function SettingsView({ isKo, language, onSaveSettings, onNotify }: SettingsViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const notify = useCallback(
    (ko: string, en: string, type: SettingsNotificationType = "info") => {
      onNotify?.(tr(ko, en), type);
    },
    [onNotify, tr],
  );

  const [searchParams, setSearchParams] = useSearchParams();
  const category = resolveCategory(searchParams);
  const setCategory = useCallback((next: CategoryFilter) => {
    setSearchParams((current) => {
      const params = new URLSearchParams(current);
      params.delete("settingsPanel");
      if (next === "all") params.delete(SETTINGS_CATEGORY_QUERY_KEY);
      else params.set(SETTINGS_CATEGORY_QUERY_KEY, next);
      return params;
    });
  }, [setSearchParams]);
  const phone = useMediaQuery(MOBILE_LAYOUT_MEDIA_QUERY);
  const wide = useMediaQuery(WIDE_LAYOUT_QUERY);
  const searchRef = useRef<HTMLInputElement>(null);
  useSlashFocus(searchRef);

  const [query, setQuery] = useState("");
  const [filters, setFilters] = useState({ differs: false, restart: false });
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [sheetKey, setSheetKey] = useState<string | null>(null);
  const [editorOpen, setEditorOpen] = useState(false);
  const [showOnboarding, setShowOnboarding] = useState(false);
  const [saving, setSaving] = useState(false);
  const [pendingDangerous, setPendingDangerous] = useState<PendingDangerousSave | null>(null);
  const [languageDraft, setLanguageDraft] = useState<string | null>(null);

  const config = useConfigEntriesDraft();
  const runtime = useRuntimeConfigDraft();
  const connection = useConnectionStatus({ live: category === "connections" });
  const connectors = useOperatorConnectors();
  const retention = usePromptRetention();
  const accounts = useProviderAccounts({ enabled: category === "usage", notify, tr });

  const statusRows = useMemo(
    () => connectionStatusRows(connection.onboarding, connection.health, tr),
    [connection.health, connection.onboarding, tr],
  );
  const items = useMemo(() => sortCatalog([
    languageItem(language, languageDraft, isKo),
    ...configItems(config.entries, config.edits, isKo),
    ...(runtime.loaded && !runtime.error
      ? runtimeItems({ saved: runtime.saved, draft: runtime.values, defaults: runtime.defaults, changedKeys: runtime.changedKeys }, resetsOnRestart(config.entries), isKo)
      : []),
    ...connectionItems({ statusRows, health: connection.health, retention, connectors }, tr, isKo),
  ]), [config.edits, config.entries, connection.health, connectors, isKo, language, languageDraft, retention, runtime.changedKeys, runtime.defaults, runtime.error, runtime.loaded, runtime.saved, runtime.values, statusRows, tr]);

  const counts = useMemo(() => catalogCounts(items), [items]);
  const filtered = useMemo(() => filterCatalog(items, { category, query, ...filters }), [category, filters, items, query]);
  const changes = useMemo(() => items.filter((item) => item.draft !== null), [items]);
  const problems = useMemo(
    () => new Map(changes.map((item) => [item.key, draftProblem(item, item.draft ?? "", tr)])),
    [changes, tr],
  );
  const problemFor = useCallback((key: string) => problems.get(key) ?? null, [problems]);
  const firstProblem = changes.find((item) => problems.get(item.key));
  const blockedReason = firstProblem ? `${firstProblem.label}: ${problems.get(firstProblem.key)}` : null;
  const selected = filtered.find((item) => item.key === selectedKey) ?? (wide ? filtered[0] ?? null : null);
  const sheetItem = sheetKey ? items.find((item) => item.key === sheetKey) ?? null : null;
  const loading = !config.loaded || !runtime.loaded;
  const categoryLabel = useCallback((id: SettingsCategory) => {
    const entry = SETTINGS_CATEGORIES.find((candidate) => candidate.id === id);
    return entry ? tr(entry.ko, entry.en) : id;
  }, [tr]);

  const { edit: editConfig } = config;
  const { change: changeRuntime, resetToDefault } = runtime;
  const handleChange = useCallback((item: CatalogItem, value: string) => {
    if (item.source === "config") editConfig(item.key, value);
    else if (item.source === "runtime") changeRuntime(item.key, runtimeValue(item, value));
    else if (item.source === "app") setLanguageDraft(value === language ? null : value);
  }, [changeRuntime, editConfig, language]);
  const handleReset = useCallback((item: CatalogItem) => {
    if (item.baseline === null) return;
    if (item.source === "runtime") resetToDefault(item.key);
    else if (item.source === "app") setLanguageDraft(item.baseline === language ? null : item.baseline);
    else editConfig(item.key, item.baseline);
  }, [editConfig, language, resetToDefault]);

  // Dangerous config keys (review gate, context clearing) wait for a confirmation before any save path runs.
  const guarded = (keys: string[], run: () => void) => {
    if (keys.length > 0) setPendingDangerous({ keys, run });
    else run();
  };

  const saveAll = async () => {
    setSaving(true);
    let failed = false;
    if (languageDraft !== null) {
      try {
        await onSaveSettings({ language: languageDraft });
        setLanguageDraft(null);
      } catch { failed = true; }
    }
    if (runtime.dirty) {
      try { await runtime.save(); } catch { failed = true; }
    }
    if (config.dirty) {
      try { await config.save({ ...config.edits }); } catch { failed = true; }
    }
    setSaving(false);
    if (failed) {
      notify("일부 설정을 저장하지 못했어요. 바뀐 값은 남아 있으니 다시 저장해 주세요.", "Some settings were not saved. Your changes are kept; try saving again.", "error");
    } else {
      notify("설정을 저장했어요.", "Saved settings.", "success");
    }
  };

  const saveOne = async (item: CatalogItem, value: string) => {
    setSaving(true);
    try {
      if (item.source === "config") await config.save({ [item.key]: value });
      else if (item.source === "app") {
        await onSaveSettings({ language: value });
        setLanguageDraft(null);
      } else await runtime.saveKey(item.key, runtimeValue(item, value), value !== item.baseline);
      setSheetKey(null);
      notify("설정을 저장했어요.", "Saved settings.", "success");
    } catch {
      notify("설정을 저장하지 못했어요. 다시 시도해 주세요.", "Could not save the setting. Try again.", "error");
    } finally {
      setSaving(false);
    }
  };

  const groups = useMemo<CatalogGroup[]>(() => SETTINGS_CATEGORIES
    .map((entry) => ({ id: entry.id, label: tr(entry.ko, entry.en), items: filtered.filter((item) => item.category === entry.id) }))
    .filter((group) => group.items.length > 0), [filtered, tr]);
  const categoryEntries = SETTINGS_CATEGORIES
    .filter((entry) => entry.id !== "other" || counts.byCategory.other)
    .map((entry) => ({ id: entry.id, label: tr(entry.ko, entry.en), count: counts.byCategory[entry.id] ?? 0 }));
  const narrowed = filters.differs || filters.restart || query.trim() !== "";
  const filterWords = [filters.differs ? tr("기본값과 다른 것만", "differing only") : null, filters.restart ? tr("재시작이 필요한 것만", "restart needed only") : null].filter(Boolean);
  const caption = narrowed
    ? [tr(`${counts.total}개 중 ${filtered.length}개`, `${filtered.length} of ${counts.total}`), ...filterWords].join(" · ")
    : tr(`설정 ${filtered.length}개`, `${filtered.length} settings`);

  const failedSources = config.error || runtime.error || connectors.isError || retention.isError;
  const notice = failedSources ? (
    <SettingsNotice role="alert" tone="warn" icon={<AlertTriangle size={15} />}>
      {tr("일부 설정을 불러오지 못했어요. 불러오지 못한 항목은 숨겼으니 새로고침해 다시 확인해 주세요.", "Some settings could not be loaded and are hidden. Refresh the page to try again.")}
    </SettingsNotice>
  ) : null;

  // Tools that are not key-value rows stay with their category: the open category shows them under its list.
  const blocks: Partial<Record<SettingsCategory, ReactNode>> = {
    workflow: <SettingsPipelineEditorSection tr={tr} isKo={isKo} open={editorOpen} onToggle={() => setEditorOpen((open) => !open)} notify={notify} />,
    usage: (
      <SettingsSection title={tr("AI 계정", "AI accounts")} description={tr("기본 계정과 추가 계정의 사용량을 봐요.", "Usage of the default and additional accounts.")} variant="open" testId="settings-section-ai-accounts">
        <SettingsProvidersPanel
          accounts={accounts}
          warningPct={Number(runtime.values.rateLimitWarningPct ?? runtime.defaults.rateLimitWarningPct ?? 80)}
          dangerPct={Number(runtime.values.rateLimitDangerPct ?? runtime.defaults.rateLimitDangerPct ?? 95)}
          tr={tr}
        />
      </SettingsSection>
    ),
    system: <SettingsMachinePanel tr={tr} />,
    connections: (
      <div className="flex min-w-0 flex-col gap-4">
        <div className="flex flex-wrap gap-2">
          <SettingsAction
            icon={<RefreshCw className={connection.health.isFetching ? "animate-spin motion-reduce:animate-none" : undefined} />}
            disabled={connection.health.isFetching}
            onClick={() => {
              void connection.health.refetch();
              void connection.onboarding.refetch();
              void connectors.refetch();
            }}
          >
            {tr("상태 다시 확인", "Check status again")}
          </SettingsAction>
          <SettingsAction icon={<RotateCcw />} onClick={() => setShowOnboarding(true)}>{tr("초기 설정 다시 하기", "Run setup again")}</SettingsAction>
        </div>
        <ClusterNodesPanel isKo={isKo} />
      </div>
    ),
  };
  const openBlock = category !== "all" && !narrowed ? blocks[category] ?? null : null;

  const saveBar = (
    <SettingsSaveBar
      changes={changes}
      blockedReason={blockedReason}
      saving={saving && !sheetItem}
      onDiscard={() => {
        runtime.discard();
        config.discard();
        setLanguageDraft(null);
      }}
      onSave={() => {
        if (!blockedReason) guarded(getDangerousConfigKeys(config.edits), () => void saveAll());
      }}
      tr={tr}
    />
  );

  const search = (
    <div className="w-full sm:w-[360px]">
      <SettingsTextField
        ref={searchRef}
        type="search"
        size="lg"
        aria-label={tr("설정 찾기", "Search settings")}
        placeholder={tr("이름·키·설명으로 찾기", "Search by name, key or description")}
        prefix={<Search aria-hidden />}
        suffix={phone ? undefined : <kbd className={`px-1.5 font-mono text-[11px] text-th-text-muted ${SETTINGS_TRAY_CLASS}`}>/</kbd>}
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        data-testid="settings-search-input"
      />
    </div>
  );

  return (
    <PageFrame
      testId="settings-page"
      title={tr("설정", "Settings")}
      actions={search}
    >
      {phone ? (
        <>
          <SettingsPhoneCatalog
            categories={categoryEntries}
            category={category}
            onCategory={setCategory}
            differing={items.filter((item) => item.overridden)}
            rows={filtered}
            searching={query.trim() !== ""}
            loading={loading}
            notice={notice}
            blocks={openBlock}
            onOpen={setSheetKey}
            tr={tr}
          />
          {changes.length > 0 ? <div className="mt-3">{saveBar}</div> : null}
        </>
      ) : (
        <SettingsDesktopCatalog
          categories={[{ id: "all", label: tr("전체", "All"), count: counts.total }, ...categoryEntries]}
          category={category}
          onCategory={setCategory}
          filters={{ ...filters, differsCount: counts.differs, restartCount: counts.restart }}
          onFilter={(name, on) => setFilters((current) => ({ ...current, [name]: on }))}
          groups={groups}
          caption={caption}
          selected={selected}
          wide={wide}
          loading={loading && !items.some((item) => item.source === "config" || item.source === "runtime")}
          notice={notice}
          saveBar={saveBar}
          blocks={openBlock}
          onSelect={setSelectedKey}
          onChange={handleChange}
          onReset={handleReset}
          problemFor={problemFor}
          allItems={items}
          categoryLabel={categoryLabel}
          tr={tr}
        />
      )}

      {sheetItem ? (
        <SettingsEditSheet
          key={sheetItem.key}
          item={sheetItem}
          allItems={items}
          saving={saving}
          onClose={() => setSheetKey(null)}
          onSave={(item, value) => guarded(item.dangerous ? [item.key] : [], () => void saveOne(item, value))}
          onSelect={setSheetKey}
          tr={tr}
        />
      ) : null}

      <Modal
        open={Boolean(pendingDangerous)}
        onClose={() => setPendingDangerous(null)}
        title={tr("저장하기 전에 확인해 주세요", "Confirm before saving")}
        description={tr(
          "자동화, 리뷰 단계, 대화 초기화에 영향을 주는 설정이 바뀌어요.",
          "These changes affect automation, review steps, or conversation resets.",
        )}
        size="sm"
      >
        <div className="space-y-4">
          <SettingsNotice tone="warn" icon={<AlertTriangle size={15} />}>
            {tr(
              "저장하면 진행 중인 카드의 리뷰·병합·대화 정책이 바로 달라질 수 있어요.",
              "Saving can immediately change review, merge, or conversation policy for active cards.",
            )}
          </SettingsNotice>
          <div>
            <div className="text-xs font-medium text-th-text-muted">{tr("바뀌는 설정", "Changing")}</div>
            <div className={`mt-2 px-3 py-2 text-[13px] leading-5 break-keep ${SETTINGS_TRAY_CLASS}`}>
              {pendingDangerous?.keys.map((key) => getDangerousConfigLabel(key, isKo)).join(", ")}
            </div>
          </div>
          <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
            <Button variant="secondary" onClick={() => setPendingDangerous(null)}>
              {tr("취소", "Cancel")}
            </Button>
            <Button
              variant="primary"
              icon={<Check />}
              loading={saving}
              disabled={saving}
              onClick={() => {
                const run = pendingDangerous?.run;
                setPendingDangerous(null);
                run?.();
              }}
            >
              {tr("확인하고 저장", "Confirm and save")}
            </Button>
          </div>
        </div>
      </Modal>

      <SettingsOnboardingOverlay isKo={isKo} onClose={() => setShowOnboarding(false)} open={showOnboarding} tr={tr} />
    </PageFrame>
  );
}
