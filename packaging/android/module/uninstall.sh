#!/system/bin/sh

# Runtime state is deliberately isolated from the immutable module directory.
# Removing it is safe in the packaging alpha because automatic activation is
# fail-closed and no persistent dm devices are created yet.
rm -rf /data/adb/loom
