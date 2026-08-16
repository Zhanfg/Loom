#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
LOG="$STATE/service.log"
SHADOW_CONF="$STATE/shadow.conf"
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
  echo "[loom] identity sidecar runtime missing"
  exit 0
fi

shadow_enabled=0
if [ -f "$SHADOW_CONF" ] && grep -Fxq 'LOOM_SHADOW_ENABLED=1' "$SHADOW_CONF"; then
  shadow_enabled=1
fi

if [ "$shadow_enabled" = 1 ]; then
  if [ ! -x "$MODDIR/bin/loom-shadow" ]; then
    printf '%s\n' 'SHADOW_RUNTIME_MISSING' > "$STATE/status"
    echo "[loom] sparse-shadow runtime missing"
    exit 0
  fi

  # Mode switches only tear down Loom-owned resources. Existing system mounts
  # and mounts owned by other modules are never targeted.
  "$MODDIR/bin/loom-sidecar" cleanup >/dev/null 2>&1 || true
  if "$MODDIR/bin/loom-shadow" activate; then
    echo "[loom] sparse-shadow sidecar activation PASS"
  else
    echo "[loom] sparse-shadow sidecar activation FAIL; Loom resources rolled back"
  fi
else
  if [ -x "$MODDIR/bin/loom-shadow" ]; then
    "$MODDIR/bin/loom-shadow" cleanup >/dev/null 2>&1 || true
  fi
  if "$MODDIR/bin/loom-sidecar" activate; then
    echo "[loom] identity sidecar activation PASS"
  else
    echo "[loom] identity sidecar activation FAIL; Loom resources rolled back"
  fi
fi

exit 0
