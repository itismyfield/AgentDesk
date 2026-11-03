#!/usr/bin/env bash

_dashboard_build_commit() {
    local action="$1" input="$2" dist="${3:-}" target recorded changes
    if [ "$action" = write ]; then
        [ -n "$input" ] || return 1
        printf '%s\n' "$input" >"$dist/.build-commit"
        return
    fi
    target="$(git -C "$input" rev-parse HEAD 2>/dev/null)" || return 1
    changes="$(git -C "$input" status --porcelain --untracked-files=all -- dashboard/ defaults.json 2>/dev/null)" || return 1
    case "$action" in
        capture)
            [ -z "$changes" ] || target="$target+dirty"
            printf '%s\n' "$target"
            ;;
        keep)
            [ -z "$changes" ] || return 1
            [ -f "$dist/index.html" ] || return 1
            [ -f "$dist/.build-commit" ] || return 1
            recorded="$(cat "$dist/.build-commit")" || return 1
            [[ "$recorded" =~ ^[0-9a-f]{40}$ ]] || return 1
            [ "$target" != "$recorded" ] || return 1
            git -C "$input" merge-base --is-ancestor "$target" "$recorded" 2>/dev/null || return 1
            printf '▸ Dashboard kept: deployed build %.9s is newer than target %.9s\n' "$recorded" "$target"
            ;;
        *) return 2 ;;
    esac
}
