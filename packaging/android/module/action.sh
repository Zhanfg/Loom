#!/system/bin/sh

MODDIR=${0%/*}
STATE=/data/adb/loom
COMPOSE_CONF="$STATE/compose.conf"
SHADOW_CONF="$STATE/shadow.conf"
EARLY_CONF="$STATE/early.conf"

printf '%s\n' 'Loom Android Alpha 5 status'
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

if [ "$compose_enabled" = 1 ] && [ -x "$MODDIR/bin/loom-shadow" ]; then
  printf '\n== flat shadow runtime ==\n'
  "$MODDIR/bin/loom-shadow" status 2>/dev/null || true
fi

if [ -f "$COMPOSE_CONF" ]; then
  printf '\n== compose.conf ==\n'
  grep -E '^(LOOM_COMPOSE_ENABLED|LOOM_COMPOSE_MODULE_ROOT|LOOM_COMPOSE_ORDER|LOOM_COMPOSE_MAX_FILES|LOOM_TARGET|LOOM_ORIGIN|LOOM_MOUNTPOINT|LOOM_TAKEOVER)=' "$COMPOSE_CONF" 2>/dev/null || true
fi

if [ -f "$EARLY_CONF" ]; then
  printf '\n== early.conf ==\n'
  grep -E '^(LOOM_EARLY_PREPARE_ENABLED|LOOM_METADATA_MOUNT|LOOM_TAKEOVER)=' "$EARLY_CONF" 2>/dev/null || true
fi

if [ -x "$MODDIR/bin/loom-early-prepare" ]; then
  printf '\n== early snapshot ==\n'
  "$MODDIR/bin/loom-early-prepare" status 2>/dev/null || true
fi

if [ -f "$STATE/current-generation" ]; then
  generation=$(cat "$STATE/current-generation" 2>/dev/null || true)
  if [ -n "$generation" ] && [ -f "$STATE/generations/$generation/state.env" ]; then
    printf '\n== current generation ==\n'
    cat "$STATE/generations/$generation/state.env"
  fi
fi

if [ -f "$STATE/early-prepared-generation" ]; then
  early_generation=$(cat "$STATE/early-prepared-generation" 2>/dev/null || true)
  if [ -n "$early_generation" ] && [ -f "/metadata/loom/early/$early_generation/descriptor.env" ]; then
    printf '\n== prepared early descriptor ==\n'
    cat "/metadata/loom/early/$early_generation/descriptor.env"
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

for log in post-fs-data.log service.log boot-completed.log compose.log compose-shadow.log shadow.log shadow-flat.log early-prepare.log sidecar.log; do
  if [ -f "$STATE/$log" ]; then
    printf '\n== %s ==\n' "$log"
    tail -n 80 "$STATE/$log"
  fi
done

printf '\n%s\n' 'Safety boundary: Alpha 5 can prepare raw /metadata sector snapshots but does not activate them during first-stage boot.'
printf '%s\n' 'OverlayFS/Magic Mount are not used by the LoomFS path; first-stage /system takeover remains disabled.'
