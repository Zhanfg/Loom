#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
LOG="$STATE/post-fs-data.log"
mkdir -p "$STATE"
chmod 0700 "$STATE"
exec >>"$LOG" 2>&1

echo "[loom] post-fs-data start"
echo "[loom] module=$MODDIR"

if [ -f "$MODDIR/disable" ]; then
  echo "[loom] module disabled; skipping"
  exit 0
fi

if [ ! -x "$MODDIR/bin/loom-sidecar" ]; then
  printf '%s\n' 'SIDECAR_RUNTIME_MISSING' > "$STATE/status"
  echo "[loom] sidecar runtime missing"
  exit 0
fi

# post-fs-data is boot-critical. Keep this phase read-only and fast: it only
# validates tools, the real origin device, and the packaged Loom binary.
# No dm device or mount is created until service.sh.
if "$MODDIR/bin/loom-sidecar" preflight; then
  echo "[loom] sidecar preflight PASS"
else
  echo "[loom] sidecar preflight FAIL; stock/current module mounts remain untouched"
fi

exit 0
