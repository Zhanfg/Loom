#!/system/bin/sh

ui_print "- Loom Android Alpha 4 single-DM generation"
ui_print "- Architecture: $ARCH"

case "$ARCH" in
  arm64|arm64-v8a|aarch64) ;;
  *) abort "! Loom Alpha 4 currently supports arm64 only" ;;
esac

for binary in loom loom-flatten loom-sidecar loom-shadow loom-shadow-layered loom-compose; do
  if [ ! -x "$MODPATH/bin/$binary" ]; then
    abort "! Missing runtime: bin/$binary"
  fi
done

set_perm_recursive "$MODPATH" 0 0 0755 0644
for binary in loom loom-flatten loom-sidecar loom-shadow loom-shadow-layered loom-compose; do
  set_perm "$MODPATH/bin/$binary" 0 0 0755
done
for script in customize.sh post-fs-data.sh service.sh boot-completed.sh action.sh uninstall.sh; do
  [ -f "$MODPATH/$script" ] && set_perm "$MODPATH/$script" 0 0 0755
done

STATE=/data/adb/loom
mkdir -p "$STATE" "$STATE/mnt" "$STATE/payload/system" "$STATE/generations" "$STATE/compose"
chmod 0700 "$STATE" "$STATE/mnt" "$STATE/payload" "$STATE/payload/system" "$STATE/generations" "$STATE/compose"

# Preserve device-specific configuration and generation history across upgrades.
for config in sidecar.conf shadow.conf compose.conf; do
  if [ ! -f "$STATE/$config" ]; then
    cp "$MODPATH/$config" "$STATE/$config" || abort "! Failed to initialize Loom $config"
    chmod 0600 "$STATE/$config"
  fi
done

ui_print "- Alpha 4 compiles enabled ordinary modules into one LoomFS block generation"
ui_print "- EROFS compilation may use temporary dm layers, then flattens them before steady state"
ui_print "- Steady state is one aggregate sparse shadow loop + one read-only effective dm device"
ui_print "- No OverlayFS, Magic Mount, or per-file bind mount is used in the LoomFS data path"
ui_print "- The effective EROFS is still mounted only below /data/adb/loom/mnt/system-generation"
ui_print "- First-stage /system takeover remains hard-disabled (LOOM_TAKEOVER=0)"
ui_print "- Interrupted generations enter recovery hold instead of automatic retry"
ui_print "- Existing module mounts are not removed or modified"
ui_print "- Reboot after installation"
