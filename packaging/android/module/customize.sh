#!/system/bin/sh

ui_print "- Loom Android sidecar Alpha 1"
ui_print "- Architecture: $ARCH"

case "$ARCH" in
  arm64|arm64-v8a|aarch64) ;;
  *) abort "! Loom sidecar Alpha 1 currently supports arm64 only" ;;
esac

if [ ! -x "$MODPATH/bin/loom" ]; then
  abort "! Loom binary missing from module package"
fi
if [ ! -x "$MODPATH/bin/loom-sidecar" ]; then
  abort "! Loom sidecar runtime missing from module package"
fi

set_perm_recursive "$MODPATH" 0 0 0755 0644
set_perm "$MODPATH/bin/loom" 0 0 0755
set_perm "$MODPATH/bin/loom-sidecar" 0 0 0755
for script in customize.sh post-fs-data.sh service.sh action.sh uninstall.sh; do
  [ -f "$MODPATH/$script" ] && set_perm "$MODPATH/$script" 0 0 0755
done

STATE=/data/adb/loom
mkdir -p "$STATE" "$STATE/mnt"
chmod 0700 "$STATE" "$STATE/mnt"

# Preserve user/device-specific configuration across upgrades. A first install
# receives the conservative sidecar-only defaults bundled with the module.
if [ ! -f "$STATE/sidecar.conf" ]; then
  cp "$MODPATH/sidecar.conf" "$STATE/sidecar.conf" || abort "! Failed to initialize Loom sidecar config"
  chmod 0600 "$STATE/sidecar.conf"
fi

ui_print "- Sidecar mode enabled by default"
ui_print "- Loom will mount only below /data/adb/loom/mnt"
ui_print "- Existing /system, /vendor, /product and current module mounts are not replaced"
ui_print "- Any activation failure is fail-closed and cleans only Loom-owned resources"
ui_print "- Reboot after installation"
