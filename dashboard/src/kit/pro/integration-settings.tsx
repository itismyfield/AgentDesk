// Kit provenance: itismyfield/agentdesk-ui-kit@b6d2ab1
import { cloneElement, isValidElement, useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type ImgHTMLAttributes, type KeyboardEvent, type ReactNode, type SyntheticEvent } from 'react';
import { createPortal } from 'react-dom';
import { Check, CircleAlert, Clock3, Copy, Eye, EyeOff, KeyRound, LoaderCircle, Plug, X } from 'lucide-react';

// Clean-room implementation of clean-specs/integration-settings/spec-v2.md and its allowed comparison PNGs.
// Demo credentials are synthetic. Domain checks do not contact a service or verify credentials.
export interface IntegrationValues {
  enabled: boolean;
  siteUrl: string;
  timeZone: string;
  apiKey: string;
}

export interface IntegrationSettingsProps {
  /** Defaults to `labels.name`. */
  name?: string;
  /** Neutral mark or a direct <img>. Omitted, empty or failed images fall back to the name's first Unicode letter or number. */
  logo?: ReactNode;
  count?: number;
  meta?: string;
  /** Read once on mount. Later changes do not replace the current draft or saved baseline. */
  defaultValues?: Partial<IntegrationValues>;
  /** Defaults to `labels.siteHint`. */
  siteHint?: string;
  timeZones?: string[];
  /** Draws key issuing when `onSave` is also present (an issued key is part of the saved draft). Nothing invents a key without it. */
  onGenerateKey?: () => string | Promise<string>;
  /**
   * Saves the start-time snapshot. Success restores this snapshot, including any prior enabled state.
   * Without it the site and time zone show as text, there is no key issuing and no save bar.
   */
  onSave?: (values: IntegrationValues) => void | Promise<void>;
  /** The toggle is immediate and is excluded from unsaved changes. Without it there is no switch and sync never flips. */
  onToggle?: (enabled: boolean) => void;
  /** Optional host clipboard adapter. Omit to use navigator.clipboard. */
  onCopyKey?: (key: string) => void | Promise<void>;
  /**
   * Short reason the settings cannot change right now. The switch and key button stay visible with `aria-disabled="true"`,
   * the fields show the saved values as text, the reason sits under the header and no handler is called. An unsaved
   * draft is kept and returns with the save bar when the reason clears.
   */
  actionDisabledReason?: string;
  /** Every visible and spoken text. Missing keys keep the Korean defaults. */
  labels?: Partial<IntegrationSettingsLabels>;
  className?: string;
  style?: CSSProperties;
}

export interface IntegrationSettingsLabels {
  name: string;
  siteHint: string;
  region: (name: string) => string;
  paused: string;
  sync: (name: string) => string;
  apiKey: string;
  showKey: string;
  hideKey: string;
  copyKey: string;
  generate: string;
  generating: string;
  regenerate: string;
  regenerating: string;
  keyNotice: string;
  keyCopied: string;
  generatingStatus: string;
  emptyKey: string;
  generateFailed: string;
  noClipboard: string;
  copyFailed: string;
  site: string;
  siteInvalid: string;
  zone: string;
  noZones: string;
  /** Read-only value that was never set. */
  notSet: string;
  zoneSheet: string;
  zoneClose: string;
  zoneSearch: string;
  zonePlaceholder: string;
  zoneEmpty: string;
  unsaved: string;
  discard: string;
  save: string;
  saving: string;
  savingStatus: string;
  saved: string;
  discarded: string;
  saveFailed: string;
}

export const integrationSettingsDefaultLabels: IntegrationSettingsLabels = {
  name: '도구 기록 연결',
  siteHint: 'https://를 제외한 대표 도메인을 입력하세요.',
  region: (name) => `${name} 설정`,
  paused: '일시 중지 · 동기화를 멈췄습니다',
  sync: (name) => `${name} 동기화`,
  apiKey: 'API 키',
  showKey: 'API 키 표시',
  hideKey: 'API 키 숨기기',
  copyKey: 'API 키 복사',
  generate: '새 API 키 발급',
  generating: '키 발급 중…',
  regenerate: '다시 발급',
  regenerating: '발급 중…',
  keyNotice: '키를 복사해 안전한 곳에 보관하세요.',
  keyCopied: '키를 복사했습니다. 안전한 곳에 보관하세요.',
  generatingStatus: '키 발급을 진행하고 있습니다.',
  emptyKey: '빈 키가 반환되었습니다. 발급 버튼으로 다시 시도하세요.',
  generateFailed: '키를 발급하지 못했습니다. 다시 발급해 주세요.',
  noClipboard: '클립보드를 사용할 수 없습니다. 키를 표시해 직접 보관하거나 복사를 다시 시도하세요.',
  copyFailed: '키를 복사하지 못했습니다. 복사 버튼으로 다시 시도하세요.',
  site: '사이트 주소',
  siteInvalid: '경로·포트 없이 도메인 형식으로 입력하세요. 마지막 구간은 문자 2개 이상입니다.',
  zone: '시간대',
  noZones: '선택 가능한 시간대 없음',
  notSet: '아직 정하지 않았어요',
  zoneSheet: '시간대 선택',
  zoneClose: '시간대 선택 닫기',
  zoneSearch: '시간대 검색',
  zonePlaceholder: '지역 또는 UTC 오프셋 검색',
  zoneEmpty: '일치하는 시간대가 없습니다.',
  unsaved: '저장하지 않은 변경 사항',
  discard: '되돌리기',
  save: '설정 저장',
  saving: '저장 중…',
  savingStatus: '설정을 저장하고 있습니다.',
  saved: '연결 설정을 저장했습니다.',
  discarded: '마지막 저장 상태로 되돌렸습니다.',
  saveFailed: '설정을 저장하지 못했습니다. 편집값을 보관했으니 다시 저장해 주세요.',
};

export const integrationTimeZones = [
  '(UTC −08:00) 태평양 작업 거점', '(UTC −05:00) 동부 기록 거점', '(UTC −03:00) 남미 운영 거점',
  '(UTC +00:00) 세계시 기록', '(UTC +01:00) 서유럽 작업 거점', '(UTC +01:00) 중부유럽 검토 거점',
  '(UTC +01:00) 북유럽 운영 거점', '(UTC +02:00) 동유럽 작업 거점', '(UTC +04:00) 걸프 운영 거점',
  '(UTC +05:30) 남아시아 작업 거점', '(UTC +08:00) 동남아시아 기록 거점', '(UTC +09:00) 서울 운영 거점',
  '(UTC +10:00) 오세아니아 작업 거점',
];

function cleanDomain(value: string) {
  return value.trim().replace(/^https?:\/\//i, '').replace(/\/+$/, '');
}
/** Labels in any script: Unicode letters, numbers and hyphens; the final label is two or more Unicode letters. Lone combining marks fail. */
function validDomain(value: string) {
  return !value || /^(?!-)[\p{L}\p{N}-]+(?:\.[\p{L}\p{N}-]+)*\.\p{L}{2,}$/u.test(value);
}
/** Keys shorter than 16 UTF-16 units show only 14 dots, so short keys expose no characters. */
function maskedKey(value: string) { return value.length < 16 ? '•'.repeat(14) : `${value.slice(0, 8)}${'•'.repeat(14)}${value.slice(-4)}`; }
function LogoMark({ logo, name }: { logo: ReactNode; name: string }) {
  const [failed, setFailed] = useState(false);
  if (!failed && isValidElement<ImgHTMLAttributes<HTMLImageElement>>(logo) && logo.type === 'img') {
    const own = logo.props.onError;
    return cloneElement(logo, { alt: logo.props.alt ?? '', draggable: false, onError: (event: SyntheticEvent<HTMLImageElement>) => { own?.(event); setFailed(true); } });
  }
  if (!failed && logo !== undefined && logo !== null && logo !== false && logo !== '') return <>{logo}</>;
  const initial = name.match(/[\p{L}\p{N}]/u)?.[0].toUpperCase();
  return initial ? <span className="aui-integration-settings-initial">{initial}</span> : null;
}

export function IntegrationSettings({
  name: nameProp, logo, count, meta = '', defaultValues,
  siteHint: siteHintProp, timeZones = integrationTimeZones,
  onGenerateKey, onSave, onToggle, onCopyKey, actionDisabledReason, labels, className = '', style,
}: IntegrationSettingsProps) {
  const id = useId();
  const text: IntegrationSettingsLabels = { ...integrationSettingsDefaultLabels, ...labels };
  const name = nameProp ?? text.name;
  const siteHint = siteHintProp ?? text.siteHint;
  // Host-owned changes: editing needs onSave, issuing also needs onGenerateKey, the switch needs onToggle.
  const reason = typeof actionDisabledReason === 'string' ? actionDisabledReason.trim() : '';
  const reasonId = `${id}-reason`;
  const editable = !!onSave && !reason;
  const canGenerate = editable && !!onGenerateKey;
  const showGenerate = canGenerate || !!reason;
  const inert = reason ? { 'data-unavailable': '', 'aria-disabled': true, 'aria-describedby': reasonId } : {};
  const [initial] = useState<IntegrationValues>(() => ({ enabled: defaultValues?.enabled ?? true, siteUrl: defaultValues?.siteUrl ?? '', timeZone: defaultValues?.timeZone ?? timeZones[0] ?? '', apiKey: defaultValues?.apiKey ?? '' }));
  const [values, setValues] = useState(initial);
  const [saved, setSaved] = useState(initial);
  const [revealed, setRevealed] = useState(false);
  const [copied, setCopied] = useState(false);
  const [generating, setGenerating] = useState(false);
  const [saving, setSaving] = useState(false);
  const generatingRef = useRef(false);
  const savingRef = useRef(false);
  const [keyError, setKeyError] = useState('');
  const [copyError, setCopyError] = useState('');
  const [saveError, setSaveError] = useState('');
  const [saveNotice, setSaveNotice] = useState('');
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const [mobile, setMobile] = useState(() => typeof window !== 'undefined' && window.matchMedia('(max-width: 899px)').matches);
  const [viewport, setViewport] = useState<{ height: number; top: number } | null>(null);
  const [portalTokens, setPortalTokens] = useState<CSSProperties>({});
  const sectionRef = useRef<HTMLElement>(null);
  const settingsRef = useRef<HTMLDivElement>(null);
  const footerRef = useRef<HTMLDivElement>(null);
  const toggleRef = useRef<HTMLButtonElement>(null);
  const siteRef = useRef<HTMLInputElement>(null);
  const zoneRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const copyTimers = useRef<ReturnType<typeof setTimeout>[]>([]);
  const domain = cleanDomain(values.siteUrl);
  const invalid = !validDomain(domain);
  // Without editing (no onSave, or a reason that arrived mid-edit) the saved values are shown. The draft stays in state
  // and comes back with the save bar when the reason clears.
  const shown = editable ? values : saved;
  const savedDomain = cleanDomain(saved.siteUrl);
  const dirty = editable && (values.siteUrl !== saved.siteUrl || values.timeZone !== saved.timeZone || values.apiKey !== saved.apiKey);
  const filtered = timeZones.filter(zone => zone.toLowerCase().includes(query.trim().toLowerCase()));
  const activeIndex = Math.min(active, Math.max(0, filtered.length - 1));
  const optionId = (index: number) => `${id}-zone-option-${index}`;

  useEffect(() => {
    const media = window.matchMedia('(max-width: 899px)');
    const update = () => setMobile(media.matches);
    media.addEventListener('change', update);
    return () => media.removeEventListener('change', update);
  }, []);
  useEffect(() => () => copyTimers.current.forEach(clearTimeout), []);
  useLayoutEffect(() => {
    if (!open || !mobile || !sectionRef.current) return;
    const source = sectionRef.current;
    const transfer = () => {
      const computed = getComputedStyle(source);
      const tokens: Record<string, string> = {};
      for (const property of Array.from(computed)) {
        if (property.startsWith('--aui-')) tokens[property] = computed.getPropertyValue(property);
      }
      setPortalTokens(tokens as CSSProperties);
    };
    transfer();
    const observer = new MutationObserver(transfer);
    for (let ancestor: Element | null = source; ancestor; ancestor = ancestor.parentElement) observer.observe(ancestor, { attributes: true, attributeFilter: ['class', 'data-theme', 'style'] });
    return () => observer.disconnect();
  }, [open, mobile, style, className]);
  useEffect(() => {
    if (!open) return;
    searchRef.current?.focus();
    const view = window.visualViewport;
    const measure = () => setViewport({ height: view?.height ?? window.innerHeight, top: view?.offsetTop ?? 0 });
    measure();
    view?.addEventListener('resize', measure);
    view?.addEventListener('scroll', measure);
    window.addEventListener('resize', measure);
    const outside = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!menuRef.current?.contains(target) && !zoneRef.current?.contains(target)) setOpen(false);
    };
    const escape = (event: globalThis.KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      event.preventDefault(); event.stopPropagation();
      setOpen(false); triggerRef.current?.focus();
    };
    document.addEventListener('pointerdown', outside);
    document.addEventListener('keydown', escape, true);
    return () => {
      view?.removeEventListener('resize', measure); view?.removeEventListener('scroll', measure);
      window.removeEventListener('resize', measure);
      document.removeEventListener('pointerdown', outside); document.removeEventListener('keydown', escape, true);
    };
  }, [open, mobile]);
  useEffect(() => {
    if (open) document.getElementById(optionId(activeIndex))?.scrollIntoView({ block: 'nearest' });
  }, [activeIndex, open, query]);

  function update<K extends keyof IntegrationValues>(key: K, value: IntegrationValues[K]) {
    setValues(previous => ({ ...previous, [key]: value })); setSaveError(''); setSaveNotice('');
  }
  function closeMenu(restoreFocus = true) {
    setOpen(false);
    if (restoreFocus) triggerRef.current?.focus();
  }
  function toggle() {
    if (reason || !onToggle) return;
    const enabled = !values.enabled;
    if (!enabled) {
      if (settingsRef.current?.contains(document.activeElement) || menuRef.current?.contains(document.activeElement)) toggleRef.current?.focus();
      setOpen(false);
    }
    setValues(previous => ({ ...previous, enabled }));
    setSaved(previous => ({ ...previous, enabled }));
    onToggle(enabled);
  }
  function focusAfterFooter(next: IntegrationValues) {
    if (footerRef.current?.contains(document.activeElement)) (next.enabled ? siteRef.current : toggleRef.current)?.focus();
    else if (!next.enabled && (settingsRef.current?.contains(document.activeElement) || menuRef.current?.contains(document.activeElement))) toggleRef.current?.focus();
  }
  function discard() {
    focusAfterFooter(saved);
    setValues({ ...saved }); setSaveError(''); setSaveNotice(text.discarded);
    if (!saved.enabled) setOpen(false);
  }
  async function generateKey() {
    if (!canGenerate || !onGenerateKey || generatingRef.current) return;
    generatingRef.current = true; setGenerating(true); setKeyError('');
    try {
      const key = await onGenerateKey();
      if (!key.trim()) throw new Error(text.emptyKey);
      setValues(previous => ({ ...previous, apiKey: key })); setRevealed(true);
    } catch (failure) {
      setKeyError(failure instanceof Error ? failure.message : text.generateFailed);
    } finally { generatingRef.current = false; setGenerating(false); }
  }
  async function copyKey() {
    setCopyError('');
    try {
      if (onCopyKey) await onCopyKey(shown.apiKey);
      else {
        if (!navigator.clipboard?.writeText) throw new Error(text.noClipboard);
        await navigator.clipboard.writeText(shown.apiKey);
      }
      setCopied(true);
      copyTimers.current.push(setTimeout(() => setCopied(false), 1400));
    } catch (failure) {
      setCopyError(failure instanceof Error ? failure.message : text.copyFailed);
    }
  }
  async function save() {
    if (!dirty || invalid || savingRef.current) return;
    savingRef.current = true; setSaving(true); setSaveError(''); setSaveNotice('');
    const snapshot = { ...values, siteUrl: domain };
    let restoreFooterFocus = footerRef.current?.contains(document.activeElement) ?? false;
    const movedFocus = (event: Event) => {
      if (!footerRef.current?.contains(event.target as Node)) restoreFooterFocus = false;
    };
    document.addEventListener('focusin', movedFocus);
    document.addEventListener('pointerdown', movedFocus);
    try {
      await onSave?.(snapshot);
      focusAfterFooter(snapshot);
      if (restoreFooterFocus && document.activeElement === document.body) (snapshot.enabled ? siteRef.current : toggleRef.current)?.focus();
      setSaved(snapshot); setValues(snapshot); setSaveNotice(text.saved);
      if (!snapshot.enabled) setOpen(false);
    } catch (failure) {
      setSaveError(failure instanceof Error ? failure.message : text.saveFailed);
    } finally { document.removeEventListener('focusin', movedFocus); document.removeEventListener('pointerdown', movedFocus); savingRef.current = false; setSaving(false); }
  }
  function choose(zone: string) { update('timeZone', zone); closeMenu(); }
  function navigate(event: KeyboardEvent<HTMLInputElement>) {
    if (event.nativeEvent.isComposing || event.keyCode === 229) return;
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End', 'Enter'].includes(event.key)) return;
    event.preventDefault();
    if (!filtered.length) return;
    if (event.key === 'Enter') { choose(filtered[activeIndex]); return; }
    setActive(event.key === 'Home' ? 0 : event.key === 'End' ? filtered.length - 1
      : (activeIndex + (event.key === 'ArrowDown' ? 1 : -1) + filtered.length) % filtered.length);
  }
  function trapFocus(event: KeyboardEvent<HTMLDivElement>) {
    if (!mobile || event.key !== 'Tab') return;
    const targets = Array.from(menuRef.current?.querySelectorAll<HTMLElement>('button:not([disabled]):not([tabindex="-1"]), input') ?? []);
    const first = targets[0]; const last = targets[targets.length - 1];
    if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
    else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
  }

  const menu = <div ref={menuRef} className={`aui-integration-settings-menu${mobile ? ' aui-integration-settings-sheet' : ''}`}
    role={mobile ? 'dialog' : undefined} aria-modal={mobile ? true : undefined} aria-labelledby={`${id}-zone-label`} onKeyDown={trapFocus}>
    {mobile && <div className="aui-integration-settings-sheet-header"><h3>{text.zoneSheet}</h3><button type="button" aria-label={text.zoneClose} className="aui-integration-settings-close" onClick={() => closeMenu()}><X size={20} aria-hidden="true" /></button></div>}
    <input ref={searchRef} className="aui-integration-settings-search" type="search" role="combobox" aria-label={text.zoneSearch} placeholder={text.zonePlaceholder}
      autoComplete="off" spellCheck={false} value={query} aria-expanded="true" aria-autocomplete="list" aria-controls={`${id}-zone-list`}
      aria-activedescendant={filtered.length ? optionId(activeIndex) : undefined} onKeyDown={navigate} onChange={event => { setQuery(event.target.value); setActive(0); }} />
    <div className="aui-integration-settings-options" role="listbox" id={`${id}-zone-list`} aria-labelledby={`${id}-zone-label`}>
      {filtered.length ? filtered.map((zone, index) => <button type="button" role="option" tabIndex={-1} id={optionId(index)} key={`${index}:${zone}`}
        aria-selected={zone === values.timeZone} data-active={index === activeIndex} className="aui-integration-settings-option"
        onPointerMove={() => setActive(index)} onMouseDown={event => event.preventDefault()} onClick={() => choose(zone)}><span dir="auto">{zone}</span>{zone === values.timeZone && <Check size={14} strokeWidth={1.4} aria-hidden="true" />}</button>)
        : <p className="aui-integration-settings-empty" role="status">{text.zoneEmpty}</p>}
    </div>
  </div>;

  return <section ref={sectionRef} className={`aui-integration-settings ${className}`} aria-label={text.region(name)} style={style}>
    <style>{settingsStyles}</style>
    <div className="aui-integration-settings-tray">
      <header className="aui-integration-settings-header">
        <span className="aui-integration-settings-logo" aria-hidden="true"><LogoMark logo={logo} name={name} /></span>
        <div className="aui-integration-settings-heading"><div><h2 dir="auto">{name}</h2>{typeof count === 'number' && Number.isFinite(count) && <span className="aui-integration-settings-count">{count.toLocaleString('en-US', { maximumFractionDigits: 3 })}</span>}</div><p dir="auto" title={values.enabled && meta ? meta : undefined}>{values.enabled ? meta : text.paused}</p></div>
        {(onToggle || reason) && <button ref={toggleRef} type="button" role="switch" aria-checked={values.enabled} aria-label={text.sync(name)} onClick={toggle} className="aui-integration-settings-switch" {...inert}><span><span /></span></button>}
      </header>
      {reason && <p id={reasonId} className="aui-integration-settings-reason" dir="auto">{reason}</p>}
      <div ref={settingsRef} className="aui-integration-settings-collapse" data-open={values.enabled} inert={!values.enabled} aria-hidden={!values.enabled}>
        <div className="aui-integration-settings-collapse-inner">
          <div className="aui-integration-settings-card">
            {(shown.apiKey || showGenerate) && <div className="aui-integration-settings-section" aria-label={text.apiKey} aria-busy={generating}>
              {shown.apiKey ? <div className="aui-integration-settings-key-row" data-revealed={revealed}>
                <code className="aui-integration-settings-key" title={revealed ? shown.apiKey : undefined}>{revealed ? shown.apiKey : maskedKey(shown.apiKey)}</code>
                <div className="aui-integration-settings-key-actions">
                  <button type="button" className="aui-integration-settings-icon-button" aria-label={revealed ? text.hideKey : text.showKey} aria-pressed={revealed} onClick={() => setRevealed(previous => !previous)}>{revealed ? <EyeOff size={15} aria-hidden="true" /> : <Eye size={15} aria-hidden="true" />}</button>
                  <button type="button" className="aui-integration-settings-icon-button" aria-label={text.copyKey} onClick={() => void copyKey()}>{copied ? <Check size={15} aria-hidden="true" /> : <Copy size={15} aria-hidden="true" />}</button>
                  {showGenerate && <button type="button" className="aui-integration-settings-regenerate" disabled={generating} onClick={() => void generateKey()} {...inert}>{generating && <LoaderCircle className="aui-integration-settings-spinner" size={14} aria-hidden="true" />}{generating ? text.regenerating : text.regenerate}</button>}
                </div>
              </div> : <button type="button" className="aui-integration-settings-generate" disabled={generating} onClick={() => void generateKey()} {...inert}><span>{generating ? text.generating : text.generate}</span>{generating ? <LoaderCircle className="aui-integration-settings-spinner" size={14} aria-hidden="true" /> : <KeyRound size={16} aria-hidden="true" />}</button>}
              {shown.apiKey && revealed && <p className="aui-integration-settings-key-notice" role="status">{copied ? text.keyCopied : text.keyNotice}</p>}
              {generating && <span className="aui-integration-settings-sr-only" role="status">{text.generatingStatus}</span>}
              {keyError && <p className="aui-integration-settings-error" role="alert"><CircleAlert size={15} aria-hidden="true" /><span>{keyError}</span></p>}
              {copyError && <p className="aui-integration-settings-error" role="alert"><CircleAlert size={15} aria-hidden="true" /><span>{copyError}</span></p>}
            </div>}
            {editable ? <div className="aui-integration-settings-section">
              <label className="aui-integration-settings-label" htmlFor={`${id}-site`}>{text.site}</label>
              <div className="aui-integration-settings-domain" data-invalid={invalid}><span aria-hidden="true">https://</span><input ref={siteRef} id={`${id}-site`} type="text" inputMode="url" autoComplete="off" spellCheck={false}
                value={values.siteUrl} placeholder="runtime.example.org" aria-invalid={invalid} aria-describedby={`${id}-site-hint`}
                onChange={event => update('siteUrl', event.target.value.replace(/^https?:\/\//i, ''))} />{domain && !invalid && <Check size={14} className="aui-integration-settings-valid" aria-hidden="true" />}</div>
              <p id={`${id}-site-hint`} dir="auto" className={`aui-integration-settings-hint${invalid ? ' aui-integration-settings-invalid' : ''}`}>{invalid ? text.siteInvalid : siteHint}</p>
            </div> : <div className="aui-integration-settings-section">
              <span className="aui-integration-settings-label">{text.site}</span>
              <p className="aui-integration-settings-value" dir="auto">{savedDomain ? `https://${savedDomain}` : text.notSet}</p>
            </div>}
            <div className="aui-integration-settings-section">
              <span className="aui-integration-settings-label" id={`${id}-zone-label`}>{text.zone}</span>
              {editable ? <>
              <div ref={zoneRef} className="aui-integration-settings-zone">
                <button ref={triggerRef} type="button" className="aui-integration-settings-zone-button" aria-labelledby={`${id}-zone-label ${id}-zone-value`} aria-haspopup="listbox" aria-expanded={open} aria-controls={open ? `${id}-zone-list` : undefined}
                  onClick={() => { setQuery(''); setActive(Math.max(0, timeZones.indexOf(values.timeZone))); setOpen(previous => !previous); }}><span id={`${id}-zone-value`} dir="auto" title={values.timeZone || undefined}>{values.timeZone || text.noZones}</span><Clock3 size={16} aria-hidden="true" /></button>
                {open && !mobile && menu}
              </div></> : <p className="aui-integration-settings-value" dir="auto">{saved.timeZone || text.notSet}</p>}
            </div>
            {saveError && <p className="aui-integration-settings-error aui-integration-settings-message" role="alert"><CircleAlert size={15} aria-hidden="true" /><span>{saveError}</span></p>}
            <span className="aui-integration-settings-sr-only" role="status">{saving ? text.savingStatus : saveNotice}</span>
          </div>
          <div className="aui-integration-settings-footer-collapse" data-open={dirty} inert={!dirty} aria-hidden={!dirty}>
            <div className="aui-integration-settings-footer-inner"><div ref={footerRef} className="aui-integration-settings-savebar"><span>{text.unsaved}</span><div><button type="button" className="aui-integration-settings-discard" onClick={discard}>{text.discard}</button><button type="button" className="aui-integration-settings-save" disabled={invalid || saving} onClick={() => void save()}>{saving && <LoaderCircle size={14} className="aui-integration-settings-spinner" aria-hidden="true" />}{saving ? text.saving : text.save}</button></div></div></div>
          </div>
        </div>
      </div>
    </div>
    {open && mobile && editable && createPortal(<div className="aui-integration-settings aui-integration-settings-overlay" style={{ ...portalTokens, '--aui-integration-settings-visible-height': `${viewport?.height ?? window.innerHeight}px`, '--aui-integration-settings-visible-top': `${viewport?.top ?? 0}px` } as CSSProperties}><div className="aui-integration-settings-backdrop" aria-hidden="true" onClick={() => closeMenu(false)} />{menu}</div>, document.body)}
  </section>;
}

const settingsStyles = `
@layer kit {
.aui-integration-settings{--aui-integration-settings-tray:var(--aui-bg-2,#f0f0f0);--aui-integration-settings-card:var(--aui-bg-1,#fff);--aui-integration-settings-raised:var(--aui-bg-1,#fff);--aui-integration-settings-raised-hover:var(--aui-bg-2,#f7f7f7);--aui-integration-settings-well:var(--aui-bg-3,#f5f5f5);--aui-integration-settings-well-hover:var(--aui-bg-3,#efefef);--aui-integration-settings-field:var(--aui-bg-1,#fff);--aui-integration-settings-search:var(--aui-bg-3,#f5f5f5);--aui-integration-settings-fg:var(--aui-fg,#171717);--aui-integration-settings-body:var(--aui-fg-dim,#404040);--aui-integration-settings-muted:var(--aui-fg-muted,#8f8f8f);--aui-integration-settings-placeholder:var(--aui-fg-faint,#b5b5b5);--aui-integration-settings-line:var(--aui-line,#e5e5e5);--aui-integration-settings-field-line:var(--aui-line,#e2e2e2);--aui-integration-settings-divider:var(--aui-line-soft,#f0f0f0);--aui-integration-settings-hover-line:var(--aui-line,#d4d4d4);--aui-integration-settings-accent:var(--aui-accent,#4fb0fe);--aui-integration-settings-selected:var(--aui-accent,#4fb0fe);--aui-integration-settings-focus:var(--aui-integration-settings-focus-color,rgb(0 96 226 / 70%));--aui-integration-settings-ok:var(--aui-ok,#1e8a4f);--aui-integration-settings-err:var(--aui-err,#f04438);--aui-integration-settings-key-bg:var(--aui-ok-soft,#f3faf6);--aui-integration-settings-key-line:var(--aui-ok-line,#d0eddc);--aui-integration-settings-ease:var(--aui-ease,cubic-bezier(.22,1,.36,1));--aui-integration-settings-r-lg:var(--aui-r-lg,16px);--aui-integration-settings-r-md:var(--aui-r-md,10px);--aui-integration-settings-r-sm:var(--aui-r-sm,7px);--aui-integration-settings-off:var(--aui-bg-3,#d9d9d9);--aui-integration-settings-icon-hover:var(--aui-bg-2,#0000000d);--aui-integration-settings-shadow:var(--aui-shadow-sm,0 1px 2px #0000000f);--aui-integration-settings-popup-shadow:var(--aui-shadow-md,0 0 0 1px #e5e5e5,0 14px 36px -10px #00000038);--aui-integration-settings-field-focus-shadow:0 1px 2px #00000008;--aui-integration-settings-error-glow:var(--aui-err-soft,#f044381a);--aui-integration-settings-save-bg:var(--aui-save-background,linear-gradient(#323137,#201e25));--aui-integration-settings-save-hover:var(--aui-save-hover-background,linear-gradient(#3b3a41,#26242b));--aui-integration-settings-save-fg:var(--aui-accent-contrast,#fff);--aui-integration-settings-save-shadow:0 0 0 1px #0d0d0d,0 2px 4px #0000001a,inset 0 1px #ffffff1f;container-type:inline-size;width:100%;max-width:560px;min-width:0;color:var(--aui-integration-settings-fg);font:14px/1.5 var(--aui-font-sans,Inter,ui-sans-serif,system-ui,sans-serif);word-break:keep-all;overflow-wrap:anywhere;-webkit-tap-highlight-color:transparent}
:where(.dark,[data-theme="dark"]) .aui-integration-settings{--aui-integration-settings-tray:var(--aui-bg-2,#161618);--aui-integration-settings-card:var(--aui-bg-1,#232326);--aui-integration-settings-raised:var(--aui-bg-3,#2a2a2d);--aui-integration-settings-raised-hover:var(--aui-bg-3,#313135);--aui-integration-settings-well:var(--aui-bg-3,#2a2a2d);--aui-integration-settings-well-hover:var(--aui-bg-3,#313135);--aui-integration-settings-field:var(--aui-bg-3,#2a2a2d);--aui-integration-settings-search:var(--aui-bg-2,#1f1f21);--aui-integration-settings-fg:var(--aui-fg,#ededed);--aui-integration-settings-body:var(--aui-fg-dim,#c4c4c4);--aui-integration-settings-muted:var(--aui-fg-muted,#8f8f8f);--aui-integration-settings-placeholder:var(--aui-fg-faint,#6b6b70);--aui-integration-settings-line:var(--aui-line,#ffffff14);--aui-integration-settings-field-line:var(--aui-line,#ffffff14);--aui-integration-settings-divider:var(--aui-line-soft,#ffffff14);--aui-integration-settings-hover-line:var(--aui-line,#ffffff24);--aui-integration-settings-selected:var(--aui-info,var(--aui-accent,#4fb0fe));--aui-integration-settings-focus:var(--aui-integration-settings-focus-color,rgb(77 141 255 / 80%));--aui-integration-settings-ok:#3fb971;--aui-integration-settings-err:var(--aui-err,#ff6b5f);--aui-integration-settings-key-bg:var(--aui-ok-soft,#7dcf9f24);--aui-integration-settings-key-line:var(--aui-ok-line,#7dcf9f4d);--aui-integration-settings-off:var(--aui-bg-3,#3a3a3e);--aui-integration-settings-icon-hover:var(--aui-bg-2,#ffffff0f);--aui-integration-settings-shadow:var(--aui-shadow-sm,0 1px 2px #0006);--aui-integration-settings-popup-shadow:var(--aui-shadow-md,0 0 0 1px #ffffff1a,0 14px 36px -10px #0000008c);--aui-integration-settings-field-focus-shadow:0 1px 2px #0000004d;--aui-integration-settings-error-glow:var(--aui-err-soft,#ff6b5f29);--aui-integration-settings-save-bg:var(--aui-save-background,linear-gradient(#f4f4f5,#e4e4e7));--aui-integration-settings-save-hover:var(--aui-save-hover-background,linear-gradient(#fff,#ececef));--aui-integration-settings-save-fg:var(--aui-accent-contrast,#121213);--aui-integration-settings-save-shadow:0 0 0 1px #0009,0 2px 4px #00000059,inset 0 1px #fff}
.aui-integration-settings *,.aui-integration-settings *::before,.aui-integration-settings *::after{box-sizing:border-box}.aui-integration-settings svg{flex:none;stroke-width:1.4}.aui-integration-settings :where(button,input){font:inherit;color:inherit}.aui-integration-settings button{cursor:pointer;touch-action:manipulation}.aui-integration-settings button:disabled{cursor:default;opacity:.5}.aui-integration-settings button:focus-visible,.aui-integration-settings-search:focus-visible{outline:2px solid var(--aui-integration-settings-focus);outline-offset:2px}.aui-integration-settings-tray{padding:6px;border-radius:calc(var(--aui-integration-settings-r-lg) + 6px);background:var(--aui-integration-settings-tray)}.aui-integration-settings-header{display:flex;align-items:center;gap:14px;padding:14px 16px 14px 14px}.aui-integration-settings-logo{flex:0 0 40px;width:40px;height:40px;display:grid;place-items:center;border:1px solid var(--aui-integration-settings-line);border-radius:11px;overflow:hidden;background:var(--aui-integration-settings-raised);box-shadow:var(--aui-shadow-sm,0 1px 2px #0000000a);color:var(--aui-integration-settings-body)}.aui-integration-settings-logo>img{display:block;width:22px;height:22px;object-fit:contain}.aui-integration-settings-initial{font-size:16px;font-weight:600;line-height:1;color:var(--aui-integration-settings-body)}.aui-integration-settings-heading{min-width:0;flex:1}.aui-integration-settings-heading>div{display:flex;align-items:center;flex-wrap:wrap;gap:4px 8px}.aui-integration-settings-heading h2{margin:0;font-size:15px;line-height:22px;font-weight:500;min-width:0;overflow-wrap:anywhere;text-align:start}.aui-integration-settings-count{flex:none;white-space:nowrap;padding:0 6px;border-radius:calc(var(--aui-integration-settings-r-sm) - 1px);box-shadow:inset 0 0 0 1px var(--aui-integration-settings-field-line);font-size:11.5px;font-weight:500;line-height:18px;font-variant-numeric:tabular-nums;color:var(--aui-integration-settings-body)}.aui-integration-settings-heading p{margin:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--aui-integration-settings-muted);font-size:13px;line-height:19px;text-align:start}.aui-integration-settings-switch{display:grid;place-items:center;flex:0 0 48px;width:48px;height:44px;padding:0;border:0;border-radius:calc(var(--aui-integration-settings-r-sm) + 1px);background:transparent}.aui-integration-settings-switch>span{position:relative;width:40px;height:24px;border-radius:999px;background:var(--aui-integration-settings-off);transition:background 220ms ease}.aui-integration-settings-switch>span>span{position:absolute;top:3px;left:3px;width:18px;height:18px;border-radius:50%;background:var(--aui-switch-thumb,#fff);box-shadow:var(--aui-shadow-sm,0 1px 3px #0000002e);transition:transform 280ms var(--aui-integration-settings-ease)}.aui-integration-settings-switch[aria-checked="true"]>span{background:var(--aui-integration-settings-accent)}.aui-integration-settings-switch[aria-checked="true"]>span>span{transform:translateX(16px)}:where(.dark,[data-theme="dark"]) .aui-integration-settings-switch>span>span{box-shadow:var(--aui-shadow-sm,0 1px 3px #00000073)}
.aui-integration-settings-collapse{display:grid;grid-template-rows:0fr;opacity:0;visibility:hidden;transition:grid-template-rows 420ms var(--aui-integration-settings-ease),opacity 260ms ease,visibility 0s 420ms}.aui-integration-settings-collapse[data-open="true"]{grid-template-rows:1fr;opacity:1;visibility:visible;transition-delay:0s}.aui-integration-settings-collapse-inner{min-width:0;min-height:0;overflow:hidden}.aui-integration-settings-collapse[data-open="true"]>.aui-integration-settings-collapse-inner{overflow:visible}.aui-integration-settings-card{border:1px solid var(--aui-integration-settings-line);border-radius:var(--aui-integration-settings-r-lg);background:var(--aui-integration-settings-card);box-shadow:var(--aui-integration-settings-shadow)}.aui-integration-settings-section{min-width:0;padding:16px 18px 18px}.aui-integration-settings-section+.aui-integration-settings-section{border-top:1px solid var(--aui-integration-settings-divider)}.aui-integration-settings-label{display:block;margin:0 0 8px;font-size:14px;font-weight:500;line-height:20px}.aui-integration-settings-generate{display:flex;align-items:center;justify-content:space-between;gap:12px;width:100%;height:42px;padding:0 14px;border:0;border-radius:calc(var(--aui-integration-settings-r-md) + 1px);background:var(--aui-integration-settings-well);color:var(--aui-integration-settings-muted);text-align:start;transition:background 160ms ease,color 160ms ease}.aui-integration-settings-generate:hover:not(:disabled){background:var(--aui-integration-settings-well-hover);color:var(--aui-integration-settings-body)}.aui-integration-settings-key-row{display:flex;align-items:center;gap:4px;height:42px;padding:0 6px 0 14px;border-radius:calc(var(--aui-integration-settings-r-md) + 1px);background:var(--aui-integration-settings-well);animation:aui-integration-settings-key-in 320ms var(--aui-integration-settings-ease)}.aui-integration-settings-key-row[data-revealed="true"]{background:var(--aui-integration-settings-key-bg);box-shadow:inset 0 0 0 1px var(--aui-integration-settings-key-line)}.aui-integration-settings-key{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font:12.5px/1.5 var(--aui-font-mono,ui-monospace,SFMono-Regular,Consolas,monospace);color:var(--aui-integration-settings-body)}.aui-integration-settings-key-actions{display:flex;align-items:center;flex:none;gap:4px}.aui-integration-settings-icon-button{display:grid;place-items:center;flex:none;width:30px;height:30px;padding:0;border:0;border-radius:calc(var(--aui-integration-settings-r-sm) + 1px);background:transparent;color:var(--aui-integration-settings-muted);transition:background 140ms ease,color 140ms ease}.aui-integration-settings-icon-button:hover{background:var(--aui-integration-settings-icon-hover);color:var(--aui-integration-settings-body)}.aui-integration-settings-regenerate{display:flex;align-items:center;justify-content:center;gap:5px;min-width:92px;height:30px;margin-left:2px;padding:0 10px;border:1px solid var(--aui-integration-settings-line);border-radius:calc(var(--aui-integration-settings-r-sm) + 1px);background:var(--aui-integration-settings-raised);box-shadow:var(--aui-shadow-sm,0 1px 2px #0000000a);font-size:12.5px!important;font-weight:500!important;color:var(--aui-integration-settings-body)}.aui-integration-settings-regenerate:hover:not(:disabled){background:var(--aui-integration-settings-raised-hover)}.aui-integration-settings-key-notice{margin:8px 0 0;font-size:12.5px;color:var(--aui-integration-settings-ok)}.aui-integration-settings-domain{display:flex;align-items:center;gap:4px;height:42px;padding:0 14px;min-width:0;border:1px solid var(--aui-integration-settings-field-line);border-radius:calc(var(--aui-integration-settings-r-md) + 1px);background:var(--aui-integration-settings-field);box-shadow:var(--aui-shadow-sm,0 1px 2px #00000008);transition:border-color 160ms ease,box-shadow 160ms ease}.aui-integration-settings-domain>span{color:var(--aui-integration-settings-placeholder);font-size:14px;flex:none}.aui-integration-settings-domain input{width:100%;height:100%;min-width:0;flex:1;padding:0;border:0;outline:none;background:transparent;font-size:14px}.aui-integration-settings-domain input::placeholder{color:var(--aui-integration-settings-placeholder)}.aui-integration-settings-domain:focus-within{border-color:color-mix(in srgb,var(--aui-integration-settings-muted) 60%,transparent);box-shadow:var(--aui-integration-settings-field-focus-shadow)}.aui-integration-settings-domain[data-invalid="true"]{border-color:var(--aui-integration-settings-err);box-shadow:0 0 0 4px var(--aui-integration-settings-error-glow)}.aui-integration-settings-valid{color:var(--aui-integration-settings-ok);animation:aui-integration-settings-key-in 240ms var(--aui-integration-settings-ease)}.aui-integration-settings-value{margin:0;font-size:14px;line-height:20px;color:var(--aui-integration-settings-body);word-break:keep-all;overflow-wrap:anywhere}.aui-integration-settings-reason{margin:-6px 16px 10px;font-size:13px;line-height:18px;color:var(--aui-integration-settings-muted);word-break:keep-all;overflow-wrap:anywhere}.aui-integration-settings [data-unavailable]{opacity:.5;cursor:not-allowed}.aui-integration-settings-hint{margin:8px 0 0;font-size:13px;line-height:18px;color:var(--aui-integration-settings-muted);overflow-wrap:anywhere;text-align:start}.aui-integration-settings-invalid{color:var(--aui-integration-settings-err)}.aui-integration-settings-zone{position:relative;min-width:0}.aui-integration-settings-zone-button{display:flex;align-items:center;justify-content:space-between;gap:8px;width:100%;min-height:42px;padding:9px 14px;text-align:start;border:1px solid var(--aui-integration-settings-field-line);border-radius:calc(var(--aui-integration-settings-r-md) + 1px);background:var(--aui-integration-settings-field);box-shadow:var(--aui-shadow-sm,0 1px 2px #00000008);font-size:14px!important}.aui-integration-settings-zone-button>span{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.aui-integration-settings-zone-button>svg{color:var(--aui-integration-settings-muted)}.aui-integration-settings-zone-button:hover{border-color:var(--aui-integration-settings-hover-line)}.aui-integration-settings-menu{position:absolute;top:calc(100% + 6px);left:0;z-index:10;width:100%;min-width:0;padding:6px;border-radius:calc(var(--aui-integration-settings-r-lg) - 2px);background:var(--aui-integration-settings-raised);box-shadow:var(--aui-integration-settings-popup-shadow);animation:aui-integration-settings-menu-in 180ms var(--aui-integration-settings-ease)}.aui-integration-settings-search{display:block;width:100%;min-width:0;height:34px;margin:0 0 4px;padding:0 10px;border:0;border-radius:calc(var(--aui-integration-settings-r-md) - 1px);background:var(--aui-integration-settings-search);color:var(--aui-integration-settings-fg);font-size:13px!important}.aui-integration-settings-search::placeholder{color:var(--aui-integration-settings-placeholder)}.aui-integration-settings-options{display:flex;flex-direction:column;gap:2px;max-height:200px;overflow-y:auto;overscroll-behavior:contain;scrollbar-width:thin}.aui-integration-settings-option{display:flex;align-items:center;justify-content:space-between;gap:10px;width:100%;min-height:32px;flex:none;padding:6px 10px;border:0;border-radius:calc(var(--aui-integration-settings-r-sm) + 1px);background:transparent;color:var(--aui-integration-settings-body);font-size:13px!important;text-align:start}.aui-integration-settings-option:hover,.aui-integration-settings-option[data-active="true"]{background:var(--aui-integration-settings-well-hover)}.aui-integration-settings-option[aria-selected="true"]{color:var(--aui-integration-settings-selected);font-weight:500}.aui-integration-settings-empty{margin:0;padding:12px 10px;color:var(--aui-integration-settings-muted);font-size:13px}
.aui-integration-settings-footer-collapse{display:grid;grid-template-rows:0fr;visibility:hidden;transition:grid-template-rows 360ms var(--aui-integration-settings-ease),visibility 0s 360ms}.aui-integration-settings-footer-collapse[data-open="true"]{grid-template-rows:1fr;visibility:visible;transition-delay:0s}.aui-integration-settings-footer-inner{min-width:0;min-height:0;overflow:hidden}.aui-integration-settings-savebar{min-width:0;display:flex;align-items:center;justify-content:space-between;gap:12px;padding:8px 8px 8px 18px;color:var(--aui-integration-settings-body);font-size:13px}.aui-integration-settings-savebar>div{display:flex;gap:6px;flex:none}.aui-integration-settings-discard,.aui-integration-settings-save{display:flex;align-items:center;justify-content:center;gap:6px;min-height:32px;padding:0 12px;border:0;border-radius:var(--aui-integration-settings-r-md);font-size:13px!important;font-weight:500!important}.aui-integration-settings-discard{background:var(--aui-integration-settings-raised);box-shadow:var(--aui-shadow-sm,0 0 0 1px var(--aui-integration-settings-line),0 1px 2px #0000000a)}.aui-integration-settings-discard:hover{background:var(--aui-integration-settings-raised-hover)}.aui-integration-settings-save{background:var(--aui-integration-settings-save-bg);color:var(--aui-integration-settings-save-fg)!important;box-shadow:var(--aui-integration-settings-save-shadow)}.aui-integration-settings-save:hover:not(:disabled){background:var(--aui-integration-settings-save-hover)}.aui-integration-settings-error{display:flex;align-items:flex-start;gap:6px;margin:8px 0 0;color:var(--aui-integration-settings-err);font-size:12.5px}.aui-integration-settings-error>svg{margin-top:3px}.aui-integration-settings-message{padding:0 18px 18px}.aui-integration-settings-sr-only{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip-path:inset(50%);white-space:nowrap;border:0}.aui-integration-settings-spinner{animation:aui-integration-settings-spin 700ms linear infinite}
.aui-integration-settings-overlay{container-type:normal;position:fixed;z-index:1000;inset:var(--aui-integration-settings-visible-top,0px) 0 auto;display:flex;align-items:flex-end;width:100%;max-width:none;height:var(--aui-integration-settings-visible-height,100dvh);padding:env(safe-area-inset-top) env(safe-area-inset-right) 0 env(safe-area-inset-left)}.aui-integration-settings-backdrop{position:absolute;inset:0;background:var(--aui-overlay,#0007)}.aui-integration-settings-sheet{position:relative;inset:auto;width:100%;max-height:min(620px,100%);display:flex;flex-direction:column;padding:12px max(16px,env(safe-area-inset-right)) max(16px,env(safe-area-inset-bottom)) max(16px,env(safe-area-inset-left));border-radius:calc(var(--aui-integration-settings-r-lg) + 6px) calc(var(--aui-integration-settings-r-lg) + 6px) 0 0}.aui-integration-settings-sheet-header{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:8px}.aui-integration-settings-sheet-header h3{margin:0;font-size:16px;font-weight:600}.aui-integration-settings-close{display:grid;place-items:center;width:44px;height:44px;padding:0;border:0;border-radius:calc(var(--aui-integration-settings-r-md) - 1px);background:var(--aui-integration-settings-well);color:var(--aui-integration-settings-body)}.aui-integration-settings-sheet .aui-integration-settings-search{flex:none;height:44px}.aui-integration-settings-sheet .aui-integration-settings-options{max-height:none;min-height:0;flex:1;touch-action:pan-y}.aui-integration-settings-sheet .aui-integration-settings-option{min-height:44px}
@container(max-width:440px){.aui-integration-settings-header{gap:10px;padding-inline:10px}.aui-integration-settings-key-row{height:auto;display:grid;grid-template-columns:minmax(0,1fr);gap:5px;padding:8px}.aui-integration-settings-key{width:100%;padding:2px 6px;min-height:26px}.aui-integration-settings-key-actions{justify-content:flex-end}.aui-integration-settings-savebar{flex-direction:column;align-items:stretch;padding:10px}.aui-integration-settings-savebar>div{width:100%}.aui-integration-settings-savebar>div>button{flex:1}}
@media(pointer:coarse){.aui-integration-settings-generate,.aui-integration-settings-key-row,.aui-integration-settings-domain,.aui-integration-settings-zone-button,.aui-integration-settings-search,.aui-integration-settings-option,.aui-integration-settings-discard,.aui-integration-settings-save{min-height:44px}.aui-integration-settings-domain{height:46px}.aui-integration-settings-domain input{height:44px}.aui-integration-settings-generate{height:44px}.aui-integration-settings-icon-button{width:44px;height:44px}.aui-integration-settings-regenerate{min-height:44px}.aui-integration-settings-key-row{height:auto;padding-block:4px}.aui-integration-settings-search{height:44px}}@media(pointer:coarse),(max-width:899px){.aui-integration-settings-domain input{font-size:16px}.aui-integration-settings-search{font-size:16px!important}}
@keyframes aui-integration-settings-spin{to{transform:rotate(360deg)}}@keyframes aui-integration-settings-key-in{from{opacity:0;transform:translateY(2px)}to{opacity:1;transform:none}}@keyframes aui-integration-settings-menu-in{from{opacity:0;transform:translateY(-4px) scale(.98)}to{opacity:1;transform:none}}
@media(prefers-reduced-motion:reduce){.aui-integration-settings *,.aui-integration-settings *::before,.aui-integration-settings *::after{animation:none!important;transition:none!important;scroll-behavior:auto!important}}
}
`;
