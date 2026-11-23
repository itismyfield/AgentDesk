#!/usr/bin/env bash
# Self-hosted runners keep pinned tools on PATH; found=true lets the job skip the install-action download.
# A missing tool or a different version prints found=false, and the install step runs as on hosted runners.
set -euo pipefail

spec=${1:?usage: preinstalled-tool.sh <tool>@<version>}
tool=${spec%@*}
version=${spec#*@}
found=false
actual=
if [[ "$spec" == *@* ]] && path=$(command -v "$tool" 2>/dev/null); then
    # cargo-nextest takes its own name as the first argument when cargo does not run it.
    if [[ "$tool" == cargo-nextest ]]; then
        actual=$("$path" nextest --version 2>/dev/null | awk 'NR == 1 {print $2}') || actual=
    else
        actual=$("$path" --version 2>/dev/null | awk 'NR == 1 {print $2}') || actual=
    fi
    [[ "$actual" == "$version" ]] && found=true
fi
if [[ "$found" == true ]]; then
    echo "$tool $version preinstalled at $path; skipping install-action"
else
    echo "$tool $version not preinstalled (found: ${actual:-none}); install-action installs it"
fi
echo "found=$found" >> "${GITHUB_OUTPUT:-/dev/stdout}"
