#!/usr/bin/env python3
"""Compiler-driven visibility widening for one crate cut, in shadow mode (#7353 VT1).

The partition (`partition.json`) lists, per future crate, the module groups it owns. For a target crate X,
X and every crate before it in `order` are "below"; every crate after X is "above". Before any real cut the
tool builds two throwaway copies of HEAD under `<repo>/target/crate_split_probe/<X>/`:

  lower  the whole lib renamed `probe_lower`; only the `mod` chain down to each below group becomes `pub`.
         Above modules still compile (privately), so a hard cut left in X does not break it.
  upper  the whole tree again, depending on `probe_lower`; each below leaf group's `mod x;` line becomes
         `pub use probe_lower::<path>::x;`, the parent re-export a member split will add.

Every round runs `cargo check --lib` on lower and `cargo check --lib --tests` on upper, reads the visibility
diagnostics (E0603 E0624 E0616 E0451 E0364 E0365, lower `private_interfaces`/`private_bounds`), and widens the
modifier on the definition line to `pub` in both copies. Nothing else is ever edited; type mismatches from an
uncut hard edge, test-only items, foreign impls and macro paths are classified and reported, not fixed.

stdout line 1 is `VISTOOL_OK ...` or `VISTOOL_FAIL <reason> ...` (exit 2). `--apply` writes the patch into the
worktree only on success; the manifest and the (partial) patch go to `--out-dir` and are never committed.
"""

import argparse
from collections import defaultdict
import difflib
import json
import os
from pathlib import Path
import re
import resource
import shutil
import subprocess
import sys
import time

VERSION = "vt1-1"
VIS_CODES = {"E0603", "E0624", "E0616", "E0451", "E0364", "E0365"}
# rustc reports a tuple-struct constructor hidden by restricted fields as E0423 (call) / E0532 (pattern).
CTOR_CODES = {"E0423", "E0532"}
VIS_LINTS = {"private_interfaces", "private_bounds"}
TEST_CFG_CODES = {"E0425", "E0412", "E0433", "E0599", "E0432", "E0422"}
IMPL_CODES = {"E0116", "E0117", "E0390"}
# Two copies of an above type (one compiled inside lower, one in upper) surface as these.
CUT_CODES = {"E0308", "E0277", "E0271", "E0631", "E0053", "E0369", "E0614", "E0605", "E0606", "E0282", "E0283"}
RESIDUES = ("cut_residue", "test_cfg", "impl_residue", "macro_path", "glob_hidden", "unclassified")
# Names a newly visible glob member would shadow from the std prelude (or the macro prelude).
PRELUDE = {"Result", "Option", "Error", "Box", "Vec", "String", "Ok", "Err", "Some", "None", "Clone", "Copy",
           "Send", "Sync", "Sized", "Unpin", "Drop", "Fn", "FnMut", "FnOnce", "Default", "Debug", "Eq",
           "PartialEq", "Ord", "PartialOrd", "Hash", "Iterator", "IntoIterator", "Extend", "ToOwned",
           "ToString", "From", "Into", "TryFrom", "TryInto", "AsRef", "AsMut", "FromIterator",
           "DoubleEndedIterator", "ExactSizeIterator", "drop", "format", "vec", "println", "panic",
           "assert", "assert_eq", "write", "writeln", "matches", "todo", "unreachable"}
VIS = r"pub\s*\(\s*(?:crate|super|self|in\s+[^)]*?)\s*\)|pub"
VIS_TOKEN = re.compile(r"\b(?:" + VIS + r")(?=[\s(]|$)")
ITEM_KW = r"fn|struct|enum|union|trait|type|const|static|mod|use|macro_rules!"
QUALS = r"(?:(?:async|const|unsafe|default|safe|extern(?:\s+\"[^\"]*\")?)\s+)*"
ITEM = re.compile(r"^(?P<indent>\s*)(?:(?P<vis>" + VIS + r")\s+)?(?P<quals>" + QUALS + r")(?P<kw>" + ITEM_KW + r")"
                  r"(?:\s+|$)(?P<rest>.*)$")
MOD_DECL = r"^(?P<indent>\s*)(?:(?P<vis>" + VIS + r")\s+)?mod\s+{name}\s*(?P<end>[;{{])"
NAME_IN_MSG = re.compile(r"`([^`]+)`")
SCOPE = {"cut_residue": "cut", "test_cfg": "T/M test-support", "impl_residue": "I", "macro_path": "macro",
         "glob_hidden": "glob", "unclassified": "?"}

TOOL_REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(TOOL_REPO / "scripts" / "ci"))
import rust_test_sources as rts  # noqa: E402


class VisFail(Exception):
    def __init__(self, reason, detail=""):
        super().__init__(f"{reason}: {detail}")
        self.reason, self.detail = reason, detail


# ---------------------------------------------------------------------------------------------- partition

def group_of(module):
    """Group key for a module path tuple: `config`, `services::tui_o`, `discord::inflight`, `discord::(mod.rs)`."""
    m = tuple(x for x in module if x != "crate")
    if not m:
        return "(root)"
    if m[0] == "services" and len(m) > 1:
        if m[1] == "discord":
            return "discord::" + (m[2] if len(m) > 2 else "(mod.rs)")
        return "services::" + m[1]
    return m[0]


def group_path(group):
    """Module path a group key names; `discord::X` lives under `services::discord`."""
    if group == "(root)":
        return ()
    head, _, tail = group.partition("::")
    if head == "discord":
        return ("services", "discord") if tail == "(mod.rs)" else ("services", "discord", tail)
    return (head, tail) if tail else (head,)


def load_partition(path):
    data = json.loads(Path(path).read_text("utf-8"))
    for key in ("base", "order", "crates"):
        if key not in data:
            raise VisFail("bad_partition", f"missing `{key}`")
    if sorted(data["order"]) != sorted(data["crates"]):
        raise VisFail("bad_partition", "order and crates disagree")
    seen = {}
    for crate in data["order"]:
        for group in data["crates"][crate]:
            if group in seen:
                raise VisFail("bad_partition", f"{group} in {seen[group]} and {crate}")
            seen[group] = crate
    return data


def inline_blocks(text):
    """[(first line, last line, names)] of inline `mod x { ... }` blocks, 1-based, outermost name first."""
    out, stack, depth = [], [], 0
    toks = rts.rust_tokens(text)
    for i, t in enumerate(toks):
        if t.kind != "punct":
            continue
        if t.value == "{":
            depth += 1
            if i >= 2 and toks[i - 2].value == "mod" and toks[i - 1].kind == "ident":
                stack.append((toks[i - 1].value, depth, toks[i - 2].line))
        elif t.value == "}":
            if stack and stack[-1][1] == depth:
                name, _, start = stack.pop()
                out.append((start, t.line, tuple(x[0] for x in stack) + (name,)))
            depth -= 1
    return out


def block_at(blocks, line):
    return max((b for b in blocks if b[0] <= line <= b[1]), key=lambda b: len(b[2]), default=(0, 0, ()))


class Layout:
    """Files and module paths of one source tree, split by partition crate."""

    def __init__(self, repo, partition, crate):
        if crate not in partition["crates"]:
            raise VisFail("bad_partition", f"unknown crate {crate}")
        self.repo = Path(repo).resolve()
        inv = rts.lib_inventory(self.repo)
        self.root_file = Path(next(iter(rts.workspace_lib_roots(self.repo).values()))[1]).relative_to(self.repo).as_posix()
        self.module_of, self.file_of, self.test_only = {}, {}, set()
        for path, modules in inv.modules.items():
            rel = Path(path).relative_to(self.repo).as_posix()
            module = min(tuple(x for x in m if x != "crate") for m in modules)
            self.module_of[rel] = module
            self.file_of.setdefault(module, rel)
            chains = inv.file_cfgs.get(path, set())
            if chains and all(any(rts._cfg_requires_test(p) for p in chain) for chain in chains):
                self.test_only.add(rel)
        self.file_of[()] = self.root_file
        self.module_of[self.root_file] = ()
        order = partition["order"]
        index = order.index(crate)
        self.owner = {g: c for c in order for g in partition["crates"][c]}
        self.below = {g for c in order[:index + 1] for g in partition["crates"][c]}
        self.crate, self.order = crate, order
        self.crate_of_file = {rel: self.owner.get(group_of(m)) for rel, m in self.module_of.items()}
        self.x_files = {rel for rel, c in self.crate_of_file.items() if c == crate}
        all_paths = [group_path(g) for g in self.owner]
        # A container group (discord::(mod.rs), services) also parents groups of other crates: chain only.
        self._inline = {}
        self.leaves = sorted(g for g in self.below if g != "(root)" and not any(
            len(p) > len(group_path(g)) and p[:len(group_path(g))] == group_path(g) for p in all_paths))

    def module_at(self, rel, line):
        """Module of a source line: the file's module plus any inline `mod x { }` blocks around it.
        A block's own `mod x {` line belongs to x."""
        if rel not in self._inline:
            try:
                self._inline[rel] = inline_blocks((self.repo / rel).read_text("utf-8"))
            except OSError:
                self._inline[rel] = []
        return self.module_of.get(rel, ()) + block_at(self._inline[rel], line)[2]

    def locate(self, module):
        """(file, first line, last line) holding a module: its own file, or an inline block of an ancestor's."""
        if module in self.file_of:
            return self.file_of[module], 1, 10 ** 9
        for cut in range(len(module) - 1, -1, -1):
            rel = self.file_of.get(module[:cut])
            if rel is None:
                continue
            self.module_at(rel, 1)
            for start, end, names in self._inline[rel]:
                if module[:cut] + names == module:
                    return rel, start, end
            return None
        return None

    def side_of(self, rel, line=None):
        """'x', 'prior' (below, not X), 'above', or None (file outside the partition) for a file or one line."""
        c = self.crate_of_file.get(rel) if line is None else self.owner.get(group_of(self.module_at(rel, line)))
        if c is None:
            return None
        if c == self.crate:
            return "x"
        return "prior" if self.order.index(c) < self.order.index(self.crate) else "above"


# ---------------------------------------------------------------------------------------------- probe

def git_head(repo):
    return subprocess.run(["git", "-C", str(repo), "rev-parse", "HEAD"], check=True, capture_output=True,
                          text=True).stdout.strip()


def extract_head(repo, dest):
    dest.mkdir(parents=True)
    archive = subprocess.Popen(["git", "-C", str(repo), "archive", "--format=tar", "HEAD"], stdout=subprocess.PIPE)
    tar = subprocess.run(["tar", "-x", "-C", str(dest)], stdin=archive.stdout, capture_output=True)
    archive.stdout.close()
    if archive.wait() or tar.returncode:
        raise VisFail("probe_extract", tar.stderr.decode(errors="replace")[-400:])


def decl_hits(lines, name, parent, file_module):
    """Indexes of `mod name;` / `mod name {` lines that declare parent::name (not a namesake in a nested block)."""
    rx = re.compile(MOD_DECL.format(name=re.escape(name)))
    blocks = inline_blocks("\n".join(lines))
    hits = []
    for i, line in enumerate(lines):
        m = rx.match(line)
        if not m:
            continue
        inner = block_at(blocks, i + 1)[2]
        if m.group("end") == "{":
            inner = inner[:-1]
        if file_module + inner == parent:
            hits.append(i)
    return hits


def attr_start(lines, index):
    """First line of the attribute/doc-comment block directly above lines[index]."""
    start = index
    while start > 0 and re.match(r"^\s*(#\[|///|//!)", lines[start - 1]):
        start -= 1
    return start


def toml_sections(text):
    """[(header or '', [lines])] keeping every original line."""
    out = [("", [])]
    for line in text.split("\n"):
        m = re.match(r"^\s*\[\[?([^\]]+)\]\]?\s*(#.*)?$", line)
        if m:
            out.append((line.strip(), [line]))
        else:
            out[-1][1].append(line)
    return out


def lower_manifest(text, lib_path):
    out, has_lib = [], False
    for header, lines in toml_sections(text):
        if header.startswith("[[bin]]"):
            continue
        if header == "[package]":
            lines = [re.sub(r'^(\s*name\s*=\s*)"[^"]*"', r'\1"probe_lower"', l) for l in lines]
        if header == "[lib]":
            has_lib = True
            lines = [l for l in lines if not re.match(r"^\s*(path|name)\s*=", l)]
            lines.insert(1, f'path = "{lib_path}"')
        out.extend(lines)
    if not has_lib:
        out += ["", "[lib]", f'path = "{lib_path}"']
    return "\n".join(out)


def upper_manifest(text):
    out, sections = [], toml_sections(text)
    if not any(header == "[dependencies]" for header, _ in sections):
        sections.append(("[dependencies]", ["[dependencies]"]))
    for header, lines in sections:
        if header == "[workspace]":
            lines = [re.sub(r"^(\s*members\s*=\s*)\[.*\]", r"\1[]", l) for l in lines]
        if "dependencies" in header:
            # Share lower's workspace path crates so upper and lower agree on one copy of their types.
            lines = [re.sub(r'(\bpath\s*=\s*")(?!\.\./|/)([^"]+)"', r'\1../lower/\2"', l) for l in lines]
        if header == "[dependencies]":
            lines.insert(1, 'probe_lower = { path = "../lower" }')
        out.extend(lines)
    return "\n".join(out)


def read_lines(path):
    return path.read_text("utf-8").split("\n")


def write_lines(path, lines):
    path.write_text("\n".join(lines), "utf-8")


def transform_lower(root, layout):
    lib_rel = str(Path(layout.root_file).parent / "probe_lower_lib.rs")
    manifest = root / "Cargo.toml"
    manifest.write_text(lower_manifest(manifest.read_text("utf-8"), lib_rel), "utf-8")
    shutil.copyfile(root / layout.root_file, root / lib_rel)
    remap = {layout.root_file: lib_rel}
    for group in sorted(layout.below):
        path = group_path(group)
        for depth in range(1, len(path) + 1):
            parent = layout.file_of.get(path[:depth - 1])
            if parent is None:
                raise VisFail("lower_probe_broken", f"no file for module {'::'.join(path[:depth - 1])}")
            target = root / remap.get(parent, parent)
            lines = read_lines(target)
            hits = decl_hits(lines, path[depth - 1], path[:depth - 1], layout.module_of.get(parent, ()))
            if not hits:
                raise VisFail("lower_probe_broken", f"no `mod {path[depth - 1]}` in {parent}")
            rx = re.compile(MOD_DECL.format(name=re.escape(path[depth - 1])))
            for i in hits:
                m = rx.match(lines[i])
                cut = m.end("vis") if m.group("vis") else m.end("indent")
                lines[i] = m.group("indent") + "pub " + lines[i][cut:].lstrip()
            write_lines(target, lines)
    return lib_rel


def transform_upper(root, layout):
    manifest = root / "Cargo.toml"
    manifest.write_text(upper_manifest(manifest.read_text("utf-8")), "utf-8")
    rewired = []
    for group in layout.leaves:
        path = group_path(group)
        parent = layout.file_of.get(path[:-1])
        target = root / parent
        lines = read_lines(target)
        rx = re.compile(MOD_DECL.format(name=re.escape(path[-1])))
        hits = decl_hits(lines, path[-1], path[:-1], layout.module_of.get(parent, ()))
        if not hits:
            raise VisFail("lower_probe_broken", f"no `mod {path[-1]}` in {parent}")
        for i in reversed(hits):
            start = attr_start(lines, i)
            attrs = lines[start:i]
            indent = rx.match(lines[i]).group("indent")
            # An inline `mod x { ... }` (child files under it) goes as a whole block.
            close = i if rx.match(lines[i]).group("end") == ";" else \
                next(b[1] - 1 for b in inline_blocks("\n".join(lines)) if b[0] == i + 1 and b[2][-1] == path[-1])
            if any(re.search(r"cfg\s*\(.*\btest\b", a) for a in attrs):
                # Lower's own test module: never checked by the probe (T slice), keep an empty stand-in.
                lines[i:close + 1] = [f"{indent}mod {path[-1]} {{}}"]
                continue
            kept = [a for a in attrs if not re.match(r"^\s*#\[\s*(path|macro_use)\b", a)]
            lines[start:close + 1] = kept + [f"{indent}pub use probe_lower::{'::'.join(path)};"]
        write_lines(target, lines)
        rewired.append(f"{parent}:{path[-1]}")
    return rewired


# ---------------------------------------------------------------------------------------------- cargo

def run_check(cargo, cwd, args, target_dir, own_root):
    """Deduplicated compiler messages of the package rooted at own_root."""
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir))
    started = time.monotonic()
    proc = subprocess.run([cargo, "check", *args, "--message-format=json"], cwd=cwd, env=env,
                          capture_output=True, text=True)
    elapsed = round(time.monotonic() - started, 1)
    out, seen = [], set()
    prefix = str(own_root.resolve()) + os.sep
    for line in proc.stdout.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get("reason") != "compiler-message":
            continue
        src = str(Path(msg.get("target", {}).get("src_path", "")).resolve())
        if not src.startswith(prefix):
            continue
        diag = msg["message"]
        diag["_test"] = bool(msg.get("target", {}).get("test")) or msg.get("profile", {}).get("test", False)
        primary = next((s for s in diag.get("spans", []) if s.get("is_primary")), None)
        key = (diag.get("level"), (diag.get("code") or {}).get("code"), diag.get("message"),
               primary and (primary["file_name"], primary["line_start"], primary["column_start"]))
        if key in seen:
            if diag["_test"]:
                continue
            out = [d for d in out if d["_key"] != key]
        seen.add(key)
        diag["_key"], diag["_root"] = key, str(cwd)
        out.append(diag)
    errors = sum(d.get("level") == "error" for d in out)
    if proc.returncode and not errors:
        raise VisFail("cargo_failed", f"{cwd.name}: " + proc.stderr.strip()[-600:])
    return out, elapsed


# ---------------------------------------------------------------------------------------------- diagnostics

class Resolver:
    """Maps rustc file names of either probe copy back to repo-relative paths."""

    def __init__(self, lower, upper, lib_rel, root_file):
        self.roots = [(lower.resolve(), "lower"), (upper.resolve(), "upper")]
        self.lib_rel, self.root_file = lib_rel, root_file

    def rel(self, file_name, run_root):
        p = Path(file_name)
        p = (p if p.is_absolute() else run_root / p).resolve()
        for root, side in self.roots:
            if p.is_relative_to(root):
                rel = p.relative_to(root).as_posix()
                return side, (self.root_file if side == "lower" and rel == self.lib_rel else rel)
        return None, p.as_posix()


def code_of(diag):
    return (diag.get("code") or {}).get("code") or ""


def primary_span(diag):
    return next((s for s in diag.get("spans", []) if s.get("is_primary")), (diag.get("spans") or [None])[0])


def is_vis(diag):
    code = code_of(diag)
    return code in VIS_CODES or (code in CTOR_CODES and "private fields" in diag.get("message", ""))


DEF_MARKS = ("defined here", "is only usable at visibility", "constructor is private if any of the fields",
             "constructor is not visible here due to private fields")


def definition_span(diag, marks=DEF_MARKS):
    """Where the restricted item is declared: the struct behind a tuple constructor first, then the
    `... defined here` note (or label), then a lint's `... is only usable at visibility` note."""
    spans = [s for s in diag.get("spans", []) if not s.get("is_primary")]
    notes = [(c.get("message", ""), sp) for c in diag.get("children", []) for sp in c.get("spans", [])]
    for mark in [m for m in DEF_MARKS[2:] if m in marks]:
        for span in spans:
            if mark in (span.get("label") or ""):
                return span
        for msg, span in notes:
            if mark in msg:
                return span
    for mark in [m for m in DEF_MARKS[:2] if m in marks]:
        for msg, span in notes:
            if mark in msg:
                return span
        for span in spans:
            if mark in (span.get("label") or ""):
                return span
    return None


def message_target(diag):
    """(kind, name, owner) parsed from the message: kind in item/method/field/ctor/reexport/interface."""
    code, msg = code_of(diag), diag.get("message", "")
    # `session_probe::SessionProbe`, `Foo<T>`: the declaration carries the last segment only.
    names = [re.sub(r"<.*", "", n).split("::")[-1] for n in NAME_IN_MSG.findall(msg)]
    if code == "E0616" or code == "E0451":
        owner = names[1] if len(names) > 1 else ""
        return "field", names[0] if names else "", owner
    if code in CTOR_CODES:
        return "ctor", "", ""
    if code == "E0624":
        return "item", names[0] if names else "", ""
    if code == "E0603" and "tuple struct constructor" in msg:
        return "ctor", names[0] if names else "", ""
    if code in ("E0364", "E0365"):
        return "item", names[0] if names else "", ""
    if code in VIS_LINTS:
        return "item", names[0] if names else "", ""
    return "item", names[0] if names else "", ""


def find_item_line(lines, start, end, name, kind):
    """Index of the line declaring `name` near [start, end] (0-based, inclusive)."""
    name_rx = re.compile(r"\b" + re.escape(name) + r"\b")
    for i in range(max(start, 0), min(end, len(lines) - 1) + 1):
        m = ITEM.match(lines[i])
        if not m:
            continue
        if m.group("kw") == "use":
            return i
        if re.match(r"(?:mut\s+)?" + re.escape(name) + r"\b", m.group("rest")):
            return i
    # A name inside a multi-line `use` tree: walk back to the line that opens the `use`.
    if 0 <= start < len(lines) and name_rx.search(lines[start]):
        for i in range(start, max(start - 60, -1), -1):
            m = ITEM.match(lines[i])
            if m and m.group("kw") == "use":
                return i
            if i < start and lines[i].rstrip().endswith((";", "}")) and not m:
                break
    return None


def widen_line(line):
    """`pub` for the item on this line; None if it is already `pub` or not an item."""
    m = ITEM.match(line)
    if not m or m.group("kw") == "macro_rules!":
        return None
    if m.group("vis") == "pub":
        return None
    body = line[m.start("quals"):]
    return m.group("indent") + "pub " + body


def split_fields(text):
    parts, depth, cur = [], 0, ""
    for ch in text:
        if ch in "<([{":
            depth += 1
        elif ch in ">)]}":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        parts.append(cur)
    return parts


def widen_tuple_fields(line, name=None, only=None, probe=False):
    """Widen the fields of a one-line `struct Name(...)`; `only` restricts to one field index.
    probe=True only answers whether the line holds such a struct."""
    m = re.search(r"\bstruct\s+" + (re.escape(name) if name else r"\w+") + r"\b(?:<[^(]*>)?\s*\(", line)
    if not m:
        return None
    depth, close = 1, None
    for i in range(m.end(), len(line)):
        depth += {"(": 1, ")": -1}.get(line[i], 0)
        if depth == 0:
            close = i
            break
    if close is None:
        return None
    if probe:
        return True
    fields = split_fields(line[m.end():close])
    new = []
    for index, field in enumerate(fields):
        lead = field[:len(field) - len(field.lstrip())]
        body = field.strip()
        if only is None or index == only:
            vis = re.match(r"(?:" + VIS + r")(?=[\s(])", body)
            body = "pub " + body[vis.end():].lstrip() if vis else "pub " + body
        new.append(lead + body)
    out = line[:m.end()] + ",".join(new) + line[close:]
    return out if out != line else None


def struct_body_field(lines, struct_line, field):
    """Index of `field:` inside the braced body that starts at struct_line."""
    depth, opened = 0, False
    rx = re.compile(r"^\s*(?:(?:" + VIS + r")\s+)?(?:r#)?" + re.escape(field) + r"\s*:")
    for i in range(struct_line, min(len(lines), struct_line + 400)):
        if opened and depth == 1 and rx.match(lines[i]):
            return i
        for ch in lines[i]:
            if ch == "{":
                depth, opened = depth + 1, True
            elif ch == "}":
                depth -= 1
        if opened and depth <= 0:
            return None
        if not opened and lines[i].rstrip().endswith(";"):
            return None
    return None


def widen_field(line):
    vis = re.match(r"^(\s*)(?:(" + VIS + r")\s+)?(?=(?:r#)?\w+\s*:)", line)
    if not vis or vis.group(2) == "pub":
        return None
    return vis.group(1) + "pub " + line[vis.end():]


def apply_op(line, op):
    """One widening on one line; ops on the same line commute (prefix vs. tuple fields vs. field)."""
    if op[0] == "item":
        new = widen_line(line)
    elif op[0] == "field":
        new = widen_field(line)
    else:
        new = widen_tuple_fields(line, op[1] or None, None if op[2] < 0 else op[2])
    return new if new is not None else line


def apply_ops(line, ops):
    for op in sorted(ops):
        line = apply_op(line, op)
    return line


class Widener:
    """Current text of every lower-side file the run edited.

    resolve() answers {(rel, line index): op} computed against the current text; a round collects every
    op first and applies them together, so diagnostic order cannot change the result."""

    def __init__(self, layout, lower, upper):
        self.layout, self.lower, self.upper = layout, lower, upper
        self.text = {}
        self.original = {}

    def lines(self, rel):
        if rel not in self.text:
            self.text[rel] = read_lines(self.lower / rel)
            self.original[rel] = list(self.text[rel])
        return self.text[rel]

    def unique_struct(self, name):
        rx = re.compile(r"^\s*(?:(?:" + VIS + r")\s+)?struct\s+" + re.escape(name) + r"\b")
        hits = []
        for rel in sorted(self.layout.x_files):
            for i, line in enumerate(self.lines(rel)):
                if rx.match(line):
                    hits.append((rel, i))
        return hits

    def _edit(self, rel, index, op, what):
        if apply_op(self.lines(rel)[index], op) == self.lines(rel)[index]:
            return {}, ("unresolved", f"{what} already pub: {rel}:{index + 1}")
        return {(rel, index): op}, "edit"

    def _place(self, rel, line):
        where = self.layout.side_of(rel, line)
        if where == "above":
            return ("cut_residue", f"definition in above file {rel}")
        if where != "x":
            return ("outside_crate", f"{rel} ({where or 'not in partition'})")
        return None

    def resolve(self, diag, resolver, run_root):
        """(edits {(rel, idx): op}, outcome) for one visibility diagnostic.

        outcome: 'edit' | (residue class | 'outside_crate' | 'unresolved', why)."""
        kind, name, owner = message_target(diag)
        if code_of(diag) in ("E0364", "E0365"):
            return self._reexport(diag, name, resolver, run_root)
        span = definition_span(diag)
        if span is None and kind == "field":
            hits = self.unique_struct(owner)
            if len(hits) != 1:
                return {}, ("unresolved", f"struct `{owner}` defined {len(hits)} times in {self.layout.crate}")
            return self._field(*hits[0], name, owner)
        if span is None:
            return {}, ("unresolved", "no definition span")
        side, rel = resolver.rel(span["file_name"], run_root)
        if side == "upper":
            # Declared in the upper copy itself: a method of an upper `impl` on a lower type, or an above type.
            return {}, ("impl_residue" if code_of(diag) == "E0624" else "cut_residue", f"upper {rel}")
        if self._place(rel, span["line_start"]):
            return {}, self._place(rel, span["line_start"])
        lines = self.lines(rel)
        start, end = span["line_start"] - 1, span["line_end"] - 1
        if kind == "field":
            sline = find_item_line(lines, start, end, owner, "item")
            if sline is None:
                return {}, ("unresolved", f"struct `{owner}` not at {rel}:{start + 1}")
            return self._field(rel, sline, name, owner)
        if kind == "ctor":
            tuple_line = next((i for i in range(start, end + 1)
                               if widen_tuple_fields(lines[i], name or None, probe=True)), None)
            if tuple_line is None:
                return {}, ("unresolved", f"tuple struct `{name}` fields not on one line at {rel}:{start + 1}")
            op = ("tuple", name, -1)
            if apply_op(lines[tuple_line], op) != lines[tuple_line]:
                return {(rel, tuple_line): op}, "edit"
            # Fields are already `pub`: the constructor is hidden by the path it is named through.
            named = definition_span(diag, DEF_MARKS[:1])
            if named is None:
                return {}, ("unresolved", f"tuple struct `{name}` fields already pub, no path span")
            side, rel = resolver.rel(named["file_name"], run_root)
            if self._place(rel, named["line_start"]):
                return {}, self._place(rel, named["line_start"])
            lines, start, end = self.lines(rel), named["line_start"] - 1, named["line_end"] - 1
        index = find_item_line(lines, start, end, name, kind)
        if index is None:
            return {}, ("unresolved", f"no item `{name}` at {rel}:{start + 1}")
        if ITEM.match(lines[index]).group("kw") == "macro_rules!":
            return {}, ("macro_path", f"macro_rules! {name} at {rel}:{index + 1}")
        if widen_line(lines[index]) is None and ITEM.match(lines[index]).group("kw") == "use":
            # An already-`pub` re-export (often `pub use inner::*;`): the blocker is behind it.
            return self._through_use(rel, index, name)
        return self._edit(rel, index, ("item",), name)

    def _reexport(self, diag, name, resolver, run_root):
        """E0364/E0365 point at the `pub use` path, not the item: resolve the path through the module map."""
        span = primary_span(diag)
        side, rel = resolver.rel(span["file_name"], run_root)
        if self._place(rel, span["line_start"]):
            return {}, self._place(rel, span["line_start"])
        i = span["line_start"] - 1
        start = find_item_line(self.lines(rel), i, i, name, "item")
        return self._through_use(rel, i if start is None else start, name)

    def _through_use(self, rel, index, name, hops=6):
        """Ops for the declarations a `use` at rel:index binds `name` to, by explicit path or by glob;
        an already-`pub` re-export on the way is followed (at most `hops` deep)."""
        text = " ".join(self.lines(rel)[index:index + 40]).split(";")[0]
        base = self.layout.module_at(rel, index + 1)
        m = re.search(r"((?:[A-Za-z_]\w*::)+)" + re.escape(name) + r"\b", text) or \
            re.search(r"((?:[A-Za-z_]\w*::)+)\{[^}]*\b" + re.escape(name) + r"\b", text)
        prefixes = [m.group(1)] if m else [g.group(1) for g in re.finditer(r"((?:[A-Za-z_]\w*::)+)\*", text)]
        edits, outcome = {}, ("unresolved", f"no path to `{name}` at {rel}:{index + 1}")
        for prefix in prefixes:
            segs = [x for x in re.sub(r"^.*\buse\s+", "", prefix).split("::") if x]
            module = () if segs and segs[0] == "crate" else base
            for seg in segs[1:] if segs and segs[0] == "crate" else segs:
                module = module[:-1] if seg == "super" else module if seg == "self" else module + (seg,)
            found = self.layout.locate(module)
            if found is None:
                continue
            target, first, last = found
            tl = self.lines(target)
            hits = [j for j in range(max(first - 1, 0), min(last, len(tl)))
                    if find_item_line(tl, j, j, name, "item") == j and self.layout.module_at(target, j + 1) == module
                    and (ITEM.match(tl[j]).group("kw") != "use" or name in use_leaf_names(" ".join(tl[j:j + 40]))
                         or re.search(r"::\*", tl[j]))]
            for j in hits:
                if self._place(target, j + 1):
                    outcome = self._place(target, j + 1)
                elif widen_line(tl[j]):
                    edits[(target, j)] = ("item",)
                elif ITEM.match(tl[j]).group("kw") == "use" and hops and (target, j) != (rel, index):
                    sub, out = self._through_use(target, j, name, hops - 1)
                    edits.update(sub)
                    outcome = outcome if out == "edit" else out
            if hits and not edits and outcome[0] == "unresolved":
                outcome = ("unresolved", f"`{name}` in {target} is already pub")
        return (edits, "edit") if edits else ({}, outcome)

    def _field(self, rel, sline, field, owner):
        lines = self.lines(rel)
        if field.isdigit():
            if not widen_tuple_fields(lines[sline], owner, probe=True):
                return {}, ("unresolved", f"tuple field {owner}.{field} not on one line at {rel}:{sline + 1}")
            return self._edit(rel, sline, ("tuple", owner, int(field)), f"{owner}.{field}")
        index = struct_body_field(lines, sline, field)
        if index is None:
            return {}, ("unresolved", f"field {owner}.{field} not found from {rel}:{sline + 1}")
        return self._edit(rel, index, ("field",), f"{owner}.{field}")

    def apply(self, ops):
        """Apply {(rel, idx): {op, ...}} to the text and both probe copies; returns {(rel, idx): new line}."""
        out, touched = {}, set()
        for (rel, index), group in sorted(ops.items()):
            lines = self.lines(rel)
            new = apply_ops(lines[index], group)
            if new != lines[index]:
                lines[index] = new
                out[(rel, index)] = new
                touched.add(rel)
        for rel in sorted(touched):
            for root in (self.lower, self.upper):
                write_lines(root / rel, self.lines(rel))
        return out


def restricted_defs(layout, read):
    """Name -> [test_gated] for X items declared with a restricted or no visibility."""
    rx = re.compile(r"^\s*(?:(pub\s*\([^)]*\))\s+)?" + QUALS + r"(?:fn|struct|enum|union|trait|type|const|static|mod)\s+(\w+)")
    out = defaultdict(list)
    for rel in sorted(layout.x_files):
        lines = read(rel)
        for i, line in enumerate(lines):
            m = rx.match(line)
            if not m:
                continue
            gated = rel in layout.test_only or any("cfg(test)" in lines[j].replace(" ", "")
                                                   for j in range(attr_start(lines, i), i))
            out[m.group(2)].append(gated)
        for i, line in enumerate(lines):
            mm = re.match(r"^\s*macro_rules!\s+(\w+)", line)
            if mm and any("macro_export" in lines[j] for j in range(attr_start(lines, i), i)):
                out[mm.group(1)].append("macro")
    return out


def classify_residue(diag, restricted):
    code, msg = code_of(diag), diag.get("message", "")
    rendered = diag.get("rendered") or ""
    names = [n.split("::")[-1] for n in NAME_IN_MSG.findall(msg)]
    if "$crate" in rendered or "macro" in msg or any("macro" in restricted.get(n, []) for n in names):
        return "macro_path"
    if code in IMPL_CODES:
        return "impl_residue"
    if code == "E0408":
        # `use Enum::*` skipped restricted variants, so a pattern binds the variant name as a variable.
        return "glob_hidden"
    if code in TEST_CFG_CODES and names and any(g is False for g in restricted.get(names[0], [])):
        # A glob import silently skips a restricted item, so the use site reports "not found".
        return "glob_hidden"
    if code in TEST_CFG_CODES and diag.get("_test"):
        return "test_cfg"
    if code in CUT_CODES:
        return "cut_residue"
    return "unclassified"


def where(diag, resolver):
    span = primary_span(diag)
    if not span:
        return "?"
    side, rel = resolver.rel(span["file_name"], Path(diag["_root"]))
    return f"{side}:{rel}:{span['line_start']}"


# ---------------------------------------------------------------------------------------------- safety

def strip_vis(line):
    return re.sub(r"\s+", " ", VIS_TOKEN.sub("", line)).strip()


def self_classify(pairs):
    """(b): every changed line pair is equal once visibility modifiers are removed."""
    return [(rel, n, before, after) for rel, n, before, after in pairs if strip_vis(before) != strip_vis(after)]


def vis_scope(vis, module):
    """Module subtree that could already see an item of `module` with this modifier; None = whole crate."""
    if vis is None or re.fullmatch(r"pub\s*\(\s*self\s*\)", vis):
        return module
    if re.fullmatch(r"pub\s*\(\s*super\s*\)", vis):
        return module[:-1]
    m = re.fullmatch(r"pub\s*\(\s*in\s+([^)]*?)\s*\)", vis)
    if m:
        segs = [s for s in m.group(1).split("::") if s]
        cur = () if segs and segs[0] == "crate" else module
        for s in segs[1:] if segs and segs[0] == "crate" else segs:
            cur = cur[:-1] if s == "super" else cur if s == "self" else cur + (s,)
        return cur
    return None  # pub(crate): (c) already crate-wide, nothing new for a glob


def use_leaf_names(text):
    """Names a `use` item binds (alias wins), ignoring globs and `self`."""
    body = re.sub(r"^\s*(?:(?:" + VIS + r")\s+)?use\s+", "", text).split(";")[0]
    names = []
    for part in re.split(r"[{},]", body):
        part = part.strip()
        if not part or part.endswith("*"):
            continue
        alias = re.search(r"\bas\s+(\w+)$", part)
        leaf = alias.group(1) if alias else part.split("::")[-1].strip()
        if leaf and leaf != "self" and re.fullmatch(r"\w+", leaf):
            names.append(leaf)
    return names


def edit_bindings(before, lines, index):
    """[(name, kind)] a widened declaration line introduces."""
    m = ITEM.match(before)
    if not m:
        field = re.match(r"^\s*(?:(?:" + VIS + r")\s+)?(\w+)\s*:", before)
        return [(field.group(1), "field")] if field else []
    kw = m.group("kw")
    if kw == "use":
        text = before
        j = index
        while ";" not in text and j + 1 < len(lines):
            j += 1
            text += " " + lines[j]
        return [(n, "use") for n in use_leaf_names(text)]
    name = re.match(r"(?:mut\s+)?(\w+)", m.group("rest"))
    return [(name.group(1), kw)] if name else []


def glob_imports(layout, read):
    """[(importer module, importer rel, line, target module)] for every `path::*` inside a `use`."""
    modules = set(layout.file_of)
    out = []
    for rel in sorted(layout.module_of):
        try:
            lines = read(rel)
        except OSError:
            continue
        for i, line in enumerate(lines):
            if not re.search(r"\buse\b", line) and "::*" not in line:
                continue
            for m in re.finditer(r"((?:[A-Za-z_]\w*::)+)\*", line):
                segs = [s for s in m.group(1).split("::") if s]
                importer = cur = layout.module_at(rel, i + 1)
                if segs[0] == "crate":
                    cur, segs = (), segs[1:]
                elif segs[0] in ("self", "super"):
                    pass
                elif importer + (segs[0],) not in modules and layout.locate(importer + (segs[0],)) is None:
                    continue
                for s in segs:
                    cur = cur[:-1] if s == "super" else cur if s == "self" else cur + (s,)
                out.append((importer, rel, i + 1, cur))
    return out


def glob_check(edits, layout, read):
    """(a): newly visible names for glob importers outside the old visibility scope."""
    imports = glob_imports(layout, read)
    risks = []
    for e in edits:
        vis = ITEM.match(e["before"])
        vis = vis.group("vis") if vis else re.match(r"^\s*(?:(" + VIS + r")\s+)?", e["before"]).group(1)
        module = layout.module_at(e["file"], e["line"])
        scope = vis_scope(vis, module)
        if scope is None or e.get("binding") == "field":
            continue
        for name, kind in e["bindings"]:
            if kind == "field":
                continue
            for importer, irel, iline, target in imports:
                if target != module or importer[:len(scope)] == scope:
                    continue
                fail = name in PRELUDE or kind == "trait"
                risks.append({"file": e["file"], "line": e["line"], "name": name, "kind": kind,
                              "importer": f"{irel}:{iline}", "fail": fail})
    return risks


def equiv_verdict(repo, base):
    """Visibility-only verdict from KR-C1's equiv_check.py; this tool never re-implements it."""
    script = TOOL_REPO / "scripts" / "crate_split" / "equiv_check.py"
    if not script.is_file():
        return "판정 보류 (equiv_check.py 없음)"
    proc = subprocess.run([sys.executable, str(script), "--base", base, "--head", "HEAD"], cwd=repo,
                          capture_output=True, text=True)
    first = (proc.stdout.strip().splitlines() or [""])[0]
    return f"exit={proc.returncode} {first}"


# ---------------------------------------------------------------------------------------------- run

def unified_patch(original, final):
    """git-apply-able diff; lines are held split on "\n", so a trailing newline is an empty last element."""
    chunks = []
    for rel in sorted(final):
        if original[rel] == final[rel]:
            continue
        a, b = ("\n".join(original[rel])).splitlines(True), ("\n".join(final[rel])).splitlines(True)
        for line in difflib.unified_diff(a, b, f"a/{rel}", f"b/{rel}", n=3):
            chunks.append(line if line.endswith("\n") else line + "\n\\ No newline at end of file\n")
    return "".join(chunks)


def changed_pairs(original, final):
    pairs = []
    for rel in sorted(final):
        for n, (a, b) in enumerate(zip(original[rel], final[rel]), 1):
            if a != b:
                pairs.append((rel, n, a, b))
    return pairs


def vistool(repo, partition_path, crate, *, out_dir, max_iters=10, apply=False, keep_probe=False,
            cargo="cargo", log=print):
    repo = Path(repo).resolve()
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    partition = load_partition(partition_path)
    head = git_head(repo)
    manifest = {"tool": VERSION, "crate": crate, "head": head, "partition_base": partition["base"],
                "rounds": [], "edits": [], "unresolved": [], "residue": {}, "glob_risk": []}
    probe_root = repo / "target" / "crate_split_probe"
    probe, target_dir = probe_root / crate, probe_root / "target"
    lower, upper = probe / "lower", probe / "upper"
    status, reason, widener, iters = "fail", None, None, 0
    try:
        layout = Layout(repo, partition, crate)
        manifest["x_files"] = len(layout.x_files)
        if probe.exists():
            shutil.rmtree(probe)
        extract_head(repo, lower)
        extract_head(repo, upper)
        lib_rel = transform_lower(lower, layout)
        manifest["upper_rewired"] = transform_upper(upper, layout)
        resolver = Resolver(lower, upper, lib_rel, layout.root_file)
        widener = Widener(layout, lower, upper)
        restricted = restricted_defs(layout, widener.lines)
        while True:
            iters += 1
            low, low_s = run_check(cargo, lower, ["--lib"], target_dir, lower)
            up, up_s = [], 0.0
            if not any(d.get("level") == "error" for d in low):
                # Upper cannot build on a broken lower; its round waits for lower's re-export fixes.
                up, up_s = run_check(cargo, upper, ["--lib", "--tests"], target_dir, upper)
            (out_dir / f"vistool-{crate}-diags-{iters}.json").write_text(
                json.dumps({"lower": low, "upper": up}, indent=0, ensure_ascii=False), "utf-8")
            vis, residue, low_err = [], defaultdict(list), []
            for d in low:
                code = code_of(d)
                if is_vis(d) or (code in VIS_LINTS and d.get("level") == "warning"):
                    vis.append((d, lower))
                elif d.get("level") == "error":
                    low_err.append(d)
            for d in up:
                if d.get("level") != "error":
                    continue
                if is_vis(d):
                    vis.append((d, upper))
                else:
                    residue[classify_residue(d, restricted)].append(d)
            edits, unresolved, outside = {}, [], []
            for d, run_root in vis:
                new, outcome = widener.resolve(d, resolver, run_root)
                if outcome == "edit":
                    for key, op in new.items():
                        entry = edits.setdefault(key, [set(), set(), set()])
                        entry[0].add(op)
                        entry[1].add(code_of(d))
                        entry[2].add(where(d, resolver))
                    continue
                kind, why = outcome
                if kind in ("cut_residue", "macro_path", "impl_residue"):
                    residue[kind].append(d)
                elif kind == "outside_crate":
                    outside.append(f"{code_of(d)} {why} <- {where(d, resolver)}")
                else:
                    unresolved.append(f"{code_of(d)} {d.get('message')} :: {why} <- {where(d, resolver)}")
            vis_count = sum(1 for d, _ in vis)
            # rustc skips its field-privacy pass (E0451) while any other error stands: say so, never hide it.
            manifest["privacy_pass_masked"] = any(d.get("level") == "error" for d in up) or not up_s
            manifest["rounds"].append({"iter": iters, "lower_errors": len(low_err), "vis": vis_count,
                                       "edits": len(edits), "unresolved": len(unresolved),
                                       "residue": {k: len(v) for k, v in sorted(residue.items())},
                                       "lower_secs": low_s, "upper_secs": up_s,
                                       # Largest finished descendant (rustc) so far, KiB on Linux.
                                       "max_child_rss_kib": resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss})
            log(f"vistool: iter={iters} lower_err={len(low_err)} vis={vis_count} edits={len(edits)} "
                f"unresolved={len(unresolved)} lower={low_s}s upper={up_s}s", file=sys.stderr)
            manifest["unresolved"] = unresolved
            if up_s or not manifest["residue"]:
                # Residue comes from the last round upper ran in; a lower-only round must not erase it.
                manifest["residue"] = {k: {"count": len(residue.get(k, [])), "scope": SCOPE[k],
                                           "first": [f"{code_of(d)} {where(d, resolver)} {d.get('message')}"
                                                     for d in residue.get(k, [])[:20]]} for k in RESIDUES}
            if low_err:
                manifest["lower_errors"] = [f"{code_of(d)} {where(d, resolver)} {d.get('message')}"
                                            for d in low_err[:20]]
                raise VisFail("lower_probe_broken" if iters == 1 and not manifest["edits"] else "lower_broken",
                              manifest["lower_errors"][0])
            if outside:
                manifest["outside"] = outside[:50]
                raise VisFail("outside_crate", outside[0])
            if not edits and not unresolved:
                # Every visibility diagnostic left is residue a later slice owns (cut, impl, macro).
                if residue.get("unclassified"):
                    raise VisFail("unclassified_residue", manifest["residue"]["unclassified"]["first"][0])
                break
            if not edits:
                raise VisFail("no_progress", unresolved[0])
            applied = widener.apply({k: v[0] for k, v in edits.items()})
            for (rel, index), line in sorted(applied.items()):
                _, codes, sites = edits[(rel, index)]
                manifest["edits"].append({"iter": iters, "file": rel, "line": index + 1,
                                          "before": widener.original[rel][index], "after": line,
                                          "code": ",".join(sorted(codes)), "sites": sorted(sites)[:5]})
            if not applied:
                raise VisFail("no_progress", (unresolved or ["edits changed nothing"])[0])
            if iters >= max_iters:
                raise VisFail("max_iters", f"{vis_count} visibility diagnostics in the last round")
        status = "ok"
    except VisFail as exc:
        reason = exc
        manifest["fail"] = {"reason": exc.reason, "detail": exc.detail}
    finally:
        final = dict(widener.text) if widener else {}
        original = dict(widener.original) if widener else {}
        if not keep_probe and probe_root.exists():
            if probe.exists():
                shutil.rmtree(probe)
            if target_dir.exists():
                shutil.rmtree(target_dir)
            if not any(probe_root.iterdir()):
                probe_root.rmdir()
    pairs = changed_pairs(original, final)
    patch = unified_patch(original, final)
    bad = self_classify(pairs)
    edits_final = []
    for rel, n, before, after in pairs:
        layout_lines = original[rel]
        edits_final.append({"file": rel, "line": n, "before": before, "after": after,
                            "bindings": edit_bindings(before, layout_lines, n - 1)})
    risks = []
    if status == "ok" and bad:
        status, reason = "fail", VisFail("non_vis_edit", f"{bad[0][0]}:{bad[0][1]}")
    if status == "ok":
        risks = glob_check(edits_final, layout, lambda rel: original[rel] if rel in original else read_lines(repo / rel))
        manifest["glob_risk"] = risks
        if any(r["fail"] for r in risks):
            r = next(r for r in risks if r["fail"])
            status, reason = "fail", VisFail("glob_prelude_collision", f"{r['name']} ({r['kind']}) <- {r['importer']}")
    manifest["final_edits"] = [{k: v for k, v in e.items() if k != "bindings"} for e in edits_final]
    manifest["files"] = len({p[0] for p in pairs})
    manifest["equiv"] = equiv_verdict(repo, head) if status == "ok" else "판정 보류 (실패)"
    if status == "ok":
        (out_dir / f"vistool-{crate}.patch").write_text(patch, "utf-8")
        if apply:
            for rel in sorted(final):
                if read_lines(repo / rel) != original[rel]:
                    status, reason = "fail", VisFail("dirty_worktree", rel)
                    break
            else:
                for rel in sorted(final):
                    if final[rel] != original[rel]:
                        write_lines(repo / rel, final[rel])
                manifest["applied"] = True
    if status != "ok":
        manifest.setdefault("fail", {"reason": reason.reason, "detail": reason.detail})
        (out_dir / f"vistool-{crate}-partial.patch").write_text(patch, "utf-8")
    (out_dir / f"vistool-{crate}-manifest.json").write_text(json.dumps(manifest, indent=1, ensure_ascii=False), "utf-8")
    res = manifest["residue"]
    count = lambda k: res.get(k, {}).get("count", 0)
    if status == "ok":
        line = (f"VISTOOL_OK crate={crate} base={head} iters={iters} files={manifest['files']} edits={len(pairs)} "
                f"cut_residue={count('cut_residue')} test_cfg={count('test_cfg')} "
                f"impl_residue={count('impl_residue')} glob_risk={len(risks)}")
    else:
        line = f"VISTOOL_FAIL {reason.reason} crate={crate} iters={iters}"
    return status == "ok", line, manifest


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--crate", required=True)
    ap.add_argument("--partition", default=str(TOOL_REPO / "scripts" / "crate_split" / "partition.json"))
    ap.add_argument("--repo", default=".")
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--max-iters", type=int, default=10)
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--keep-probe", action="store_true")
    ap.add_argument("--cargo", default="cargo")
    args = ap.parse_args(argv)
    try:
        ok, line, manifest = vistool(args.repo, args.partition, args.crate, out_dir=args.out_dir,
                                     max_iters=args.max_iters, apply=args.apply, keep_probe=args.keep_probe,
                                     cargo=args.cargo)
    except VisFail as exc:
        print(f"VISTOOL_FAIL {exc.reason} crate={args.crate} iters=0")
        print(exc.detail, file=sys.stderr)
        return 2
    print(line)
    print(f"EQUIV {manifest['equiv']}")
    if not ok:
        print(manifest.get("fail", {}).get("detail", ""), file=sys.stderr)
    return 0 if ok else 2


if __name__ == "__main__":
    sys.exit(main())
