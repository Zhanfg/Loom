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

# Packaging alpha deliberately does not create or replace Android mounts yet.
# The activation bridge will be wired here without changing the module format
# or the GitHub Actions packaging workflow.
printf '%s\n' 'PACKAGED_NO_AUTOMOUNT' > "$STATE/status"
echo "[loom] activation bridge not enabled; leaving stock mounts untouched"
exit 0
