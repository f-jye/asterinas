#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

source /etc/profile

# Step 1: run dbus
mkdir -p /var/lib/dbus /usr/share/X11/xorg.conf.d /run/user/0
[ -f /var/lib/dbus/machine-id ] || dbus-uuidgen --ensure=/var/lib/dbus/machine-id

# The user bus: the user manager (systemd --user) and the session share it.
if [ ! -S /run/user/0/bus ]; then
  dbus-daemon --session --fork --address=unix:path=/run/user/0/bus
fi
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/0/bus
export XDG_RUNTIME_DIR=/run/user/0
export XDG_SESSION_TYPE=x11
export XDG_CURRENT_DESKTOP=GNOME-Flashback:GNOME

# Step 2: run Xorg
XKB_DATA="/run/current-system/sw/share/X11/xkb"
MODULE_PATH="/run/current-system/sw/lib/xorg/modules"

nohup Xorg :0 vt1 \
  -modulepath "$MODULE_PATH" \
  -xkbdir "$XKB_DATA" \
  -logverbose 0 \
  -logfile /var/log/xorg_debug.log \
  -novtswitch \
  -keeptty \
  > /var/log/xorg.log 2>&1 &

# Step 3: start the GNOME Flashback session components directly.
export DISPLAY=:0
LOG=/var/log/gnome-session.log
mkdir -p "$(dirname "$LOG")"
: > "$LOG"

# Wait for Xorg to be ready.
sleep 2

nohup metacity >>"$LOG" 2>&1 &
nohup gnome-flashback >>"$LOG" 2>&1 &
nohup gnome-panel >>"$LOG" 2>&1 &
