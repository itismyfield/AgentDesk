"""Which definition a rustc type path names (`lower::b::Point`, `upper::b::Point`, `crate::b::Point`).

rustc prints a type as the reporting crate sees it: its own types without a crate name, a foreign type through
its crate name or through a public re-export (`pub use lower::{b, c};`). An upper-local `mod lower` prints the
same text as the lower crate. So a path is read every way the reporting target could read it (local, and through
each crate name), following modules and explicit or glob `use` lines across crates, and owns a lower definition
only when every reading lands on that one definition.
"""

from pathlib import Path
import re

import vistool as vt

HOPS = 8


class Tree(vt.Layout):
    """Module map of one crate target (any root file under `base`); `locate`/`module_at` come from Layout."""

    def __init__(self, root, base):
        self.repo = Path(base).resolve()
        root = Path(root).resolve()
        inv = vt.rts.walk(root, self.repo)
        self.module_of, self.file_of, self._inline = {}, {}, {}
        for path, modules in inv.modules.items():
            rel = Path(path).resolve().relative_to(self.repo).as_posix()
            module = min(tuple(x for x in m if x != "crate") for m in modules)
            self.module_of[rel] = module
            self.file_of.setdefault(module, rel)
        self.root_file = root.relative_to(self.repo).as_posix()
        self.file_of[()], self.module_of[self.root_file] = self.root_file, ()


class Owner:
    """Definitions a type path names. `crates` maps every other crate's name to its lib root under `base`;
    lower is `lower_lib`, read through its layout and the widener's current text."""

    def __init__(self, layout, read, lower_lib, crates, upper_root, base):
        self.base, self.upper_root = Path(base), str(Path(upper_root).resolve())
        self.names = {name: str(Path(root).resolve()) for name, root in crates.items()}
        self.names[lower_lib] = lower_lib
        self.lower = lower_lib
        self.trees = {lower_lib: layout}
        self.reads = {lower_lib: read}
        self._levels = {}

    def definitions(self, segs, name, side, src=None):
        """{(crate key, rel, line index)} `segs::name` names for a diagnostic of `side`, or None when the path
        cannot be read (no module segment, unknown side)."""
        if side not in ("lower", "upper") or not segs:
            return None
        local = self.lower if side == "lower" else self._key(src or self.upper_root)
        segs = tuple(segs[1:] if segs[0] == "crate" else segs)
        out = self._item(local, (), segs, name, HOPS)
        if segs and self.names.get(segs[0], local) != local:
            out |= self._item(self.names[segs[0]], (), segs[1:], name, HOPS)
        return out

    def _key(self, root):
        key = str(Path(root).resolve())
        if key not in self.trees:
            self.trees[key] = Tree(key, self.base)
            self.reads[key] = lambda rel, tree=self.trees[key]: vt.read_lines(tree.repo / rel)
        return key

    def _tree(self, key):
        return self.trees[key] if key == self.lower else self.trees[self._key(key)]

    def _lines_in(self, key, module):
        """(rel, lines, [item-level indexes of `module`]) or None when the crate has no such module."""
        tree = self._tree(key)
        found = tree.locate(module)
        if found is None:
            return None
        rel, first, last = found
        lines = self.reads[key](rel)
        if (key, rel) not in self._levels:
            self._levels[(key, rel)] = vt.item_level_lines(lines)
        level = self._levels[(key, rel)]
        return rel, lines, [j for j in range(max(first - 1, 0), min(last, len(lines)))
                            if j in level and tree.module_at(rel, j + 1) == module]

    def _uses(self, key, module, name):
        """(path, original) of every `use` in the module binding `name`, then (path, '*') of its globs."""
        found = self._lines_in(key, module)
        if found is None:
            return [], []
        _, lines, idx = found
        entries = [e for j in idx if (lambda m: m and m.group("kw") == "use")(vt.ITEM.match(lines[j]))
                   for e in vt.use_entries(vt.use_text(lines, j))]
        return [(p, o) for p, o, b in entries if b == name and o != "*"], [p for p, o, _ in entries if o == "*"]

    def _item(self, key, module, segs, name, hops):
        if not segs:
            return self._structs(key, module, name, hops)
        out = set()
        for k, m in self._child(key, module, segs[0], hops):
            out |= self._item(k, m, segs[1:], name, hops)
        return out

    def _child(self, key, module, seg, hops):
        """{(crate key, module)} the name `seg` stands for as a module inside `module`."""
        if self._tree(key).locate(module + (seg,)) is not None:
            return {(key, module + (seg,))}
        if hops <= 0:
            return set()
        named, globs = self._uses(key, module, seg)
        out = set()
        for path, orig in named:
            out |= self._use_target(key, module, list(path) + [orig], hops - 1)
        if not named:
            for path in globs:
                for k, m in self._use_target(key, module, list(path), hops - 1):
                    out |= self._child(k, m, seg, hops - 1)
        return out

    def _structs(self, key, module, name, hops):
        found = self._lines_in(key, module)
        if found is None:
            return set()
        rel, lines, idx = found
        rx = re.compile(r"^\s*(?:(?:" + vt.VIS + r")\s+)?struct\s+" + re.escape(name) + r"\b")
        out = {(key, rel, j) for j in idx if rx.match(lines[j])}
        if out or hops <= 0:
            return out
        named, globs = self._uses(key, module, name)
        for path, orig in named:
            for k, m in self._use_target(key, module, list(path), hops - 1):
                out |= self._structs(k, m, orig, hops - 1)
        if not named:
            for path in globs:
                for k, m in self._use_target(key, module, list(path), hops - 1):
                    out |= self._structs(k, m, name, hops - 1)
        return out

    def _use_target(self, key, module, path, hops):
        """{(crate key, module)} a `use` path names from `module` (2018 paths: crate/self/super, a local item,
        else a crate name)."""
        if not path:
            return {(key, module)}
        head, rest = path[0], path[1:]
        if head == "crate":
            starts = {(key, ())}
        elif head in ("self", "super"):
            starts = {(key, module if head == "self" else module[:-1])}
        else:
            starts = self._child(key, module, head, hops)
            if not starts and head in self.names:
                starts = {(self.names[head], ())}
        for seg in rest:
            starts = {(k, m[:-1]) for k, m in starts} if seg == "super" else \
                {hit for k, m in starts for hit in self._child(k, m, seg, hops)}
        return starts

    def owned(self, path, name, side, src, hits):
        """The one lower struct of `hits` the message path names, or [] when its owner is not certain."""
        defs = self.definitions(re.sub(r"<.*", "", path).split("::")[:-1], name, side, src)
        if not defs or len(defs) != 1:
            return []
        key, rel, index = next(iter(defs))
        return [(rel, index)] if key == self.lower and (rel, index) in hits else []
