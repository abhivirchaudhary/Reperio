#!/bin/sh
# Flush and release every drive before the power goes.
sync
umount -a -r 2>/dev/null
