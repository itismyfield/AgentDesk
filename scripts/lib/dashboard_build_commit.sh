#!/usr/bin/env bash

_dashboard_build_commit() {
    local action="$1" repo="$2" dist="$3" target recorded changes
    target="$(git -C "$repo" rev-parse HEAD)" || return 1
    case "$action" in
        write)
            changes="$(git -C "$repo" status --porcelain --untracked-files=all -- dashboard/)" || return 1
            [ -z "$changes" ] || target="$target+dirty"
            printf '%s\n' "$target" >"$dist/.build-commit"
            ;;
        keep)
            [ -f "$dist/.build-commit" ] || return 1
            recorded="$(cat "$dist/.build-commit")" || return 1
            [[ "$recorded" =~ ^[0-9a-f]{40}$ ]] || return 1
            [ "$target" != "$recorded" ] || return 1
            git -C "$repo" merge-base --is-ancestor "$target" "$recorded" 2>/dev/null || return 1
            printf '▸ Dashboard kept: deployed build %.9s is newer than target %.9s\n' "$recorded" "$target"
            ;;
        *) return 2 ;;
    esac
}
