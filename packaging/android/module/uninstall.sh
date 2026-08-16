#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom

# Tear down only Loom-owned resources before removing persistent state.
# Shadow layers are removed first, then the identity sidecar. Never touch
# /system, /vendor, /product, or mounts created by another module.
if [ -x "$MODDIR/bin/loom-shadow" ]; then
  "$MODDIR/bin/loom-shadow" cleanup >/dev/null 2>&1 || true
fi
if [ -x "$MODDIR/bin/loom-sidecar" ]; then
  "$MODDIR/bin/loom-sidecar" cleanup >/dev/null 2>&1 || true
fi

rm -rf "$STATE"
