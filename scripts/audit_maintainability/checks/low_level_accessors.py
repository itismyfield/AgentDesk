"""Per-site no-growth audit for raw host-policy access outside its owners."""
from __future__ import annotations

from collections import Counter
from hashlib import sha256
import json

from ci.rust_test_sources import Inventory, crate_roots, walk
from ..rust_accessor_scan import (Token, boot_bindings, call_arguments, exports_for,
                                imports, pairs, receiver_start, resolve, scopes, tokens)
from .. import common
from ..common import Finding
from . import CheckSpec

CONFIG = ("src/config/session_hosts.rs", "src/config/session_hosts/")
INVENTORY = (*CONFIG, "src/services/session_host/herdr_registry.rs",
             "src/services/session_host/herdr/managed_config/maintenance.rs",
             "src/services/session_host/herdr/policy/diagnostics.rs")
RULES = {
    "herdr_endpoint": {"accessor": "herdr_endpoint", "owned_paths": INVENTORY},
    "herdr_configured_for_tui_launch": {"accessor": "herdr_configured_for_tui_launch",
        "owned_paths": ("src/services/herdr_launch.rs", "src/services/herdr_launch/")},
    "configured_channel_ids": {"accessor": "configured_channel_ids", "owned_paths": CONFIG},
    "with_boot": {"accessor": "with_boot", "owned_paths": INVENTORY},
    "boot_channels": {"accessor": "channels", "owned_paths": INVENTORY},
}


def owned(path, paths):
    return any(path.startswith(p) if p.endswith("/") else path == p for p in paths)


def identity(row):
    return tuple(row[k] for k in ("rule", "path", "enclosing_fn", "normalized_hash"))


def compare(accepted, candidate, *, relabel=False):
    """Deletions buy no new sites; relabeling preserves the complete hash multiset."""
    metadata = {}
    for row in [*accepted, *candidate]:
        config = (row["accessor"], tuple(row["owned_paths"]))
        if row["rule"] in metadata and metadata[row["rule"]] != config:
            return ["accessor or owner scope changed"]
        metadata[row["rule"]] = config
    key = (lambda r: (r["rule"], r["normalized_hash"])) if relabel else identity
    def counts(rows):
        result = Counter()
        for row in rows:
            if type(row["count"]) is not int or row["count"] < 1:
                raise ValueError("site count must be a positive integer")
            result[key(row)] += row["count"]
        return result
    old, new = counts(accepted), counts(candidate)
    if relabel:
        return [] if old == new else ["relabel requires identical (rule, normalized hash) multiplicities"]
    return [f"new/increased accessor site {site}: {new[site]} > {old[site]}" for site in sorted(new) if new[site] > old[site]]


def _sites(path, text, production, *, module=(), exports=None):
    ts = tokens(text)
    matched = pairs(ts)
    metadata, declarations = scopes(ts, matched)
    aliases, excluded = imports(ts, production, metadata)
    values, rows = [t.value for t in ts], {}
    bindings = boot_bindings(ts, metadata, aliases, matched, module=module, exports=exports)
    for i, token in enumerate(ts):
        if (i in excluded or i in declarations or token.kind != "code" or token.start not in production
                or not (token.value[0].isalpha() or token.value[0] == "_")):
            continue
        start, names = i, [token.value]
        while start >= 2 and values[start - 1] == "::":
            start -= 2
            names.insert(0, values[start])
        if values[i + 1:i + 2] == ["::"] and values[i + 2:i + 3] != ["<"]:
            continue
        scope, function = metadata[i]
        names = resolve(names, scope, aliases, module=module, exports=exports)
        accessor = names[-1]
        method = values[max(0, start - 1):start] == ["."]
        rule = accessor if accessor in RULES and accessor != "boot_channels" else None
        if accessor == "channels" and names[-2:-1] == ["BootSessionHosts"]:
            rule = "boot_channels"
        receiver = receiver_start(values, start, matched) if method else start
        if accessor == "channels" and method:
            kinds = [kind for name, kind, fn, where, lo, hi in bindings
                     if name == values[receiver] and (fn == function or fn == "<module>")
                     and scope[:len(where)] == where and lo <= i <= hi]
            if (kinds[-1:] == ["BootSessionHosts"] or not kinds and values[receiver] == "boot"):
                rule = "boot_channels"
        if rule is None or owned(path, RULES[rule]["owned_paths"]):
            continue
        # A reference passed as a function value is a site too; imports/definitions are not.
        opening = call_arguments(values, i)
        end = matched[opening] + 1 if opening is not None else i + 1
        normalized = [("accessor", rule)]
        if method:
            normalized.extend((t.kind, t.value) for t in ts[receiver:start])
        normalized.extend((t.kind, t.value) for t in ts[i + 1:end])
        digest = sha256(json.dumps(normalized, separators=(",", ":")).encode()).hexdigest()
        row = {"rule": rule, "accessor": RULES[rule]["accessor"], "owned_paths": list(RULES[rule]["owned_paths"]),
               "path": path, "enclosing_fn": function, "normalized_hash": digest, "count": 1, "line": token.line}
        key = identity(row)
        if key in rows:
            rows[key]["count"] += 1
        else:
            rows[key] = row
    return list(rows.values())


def scan(repo):
    """Scan the non-test module graph across all crate roots and cfg targets once per file."""
    repo = repo.resolve()
    roots = crate_roots(repo)
    if not roots:
        raise ValueError("no Rust crate roots found")
    sources = {}
    inventory = Inventory()
    for root in roots:
        walk(root, repo, inventory, production=True)
        if inventory.read_failures or inventory.errors:
            raise ValueError("; ".join(inventory.read_failures + inventory.errors))
        for path, ts in inventory.source_tokens.items():
            if ts is not None:
                sources.setdefault(path, set()).update(t.start for t in ts if t.kind != "string")
    exports, views = {}, {}
    for path, production in sorted(sources.items()):
        ts = [Token(t.value, t.start, t.line, "literal" if t.kind == "string" else "code")
              for t in inventory.source_tokens[path]]
        metadata, _ = scopes(ts, pairs(ts))
        aliases, _ = imports(ts, production, metadata)
        module = min(inventory.modules[path])
        exports.update(exports_for(ts, metadata, aliases, module))
        views[path] = (ts, aliases, module)
    names = {config["accessor"] for config in RULES.values()}
    for key, target in exports.items():
        if resolve(target, (), {}, exports=exports)[-1] in names:
            names.add(key[-1])
    rows = []
    for path, production in sorted(sources.items()):
        ts, aliases, module = views[path]
        if not any(t.kind == "code" and t.value in names for t in ts):
            continue
        rows.extend(_sites(path.relative_to(repo).as_posix(), path.read_text("utf-8"), production,
                           module=module, exports=exports))
    return sorted(rows, key=identity)


def _run(_allowlist):
    from .low_level_accessor_baseline import DIRECTORY, baseline_growth, load_baselines
    try:
        try:
            accepted = load_baselines(common.REPO_ROOT)
        except FileNotFoundError as exc:
            yield Finding("low_level_accessors", "warn", str(DIRECTORY), None,
                          "accessor baseline missing; skipping accessor ratchet",
                          {"missing_path": str(exc.filename)})
            return
        rows = scan(common.REPO_ROOT)
        errors = compare(accepted, rows) + baseline_growth(common.REPO_ROOT, accepted)
        for error in errors:
            yield Finding("low_level_accessors", "error", "scripts/audit_maintainability/baselines/generated/low_level_accessors", None, error)
        for row in rows:
            yield Finding("low_level_accessors", "info", row["path"], row["line"],
                          f"{row['rule']} in {row['enclosing_fn']} (count={row['count']})", row)
    except (OSError, ValueError, KeyError, TypeError) as exc:
        yield Finding("low_level_accessors", "error", "scripts/audit_maintainability/baselines/generated/low_level_accessors", None, str(exc))


CHECK = CheckSpec(key="low_level_accessors", title="Low-level host accessors", hard_gate=True,
                  description="Raw accessor site multiset outside policy owners may only shrink.", runner=_run)
