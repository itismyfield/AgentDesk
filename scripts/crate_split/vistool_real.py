"""vistool real mode (#7353 VT2): the widening rounds on a workspace whose lower crate is already a member.

    vistool.py --real <workspace root> --lower <member dir> [--upper <member dir, default .>] --out-dir DIR [--apply]

Shadow mode builds two copies of one crate. Once the cut exists (M slices), the lower crate is a real
workspace member (`crates/<x>/Cargo.toml`) and this mode checks the workspace as it is: one `git archive HEAD`
copy under `<root>/target/crate_split_probe/real/ws`, `cargo check --lib -p <lower>` and
`cargo check --lib --tests -p <upper>` per round, the same classification (visibility, cut_residue, test_cfg,
impl_residue, macro_path, glob_hidden) and the same widening rule (only the modifier on a lower definition
line). Upper and every other member are never edited.

- `--lower`/`--upper` must be explicit `[workspace] members` with a lib target: anything else is
  `VISTOOL_FAIL no_member`. It never falls back to shadow mode.
- Success is `VISTOOL_OK mode=real ... residue=0`; any residue left once nothing needs widening is
  `VISTOOL_FAIL residue mode=real` with the class table on stdout.
"""

from pathlib import Path
import shutil
import subprocess

import vistool as vt


def workspace_members(repo):
    """{member dir relative to repo ('.' for the root package): (manifest, parsed toml)}."""
    repo = Path(repo).resolve()  # a symlinked path (macOS /var -> /private/var) must match resolved manifests
    try:
        packages = vt.rts.workspace_packages(repo)
    except (ValueError, OSError) as exc:
        raise vt.VisFail("no_member", f"workspace manifest: {exc}")
    out = {}
    for manifest, cargo in packages:
        if "package" in cargo:
            rel = manifest.parent.resolve().relative_to(repo).as_posix()
            out[rel or "."] = (manifest, cargo)
    return out


def norm(member):
    rel = Path(member).as_posix().strip("/")
    return "." if rel in ("", ".") else rel.removeprefix("./")


def member_of(rel, members):
    """Longest member dir holding a repo-relative path ('.' last)."""
    hits = [m for m in members if m != "." and (rel == m or rel.startswith(m + "/"))]
    return max(hits, key=len) if hits else ("." if "." in members else None)


class RealLayout(vt.Layout):
    """Module map of the lower member only; `side_of` answers by member instead of by partition group."""

    def __init__(self, repo, members, lower, upper, label):
        self.repo, self.members, self.lower_dir, self.upper_dir = Path(repo).resolve(), members, lower, upper
        manifest, cargo = members[lower]
        root = (manifest.parent / cargo.get("lib", {}).get("path", "src/lib.rs")).resolve()
        inv = vt.rts.walk(root, self.repo, manifest_dir=manifest.parent.resolve())
        self.root_file = root.relative_to(self.repo).as_posix()
        self.module_of, self.file_of, self.test_only = {}, {}, set()
        for path, modules in inv.modules.items():
            rel = Path(path).resolve().relative_to(self.repo).as_posix()
            module = min(tuple(x for x in m if x != "crate") for m in modules)
            self.module_of[rel] = module
            self.file_of.setdefault(module, rel)
            chains = inv.file_cfgs.get(path, set())
            if chains and all(any(vt.rts._cfg_requires_test(p) for p in chain) for chain in chains):
                self.test_only.add(rel)
        self.file_of[()] = self.root_file
        self.module_of[self.root_file] = ()
        self.crate, self.order, self.leaves, self._inline = label, [label], [], {}
        self.x_files = set(self.module_of)

    def side_of(self, rel, line=None):
        if rel in self.x_files:
            return "x"
        return "above" if member_of(rel, self.members) == self.upper_dir else None


class RealResolver:
    """rustc file names (relative to the workspace root) -> ('lower' | 'upper' | None, repo-relative path)."""

    def __init__(self, ws, members, lower, upper):
        self.ws, self.members, self.lower, self.upper = ws.resolve(), members, lower, upper

    def rel(self, file_name, run_root):
        p = Path(file_name)
        p = (p if p.is_absolute() else run_root / p).resolve()
        if not p.is_relative_to(self.ws):
            return None, p.as_posix()
        rel = p.relative_to(self.ws).as_posix()
        owner = member_of(rel, self.members)
        return ("lower" if owner == self.lower else "upper" if owner == self.upper else None), rel


class RealProbe:
    mode = "real"

    def __init__(self, repo, lower, upper="."):
        self.repo, self.lower_dir, self.upper_dir = Path(repo).resolve(), norm(lower), norm(upper)
        self.crate = self.lower_dir
        self.label = "real-" + Path(self.lower_dir).name.replace(".", "root")
        self.probe_root = self.repo / "target" / "crate_split_probe"
        self.probe, self.target_dir = self.probe_root / "real", self.probe_root / "target"
        self.lower = self.upper = self.probe / "ws"

    def _validate(self):
        top = subprocess.run(["git", "-C", str(self.repo), "rev-parse", "--show-toplevel"], capture_output=True,
                             text=True).stdout.strip()
        if not top or Path(top).resolve() != self.repo:
            raise vt.VisFail("no_member", f"{self.repo} is not the git top level of its workspace")
        members = workspace_members(self.repo)
        for role, rel in (("lower", self.lower_dir), ("upper", self.upper_dir)):
            if rel not in members:
                raise vt.VisFail("no_member", f"--{role} {rel}: no workspace member manifest "
                                              f"(members: {', '.join(sorted(members)) or 'none'})")
            manifest, cargo = members[rel]
            if not (manifest.parent / cargo.get("lib", {}).get("path", "src/lib.rs")).is_file():
                raise vt.VisFail("no_member", f"--{role} {rel}: member has no lib target")
        if self.lower_dir == self.upper_dir:
            raise vt.VisFail("no_member", "--lower and --upper name the same member")
        return members

    def prepare(self, manifest):
        members = self._validate()
        self.members = members
        name = lambda rel: members[rel][1]["package"]["name"]
        self.lower_pkg, self.upper_pkg = name(self.lower_dir), name(self.upper_dir)
        self.crate = self.lower_pkg
        manifest.update(crate=self.lower_pkg, upper=self.upper_pkg, lower_dir=self.lower_dir,
                        upper_dir=self.upper_dir)
        layout = RealLayout(self.repo, members, self.lower_dir, self.upper_dir, self.lower_pkg)
        manifest["x_files"] = len(layout.x_files)
        if self.probe.exists():
            shutil.rmtree(self.probe)
        vt.extract_head(self.repo, self.lower)
        return layout, RealResolver(self.lower, members, self.lower_dir, self.upper_dir)

    def _check(self, cargo, pkg, rel, args):
        own = self.lower / rel
        return vt.run_check(cargo, self.lower, [*args, "-p", pkg], self.target_dir, own,
                            manifest=own / "Cargo.toml")

    def check_lower(self, cargo):
        return self._check(cargo, self.lower_pkg, self.lower_dir, ["--lib"])

    def check_upper(self, cargo):
        return self._check(cargo, self.upper_pkg, self.upper_dir, ["--lib", "--tests"])

    def accept(self, manifest):
        left = {k: v["count"] for k, v in manifest["residue"].items() if v["count"]}
        if left:
            raise vt.VisFail("residue", " ".join(f"{k}={n}" for k, n in sorted(left.items())))

    def cleanup(self):
        if self.probe.exists():
            shutil.rmtree(self.probe)

    def ok_line(self, head, iters, manifest, edits, risks, count):
        return (f"VISTOOL_OK mode=real crate={self.crate} upper={self.upper_pkg} base={head} iters={iters} "
                f"files={manifest['files']} edits={edits} residue=0 glob_risk={len(risks)}")

    def fail_line(self, reason, iters):
        return f"VISTOOL_FAIL {reason} mode=real crate={self.crate} iters={iters}"


def residue_table(manifest):
    """Markdown rows of the residue classes (count, owning slice, first sites)."""
    rows = ["| class | count | scope | first |", "|---|---|---|---|"]
    for k, v in manifest.get("residue", {}).items():
        rows.append(f"| {k} | {v['count']} | {v['scope']} | {'; '.join(v['first'][:3]) or '-'} |")
    return rows


def real(repo, lower, upper=".", *, out_dir, max_iters=10, apply=False, keep_probe=False, cargo="cargo",
         log=print):
    repo = Path(repo).resolve()
    return vt.run_probe(RealProbe(repo, lower, upper), repo, out_dir=out_dir, max_iters=max_iters, apply=apply,
                        keep_probe=keep_probe, cargo=cargo, log=log)
