#!/bin/sh
# Build and install flipad-host on the PC (x86_64 Linux with PipeWire),
# as a systemd user service that starts with your session.
set -eu
cd "$(dirname "$0")/.."

cargo build --release -p flipad-host
install -Dm755 target/release/flipad-host "$HOME/.local/bin/flipad-host"
install -Dm644 packaging/flipad-host.service "$HOME/.config/systemd/user/flipad-host.service"
systemctl --user daemon-reload
systemctl --user enable --now flipad-host

# first start creates the pairing token
sleep 1
echo "flipad-host is running. Token for the handheld:"
cat "$HOME/.config/flipad/token"
