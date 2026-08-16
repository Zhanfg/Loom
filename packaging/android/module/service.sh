#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
LOG="$STATE/service.log"
mkdir -p "$STATE"
chmod 0700 "$STATE"
exec >>"$LOG" 2>&1

echo "[loom] service start"

if [ -f "$MODDIR/disable" ]; then
  echo "[loom] module disabled; skipping"
  exit 0
fi

if [ ! -x "$MODDIR/bin/loom-sidecar" ]; then
  printf '%s\n' 'SIDECAR_RUNTIME_MISSING' > "$STATE/status"
  echo "[loom] sidecar runtime missing"
  exit 0
fi

# Alpha 1 is intentionally sidecar-only. The runtime creates a Loom-owned
# read-only dm-linear identity view and mounts it below /data/adb/loom/mnt.
# It never unmounts, remounts, bind-mounts, or overlays /system, /vendor,
# /product, or any mount created by an existing module.
if "$MODDIR/bin/loom-sidecar" activate; then
  echo "[loom] sidecar activation PASS"
else
  echo "[loom] sidecar activation FAIL; Loom resources rolled back"
fi

exit 0
