#!/usr/bin/env python3
"""KRX test-only gates that upper tests use -> `test-support` (#7353 KRX-TS). Static, standard library only.

    python3 scripts/crate_split/test_support_codemod.py [--tree DIR] [--count | --check] [--plan OUT.json]
        [--exclude FILE] [--list-test-modules]

Once KRX is its own crate it is never built with `cfg(test)`, so a KE, DL or ROOT test that names a KRX item
behind a test gate stops compiling. The KRX crate will export those items under its `test-support` feature,
which the upper crates enable only as a dev-dependency. This tool rewrites each such gate in place:

  `#[cfg(test)]`          -> `#[cfg(any(test, feature = "test-support"))]`
  `#[cfg(all(test, X))]`  -> `#[cfg(all(any(test, feature = "test-support"), X))]`
  `#[cfg(test)]` over `#[cfg(unix)]`: the line that requires `test` (the first) changes, the other stays

Nothing else on a line changes (indentation and a trailing comment are kept). While the root package does not
declare `test-support` (Cargo.toml only registers the value for check-cfg) both spellings are the same cfg.

Which gates: an upper test region (a test-only file, or a test-gated block or line of an upper file) names a
KRX item that is test-gated (vistool_testcfg.gated_attr / gated_ranges, strict: `test-support` does not
count, and Layout.test_only for whole files). Names resolve through `use` trees and aliases, `crate::` /
`super::` / `self::` paths, `Type::assoc` on a KRX type, `.method` / `.field` on a gated impl member that is
not private (a name also declared ungated counts only where the file names the impl's type), and bare names
after a glob import. Opening the item needs every test gate above it: its own, every gated block around it,
and the gates on the `mod` declarations that reach its file. A gated `impl Drop for T` goes with a target
`struct T` / `enum T` (a guard whose restore stayed test-only would vanish silently).

test_support_exclude.json lists gates this slice leaves alone, keyed by file and item (`impl Runner > fn
probes_mut`, never a line number). An item whose gate chain holds an excluded gate is deferred whole. The
tool fails closed (exit 3) when a gate it would change opens a module holding tests, or has a `cfg(not(test))`
twin of the same name, and is not listed; and when a listed entry matches nothing (exit 2).

Rerunning on its own output changes nothing: a converted gate no longer requires `test`.
"""

from __future__ import annotations

import argparse
import collections
import functools
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import vistool as vt  # noqa: E402
import vistool_testcfg as tc  # noqa: E402

rts = vt.rts
TS = 'any(test, feature = "test-support")'
UPPER = ("KE", "DL", "ROOT")
EXCLUDE = HERE / "test_support_exclude.json"
FIELD = re.compile(r"^\s*(?:(?:" + vt.VIS + r")\s+)?(\w+)\s*:\s*[^:]")
IMPL = re.compile(r"^\s*(?:unsafe\s+)?impl\b(.*)$")
MACRO = re.compile(r"^\s*(?:(?:" + vt.VIS + r")\s+)?(\w+)!\s*[{(\[]")
CONTAINER = re.compile(tc.ITEM_START.pattern + r"|^\s*(?:(?:" + vt.VIS + r")\s+)?\w+!\s*\{")
CFG_LINE = re.compile(r"^(\s*)#\[cfg\(")
NOT_TEST = re.compile(r"\bnot\s*\(\s*(?:test\b|any\s*\(\s*test\b)")
TEST_ATTR = re.compile(r"^\s*#\[\s*(?:(?:tokio|async_std|actix_rt)\s*::\s*)?test\b")
KW = re.compile(r"\b(fn|struct|enum|union|trait|type|const|static|mod|macro_rules!)\s")


class CodemodError(Exception):
    def __init__(self, code, lines):
        super().__init__("\n".join(lines))
        self.code, self.lines = code, lines


# ---------------------------------------------------------------------------------------------- one line

def cfg_predicate(line):
    """(start, end) of the predicate inside a `#[cfg(...)]` that opens the line, or None."""
    m = CFG_LINE.match(line)
    if not m:
        return None
    depth, i, quoted = 1, m.end(), False
    while i < len(line):
        ch = line[i]
        if quoted:
            if ch == "\\":
                i += 1
            elif ch == '"':
                quoted = False
        elif ch == '"':
            quoted = True
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return (m.end(), i) if line[i + 1:i + 2] == "]" else None
        i += 1
    return None


def top_args(text):
    """[(start, end)] of the comma-separated arguments of one paren group's inside text."""
    out, depth, start, quoted = [], 0, 0, False
    for i, ch in enumerate(text):
        if quoted:
            quoted = ch != '"'
        elif ch == '"':
            quoted = True
        elif ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == "," and depth == 0:
            out.append((start, i))
            start = i + 1
    out.append((start, len(text)))
    return out


def rewrite(line):
    """The line with its gate's `test` atom spelled `any(test, feature = "test-support")`; None if no rule."""
    span = cfg_predicate(line)
    if span is None:
        return None
    a, b = span
    pred = line[a:b]
    if pred.strip() == "test":
        lead = len(pred) - len(pred.lstrip())
        return line[:a + lead] + TS + line[a + lead + 4:]
    m = re.match(r"^(\s*all\s*\()(.*)\)\s*$", pred)
    if not m:
        return None
    inner, off = m.group(2), a + m.end(1)
    for s, e in top_args(inner):
        arg = inner[s:e]
        if arg.strip() == "test":
            at = off + s + len(arg) - len(arg.lstrip())
            return line[:at] + TS + line[at + 4:]
    return None


def real_gate(line):
    """A `#[cfg(..)]` opening the line whose predicate only holds under `test` (strict)."""
    span = cfg_predicate(line)
    return span is not None and rts._cfg_requires_test_text(line[span[0]:span[1]], strict=True)


# ---------------------------------------------------------------------------------------------- tree

def speedups():
    """Memoize the pure tokenizers: one tree tokenizes each file for several passes."""
    if not hasattr(rts.rust_tokens, "cache_info"):
        rts.rust_tokens = functools.lru_cache(maxsize=256)(rts.rust_tokens)
        vt.inline_blocks = functools.lru_cache(maxsize=4096)(vt.inline_blocks)


class Tree:
    def __init__(self, root):
        speedups()
        self.root = Path(root).resolve()
        self._layout = None
        self._lines, self._ranges, self._blocks, self._module_at, self.unowned = {}, {}, {}, {}, {}

    @property
    def layout(self):
        """vistool's partition layout of the tree (built on first use: labels need none)."""
        if self._layout is None:
            partition = vt.load_partition(self.root / "scripts/crate_split/partition.json")
            self._layout = vt.Layout(self.root, partition, "KRX")
        return self._layout

    def lines(self, rel):
        if rel not in self._lines:
            self._lines[rel] = (self.root / rel).read_text("utf-8").split("\n")
        return self._lines[rel]

    def ranges(self, rel):
        if rel not in self._ranges:
            lines = self.lines(rel)
            # gated_ranges needs a `cfg(` line; most files have none and need no tokens.
            self._ranges[rel] = tc.gated_ranges(lines) if any("cfg" in x for x in lines) else []
        return self._ranges[rel]

    def module_at(self, rel, line):
        key = (rel, line)
        if key not in self._module_at:
            self._module_at[key] = tuple(self.layout.module_at(rel, line))
        return self._module_at[key]

    def crate_of(self, rel):
        """Partition crate of a file; a module with no partition key takes its nearest ancestor key's crate
        (`discord::krx_tui_o_test_host` -> `discord::(mod.rs)`)."""
        lay = self.layout
        c = lay.crate_of_file.get(rel)
        if c is not None:
            return c
        m = lay.module_of.get(rel, ())
        for n in range(len(m) - 1, -1, -1):
            key = vt.group_key(m[:n]) if m[:n] != ("services",) else "services"
            if key in lay.owner:
                self.unowned[rel] = key
                return lay.owner[key]
        return None

    def blocks(self, rel):
        """[(first, last, index)] 1-based line ranges of every item or macro block with a `{` body."""
        if rel in self._blocks:
            return self._blocks[rel]
        lines = self.lines(rel)
        toks = rts.rust_tokens("\n".join(lines))
        close, stack = {}, []
        for i, t in enumerate(toks):
            if t.kind == "punct" and t.value == "{":
                stack.append(i)
            elif t.kind == "punct" and t.value == "}" and stack:
                close[stack.pop()] = t.line
        out, starts, k = [], [t.line for t in toks], 0
        for index, line in enumerate(lines):
            if not CONTAINER.match(line):
                continue
            depth = 0
            while k < len(starts) and starts[k] < index + 1:
                k += 1
            for i in range(k, len(toks)):
                t = toks[i]
                if t.kind != "punct":
                    continue
                if t.value in "([":
                    depth += 1
                elif t.value in ")]":
                    depth -= 1
                elif depth <= 0 and t.value == ";":
                    break
                elif depth <= 0 and t.value == "{":
                    out.append((index + 1, close.get(i, t.line), index))
                    break
        self._blocks[rel] = out
        return out

    def label(self, rel, index):
        """`impl Runner > fn probes_mut`: the item at lines[index] inside the items around it."""
        outer = [own_label(self.lines(rel), j) for a, b, j in self.blocks(rel) if a < index + 1 <= b]
        return " > ".join(outer + [own_label(self.lines(rel), index)])

    def attr_gates(self, rel, index):
        """Indexes of the real test gates among the attributes directly above lines[index]."""
        lines = self.lines(rel)
        return [j for j in range(vt.attr_start(lines, index), index) if real_gate(lines[j])]

    def chain(self, rel, line, seen=None):
        """Every real test gate (rel, attr index) that must open for the item at `line` (1-based) to build."""
        out = [(rel, j) for j in self.attr_gates(rel, line - 1)]
        for a, b in self.ranges(rel):
            if a <= line <= b and a != line:
                out += [(rel, j) for j in self.attr_gates(rel, a - 1)]
        return out + self.file_chain(rel, seen)

    def file_chain(self, rel, seen=None):
        """Gates on the `mod` declarations that reach a file, up to the crate root."""
        seen = set() if seen is None else seen
        lay = self.layout
        m = lay.module_of.get(rel, ())
        if not m or rel in seen:
            return []
        seen.add(rel)
        parent, name = m[:-1], m[-1]
        loc = lay.locate(parent)
        if not loc:
            return []
        prel = loc[0]
        try:
            lines = self.lines(prel)
        except OSError:
            return []
        out = []
        for i in vt.decl_hits(lines, name, parent, lay.module_of.get(prel, ())):
            out += [g for g in self.chain(prel, i + 1, seen) if g not in out]
        return out


def own_label(lines, index):
    line = lines[index]
    m = tc.DECL.match(line)
    if m:
        kw = KW.search(line)
        return f"{kw.group(1) if kw else 'item'} {m.group(1)}"
    if IMPL.match(line):
        head = " ".join(x.strip() for x in lines[index:index + 6]).split("{")[0]
        head = re.sub(r"^(?:unsafe\s+)?impl\s*(<[^{]*?>)?\s*", "", head.strip(), count=1)
        head = re.sub(r"\bwhere\b.*", "", head)
        head = re.sub(r"<.*?>", "", head)
        return "impl " + re.sub(r"\s+", " ", head).strip()
    if re.match(r"^\s*(?:(?:" + vt.VIS + r")\s+)?use\s", line):
        text = " ".join(x.strip() for x in lines[index:index + 8]).split(";")[0]
        return re.sub(r"\s+", " ", re.sub(r"^(?:(?:" + vt.VIS + r")\s+)?", "", text.strip()))
    mm = MACRO.match(line)
    if mm:
        body = " ".join(lines[index:index + 6])
        inner = re.search(r"\b(?:static|const|fn|struct)\s+(\w+)", body[body.find("!") + 1:])
        return f"{mm.group(1)}!" + (f"{{{inner.group(1)}}}" if inner else "")
    f = FIELD.match(line)
    if f and not line.strip().startswith(("let ", "fn ")):
        return f"field {f.group(1)}"
    return "stmt " + re.sub(r"\s+", " ", line.strip())


def impl_type(lines, index):
    """Self type name of an `impl ... {` header starting at lines[index]."""
    head = " ".join(lines[index:index + 6]).split("{")[0]
    head = re.sub(r"^\s*(?:unsafe\s+)?impl\s*(<[^{]*?>)?\s*", "", head, count=1)
    head = re.sub(r"\bwhere\b.*", "", head)
    if re.search(r"\bfor\b", head):
        head = re.split(r"\bfor\b", head)[-1]
    head = re.sub(r"<.*", "", head).strip()
    return head.split("::")[-1].strip("&* ") or "?"


# ---------------------------------------------------------------------------------------------- scan

def krx_gated(tree):
    """KRX test-gated declarations: (items by name, gated modules {module: (rel, line, is_file)})."""
    lay = tree.layout
    by_name, mods = collections.defaultdict(list), {}
    for rel in sorted(r for r in lay.module_of if r.endswith(".rs") and tree.crate_of(r) == "KRX"):
        lines = tree.lines(rel)
        whole = rel in lay.test_only
        if whole:
            mods.setdefault(lay.module_of.get(rel, ()), (rel, 1, True))
        ranges = tree.ranges(rel)
        for i, line in enumerate(lines):
            n = i + 1
            own = tc.gated_attr(lines, i)
            inside = [r for r in ranges if r[0] <= n <= r[1]]
            if not (own or inside or whole):
                continue
            outer = min(inside, key=lambda r: r[0]) if inside else None
            # An outside module can name a member gated on its own line or by a gated `impl` block.
            member_ok = not whole and (outer is None or outer[0] == n or IMPL.match(lines[outer[0] - 1]))
            typ = None
            for a, b in ranges:
                if a <= n <= b and IMPL.match(lines[a - 1]):
                    typ = impl_type(lines, a - 1)
            if typ is None:
                ind = len(line) - len(line.lstrip())
                for j in range(i - 1, -1, -1):
                    lj = lines[j]
                    if IMPL.match(lj) and len(lj) - len(lj.lstrip()) < ind:
                        typ = impl_type(lines, j)
                        break
                    if lj.strip() and not lj[:1].isspace() and not IMPL.match(lj) and \
                            not lj.startswith(("#", "//", "}")):
                        break
            m = tc.DECL.match(line)
            module = tree.module_at(rel, n)
            vis = re.match(r"^\s*(" + vt.VIS + r")\s", line)
            if m:
                kw = KW.search(line).group(1)
                kind = "method" if typ and kw in ("fn", "const", "type") and line[:1].isspace() else kw
                by_name[m.group(1)].append({"name": m.group(1), "kind": kind, "rel": rel, "line": n,
                                            "module": tuple(module), "type": typ if kind == "method" else None,
                                            "vis": vis.group(1) if vis else "", "member_ok": member_ok})
                if kw == "mod":
                    key = tuple(module) if line.rstrip().endswith("{") else tuple(module) + (m.group(1),)
                    mods.setdefault(key, (rel, n, False))
                continue
            if own and re.match(r"^\s*(?:(?:" + vt.VIS + r")\s+)?use\s", line):
                text = " ".join(lines[i:i + 8]).split(";")[0] + ";"
                for path, orig, bound in vt.use_entries(text):
                    if orig == "*":
                        continue
                    # What the `use` re-exports: a path relative to its module unless it starts at the crate.
                    segs = list(path) + [orig]
                    target = resolve(segs, module, {}) if segs[0] in ("crate", "self", "super") else \
                        list(module) + segs
                    by_name[bound].append({"name": bound, "kind": "use", "rel": rel, "line": n, "module": tuple(module),
                                           "type": None, "vis": "", "member_ok": False, "target": target})
                continue
            if own and not IMPL.match(line):
                f = FIELD.match(line)
                if f and not line.strip().startswith(("let ", "fn ")):
                    by_name[f.group(1)].append({"name": f.group(1), "kind": "field", "rel": rel, "line": n,
                                                "module": tuple(module), "type": None, "vis": "",
                                                "member_ok": member_ok})
    return by_name, mods


def ungated_names(tree):
    """Declaration names outside every test gate anywhere in the lib (`.name` ambiguity)."""
    out = collections.Counter()
    lay = tree.layout
    for rel in lay.module_of:
        if rel in lay.test_only or not rel.endswith(".rs"):
            continue
        lines = tree.lines(rel)
        ranges = None
        for i, line in enumerate(lines):
            m = tc.DECL.match(line) or FIELD.match(line)
            if not m:
                continue
            if ranges is None:
                ranges = tree.ranges(rel)
            if tc.gated_attr(lines, i) or any(a <= i + 1 <= b for a, b in ranges):
                continue
            out[m.group(1)] += 1
    return out


def region(tree, rel):
    lines = tree.lines(rel)
    if rel in tree.layout.test_only:
        return set(range(1, len(lines) + 1))
    out = set()
    for a, b in tree.ranges(rel):
        out.update(range(a, b + 1))
    for i in range(len(lines)):
        if tc.gated_attr(lines, i):
            # A gated one-liner (`#[cfg(test)] use ...;`): up to its `;`.
            j = i
            while j < len(lines) and ";" not in lines[j] and "{" not in lines[j]:
                j += 1
            out.update(range(i + 1, j + 2))
    return out


def resolve(segs, base, aliases):
    """Absolute module path for path segments used in module `base`; None for extern / unknown."""
    segs = list(segs)
    if not segs:
        return None
    if segs[0] == "crate":
        return segs[1:]
    if segs[0] in ("super", "self"):
        cur, k = list(base), 0
        while k < len(segs) and segs[k] in ("super", "self"):
            if segs[k] == "super":
                cur = cur[:-1]
            k += 1
        return cur + segs[k:]
    if segs[0] in aliases:
        return list(aliases[segs[0]]) + segs[1:]
    return None


def same_tail(module, path):
    """`herdr::autostart::real_tests` reached as `session_host::autostart::real_tests`: a re-exported module."""
    return module == path or len(module) >= 2 and module[-2:] == path[-2:]


def hits(tree, by_name, mods, ungated):
    """[(upper use site, target)] where target is ('mod', module) or ('item', rel, line)."""
    lay = tree.layout
    out = []

    def gated_mod(path):
        for n in range(len(path), 0, -1):
            if tuple(path[:n]) in mods:
                return tuple(path[:n])
        return None

    def free(module, name):
        return [g for g in by_name.get(name, []) if g["kind"] not in ("method", "field")
                and g["module"] == tuple(module)]

    for rel in sorted(lay.module_of):
        if not rel.endswith(".rs") or tree.crate_of(rel) not in UPPER:
            continue
        reg = region(tree, rel)
        if not reg:
            continue
        text = "\n".join(tree.lines(rel))
        toks = rts.rust_tokens(text, extended=True)
        aliases, globs, i = {}, [], 0

        def hit(line, target):
            out.append((f"{rel}:{line}", target))

        while i < len(toks):
            t = toks[i]
            if t.line not in reg:
                i += 1
                continue
            base = tree.module_at(rel, t.line)
            if t.kind == "ident" and t.value == "use" and (i == 0 or toks[i - 1].value in (
                    ";", "}", "{", "]", ")", "pub", "crate", "super")):
                j = i
                while j < len(toks) and toks[j].value != ";":
                    j += 1
                seg = text[t.start:toks[j].start + 1] if j < len(toks) else text[t.start:]
                for path, orig, bound in vt.use_entries(seg):
                    ab = resolve(list(path) + ([] if orig == "*" else [orig]), base, aliases)
                    if ab is None:
                        continue
                    if orig == "*":
                        gm = gated_mod(ab)
                        if gm:
                            hit(t.line, ("mod", gm))
                        globs.append(tuple(ab))
                        continue
                    aliases[bound] = ab
                    gm = gated_mod(ab)
                    if gm:
                        hit(t.line, ("mod", gm))
                    # An item inside a gated mod may carry its own gate too: both must open.
                    for g in free(ab[:-1], ab[-1]):
                        hit(t.line, ("item", g["rel"], g["line"]))
                i = j + 1
                continue
            if t.kind == "ident" and i + 2 < len(toks) and toks[i + 1].value == ":" and toks[i + 2].value == ":" \
                    and not (i >= 2 and toks[i - 1].value == ":" and toks[i - 2].value == ":"):
                segs, j = [t.value], i
                while j + 3 < len(toks) and toks[j + 1].value == ":" and toks[j + 2].value == ":" \
                        and toks[j + 3].kind == "ident":
                    segs.append(toks[j + 3].value)
                    j += 3
                ab = resolve(segs, base, aliases)
                if ab is not None:
                    gm = gated_mod(ab)
                    if gm:
                        hit(t.line, ("mod", gm))
                    for k in range(len(ab), 0, -1):
                        found = free(ab[:k - 1], ab[k - 1])
                        for g in found:
                            hit(t.line, ("item", g["rel"], g["line"]))
                        if found:
                            break
                # Type::assoc on a type that resolves to a KRX module, whether or not the type itself or a
                # gated mod around it is a hit too: opening those must reveal nothing new. A method declared
                # in another module counts when the path re-exports it (the last two module names agree) or an
                # outside module can name the method, never a private helper of an unrelated `mod tests`.
                for k in range(len(segs) - 1):
                    a, b = segs[k], segs[k + 1]
                    if not a[:1].isupper():
                        continue
                    tab = resolve(segs[:k + 1], base, aliases)
                    if tab is None or len(tab) < 1:
                        break
                    if lay.owner.get(vt.group_of(tuple(tab[:-1]), lay.owner)) != "KRX":
                        break
                    for g in by_name.get(b, []):
                        if g["kind"] == "method" and g["type"] == tab[-1] and (
                                same_tail(g["module"], tuple(tab[:-1])) or g["vis"] and g["member_ok"]):
                            hit(t.line, ("item", g["rel"], g["line"]))
                    break
                i = j + 1
                continue
            if t.kind == "ident" and i > 0 and toks[i - 1].value == "." and t.value in by_name:
                for g in by_name[t.value]:
                    if g["kind"] not in ("method", "field") or not (g["vis"] and g["member_ok"]):
                        continue
                    called = i + 1 < len(toks) and toks[i + 1].value in ("(", ":")
                    if (g["kind"] == "method") != called and g["kind"] != "field":
                        continue
                    owner = g["type"] or ""
                    # A name also declared ungated counts only where the file names the gated impl's type.
                    if ungated[t.value] and not (owner and re.search(r"\b" + re.escape(owner) + r"\b", text)):
                        continue
                    hit(t.line, ("item", g["rel"], g["line"]))
                i += 1
                continue
            if t.kind == "ident" and t.value in by_name and not (i > 0 and toks[i - 1].value in (".", ":")):
                for gm in globs:
                    for g in free(gm, t.value):
                        hit(t.line, ("item", g["rel"], g["line"]))
            i += 1
    return out


# ---------------------------------------------------------------------------------------------- plan

def load_exclude(path):
    doc = json.loads(Path(path).read_text("utf-8"))
    keys = [(e["file"], e["item"]) for e in doc["entries"]]
    dup = [k for k, n in collections.Counter(keys).items() if n > 1]
    if dup:
        raise CodemodError(2, [f"duplicate exclusion {f} :: {i}" for f, i in dup])
    return doc["entries"]


def cfg_items(tree, rel):
    """label -> [item index] of every item in the file carrying a `#[cfg(..)]` attribute line."""
    out = collections.defaultdict(list)
    lines = tree.lines(rel)
    for i, line in enumerate(lines):
        if CFG_LINE.match(line):
            k = item_index(lines, i)
            if k < len(lines) and k not in out[tree.label(rel, k)]:
                out[tree.label(rel, k)].append(k)
    return out


def item_index(lines, attr):
    """Index of the item an attribute line sits above."""
    i = attr
    while i < len(lines) and re.match(r"^\s*(#\[|///|//!)", lines[i]):
        i += 1
    return i


def holds_tests(tree, rel, index):
    """Whether the gated `mod` at lines[index] holds test functions (its block, or its files)."""
    lines = tree.lines(rel)
    m = tc.DECL.match(lines[index])
    if not m or KW.search(lines[index]).group(1) != "mod":
        return False
    lay = tree.layout
    module = tree.module_at(rel, index + 1)
    if lines[index].rstrip().endswith(";"):
        module = tuple(module) + (m.group(1),)
    for a, b in tree.ranges(rel):
        if a == index + 1 and any(TEST_ATTR.match(x) for x in lines[a - 1:b]):
            return True
    for other, mod in lay.module_of.items():
        if tuple(mod[:len(module)]) == tuple(module) and other != rel and other.endswith(".rs"):
            if any(TEST_ATTR.match(x) for x in tree.lines(other)):
                return True
    return False


def plan(tree, exclude_entries):
    """The edits and their counts; raises CodemodError when a rule or the exclusion list does not fit."""
    lay = tree.layout
    by_name, mods = krx_gated(tree)
    found = hits(tree, by_name, mods, ungated_names(tree))
    uses = {(g["rel"], g["line"]): g["target"] for v in by_name.values() for g in v if g["kind"] == "use"}

    def chain_of(target, depth=0):
        """Gates to open for a hit; a gated re-export also needs what it re-exports (`pub use spy::Guard`)."""
        if target[0] == "mod":
            rel, line, is_file = mods[target[1]]
            return tree.file_chain(rel) if is_file else tree.chain(rel, line)
        out = list(tree.chain(target[1], target[2]))
        path = uses.get(target[1:])
        if path and depth < 8:
            inner = [("mod", tuple(path[:n])) for n in range(len(path), 0, -1) if tuple(path[:n]) in mods][:1]
            inner += [("item", g["rel"], g["line"]) for g in by_name.get(path[-1], [])
                      if g["kind"] not in ("method", "field") and g["module"] == tuple(path[:-1])]
            for t in inner:
                out += [g for g in chain_of(t, depth + 1) if g not in out]
        return out

    chains = {}
    for _, target in found:
        if target not in chains:
            chains[target] = chain_of(target)
    gates = {g for chain in chains.values() for g in chain}
    # A gated `impl Drop for T` goes with its gated `struct T` / `enum T`.
    companions = set()
    for rel, j in sorted(gates):
        lines = tree.lines(rel)
        m = re.match(r"^\s*(?:(?:" + vt.VIS + r")\s+)?(?:struct|enum)\s+(\w+)", lines[item_index(lines, j)])
        if not m:
            continue
        for i, line in enumerate(lines):
            if re.match(r"^\s*impl\s*(<[^>]*>)?\s*Drop\s+for\s+" + m.group(1) + r"\b", line):
                companions.update((rel, k) for k in tree.attr_gates(rel, i) if (rel, k) not in gates)
    gates |= companions
    label = {g: tree.label(g[0], item_index(tree.lines(g[0]), g[1])) for g in gates}
    # Exclusions: each entry names items carrying a cfg attribute in its file.
    excluded, pinned, stale = set(), [], []
    for e in exclude_entries:
        rel = e["file"]
        try:
            matched = cfg_items(tree, rel).get(e["item"], [])
        except OSError:
            matched = []
        if not matched:
            stale.append(f"exclusion matches nothing: {rel} :: {e['item']}")
            continue
        hit_gates = {g for g in gates if g[0] == rel and label[g] == e["item"]}
        excluded |= hit_gates
        if not hit_gates:
            pinned.append((rel, e["item"]))
    if stale:
        raise CodemodError(2, stale)
    deferred_items = {t for t, chain in chains.items() if any(g in excluded for g in chain)}
    wanted = {g for t, chain in chains.items() if t not in deferred_items for g in chain}
    wanted |= {g for g in companions if g not in excluded}
    deferred = gates - excluded - wanted
    problems = []
    for g in sorted(wanted):
        rel, j = g
        lines = tree.lines(rel)
        if rewrite(lines[j]) is None:
            problems.append(f"no rewrite rule: {rel}:{j + 1} {lines[j].strip()}")
            continue
        index = item_index(lines, j)
        if holds_tests(tree, rel, index):
            problems.append(f"opens a test module, not excluded: {rel} :: {label[g]}")
        name = tc.DECL.match(lines[index])
        if name:
            for i, line in enumerate(lines):
                if i != j and CFG_LINE.match(line) and NOT_TEST.search(line):
                    k = item_index(lines, i)
                    other = tc.DECL.match(lines[k]) if k < len(lines) else None
                    if other and other.group(1) == name.group(1):
                        problems.append(f"has a cfg(not(test)) twin, not excluded: {rel} :: {label[g]}")
    if problems:
        raise CodemodError(3, problems)
    edits = [{"file": rel, "line": j + 1, "item": label[(rel, j)], "before": tree.lines(rel)[j],
              "after": rewrite(tree.lines(rel)[j])} for rel, j in sorted(wanted)]
    dl = {g for site, t in found if tree.crate_of(site.rsplit(":", 1)[0]) == "DL" for g in chains[t]}
    counts = {"scanned": len(gates), "excluded": len(excluded) + len(deferred), "listed": len(excluded),
              "deferred": len(deferred), "subs": len(edits), "files": len({e["file"] for e in edits}),
              "companions": len(companions), "pinned": len(pinned), "dl_scanned": len(dl),
              "dl_subs": len(wanted & dl)}
    state = lambda g: "excluded" if g in excluded else "deferred" if g in deferred else "edit"
    rows = [{"file": g[0], "line": g[1] + 1, "item": label[g], "state": state(g), "companion": g in companions}
            for g in sorted(gates)]
    return {"edits": edits, "counts": counts, "gates": rows,
            "excluded": sorted(f"{g[0]} :: {label[g]}" for g in excluded),
            "deferred": sorted(f"{g[0]} :: {label[g]}" for g in deferred), "pinned": pinned}


def test_module_entries(tree):
    """Exclusion rows for every scanned gate that opens a module holding tests (`--list-test-modules`)."""
    try:
        return plan(tree, [])
    except CodemodError as err:
        rows = []
        for line in err.lines:
            m = re.match(r"opens a test module, not excluded: (\S+) :: (.*)$", line)
            if m:
                rows.append({"file": m.group(1), "item": m.group(2), "reason": "test_module"})
        return rows


def apply(tree, edits):
    by_file = collections.defaultdict(list)
    for e in edits:
        by_file[e["file"]].append(e)
    for rel, rows in by_file.items():
        lines = list(tree.lines(rel))
        for e in rows:
            assert lines[e["line"] - 1] == e["before"], (rel, e["line"])
            lines[e["line"] - 1] = e["after"]
        (tree.root / rel).write_text("\n".join(lines), "utf-8")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--tree", default=str(HERE.parents[1]))
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--count", action="store_true", help="print the counts, write nothing")
    mode.add_argument("--check", action="store_true", help="exit 1 when the tree still has edits to make")
    mode.add_argument("--list-test-modules", action="store_true")
    parser.add_argument("--exclude", default=str(EXCLUDE))
    parser.add_argument("--plan", help="write the edit plan as JSON")
    args = parser.parse_args(argv)
    tree = Tree(args.tree)
    if args.list_test_modules:
        print(json.dumps(test_module_entries(tree), indent=1, ensure_ascii=False))
        return 0
    try:
        result = plan(tree, load_exclude(args.exclude))
    except CodemodError as err:
        print(f"TS_CODEMOD_FAIL code={err.code}")
        print("\n".join(err.lines))
        return err.code
    if args.plan:
        Path(args.plan).write_text(json.dumps(result, indent=1, ensure_ascii=False) + "\n", "utf-8")
    c = result["counts"]
    print(f"TS_CODEMOD scanned={c['scanned']} excluded={c['excluded']} (listed={c['listed']} "
          f"deferred={c['deferred']}) subs={c['subs']} files={c['files']} companions={c['companions']} "
          f"pinned={c['pinned']} dl_scanned={c['dl_scanned']} dl_subs={c['dl_subs']}")
    if args.check:
        return 1 if result["edits"] else 0
    if not args.count:
        apply(tree, result["edits"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
