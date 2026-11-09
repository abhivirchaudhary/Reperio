#!/bin/sh
# What the first screen runs: the recovery menu, again and again, with a
# way to restart or power off when it ends.
export PATH=/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export TERM=${TERM:-linux}

while true; do
    clear
    reperio menu
    clear
    echo "Reperio rescue"
    echo
    echo "  1) Back to the recovery menu"
    echo "  2) Restart the computer"
    echo "  3) Power off"
    echo "  4) Command line (for experts; Alt+F2 has one too)"
    echo
    printf "Type a number and press Enter: "
    read -r answer || continue
    case "$answer" in
    2) sync; reboot -f ;;
    3) sync; poweroff -f ;;
    4) /bin/ash -l ;;
    esac
done
