#!/usr/bin/env bash
# Runs ON the Raspberry Pi (copied and started by deploy.sh). Installs the new binary and restarts the service.
set -e
mv ~/photopi/photopi.new ~/photopi/photopi
chmod +x ~/photopi/photopi
UNIT=/etc/systemd/system/photopi.service
if [ ! -f "$UNIT" ] || [ "${FORCE_UNIT:-0}" = "1" ]; then
  sudo sh -c "cat /tmp/photopi.service > $UNIT"
  echo "systemd unit installed: check $UNIT (PHOTO_ROOT, user...) then: sudo systemctl daemon-reload"
else
  echo "systemd unit already present: left untouched"
fi
rm -f /tmp/photopi.service
sudo systemctl daemon-reload
sudo systemctl enable photopi
sudo systemctl restart photopi
sleep 2
systemctl is-active photopi
curl -s localhost:2283/api/server/ping; echo
