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

commit_ok=1
if [ "$compose_enabled" = 1 ] && [ -x "$MODDIR/bin/loom-compose" ]; then
  if "$MODDIR/bin/loom-compose" commit; then
    echo "[loom] generation commit PASS"
  else
    echo "[loom] generation commit FAIL; pending marker retained for recovery"
    commit_ok=0
  fi
fi

# Alpha 5 prepare is intentionally after a successful boot commit. It only copies
# the already-validated aggregate shadow into /metadata and prepares raw-sector
# descriptors for a future first-stage handoff. It never changes this boot's mount.
if [ "$commit_ok" = 1 ] && [ -x "$MODDIR/bin/loom-early-prepare" ]; then
  if "$MODDIR/bin/loom-early-prepare" prepare; then
    echo "[loom] early snapshot prepare completed or disabled"
  else
    echo "[loom] early snapshot prepare FAIL; active LoomFS generation remains unchanged"
  fi
fi

exit 0
