#!/usr/bin/env python3
"""Compiler-driven visibility widening for one crate cut, in shadow or real mode (#7353 VT1, VT2).

The partition (`partition.json`) lists, per future crate, the module groups it owns. A key may name a deeper
module (`discord::outbound::notification_policy`); the longest key that prefixes a module owns it. For a target crate X,
X and every crate before it in `order` are "below"; every crate after X is "above". Before any real cut the
tool builds two throwaway copies of HEAD under `<repo>/target/crate_split_probe/<X>/`:

  lower  the whole lib renamed `probe_lower`; only the `mod` chain down to each below leaf group becomes `pub`.
         Above modules still compile (privately), so a hard cut left in X does not break it. It declares the
         `test-support` feature, which upper turns on, with its dev dependencies joined to the normal ones.
  upper  the whole tree again, depending on `probe_lower`; each below leaf group's `mod x;` line becomes
         `pub use probe_lower::<path>::x;`, the parent re-export a member split will add. Where the line's cfg
         opens for upper but lower's build has no such module, an empty `mod x {}` stands in (vistool_gate.py).

Every round runs `cargo check --lib` on lower and `cargo check --lib --tests` on upper, reads the visibility
diagnostics (E0603 E0624 E0616 E0451 E0364 E0365, lower E0446 and `private_interfaces`/`private_bounds`), and
widens the modifier on the definition line to `pub` in both copies (a type E0446 names: `pub(crate)` first, `pub`
once rustc reports it again). Nothing else is ever edited; type mismatches from an uncut hard edge, test-only
items, foreign impls and macro paths are classified and reported, not fixed.

stdout line 1 is `VISTOOL_OK ...` or `VISTOOL_FAIL <reason> ...` (exit 2). `--apply` writes the patch into the
worktree only on success; the manifest and the (partial) patch go to `--out-dir` and are never committed.
`--out-dir` also gets `vistool-<X>-test-cfg.json` (vistool_testcfg.py): every upper test use of a lower
`#[cfg(test)]` item. `--real <workspace> --lower <member>` runs the same rounds on an existing member crate
(vistool_real.py) and fails unless no residue is left.
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

VERSION = "mkrxv-1"
VIS_CODES = {"E0603", "E0624", "E0616", "E0451", "E0364", "E0365"}
# rustc reports a tuple-struct constructor hidden by restricted fields as E0423 (call) / E0532 (pattern).
CTOR_CODES = {"E0423", "E0532"}
VIS_LINTS = {"private_interfaces", "private_bounds"}
# A hard error in lower once a widened type's trait impl names a narrower type in an associated type.
IFACE_CODE = "E0446"
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
ALREADY_PUB = "is already pub"
NAME_IN_MSG = re.compile(r"`([^`]+)`")
SCOPE = {"cut_residue": "cut", "test_cfg": "T/M test-support", "impl_residue": "I", "macro_path": "macro",
         "glob_hidden": "glob", "unclassified": "?"}

TOOL_REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(TOOL_REPO / "scripts" / "ci"))
import rust_test_sources as rts  # noqa: E402
sys.path.append(str(TOOL_REPO / "scripts"))  # rust_cfg, for vistool_gate

# Children (vistool_real, vistool_testcfg) import this module; as a script it must be the same object.
sys.path.insert(0, str(Path(__file__).resolve().parent))


class VisFail(Exception):
    def __init__(self, reason, detail=""):
        super().__init__(f"{reason}: {detail}")
        self.reason, self.detail = reason, detail


# ---------------------------------------------------------------------------------------------- partition

# Groups of one file: `group_of` gives each of their child modules a key of its own.
ONE_FILE = ("(root)", "services", "discord::(mod.rs)")


def group_key(path):
    """The one key spelling of an exact module path; `services::discord::X` is `discord::X`."""
    if path[:2] == ("services", "discord"):
        return "discord::" + ("::".join(path[2:]) or "(mod.rs)")
    return "::".join(path) or "(root)"


def group_of(module, keys=()):
    """Group key for a module path tuple: `config`, `services::tui_o`, `discord::inflight`, `discord::(mod.rs)`.
    A deeper key in `keys` wins over that group: the longest one that prefixes the module by whole segments."""
    m = tuple(x for x in module if x != "crate")
    base = 1 if m[:1] != ("services",) else 3 if m[1:2] == ("discord",) else 2
    deep = (group_key(m[:n]) for n in range(len(m), base, -1))
    return next((key for key in deep if key in keys), group_key(m[:base]))


def group_path(group):
    """Module path a group key names; `discord::X` lives under `services::discord`."""
    if group == "(root)":
        return ()
    head, _, tail = group.partition("::")
    if head == "discord":
        return ("services", "discord") + (() if tail == "(mod.rs)" else tuple(tail.split("::")))
    return tuple(group.split("::"))


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
            # One spelling per module path, or two crates could own the same module under different keys.
            path = group_path(group)
            if group_key(path) != group:
                raise VisFail("bad_partition", f"{group} is not a group key (`a::b`, `discord::x::y`)")
            # A crate cannot own a module inside the tree of a crate below it; the other way is a deep key.
            over = next((a for a in (group_key(path[:n]) for n in range(1, len(path)))
                         if a not in ONE_FILE and seen.get(a, crate) != crate), None)
            if over:
                raise VisFail("bad_partition", f"{group} ({crate}) is under {over} of the earlier crate {seen[over]}")
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
            chains = inv.file_cfg_texts.get(path, set())
            if chains and all(any(rts._cfg_requires_test_text(cfg, strict=True) for cfg in chain) for chain in chains):
                self.test_only.add(rel)
        self.file_of[()] = self.root_file
        self.module_of[self.root_file] = ()
        order = partition["order"]
        index = order.index(crate)
        self.owner = {g: c for c in order for g in partition["crates"][c]}
        self.below = {g for c in order[:index + 1] for g in partition["crates"][c]}
        self.crate, self.order = crate, order
        self.crate_of_file = {rel: self.owner.get(group_of(m, self.owner)) for rel, m in self.module_of.items()}
        self.x_files = {rel for rel, c in self.crate_of_file.items() if c == crate}
        paths = {g: group_path(g) for g in self.owner}
        parents = lambda g, pool: any(len(paths[h]) > len(paths[g]) and paths[h][:len(paths[g])] == paths[g]
                                      for h in pool)
        self._inline = {}
        # transform_upper's record, per file it rewrote: for each upper line the index of lower's line it
        # still is (None: a line upper has of its own).
        self.upper_origin = {}
        # A one-file group that parents other groups (discord::(mod.rs), services) is chain only. Any other
        # below group goes to upper whole, so a deep key under it needs no line of its own.
        whole = sorted(g for g in self.below if g != "(root)" and not (g in ONE_FILE and parents(g, self.owner)))
        self.leaves = [g for g in whole if not any(parents(h, [g]) for h in whole)]

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
        c = self.crate_of_file.get(rel) if line is None else \
            self.owner.get(group_of(self.module_at(rel, line), self.owner))
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


PROBE_FEATURE = "test-support"
REEXPORT = "pub use probe_lower::"
# An always-true target table: cargo unions it with `[dependencies]`, so no dependency line has to be merged.
PROBE_DEV_DEPS = "[target.'cfg(any(unix, not(unix)))'.dependencies"


def lower_manifest(text, lib_path):
    out, has_lib, has_features = [], False, False
    for header, lines in toml_sections(text):
        if header.startswith("[[bin]]"):
            continue
        if re.match(r"\[dev-dependencies[.\]]", header):
            # Feature-gated items build outside `cfg(test)` here and still use dev dependencies.
            lines[0] = lines[0].replace("[dev-dependencies", PROBE_DEV_DEPS, 1)
        if header == "[features]":
            has_features = True
            if not any(re.match(rf"\s*{PROBE_FEATURE}\s*=", l) for l in lines):
                lines.insert(1, f"{PROBE_FEATURE} = []")
        if header == "[package]":
            lines = [re.sub(r'^(\s*name\s*=\s*)"[^"]*"', r'\1"probe_lower"', l) for l in lines]
        if header == "[lib]":
            has_lib = True
            lines = [l for l in lines if not re.match(r"^\s*(path|name)\s*=", l)]
            lines.insert(1, f'path = "{lib_path}"')
        out.extend(lines)
    if not has_lib:
        out += ["", "[lib]", f'path = "{lib_path}"']
    if not has_features:
        out += ["", "[features]", f"{PROBE_FEATURE} = []"]
    return "\n".join(out)


def upper_manifest(text):
    out, sections = [], toml_sections(text)
    if not any(header == "[dependencies]" for header, _ in sections):
        sections.append(("[dependencies]", ["[dependencies]"]))
    for header, lines in sections:
        if header == "[workspace]":
            # `members = [...]` may wrap over several lines (VT1 review P2-4).
            lines = re.sub(r"(?ms)^(\s*members\s*=\s*)\[[^\]]*\]", r"\1[]", "\n".join(lines)).split("\n")
        if "dependencies" in header:
            # Share lower's workspace path crates so upper and lower agree on one copy of their types.
            lines = [re.sub(r'(\bpath\s*=\s*")(?!\.\./|/)([^"]+)"', r'\1../lower/\2"', l) for l in lines]
        if header == "[dependencies]":
            lines.insert(1, f'probe_lower = {{ path = "../lower", features = ["{PROBE_FEATURE}"] }}')
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
    # A deep key under a below group rides with that group. Opening its `mod` line here would change a file
    # the patch is cut from, and the patch would no longer apply to HEAD.
    for group in sorted(g for g in layout.below if g in ONE_FILE or g in layout.leaves):
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
    import vistool_gate
    rewired, layout.upper_origin = [], {}
    for group in layout.leaves:
        path = group_path(group)
        parent = layout.file_of.get(path[:-1])
        target = root / parent
        lines = read_lines(target)
        origin = layout.upper_origin.setdefault(parent, list(range(len(lines))))
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
            verdict, lower_cfg = vistool_gate.lower_gate(attrs)
            stand_in = f"{indent}mod {path[-1]} {{}}"
            if verdict == "never":
                # Lower's own test module: never checked by the probe (T slice), keep an empty stand-in.
                # A module `test-support` opens is built in lower, so it is re-exported like any other.
                lines[i:close + 1] = [stand_in]
                origin[i:close + 1] = [None]
                continue
            kept = [a for a in attrs if not re.match(r"^\s*#\[\s*(path|macro_use)\b", a)]
            new = kept + [f"{indent}{REEXPORT}{'::'.join(path)};"]
            if verdict == "split":
                # The gate also opens where lower has no such module (`test` with another feature off):
                # re-export only where lower has it, and keep the stand-in for the rest.
                new = kept + [f"{indent}#[cfg({lower_cfg})]", new[-1]] + \
                    kept + [f"{indent}#[cfg(not({lower_cfg}))]", stand_in]
            lines[start:close + 1] = new
            origin[start:close + 1] = [None] * len(new)
        write_lines(target, lines)
        rewired.append(f"{parent}:{path[-1]}")
    return rewired


# ---------------------------------------------------------------------------------------------- cargo

def run_check(cargo, cwd, args, target_dir, own_root, manifest=None):
    """Deduplicated compiler messages of one package: the one owning `manifest` when given (a member's lib
    path may leave its directory), else the one whose target sources sit under own_root."""
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
        if manifest is None and not src.startswith(prefix):
            continue
        if manifest is not None and Path(msg.get("manifest_path", "")).resolve() != Path(manifest).resolve():
            continue
        diag = msg["message"]
        diag["_test"] = bool(msg.get("target", {}).get("test")) or msg.get("profile", {}).get("test", False)
        diag["_src"] = msg.get("target", {}).get("src_path")  # the reporting target, for reading type paths
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
             "constructor is not visible here due to private fields", "` declared as ")


def definition_span(diag, marks=DEF_MARKS):
    """Where the restricted item is declared: the struct behind a tuple constructor (or E0446's
    `X declared as ..` label) first, then the `... defined here` note (or label), then a lint's
    `... is only usable at visibility` note."""
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
            # Only a `use` that binds the name (or a glob that may) declares it here.
            text = use_text(lines, i)
            if name in use_leaf_names(text) or "::*" in text:
                return i
            continue
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


def use_text(lines, index):
    """The `use` statement starting at lines[index], joined up to its `;`."""
    return " ".join(lines[index:index + 40]).split(";")[0]


def item_level_lines(lines):
    """0-based indexes whose first token sits at item level of its module (not inside a fn/impl/block)."""
    out, stack, depth, seen = set(), [], 0, set()
    toks = rts.rust_tokens("\n".join(lines))
    for i, t in enumerate(toks):
        if t.line not in seen:
            seen.add(t.line)
            if depth == (stack[-1] if stack else 0):
                out.add(t.line - 1)
        if t.kind != "punct":
            continue
        if t.value == "{":
            depth += 1
            if i >= 2 and toks[i - 2].value == "mod" and toks[i - 1].kind == "ident":
                stack.append(depth)
        elif t.value == "}":
            if stack and stack[-1] == depth:
                stack.pop()
            depth -= 1
    return out


def widen_line(line, to="pub"):
    """`to` (`pub`, or the `pub(crate)` notch) for the item on this line; None if it is already that wide or
    not an item."""
    m = ITEM.match(line)
    if not m or m.group("kw") == "macro_rules!":
        return None
    if re.sub(r"\s", "", m.group("vis") or "") in ("pub", to):
        return None
    body = line[m.start("quals"):]
    return m.group("indent") + to + " " + body


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
    parts.append(cur)  # blank after a trailing comma: kept so the comma survives a rewrite
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
        lead = len(field) - len(field.lstrip())
        if field.strip() and (only is None or index == only):
            # Only the visibility token changes; the type and the spacing around it stay as written.
            vis = re.match(r"(?:" + VIS + r")(?=[\s(])", field[lead:])
            field = field[:lead] + ("pub" + field[lead + vis.end():] if vis else "pub " + field[lead:])
        new.append(field)
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
    elif op[0] == "crate":
        new = widen_line(line, "pub(crate)")
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
        self._level_cache = {}
        self.owner = None  # vistool_paths.Owner: reads E0616 type paths across crates

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

    def resolve(self, diag, resolver, run_root, reporter=None):
        """(edits {(rel, idx): op}, outcome) for one visibility diagnostic `reporter` ('lower' | 'upper') gave.

        outcome: 'edit' | (residue class | 'outside_crate' | 'unresolved', why)."""
        kind, name, owner = message_target(diag)
        if code_of(diag) in ("E0364", "E0365"):
            return self._reexport(diag, name, resolver, run_root)
        span = definition_span(diag)
        if span is None and kind == "field":
            hits = self.unique_struct(owner)
            names = NAME_IN_MSG.findall(diag.get("message", ""))
            if len(hits) > 1 and self.owner and len(names) > 1:
                # Same-named structs: the type path picks one only when its owner is certain.
                hits = self.owner.owned(names[1], owner, reporter, diag.get("_src"), hits) or hits
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
        if ITEM.match(lines[index]).group("kw") == "use":
            # A re-export (often an already-`pub` `pub use inner::*;`): the blocker may be behind it, and
            # a `use` of an above item must stay restricted (P1-2), so resolve it before widening.
            return self._through_use(rel, index, name)
        return self._edit(rel, index, self._notch(diag, rel, index), name)

    def _notch(self, diag, rel, index):
        """`pub`, except for E0446 on a type narrower than the crate: rustc does not say how visible the
        interface is, so the type goes to `pub(crate)` and the next round reports it again if that is short."""
        if code_of(diag) != IFACE_CODE or "crate-private" in diag.get("message", ""):
            return ("item",)
        return ("item",) if widen_line(self.lines(rel)[index], "pub(crate)") is None else ("crate",)

    def _reexport(self, diag, name, resolver, run_root):
        """E0364/E0365 point at the `pub use` path, not the item: resolve the path through the module map."""
        span = primary_span(diag)
        side, rel = resolver.rel(span["file_name"], run_root)
        if self._place(rel, span["line_start"]):
            return {}, self._place(rel, span["line_start"])
        i = span["line_start"] - 1
        start = find_item_line(self.lines(rel), i, i, name, "item")
        return self._through_use(rel, i if start is None else start, name)

    def _levels(self, rel):
        if rel not in self._level_cache:
            self._level_cache[rel] = item_level_lines(self.lines(rel))
        return self._level_cache[rel]

    def _through_use(self, rel, index, name, hops=6):
        """Ops for a `use` at rel:index that binds `name` (explicit path or glob): the declaration behind it,
        every restricted re-export on the way, and this line when it is restricted.

        - A chain that reaches an above (or unpartitioned) item yields that class and no op at all, so no
          `pub use` of an above `pub(crate)` item is ever written (lower would fail with E0364).
        - In the target module, a direct declaration or explicit binding of `name` wins; only when there is
          none are its visible `pub(..) use x::*;` globs searched, and a glob is widened only if `name` is
          found behind it. Private `use x::*;` and fn-local `use`s are never touched."""
        # Every entry (the diagnostic's own line, a hop, a module re-export) obeys the boundary: a `use` the
        # target crate does not own is never widened, and the chain is that residue.
        if self._place(rel, index + 1):
            return {}, self._place(rel, index + 1)
        lines = self.lines(rel)
        text = use_text(lines, index)
        base = self.layout.module_at(rel, index + 1)
        own = {(rel, index): ("item",)} if widen_line(lines[index]) else {}
        # The flattened tree names the original item behind an alias and the full nested path (P2-2).
        entries = use_entries(text)
        # E0364/E0365 on an aliased re-export name the original item, other diagnostics the bound name.
        named = [(path, orig) for path, orig, bound in entries if bound == name and orig != "*"] or \
            [(path, orig) for path, orig, bound in entries if orig == name]
        targets = named or [(path, name) for path, orig, _ in entries if orig == "*"]
        edits, reached, outcome = {}, False, ("unresolved", f"no path to `{name}` at {rel}:{index + 1}")
        for segs, orig in targets:
            segs = list(segs)
            module = module_path(segs, base)
            found = self.layout.locate(module)
            if found is None:
                if named and segs and segs[0] not in ("crate", "self", "super") and \
                        self.layout.locate(base + (segs[0],)) is None:
                    reached = True  # another crate's (public) item: only this line may need `pub`
                    continue
                # A module named only through a re-export (`pub(crate) use inner::moved;`, an extracted member's
                # `use adk_base::domain::x;`): follow it, widening the re-export chain with the item.
                real, via, out = self._resolve_module(module, hops) if hops else (None, {}, outcome)
                if out != "edit":
                    if out[0] != "unresolved":
                        return {}, out
                    continue
                if real is None:
                    reached = True  # the module lives in another crate: its items are already public
                    edits.update(via)
                    continue
                module, found = real, self.layout.locate(real)
            else:
                via = {}
            target, first, last = found
            if self._place(target, first):
                return {}, self._place(target, first)
            tl, levels = self.lines(target), self._levels(target)
            cand = [j for j in range(max(first - 1, 0), min(last, len(tl)))
                    if j in levels and find_item_line(tl, j, j, orig, "item") == j
                    and self.layout.module_at(target, j + 1) == module and (target, j) != (rel, index)]
            is_use = lambda j: ITEM.match(tl[j]).group("kw") == "use"
            direct = [j for j in cand if not is_use(j) or orig in use_leaf_names(use_text(tl, j))]
            globs = [j for j in cand if j not in direct and ITEM.match(tl[j]).group("vis")]
            for j in direct or globs:
                if self._place(target, j + 1):
                    return {}, self._place(target, j + 1)
                if not is_use(j):
                    reached = True
                    if widen_line(tl[j]):
                        edits[(target, j)] = ("item",)
                    continue
                sub, out = self._through_use(target, j, orig, hops - 1) if hops else ({}, outcome)
                if out == "edit":
                    reached = True
                    edits.update(sub)
                elif ALREADY_PUB in out[1]:
                    reached = True  # the rest of the chain is public: only the restricted lines in front widen
                elif out[0] != "unresolved":
                    return {}, out
            if reached:
                edits.update(via)
        if not reached:
            return {}, outcome
        edits.update(own)
        if not edits:
            return {}, ("unresolved", f"`{name}` behind {rel}:{index + 1} {ALREADY_PUB}")
        return edits, "edit"

    def _resolve_module(self, module, hops):
        """(module that locates, or None when it lives in another crate; ops widening the `use` lines that
        re-export it; 'edit' or a failure outcome) for a module path no file or inline block declares."""
        miss = ("unresolved", f"no module {'::'.join(module)}")
        for cut in range(len(module) - 1, -1, -1):
            found = self.layout.locate(module[:cut])
            if found is None:
                continue
            rel, first, last = found
            tl, levels = self.lines(rel), self._levels(rel)
            is_use = lambda line: (lambda m: bool(m) and m.group("kw") == "use")(ITEM.match(line))
            uses = [j for j in range(max(first - 1, 0), min(last, len(tl)))
                    if j in levels and is_use(tl[j]) and self.layout.module_at(rel, j + 1) == module[:cut]]
            # An explicit binding of the module name first; else a visible glob over a module declaring it.
            picks = [(j, [(p, o) for p, o, b in use_entries(use_text(tl, j)) if b == module[cut] and o != "*"])
                     for j in uses]
            picks = [(j, hit) for j, hit in picks if hit] or [
                (j, [(p, module[cut])]) for j in uses if ITEM.match(tl[j]).group("vis")
                for p, o, _ in use_entries(use_text(tl, j))
                if o == "*" and self.layout.locate(module_path(list(p), module[:cut]) + (module[cut],))]
            for j, hit in picks[:1]:
                sub, out = self._through_use(rel, j, module[cut], hops - 1)
                if out != "edit" and ALREADY_PUB not in out[1]:
                    return None, {}, out
                segs = list(hit[0][0]) + [hit[0][1]]
                if segs[0] not in ("crate", "self", "super") and self.layout.locate(module[:cut] + (segs[0],)) is None:
                    return None, sub, "edit"
                target = module_path(segs, module[:cut]) + module[cut + 1:]
                if self.layout.locate(target) is None:
                    real, more, out = self._resolve_module(target, hops - 1) if hops > 1 else (None, {}, miss)
                    if out != "edit":
                        return None, {}, out
                    return real, {**sub, **more}, "edit"
                return target, sub, "edit"
            return None, {}, miss
        return None, {}, miss

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
        """Apply {(rel, idx): {op, ...}} to the text and both probe copies; returns {(rel, idx): new line}.

        Upper's files hold its re-exports, so each edit goes to upper's own copy of the line, never lower's
        file. A line upper does not have as lower has it stops the run before anything is written."""
        out, upper = {}, {}
        for (rel, index), group in sorted(ops.items()):
            old = self.lines(rel)[index]
            new = apply_ops(old, group)
            if new == old:
                continue
            out[(rel, index)] = new
            origin = self.layout.upper_origin.get(rel) if self.upper != self.lower else []
            at = index if origin is None else origin.index(index) if index in origin else None
            if at is None:
                continue  # one checkout serves both sides, or the line is in a module upper re-exports
            if rel not in upper:
                upper[rel] = read_lines(self.upper / rel)
            if at >= len(upper[rel]) or upper[rel][at] != old:
                raise VisFail("upper_probe_broken",
                              f"{rel}:{index + 1} is not this line in the upper copy: {old.strip()}")
            upper[rel][at] = new
        lower = {}
        for (rel, index), new in out.items():
            lower.setdefault(rel, self.lines(rel))[index] = new
        for root, files in ((self.lower, lower), (self.upper, upper)):
            for rel in sorted(files):
                write_lines(root / rel, files[rel])
        return out


def module_path(segs, base):
    """Module a `use` path prefix names from module `base` (`crate::`, `self::`, `super::`, child)."""
    module = () if segs and segs[0] == "crate" else base
    for seg in segs[1:] if segs and segs[0] == "crate" else segs:
        module = module[:-1] if seg == "super" else module if seg == "self" else module + (seg,)
    return module


def restricted_defs(layout, read):
    """Name -> [test_gated] for X items declared with a restricted or no visibility.
    An item inside a test-gated block (`#[cfg(test)] impl X {..}`, `cfg(all(test, unix))`) is gated too."""
    import vistool_testcfg
    rx = re.compile(r"^\s*(?:(pub\s*\([^)]*\))\s+)?" + QUALS + r"(?:fn|struct|enum|union|trait|type|const|static|mod)\s+(\w+)")
    out = defaultdict(list)
    for rel in sorted(layout.x_files):
        lines = read(rel)
        ranges = vistool_testcfg.gated_ranges(lines)
        for i, line in enumerate(lines):
            m = rx.match(line)
            if not m:
                continue
            gated = rel in layout.test_only or vistool_testcfg.gated_attr(lines, i) or \
                any(a <= i + 1 <= b for a, b in ranges)
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
    if code in TEST_CFG_CODES - {"E0599"} and names and any(g is False for g in restricted.get(names[0], [])):
        # A glob import silently skips a restricted item, so the use site reports "not found"; a method or
        # associated item (E0599) is never looked up through a glob.
        return "glob_hidden"
    if code in TEST_CFG_CODES and diag.get("_test"):
        return "test_cfg"
    if code in CUT_CODES:
        return "cut_residue"
    return "unclassified"


def probe_reexport_error(diag):
    """An unresolved import on a `pub use probe_lower::..;` line transform_upper wrote."""
    lines = [t.get("text", "") for t in (primary_span(diag) or {}).get("text") or []]
    return code_of(diag) == "E0432" and any(line.lstrip().startswith(REEXPORT) for line in lines)


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


USE_TOKEN = re.compile(r"(?:r#)?\w+|::|[{},*]")


def use_entries(text):
    """[(path segments, original name, bound name)] of one `use` statement, its brace tree flattened.
    A glob is (path, '*', '*'); `a::{self}` binds `a`; `x as y` binds `y`."""
    body = re.sub(r"^\s*(?:(?:" + VIS + r")\s+)?use\s+", "", text).split(";")[0]
    toks = [t.removeprefix("r#") for t in USE_TOKEN.findall(body)]
    out = []

    def tree(i, prefix):
        path = list(prefix)
        while i < len(toks):
            t = toks[i]
            if t == "::":
                i += 1
            elif t == "{":
                i += 1
                while i < len(toks) and toks[i] != "}":
                    i = i + 1 if toks[i] == "," else max(tree(i, path), i + 1)
                return i + 1
            elif t == "*":
                out.append((tuple(path), "*", "*"))
                return i + 1
            elif t in ",}":
                return i
            else:
                path.append(t)
                i += 1
                if i < len(toks) and toks[i] == "::":
                    continue
                bound = None
                if i + 1 < len(toks) and toks[i] == "as":
                    bound, i = toks[i + 1], i + 2
                if path[-1] == "self" and len(path) > 1:
                    path = path[:-1]
                out.append((tuple(path[:-1]), path[-1], bound or path[-1]))
                return i
        return i

    tree(0, ())
    return out


def use_leaf_names(text):
    """Names a `use` item binds (alias wins), ignoring globs and `_`."""
    return [bound for _, orig, bound in use_entries(text) if orig != "*" and bound != "_"]


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
        globs = [("*", "glob")] if "::*" in text.split(";")[0] else []
        return [(n, "use") for n in use_leaf_names(text)] + globs
    name = re.match(r"(?:mut\s+)?(\w+)", m.group("rest"))
    return [(name.group(1), kw)] if name else []


def glob_imports(layout, read):
    """[(importer module, importer rel, line, target module)] for every glob of a `use` tree (braces too)."""
    modules = set(layout.file_of)
    out = []
    for rel in sorted(layout.module_of):
        try:
            lines = read(rel)
        except OSError:
            continue
        for i, line in enumerate(lines):
            m = ITEM.match(line)
            if not m or m.group("kw") != "use":
                continue
            for path, orig, _ in use_entries(use_text(lines, i)):
                segs = list(path)
                if orig != "*" or not segs:
                    continue
                importer = cur = layout.module_at(rel, i + 1)
                if segs[0] == "crate":
                    cur, segs = (), segs[1:]
                elif segs[0] in ("self", "super"):
                    pass
                elif importer + (segs[0],) not in modules and layout.locate(importer + (segs[0],)) is None:
                    continue
                for seg in segs:
                    cur = cur[:-1] if seg == "super" else cur if seg == "self" else cur + (seg,)
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
            if kind == "glob":
                # Which names a widened restricted glob re-export brings (a trait? a prelude name?) is not
                # resolved here: fail closed.
                risks.append({"file": e["file"], "line": e["line"], "name": "*", "kind": "glob",
                              "importer": "(glob re-export widened beyond its old scope)", "fail": True})
                continue
            for importer, irel, iline, target in imports:
                if target != module or importer[:len(scope)] == scope:
                    continue
                fail = name in PRELUDE or kind == "trait"
                risks.append({"file": e["file"], "line": e["line"], "name": name, "kind": kind,
                              "importer": f"{irel}:{iline}", "fail": fail})
    return risks


def equiv_verdict(repo, head, patch, out_dir):
    """KR-C1's equiv_check.py class for the patch itself; this tool never re-implements it.

    The patch goes into a throwaway commit on top of HEAD (own index file, nothing checked out), which is
    compared statically (`--skip-cargo`): a converged run must be `class=vis` (VT1 review P2-3)."""
    script = TOOL_REPO / "scripts" / "crate_split" / "equiv_check.py"
    if not script.is_file():
        return "판정 보류 (equiv_check.py 없음)"
    if not patch:
        return "skip (no edits)"
    index = Path(out_dir).resolve() / "equiv.index"
    env = dict(os.environ, GIT_INDEX_FILE=str(index))
    git = lambda *a, **k: subprocess.run(["git", "-C", str(repo), *a], env=env, capture_output=True, text=True, **k)
    try:
        steps = [git("read-tree", head), git("apply", "--cached", "-", input=patch)]
        tree = git("write-tree")
        steps.append(tree)
        commit = git("-c", "user.name=vistool", "-c", "user.email=vistool@example.invalid", "commit-tree",
                     tree.stdout.strip(), "-p", head, "-m", "vistool patch (equiv probe)")
        steps.append(commit)
    finally:
        index.unlink(missing_ok=True)
    broken = next((p for p in steps if p.returncode), None)
    if broken is not None:
        return f"git_failed {broken.stderr.strip()[-300:]}"
    proc = subprocess.run([sys.executable, str(script), "--repo", str(repo), "--base", head,
                           "--head", commit.stdout.strip(), "--skip-cargo"], capture_output=True, text=True)
    row = re.search(r"^\| b class \| ([^|]+?) \|", proc.stdout, re.M)
    return f"class={row.group(1)}" if row else f"exit={proc.returncode} {(proc.stdout.splitlines() or [''])[0]}"


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


class ShadowProbe:
    """Two throwaway copies of HEAD (lower, upper) under <repo>/target/crate_split_probe/<X>/."""
    mode = "shadow"
    lib_name = "probe_lower"

    def __init__(self, repo, partition_path, crate):
        self.repo, self.crate, self.label = repo, crate, crate
        self.partition = load_partition(partition_path)
        self.probe_root = repo / "target" / "crate_split_probe"
        self.probe, self.target_dir = self.probe_root / crate, self.probe_root / "target"
        self.lower, self.upper = self.probe / "lower", self.probe / "upper"

    def prepare(self, manifest):
        manifest["partition_base"] = self.partition["base"]
        layout = Layout(self.repo, self.partition, self.crate)
        manifest["x_files"] = len(layout.x_files)
        if self.probe.exists():
            shutil.rmtree(self.probe)
        extract_head(self.repo, self.lower)
        extract_head(self.repo, self.upper)
        lib_rel = transform_lower(self.lower, layout)
        manifest["upper_rewired"] = transform_upper(self.upper, layout)
        return layout, Resolver(self.lower, self.upper, lib_rel, layout.root_file)

    def check_lower(self, cargo):
        # With the feature upper turns on, so a gated item that does not build is a lower error with a span.
        return run_check(cargo, self.lower, ["--lib", "--features", PROBE_FEATURE], self.target_dir, self.lower)

    def check_upper(self, cargo):
        return run_check(cargo, self.upper, ["--lib", "--tests"], self.target_dir, self.upper)

    def path_owner(self, layout, read):
        """Type paths read against probe_lower and the upper copy (named as the repo package)."""
        import tomllib
        import vistool_paths
        cargo = tomllib.loads((self.repo / "Cargo.toml").read_text("utf-8"))
        name = cargo.get("lib", {}).get("name") or cargo["package"]["name"].replace("-", "_")
        root = self.upper / layout.root_file
        return vistool_paths.Owner(layout, read, self.lib_name, {name: root}, root, self.upper)

    def accept(self, manifest):
        """Last word on a converged run; shadow leaves residue to the slices that own it."""

    def cleanup(self):
        if self.probe.exists():
            shutil.rmtree(self.probe)

    def ok_line(self, head, iters, manifest, edits, risks, count):
        return (f"VISTOOL_OK crate={self.crate} base={head} iters={iters} files={manifest['files']} edits={edits} "
                f"cut_residue={count('cut_residue')} test_cfg={count('test_cfg')} "
                f"impl_residue={count('impl_residue')} glob_risk={len(risks)}")

    def fail_line(self, reason, iters):
        return f"VISTOOL_FAIL {reason} crate={self.crate} iters={iters}"


def vistool(repo, partition_path, crate, *, out_dir, max_iters=10, apply=False, keep_probe=False,
            cargo="cargo", log=print):
    repo = Path(repo).resolve()
    return run_probe(ShadowProbe(repo, partition_path, crate), repo, out_dir=out_dir, max_iters=max_iters,
                     apply=apply, keep_probe=keep_probe, cargo=cargo, log=log)


def run_probe(probe, repo, *, out_dir, max_iters=10, apply=False, keep_probe=False, cargo="cargo", log=print):
    """The widening rounds shared by shadow and real mode; `probe` owns the copies and the cargo calls."""
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    head = git_head(repo)
    label = probe.label
    manifest = {"tool": VERSION, "mode": probe.mode, "crate": probe.crate, "head": head,
                "rounds": [], "edits": [], "unresolved": [], "residue": {}, "glob_risk": []}
    status, reason, widener, iters, layout, resolver, test_cfg = "fail", None, None, 0, None, None, []
    try:
        layout, resolver = probe.prepare(manifest)
        widener = Widener(layout, probe.lower, probe.upper)
        widener.owner = probe.path_owner(layout, widener.lines)
        restricted = restricted_defs(layout, widener.lines)
        while True:
            iters += 1
            low, low_s = probe.check_lower(cargo)
            up, up_s, ran_upper = [], 0.0, False
            if not any(d.get("level") == "error" for d in low):
                # Upper cannot build on a broken lower; its round waits for lower's re-export fixes.
                up, up_s = probe.check_upper(cargo)
                ran_upper = True
            (out_dir / f"vistool-{label}-diags-{iters}.json").write_text(
                json.dumps({"lower": low, "upper": up}, indent=0, ensure_ascii=False), "utf-8")
            vis, residue, low_err = [], defaultdict(list), []
            for d in low:
                code = code_of(d)
                if is_vis(d) or (code in VIS_LINTS and d.get("level") == "warning") or code == IFACE_CODE:
                    vis.append((d, probe.lower, True))
                elif d.get("level") == "error":
                    low_err.append(d)
            for d in up:
                if d.get("level") != "error":
                    continue
                if probe_reexport_error(d) or code_of(d) == IFACE_CODE:
                    # Lower lacks a module upper re-exports, or upper's interface leaks a type no edit here
                    # reaches: as residue it would hide every edit behind it and pass a build that failed.
                    raise VisFail("upper_probe_broken", f"{where(d, resolver)} {d.get('message')}")
                if is_vis(d):
                    vis.append((d, probe.upper, False))
                else:
                    residue[classify_residue(d, restricted)].append(d)
            edits, unresolved, outside = {}, [], []
            for d, run_root, from_lower in vis:
                new, outcome = widener.resolve(d, resolver, run_root, "lower" if from_lower else "upper")
                if outcome == "edit":
                    for key, op in new.items():
                        entry = edits.setdefault(key, [set(), set(), set()])
                        entry[0].add(op)
                        entry[1].add(code_of(d))
                        entry[2].add(where(d, resolver))
                    continue
                kind, why = outcome
                if from_lower and d.get("level") == "error":
                    # A lower error nothing here repairs keeps upper from running: never a success (P1-2).
                    low_err.append(d)
                elif kind in ("cut_residue", "macro_path", "impl_residue"):
                    residue[kind].append(d)
                elif kind == "outside_crate":
                    outside.append(f"{code_of(d)} {why} <- {where(d, resolver)}")
                else:
                    unresolved.append(f"{code_of(d)} {d.get('message')} :: {why} <- {where(d, resolver)}")
            vis_count = len(vis)
            # rustc skips its field-privacy pass (E0451) while any other error stands: say so, never hide it.
            manifest["privacy_pass_masked"] = any(d.get("level") == "error" for d in up) or not ran_upper
            manifest["rounds"].append({"iter": iters, "lower_errors": len(low_err), "vis": vis_count,
                                       "edits": len(edits), "unresolved": len(unresolved),
                                       "residue": {k: len(v) for k, v in sorted(residue.items())},
                                       "lower_secs": low_s, "upper_secs": up_s,
                                       # Largest finished descendant (rustc) so far, KiB on Linux.
                                       "max_child_rss_kib": resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss})
            log(f"vistool: iter={iters} lower_err={len(low_err)} vis={vis_count} edits={len(edits)} "
                f"unresolved={len(unresolved)} lower={low_s}s upper={up_s}s", file=sys.stderr)
            manifest["unresolved"] = unresolved
            if ran_upper or not manifest["residue"]:
                # Residue comes from the last round upper ran in; a lower-only round must not erase it.
                manifest["residue"] = {k: {"count": len(residue.get(k, [])), "scope": SCOPE[k],
                                           "first": [f"{code_of(d)} {where(d, resolver)} {d.get('message')}"
                                                     for d in residue.get(k, [])[:20]]} for k in RESIDUES}
                test_cfg = list(residue.get("test_cfg", []))
            if low_err:
                manifest["lower_errors"] = [f"{code_of(d)} {where(d, resolver)} {d.get('message')}"
                                            for d in low_err[:20]]
                raise VisFail("lower_probe_broken" if iters == 1 and not manifest["edits"] else "lower_broken",
                              manifest["lower_errors"][0])
            if outside:
                manifest["outside"] = outside[:50]
                raise VisFail("outside_crate", outside[0])
            if not edits and not unresolved and ran_upper:
                # Every visibility diagnostic left is residue a later slice owns (cut, impl, macro).
                if residue.get("unclassified"):
                    raise VisFail("unclassified_residue", manifest["residue"]["unclassified"]["first"][0])
                probe.accept(manifest)
                break
            if not edits:
                raise VisFail("no_progress", (unresolved or ["upper did not run and nothing is left to edit"])[0])
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
        if not keep_probe and probe.probe_root.exists():
            probe.cleanup()
            if probe.target_dir.exists():
                shutil.rmtree(probe.target_dir)
            if not any(probe.probe_root.iterdir()):
                probe.probe_root.rmdir()
    read_original = lambda rel: original[rel] if rel in original else read_lines(repo / rel)
    if layout is not None and resolver is not None:
        import vistool_testcfg
        manifest["test_cfg_list"] = vistool_testcfg.write_list(
            out_dir / f"vistool-{label}-test-cfg.json", test_cfg, resolver, layout, read_original,
            {"tool": VERSION, "mode": probe.mode, "crate": probe.crate, "head": head})
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
        risks = glob_check(edits_final, layout, read_original)
        manifest["glob_risk"] = risks
        if any(r["fail"] for r in risks):
            r = next(r for r in risks if r["fail"])
            status, reason = "fail", VisFail("unresolved_glob_reexport" if r["kind"] == "glob" else
                                             "glob_prelude_collision", f"{r['name']} ({r['kind']}) <- {r['importer']}")
    manifest["final_edits"] = [{k: v for k, v in e.items() if k != "bindings"} for e in edits_final]
    manifest["files"] = len({p[0] for p in pairs})
    if status == "ok":
        manifest["equiv"] = equiv_verdict(repo, head, patch, out_dir)
        if not manifest["equiv"].startswith(("class=vis", "판정 보류", "skip")):
            status, reason = "fail", VisFail("equiv_not_vis", manifest["equiv"])
    else:
        manifest["equiv"] = "판정 보류 (실패)"
    if status == "ok":
        (out_dir / f"vistool-{label}.patch").write_text(patch, "utf-8")
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
        (out_dir / f"vistool-{label}-partial.patch").write_text(patch, "utf-8")
    (out_dir / f"vistool-{label}-manifest.json").write_text(json.dumps(manifest, indent=1, ensure_ascii=False), "utf-8")
    res = manifest["residue"]
    count = lambda k: res.get(k, {}).get("count", 0)
    if status == "ok":
        line = probe.ok_line(head, iters, manifest, len(pairs), risks, count)
    else:
        line = probe.fail_line(reason.reason, iters)
    return status == "ok", line, manifest


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--crate", help="shadow mode: partition crate to cut out (KRX, KE, DL)")
    ap.add_argument("--partition", default=str(TOOL_REPO / "scripts" / "crate_split" / "partition.json"))
    ap.add_argument("--repo", default=".")
    ap.add_argument("--real", metavar="WORKSPACE", help="real mode: workspace root whose --lower member exists")
    ap.add_argument("--lower", help="real mode: lower member dir (crates/<x>)")
    ap.add_argument("--upper", default=".", help="real mode: upper member dir (default: the root package)")
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--max-iters", type=int, default=10)
    ap.add_argument("--apply", action="store_true")
    ap.add_argument("--keep-probe", action="store_true")
    ap.add_argument("--cargo", default="cargo")
    args = ap.parse_args(argv)
    common = dict(out_dir=args.out_dir, max_iters=args.max_iters, apply=args.apply, keep_probe=args.keep_probe,
                  cargo=args.cargo)
    if args.real is not None:
        import vistool_real
        if not args.lower:
            print(f"VISTOOL_FAIL no_member mode=real crate=? iters=0")
            print("--real needs --lower <member dir>", file=sys.stderr)
            return 2
        ok, line, manifest = vistool_real.real(args.real, args.lower, args.upper, **common)
        print(line)
        if not ok and manifest.get("residue"):
            print("\n".join(vistool_real.residue_table(manifest)))
    else:
        if not args.crate:
            ap.error("--crate is required (or --real WORKSPACE --lower DIR)")
        try:
            ok, line, manifest = vistool(args.repo, args.partition, args.crate, **common)
        except VisFail as exc:
            print(f"VISTOOL_FAIL {exc.reason} crate={args.crate} iters=0")
            print(exc.detail, file=sys.stderr)
            return 2
        print(line)
    print(f"EQUIV {manifest['equiv']}")
    if manifest.get("test_cfg_list"):
        t = manifest["test_cfg_list"]
        print(f"TEST_CFG count={t['count']} resolved={t['resolved']} file={t['file']}")
    if not ok:
        print(manifest.get("fail", {}).get("detail", ""), file=sys.stderr)
    return 0 if ok else 2


if __name__ == "__main__":
    sys.modules.setdefault("vistool", sys.modules[__name__])
    sys.exit(main())
