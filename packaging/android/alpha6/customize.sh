#!/system/bin/sh

ui_print "- Loom Android Alpha 6 early recovery protocol"
ui_print "- Architecture: $ARCH"

case "$ARCH" in
  arm64|arm64-v8a|aarch64) ;;
  *) abort "! Loom Alpha 6 currently supports arm64 only" ;;
esac

for binary in \
  loom loom-flatten loom-early-map loom-early-state loom-fiemap \
  loom-sidecar loom-shadow loom-shadow-layered loom-compose loom-early-prepare; do
  if [ ! -x "$MODPATH/bin/$binary" ]; then
    abort "! Missing runtime: bin/$binary"
  fi
done

set_perm_recursive "$MODPATH" 0 0 0755 0644
for binary in \
  loom loom-flatten loom-early-map loom-early-state loom-fiemap \
  loom-sidecar loom-shadow loom-shadow-layered loom-compose loom-early-prepare; do
  set_perm "$MODPATH/bin/$binary" 0 0 0755
done
for script in customize.sh post-fs-data.sh service.sh boot-completed.sh action.sh uninstall.sh; do
  [ -f "$MODPATH/$script" ] && set_perm "$MODPATH/$script" 0 0 0755
done

STATE=/data/adb/loom
mkdir -p "$STATE" "$STATE/mnt" "$STATE/payload/system" "$STATE/generations" "$STATE/compose"
chmod 0700 "$STATE" "$STATE/mnt" "$STATE/payload" "$STATE/payload/system" "$STATE/generations" "$STATE/compose"

for config in sidecar.conf shadow.conf compose.conf early.conf recovery.conf; do
  if [ ! -f "$STATE/$config" ]; then
    cp "$MODPATH/$config" "$STATE/$config" || abort "! Failed to initialize Loom $config"
    chmod 0600 "$STATE/$config"
  fi
done

ui_print "- Alpha 6 retains Alpha 5 prepare-only raw /metadata snapshots"
ui_print "- Added one-shot desired/attempted/confirmed/failed/force-stock recovery state tool"
ui_print "- Snapshot bytes are SHA-256 verified before any candidate can be returned"
ui_print "- A candidate can only become confirmed after a real attempted marker exists"
ui_print "- LOOM_EARLY_AUTO_ARM=0 and LOOM_EARLY_AUTO_CONFIRM=0 are mandatory in this build"
ui_print "- No boot script arms or confirms an early generation automatically"
ui_print "- First-stage /system takeover remains disabled (LOOM_TAKEOVER=0)"
ui_print "- Recovery protocol is installed now so the future first-stage host can reuse it directly"
ui_print "- Reboot after installation"
