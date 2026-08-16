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

if [ ! -x "$MODDIR/bin/loom" ]; then
  echo "[loom] binary missing or not executable"
  printf '%s\n' 'BINARY_MISSING' > "$STATE/status"
  exit 0
fi

if "$MODDIR/bin/loom" --help >/dev/null 2>&1; then
  printf '%s\n' 'PACKAGED_PRECHECK_OK' > "$STATE/status"
  echo "[loom] arm64 binary precheck PASS"
else
  printf '%s\n' 'BINARY_PRECHECK_FAILED' > "$STATE/status"
  echo "[loom] arm64 binary precheck FAIL"
fi
