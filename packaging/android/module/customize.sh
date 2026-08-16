#!/system/bin/sh

ui_print "- Loom Android packaging alpha"
ui_print "- Architecture: $ARCH"

case "$ARCH" in
  arm64|arm64-v8a|aarch64) ;;
  *) abort "! Loom packaging alpha currently supports arm64 only" ;;
esac

if [ ! -x "$MODPATH/bin/loom" ]; then
  abort "! Loom binary missing from module package"
fi

set_perm_recursive "$MODPATH" 0 0 0755 0644
set_perm "$MODPATH/bin/loom" 0 0 0755
for script in customize.sh post-fs-data.sh service.sh action.sh uninstall.sh; do
  [ -f "$MODPATH/$script" ] && set_perm "$MODPATH/$script" 0 0 0755
done

mkdir -p /data/adb/loom
chmod 0700 /data/adb/loom

ui_print "- Installed Loom binary and runtime hooks"
ui_print "- Automatic effective-view activation is fail-closed in this packaging alpha"
ui_print "- Reboot after installation"
