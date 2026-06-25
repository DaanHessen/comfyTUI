#!/usr/bin/env bash
set -euo pipefail
rm -f "${HOME}/.local/bin/comfytui"
printf 'Removed ~/.local/bin/comfytui\n'
printf 'Configuration was retained at %s\n' "${XDG_CONFIG_HOME:-${HOME}/.config}/comfytui/config.toml"
