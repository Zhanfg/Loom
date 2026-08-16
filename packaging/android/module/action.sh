#!/system/bin/sh

STATE=/data/adb/loom
printf '%s\n' 'Loom module status'
printf '%s\n' '------------------'
if [ -f "$STATE/status" ]; then
  printf 'status: '
  cat "$STATE/status"
else
  printf '%s\n' 'status: not initialized'
fi

for log in post-fs-data.log service.log; do
  if [ -f "$STATE/$log" ]; then
    printf '\n== %s ==\n' "$log"
    tail -n 40 "$STATE/$log"
  fi
done
