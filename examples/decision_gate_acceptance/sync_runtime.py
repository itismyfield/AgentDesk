#!/usr/bin/env python3
from pathlib import Path

here = Path(__file__).resolve().parent
source = here.parents[1] / "src/services/decision_gate/runtime.rs"
(here / "runtime/mod.rs").write_bytes(source.read_bytes())

adapter = here.parents[1] / "tests/support/decision_gate_capture_parser/generated"
adapter.mkdir(parents=True, exist_ok=True)
for name, origin in [("types", "types.rs"), ("parser", "runtime/parser.rs")]:
    original = here.parents[1] / "src/services/decision_gate" / origin
    lines = []
    for line in original.read_text().splitlines():
        if line.strip() == "#[cfg(test)]":
            continue
        if name == "types" and line.lstrip().startswith("pub(crate) "):
            line = line.replace("pub(crate)", "pub", 1)
        lines.append(line)
    (adapter / f"{name}.rs").write_text("\n".join(lines) + "\n")
