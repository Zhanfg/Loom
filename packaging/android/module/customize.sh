#!/system/bin/sh

ui_print "- Loom Android Alpha 5 early snapshot prepare"
ui_print "- Architecture: $ARCH"

case "$ARCH" in
  arm64|arm64-v8a|aarch64) ;;
  *) abort "! Loom Alpha 5 currently supports arm64 only" ;;
esac

for binary in \
  loom loom-flatten loom-early-map loom-fiemap \
  loom-sidecar loom-shadow loom-shadow-layered loom-compose loom-early-prepare; do
  if [ ! -x "$MODPATH/bin/$binary" ]; then
    abort "! Missing runtime: bin/$binary"
  fi
done

set_perm_recursive "$MODPATH" 0 0 0755 0644
for binary in \
  loom loom-flatten loom-early-map loom-fiemap \
  loom-sidecar loom-shadow loom-shadow-layered loom-compose loom-early-prepare; do
  set_perm "$MODPATH/bin/$binary" 0 0 0755
done
for script in customize.sh post-fs-data.sh service.sh boot-completed.sh action.sh uninstall.sh; do
  [ -f "$MODPATH/$script" ] && set_perm "$MODPATH/$script" 0 0 0755
done

STATE=/data/adb/loom
mkdir -p "$STATE" "$STATE/mnt" "$STATE/payload/system" "$STATE/generations" "$STATE/compose"
chmod 0700 "$STATE" "$STATE/mnt" "$STATE/payload" "$STATE/payload/system" "$STATE/generations" "$STATE/compose"

# Preserve device-specific configuration and generation history across upgrades.
for config in sidecar.conf shadow.conf compose.conf early.conf; do
  if [ ! -f "$STATE/$config" ]; then
    cp "$MODPATH/$config" "$STATE/$config" || abort "! Failed to initialize Loom $config"
    chmod 0600 "$STATE/$config"
  fi
done

ui_print "- Alpha 5 retains Alpha 4 single-DM LoomFS generation semantics"
ui_print "- Optional early preparation copies the committed aggregate shadow into ext4 /metadata"
ui_print "- FIEMAP + loom-early-map convert that file into raw metadata-sector mappings"
ui_print "- Early preparation is disabled by default: LOOM_EARLY_PREPARE_ENABLED=0"
ui_print "- Prepared snapshots are NOT activated by this build"
ui_print "- No OverlayFS, Magic Mount, per-file bind mount, or early shadow loop is required by the prepared map"
ui_print "- First-stage /system takeover remains hard-disabled (LOOM_TAKEOVER=0)"
ui_print "- A prepare failure does not change the active Alpha 4 LoomFS generation"
ui_print "- Existing module mounts are not removed or modified"
ui_print "- Reboot after installation"
