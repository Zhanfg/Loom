#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 10 ]]; then
  echo "usage: $0 <loom> <loom-flatten> <loom-early-map> <loom-early-state> <loom-fiemap> <output-zip> <version-name> <version-code> <source-ref> <source-sha>" >&2
  exit 2
fi

BINARY="$1"
FLATTEN_BINARY="$2"
EARLY_MAP_BINARY="$3"
EARLY_STATE_BINARY="$4"
FIEMAP_BINARY="$5"
OUTPUT="$6"
VERSION_NAME="$7"
VERSION_CODE="$8"
SOURCE_REF="$9"
SOURCE_SHA="${10}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE="$ROOT/module"
ALPHA6="$ROOT/alpha6"

for pair in \
  "loom:$BINARY" \
  "loom-flatten:$FLATTEN_BINARY" \
  "loom-early-map:$EARLY_MAP_BINARY" \
  "loom-early-state:$EARLY_STATE_BINARY" \
  "loom-fiemap:$FIEMAP_BINARY"; do
  name=${pair%%:*}
  path=${pair#*:}
  [[ -f "$path" ]] || { echo "missing binary: $name ($path)" >&2; exit 1; }
done
for runtime in loom-sidecar loom-shadow loom-shadow-commit loom-compose loom-early-prepare; do
  [[ -f "$TEMPLATE/bin/$runtime" ]] || { echo "missing Android runtime: $runtime" >&2; exit 1; }
done
for config in sidecar.conf shadow.conf compose.conf early.conf; do
  [[ -f "$TEMPLATE/$config" ]] || { echo "missing Android configuration: $config" >&2; exit 1; }
done
[[ -f "$ALPHA6/recovery.conf" ]] || { echo "missing Alpha 6 recovery.conf" >&2; exit 1; }
[[ -f "$ALPHA6/customize.sh" ]] || { echo "missing Alpha 6 customize.sh" >&2; exit 1; }
[[ "$VERSION_CODE" =~ ^[0-9]+$ ]] || { echo "version code must be numeric" >&2; exit 1; }

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STAGE="$TMP/module"
mkdir -p "$STAGE/bin"
cp -a "$TEMPLATE/." "$STAGE/"
rm -f "$STAGE/module.prop.in"

mv "$STAGE/bin/loom-shadow" "$STAGE/bin/loom-shadow-layered"
mv "$STAGE/bin/loom-shadow-commit" "$STAGE/bin/loom-shadow"
install -m 0755 "$BINARY" "$STAGE/bin/loom"
install -m 0755 "$FLATTEN_BINARY" "$STAGE/bin/loom-flatten"
install -m 0755 "$EARLY_MAP_BINARY" "$STAGE/bin/loom-early-map"
install -m 0755 "$EARLY_STATE_BINARY" "$STAGE/bin/loom-early-state"
install -m 0755 "$FIEMAP_BINARY" "$STAGE/bin/loom-fiemap"
install -m 0755 "$ALPHA6/customize.sh" "$STAGE/customize.sh"
install -m 0644 "$ALPHA6/recovery.conf" "$STAGE/recovery.conf"
touch "$STAGE/skip_mount"

cat >"$STAGE/module.prop" <<EOF
id=loom
name=Loom
version=$VERSION_NAME
versionCode=$VERSION_CODE
author=Loom Project
description=Systemless block-view runtime. Alpha 6 packages the one-shot early recovery protocol beside Alpha 5 raw /metadata snapshots. Early arm/confirm remain manual and first-stage takeover stays disabled. Source $SOURCE_SHA.
EOF

cat >"$STAGE/build-info.txt" <<EOF
project=Loom
package_kind=android-kernelsu-magisk-module
source_ref=$SOURCE_REF
source_sha=$SOURCE_SHA
version=$VERSION_NAME
versionCode=$VERSION_CODE
architecture=arm64-v8a
runtime_activation=alpha6-early-recovery-protocol
filesystem_fabric=origin-plus-aggregate-sparse-shadow
stable_composition=flattened-single-dm-linear
stable_dm_depth=1
early_snapshot_storage=metadata-ext4-raw-extents
early_snapshot_default=disabled
early_snapshot_state=prepared-not-active
early_recovery_protocol=one-shot-last-good-force-stock
early_recovery_impl=dependency-free-c
early_snapshot_sha256_verify=true
early_auto_arm=false
early_auto_confirm=false
early_state_root=/metadata/loom/state
early_snapshot_root=/metadata/loom/early
early_shadow_loop_required=false
generation_commit=boot-completed
uses_overlayfs=false
uses_magic_mount=false
first_stage_takeover=false
system_takeover=false
EOF

chmod 0644 "$STAGE/module.prop" "$STAGE/build-info.txt" "$STAGE/skip_mount" \
  "$STAGE/sidecar.conf" "$STAGE/shadow.conf" "$STAGE/compose.conf" \
  "$STAGE/early.conf" "$STAGE/recovery.conf"
for binary in \
  loom loom-flatten loom-early-map loom-early-state loom-fiemap \
  loom-sidecar loom-shadow loom-shadow-layered loom-compose loom-early-prepare; do
  chmod 0755 "$STAGE/bin/$binary"
done
for script in customize.sh post-fs-data.sh service.sh boot-completed.sh action.sh uninstall.sh; do
  [[ -f "$STAGE/$script" ]] || { echo "missing module script: $script" >&2; exit 1; }
  chmod 0755 "$STAGE/$script"
done

# This package must never smuggle recovery state transitions into boot scripts
# before a real first-stage host exists.
for script in post-fs-data.sh service.sh boot-completed.sh; do
  if grep -Eq 'loom-early-state[[:space:]]+(arm|confirm)' "$STAGE/$script"; then
    echo "forbidden automatic early-state transition in $script" >&2
    exit 1
  fi
done

mkdir -p "$(dirname "$OUTPUT")"
OUTPUT_ABS="$(cd "$(dirname "$OUTPUT")" && pwd)/$(basename "$OUTPUT")"
rm -f "$OUTPUT_ABS"
(
  cd "$STAGE"
  zip -X -9 -r "$OUTPUT_ABS" . >/dev/null
)
unzip -t "$OUTPUT_ABS" >/dev/null
printf 'Android module package: %s\n' "$OUTPUT_ABS"
printf 'Source: %s (%s)\n' "$SOURCE_REF" "$SOURCE_SHA"
printf 'Version: %s (%s)\n' "$VERSION_NAME" "$VERSION_CODE"
