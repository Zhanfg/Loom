#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom

# Tear down only Loom-owned resources. The Alpha 3 flashable-generation build
# never targets /system itself, another module's mount, or the authoritative
# block device.
if [ -x "$MODDIR/bin/loom-compose" ]; then
  "$MODDIR/bin/loom-compose" cleanup >/dev/null 2>&1 || true
fi
if [ -x "$MODDIR/bin/loom-shadow" ]; then
  "$MODDIR/bin/loom-shadow" cleanup >/dev/null 2>&1 || true
fi
if [ -x "$MODDIR/bin/loom-sidecar" ]; then
  "$MODDIR/bin/loom-sidecar" cleanup >/dev/null 2>&1 || true
fi

rm -rf "$STATE"
