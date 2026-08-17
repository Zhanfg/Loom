#!/system/bin/sh

ui_print "- Loom Android Alpha 3 flashable generation"
ui_print "- Architecture: $ARCH"

case "$ARCH" in
  arm64|arm64-v8a|aarch64) ;;
  *) abort "! Loom Alpha 3 currently supports arm64 only" ;;
esac

for binary in loom loom-sidecar loom-shadow loom-compose; do
  if [ ! -x "$MODPATH/bin/$binary" ]; then
    abort "! Missing runtime: bin/$binary"
  fi
done

set_perm_recursive "$MODPATH" 0 0 0755 0644
for binary in loom loom-sidecar loom-shadow loom-compose; do
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

ui_print "- Alpha 3 scans enabled ordinary modules and composes one LoomFS block generation"
ui_print "- Composition is origin + sparse shadow blocks; no OverlayFS or Magic Mount backend is used"
ui_print "- The effective EROFS is mounted only below /data/adb/loom/mnt/system-generation"
ui_print "- First-stage /system takeover remains hard-disabled (LOOM_TAKEOVER=0) in this flashable build"
ui_print "- A pending generation is committed only after boot-completed; interrupted boots enter recovery hold"
ui_print "- Existing module mounts are not removed or modified"
ui_print "- Reboot after installation"
