#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
COMPOSE_CONF="$STATE/compose.conf"
SHADOW_CONF="$STATE/shadow.conf"

printf '%s\n' 'Loom Android Alpha 3 status'
printf '%s\n' '---------------------------'

compose_enabled=0
if [ -f "$COMPOSE_CONF" ] && grep -Fxq 'LOOM_COMPOSE_ENABLED=1' "$COMPOSE_CONF"; then
  compose_enabled=1
fi

if [ "$compose_enabled" = 1 ]; then
  printf '%s\n' 'selected_mode=block-generation-compose'
  if [ -x "$MODDIR/bin/loom-compose" ]; then
    "$MODDIR/bin/loom-compose" status 2>/dev/null || true
  else
    printf '%s\n' 'status=COMPOSE_RUNTIME_MISSING'
  fi
else
  shadow_enabled=0
  if [ -f "$SHADOW_CONF" ] && grep -Fxq 'LOOM_SHADOW_ENABLED=1' "$SHADOW_CONF"; then
    shadow_enabled=1
  fi
  printf 'selected_mode=%s\n' "$( [ "$shadow_enabled" = 1 ] && printf 'sparse-shadow-single-source' || printf 'identity' )"
  if [ "$shadow_enabled" = 1 ] && [ -x "$MODDIR/bin/loom-shadow" ]; then
    "$MODDIR/bin/loom-shadow" status 2>/dev/null || true
  elif [ -x "$MODDIR/bin/loom-sidecar" ]; then
    "$MODDIR/bin/loom-sidecar" status 2>/dev/null || true
  fi
fi

if [ -f "$COMPOSE_CONF" ]; then
  printf '\n== compose.conf ==\n'
  grep -E '^(LOOM_COMPOSE_ENABLED|LOOM_COMPOSE_MODULE_ROOT|LOOM_COMPOSE_ORDER|LOOM_COMPOSE_MAX_FILES|LOOM_TARGET|LOOM_ORIGIN|LOOM_MOUNTPOINT|LOOM_TAKEOVER)=' "$COMPOSE_CONF" 2>/dev/null || true
fi

if [ -f "$STATE/current-generation" ]; then
  generation=$(cat "$STATE/current-generation" 2>/dev/null || true)
  if [ -n "$generation" ] && [ -f "$STATE/generations/$generation/state.env" ]; then
    printf '\n== current generation ==\n'
    cat "$STATE/generations/$generation/state.env"
  fi
fi

if [ -f "$STATE/recovery-hold" ]; then
  printf '\n%s\n' 'RECOVERY HOLD ACTIVE'
  printf 'held_generation=%s\n' "$(cat "$STATE/recovery-hold" 2>/dev/null || printf unknown)"
  printf '%s\n' 'Loom will not automatically reactivate a generation after an interrupted boot.'
  printf '%s\n' 'After diagnosing the cause, clear it explicitly with:'
  printf '  su -c %s/bin/loom-compose resume\n' "$MODDIR"
fi

printf '\n== available ordinary module payload sources ==\n'
found=0
if [ -d /data/adb/modules ]; then
  for module in /data/adb/modules/*; do
    [ -d "$module/system" ] || continue
    [ -f "$module/disable" ] && continue
    [ -f "$module/remove" ] && continue
    [ -f "$module/skip_mount" ] && continue
    id=${module##*/}
    [ "$id" = loom ] && continue
    if [ -f "$module/module.prop" ] && grep -Eq '^metamodule=(1|true)$' "$module/module.prop" 2>/dev/null; then
      continue
    fi
    printf '%s\n' "$id"
    found=1
  done
fi
[ "$found" = 1 ] || printf '%s\n' '(none)'

for log in post-fs-data.log service.log boot-completed.log compose.log compose-shadow.log shadow.log sidecar.log; do
  if [ -f "$STATE/$log" ]; then
    printf '\n== %s ==\n' "$log"
    tail -n 80 "$STATE/$log"
  fi
done

printf '\n%s\n' 'Safety boundary: this build composes a real block-level effective view but does not replace the Android first-stage /system mount.'
printf '%s\n' 'OverlayFS/Magic Mount are not used by the LoomFS composition path.'
