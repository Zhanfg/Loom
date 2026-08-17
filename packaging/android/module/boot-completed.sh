#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
LOG="$STATE/boot-completed.log"
COMPOSE_CONF="$STATE/compose.conf"

mkdir -p "$STATE"
chmod 0700 "$STATE" 2>/dev/null || true
exec >>"$LOG" 2>&1

echo "[loom] boot-completed start"

if [ -f "$MODDIR/disable" ]; then
  echo "[loom] module disabled; generation not committed"
  exit 0
fi

compose_enabled=0
if [ -f "$COMPOSE_CONF" ] && grep -Fxq 'LOOM_COMPOSE_ENABLED=1' "$COMPOSE_CONF"; then
  compose_enabled=1
fi

if [ "$compose_enabled" = 1 ] && [ -x "$MODDIR/bin/loom-compose" ]; then
  if "$MODDIR/bin/loom-compose" commit; then
    echo "[loom] generation commit PASS"
  else
    echo "[loom] generation commit FAIL; pending marker retained for recovery"
  fi
fi

exit 0
