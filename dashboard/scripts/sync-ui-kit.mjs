#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const DASHBOARD_DIR = resolve(SCRIPT_DIR, "..");
const DEST_KIT_DIR = resolve(DASHBOARD_DIR, "src/kit");

const DEFAULT_TARGETS = [
  "pro/agent-monitor.tsx",
  "pro/command-menu.tsx",
  "pro/compact-breakdown.tsx",
  "pro/kpi-chart.tsx",
  "pro/usage-meter.tsx",
  "pro/running-now.tsx",
  "free/card-clean.tsx",
  "free/select-clean.tsx",
  "free/button-clean.tsx",
];

function parseArgs(argv) {
  let kitRepo = process.env.AGENTDESK_UI_KIT_DIR ?? "/Users/itismyfield/agentdesk-ui-kit";
  let ref = "origin/main";
  const files = [];
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--kit-repo" && argv[i + 1]) {
      kitRepo = argv[++i];
    } else if (arg === "--ref" && argv[i + 1]) {
      ref = argv[++i];
    } else if (!arg.startsWith("-")) {
      files.push(arg);
    }
  }
  return { kitRepo, ref, files: files.length > 0 ? files : DEFAULT_TARGETS };
}

function gitShow(kitRepo, args) {
  return execFileSync("git", ["-C", kitRepo, ...args], { encoding: "utf8" });
}

function stripDemoSection(source) {
  const demoStart = /(?:\r?\n\/\*\s*-+\s*demos\s*\*\/\s*)?\r?\nexport (?:type \w+DemoState|function \w+Demo)\b[\s\S]*$/;
  return source.replace(demoStart, "\n");
}

function pruneUnusedNamedImports(source) {
  return source.replace(
    /^import\s*\{([^}]+)\}\s*from\s*(["'](?:lucide-react|react)["']);?[ \t]*\r?\n/gm,
    (full, specifiersRaw, moduleLiteral) => {
      const restOfFile = source.slice(source.indexOf(full) + full.length);
      const kept = specifiersRaw
        .split(",")
        .map((part) => part.trim())
        .filter(Boolean)
        .filter((spec) => {
          const cleaned = spec.replace(/^type\s+/, "").trim();
          const localName = cleaned.includes(" as ")
            ? cleaned.split(/\s+as\s+/)[1].trim()
            : cleaned;
          return new RegExp(`\\b${localName.replace(/\$/g, "\\$")}\\b`).test(restOfFile);
        });
      if (kept.length === 0) return "";
      return `import { ${kept.join(", ")} } from ${moduleLiteral};\n`;
    },
  );
}

function stripDemoCssLines(cssBody) {
  return cssBody
    .split("\n")
    .filter((line) => {
      const trimmed = line.trim();
      return !/^(?::where\([^)]+\)\s*)?\.aui-[\w-]+-demo(?:\b|[-_{.: ])/.test(trimmed) &&
        !/^@media[^{]*\{\s*\.aui-[\w-]+-demo[\w-]*\s*\{.*\}\s*\}$/.test(trimmed);
    })
    .join("\n");
}

function wrapStyleVariablesInKitLayer(source) {
  const styleVars = new Set();
  for (const match of source.matchAll(/<style\b[^>]*>\s*\{\s*([A-Za-z_$][\w$]*)\s*\}\s*<\/style>/g)) {
    styleVars.add(match[1]);
  }
  let next = source;
  for (const varName of styleVars) {
    const declPattern = new RegExp(
      `(const\\s+${varName}\\s*=\\s*(?:\\/\\*[^*]*\\*\\/\\s*)?)\`([\\s\\S]*?)\`;`,
    );
    next = next.replace(declPattern, (_full, prefix, cssBody) => {
      const cleaned = stripDemoCssLines(cssBody).trim();
      if (cleaned.startsWith("@layer kit")) {
        return `${prefix}\`\n${cleaned}\n\`;`;
      }
      return `${prefix}\`\n@layer kit {\n${cleaned}\n}\n\`;`;
    });
  }
  return next;
}

function applyKitFixes(relPath, source) {
  let next = source;
  if (relPath === "pro/command-menu.tsx") {
    // #8369 P2-6: .aui-command-menu-search:focus-within already draws the search box ring.
    next = next.replace(
      ".aui-command-menu button:focus-visible,.aui-command-menu input:focus-visible{",
      ".aui-command-menu button:focus-visible{",
    );
    next = next.replace(
      ".aui-command-menu-search input{",
      ".aui-command-menu-search input:focus,.aui-command-menu-search input:focus-visible{outline:none}.aui-command-menu-search input{",
    );
  }
  if (relPath === "pro/compact-breakdown.tsx") {
    // #8369 P2-5 & UX P1-6: exact 0%/<1%/>99%/100% boundaries and integer count-up rounding.
    next = next.replace(
      "const shareOf = (value: number, total: number) => value / Math.max(total, .0001) * 100;",
      "const shareOf = (value: number, total: number) => total <= 0 || value <= 0 ? 0 : value >= total ? 100 : value / total * 100;",
    );
    next = next.replace(
      "const shareText = (share: number) => share > 0 && share < 1 ? '<1%' : share > 99 && share < 100 ? '>99%' : `${Math.round(share)}%`;",
      "const shareText = (share: number) => share <= 0 ? '0%' : share < 1 ? '<1%' : share >= 100 ? '100%' : share > 99 ? '>99%' : `${Math.round(share)}%`;",
    );
    next = next.replace(
      "const interpolate = (a: number, b: number) => a + (b - a) * progress;",
      "const integerMode = Object.values(to.values).every((v) => Number.isInteger(v));\n      const interpolate = (a: number, b: number) => integerMode ? Math.round(a + (b - a) * progress) : a + (b - a) * progress;",
    );
    // #8369 P2-4: surface full readout for selected long-name rows in onSelect mode too.
    next = next.replace(
      "{active && <p className=\"aui-compact-breakdown-full-readout\" data-long={active.label.length > 24} aria-hidden=\"true\">{active.label} · {format(active.value)} · {shareText(shareOf(active.value, total))}</p>}",
      "{(() => { const readout = active ?? (selectedId != null ? remembered.find((item) => item.present && item.source === selectedId) : undefined); return readout ? <p className=\"aui-compact-breakdown-full-readout\" data-long={readout.label.length > 16} aria-hidden=\"true\">{readout.label} · {format(readout.value)} · {shareText(shareOf(readout.value, total))}</p> : null; })()}",
    );
  }
  if (relPath === "pro/usage-meter.tsx") {
    // Avoid initial mount 0% -> 60% -> 100% interpolation illusion; render true snapshot at mount and animate updates only.
    next = next.replace(
      "const [entered, setEntered] = useState(false);\n  const [numbers, setNumbers] = useState<NumericSnapshot>(() => Object.fromEntries(Object.keys(target).map((name) => [name, 0])));",
      "const [entered] = useState(true);\n  const [numbers, setNumbers] = useState<NumericSnapshot>(() => ({ ...target }));",
    );
    next = next.replace(
      "  useEffect(() => {\n    const element = root.current;\n    if (!element || typeof IntersectionObserver === 'undefined') { setEntered(true); return; }\n    const observer = new IntersectionObserver((entries) => {\n      if (entries.some((entry) => entry.isIntersecting && entry.intersectionRatio >= 0.2)) {\n        setEntered(true);\n        observer.disconnect();\n      }\n    }, { threshold: 0.2 });\n    observer.observe(element);\n    return () => observer.disconnect();\n  }, [root]);\n",
      "",
    );
    next = next.replace(
      "    const origin = displayed.current;\n    const duration = hasStarted.current ? 650 : 1200;\n    hasStarted.current = true;\n    let frame = 0;\n    const publish = (next: NumericSnapshot) => { displayed.current = next; setNumbers(next); };\n    if (reduced) { publish(destination); return; }",
      "    const publish = (next: NumericSnapshot) => { displayed.current = next; setNumbers(next); };\n    if (!hasStarted.current || reduced) { hasStarted.current = true; publish(destination); return; }\n    const origin = displayed.current;\n    const duration = 650;\n    let frame = 0;",
    );
  }
  return next;
}

export function transformKitSource(rawSource, relPath, shortSha) {
  const withoutOldHeader = rawSource.replace(/^\/\/ Kit provenance: [^\r\n]*\r?\n/, "");
  let transformed = stripDemoSection(withoutOldHeader);
  transformed = wrapStyleVariablesInKitLayer(transformed);
  transformed = applyKitFixes(relPath, transformed);
  transformed = pruneUnusedNamedImports(transformed);
  return `// Kit provenance: itismyfield/agentdesk-ui-kit@${shortSha}\n${transformed.trimEnd()}\n`;
}

function main() {
  const { kitRepo, ref, files } = parseArgs(process.argv.slice(2));
  const shortSha = gitShow(kitRepo, ["rev-parse", "--short=7", ref]).trim();
  for (const relPath of files) {
    const raw = gitShow(kitRepo, ["show", `${ref}:src/kit/${relPath}`]);
    const output = transformKitSource(raw, relPath, shortSha);
    const outPath = resolve(DEST_KIT_DIR, relPath);
    mkdirSync(dirname(outPath), { recursive: true });
    writeFileSync(outPath, output, "utf8");
    console.log(`synced src/kit/${relPath} (@${shortSha})`);
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main();
}
