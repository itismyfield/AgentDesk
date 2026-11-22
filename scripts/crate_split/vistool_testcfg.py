"""test_cfg list of a vistool run (#7353 VT2): where upper tests use a lower `#[cfg(test)]` item.

A dependency is never built with `cfg(test)`, so an upper test that names a lower test-only helper fails to
resolve (E0425 E0412 E0433 E0432 E0422 E0599) once the crates are cut. vistool classifies those as `test_cfg`
residue; this module writes them as `vistool-<label>-test-cfg.json` for the KRX-T slices and M's
`test-support` feature:

    {"tool": ..., "mode": "shadow"|"real", "crate": ..., "head": ..., "count": N, "resolved": R,
     "by_code": {"E0425": n, ...}, "items": [
       {"use_site": "src/x.rs:12", "use_source": "<line text>", "item": "crate::a::helper", "name": "helper",
        "definition": "src/a.rs:40" | null, "definition_from": "rustc", "definition_candidates": 1, "code": "E0425", "message": "..."}]}

`definition` is the declaration rustc's `found an item that was configured out` note points at, else the one
test-gated declaration of `name` in the lower crate (narrowed by the module the path names when there are
several); null when none or several remain (`definition_from` rustc|scan, `definition_candidates`).
"""

import bisect
from collections import Counter, defaultdict
import json
from pathlib import Path
import re

import vistool as vt

DECL = re.compile(r"^\s*(?:(?:" + vt.VIS + r")\s+)?" + vt.QUALS +
                  r"(?:fn|struct|enum|union|trait|type|const|static|mod|macro_rules!)\s+(?:r#)?(\w+)")
ITEM_START = re.compile(r"^\s*(?:(?:" + vt.VIS + r")\s+)?" + vt.QUALS +
                        r"(?:fn|struct|enum|union|trait|type|const|static|mod|impl|macro_rules!)\b")
CFG_ATTR = re.compile(r"#!?\[\s*cfg\s*\((.*)\)\s*\]")


def gated_attr(lines, index):
    """Whether the attributes directly above lines[index] include a cfg that only holds under `test`."""
    for j in range(vt.attr_start(lines, index), index):
        m = CFG_ATTR.search(lines[j])
        if m and vt.rts._cfg_requires_test_text(m.group(1)):
            return True
    return False


def gated_ranges(lines):
    """[(first, last)] 1-based line ranges of test-gated items with a body (`#[cfg(test)] mod t {..}`,
    `#[cfg(all(test, unix))] impl X {..}`); items ended by `;` before any `{` have no range."""
    toks = vt.rts.rust_tokens("\n".join(lines))
    close, stack = {}, []
    for i, t in enumerate(toks):
        if t.kind == "punct" and t.value == "{":
            stack.append(i)
        elif t.kind == "punct" and t.value == "}" and stack:
            close[stack.pop()] = t.line
    out, starts = [], [t.line for t in toks]
    for index, line in enumerate(lines):
        if not ITEM_START.match(line) or not gated_attr(lines, index):
            continue
        depth = 0
        for i in range(bisect.bisect_left(starts, index + 1), len(toks)):
            t = toks[i]
            if t.kind != "punct":
                continue
            if t.value in "([":
                depth += 1
            elif t.value in ")]":
                depth -= 1
            elif depth == 0 and t.value == ";":
                break
            elif depth == 0 and t.value == "{":
                out.append((index + 1, close.get(i, t.line)))
                break
    return out


def gated_defs(layout, read):
    """name -> [(rel, line, module)] for every test-gated declaration in the lower crate's files."""
    out = defaultdict(list)
    for rel in sorted(layout.x_files):
        lines = read(rel)
        ranges = gated_ranges(lines)
        whole = rel in layout.test_only
        for index, line in enumerate(lines):
            m = DECL.match(line)
            if not m:
                continue
            if whole or gated_attr(lines, index) or any(a <= index + 1 <= b for a, b in ranges):
                out[m.group(1)].append((rel, index + 1, layout.module_at(rel, index + 1)))
    return out


def item_path(diag):
    """(name, path as the use site spells it) from the diagnostic message."""
    code, msg = vt.code_of(diag), diag.get("message", "")
    ticks = vt.NAME_IN_MSG.findall(msg)
    if not ticks:
        return "", ""
    if code == "E0432":  # unresolved import `a::b::c`
        return ticks[0].split("::")[-1], ticks[0]
    for rx in (r"could not find `([^`]+)` in `([^`]+)`", r"`([^`]+)` in module `([^`]+)`",
               r"named `([^`]+)` found for [\w ]+? `([^`<]+)"):
        m = re.search(rx, msg)
        if m:
            return m.group(1), f"{m.group(2)}::{m.group(1)}"
    name = re.sub(r"<.*", "", ticks[0])
    return name.split("::")[-1], name


def module_of_path(path):
    """Module a `crate::a::b::name` path names (None when it is relative or names a type)."""
    segs = path.split("::")[:-1]
    if not segs or segs[0] != "crate":
        return None
    return tuple(segs[1:])


def definition(name, path, defs):
    cands = defs.get(name, [])
    if len(cands) > 1:
        module = module_of_path(path)
        narrowed = [c for c in cands if module is not None and c[2] == module]
        if len(narrowed) == 1:
            return narrowed[0], len(cands)
    return (cands[0] if len(cands) == 1 else None), len(cands)


def rustc_definition(diag, resolver):
    """The lower declaration rustc names in its `found an item that was configured out` note, if any."""
    for child in diag.get("children", []):
        if "configured out" not in child.get("message", ""):
            continue
        spans = child.get("spans") or []
        span = next((s for s in spans if s.get("is_primary")), spans[0] if spans else None)
        if span:
            side, rel = resolver.rel(span["file_name"], Path(diag["_root"]))
            if side == "lower":
                return f"{rel}:{span['line_start']}"
    return None


def entries(diags, resolver, layout, read):
    defs = gated_defs(layout, read)
    out = []
    for d in diags:
        span = vt.primary_span(d) or {}
        _, rel = resolver.rel(span["file_name"], Path(d["_root"])) if span else (None, "?")
        name, path = item_path(d)
        found, n = definition(name, path, defs)
        noted = rustc_definition(d, resolver)
        out.append({"use_site": f"{rel}:{span.get('line_start', 0)}",
                    "use_source": ((span.get("text") or [{}])[0].get("text") or "").strip(),
                    "item": path, "name": name,
                    "definition": noted or (f"{found[0]}:{found[1]}" if found else None),
                    "definition_from": "rustc" if noted else ("scan" if found else None),
                    "definition_candidates": n, "code": vt.code_of(d), "message": d.get("message", "")})
    site = lambda e: (e["use_site"].rsplit(":", 1)[0], int(e["use_site"].rsplit(":", 1)[1] or 0), e["item"])
    return sorted(out, key=site)


def write_list(path, diags, resolver, layout, read, header):
    """Write the JSON list (always, empty when there is no test_cfg residue); returns its summary."""
    items = entries(diags, resolver, layout, read)
    resolved = sum(1 for e in items if e["definition"])
    doc = dict(header, count=len(items), resolved=resolved,
               by_code=dict(sorted(Counter(e["code"] for e in items).items())), items=items)
    Path(path).write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n", "utf-8")
    return {"file": Path(path).name, "count": len(items), "resolved": resolved}
