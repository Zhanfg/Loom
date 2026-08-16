#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
SHADOW_CONF="$STATE/shadow.conf"

printf '%s\n' 'Loom shadow sidecar Alpha 2 status'
printf '%s\n' '-----------------------------------'

shadow_enabled=0
if [ -f "$SHADOW_CONF" ] && grep -Fxq 'LOOM_SHADOW_ENABLED=1' "$SHADOW_CONF"; then
  shadow_enabled=1
fi
printf 'selected_mode=%s\n' "$( [ "$shadow_enabled" = 1 ] && printf 'sparse-shadow' || printf 'identity' )"

if [ "$shadow_enabled" = 1 ] && [ -x "$MODDIR/bin/loom-shadow" ]; then
  "$MODDIR/bin/loom-shadow" status 2>/dev/null || true
elif [ -x "$MODDIR/bin/loom-sidecar" ]; then
  "$MODDIR/bin/loom-sidecar" status 2>/dev/null || true
elif [ -f "$STATE/status" ]; then
  printf 'status='
  cat "$STATE/status"
else
  printf '%s\n' 'status=not-initialized'
fi

if [ -f "$SHADOW_CONF" ]; then
  printf '\n== shadow.conf ==\n'
  grep -E '^(LOOM_SHADOW_ENABLED|LOOM_SOURCE_MODULE_ID|LOOM_PAYLOAD_ROOT|LOOM_MAX_LAYERS|LOOM_TAKEOVER)=' "$SHADOW_CONF" 2>/dev/null || true
fi

printf '\n== available module payload sources ==\n'
found=0
if [ -d /data/adb/modules ]; then
  for module in /data/adb/modules/*; do
    [ -d "$module/system" ] || continue
    id=${module##*/}
    [ "$id" = loom ] && continue
    printf '%s\n' "$id"
    found=1
  done
fi
[ "$found" = 1 ] || printf '%s\n' '(none with a system/ tree)'

for log in post-fs-data.log service.log sidecar.log shadow.log; do
  if [ -f "$STATE/$log" ]; then
    printf '\n== %s ==\n' "$log"
    tail -n 80 "$STATE/$log"
  fi
done

printf '\n%s\n' 'Safety: Alpha 2 never mounts over /system, /vendor, /product, or another module mount.'
printf '%s\n' 'To import an existing module, set LOOM_SOURCE_MODULE_ID=<id> and LOOM_SHADOW_ENABLED=1 in /data/adb/loom/shadow.conf, then reboot.'
