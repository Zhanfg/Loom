#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
LOG="$STATE/post-fs-data.log"
COMPOSE_CONF="$STATE/compose.conf"
SHADOW_CONF="$STATE/shadow.conf"
mkdir -p "$STATE"
chmod 0700 "$STATE" 2>/dev/null || true
exec >>"$LOG" 2>&1

echo "[loom] post-fs-data start"
echo "[loom] module=$MODDIR"

if [ -f "$MODDIR/disable" ]; then
  echo "[loom] module disabled; skipping"
  exit 0
fi

compose_enabled=0
if [ -f "$COMPOSE_CONF" ] && grep -Fxq 'LOOM_COMPOSE_ENABLED=1' "$COMPOSE_CONF"; then
  compose_enabled=1
fi

# Alpha 3 keeps post-fs-data mutation-free. It validates module inventory and
# the selected block-generation runtime here; loop/dm creation remains in the
# later service stage until first-stage takeover has its own proven bootstrap.
if [ "$compose_enabled" = 1 ]; then
  if [ ! -x "$MODDIR/bin/loom-compose" ]; then
    printf '%s\n' 'COMPOSE_RUNTIME_MISSING' >"$STATE/status"
    echo "[loom] compose runtime missing"
    exit 0
  fi
  if "$MODDIR/bin/loom-compose" preflight; then
    echo "[loom] composed-generation preflight PASS"
  else
    echo "[loom] composed-generation preflight FAIL; existing mounts remain untouched"
  fi
  exit 0
fi

shadow_enabled=0
if [ -f "$SHADOW_CONF" ] && grep -Fxq 'LOOM_SHADOW_ENABLED=1' "$SHADOW_CONF"; then
  shadow_enabled=1
fi

if [ "$shadow_enabled" = 1 ]; then
  if [ ! -x "$MODDIR/bin/loom-shadow" ]; then
    printf '%s\n' 'SHADOW_RUNTIME_MISSING' >"$STATE/status"
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
    printf '%s\n' 'SIDECAR_RUNTIME_MISSING' >"$STATE/status"
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
