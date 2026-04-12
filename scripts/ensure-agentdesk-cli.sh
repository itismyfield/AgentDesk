#!/usr/bin/env bash
set -euo pipefail

HOME_DIR="${HOME:?HOME is required}"

select_target_dir() {
    local requested="${AGENTDESK_CLI_DIR:-}"
    if [ -n "$requested" ]; then
        printf '%s\n' "$requested"
        return 0
    fi

    local path_entry
    IFS=':' read -r -a path_entries <<< "${PATH:-}"
    for path_entry in "${path_entries[@]}"; do
        case "$path_entry" in
            "$HOME_DIR/bin"|"$HOME_DIR/.local/bin")
                printf '%s\n' "$path_entry"
                return 0
                ;;
        esac
    done

    printf '%s\n' "$HOME_DIR/bin"
}

TARGET_DIR="$(select_target_dir)"
WRAPPER_PATH="$TARGET_DIR/agentdesk"
TMP_PATH="$WRAPPER_PATH.tmp.$$"

mkdir -p "$TARGET_DIR"

cat >"$TMP_PATH" <<EOF
#!/usr/bin/env bash
set -euo pipefail

home_dir="\${HOME:-$HOME_DIR}"
candidates=(
  "\$home_dir/.adk/release/bin/agentdesk"
  "\$home_dir/.adk/release/agentdesk"
)

for candidate in "\${candidates[@]}"; do
  if [ -x "\$candidate" ]; then
    exec "\$candidate" "\$@"
  fi
done

echo "agentdesk: no installed runtime binary found" >&2
echo "looked for:" >&2
for candidate in "\${candidates[@]}"; do
  echo "  - \$candidate" >&2
done
exit 127
EOF

chmod 755 "$TMP_PATH"
mv "$TMP_PATH" "$WRAPPER_PATH"

echo "✓ agentdesk CLI wrapper installed: $WRAPPER_PATH"
if [[ ":${PATH:-}:" != *":$TARGET_DIR:"* ]]; then
    echo "  [WARN] $TARGET_DIR is not on PATH"
fi
