import { useEffect, useRef, useState, type CSSProperties, type ReactNode } from 'react';

// Clean-room card from the leak-checked stat-card spec. No Pro source.
export interface StatCardProps {
  label: string;
  value: number | string;
  format?: (value: number) => string;
  delta?: number;
  formatDelta?: (value: number) => string;
  deltaLabel?: string;
  lowerIsBetter?: boolean;
  icon?: ReactNode;
  chart?: ReactNode;
  layout?: 'stack' | 'beside';
  footer?: ReactNode;
  loading?: boolean;
  href?: string;
  onPress?: () => void;
  action?: ReactNode;
  active?: boolean;
  className?: string;
  style?: CSSProperties;
}
export function statNumber(value: number) {
  const absolute = Math.abs(value);
  if (absolute >= 1_000_000) return new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 }).format(value);
  if (absolute < 10) return new Intl.NumberFormat('en-US', { maximumFractionDigits: 2 }).format(value);
  return new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 }).format(value);
}
export function statDelta(value: number) {
  const absolute = Math.abs(value);
  const text = Number.isInteger(absolute) ? String(absolute) : absolute.toFixed(1);
  return `${text}%`;
}

const styles = `
.aui-stat-card { --aui-stat-surface: #fff; --aui-stat-ink: #171717; --aui-stat-muted: #8f8f8f; --aui-stat-line: #ebebeb; --aui-stat-ok: #1e8a4f; --aui-stat-err: #f04438; --aui-stat-bone: #f0f0f0; --aui-stat-focus: rgba(0,96,226,.7); --aui-stat-shadow: 0 1px 2px rgb(0 0 0 / .03), 0 8px 24px rgb(0 0 0 / .035); --aui-stat-shadow-hover: 0 1px 2px rgb(0 0 0 / .04), 0 12px 32px rgb(0 0 0 / .07); position: relative; display: flex; flex-direction: column; width: 100%; min-width: 0; border: 1px solid var(--aui-stat-line); border-radius: 20px; background: var(--aui-stat-surface); color: var(--aui-stat-ink); box-shadow: var(--aui-stat-shadow); font: 14px/20px Inter, ui-sans-serif, system-ui, sans-serif; font-variant-numeric: tabular-nums; }
:where(.dark,[data-theme="dark"]) .aui-stat-card { --aui-stat-surface: #232326; --aui-stat-ink: #ededed; --aui-stat-muted: #8f8f8f; --aui-stat-line: rgb(255 255 255 / .08); --aui-stat-ok: #5fc88a; --aui-stat-err: #ff6b5f; --aui-stat-bone: rgb(255 255 255 / .06); --aui-stat-focus: rgba(77,141,255,.8); --aui-stat-shadow: 0 1px 2px rgb(0 0 0 / .4), 0 8px 24px rgb(0 0 0 / .35); --aui-stat-shadow-hover: 0 1px 2px rgb(0 0 0 / .45), 0 12px 32px rgb(0 0 0 / .5); }
.aui-stat-card-body { display: flex; flex-direction: column; gap: 16px; min-width: 0; padding: 20px; }
.aui-stat-card-body[data-dim] { opacity: .45; transition: opacity 260ms ease; }
.aui-stat-card:hover .aui-stat-card-body[data-dim] { opacity: .75; }
.aui-stat-card-top { display: flex; gap: 8px; align-items: center; min-width: 0; color: var(--aui-stat-muted); }
.aui-stat-card-top svg { width: 16px; height: 16px; flex: none; }
.aui-stat-card-top span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.aui-stat-card-value { margin-top: 6px; font-size: 28px; font-weight: 500; line-height: 34px; letter-spacing: -.02em; overflow-wrap: anywhere; }
.aui-stat-card-delta { display: flex; gap: 8px; align-items: center; margin-top: 4px; min-width: 0; font-size: 13px; line-height: 20px; }
.aui-stat-card-delta b { display: inline-flex; gap: 4px; align-items: center; font-size: 14px; font-weight: 450; white-space: nowrap; }
.aui-stat-card-delta em { overflow: hidden; color: var(--aui-stat-muted); font-style: normal; text-overflow: ellipsis; white-space: nowrap; }
.aui-stat-card-row { display: grid; grid-template-columns: minmax(0,1fr) minmax(0,44%); gap: 20px; align-items: end; }
.aui-stat-card-foot { margin-top: auto; padding: 0 20px 18px; color: var(--aui-stat-muted); font-size: 13px; line-height: 18px; overflow-wrap: anywhere; }
.aui-stat-card-hit { position: absolute; inset: 0; border-radius: inherit; }
.aui-stat-card-hit:focus { outline: none; }
.aui-stat-card-hit:focus-visible { outline: 2px solid var(--aui-stat-focus); outline-offset: 2px; }
.aui-stat-card-action { position: absolute; top: -4px; right: -6px; z-index: 2; }
.aui-stat-card-chart, .aui-stat-card-action { position: relative; z-index: 1; }
.aui-stat-card[data-link]:hover { transform: translateY(-1px); box-shadow: var(--aui-stat-shadow-hover); transition: transform 200ms cubic-bezier(.22,1,.36,1), box-shadow 200ms ease; }
.aui-stat-card-bone { position: relative; overflow: hidden; border-radius: 6px; background: var(--aui-stat-bone); }
.aui-stat-card-bone::after { position: absolute; inset: 0; background: linear-gradient(90deg, transparent, rgb(255 255 255 / .8), transparent); content: ""; animation: aui-stat-card-sheen 1600ms ease-in-out infinite; }
:where(.dark,[data-theme="dark"]) .aui-stat-card-bone::after { background: linear-gradient(90deg, transparent, rgb(255 255 255 / .06), transparent); }
@keyframes aui-stat-card-sheen { from { transform: translateX(-100%); } to { transform: translateX(100%); } }
@container aui-stat (max-width: 359px) { .aui-stat-card-row { grid-template-columns: minmax(0,1fr); } }
@media (prefers-reduced-motion: reduce) {
  .aui-stat-card, .aui-stat-card-bone::after { animation: none !important; transition: none !important; }
  .aui-stat-card[data-link]:hover { transform: none; }
}
`;

function useRoll(target: number, active: boolean) {
  const [shown, setShown] = useState(active ? 0 : target);
  const from = useRef(shown);
  useEffect(() => {
    if (!active || matchMedia('(prefers-reduced-motion: reduce)').matches) { setShown(target); from.current = target; return; }
    const start = performance.now();
    const origin = from.current;
    let frame = 0;
    const tick = (now: number) => {
      const t = Math.min(1, (now - start) / 720);
      const eased = 1 - (1 - t) ** 4;
      const next = origin + (target - origin) * eased;
      setShown(next);
      if (t < 1) frame = requestAnimationFrame(tick);
      else from.current = target;
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [target, active]);
  return shown;
}

export function StatCard({ label, value, format = statNumber, delta, formatDelta = statDelta, deltaLabel, lowerIsBetter = false, icon, chart, layout = 'stack', footer, loading = false, href, onPress, action, active = true, className = '', style }: StatCardProps) {
  const numeric = typeof value === 'number' && Number.isFinite(value);
  const rolled = useRoll(numeric ? value : 0, numeric && !loading);
  const text = numeric ? format(loading ? value : rolled) : String(value);
  const good = delta === undefined ? null : delta === 0 ? 'flat' : (delta > 0) !== lowerIsBetter ? 'good' : 'bad';
  const name = `${label}, ${loading ? '불러오는 중' : text}${delta !== undefined && Number.isFinite(delta) ? `, ${delta > 0 ? '증가' : delta < 0 ? '감소' : '변화 없음'} ${formatDelta(Math.abs(delta))}` : ''}`;
  const body = (
    <>
      <div className="aui-stat-card-top">{icon}{icon ? null : null}<span>{label}</span></div>
      {loading ? <div className="aui-stat-card-bone" style={{ width: 112, height: 28 }} /> : <div className="aui-stat-card-value" data-value={text}>{text}</div>}
      {delta !== undefined && Number.isFinite(delta) && (loading ? <div className="aui-stat-card-bone" style={{ width: 132, height: 16 }} /> : (
        <div className="aui-stat-card-delta" data-tone={good}>
          <b style={{ color: good === 'good' ? 'var(--aui-stat-ok)' : good === 'bad' ? 'var(--aui-stat-err)' : 'var(--aui-stat-muted)' }}>
            {good !== 'flat' && <svg viewBox="0 0 8 8" width="8" height="8" aria-hidden="true" style={{ transform: good === 'bad' ? 'scaleY(-1)' : undefined }}><path d="M4 1 L7 7 H1 Z" fill="currentColor" /></svg>}
            {formatDelta(Math.abs(delta))}
          </b>
          {deltaLabel && <em title={deltaLabel}>{deltaLabel}</em>}
        </div>
      ))}
    </>
  );
  return (
    <article className={`aui-stat-card ${className}`} data-link={href || onPress ? '' : undefined} aria-busy={loading || undefined} style={{ containerType: 'inline-size', containerName: 'aui-stat', ...style }}>
      <style>{styles}</style>
      {href ? <a className="aui-stat-card-hit" href={href} aria-label={name} /> : onPress ? <button type="button" className="aui-stat-card-hit" aria-label={name} onClick={onPress} /> : <span className="aui-stat-card-sr" style={{ position: 'absolute', width: 1, height: 1, overflow: 'hidden' }}>{name}</span>}
      {action && <div className="aui-stat-card-action">{action}</div>}
      <div className="aui-stat-card-body" data-dim={active ? undefined : ''}>
        {layout === 'beside' ? <div className="aui-stat-card-row"><div>{body}</div><div className="aui-stat-card-chart">{loading && chart ? <div className="aui-stat-card-bone" style={{ width: '100%', height: 44 }} /> : chart}</div></div> : <>{body}<div className="aui-stat-card-chart">{loading && chart ? <div className="aui-stat-card-bone" style={{ width: '100%', height: 44 }} /> : chart}</div></>}
      </div>
      {footer && <div className="aui-stat-card-foot">{footer}</div>}
    </article>
  );
}

export function StatCardDemo({ state = 'default' }: { state?: 'default' | 'loading' | 'down' | 'flat' | 'beside' | 'long' | 'empty' }) {
  const spark = <svg viewBox="0 0 280 48" width="100%" height="48" aria-hidden="true"><path d="M0 28 L46 14 L93 24 L140 6 L186 18 L233 36 L280 40" fill="none" stroke="#0190fe" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" /></svg>;
  const width = '100%';
  const maxWidth = 360;
  if (state === 'loading') return <div style={{ width, maxWidth }}><StatCard label="Weekly credits" value={6420} delta={12.4} deltaLabel="vs last week" chart={spark} footer="Resets in 4 hours" loading /></div>;
  if (state === 'empty') return <div style={{ width, maxWidth }}><StatCard label="Weekly credits" value={0} footer="Resets in 4 hours" /></div>;
  if (state === 'down') return <div style={{ width, maxWidth }}><StatCard label="Reply latency" value={1.8} delta={-3} lowerIsBetter deltaLabel="vs last week" /></div>;
  if (state === 'flat') return <div style={{ width, maxWidth }}><StatCard label="Weekly credits" value={6420} delta={0} /></div>;
  if (state === 'beside') return <div style={{ width: '100%', maxWidth: 520 }}><StatCard label="Weekly credits" value={6420} delta={12.4} chart={spark} layout="beside" href="#credits" active={false} /></div>;
  if (state === 'long') return <div style={{ width: '100%', maxWidth: 280 }}><StatCard label="아주 긴 주간 크레딧 지표 이름과 영문 weekly-credit-ledger-identifier" value="128400.55 credits" delta={12.4} deltaLabel="지난 주 같은 기간과 비교한 매우 긴 설명" footer="초기화까지 남은 시간이 길어도 카드 안에서 줄바꿈됩니다." /></div>;
  return <div style={{ width, maxWidth }}><StatCard label="Weekly credits" value={6420} delta={12.4} deltaLabel="vs last week" chart={spark} footer="Resets in 4 hours" /></div>;
}
