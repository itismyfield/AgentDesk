#!/usr/bin/env bash

# Shared by the main-CI issue triage and the infrastructure-rerun classifier.
# A match withholds an infrastructure-only classification: in an ambiguous
# mixed log, treating the job as a real failure is the fail-safe direction.
# The cost of a false positive is a manual rerun; a false negative can silently
# retry a broken gate into green. Keep this predicate in one place so those two
# consumers cannot drift back to different safety boundaries.
# Beyond Rust compile/test output, the markers cover this repo's rustfmt,
# ShellCheck, Python unittest, PyYAML, and linker gate failure shapes. The
# `error[E` prefix deliberately accepts future/non-numeric rustc diagnostic
# codes: fail-safe regression handling is safer than silently retrying them,
# while the literal rustc diagnostic prefix keeps the match bounded.
# Most alternatives are intentionally unanchored because downloaded Actions
# logs may prefix each emitted line with timestamps or job/step names. The
# linker alternative instead requires a line/start-token boundary, which still
# accepts prefixes such as `/usr/bin/ld:` without matching ordinary prose.
# `error: could not compile` followed by SIGKILL can be runner OOM, and linker
# errors accompanied by `No space left on device` can be disk exhaustion. They
# remain regressions fail-safe when mixed with those infrastructure symptoms.
REAL_FAILURE_REGEX='test result: FAILED|error\[E|error: could not compile|panicked at|assertion .*failed|Diff in .*:[0-9]+:|SC[0-9]{4} \((error|warning|info|style)\):|FAILED \([^)]*(failures|errors)=[0-9]+|yaml\.(scanner|parser|composer|constructor)\.[A-Za-z]+Error:|(^|[[:space:]/])ld: cannot find '

log_has_real_failure() {
  local log_path="$1"
  [[ -s "$log_path" ]] || return 1
  grep -a -E -i -q -- "$REAL_FAILURE_REGEX" "$log_path"
}
