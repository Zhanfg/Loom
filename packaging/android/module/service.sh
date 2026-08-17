#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
LOG="$STATE/service.log"
COMPOSE_CONF="$STATE/compose.conf"
SHADOW_CONF="$STATE/shadow.conf"
mkdir -p "$STATE"
chmod 0700 "$STATE" 2>/dev/null || true
exec >>"$LOG" 2>&1

echo "[loom] service start"

if [ -f "$MODDIR/disable" ]; then
  echo "[loom] module disabled; skipping"
  exit 0
fi

compose_enabled=0
if [ -f "$COMPOSE_CONF" ] && grep -Fxq 'LOOM_COMPOSE_ENABLED=1' "$COMPOSE_CONF"; then
  compose_enabled=1
fi

if [ "$compose_enabled" = 1 ]; then
  if [ ! -x "$MODDIR/bin/loom-compose" ]; then
    printf '%s\n' 'COMPOSE_RUNTIME_MISSING' >"$STATE/status"
    echo "[loom] composed-generation runtime missing"
    exit 0
  fi

  # Mode switches tear down only Loom-owned validation resources. Never target
  # stock Android mounts or another module's mount namespace.
  [ -x "$MODDIR/bin/loom-sidecar" ] && "$MODDIR/bin/loom-sidecar" cleanup >/dev/null 2>&1 || true
  [ -x "$MODDIR/bin/loom-shadow" ] && "$MODDIR/bin/loom-shadow" cleanup >/dev/null 2>&1 || true

  if "$MODDIR/bin/loom-compose" activate; then
    echo "[loom] composed LoomFS generation activation PASS"
  else
    echo "[loom] composed LoomFS generation activation FAIL/HOLD; existing mounts remain untouched"
  fi
  exit 0
fi

if [ ! -x "$MODDIR/bin/loom-sidecar" ]; then
  printf '%s\n' 'SIDECAR_RUNTIME_MISSING' >"$STATE/status"
  echo "[loom] identity sidecar runtime missing"
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
