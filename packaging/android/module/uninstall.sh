#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom

# Tear down only Loom-owned sidecar resources before removing persistent state.
# Never touch /system, /vendor, /product, or mounts created by another module.
if [ -x "$MODDIR/bin/loom-sidecar" ]; then
  "$MODDIR/bin/loom-sidecar" cleanup >/dev/null 2>&1 || true
fi

rm -rf "$STATE"
