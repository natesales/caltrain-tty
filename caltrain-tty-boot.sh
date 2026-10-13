#!/bin/bash
set -u
export TERM=linux
FONT=/usr/share/consolefonts/Lat15-TerminusBold32x16.psf.gz
for _ in $(seq 1 120); do
  name=$(cat /sys/class/graphics/fb0/name 2>/dev/null || true)
  if [ "$name" = "vc4drmfb" ]; then
    break
  fi
  sleep 0.5
done
sleep 0.5
if [ -r "$FONT" ]; then
  sudo -n setfont "$FONT" -C /dev/tty1 || true
fi
clear
exec /usr/local/bin/caltrain-tty --stop 70141 --stop 70142
