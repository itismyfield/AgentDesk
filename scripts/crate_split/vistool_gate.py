"""Which shadow probe build opens a cfg-gated `mod` line that upper re-exports from lower.

Upper checks with and without `test`; lower builds without it and with the probe feature on. Every other cfg
atom has one value for both builds, so lower's reading of a gate is the gate with those two atoms fixed.
"""

import itertools

import rust_cfg
from vistool import PROBE_FEATURE


def _probe_atom(node):
    """'test', 'feature' (the probe feature), or None for an atom both builds read alike."""
    if node.name == "test":
        return "test"
    return "feature" if node.name == "feature" and node.value == PROBE_FEATURE else None


def _atoms(node):
    return [node] if node.kind == "atom" else [a for child in node.children for a in _atoms(child)]


def _holds(node, on):
    if node.kind == "atom":
        return on(node)
    values = [_holds(child, on) for child in node.children]
    return all(values) if node.kind == "all" else any(values) if node.kind == "any" else not any(values[:1])


def _lower_text(node):
    """The cfg as lower's build reads it: `test` is off (`any()`), the probe feature on (`all()`)."""
    if node.kind != "atom":
        return f"{node.kind}({', '.join(_lower_text(child) for child in node.children)})"
    fixed = {"test": "any()", "feature": "all()"}.get(_probe_atom(node))
    return fixed or (node.name if node.value is None else f'{node.name} = "{node.value}"')


def lower_gate(attrs):
    """(verdict, lower's cfg text) for the attribute lines above a `mod` line.

    'same': lower has the module in every build where upper opens the line. 'never': in none of them (lower's
    own test module). 'split': only where the cfg text holds; a cfg that does not parse counts as shared."""
    nodes = [n for n in map(rust_cfg.parse_cfg_condition, rust_cfg.attribute_conditions("\n".join(attrs))) if n]
    key = lambda atom: (atom.name, atom.value)
    free = sorted({key(a) for n in nodes for a in _atoms(n) if not _probe_atom(a)}, key=str)
    seen = set()
    for bits in itertools.product((False, True), repeat=min(len(free), 12)):
        env = dict(zip(free, bits))
        holds = lambda test, feature: all(_holds(n, lambda a: {"test": test, "feature": feature}.get(
            _probe_atom(a), env.get(key(a), True))) for n in nodes)
        if any(holds(test, feature) for test in (False, True) for feature in (False, True)):
            seen.add(holds(False, True))
    if seen == {False, True}:
        text = ", ".join(_lower_text(n) for n in nodes)
        return "split", text if len(nodes) == 1 else f"all({text})"
    return ("never" if seen == {False} else "same"), ""
