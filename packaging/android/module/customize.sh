#!/system/bin/sh

ui_print "- Loom Android shadow sidecar Alpha 2"
ui_print "- Architecture: $ARCH"

case "$ARCH" in
  arm64|arm64-v8a|aarch64) ;;
  *) abort "! Loom shadow sidecar Alpha 2 currently supports arm64 only" ;;
esac

if [ ! -x "$MODPATH/bin/loom" ]; then
  abort "! Loom binary missing from module package"
fi
if [ ! -x "$MODPATH/bin/loom-sidecar" ]; then
  abort "! Loom identity sidecar runtime missing from module package"
fi
if [ ! -x "$MODPATH/bin/loom-shadow" ]; then
  abort "! Loom sparse-shadow runtime missing from module package"
fi

set_perm_recursive "$MODPATH" 0 0 0755 0644
set_perm "$MODPATH/bin/loom" 0 0 0755
set_perm "$MODPATH/bin/loom-sidecar" 0 0 0755
set_perm "$MODPATH/bin/loom-shadow" 0 0 0755
for script in customize.sh post-fs-data.sh service.sh action.sh uninstall.sh; do
  [ -f "$MODPATH/$script" ] && set_perm "$MODPATH/$script" 0 0 0755
done

STATE=/data/adb/loom
mkdir -p "$STATE" "$STATE/mnt" "$STATE/payload/system"
chmod 0700 "$STATE" "$STATE/mnt" "$STATE/payload" "$STATE/payload/system"

# Preserve device-specific configuration and payloads across upgrades.
if [ ! -f "$STATE/sidecar.conf" ]; then
  cp "$MODPATH/sidecar.conf" "$STATE/sidecar.conf" || abort "! Failed to initialize Loom sidecar config"
  chmod 0600 "$STATE/sidecar.conf"
fi
if [ ! -f "$STATE/shadow.conf" ]; then
  cp "$MODPATH/shadow.conf" "$STATE/shadow.conf" || abort "! Failed to initialize Loom shadow config"
  chmod 0600 "$STATE/shadow.conf"
fi

ui_print "- Identity sidecar remains the default after install"
ui_print "- Sparse-shadow engine is installed but disabled until shadow.conf enables it"
ui_print "- Sparse-shadow can import regular files from an existing module without disabling that module"
ui_print "- Loom mounts only below /data/adb/loom/mnt; system takeover is hard-disabled in Alpha 2"
ui_print "- Any failure rolls back Loom-owned dm/loop/mount resources"
ui_print "- Reboot after installation"
