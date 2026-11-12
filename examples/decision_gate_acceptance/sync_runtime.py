#!/usr/bin/env python3
from pathlib import Path

here = Path(__file__).resolve().parent
source = here.parents[1] / "src/services/decision_gate/runtime.rs"
(here / "runtime/mod.rs").write_bytes(source.read_bytes())
