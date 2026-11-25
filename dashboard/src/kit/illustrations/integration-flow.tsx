"use client";

import { useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";

export interface IntegrationFlowItem {
  name: string;
  title?: string;
  mark?: ReactNode;
  mono?: boolean;
}

export interface IntegrationFlowProps {
  label?: string;
  items?: IntegrationFlowItem[];
  className?: string;
  style?: CSSProperties;
}

const WASH_LIGHT = [
  "#fe8971 #fc806e #fa766b #f8706a #f8716c #f77471 #f87d7a #f98d89 #fbada9 #fdc0bb",
  "#fe886f #fd816e #fb796d #f9766e #f87670 #f87974 #f87f7b #f98d89 #fba09b #fdb5af",
  "#fe896d #fd836e #fb7d6f #fa7c72 #f97c75 #f87d78 #f9837d #f98e89 #fca099 #fdb4ac",
  "#fe8a6d #fe866f #fc8070 #fb8276 #fa847a #f9847c #fa9a96 #faa19c #fcafa8 #fdb4ab",
  "#ff8a6b #fe886d #fc8471 #fc877a #fa8a80 #fa8a82 #faa6a0 #faa09b #fcada5 #fdb4aa",
  "#ff8c69 #fe8a6d #fd8772 #fd8a7a #fb9183 #fa9086 #fa9892 #fa9f98 #fcaba2 #fdb4a7",
  "#fe8e6a #fe896c #fd8770 #fd8c7a #fc9285 #fa9288 #fa938a #fa988f #fba498 #fdb4a5",
  "#fe906c #fe8c6c #fd886e #fd8d78 #fb9283 #fa9589 #f9958d #f99a91 #faa497 #fdb2a1",
  "#fe936d #fe8f6c #fd8b6f #fd8c79 #fb917e #fa9788 #fa968b #fa9990 #faa395 #fcb19e",
  "#fd9371 #fd916e #fd8e6e #fc8d76 #fc917b #f99686 #f9968c #f8998f #f8a091 #fbaf9c",
  "#fd9575 #f58667 #f58262 #f58366 #fb8e76 #f99282 #ef8f83 #ee8f83 #f79c8c #faac97",
  "#fc9f85 #fb9578 #fc9172 #fc9274 #fb967e #f89b8b #f6998e #f5998f #f7a193 #f8b19e",
  "#fcad9a #fca58c #fda286 #fda287 #fca691 #faa99b #f7b3ac #f7b0a9 #f7b6ad #f9b8ab",
  "#fcbaac #fcb4a0 #fcb39b #fcb39c #fbb6a3 #f9b7ac #f9c4be #f6b8b3 #f6bdb6 #f8c1b8",
  "#fcc8bd #fdc4b4 #fdc4b1 #fdc5b2 #fdc7b9 #fac7bf #f9c8c4 #f8c4c0 #f8c6c2 #f9ccc6",
  "#fed8d0 #fed6cb #fed6c8 #fed7ca #fedacf #fcd9d3 #fad4d0 #f9cfcd #f9d0ce #fad9d6",
  "#fee9e4 #fee8e2 #ffe8e1 #ffe9e2 #feebe6 #feebe8 #fce7e5 #fbe3e2 #fbe4e3 #fce9e7",
  "#fff5f3 #fff4f1 #fff4f1 #fff4f1 #fff5f2 #fff6f5 #fff6f5 #fff7f7 #fff7f7 #fff8f8",
];
const WASH_DARK = [
  "#c16451 #c05c4c #c15249 #bf4d48 #bd4c48 #b74946 #ad4846 #a34a47 #975552 #895653",
  "#c46653 #c35d4c #c15449 #c04e48 #bc4c46 #b64a46 #ad4846 #a34a48 #984d4a #894f4c",
  "#c76853 #c45f4d #c15448 #c04f47 #bb4c46 #b44a45 #ae4945 #a34b47 #9a4f4b #8d514c",
  "#c86953 #c4604c #c25548 #bf5045 #ba4d44 #b34b44 #b45c58 #aa5b58 #a5615b #8e534c",
  "#c86c52 #c4604b #c15746 #be5044 #b94d42 #b14b43 #ae4f48 #a85853 #a35e57 #8e544b",
  "#c76b50 #c46048 #c15744 #bd5042 #b74d40 #b04b42 #ae534b #a5544d #9f5a51 #90544a",
  "#c56a4d #c16047 #c05744 #bd5040 #b84d3f #b0493f #a84940 #a14a43 #9b5247 #905449",
  "#c0684a #bc5e44 #bc5641 #ba503e #b74d3d #af493e #a4483d #9e493f #9b5044 #925446",
  "#ba6347 #aa543c #a44f37 #ae4d39 #b84c3b #ae493b #964034 #974439 #994e41 #925243",
  "#b25d43 #a6513a #a54f37 #ab4d38 #b54c39 #ab4638 #943e33 #944136 #944b3c #915140",
  "#ac5841 #9a4b35 #9f4a33 #9f4731 #b64d38 #ab4637 #89392e #853930 #994b3b #8f4e3c",
  "#974d3a #a24f38 #a74f37 #a94d34 #a44734 #993f31 #923c32 #8d3d33 #8a4235 #804436",
  "#824335 #8d4634 #924832 #924631 #8d4130 #843a2e #86443c #82423b #80453d #703b30",
  "#6f3a30 #783e2f #7c402e #7c3e2d #763a2c #6e332b #764541 #6e3632 #6c3833 #5f322b",
  "#5d332c #64362b #66382a #65372a #603429 #5a2d27 #5a2f2b #5a2d2a #582e2b #502b27",
  "#4b2d28 #4f2f27 #503026 #4f2f26 #4a2d26 #462825 #462524 #462424 #462524 #402524",
  "#372625 #392724 #3a2824 #382723 #352523 #332322 #332122 #342222 #342222 #312222",
  "#2b2223 #2b2223 #2b2223 #2b2223 #2a2122 #292021 #292021 #292122 #292021 #282021",
];

const STAGE_W = 2400;
const STAGE_H = 1490;
const DEFAULT_TITLE = "API Integration";
const SLOTS = [
  { x: 480, y: -845.68, ghost: true, hide: true },
  { x: 240, y: -562.68, ghost: true, hide: false },
  { x: 80, y: -279.68, ghost: true, hide: false },
  { x: 0, y: 0, ghost: false, hide: false },
  { x: 80, y: 279.32, ghost: true, hide: false },
  { x: 240, y: 558.32, ghost: true, hide: false },
  { x: 480, y: 838.32, ghost: true, hide: true },
];
const BASE_SLOTS = [1, 2, 3, 4, 5, 0];

function Pie() {
  return (
    <svg viewBox="0 0 64 64" aria-hidden="true">
      <defs>
        <mask id="aui-if-pie-mask">
          <rect width="64" height="64" fill="#fff" />
          {[18, 90, 162, 234, 306].map((angle) => (
            <path key={angle} d="M32 32 Q35 18 30 4" fill="none" stroke="#000" strokeWidth="4.2" strokeLinecap="round" transform={`rotate(${angle} 32 32)`} />
          ))}
          <circle cx="32" cy="32" r="2.2" fill="#000" />
        </mask>
      </defs>
      <circle cx="32" cy="32" r="25" fill="currentColor" mask="url(#aui-if-pie-mask)" />
    </svg>
  );
}
function Sun() {
  return (
    <svg viewBox="0 0 64 64" aria-hidden="true">
      <circle cx="32" cy="32" r="11" fill="currentColor" />
      {[0, 30, 60, 90, 120, 150, 180, 210, 240, 270, 300, 330].map((angle) => (
        <rect key={angle} x="30" y="6" width="4" height="10" rx="2" fill="currentColor" transform={`rotate(${angle} 32 32)`} />
      ))}
    </svg>
  );
}
function Flake() {
  return (
    <svg viewBox="0 0 64 64" fill="none" stroke="currentColor" strokeWidth="4.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      {[0, 60, 120].map((angle) => (
        <g key={angle} transform={`rotate(${angle} 32 32)`}>
          <path d="M32 7v17M32 40v17M24 13l8 8 8-8M24 51l8-8 8 8" />
        </g>
      ))}
      <polygon points="32,24 39,28 39,36 32,40 25,36 25,28" strokeWidth="4" />
    </svg>
  );
}
function Hash() {
  return (
    <svg viewBox="0 0 64 64" aria-hidden="true">
      <rect x="6" y="21" width="24" height="10" rx="5" fill="#36c5f0" />
      <path d="M20 11a5 5 0 0 1 10 0v5h-5a5 5 0 0 1-5-5Z" fill="#36c5f0" />
      <rect x="34" y="6" width="10" height="24" rx="5" fill="#2eb67d" />
      <path d="M53 20a5 5 0 0 1 0 10h-5v-5a5 5 0 0 1 5-5Z" fill="#2eb67d" />
      <rect x="34" y="34" width="24" height="10" rx="5" fill="#ecb22e" />
      <path d="M44 53a5 5 0 0 1-10 0v-5h5a5 5 0 0 1 5 5Z" fill="#ecb22e" />
      <rect x="20" y="34" width="10" height="24" rx="5" fill="#e01e5a" />
      <path d="M11 44a5 5 0 0 1 0-10h5v5a5 5 0 0 1-5 5Z" fill="#e01e5a" />
    </svg>
  );
}
function ClickUp() {
  return (
    <svg viewBox="0 0 64 64" aria-hidden="true">
      <path d="M13 28 L32 11 L51 28 L44 35 L32 24 L20 35 Z" fill="#7b68ee" />
      <path d="M15 44 C21 51, 43 51, 49 44 L55 50 C46 60, 18 60, 9 50 Z" fill="#ff5286" />
    </svg>
  );
}
function Zendesk() {
  return (
    <svg viewBox="0 0 64 64" fill="currentColor" aria-hidden="true">
      <polygon points="8,14 30,14 8,40" />
      <path d="M8 50 a11 11 0 0 1 22 0 Z" />
      <path d="M34 14 a11 11 0 0 0 22 0 Z" />
      <polygon points="56,50 34,50 56,24" />
    </svg>
  );
}

const builtin: IntegrationFlowItem[] = [
  { name: "Okta", mark: <Sun />, mono: true },
  { name: "Snowflake", mark: <Flake />, mono: false },
  { name: "Slack", mark: <Hash /> },
  { name: "ClickUp", mark: <ClickUp /> },
  { name: "Zendesk", mark: <Zendesk />, mono: true },
];

type Phase = "static" | "dwell" | "charge" | "beam" | "rest" | "move";

export function IntegrationFlow({
  label = "연동이 분석 허브로 모이는 흐름",
  items,
  className = "",
  style,
}: IntegrationFlowProps) {
  const arcGradId = useId();
  const root = useRef<HTMLDivElement>(null);
  const washRef = useRef<HTMLCanvasElement>(null);
  const [scale, setScale] = useState(0);
  const [reduced, setReduced] = useState(() =>
    typeof window !== "undefined" && typeof window.matchMedia === "function"
      ? window.matchMedia("(prefers-reduced-motion: reduce)").matches
      : true,
  );
  const [cycle, setCycle] = useState(0);
  const [phase, setPhase] = useState<Phase>("static");
  const list = items && items.length ? items : builtin;

  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    const sync = () => setReduced(mq.matches);
    sync();
    mq.addEventListener("change", sync);
    return () => mq.removeEventListener("change", sync);
  }, []);

  useEffect(() => {
    if (reduced) {
      setCycle(0);
      setPhase("static");
      return;
    }
    let cancelled = false;
    const timers: number[] = [];
    const schedule = (fn: () => void, ms: number) => {
      const id = window.setTimeout(() => { if (!cancelled) fn(); }, ms);
      timers.push(id);
    };
    const runCycle = () => {
      setPhase("dwell");
      schedule(() => setPhase("charge"), 320);
      schedule(() => setPhase("beam"), 320 + 620);
      schedule(() => setPhase("rest"), 320 + 620 + 1100);
      schedule(() => {
        setPhase("move");
        setCycle((prev) => prev + 1);
      }, 320 + 620 + 1100 + 260);
      schedule(runCycle, 3200);
    };
    runCycle();
    return () => {
      cancelled = true;
      timers.forEach((id) => clearTimeout(id));
    };
  }, [reduced]);

  useLayoutEffect(() => {
    const node = root.current;
    if (!node) return;
    const measure = () => setScale(node.clientWidth / STAGE_W);
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  useLayoutEffect(() => {
    const paint = () => {
      const canvas = washRef.current;
      if (!canvas) return;
      const dark = document.documentElement.dataset.theme === "dark" || document.documentElement.classList.contains("dark");
      const grid = (dark ? WASH_DARK : WASH_LIGHT).map((row) => row.split(" "));
      const rows = grid.length;
      const cols = grid[0].length;
      const src = document.createElement("canvas");
      src.width = cols;
      src.height = rows;
      const source = src.getContext("2d");
      if (!source) return;
      const pixels = source.createImageData(cols, rows);
      grid.forEach((row, y) => row.forEach((hex, x) => {
        const value = Number.parseInt(hex.slice(1), 16);
        const index = (y * cols + x) * 4;
        pixels.data[index] = (value >> 16) & 255;
        pixels.data[index + 1] = (value >> 8) & 255;
        pixels.data[index + 2] = value & 255;
        pixels.data[index + 3] = 255;
      }));
      source.putImageData(pixels, 0, 0);
      canvas.width = 600;
      canvas.height = 372;
      const context = canvas.getContext("2d");
      if (!context) return;
      context.imageSmoothingEnabled = true;
      context.imageSmoothingQuality = "high";
      context.drawImage(src, 0, 0, canvas.width, canvas.height);
    };
    paint();
    const theme = new MutationObserver(paint);
    theme.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme", "class"] });
    return () => theme.disconnect();
  }, []);

  const stageHeight = scale ? Math.ceil(STAGE_H * scale) : undefined;

  return (
    <div
      ref={root}
      className={`aui-integration-flow ${className}`.trim()}
      style={scale ? { ...style, aspectRatio: "auto" } : style}
      role="img"
      aria-label={label}
      data-phase={reduced ? "static" : phase}
    >
      <style>{css}</style>
      <div className="aui-integration-flow-stage" style={{ height: stageHeight, visibility: scale ? "visible" : "hidden" }} aria-hidden="true">
        <div className="aui-integration-flow-world" style={{ transform: `scale(${scale || 1})` }}>
          <canvas className="aui-integration-flow-wash" ref={washRef} />
          <div className="aui-integration-flow-board">
            <svg className="aui-integration-flow-arc" viewBox="0 0 1519.5 1557.75">
              <defs>
                <linearGradient id={arcGradId} x1="0" y1="0" x2="0" y2="1">
                  <stop offset="0%" stopColor="var(--arc-stop)" stopOpacity="0.18" />
                  <stop offset="30%" stopColor="var(--arc-mid)" stopOpacity="0.72" />
                  <stop offset="70%" stopColor="var(--arc-mid)" stopOpacity="0.72" />
                  <stop offset="100%" stopColor="var(--arc-stop)" stopOpacity="0.12" />
                </linearGradient>
              </defs>
              <ellipse cx="759.75" cy="778.875" rx="757.5" ry="776.625" fill="none" stroke={`url(#${arcGradId})`} />
            </svg>
            <div className="aui-integration-flow-beam" />
            <div className="aui-integration-flow-hub" data-lit={!reduced && (phase === "beam" || phase === "rest") ? "true" : undefined}>
              <span className="aui-integration-flow-badge"><Pie /></span>
              <span className="aui-integration-flow-copy"><strong>Data Analysis</strong><small>Centralized Data Processing</small></span>
            </div>
            {BASE_SLOTS.map((baseSlot, index) => {
              const rawSlot = (baseSlot + cycle) % 6;
              const exiting = !reduced && phase === "move" && cycle > 0 && rawSlot === 0;
              const slotIndex = exiting ? 6 : rawSlot;
              const slot = SLOTS[slotIndex];
              const item = list[index % list.length];
              const instantJump = !reduced && phase !== "move" && rawSlot === 0;
              return (
                <div
                  key={index}
                  className="aui-integration-flow-pill"
                  data-ghost={slot.ghost || undefined}
                  data-hide={slot.hide || undefined}
                  data-mono={item.mono || undefined}
                  data-charged={!slot.ghost && !reduced && (phase === "charge" || phase === "beam") ? "true" : undefined}
                  data-jump={instantJump ? "true" : undefined}
                  style={{ transform: `translate(${slot.x}px, ${slot.y}px)` }}
                >
                  <span className="aui-integration-flow-badge">{item.mark ?? <Sun />}</span>
                  <span className="aui-integration-flow-copy"><strong>{item.title || DEFAULT_TITLE}</strong><small>{item.name}</small></span>
                </div>
              );
            })}
          </div>
          <div className="aui-integration-flow-bottom-fade" />
        </div>
      </div>
    </div>
  );
}

const css = `
.aui-integration-flow{--arc-mid:#ffffff;--arc-stop:#ffffff;width:100%;min-width:0;max-width:100%;overflow:hidden;aspect-ratio:2400/1490;container-type:inline-size;color:#1f1f21;font-family:var(--aui-font-sans,Inter,ui-sans-serif,system-ui,sans-serif);pointer-events:none;user-select:none}
.aui-integration-flow-stage{position:relative;width:100%;overflow:hidden;background:#ffffff}
.aui-integration-flow-world{position:absolute;left:0;top:0;width:2400px;height:1490px;transform-origin:0 0}
.aui-integration-flow-wash{position:absolute;inset:0;width:100%;height:100%;display:block}
.aui-integration-flow-bottom-fade{position:absolute;z-index:2;left:0;right:0;bottom:0;height:520px;background:linear-gradient(180deg,rgba(255,255,255,0) 0%,rgba(255,255,255,.68) 68%,#ffffff 100%);pointer-events:none}
.aui-integration-flow-board{position:absolute;left:0;top:0;width:1800px;height:1200px;transform-origin:0 0;transform:translateY(-55px) scale(1.3333333333)}
.aui-integration-flow-arc{position:absolute;left:1100.25px;top:-178.5px;width:1519.5px;height:1557.75px;overflow:visible;stroke-width:3.75;stroke-dasharray:12 18;stroke-linecap:round}
.aui-integration-flow-beam{position:absolute;left:780px;top:598.5px;width:150px;height:3px;opacity:0;transform:translateX(240px);background:linear-gradient(90deg,rgba(255,255,255,0) 0%,#ffffff 40%,#ffffff 55%,rgba(255,255,255,.08) 100%);pointer-events:none}
.aui-integration-flow-beam::before{content:"";position:absolute;left:12px;right:12px;top:-3px;height:9px;border-radius:999px;background:radial-gradient(ellipse at 48% 50%,rgba(255,255,255,.95),rgba(255,255,255,0) 75%);filter:blur(3px)}
.aui-integration-flow[data-phase="static"] .aui-integration-flow-beam{opacity:1;transform:translateX(45px)}
.aui-integration-flow[data-phase="beam"] .aui-integration-flow-beam{animation:aui-if-beam 1100ms cubic-bezier(.55,.06,.25,1) forwards}
@keyframes aui-if-beam{0%{opacity:0;transform:translateX(241px)}12%{opacity:1}88%{opacity:1}100%{opacity:0;transform:translateX(-155px)}}
.aui-integration-flow-hub,.aui-integration-flow-pill{position:absolute;box-sizing:border-box;display:flex;align-items:center;gap:24px;height:158.64px;border-radius:999px;background:linear-gradient(180deg,rgba(255,255,255,.78),rgba(255,255,255,.42));color:rgba(0,0,0,.8);box-shadow:inset 0 1.5px 0 rgba(255,255,255,.55),0 18px 40px rgba(0,0,0,.08)}
.aui-integration-flow-hub{left:137px;top:520.68px;width:643px;padding:20px 20px 20px 60px;flex-direction:row-reverse;justify-content:space-between;transition:box-shadow 420ms ease,background 420ms ease}
.aui-integration-flow-hub[data-lit="true"]{box-shadow:inset 0 1.5px 0 rgba(255,255,255,.85),0 0 36px rgba(255,255,255,.45),0 18px 40px rgba(0,0,0,.08)}
.aui-integration-flow-pill{left:1020px;top:520.68px;width:493px;padding:20px 36px 20px 20px;transition:transform 900ms cubic-bezier(.65,.02,.25,1),opacity 900ms cubic-bezier(.65,.02,.25,1),background 900ms cubic-bezier(.65,.02,.25,1),color 900ms cubic-bezier(.65,.02,.25,1),box-shadow 500ms ease-in}
.aui-integration-flow-pill[data-jump="true"]{transition:none!important}
.aui-integration-flow-pill[data-charged="true"]{box-shadow:inset 0 1.5px 0 rgba(255,255,255,.85),0 0 32px rgba(255,255,255,.4),0 18px 40px rgba(0,0,0,.08)}
.aui-integration-flow-pill[data-ghost]{background:rgba(255,255,255,.12);color:rgba(255,255,255,.8);box-shadow:inset 0 1px 0 rgba(255,255,255,.18)}
.aui-integration-flow-pill[data-hide]{opacity:0}
.aui-integration-flow-badge{display:grid;place-items:center;flex:none;width:118.64px;height:118.64px;border-radius:50%;background:rgba(255,255,255,.66);color:#000000;transition:background 900ms cubic-bezier(.65,.02,.25,1),color 900ms cubic-bezier(.65,.02,.25,1)}
.aui-integration-flow-badge svg{width:64px;height:64px;display:block;transition:filter 900ms cubic-bezier(.65,.02,.25,1)}
.aui-integration-flow-pill .aui-integration-flow-badge svg{filter:brightness(1) invert(0)}
.aui-integration-flow-pill[data-ghost] .aui-integration-flow-badge{background:rgba(255,255,255,.14);color:#ffffff}
.aui-integration-flow-pill[data-ghost] .aui-integration-flow-badge svg{filter:brightness(0) invert(1)}
.aui-integration-flow-pill[data-mono]:not([data-ghost]) .aui-integration-flow-badge{color:#000000}
.aui-integration-flow-copy{display:flex;flex-direction:column;gap:8px;min-width:0;flex:1}
.aui-integration-flow-hub .aui-integration-flow-copy{align-items:flex-end;text-align:right}
.aui-integration-flow-hub small{max-width:396px}
.aui-integration-flow-hub strong,.aui-integration-flow-pill strong{display:block;min-width:0;overflow:hidden;font-size:37px;line-height:40px;font-weight:400;letter-spacing:-.02em;text-overflow:ellipsis;white-space:nowrap}
.aui-integration-flow-hub small,.aui-integration-flow-pill small{display:block;min-width:0;overflow:hidden;font-size:33.5px;line-height:36px;font-weight:380;letter-spacing:-.02em;text-overflow:ellipsis;white-space:nowrap;color:rgba(0,0,0,.4)}
.aui-integration-flow-pill[data-ghost] small{color:rgba(255,255,255,.4)}
:where(.dark,[data-theme="dark"]) .aui-integration-flow{--arc-mid:#ffd9cc;--arc-stop:#ffd9cc}
:where(.dark,[data-theme="dark"]) .aui-integration-flow-stage{background:#1f1f21}
:where(.dark,[data-theme="dark"]) .aui-integration-flow-bottom-fade{background:linear-gradient(180deg,rgba(31,31,33,0) 0%,rgba(31,31,33,.68) 68%,#1f1f21 100%)}
:where(.dark,[data-theme="dark"]) .aui-integration-flow-hub,:where(.dark,[data-theme="dark"]) .aui-integration-flow-pill:not([data-ghost]){background:linear-gradient(180deg,rgba(34,32,34,.81),rgba(34,32,34,.48));color:rgba(255,255,255,.92);box-shadow:inset 0 1.5px 0 rgba(255,255,255,.14),0 18px 40px rgba(0,0,0,.22)}
:where(.dark,[data-theme="dark"]) .aui-integration-flow-hub small,:where(.dark,[data-theme="dark"]) .aui-integration-flow-pill:not([data-ghost]) small{color:rgba(255,255,255,.5)}
:where(.dark,[data-theme="dark"]) .aui-integration-flow-hub .aui-integration-flow-badge,:where(.dark,[data-theme="dark"]) .aui-integration-flow-pill:not([data-ghost]) .aui-integration-flow-badge{background:rgba(255,255,255,.11);color:rgba(255,255,255,.92)}
:where(.dark,[data-theme="dark"]) .aui-integration-flow-pill[data-ghost]{background:rgba(255,255,255,.07);color:rgba(255,255,255,.8);box-shadow:inset 0 1px 0 rgba(255,255,255,.12)}
:where(.dark,[data-theme="dark"]) .aui-integration-flow-pill[data-ghost] .aui-integration-flow-badge{background:rgba(255,255,255,.13);color:#ffffff}
:where(.dark,[data-theme="dark"]) .aui-integration-flow-pill[data-mono]:not([data-ghost]) .aui-integration-flow-badge{color:#ffffff}
@media (prefers-reduced-motion:reduce){.aui-integration-flow *,.aui-integration-flow *::before,.aui-integration-flow *::after{animation:none!important;transition:none!important}.aui-integration-flow-beam{opacity:1!important;transform:translateX(45px)!important}}
`;

export function IntegrationFlowDemo({ state = "default" }: { state?: "default" | "long" | "empty" | "single" }) {
  if (state === "empty") return <IntegrationFlow items={[]} />;
  if (state === "single") return <IntegrationFlow items={[{ name: "릴레이", title: "도구 연결" }]} />;
  if (state === "long") return <IntegrationFlow items={builtin.map((item) => ({ ...item, name: `${item.name} · AgentDesk 장기 실행 식별자`, title: "아주 긴 연동 제목과 도구 호출 기록" }))} />;
  return <IntegrationFlow />;
}
