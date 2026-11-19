#!/usr/bin/env bash

_stage_release_routines() {
    local repo="$1" live="$2/routines" staged="$2/routines.new" path
    [ -d "$repo/routines" ] || return 0

    git -C "$repo" ls-files -z -- routines/ | (
        # With no tracked routines, leave live files in place through the deploy.
        IFS= read -r -d '' path || exit 0
        echo "▸ Staging routines..." >&2
        rm -rf "$staged" || exit
        mkdir -p "$staged" || exit
        if [ -d "$live" ]; then
            rsync -a "$live/" "$staged/" || exit
        fi

        # Tracked routines win even if the live copy has the same size and mtime.
        while :; do
            printf '%s\0' "${path#routines/}"
            IFS= read -r -d '' path || break
        done | rsync -a --no-recursive --ignore-times --from0 --files-from=- \
            "$repo/routines/" "$staged/" || exit
        printf '%s\n' "$staged"
    )
}
