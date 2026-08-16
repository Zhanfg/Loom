#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom

printf '%s\n' 'Loom sidecar Alpha 1 status'
printf '%s\n' '---------------------------'

if [ -x "$MODDIR/bin/loom-sidecar" ]; then
  "$MODDIR/bin/loom-sidecar" status 2>/dev/null || true
elif [ -f "$STATE/status" ]; then
  printf 'status: '
  cat "$STATE/status"
else
  printf '%s\n' 'status: not initialized'
fi

for log in post-fs-data.log service.log sidecar.log; do
  if [ -f "$STATE/$log" ]; then
    printf '\n== %s ==\n' "$log"
    tail -n 60 "$STATE/$log"
  fi
done

printf '\n%s\n' 'Safety mode: sidecar only; stock/current module mounts are never replaced.'
