#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
LOG="$STATE/post-fs-data.log"
SHADOW_CONF="$STATE/shadow.conf"
mkdir -p "$STATE"
chmod 0700 "$STATE"
exec >>"$LOG" 2>&1

echo "[loom] post-fs-data start"
echo "[loom] module=$MODDIR"

if [ -f "$MODDIR/disable" ]; then
  echo "[loom] module disabled; skipping"
  exit 0
fi

shadow_enabled=0
if [ -f "$SHADOW_CONF" ] && grep -Fxq 'LOOM_SHADOW_ENABLED=1' "$SHADOW_CONF"; then
  shadow_enabled=1
fi

# post-fs-data remains read-only and fast. It validates the selected runtime
# only; all loop/dm creation and mounting stays in the later service phase.
if [ "$shadow_enabled" = 1 ]; then
  if [ ! -x "$MODDIR/bin/loom-shadow" ]; then
    printf '%s\n' 'SHADOW_RUNTIME_MISSING' > "$STATE/status"
    echo "[loom] sparse-shadow runtime missing"
    exit 0
  fi
  if "$MODDIR/bin/loom-shadow" preflight; then
    echo "[loom] sparse-shadow preflight PASS"
  else
    echo "[loom] sparse-shadow preflight FAIL; existing mounts remain untouched"
  fi
else
  if [ ! -x "$MODDIR/bin/loom-sidecar" ]; then
    printf '%s\n' 'SIDECAR_RUNTIME_MISSING' > "$STATE/status"
    echo "[loom] identity sidecar runtime missing"
    exit 0
  fi
  if "$MODDIR/bin/loom-sidecar" preflight; then
    echo "[loom] identity sidecar preflight PASS"
  else
    echo "[loom] identity sidecar preflight FAIL; existing mounts remain untouched"
  fi
fi

exit 0
