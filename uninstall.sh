#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root: sudo ./uninstall.sh" >&2
    exit 1
fi

systemctl disable --now gtr9-fan-control.service 2>/dev/null || true
/usr/local/sbin/gtr9-fan-control restore 2>/dev/null || true
rm -f /etc/systemd/system/gtr9-fan-control.service /usr/local/sbin/gtr9-fan-control
systemctl daemon-reload
echo "Removed binary and service. Configuration remains at /etc/gtr9-fan-control.conf."
