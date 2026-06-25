#!/usr/bin/env bash
set -euo pipefail
rm -f "${HOME}/.local/bin/comfywise"
printf 'Removed ~/.local/bin/comfywise\n'
printf 'Configuration was retained at %s\n' "${XDG_CONFIG_HOME:-${HOME}/.config}/comfywise/config.toml"
