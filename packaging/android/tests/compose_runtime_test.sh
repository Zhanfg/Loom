#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
COMPOSER="$ROOT/packaging/android/module/bin/loom-compose"

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

MODDIR="$TMP/loom-module"
STATE="$TMP/state"
MODULES="$TMP/modules"
mkdir -p "$MODDIR/bin" "$STATE/mnt" "$STATE/payload/system" "$MODULES"
cp "$COMPOSER" "$MODDIR/bin/loom-compose"
chmod +x "$MODDIR/bin/loom-compose"

cat >"$MODDIR/bin/loom" <<'EOF'
#!/bin/sh
exit 0
EOF

cat >"$MODDIR/bin/loom-shadow" <<'EOF'
#!/bin/sh
set -eu
STATE=${LOOM_STATE_DIR:?}
case "${1:-status}" in
  activate)
    mkdir -p "$STATE/shadow-runtime"
    cat >"$STATE/shadow-runtime/runtime.env" <<EOT
LOOM_MODE=sparse-shadow-sidecar
LOOM_EFFECTIVE_DEVICE=/dev/fake-loom-generation
LOOM_MOUNTPOINT=$STATE/mnt/system-generation
EOT
    exit 0
    ;;
  cleanup)
    rm -rf "$STATE/shadow-runtime"
    exit 0
    ;;
  *) exit 0 ;;
esac
EOF
chmod +x "$MODDIR/bin/loom" "$MODDIR/bin/loom-shadow"

make_module() {
  local id=$1
  mkdir -p "$MODULES/$id/system"
  cat >"$MODULES/$id/module.prop" <<EOF
id=$id
name=$id
version=1
versionCode=1
author=test
EOF
}

make_module a
mkdir -p "$MODULES/a/system/etc"
printf 'from-a\n' >"$MODULES/a/system/etc/priority.txt"

make_module b
mkdir -p "$MODULES/b/system/etc"
printf 'from-b\n' >"$MODULES/b/system/etc/priority.txt"

make_module c
mkdir -p "$MODULES/c/system/bin"
printf 'tool-c\n' >"$MODULES/c/system/bin/tool"

make_module disabled
printf 'disabled\n' >"$MODULES/disabled/system/disabled.txt"
touch "$MODULES/disabled/disable"

make_module skipped
printf 'skipped\n' >"$MODULES/skipped/system/skipped.txt"
touch "$MODULES/skipped/skip_mount"

make_module meta-other
printf 'meta\n' >"$MODULES/meta-other/system/meta.txt"
printf 'metamodule=1\n' >>"$MODULES/meta-other/module.prop"

CONF="$STATE/compose.conf"
cat >"$CONF" <<EOF
LOOM_COMPOSE_ENABLED=1
LOOM_COMPOSE_MODULE_ROOT=$MODULES
LOOM_COMPOSE_ORDER=lexical-last-wins
LOOM_COMPOSE_MAX_FILES=128
LOOM_TARGET=system
LOOM_ORIGIN=/dev/fake-origin
LOOM_MOUNTPOINT=$STATE/mnt/system-generation
LOOM_TAKEOVER=0
EOF

run_compose() {
  LOOM_MODDIR="$MODDIR" \
  LOOM_STATE_DIR="$STATE" \
  LOOM_COMPOSE_CONFIG="$CONF" \
  LOOM_TEST_UID=0 \
    sh "$MODDIR/bin/loom-compose" "$@"
}

run_compose preflight
grep -Fxq 'COMPOSE_PREFLIGHT_OK' "$STATE/status"

run_compose activate
grep -Fxq 'COMPOSE_ACTIVE_PENDING_BOOT' "$STATE/status"
test -f "$STATE/pending-generation"
test "$(cat "$STATE/payload/system/.compose-current/etc/priority.txt")" = 'from-b'
test "$(cat "$STATE/payload/system/.compose-current/bin/tool")" = 'tool-c'
test ! -e "$STATE/payload/system/.compose-current/disabled.txt"
test ! -e "$STATE/payload/system/.compose-current/skipped.txt"
test ! -e "$STATE/payload/system/.compose-current/meta.txt"
grep -q '/a$' "$STATE/compose/modules.list"
grep -q '/b$' "$STATE/compose/modules.list"
grep -q '/c$' "$STATE/compose/modules.list"
! grep -q '/disabled$' "$STATE/compose/modules.list"
! grep -q '/skipped$' "$STATE/compose/modules.list"
! grep -q '/meta-other$' "$STATE/compose/modules.list"

run_compose commit
grep -Fxq 'COMPOSE_COMMITTED' "$STATE/status"
test -f "$STATE/current-generation"
test ! -e "$STATE/pending-generation"

# A pending marker surviving to the next activation must hold automatic retry.
printf 'g-interrupted\n' >"$STATE/pending-generation"
if run_compose activate; then
  echo 'expected interrupted generation to enter recovery hold' >&2
  exit 1
fi
grep -Fxq 'COMPOSE_RECOVERY_HOLD' "$STATE/status"
grep -Fxq 'g-interrupted' "$STATE/recovery-hold"
run_compose resume
test ! -e "$STATE/pending-generation"
test ! -e "$STATE/recovery-hold"

# Unsupported VFS semantics must fail closed rather than being silently omitted.
make_module z-symlink
mkdir -p "$MODULES/z-symlink/system/etc"
ln -s priority.txt "$MODULES/z-symlink/system/etc/link"
if run_compose activate; then
  echo 'expected symlink payload tree to fail closed' >&2
  exit 1
fi
grep -Fxq 'COMPOSE_UNSUPPORTED_TREE' "$STATE/status"
rm -rf "$MODULES/z-symlink"

# The flashable-generation build must reject takeover=1 at config preflight.
sed -i 's/^LOOM_TAKEOVER=0$/LOOM_TAKEOVER=1/' "$CONF"
if run_compose preflight; then
  echo 'expected LOOM_TAKEOVER=1 to be rejected' >&2
  exit 1
fi
grep -Fxq 'COMPOSE_CONFIG_INVALID' "$STATE/status"

printf '%s\n' 'compose runtime tests: PASS'
