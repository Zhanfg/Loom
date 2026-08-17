#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom

# Tear down only Loom-owned runtime resources. Alpha 5 still never targets
# /system itself, another module's mount, or the authoritative system device.
if [ -x "$MODDIR/bin/loom-compose" ]; then
  "$MODDIR/bin/loom-compose" cleanup >/dev/null 2>&1 || true
fi
if [ -x "$MODDIR/bin/loom-shadow" ]; then
  "$MODDIR/bin/loom-shadow" cleanup >/dev/null 2>&1 || true
fi
if [ -x "$MODDIR/bin/loom-sidecar" ]; then
  "$MODDIR/bin/loom-sidecar" cleanup >/dev/null 2>&1 || true
fi

# Early snapshots use a fixed Loom-owned namespace. Do not honor configurable
# metadata paths during uninstall: that could expand the deletion boundary.
if [ -d /metadata/loom/early ]; then
  rm -rf /metadata/loom/early >/dev/null 2>&1 || true
  rmdir /metadata/loom >/dev/null 2>&1 || true
fi

rm -rf "$STATE"
