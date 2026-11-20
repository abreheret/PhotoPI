#!/usr/bin/env bash
# Build for the Raspberry Pi (aarch64) on your computer, upload the binary and restart the systemd service.
#
#   PI=pi@raspberrypi.local ./deploy/deploy.sh
#
# Needs: zig (brew install zig), cargo-zigbuild (cargo install cargo-zigbuild --locked),
#        rustup target add aarch64-unknown-linux-gnu. `sudo` on the Pi asks for its password (ssh -t).
# On the Pi: ffmpeg/ffprobe (sudo apt install ffmpeg) for video thumbnails and video metadata.
#
# The systemd unit (deploy/photopi.service) is installed ONLY if the Pi does not have one yet, because it
# holds YOUR settings (photo folder...). Edit the copy on the Pi afterwards. To overwrite an existing unit
# on purpose: FORCE_UNIT=1 PI=... ./deploy/deploy.sh
#
# First time: create your account on the Pi (the password is read from stdin, one line):
#   ssh -t "$PI" 'read -rsp "password: " P; echo; echo "$P" | PHOTO_DB=$HOME/.local/share/photopi/photopi.db ~/photopi/photopi user add you@example.com "Your Name"'
set -euo pipefail
PI="${PI:?set PI=user@host (the SSH target of the Raspberry Pi)}"
cd "$(dirname "$0")/.."

cargo test
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.36   # .2.36 = glibc floor (Raspberry Pi OS has >= 2.36)
BIN=target/aarch64-unknown-linux-gnu/release/photopi

ssh "$PI" 'mkdir -p ~/photopi'
scp "$BIN" "$PI:photopi/photopi.new"
scp deploy/photopi.service "$PI:/tmp/photopi.service"
scp deploy/remote-install.sh "$PI:/tmp/photopi-install.sh"
# A real terminal (ssh -t) is needed for the sudo password prompt; that is why the remote steps are a file, not stdin.
ssh -t "$PI" "FORCE_UNIT=${FORCE_UNIT:-0} bash /tmp/photopi-install.sh; rm -f /tmp/photopi-install.sh"
