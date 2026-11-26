"""Which lower definition a rustc type path names (`lower::b::Point`, `upper::b::Point`, `crate::b::Point`).

rustc prints a type as the reporting target sees it: its own types without a crate name (sometimes cut to a
suffix), a foreign type through its crate name or a public re-export. The path is read locally and, when it
starts with a crate name, through that crate, following modules and `use` re-exports. An edit is allowed only
when every reading finished and they all land on one lower definition; anything uncertain raises `Hold` with a
code from HOLD_CODES, and the diagnostic is left to a hand check.
"""

from pathlib import Path
import re

import vistool as vt

HOPS = 8
HOLD_CODES = {
    "absolute_path": "a `::` path not at the start of a use statement, or naming no known crate",
    "glob_priority": "the name could come from more than one glob re-export",
    "use_cycle": "a `use` chain comes back to itself",
    "hops": "a `use` chain longer than the search budget",
    "module_crate_name": "the path starts with a name that is both a local module and a crate",
    "multi_module_file": "a file reached under more than one module path (`#[path]` twice)",
    "cfg_module": "a module behind cfg, or one module with more than one file or block",
    "renamed_dep": "a dependency renamed with `package =`",
    "trimmed_path": "the path starts at no known root (rustc cut it to a suffix)",
    "map_failed": "the module map of a file on the way could not be read whole",
    "unresolved_path": "a segment the module maps do not name",
    "non_struct_item": "the name is also a type alias, enum, union or trait there",
    "not_lower": "the one definition is not in the lower crate",
    "ambiguous": "more than one definition",
}
STRUCT = r"^\s*(?:(?:" + vt.VIS + r")\s+)?struct\s+{}\b"
OTHER = r"^\s*(?:(?:" + vt.VIS + r")\s+)?(?:type|enum|union|trait)\s+{}\b"


class Hold(Exception):
    def __init__(self, code, detail=""):
        super().__init__(f"{code} {detail}".strip())
        self.code, self.detail = code, detail


def renamed_deps(cargos):
    """Names dependencies are imported under through `package = ..` in any of the parsed manifests."""
    out = set()
    for cargo in cargos:
        tables = [cargo] + list(cargo.get("target", {}).values())
        for table in tables:
            for kind in ("dependencies", "dev-dependencies", "build-dependencies"):
                out |= {k.replace("-", "_") for k, v in table.get(kind, {}).items()
                        if isinstance(v, dict) and "package" in v}
    return out


class Tree(vt.Layout):
    """Module map of one crate target, keeping what makes a file uncertain: several module paths, several files
    for one module, a cfg on the way, or a walker error/unreadable site in it."""

    def __init__(self, root, base, manifest_dir=None):
        self.repo, root = Path(base).resolve(), Path(root).resolve()
        rel = lambda p: Path(p).resolve().relative_to(self.repo).as_posix()
        try:
            inv = vt.rts.walk(root, self.repo, manifest_dir=manifest_dir)
            self.modules = {rel(p): {tuple(x for x in m if x != "crate") for m in ms} for p, ms in inv.modules.items()}
            self.gated = {rel(p) for p, chains in inv.file_cfg_texts.items() if any(chains)}
            self.root_file = rel(root)
        except (OSError, UnicodeError, ValueError) as exc:
            raise Hold("map_failed", f"{root}: {exc}")
        self.broken = set()
        for entry in inv.errors + inv.unreadable + inv.read_failures:
            site = Path(entry.split(":")[0])
            try:
                self.broken.add(rel(site if site.is_absolute() else self.repo / site))
            except ValueError:
                pass
        self.modules.setdefault(self.root_file, set()).add(())
        self.files = {}
        for r, ms in self.modules.items():
            for m in ms:
                self.files.setdefault(m, set()).add(r)
        self.module_of = {r: min(ms) for r, ms in self.modules.items()}
        self.file_of = {m: min(rs) for m, rs in self.files.items()}
        self._inline = {}


class Owner:
    """Definitions a type path names. `crates` maps each crate name to (root file, base dir, manifest dir);
    `lower` is the lower crate's name, read through `read` (the widener's current text)."""

    def __init__(self, lower, read, crates, upper_root, renamed=()):
        self.lower, self.renamed, self.upper_root = lower, set(renamed), str(Path(upper_root).resolve())
        self.roots = {lower: crates[lower]}
        self.names = {lower: lower}
        for name, spec in crates.items():
            if name != lower:
                self.names[name] = str(Path(spec[0]).resolve())
                self.roots[self.names[name]] = spec
        self.base = next((s[1] for n, s in crates.items() if n != lower), crates[lower][1])
        self.trees, self.reads, self._levels = {}, {lower: read}, {}

    def owned(self, path, name, side, src, hits):
        """([(rel, line)] of the one lower struct in `hits` the message path names, None), or ([], hold code)."""
        try:
            defs = self.definitions(re.sub(r"<.*", "", path).split("::")[:-1], name, side, src)
        except Hold as hold:
            return [], f"{hold.code} {hold.detail}".strip()
        if len(defs) != 1:
            return [], f"ambiguous {len(defs)} definitions"
        key, rel, index = next(iter(defs))
        if key != self.lower or (rel, index) not in hits:
            return [], f"not_lower {rel}:{index + 1}"
        return [(rel, index)], None

    def definitions(self, segs, name, side, src=None):
        if side not in ("lower", "upper"):
            raise Hold("unresolved_path", "no reporting side")
        if not segs:
            raise Hold("trimmed_path", name)
        local = self.lower if side == "lower" else self._key(src or self.upper_root)
        segs = tuple(segs[1:] if segs[0] == "crate" else segs)
        if not segs:
            return self._structs(local, (), name, ())
        first = self._child(local, (), segs[0], ())
        crate = self.names.get(segs[0]) if self.names.get(segs[0], local) != local else None
        if first and crate:
            raise Hold("module_crate_name", segs[0])
        if not first and not crate:
            raise Hold("trimmed_path", "::".join(segs))
        if first:
            return self._item(local, (), segs, name, ())
        return self._item(crate, (), segs[1:], name, ())

    def _key(self, root):
        key = str(Path(root).resolve())
        if key not in self.roots:
            self.roots[key] = (key, self.base, None)
        return key

    def _tree(self, key):
        if key not in self.trees:
            self.trees[key] = Tree(*self.roots[key])
            self.reads.setdefault(key, lambda rel, tree=self.trees[key]: vt.read_lines(tree.repo / rel))
        return self.trees[key]

    def _check(self, tree, rel, module):
        if rel in tree.broken:
            raise Hold("map_failed", rel)
        if len(tree.modules.get(rel, ())) > 1:
            raise Hold("multi_module_file", rel)
        if rel in tree.gated or len(tree.files.get(module, ())) > 1:
            raise Hold("cfg_module", f"{rel} ({'::'.join(module)})")

    def _locate(self, key, module):
        """(rel, first, last) of a module in its crate, None when it has none; Hold when uncertain."""
        tree = self._tree(key)
        if module in tree.files:
            self._check(tree, tree.file_of[module], module)
            return tree.file_of[module], 1, 10 ** 9
        for cut in range(len(module) - 1, -1, -1):
            rel = tree.file_of.get(module[:cut])
            if rel is None:
                continue
            self._check(tree, rel, module[:cut])
            tree.module_at(rel, 1)
            blocks = [(s, e) for s, e, names in tree._inline[rel] if module[:cut] + names == module]
            if not blocks:
                return None
            lines = self.reads[key](rel)
            above = lines[max(blocks[0][0] - 4, 0):blocks[0][0] - 1]
            if len(blocks) > 1 or any(line.strip().startswith("#[cfg") for line in above):
                raise Hold("cfg_module", f"{rel}:{blocks[0][0]}")
            return rel, blocks[0][0], blocks[0][1]
        return None

    def _lines_in(self, key, module):
        found = self._locate(key, module)
        if found is None:
            raise Hold("unresolved_path", f"no module {'::'.join(module)} in {key}")
        rel, first, last = found
        lines = self.reads[key](rel)
        if (key, rel) not in self._levels:
            self._levels[(key, rel)] = vt.item_level_lines(lines)
        level, tree = self._levels[(key, rel)], self._tree(key)
        return rel, lines, [j for j in range(max(first - 1, 0), min(last, len(lines)))
                            if j in level and tree.module_at(rel, j + 1) == module]

    def _uses(self, key, module, name):
        """([(path, original, absolute)] of `use` entries binding `name`, [(path, absolute)] of globs)."""
        _, lines, idx = self._lines_in(key, module)
        named, globs = [], []
        for j in idx:
            m = vt.ITEM.match(lines[j])
            if not (m and m.group("kw") == "use"):
                continue
            text = vt.use_text(lines, j)
            body = re.sub(r"^\s*(?:(?:" + vt.VIS + r")\s+)?use\s+", "", text)
            absolute = body.lstrip().startswith("::")
            entries = vt.use_entries(text)
            hit = [(p, o) for p, o, b in entries if b == name and o != "*"]
            glob = [p for p, o, _ in entries if o == "*"]
            if (hit or glob) and re.search(r"[{,]\s*::", body):
                raise Hold("absolute_path", f"{key}:{j + 1}")
            named += [(p, o, absolute) for p, o in hit]
            globs += [(p, absolute) for p in glob]
        return named, globs

    def _item(self, key, module, segs, name, stack):
        mods = {(key, module)}
        for seg in segs:
            nxt = set()
            for k, m in mods:
                hit = self._child(k, m, seg, stack)
                if not hit:
                    raise Hold("unresolved_path", f"{seg} under {'::'.join(m) or 'root'} in {k}")
                nxt |= hit
            mods = nxt
        out = set()
        for k, m in mods:
            found = self._structs(k, m, name, stack)
            if not found:
                raise Hold("unresolved_path", f"no struct {name} in {'::'.join(m) or 'root'}")
            out |= found
        return out

    def _child(self, key, module, seg, stack, globs=True):
        """{(crate key, module)} for the module `seg` inside `module`; empty only when it is certainly absent.
        A `use` path head skips globs: rustc rejects a head both a glob and a crate could name."""
        found = self._locate(key, module + (seg,))
        named, globbed = self._uses(key, module, seg)
        globs = globbed if globs else []
        if found is not None:
            if named:
                raise Hold("ambiguous", f"module and use both bind {seg}")
            return {(key, module + (seg,))}
        return self._through(key, module, seg, named, globs, stack, self._child)

    def _structs(self, key, module, name, stack):
        rel, lines, idx = self._lines_in(key, module)
        if any(re.match(OTHER.format(re.escape(name)), lines[j]) for j in idx):
            raise Hold("non_struct_item", f"{rel}: {name}")
        direct = {(key, rel, j) for j in idx if re.match(STRUCT.format(re.escape(name)), lines[j])}
        named, globs = self._uses(key, module, name)
        if direct:
            if named:
                raise Hold("ambiguous", f"struct and use both bind {name}")
            return direct
        return self._through(key, module, name, named, globs, stack, self._structs)

    def _through(self, key, module, name, named, globs, stack, step):
        """`name` behind the module's explicit `use` lines, else behind its one glob."""
        if not named and len(globs) > 1:
            raise Hold("glob_priority", f"{name} in {'::'.join(module) or 'root'} of {key}")
        out = set()
        for path, orig, absolute in named or [(p, name, a) for p, a in globs]:
            starts, deeper = self._use_target(key, module, list(path), absolute, stack)
            for k, m in starts:
                hit = step(k, m, orig, deeper)
                if named and not hit:
                    raise Hold("unresolved_path", f"use of {orig} in {key}")
                out |= hit
        return out

    def _use_target(self, key, module, path, absolute, stack):
        """({(crate key, module)} the module part of a `use` path names (2018 paths), the stack with it)."""
        entry = (key, module, tuple(path))
        if entry in stack:
            raise Hold("use_cycle", "::".join(path))
        if len(stack) >= HOPS:
            raise Hold("hops", "::".join(path))
        stack = stack + (entry,)
        if not path:
            return {(key, module)}, stack
        head, rest = path[0], path[1:]
        if absolute:
            if head not in self.names or head in self.renamed:
                raise Hold("absolute_path", f"::{head}")
            starts = {(self.names[head], ())}
        elif head in ("crate", "self", "super"):
            starts = {(key, () if head == "crate" else module if head == "self" else module[:-1])}
        else:
            starts = self._child(key, module, head, stack, globs=False)
            if starts and (head in self.names or head in self.renamed):
                raise Hold("module_crate_name", head)
            if not starts:
                if head in self.renamed:
                    raise Hold("renamed_dep", head)
                if head not in self.names:
                    raise Hold("unresolved_path", f"use {head} in {key}")
                starts = {(self.names[head], ())}
        for seg in rest:
            if seg == "super":
                starts = {(k, m[:-1]) for k, m in starts}
                continue
            nxt = set()
            for k, m in starts:
                hit = self._child(k, m, seg, stack)
                if not hit:
                    raise Hold("unresolved_path", f"use {'::'.join(path)} in {key}")
                nxt |= hit
            starts = nxt
        return starts, stack
