"""Allow-list class of a crate-split diff for equiv_check.py (#7353 VT2): `vis`, `move` or `C`.

Nothing is approved unless it is one of these edits, compared as Rust tokens (strings, raw/byte/C strings,
chars and lifetimes are single tokens; comments, doc comments included, are dropped):

  vis   the visibility group (`pub`, `pub(crate|self|super|in ..)`) leading an item, a field of a braced
        struct/union, or a top-level tuple field — outside every macro token tree and fn body — is added,
        dropped or changed. Nothing else in the file changes.
  move  module-level units (items, inline `mod` heads, `use` statements) leave one module and arrive in
        another with the same tokens, or stay and differ only in paths. Units pair 1:1 by (kind, name);
        a key with more than one candidate on a side is C. Every changed path must resolve, from the crate
        root, to the same definition or to a moved definition's new place; a moved body re-resolves its
        `crate`/`self`/`super`, child-module and scope-bound names. `use` edits: a scope with a glob on
        either side must keep exactly the same bindings and globs; elsewhere a binding may only appear for
        a name the scope did not use before, disappear for a name the scope no longer uses, or point at a
        moved definition's new place. `mod x;` lines may appear or disappear.
  C     everything else, and whatever cannot be resolved with certainty.
"""

from __future__ import annotations

import collections
import re
from dataclasses import dataclass, field

KEYWORDS = {"as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false",
            "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
            "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            "union", "default", "macro_rules"}
ITEM_KWS = ("fn", "struct", "enum", "union", "trait", "impl", "mod", "type", "const", "static", "use", "extern")
STD_CRATES = {"std", "core", "alloc", "proc_macro", "test"}
PRELUDE = {"Result", "Option", "Error", "Box", "Vec", "String", "Ok", "Err", "Some", "None", "Clone", "Copy", "Send",
           "Sync", "Sized", "Drop", "Fn", "FnMut", "FnOnce", "Default", "Debug", "Eq", "PartialEq", "Ord",
           "PartialOrd", "Hash", "Iterator", "IntoIterator", "ToOwned", "ToString", "From", "Into", "TryFrom",
           "TryInto", "AsRef", "AsMut", "FromIterator", "Extend"}
LEX = re.compile(r"""
    (?P<ws>\s+)
  | (?P<line>//[^\n]*)
  | (?P<raw>[bc]?r(?P<h>\#*)"[\s\S]*?"(?P=h))
  | (?P<str>[bc]?"(?:\\[\s\S]|[^"\\])*")
  | (?P<chr>b?'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]*\}|.)|[^\\'\n])')
  | (?P<life>'(?:r\#)?[A-Za-z_]\w*)
  | (?P<rawid>r\#[A-Za-z_]\w*)
  | (?P<num>\d[\w]*(?:\.\d[\w]*)?)
  | (?P<id>[A-Za-z_]\w*)
  | (?P<path>::)
  | (?P<p>.)
""", re.X)
PAIRS = {"(": ")", "[": "]", "{": "}"}


class LexError(ValueError):
    pass


@dataclass
class Tok:
    kind: str  # id lit life p
    text: str
    line: int


def lex(text: str) -> list:
    """Rust tokens; comments (nested block comments too) dropped."""
    out, i, line = [], 0, 1
    while i < len(text):
        if text.startswith("/*", i):
            depth, j = 0, i
            while j < len(text):
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                    if depth == 0:
                        break
                else:
                    j += 1
            if depth:
                raise LexError(f"unterminated block comment at line {line}")
            line += text.count("\n", i, j)
            i = j
            continue
        m = LEX.match(text, i)
        kind = m.lastgroup if m.lastgroup != "h" else "raw"
        if kind not in ("ws", "line"):
            k = {"raw": "lit", "str": "lit", "chr": "lit", "num": "lit", "life": "life", "rawid": "id",
                 "id": "id", "path": "p", "p": "p"}[kind]
            out.append(Tok(k, m.group(0), line))
        line += m.group(0).count("\n")
        i = m.end()
    return out


def match_groups(toks: list) -> dict:
    close, stack = {}, []
    for i, t in enumerate(toks):
        if t.kind == "p" and t.text in PAIRS:
            stack.append(i)
        elif t.kind == "p" and t.text in ")]}":
            if not stack or PAIRS[toks[stack[-1]].text] != t.text:
                raise LexError(f"unbalanced `{t.text}` at line {t.line}")
            close[stack.pop()] = i
    if stack:
        raise LexError(f"unclosed `{toks[stack[-1]].text}` at line {toks[stack[-1]].line}")
    return close


@dataclass
class Unit:
    path: str
    module: tuple
    kind: str  # fn struct enum union trait impl mod modhead type const static use extern macro attr other
    name: str
    start: int
    end: int  # exclusive
    line: int
    tokens: tuple = ()  # texts, allowed visibility removed
    raw: tuple = ()     # texts as written


@dataclass
class FileShape:
    path: str
    module: tuple
    toks: list
    units: list = field(default_factory=list)
    vis: set = field(default_factory=set)  # token indexes of allowed visibility groups


def is_p(t, text):
    return t.kind == "p" and t.text == text


def vis_end(toks, i, close):
    """End (exclusive) of a visibility group starting at i, or i when there is none."""
    if i < len(toks) and toks[i].kind == "id" and toks[i].text == "pub":
        if i + 1 < len(toks) and is_p(toks[i + 1], "(") and i + 2 < len(toks) and \
                toks[i + 2].text in ("crate", "self", "super", "in"):
            return close[i + 1] + 1
        return i + 1
    return i


def skip_attrs(toks, i, j, close):
    while i + 1 < j and is_p(toks[i], "#") and is_p(toks[i + 1], "["):
        i = close[i + 1] + 1
    return i


class Shaper:
    """Splits one file into module-level units and finds the allowed visibility positions."""

    def __init__(self, path, module, text):
        self.shape = FileShape(path, module, lex(text))
        self.close = match_groups(self.shape.toks)

    def run(self):
        self.items(0, len(self.shape.toks), self.shape.module, units=True)
        return self.shape

    def mark(self, i):
        end = vis_end(self.shape.toks, i, self.close)
        self.shape.vis.update(range(i, end))
        return end

    def items(self, i, j, module, units):
        toks, close = self.shape.toks, self.close
        while i < j:
            start = i
            if is_p(toks[i], "#") and i + 2 < j and is_p(toks[i + 1], "!") and is_p(toks[i + 2], "["):
                end = close[i + 2] + 1
                if units:
                    self.unit(module, "attr", "", start, end, start)
                i = end
                continue
            if is_p(toks[i], ";"):
                i += 1
                continue
            i = skip_attrs(toks, i, j, close)
            body = self.mark(i)
            kind, name, end, inner = self.item(body, j)
            if kind == "mod" and inner is not None:
                if units:
                    self.unit(module, "modhead", name, start, inner, start)
                self.items(inner + 1, close[inner], module + (name,), units)
            else:
                if inner is not None and kind in ("impl", "trait", "extern"):
                    self.items(inner + 1, close[inner], module, units=False)
                if units:
                    self.unit(module, kind, name, start, end, start)
            i = end

    def item(self, i, j):
        """(kind, name, end, index of the body `{` to descend or None) of the item whose keywords start at i."""
        toks, close = self.shape.toks, self.close
        if i + 1 < j and toks[i].kind == "id" and (is_p(toks[i + 1], "!") or is_p(toks[i + 1], "::")):
            # A macro invocation or macro_rules!: one unit, never descended.
            k = i
            while k < j and not is_p(toks[k], "!"):
                k += 1
            name = "".join(t.text for t in toks[i:k])
            k += 1
            if k < j and toks[k].kind == "id":
                name, k = f"{name}!{toks[k].text}", k + 1
            if k < j and toks[k].kind == "p" and toks[k].text in PAIRS:
                end = close[k] + 1
                if toks[k].text != "{" and end < j and is_p(toks[end], ";"):
                    end += 1
                return "macro", name, end, None
            return "other", name, self.to_semicolon(i, j), None
        kind, k, name = None, i, ""
        while k < j:
            t = toks[k]
            if t.kind == "p" and t.text in PAIRS:
                if t.text == "{":
                    break
                k = close[k] + 1
                continue
            if is_p(t, ";") or is_p(t, "="):
                break
            if t.kind == "id" and t.text in ITEM_KWS and kind is None:
                if t.text == "const" and k + 1 < j and toks[k + 1].text in ("fn", "unsafe", "async", "extern"):
                    k += 1
                    continue
                if t.text == "extern" and k + 1 < j and (toks[k + 1].text == "crate" or toks[k + 1].kind == "lit"
                                                         and k + 2 < j and toks[k + 2].text == "fn"):
                    kind = "externcrate" if toks[k + 1].text == "crate" else None
                    k += 1
                    continue
                if t.text == "unsafe" or t.text == "async":
                    k += 1
                    continue
                kind = t.text
                if k + 1 < j and toks[k + 1].kind == "id" and kind not in ("impl", "use"):
                    name = toks[k + 1].text
            k += 1
        if kind in (None,) and k < j and is_p(toks[k], "{"):
            kind = "extern" if any(toks[x].text == "extern" for x in range(i, k)) else "other"
        if kind in ("use", "externcrate", "const", "static", "type", None) or kind == "other":
            end = self.to_semicolon(i, j)
            return kind or "other", name, end, None
        if kind == "impl":
            name = " ".join(t.text for t in toks[i:k] if t.kind != "p" or t.text not in ";")
        if k < j and is_p(toks[k], "{"):
            if kind in ("struct", "union"):
                self.fields(k)
            return kind, name, close[k] + 1, (k if kind in ("mod", "impl", "trait", "extern") else None)
        if kind == "struct":
            self.tuple_fields(i, k)
        return kind, name, (k + 1 if k < j and is_p(toks[k], ";") else k), None

    def to_semicolon(self, i, j):
        toks, close = self.shape.toks, self.close
        while i < j:
            if toks[i].kind == "p" and toks[i].text in PAIRS:
                i = close[i] + 1
                continue
            if is_p(toks[i], ";"):
                return i + 1
            i += 1
        return j

    def fields(self, brace):
        toks, close = self.shape.toks, self.close
        i, j = brace + 1, close[brace]
        while i < j:
            i = skip_attrs(toks, i, j, close)
            self.mark(i)
            while i < j and not is_p(toks[i], ","):
                i = close[i] + 1 if toks[i].kind == "p" and toks[i].text in PAIRS else i + 1
            i += 1

    def tuple_fields(self, i, k):
        """The paren group right after `struct Name<..>`: its top-level fields."""
        toks, close = self.shape.toks, self.close
        m = i
        while m < k and toks[m].text != "struct":
            m += 1
        m += 2
        depth = 0
        while m < k:
            t = toks[m]
            if is_p(t, "<"):
                depth += 1
            elif is_p(t, ">"):
                depth -= 1
            elif depth == 0 and is_p(t, "("):
                break
            m = close[m] + 1 if t.kind == "p" and t.text in PAIRS else m + 1
        if m >= k or not is_p(toks[m], "("):
            return
        a, b = m + 1, close[m]
        while a < b:
            a = skip_attrs(toks, a, b, close)
            self.mark(a)
            angle = 0
            while a < b and not (angle == 0 and is_p(toks[a], ",")):
                if is_p(toks[a], "<"):
                    angle += 1
                elif is_p(toks[a], ">"):
                    angle -= 1
                a = close[a] + 1 if toks[a].kind == "p" and toks[a].text in PAIRS else a + 1
            a += 1

    def unit(self, module, kind, name, start, end, line_at):
        toks = self.shape.toks
        if kind == "mod":
            kind = "moddecl"
        self.shape.units.append(Unit(self.shape.path, module, kind, name, start, end, toks[line_at].line))


def finish(shape: FileShape):
    for u in shape.units:
        u.raw = tuple(t.text for t in shape.toks[u.start:u.end])
        u.tokens = tuple(t.text for n, t in enumerate(shape.toks[u.start:u.end], u.start) if n not in shape.vis)
    return shape


def shape_file(path, module, text) -> FileShape:
    return finish(Shaper(path, module, text).run())


# ---------------------------------------------------------------------------------------------- scopes

def use_entries(toks: tuple) -> list:
    """[(path segments, bound name)] of one `use` unit's tokens (globs bind '*')."""
    try:
        k = list(toks).index("use") + 1
    except ValueError:
        return []
    body = list(toks[k:])
    if body and body[-1] == ";":
        body.pop()
    out = []

    def tree(i, prefix):
        path = list(prefix)
        while i < len(body):
            t = body[i]
            if t == "::":
                i += 1
            elif t == "{":
                i += 1
                while i < len(body) and body[i] != "}":
                    i = i + 1 if body[i] == "," else max(tree(i, path), i + 1)
                return i + 1
            elif t == "*":
                out.append((tuple(path), "*"))
                return i + 1
            elif t in (",", "}"):
                return i
            else:
                path.append(t)
                i += 1
                if i < len(body) and body[i] == "::":
                    continue
                bound = path[-1]
                if i + 1 < len(body) and body[i] == "as":
                    bound, i = body[i + 1], i + 2
                if path[-1] == "self" and len(path) > 1:
                    path.pop()
                    bound = path[-1] if bound == "self" else bound
                out.append((tuple(path), bound))
                return i
        return i

    tree(0, ())
    return out


@dataclass
class Scope:
    module: tuple
    items: set = field(default_factory=set)
    bindings: dict = field(default_factory=dict)  # name -> [path segments]
    globs: list = field(default_factory=list)      # [path segments]
    idents: set = field(default_factory=set)


def scopes_of(shapes: list) -> dict:
    out = {}
    for shape in shapes:
        out.setdefault(shape.module, Scope(shape.module))
        for u in shape.units:
            scope = out.setdefault(u.module, Scope(u.module))
            if u.kind != "use":
                scope.idents.update(t for t in u.raw if re.match(r"^[A-Za-z_]\w*$", t))
            if u.kind == "use":
                for path, bound in use_entries(u.raw):
                    if bound == "*":
                        scope.globs.append(path)
                    elif bound != "_":
                        scope.bindings.setdefault(bound, []).append(path)
            elif u.name and u.kind not in ("impl", "macro", "attr", "other"):
                scope.items.add(u.name)
            if u.kind == "modhead":
                out.setdefault(u.module + (u.name,), Scope(u.module + (u.name,)))
    return out


class Resolver:
    """Definition paths (crate name first) of written paths in one tree's scopes; None when unsure."""

    def __init__(self, scopes: dict, crates: set):
        self.scopes, self.crates = scopes, crates

    def path(self, segs, module, depth=0):
        segs = [s for s in segs]
        if not segs or depth > 8:
            return None
        if segs[0] == "":
            return tuple(segs[1:]) if len(segs) > 1 and segs[1] in self.crates else None
        first, scope = segs[0], self.scopes.get(module)
        if first == "crate":
            return (module[0],) + tuple(segs[1:])
        if first == "self":
            return module + tuple(segs[1:])
        if first == "super":
            n = 0
            while n < len(segs) and segs[n] == "super":
                n += 1
            return module[:-n] + tuple(segs[n:]) if len(module) > n else None
        if first == "Self" or scope is None:
            return None
        bound = scope.bindings.get(first, [])
        if len(bound) > 1:
            return None
        if bound:
            base = self.path(list(bound[0]), module, depth + 1)
            return base + tuple(segs[1:]) if base else None
        if first in scope.items:
            return module + tuple(segs)
        if first in self.crates:
            return tuple(segs)
        return None

    def name(self, name, module):
        """Where an unqualified name points in a scope: a binding or an item of the module; None otherwise."""
        scope = self.scopes.get(module)
        if scope is None:
            return None
        if name in scope.bindings:
            return self.path([name], module)
        if name in scope.items:
            return module + (name,)
        return None

    def globs(self, module):
        scope = self.scopes.get(module)
        return None if scope is None else frozenset(self.path(list(g) + ["*"], module) for g in scope.globs)

    def binds(self, name, module):
        scope = self.scopes.get(module)
        return scope is not None and (name in scope.bindings or name in scope.items)


# ---------------------------------------------------------------------------------------------- classify

def path_exprs(tokens) -> list:
    """[(prefix, name, after_dot)] once `a::b::c` paths fold into ((a, b), c); `::x` keeps a '' head."""
    out, prefix, joining = [], (), False
    for k, tok in enumerate(tokens):
        if tok == "::":
            if out and not joining and re.match(r"^(?:[A-Za-z_]\w*|>)$", tokens[k - 1]):
                before, name, dot = out.pop()
                prefix = before + (name,)
            elif not joining:
                prefix = ("",)  # `::std::x`, an absolute path
            joining = True
        elif joining:
            out.append((prefix, tok, False))
            prefix, joining = (), False
        else:
            out.append(((), tok, k > 0 and tokens[k - 1] == "."))
    return out


@dataclass
class Verdict:
    klass: str = "move"
    reasons: list = field(default_factory=list)  # (path, line, why)
    moved: int = 0
    inplace: int = 0
    vis: int = 0
    paths: int = 0
    uses: int = 0
    moddecls: int = 0

    def c(self, path, line, why):
        self.reasons.append((path, line, why))


def mapped(old, new, moves: dict) -> bool:
    if old is None or new is None:
        return False
    if old == new:
        return True
    for n in range(len(old), 0, -1):
        target = moves.get(old[:n])
        if target is not None:
            return target + old[n:] == new
    return False


class Classifier:
    def __init__(self, base_shapes, head_shapes, crates):
        self.base, self.head = base_shapes, head_shapes
        self.rb = Resolver(scopes_of(base_shapes), crates)
        self.rh = Resolver(scopes_of(head_shapes), crates)
        self.v = Verdict()

    def run(self) -> Verdict:
        removed = [u for s in self.base for u in s.units]
        added = [u for s in self.head for u in s.units]
        # Units present unchanged in the same module cancel out.
        same = collections.Counter((u.module, u.raw) for u in added)
        keep_r = []
        for u in removed:
            if same[(u.module, u.raw)]:
                same[(u.module, u.raw)] -= 1
            else:
                keep_r.append(u)
        left = collections.Counter((u.module, u.raw) for u in removed) - collections.Counter(
            (u.module, u.raw) for u in keep_r)
        keep_a = []
        for u in added:
            if left[(u.module, u.raw)]:
                left[(u.module, u.raw)] -= 1
            else:
                keep_a.append(u)
        uses_r = [u for u in keep_r if u.kind == "use"]
        uses_a = [u for u in keep_a if u.kind == "use"]
        # A `use` that only changed its visibility is vis; any other changed `use` keeps no attributes
        # (a cfg on a `use` decides which builds have the binding).
        for r in list(uses_r):
            a = next((a for a in uses_a if a.module == r.module and a.tokens == r.tokens), None)
            if a is not None:
                uses_r.remove(r)
                uses_a.remove(a)
                self.v.vis += 1
        for u in uses_r + uses_a:
            if u.tokens and u.tokens[0] == "#":
                self.v.c(u.path, u.line, f"`use` with attributes changed: {' '.join(u.raw[:12])}")
        rest_r = [u for u in keep_r if u.kind != "use"]
        rest_a = [u for u in keep_a if u.kind != "use"]
        self.macro_order()
        pairs, moves = self.pair(rest_r, rest_a)
        for r, a in pairs:
            self.equivalent(r, a, moves)
        self.use_changes(uses_r, uses_a, moves)
        if self.v.reasons:
            self.v.klass = "C"
        elif self.v.moved or self.v.paths or self.v.uses or self.v.moddecls:
            self.v.klass = "move"
        elif self.v.vis:
            self.v.klass = "vis"
        return self.v

    def macro_order(self):
        """`macro_rules!` is textually scoped: a file holding one keeps the order of the units it keeps."""
        heads = {s.path: s for s in self.head}
        for shape in self.base:
            other = heads.get(shape.path)
            if other is None or not any(u.name.startswith("macro_rules!") for u in shape.units + other.units):
                continue
            common = collections.Counter(u.raw for u in shape.units) & collections.Counter(u.raw for u in other.units)
            order = lambda units: [u.raw for u in units if common[u.raw]]
            if order(shape.units) != order(other.units):
                self.v.c(shape.path, 1, "units reordered in a file with `macro_rules!`")

    def key(self, u, with_module):
        name = u.name if u.name else " ".join(u.tokens[:12])
        return ((u.module,) if with_module else ()) + (u.kind, name)

    def pair(self, removed, added):
        pairs, moves = [], {}
        for with_module in (True, False):
            by_r, by_a = collections.defaultdict(list), collections.defaultdict(list)
            for u in removed:
                by_r[self.key(u, with_module)].append(u)
            for u in added:
                by_a[self.key(u, with_module)].append(u)
            for k in set(by_r) & set(by_a):
                if len(by_r[k]) == 1 and len(by_a[k]) == 1:
                    r, a = by_r[k][0], by_a[k][0]
                    pairs.append((r, a))
                    removed.remove(r)
                    added.remove(a)
                    if r.module != a.module and r.name and r.kind not in ("impl", "macro", "attr", "other"):
                        moves[r.module + (r.name,)] = a.module + (a.name,)
        for side, units in (("removed", removed), ("added", added)):
            for u in units:
                if u.kind == "moddecl":
                    self.v.moddecls += 1
                    continue
                self.v.c(u.path, u.line, f"{side} {u.kind} `{u.name or ' '.join(u.raw[:6])}` has no 1:1 counterpart")
        return pairs, moves

    def equivalent(self, r, a, moves):
        if r.kind == "macro" and r.module != a.module:
            self.v.c(a.path, a.line, f"macro `{a.name}` moved (textual order matters)")
            return
        if r.raw != a.raw and r.tokens == a.tokens and r.module == a.module:
            self.v.vis += 1
            self.v.inplace += 1
            return
        old, new = path_exprs(r.tokens), path_exprs(a.tokens)
        if [n for _, n, _ in old] != [n for _, n, _ in new] or [d for *_, d in old] != [d for *_, d in new]:
            self.v.c(a.path, a.line, f"{a.kind} `{a.name}` changed beyond paths and visibility")
            return
        moved = r.module != a.module
        if moved:
            self.v.moved += 1
            gb, gh = self.rb.globs(r.module), self.rh.globs(a.module)
        else:
            self.v.inplace += 1
        for (p, n, dot), (q, _, _) in zip(old, new):
            if dot or n in KEYWORDS or not re.match(r"^[A-Za-z_]\w*$", n):
                if p == q:
                    continue
            if p == q and not moved:
                continue
            if p != q:
                ro, rn = self.rb.path(list(p) + [n], r.module), self.rh.path(list(q) + [n], a.module)
                if not mapped(ro, rn, moves):
                    self.v.c(a.path, a.line, f"path `{'::'.join(p + (n,))}` -> `{'::'.join(q + (n,))}` is not "
                                             f"the same or a moved definition ({ro} -> {rn})")
                    return
                self.v.paths += 1
                continue
            # A moved body, same text: names that depend on the scope must still mean the same thing.
            head = p[0] if p else n
            if dot or head in KEYWORDS - {"crate", "self", "super"} or not re.match(r"^[A-Za-z_]\w*$", head):
                continue
            scoped = head in ("crate", "self", "super") or self.rb.binds(head, r.module) or \
                self.rh.binds(head, a.module)
            if scoped:
                ro = self.rb.path(list(p) + [n], r.module) if p else self.rb.name(n, r.module)
                rn = self.rh.path(list(q) + [n], a.module) if q else self.rh.name(n, a.module)
                if not mapped(ro, rn, moves):
                    self.v.c(a.path, a.line, f"moved `{a.name}`: `{'::'.join(p + (n,))}` means {ro} before, "
                                             f"{rn} after")
                    return
            elif gb != gh and n not in PRELUDE:
                self.v.c(a.path, a.line, f"moved `{a.name}`: `{n}` may come from different globs")
                return

    def use_changes(self, removed, added, moves):
        modules = {u.module for u in removed + added}
        for module in sorted(modules):
            sb, sh = self.rb.scopes.get(module), self.rh.scopes.get(module)
            if sb is None or sh is None:
                self.v.uses += 1  # the module only exists on one side: its imports are new or gone with it
                continue
            unit = next(u for u in removed + added if u.module == module)
            gb = sorted(sb.globs)
            gh = sorted(sh.globs)
            bb = {k: sorted(v) for k, v in sb.bindings.items()}
            bh = {k: sorted(v) for k, v in sh.bindings.items()}
            if (gb or gh) and (gb != gh or bb != bh):
                self.v.c(unit.path, unit.line, f"`use` change in `{'::'.join(module)}`, a scope with a glob")
                continue
            for name in sorted(set(bb) | set(bh)):
                if bb.get(name) == bh.get(name):
                    continue
                if len(bb.get(name, [])) > 1 or len(bh.get(name, [])) > 1:
                    self.v.c(unit.path, unit.line, f"`{name}` bound more than once in `{'::'.join(module)}`")
                    break
                before = self.rb.path(list(bb[name][0]), module) if name in bb else \
                    (module + (name,) if name in sb.items else None)
                after = self.rh.path(list(bh[name][0]), module) if name in bh else \
                    (module + (name,) if name in sh.items else None)
                if name in bh and name not in bb and before is None and name not in sb.idents and name not in PRELUDE:
                    pass  # a fresh name the scope never used
                elif name in bb and name not in bh and after is None and name not in sh.idents:
                    pass  # dropped together with its last use
                elif not mapped(before, after, moves):
                    self.v.c(unit.path, unit.line, f"`use` binds `{name}` in `{'::'.join(module)}` to {after}, "
                                                   f"was {before}")
                    break
                self.v.uses += 1


def classify(base_files: dict, head_files: dict, modules: dict, crates: set) -> Verdict:
    """base_files/head_files: {path: text or None} of the changed .rs files; modules: {(side, path): module}."""
    shapes = {"-": [], "+": []}
    verdict = Verdict()
    for side, files in (("-", base_files), ("+", head_files)):
        for path, text in sorted(files.items()):
            if text is None:
                continue
            try:
                shapes[side].append(shape_file(path, modules[(side, path)], text))
            except LexError as err:
                verdict.c(path, 0, f"cannot tokenize ({err})")
    if verdict.reasons:
        verdict.klass = "C"
        return verdict
    return Classifier(shapes["-"], shapes["+"], crates).run()
