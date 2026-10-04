#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root: sudo ./install.sh" >&2
    exit 1
fi

test -x target/release/gtr9-fan-control || {
    echo "Build first: cargo build --release" >&2
    exit 1
}

install -o root -g root -m 0755 target/release/gtr9-fan-control /usr/local/sbin/gtr9-fan-control
if [ ! -e /etc/gtr9-fan-control.conf ]; then
    install -o root -g root -m 0644 gtr9-fan-control.conf /etc/gtr9-fan-control.conf
fi
install -o root -g root -m 0644 systemd/gtr9-fan-control.service /etc/systemd/system/gtr9-fan-control.service
systemctl daemon-reload
echo "Installed. Validate with: gtr9-fan-control validate"
echo "Enable after calibration: sudo systemctl enable --now gtr9-fan-control"
