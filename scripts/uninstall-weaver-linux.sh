#!/bin/sh
# Remove the systemd user service and installed binary. User data is preserved.

set -eu

config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
bin_home=${XDG_BIN_HOME:-"$HOME/.local/bin"}
unit_file="$config_home/systemd/user/weaver.service"
binary="$bin_home/weaver"

if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now weaver.service 2>/dev/null || true
fi
rm -f -- "$unit_file" "$binary"
if command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload
fi

echo "Removed the weaver service and binary."
echo "Configuration and database were preserved under ${config_home}/weaver and ${XDG_DATA_HOME:-"$HOME/.local/share"}/weaver."

