#!/usr/bin/env bash

# Shared by the main-CI issue triage and the infrastructure-rerun classifier.
# A match withholds an infrastructure-only classification: in an ambiguous
# mixed log, treating the job as a real failure is the fail-safe direction.
# The cost of a false positive is a manual rerun; a false negative can silently
# retry a broken gate into green. Keep this predicate in one place so those two
# consumers cannot drift back to different safety boundaries.
# Beyond Rust compile/test output, the markers cover this repo's rustfmt,
# ShellCheck, Python unittest, PyYAML/Psych, and linker gate failure shapes.
# They are intentionally unanchored because downloaded Actions logs may prefix
# each emitted line with timestamps or job/step names.
REAL_FAILURE_REGEX='test result: FAILED|error\[E|error: could not compile|panicked at|assertion .*failed|Diff in .*:[0-9]+:|SC[0-9]{4} \((error|warning|info|style)\):|FAILED \([^)]*(failures|errors)=[0-9]+|yaml\.(scanner|parser|composer|constructor)\.[A-Za-z]+Error:|Psych::SyntaxError|ld: cannot find '

log_has_real_failure() {
  local log_path="$1"
  [[ -s "$log_path" ]] || return 1
  grep -a -E -i -q -- "$REAL_FAILURE_REGEX" "$log_path"
}
