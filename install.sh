#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
BIN_PATH="${BIN_DIR}/comfytui"
CONFIG_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/comfytui"
CONFIG_PATH="${CONFIG_DIR}/config.toml"
PATH_WAS_ADDED=0

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

command -v cargo >/dev/null 2>&1 || fail \
    "Rust/Cargo is not installed. On Arch: sudo pacman -S --needed rust"
command -v systemd-run >/dev/null 2>&1 || fail "systemd-run is required"
command -v systemctl >/dev/null 2>&1 || fail "systemctl is required"

printf 'Compiling and running ComfyTUI unit tests...\n'
cd "$SCRIPT_DIR"
cargo test --release

printf 'Building ComfyTUI in release mode...\n'
cargo build --release

install -d "$BIN_DIR"
install -m 0755 "$SCRIPT_DIR/target/release/comfytui" "$BIN_PATH"

install -d "$CONFIG_DIR"
if [[ ! -e "$CONFIG_PATH" ]]; then
    sed \
        -e "s#/home/daanh#${HOME//\#/\\#}#g" \
        -e "s#/media/daanh#/media/${USER}#g" \
        "$SCRIPT_DIR/comfytui.example.toml" > "$CONFIG_PATH"
    printf 'Created %s\n' "$CONFIG_PATH"
else
    # v0.1.1 migration: the original generated config disabled sampler previews.
    # Only change the exact old two-line setting; leave all other user edits alone.
    if grep -Fq '"--preview-method",' "$CONFIG_PATH" \
        && sed -n '/"--preview-method",/{n;p;}' "$CONFIG_PATH" | grep -Fq '"none"'; then
        cp -a "$CONFIG_PATH" "${CONFIG_PATH}.bak-v0.1.0"
        sed -i '/"--preview-method",/{n;s/"none"/"auto"/;}' "$CONFIG_PATH"
        printf 'Enabled live previews in %s (backup: %s)\n' \
            "$CONFIG_PATH" "${CONFIG_PATH}.bak-v0.1.0"
    else
        printf 'Preserved existing config: %s\n' "$CONFIG_PATH"
    fi
fi

# v0.2.0 migration: expose the local ComfyUI API address used by the
# generation panel. Older configs remain valid through serde defaults, but
# writing the values makes them easy to change when a non-default port is used.
if ! grep -Eq '^[[:space:]]*api_host[[:space:]]*=' "$CONFIG_PATH"; then
    cat >> "$CONFIG_PATH" <<'EOF'

# Local ComfyUI API used for queue/generation details in the TUI.
api_host = "127.0.0.1"
api_port = 8188
EOF
    printf 'Added ComfyUI API settings to %s\n' "$CONFIG_PATH"
fi

if [[ ":${PATH}:" != *":${BIN_DIR}:"* ]]; then
    ZSHRC="${HOME}/.zshrc"
    PATH_LINE='export PATH="$HOME/.local/bin:$PATH"'
    if [[ ! -f "$ZSHRC" ]] || ! grep -Fqx "$PATH_LINE" "$ZSHRC"; then
        printf '\n%s\n' "$PATH_LINE" >> "$ZSHRC"
        printf 'Added ~/.local/bin to PATH in %s\n' "$ZSHRC"
    fi
    PATH_WAS_ADDED=1
    export PATH="${BIN_DIR}:${PATH}"
fi

printf '\nInstalled: %s\n' "$BIN_PATH"
printf 'Running preflight checks...\n\n'
if ! "$BIN_PATH" --check; then
    printf '\nThe binary installed correctly, but preflight found a configuration/system issue.\n' >&2
    printf 'Edit: %s\n' "$CONFIG_PATH" >&2
    exit 2
fi

printf '\nDone.\n'
if (( PATH_WAS_ADDED )); then
    printf 'Open a new terminal, or run: source ~/.zshrc\n'
fi
printf 'Start it with: comfytui\n'
