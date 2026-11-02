#!/bin/sh
# Build and install weaver as a systemd user service.

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
bin_home=${XDG_BIN_HOME:-"$HOME/.local/bin"}
systemd_home="$config_home/systemd/user"

config_dir="$config_home/weaver"
data_dir="$data_home/weaver"
config_file="$config_dir/config.yaml"
unit_file="$systemd_home/weaver.service"
binary="$bin_home/weaver"

command -v cargo >/dev/null 2>&1 || {
    echo "error: cargo is required; install a recent stable Rust toolchain" >&2
    exit 1
}
command -v systemctl >/dev/null 2>&1 || {
    echo "error: systemd is required for the user service" >&2
    exit 1
}

echo "Building weaver (release)..."
cargo build --release --locked --manifest-path "$repo_root/Cargo.toml"

mkdir -p "$bin_home" "$config_dir" "$data_dir" "$systemd_home"
install -m 0755 "$repo_root/target/release/weaver" "$binary"

if [ ! -e "$config_file" ]; then
    cat >"$config_file" <<EOF
# Weaver's AI integrations are optional. See the project README for provider settings.
db: "$data_dir/arch.db"
daemon: true
bind: 127.0.0.1:8444
EOF
    chmod 0600 "$config_file"
    echo "Created $config_file"
else
    echo "Keeping existing $config_file"
fi

cat >"$unit_file" <<EOF
[Unit]
Description=Weaver architectural-memory MCP daemon
After=network.target

[Service]
Type=simple
ExecStart="$binary" --config "$config_file"
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable weaver.service
# restart, not just start, so a reinstall picks up the new binary and unit
systemctl --user restart weaver.service

echo "Installed weaver to $binary"
echo "MCP endpoint: http://127.0.0.1:8444/mcp"
echo "Status: systemctl --user status weaver"
echo "Logs: journalctl --user -u weaver -f"
