#!/bin/sh
# Cross-build flipad-client for the handheld (aarch64) and install it there.
#
#   HANDHELD=root@192.168.1.50 ./scripts/deploy-handheld.sh
#
# Optional: SSH_OPTS="-i key ..." for ssh/scp, HOST_IP=<PC address> to pin
# the PC instead of discovering it, ADD_STEAM_SHORTCUT=1 to add "Flip Pad"
# to the handheld's Steam library (Steam must be running there).
set -eu
cd "$(dirname "$0")/.."
: "${HANDHELD:?set HANDHELD=root@<handheld-ip>}"
SSH_OPTS=${SSH_OPTS:-}
USER_ON_HANDHELD=${USER_ON_HANDHELD:-deck}

cargo build --release --target aarch64-unknown-linux-gnu -p flipad-client
BIN=target/aarch64-unknown-linux-gnu/release/flipad-client

# shellcheck disable=SC2086
scp $SSH_OPTS "$BIN" "$HANDHELD:/tmp/flipad-client"
# shellcheck disable=SC2086
scp $SSH_OPTS tools/steam-cdp.py "$HANDHELD:/tmp/flipad-steam-cdp.py"

TOKEN=$(cat "$HOME/.config/flipad/token")
# shellcheck disable=SC2086
ssh $SSH_OPTS "$HANDHELD" sh -s <<EOF
set -eu
U=$USER_ON_HANDHELD
H=\$(getent passwd \$U | cut -d: -f6)
install -D -o \$U -g \$U -m755 /tmp/flipad-client \$H/.local/bin/flipad-client
install -d -o \$U -g \$U -m700 \$H/.config/flipad
printf '%s\n' '$TOKEN' > \$H/.config/flipad/token
chmod 600 \$H/.config/flipad/token
if [ -n "${HOST_IP:-}" ]; then echo "${HOST_IP:-}" > \$H/.config/flipad/host; fi
chown -R \$U:\$U \$H/.config/flipad
rm -f /tmp/flipad-client
if [ "${ADD_STEAM_SHORTCUT:-0}" = 1 ]; then
  python3 /tmp/flipad-steam-cdp.py add-shortcut "Flip Pad" "\$H/.local/bin/flipad-client" "\$H"
fi
echo "installed for \$U"
EOF
